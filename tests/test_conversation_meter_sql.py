"""Conversation actor grants require actual, still-reserved resource funding."""
from __future__ import annotations

import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/conversation_meter.rs").read_text(encoding="utf-8")
FUNDING: str = next(iter(re.findall(r'const FUNDING_SQL: &str = "([^"\n]+)";', SOURCE)))


class ConversationMeterSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("alice", "bob"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,?,?)", (owner, "issuer", owner, 0, 0))
            self.db.execute("INSERT INTO usage_accounts VALUES(?,10000,0,0,0)", (owner,))
        self.db.execute("INSERT INTO agents VALUES('agent','alice','agent','{}',0,0,NULL)")
        self.db.execute("INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at) VALUES('session','alice','agent','do','idle','{}','pin',0,0)")

    def tearDown(self) -> None:
        self.db.close()

    def reserve(self, session: str | None = None) -> None:
        self.db.execute("INSERT INTO usage_reservations(id,account_id,session_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES('run','alice',?,'conversation:run','hash',100,'reserved',0,0)", (session,))

    def observe(self, session: str | None = None) -> None:
        self.db.execute("INSERT INTO resource_observations(run_id,account_id,session_id,event_key,resource,unit,quantity,price_version,observed_at,status) VALUES('run','alice',?,'event','durable_object','wall_ms',NULL,'tariff',NULL,'pending')", (session,))

    def funded(self, owner: str = "alice", session: str | None = None, event: str = "event") -> bool:
        return self.db.execute(FUNDING, ("run", owner, session, event)).fetchone() is not None

    def test_account_and_session_grants_do_not_cross_bindings(self) -> None:
        self.reserve("session")
        self.observe("session")
        self.assertTrue(self.funded(session="session"))
        self.assertFalse(self.funded())
        self.assertFalse(self.funded(session="other"))
        self.assertFalse(self.funded(owner="bob", session="session"))

    def test_unfunded_or_unrelated_event_never_authorizes_actor_work(self) -> None:
        self.assertFalse(self.funded())
        self.reserve()
        self.assertFalse(self.funded())
        self.observe()
        self.assertTrue(self.funded())
        self.assertFalse(self.funded(event="another-attempt"))

    def test_settled_reservation_and_already_observed_grant_cannot_replay(self) -> None:
        self.reserve()
        self.observe()
        self.db.execute("UPDATE resource_observations SET quantity='1',status='observed',observed_at=1")
        self.assertFalse(self.funded())
        self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=0")
        self.assertFalse(self.funded())

    def test_gate_does_not_clear_unknown_measurements_or_release_budget(self) -> None:
        self.reserve()
        self.observe()
        self.assertFalse(self.funded(event="missing"))
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone(), ("reserved",))
        self.assertEqual(self.db.execute("SELECT status,quantity FROM resource_observations").fetchone(), ("pending", None))

    def test_pending_actor_attempt_prevents_clearing_its_hold(self) -> None:
        self.reserve()
        self.observe()
        with self.assertRaisesRegex(sqlite3.IntegrityError, "resource_observation_pending"):
            self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=0")
        self.assertTrue(self.funded())
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone(), ("reserved",))


if __name__ == "__main__":
    unittest.main()
