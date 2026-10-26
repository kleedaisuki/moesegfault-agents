"""Dedicated external execution has its own memory, cleanup and origin lease."""
from __future__ import annotations

from pathlib import Path
import re
import sqlite3
import unittest

import test_code_context_sql as fixtures

SOURCE: str = (Path(__file__).resolve().parents[1] / "crates/agents-worker/src/code_contexts.rs").read_text(encoding="utf-8")
CLAIM: str = next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if sql.startswith("WITH funded_actor"))


class CodeExecutorSqlTests(unittest.TestCase):
    fixture: fixtures.CodeContextTests
    db: sqlite3.Connection

    def setUp(self) -> None:
        self.fixture = fixtures.CodeContextTests(methodName="runTest")
        self.fixture.setUp()
        self.db = self.fixture.db
        self.db.execute("UPDATE usage_accounts SET budget_microusd=1000")
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO usage_reservations(id,account_id,session_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES(?,?,NULL,?,'cleanup',100,'reserved',0,0)", ("cleanup-" + owner, owner, "cleanup-" + owner))

    def tearDown(self) -> None:
        self.fixture.tearDown()

    def claim(self, owner: str, identifier: str, key: str = "f" * 64, cleanup: str | None = None) -> None:
        self.db.execute(CLAIM, (identifier, owner, owner + "-0", "basic1g", "run-" + owner + "-0", "2", "1200002", "2", "4g", key, cleanup or "cleanup-" + owner))

    def test_external_memory_is_not_the_source_actor_sku_and_executor_is_exclusive(self) -> None:
        self.claim("a", "first")
        row: sqlite3.Row = self.db.execute("SELECT * FROM code_contexts WHERE allocation_id='first'").fetchone()
        self.assertEqual((row["source_profile"], row["executor_kind"], row["memory_limit"]), ("basic1g", "workspace_daemon", "4g"))
        self.claim("b", "collision")
        self.assertIsNone(self.db.execute("SELECT 1 FROM code_contexts WHERE allocation_id='collision'").fetchone())
        self.db.execute("UPDATE code_contexts SET state='expired',ended_at_ms=3,allocation_released_at_ms=3 WHERE allocation_id='first'")
        self.claim("b", "successor")
        self.assertIsNotNone(self.db.execute("SELECT 1 FROM code_contexts WHERE allocation_id='successor'").fetchone())

    def test_cleanup_is_separately_owned_and_ended_runs_cannot_claim(self) -> None:
        self.claim("a", "foreign-cleanup", cleanup="cleanup-b")
        self.assertIsNone(self.db.execute("SELECT 1 FROM code_contexts WHERE allocation_id='foreign-cleanup'").fetchone())
        self.db.execute("UPDATE usage_reservations SET execution_finished_at=1 WHERE id='run-a-0'")
        self.claim("a", "ended-run")
        self.assertIsNone(self.db.execute("SELECT 1 FROM code_contexts WHERE allocation_id='ended-run'").fetchone())

    def test_unused_cleanup_refund_never_discards_an_unknown_observation(self) -> None:
        state_source: str = (Path(__file__).resolve().parents[1] / "crates/agents-worker/src/code_executor_state.rs").read_text(encoding="utf-8")
        release: str = re.search(r'RELEASE_UNUSED_SQL:\s*&str\s*=\s*"([^"\n]+)"', state_source).group(1)
        self.db.execute("INSERT INTO resource_observations(run_id,account_id,session_id,event_key,resource,unit,quantity,price_version,observed_at,status) VALUES('cleanup-a','a',NULL,'unknown','durable_object','wall_ms',NULL,'fixture',NULL,'pending')")
        self.db.execute(release, ("cleanup-a", "a", "3"))
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations WHERE id='cleanup-a'").fetchone()[0], "reserved")
        self.db.execute(release, ("cleanup-b", "b", "3"))
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations WHERE id='cleanup-b'").fetchone()[0], "released")

    def test_pinned_backend_memory_cleanup_and_origin_cannot_be_rewritten(self) -> None:
        self.claim("a", "frozen")
        for field, value in (("executor_kind", "platform"), ("memory_limit", "16g"), ("cleanup_run_id", "cleanup-b"), ("executor_lease_key", "e" * 64)):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(f"UPDATE code_contexts SET {field}=? WHERE allocation_id='frozen'", (value,))


if __name__ == "__main__":
    unittest.main()
