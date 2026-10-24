"""Account-scoped Space accounting uses the same immutable ledger as sessions."""

from __future__ import annotations

import pathlib
import re
import sqlite3
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0011_account_resources.sql"
RESOURCE_SOURCE = (ROOT / "crates/agents-worker/src/resources.rs").read_text(encoding="utf-8")
RECONCILE_SOURCE = (ROOT / "crates/agents-worker/src/reconcile.rs").read_text(encoding="utf-8")


def query(source: str, prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', source) if sql.startswith(prefix))


DECLARE = query(RESOURCE_SOURCE, "INSERT INTO resource_observations")
LOOKUP = query(RESOURCE_SOURCE, "SELECT resource,unit,quantity,price_version,status FROM resource_observations")
STORAGE = query(RECONCILE_SOURCE, "INSERT INTO resource_storage_objects")
DEFICIT = query(RECONCILE_SOURCE, "INSERT INTO resource_storage_deficits")


class AccountResourceSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            if migration.name < MIGRATION.name:
                self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("alice", "bob"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,?,?)", (owner, "issuer", owner, 0, 0))
            self.db.execute("INSERT INTO usage_accounts VALUES(?,1000,0,0,0)", (owner,))
            self.db.execute("INSERT INTO agents VALUES(?,?,?,'{}',0,0,NULL)", (owner, owner, owner))
            self.db.execute("INSERT INTO sessions VALUES(?,?,?,?,'idle','{}','pin',0,0,NULL,NULL,NULL)",
                            (owner, owner, owner, "do-" + owner))
        self.db.commit()

    def tearDown(self) -> None:
        self.db.close()

    def migrate(self) -> None:
        self.db.executescript("BEGIN;\n" + MIGRATION.read_text(encoding="utf-8") + "\nCOMMIT;")
        self.assertEqual(self.db.execute("PRAGMA foreign_key_check").fetchall(), [])

    def reserve(self, key: str, owner: str, session: str | None) -> None:
        self.db.execute("""INSERT INTO usage_reservations
            (id,account_id,idempotency_key,request_hash,session_id,reserved_microusd,state,created_at,updated_at)
            VALUES(?,?,?,?,?,100,'reserved',0,0)""", (key, owner, key, key, session))

    def declare(self, key: str, owner: str, session: str | None, event: str = "upload") -> None:
        self.db.execute(DECLARE, (key, owner, session, event, "r2_operation", "class_a", "test",
                                  key, owner, session))

    def storage(self, key: str, owner: str, session: str | None) -> None:
        self.db.execute(STORAGE, (key, owner, session, "1024", "0", "0"))

    def deficit(self, key: str, owner: str, session: str | None, reservation: str) -> None:
        self.db.execute(DEFICIT, (key, owner, session, "0", "1000", "1024000", "1", "test", "{}",
                                  reservation, key, "0", key))

    def test_upgrade_preserves_old_pending_settled_and_storage_rows(self) -> None:
        self.reserve("pending", "alice", "alice")
        self.reserve("settled", "alice", "alice")
        self.db.execute("INSERT INTO resource_observations VALUES('pending','alice','alice','put','r2_operation','class_a',NULL,'test',NULL,'pending')")
        self.db.execute("INSERT INTO resource_observations VALUES('settled','alice','alice','get','r2_operation','class_b','1','test',0,'observed')")
        self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=0 WHERE id='settled'")
        self.storage("archive", "alice", "alice")
        self.deficit("archive", "alice", "alice", "retention")
        tables = ("resource_observations", "resource_storage_objects", "resource_storage_deficits",
                  "usage_reservations", "usage_accounts", "reservation_vintages")
        before = {table: self.db.execute(f"SELECT * FROM {table}").fetchall() for table in tables}
        self.db.commit()
        self.migrate()
        for table, rows in before.items():
            self.assertEqual(self.db.execute(f"SELECT * FROM {table}").fetchall(), rows, table)
        with self.assertRaisesRegex(sqlite3.IntegrityError, "resource_observation_pending"):
            self.db.execute("UPDATE usage_reservations SET state='released' WHERE id='pending'")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_resource_observation"):
            self.db.execute("UPDATE resource_observations SET quantity='2' WHERE run_id='settled'")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_storage_charge"):
            self.db.execute("DELETE FROM resource_storage_deficits")

    def test_account_and_session_scopes_both_settle_without_synthetic_sessions(self) -> None:
        self.migrate()
        original_sessions = self.db.execute("SELECT * FROM sessions").fetchall()
        for session in (None, "alice"):
            key = "account" if session is None else "session"
            self.reserve(key, "alice", session)
            self.declare(key, "alice", session)
            self.assertEqual(self.db.execute(LOOKUP, (key, "alice", session, "upload")).fetchone(),
                             ("r2_operation", "class_a", None, "test", "pending"))
            with self.assertRaisesRegex(sqlite3.IntegrityError, "resource_observation_pending"):
                self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=0 WHERE id=?", (key,))
            self.db.execute("INSERT INTO usage_ledger VALUES(?,?,?,'r2_operation',1,'class_a',1,'test',NULL,1)",
                            (key, key, "resource:upload"))
            self.db.execute("UPDATE resource_observations SET quantity='1',status='observed',observed_at=1 WHERE run_id=?", (key,))
            self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=1 WHERE id=?", (key,))
        self.assertEqual(self.db.execute("SELECT spent_microusd,reserved_microusd FROM usage_accounts WHERE account_id='alice'").fetchone(), (2, 0))
        self.assertEqual(self.db.execute("SELECT * FROM sessions").fetchall(), original_sessions)

    def test_null_scope_never_crosses_account_or_reservation_boundaries(self) -> None:
        self.migrate()
        self.reserve("account", "alice", None)
        self.reserve("session", "alice", "alice")
        for key, owner, session in (("account", "bob", None), ("account", "alice", "alice"),
                                     ("session", "alice", None), ("session", "bob", "bob")):
            self.declare(key, owner, session)
            self.assertIsNone(self.db.execute(LOOKUP, (key, owner, session, "upload")).fetchone())
            with self.assertRaisesRegex(sqlite3.IntegrityError, "resource_observation_owner_mismatch"):
                self.db.execute("INSERT INTO resource_observations VALUES(?,?,?,'forged','r2_operation','class_a',NULL,'test',NULL,'pending')",
                                (key, owner, session))
        self.declare("account", "alice", None)
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_resource_observation"):
            self.db.execute("UPDATE resource_observations SET session_id='alice' WHERE run_id='account'")

    def test_storage_retention_scopes_match_and_cannot_be_reassigned(self) -> None:
        self.migrate()
        self.storage("account-file", "alice", None)
        self.storage("session-file", "alice", "alice")
        self.deficit("account-file", "alice", None, "account-retention")
        self.deficit("session-file", "alice", "alice", "session-retention")
        for session in ("alice", "bob"):
            with self.assertRaisesRegex(sqlite3.IntegrityError, "storage_object_owner_mismatch"):
                self.storage("forged-" + session, "bob" if session == "alice" else "alice", session)
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_storage_charge"):
            self.db.execute("UPDATE resource_storage_deficits SET session_id='alice' WHERE object_key='account-file'")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_storage_charge"):
            self.db.execute("UPDATE resource_storage_deficits SET session_id=NULL WHERE object_key='session-file'")
        for key, session in (("wrong-null", None), ("wrong-session", "alice")):
            self.storage(key, "alice", "alice" if session is None else None)
            with self.assertRaisesRegex(sqlite3.IntegrityError, "storage_deficit_owner_mismatch"):
                self.deficit(key, "alice", session, key)

    def test_migration_rollback_restores_schema_and_data(self) -> None:
        self.reserve("legacy", "alice", "alice")
        self.declare("legacy", "alice", "alice")
        self.db.commit()
        before = self.db.execute("SELECT sql FROM sqlite_schema WHERE type='table' AND name='resource_observations'").fetchone()
        with self.assertRaises(sqlite3.OperationalError):
            self.db.executescript("BEGIN;\n" + MIGRATION.read_text(encoding="utf-8") + "\nSELECT missing_migration_guard();\nCOMMIT;")
        self.db.rollback()
        self.assertEqual(self.db.execute("SELECT sql FROM sqlite_schema WHERE type='table' AND name='resource_observations'").fetchone(), before)
        self.assertEqual(self.db.execute("SELECT session_id FROM resource_observations").fetchall(), [("alice",)])
        self.assertEqual(self.db.execute("PRAGMA foreign_key_check").fetchall(), [])


if __name__ == "__main__":
    unittest.main()
