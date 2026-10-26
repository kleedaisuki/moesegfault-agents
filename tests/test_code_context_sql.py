"""Owned code routing and retained capacity use actual immutable SQL transitions."""
from __future__ import annotations

from pathlib import Path
import random
import re
import sqlite3
import unittest

ROOT: Path = Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/code_contexts.rs").read_text(encoding="utf-8")


def statement(prefix: str) -> str:
    matches: list[str] = [sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if sql.startswith(prefix)]
    assert len(matches) == 1, prefix
    return matches[0]


CLAIM: str = statement("INSERT INTO code_contexts")
BIND: str = statement("UPDATE code_contexts SET container_id=")
TOUCH: str = statement("UPDATE code_contexts SET last_used_at_ms=")
END: str = statement("UPDATE code_contexts SET state=")
RELEASE: str = statement("UPDATE code_contexts SET allocation_released_at_ms=")


class CodeContextTests(unittest.TestCase):
    db: sqlite3.Connection

    def setUp(self) -> None:
        self.db = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA foreign_keys=ON")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
            self.db.execute("INSERT INTO agents VALUES(?,?,?,'{}',0,0,NULL)", (owner, owner, owner))
            self.db.execute("INSERT INTO usage_accounts(account_id,updated_at) VALUES(?,0)", (owner,))
            for n in range(4):
                session: str = f"{owner}-{n}"
                self.db.execute("INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at) VALUES(?,?,?,?,'active','{}','pin',0,0)", (session, owner, owner, session))
                self.db.execute("INSERT INTO usage_reservations(id,account_id,session_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES(?,?,?,?,'fixture',0,'reserved',0,0)", (f"run-{session}", owner, session, f"fixture-{session}"))

    def tearDown(self) -> None:
        self.db.close()

    def claim(self, allocation: str, source: str = "a-0", capacity: int = 1, now: int = 100, owner: str = "a") -> bool:
        return self.db.execute(CLAIM, (allocation, owner, source, "basic1g", f"run-{source}", str(now), str(now + 1_200_000), str(capacity))).rowcount == 1

    def bind(self, allocation: str, source: str = "a-0", now: int = 200, owner: str = "a") -> str:
        cid: str = "cntr_" + allocation.encode().hex().ljust(32, "a")[:32]
        self.db.execute(BIND, (cid, "boot", "policy", str(now), str(now + 1_200_000), allocation, owner, source, "basic1g"))
        return cid

    def test_conversation_archive_does_not_free_retained_code_capacity(self) -> None:
        self.assertTrue(self.claim("first"))
        self.bind("first")
        self.db.execute("UPDATE sessions SET status='closed' WHERE id='a-0'")
        self.assertFalse(self.claim("blocked", "a-1"))
        self.db.execute(END, ("expired", "300", "a", "first"))
        self.assertFalse(self.claim("still-held", "a-1"))
        self.db.execute(RELEASE, ("400", "a", "first"))
        self.assertTrue(self.claim("next", "a-1"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_ledger").fetchone()[0], 0)

    def test_replacing_a_retired_namespace_on_the_same_vm_does_not_double_allocate(self) -> None:
        self.assertTrue(self.claim("first"))
        old: str = self.bind("first")
        self.db.execute(END, ("expired", "300", "a", "first"))
        self.assertTrue(self.claim("replacement", now=400))
        new: str = self.bind("replacement", now=500)
        self.assertNotEqual(new, old)
        self.assertEqual(self.db.execute("SELECT COUNT(DISTINCT source_session_id) FROM code_contexts WHERE allocation_released_at_ms IS NULL").fetchone()[0], 1)
        self.assertFalse(self.claim("parallel", now=600))

    def test_identity_owner_funding_and_terminal_generation_are_not_redirectable(self) -> None:
        self.assertFalse(self.claim("foreign-source", "b-0", owner="a"))
        self.assertTrue(self.claim("first"))
        cid: str = self.bind("first")
        self.assertIsNone(self.db.execute("SELECT container_id FROM code_contexts WHERE account_id='b' AND container_id=?", (cid,)).fetchone())
        for assignment in ("account_id='b'", "source_session_id='a-1'", "source_profile='standard4g'", "daemon_boot_id='other'", "container_id='cntr_changed'", "policy_revision='changed'"):
            with self.subTest(assignment=assignment), self.assertRaises(sqlite3.IntegrityError):
                self.db.execute("UPDATE code_contexts SET " + assignment + " WHERE allocation_id='first'")
        self.db.execute(END, ("expired", "300", "a", "first"))
        self.bind("first", now=400)
        self.assertEqual(self.db.execute("SELECT state FROM code_contexts WHERE allocation_id='first'").fetchone()[0], "expired")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE code_contexts SET state='ready',ended_at_ms=NULL WHERE allocation_id='first'")
        self.db.execute(RELEASE, ("400", "a", "first"))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE code_contexts SET allocation_released_at_ms=NULL WHERE allocation_id='first'")

    def test_random_capacity_and_clock_transitions_preserve_the_plan_bound(self) -> None:
        rng: random.Random = random.Random(20261026)
        for tick in range(1, 401):
            now: int = tick * 1000
            action: int = rng.randrange(4)
            source: str = "a-" + str(rng.randrange(4))
            if action == 0:
                allocation: str = f"slot-{tick}"
                if self.claim(allocation, source, capacity=2, now=now):
                    self.bind(allocation, source, now + 1)
            else:
                rows: list[sqlite3.Row] = self.db.execute("SELECT allocation_id,state FROM code_contexts WHERE account_id='a'").fetchall()
                if rows:
                    selected: sqlite3.Row = rng.choice(rows)
                    allocation = selected["allocation_id"]
                    if action == 1:
                        self.db.execute(TOUCH, (str(now), str(now + 1_200_000), "a", allocation))
                    elif action == 2:
                        self.db.execute(END, ("expired", str(now), "a", allocation))
                    else:
                        self.db.execute(RELEASE, (str(now), "a", allocation))
            active: int = self.db.execute("SELECT COUNT(DISTINCT source_session_id) FROM code_contexts WHERE account_id='a' AND allocation_released_at_ms IS NULL").fetchone()[0]
            self.assertLessEqual(active, 2)
            self.assertEqual(self.db.execute("SELECT COUNT(*) FROM code_contexts WHERE state='ready' AND (container_id IS NULL OR ended_at_ms IS NOT NULL)").fetchone()[0], 0)

    def test_explicit_deletion_of_an_expired_resource_preserves_its_identity_and_end_time(self) -> None:
        self.assertTrue(self.claim("expired"))
        cid: str = self.bind("expired")
        self.db.execute(END, ("expired", "300", "a", "expired"))
        self.db.execute(END, ("deleted", "400", "a", "expired"))
        row: sqlite3.Row = self.db.execute("SELECT state,container_id,ended_at_ms FROM code_contexts WHERE allocation_id='expired'").fetchone()
        self.assertEqual(tuple(row), ("deleted", cid, 300))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE code_contexts SET state='ready',ended_at_ms=NULL WHERE allocation_id='expired'")

    def test_finished_run_fence_prevents_a_late_uncreated_capacity_reservation(self) -> None:
        self.db.execute("UPDATE usage_reservations SET execution_finished_at=300 WHERE id='run-a-0'")
        self.assertFalse(self.claim("late"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM code_contexts").fetchone()[0], 0)
        self.assertTrue(self.claim("independent", "a-1"))


if __name__ == "__main__":
    unittest.main()
