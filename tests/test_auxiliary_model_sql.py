"""Account model operations reuse actual concurrency and immutable money SQL."""
from __future__ import annotations

import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
METER: str = (ROOT / "crates/agents-worker/src/metering.rs").read_text(encoding="utf-8")
SCOPED: str = (ROOT / "crates/agents-worker/src/store_auxiliary.rs").read_text(encoding="utf-8")


def query(source: str, prefix: str) -> str:
    return next(value for value in re.findall(r'\.(?:prepare|statement)\("([^"\n]+)"', source) if value.startswith(prefix))


ADMIT: str = query(METER, "INSERT INTO usage_reservations")
END: str = query(METER, "UPDATE usage_reservations SET execution_finished_at")
ACTIVE: str = query(SCOPED, "SELECT id FROM usage_reservations")
COMPLETION: str = query(SCOPED, "SELECT request_hash AS payload FROM usage_reservations")
APPEND: str = query(METER, "INSERT INTO usage_ledger")
SETTLE: str = query(METER, "UPDATE usage_reservations SET state='settled'")


class AuxiliaryModelSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("alice", "bob"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,?,?)", (owner, "issuer", owner, 0, 0))
            self.db.execute("INSERT INTO usage_accounts VALUES(?,10000,0,0,0)", (owner,))

    def tearDown(self) -> None:
        self.db.close()

    def admit(self, run: str, owner: str = "alice", limit: int = 1, session: str | None = None) -> bool:
        key: str = "run:" + run
        self.db.execute(ADMIT, (run, owner, session, key, "hash:" + run, "10", "1", "1", owner, key, str(limit), owner, str(limit)))
        return self.db.execute("SELECT id FROM usage_reservations WHERE id=?", (run,)).fetchone() is not None

    def completion(self, key: str, owner: str = "alice", amount: int = 30) -> None:
        self.db.execute("""INSERT INTO usage_reservations
            (id,account_id,session_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at)
            VALUES(?,?,NULL,?,?,?,'reserved',1,1)""", (key, owner, "completion:" + key, "payload:" + key, amount))

    def test_account_compaction_uses_concurrency_without_a_synthetic_session(self) -> None:
        self.assertTrue(self.admit("compact_one"))
        self.assertFalse(self.admit("compact_two"))
        self.assertEqual(self.db.execute(ACTIVE, ("compact_one", "alice", None)).fetchone(), ("compact_one",))
        self.assertIsNone(self.db.execute(ACTIVE, ("compact_one", "bob", None)).fetchone())
        self.assertIsNone(self.db.execute(ACTIVE, ("compact_one", "alice", "invented-session")).fetchone())
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM sessions").fetchone(), (0,))
        self.assertTrue(self.admit("bob_one", "bob"))

    def test_known_model_charge_is_immutable_and_settles_once_at_account_scope(self) -> None:
        self.assertTrue(self.admit("compact_one"))
        self.completion("compact_model")
        self.assertEqual(self.db.execute(COMPLETION, ("compact_model", "alice", None)).fetchone(), ("payload:compact_model",))
        self.assertIsNone(self.db.execute(COMPLETION, ("compact_model", "bob", None)).fetchone())
        args: tuple[str, ...] = ("charge", "compact_model", "compact", "model", "15", "tokens", "12", "test-price", "provider-response", "2", "compact_model", "compact")
        self.db.execute(APPEND, args)
        self.db.execute(APPEND, args)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_ledger WHERE reservation_id='compact_model'").fetchone(), (1,))
        self.db.execute(SETTLE, ("compact_model", "2", "compact_model"))
        self.assertEqual(self.db.execute("SELECT spent_microusd,reserved_microusd FROM usage_accounts WHERE account_id='alice'").fetchone(), (12, 10))
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_reservation"):
            self.db.execute("UPDATE usage_reservations SET session_id='invented-session' WHERE id='compact_model'")

    def test_account_and_session_work_share_the_same_execution_limit(self) -> None:
        self.db.execute("INSERT INTO agents VALUES('agent','alice','Agent','{}',0,0,NULL)")
        self.db.execute("""INSERT INTO sessions
            (id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at)
            VALUES('session','alice','agent','do-session','idle','{}','pin',0,0)""")
        self.assertTrue(self.admit("chat_run", session="session"))
        self.assertIsNone(self.db.execute(ACTIVE, ("chat_run", "alice", None)).fetchone())
        self.assertEqual(self.db.execute(ACTIVE, ("chat_run", "alice", "session")).fetchone(), ("chat_run",))
        self.assertFalse(self.admit("compact_run"))
        self.db.execute(END, ("2000", "chat_run", "alice"))
        self.assertTrue(self.admit("compact_run"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM sessions").fetchone(), (1,))

    def test_finishing_execution_does_not_erase_an_unknown_model_hold(self) -> None:
        self.assertTrue(self.admit("compact_one"))
        self.completion("unknown_model")
        self.db.execute(END, ("2000", "compact_one", "alice"))
        self.assertTrue(self.admit("compact_two"))
        self.assertEqual(self.db.execute("SELECT state,reserved_microusd,settled_microusd FROM usage_reservations WHERE id='unknown_model'").fetchone(), ("reserved", 30, None))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_ledger WHERE reservation_id='unknown_model'").fetchone(), (0,))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM sessions").fetchone(), (0,))


if __name__ == "__main__":
    unittest.main()
