"""Actual SQLite capacity races and lifecycle transitions, without a Worker build."""
from __future__ import annotations

import concurrent.futures
import json
import pathlib
import random
import re
import sqlite3
import tempfile
import threading
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/conversation_quota.rs").read_text(encoding="utf-8")


def query(prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if sql.startswith(prefix))


RESERVE: str = query("INSERT INTO conversation_capacity_objects(")
RETRY: str = query("UPDATE conversation_capacity_objects SET state='reserved'")
COMMIT: str = query("UPDATE conversation_capacity_objects SET state='retained',updated_at_ms=CAST(?4")
RELEASE: str = query("UPDATE conversation_capacity_objects SET state='released',release_proof=?4")
BATCH: str = query("WITH desired AS")
BATCH_COMMIT: str = query("UPDATE conversation_capacity_objects SET state='retained',updated_at_ms=CAST(?3")
BATCH_RELEASE: str = query("UPDATE conversation_capacity_objects SET state='released',release_proof='never_published'")


def initialize(db: sqlite3.Connection) -> None:
    db.execute("PRAGMA foreign_keys=ON")
    for migration in sorted((ROOT / "migrations").glob("*.sql")):
        db.executescript(migration.read_text(encoding="utf-8"))
    assert db.execute("SELECT 1 FROM sqlite_master WHERE name='conversation_capacity_objects'").fetchone() is not None
    for owner in ("owner", "other"):
        db.execute("INSERT INTO accounts VALUES(?,?,?,?,?)", (owner, "issuer", owner, 0, 0))
        db.execute("INSERT INTO conversation_capacity_accounts(account_id) VALUES(?)", (owner,))
    db.commit()


def reserve(db: sqlite3.Connection, object_id: str, size: int, limit: int,
            owner: str = "owner", conversation: str = "conv") -> str | None:
    db.execute(RESERVE, (owner, conversation, object_id, "a" * 64, str(size), "0", str(limit)))
    row: tuple[str] | None = db.execute(
        "SELECT state FROM conversation_capacity_objects WHERE account_id=? AND conversation_id=? AND object_id=?",
        (owner, conversation, object_id),
    ).fetchone()
    if row is None:
        return None
    if row[0] == "released":
        db.execute(RETRY, (owner, conversation, object_id, "1", str(limit)))
        row = db.execute(
            "SELECT state FROM conversation_capacity_objects WHERE account_id=? AND conversation_id=? AND object_id=?",
            (owner, conversation, object_id),
        ).fetchone()
    return row[0] if row is not None and row[0] != "released" else None


def counters(db: sqlite3.Connection, owner: str = "owner") -> tuple[int, int]:
    row: tuple[int, int] = db.execute(
        "SELECT reserved_bytes,retained_bytes FROM conversation_capacity_accounts WHERE account_id=?", (owner,)
    ).fetchone()
    return row


class ConversationQuotaTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        initialize(self.db)

    def tearDown(self) -> None:
        self.db.close()

    def test_owner_wide_budget_and_idempotency_after_full_or_downgrade(self) -> None:
        self.assertEqual(reserve(self.db, "a", 60, 100), "reserved")
        self.assertIsNone(reserve(self.db, "b", 60, 100, conversation="second"))
        self.assertEqual(reserve(self.db, "a", 60, 10), "reserved")
        self.assertEqual(reserve(self.db, "a", 60, 100, owner="other"), "reserved")
        self.assertEqual(counters(self.db), (60, 0))
        self.assertEqual(counters(self.db, "other"), (60, 0))

    def test_commit_and_confirmed_removal_are_idempotent(self) -> None:
        reserve(self.db, "a", 60, 100)
        self.db.execute(COMMIT, ("owner", "conv", "a", "2"))
        self.db.execute(COMMIT, ("owner", "conv", "a", "3"))
        self.assertEqual(counters(self.db), (0, 60))
        self.db.execute(RELEASE, ("owner", "conv", "a", "content_removed", "4"))
        self.db.execute(RELEASE, ("owner", "conv", "a", "content_removed", "5"))
        self.assertEqual(counters(self.db), (0, 0))
        self.assertEqual(reserve(self.db, "a", 60, 100), "reserved")
        self.assertEqual(counters(self.db), (60, 0))

    def test_tombstone_or_unpublished_claim_cannot_refund_retained_body(self) -> None:
        reserve(self.db, "a", 60, 100)
        self.db.execute(COMMIT, ("owner", "conv", "a", "2"))
        self.db.execute(RELEASE, ("owner", "conv", "a", "never_published", "3"))
        self.assertEqual(counters(self.db), (0, 60))
        self.assertIsNone(reserve(self.db, "b", 60, 100))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE conversation_capacity_objects SET state='released',release_proof='never_published' WHERE object_id='a'")

    def test_unknown_outcome_never_expires_and_known_absence_can_refund(self) -> None:
        reserve(self.db, "a", 60, 100)
        self.db.execute("UPDATE conversation_capacity_objects SET updated_at_ms=9999999999999 WHERE object_id='a'")
        self.assertIsNone(reserve(self.db, "b", 60, 100))
        self.db.execute(RELEASE, ("owner", "conv", "a", "never_published", "3"))
        self.assertEqual(counters(self.db), (0, 0))
        self.assertEqual(reserve(self.db, "b", 60, 100), "reserved")

    def test_object_identity_is_immutable(self) -> None:
        reserve(self.db, "a", 60, 100)
        for field, value in (("bytes", 61), ("digest", "b" * 64), ("conversation_id", "other")):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(f"UPDATE conversation_capacity_objects SET {field}=? WHERE object_id='a'", (value,))

    def test_atomic_batch_reserves_all_or_none_and_retries_once(self) -> None:
        payloads: list[dict[str, str | int]] = [
            {"conversation_id": "conv", "object_id": str(index), "digest": "a" * 64, "bytes": 40}
            for index in range(2)
        ]
        encoded: str = json.dumps(payloads)
        self.db.execute(BATCH, ("owner", encoded, "0", "70"))
        self.assertEqual(counters(self.db), (0, 0))
        self.db.execute(BATCH, ("owner", encoded, "1", "100"))
        self.db.execute(BATCH, ("owner", encoded, "2", "100"))
        self.assertEqual(counters(self.db), (80, 0))
        self.db.execute(BATCH_COMMIT, ("owner", encoded, "3"))
        self.db.execute(BATCH_COMMIT, ("owner", encoded, "4"))
        self.assertEqual(counters(self.db), (0, 80))
        self.db.execute(BATCH_RELEASE, ("owner", encoded, "5"))
        self.assertEqual(counters(self.db), (0, 80), "Retained header blocks cannot be refunded as unpublished")

    def test_batch_identity_conflict_never_partially_allocates(self) -> None:
        reserve(self.db, "existing", 20, 100)
        payloads: list[dict[str, str | int]] = [
            {"conversation_id": "conv", "object_id": "new", "digest": "a" * 64, "bytes": 20},
            {"conversation_id": "conv", "object_id": "existing", "digest": "b" * 64, "bytes": 20},
        ]
        self.db.execute(BATCH, ("owner", json.dumps(payloads), "0", "100"))
        self.assertEqual(counters(self.db), (20, 0))
        self.assertIsNone(self.db.execute("SELECT 1 FROM conversation_capacity_objects WHERE object_id='new'").fetchone())

    def test_transition_sequences_conserve_the_two_counters(self) -> None:
        rng: random.Random = random.Random(73639)
        expected: dict[str, tuple[int, str]] = {}
        for step in range(500):
            object_id: str = f"object_{rng.randrange(20)}"
            size: int = 10 + int(object_id.split("_")[1])
            action: str = rng.choice(("reserve", "commit", "release"))
            if action == "reserve":
                state: str | None = reserve(self.db, object_id, size, 160)
                if state is not None:
                    expected[object_id] = (size, state)
            elif action == "commit":
                self.db.execute(COMMIT, ("owner", "conv", object_id, str(step)))
                if object_id in expected and expected[object_id][1] == "reserved":
                    expected[object_id] = (size, "retained")
            else:
                self.db.execute(RELEASE, ("owner", "conv", object_id, "content_removed", str(step)))
                if object_id in expected:
                    expected[object_id] = (size, "released")
            reserved: int = sum(n for n, state in expected.values() if state == "reserved")
            retained: int = sum(n for n, state in expected.values() if state == "retained")
            self.assertEqual(counters(self.db), (reserved, retained))
            self.assertLessEqual(reserved + retained, 160)

    def test_real_concurrent_connections_cannot_overbook(self) -> None:
        temporary: pathlib.Path = ROOT / ".temp"
        temporary.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="conversation-quota-", dir=temporary) as directory:
            path: pathlib.Path = pathlib.Path(directory).resolve() / "capacity.sqlite"
            self.assertIn(temporary.resolve(), path.parents)
            db: sqlite3.Connection = sqlite3.connect(path)
            initialize(db)
            db.close()
            barrier: threading.Barrier = threading.Barrier(2)

            def contender(object_id: str) -> bool:
                connection: sqlite3.Connection = sqlite3.connect(path, timeout=5)
                try:
                    connection.execute("PRAGMA foreign_keys=ON")
                    barrier.wait(timeout=5)
                    accepted: bool = reserve(connection, object_id, 60, 100) is not None
                    connection.commit()
                    return accepted
                finally:
                    connection.close()

            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                accepted: list[bool] = list(pool.map(contender, ("one", "two")))
            self.assertEqual(sorted(accepted), [False, True])
            check: sqlite3.Connection = sqlite3.connect(path)
            self.assertEqual(counters(check), (60, 0))
            check.close()


if __name__ == "__main__":
    unittest.main()
