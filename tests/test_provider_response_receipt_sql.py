"""Exercise actual receipt admission SQL and immutable runtime capacity fences."""
from __future__ import annotations
from pathlib import Path
import re
import sqlite3
import unittest

ROOT: Path = Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/provider_response_capacity.rs").read_text(encoding="utf-8")
GC: str = (ROOT / "crates/agents-worker/src/provider_response_gc.rs").read_text(encoding="utf-8")


def sql(prefix: str, source: str = SOURCE) -> str:
    return next(s for s in re.findall(r'\.prepare\("([^"\n]+)"\)', source) if s.startswith(prefix))


class ReceiptCapacityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.execute("CREATE TABLE accounts(id TEXT PRIMARY KEY)")
        self.db.executemany("INSERT INTO accounts VALUES(?)", [("a",), ("b",)])
        self.db.executescript((ROOT / "migrations/0029_provider_response_receipts.sql").read_text(encoding="utf-8"))

    def tearDown(self) -> None:
        self.db.close()

    def reserve(self, operation: str, owner: str = "a", size: int = 100, limit: int = 200) -> None:
        self.db.execute(sql("INSERT INTO provider_response_operations"), (owner, operation, None, None, "2", str(size), "0", str(limit)))

    def frame(self, operation: str, identity: str = "a", size: int = 40) -> None:
        self.db.execute(sql("INSERT INTO provider_response_frames"), ("a", operation, identity * 64, '{"body":"metadata-only"}', str(size)))

    def test_capacity_is_owner_scoped_and_atomic_including_concurrent_operations(self) -> None:
        for size in range(1, 40):
            operation = str(size)
            self.reserve(operation, size=size, limit=200)
            used = self.db.execute("SELECT SUM(capacity_bytes) FROM provider_response_operations WHERE account_id='a'").fetchone()[0]
            self.assertLessEqual(used, 200)
        self.reserve("foreign", owner="b", size=200)
        self.assertEqual(self.db.execute("SELECT capacity_bytes FROM provider_response_operations WHERE account_id='b'").fetchone()[0], 200)

    def test_descriptor_frame_bound_release_and_tombstone_fence(self) -> None:
        self.reserve("op")
        self.frame("op")
        self.frame("op", "b")
        with self.assertRaises(sqlite3.IntegrityError):
            self.frame("op", "c", 1)
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(sql("UPDATE provider_response_operations SET state='released'"), ("1", "a", "op"))
        self.db.execute("UPDATE provider_response_frames SET deleted=1")
        self.db.execute(sql("UPDATE provider_response_operations SET state='released'"), ("1", "a", "op"))
        self.assertEqual(self.db.execute("SELECT state,capacity_bytes FROM provider_response_operations").fetchone(), ("released", 0))
        with self.assertRaises(sqlite3.IntegrityError):
            self.frame("op", "c", 1)
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE provider_response_frames SET deleted=0")

    def test_finalization_releases_unused_capacity_but_never_expires_recovery(self) -> None:
        self.reserve("op")
        self.frame("op", size=23)
        self.db.execute(sql("UPDATE provider_response_operations SET capacity_bytes="), ("a", "op", "recovery", None, "1"))
        self.assertEqual(self.db.execute("SELECT state,capacity_bytes,expires_at_ms FROM provider_response_operations").fetchone(), ("recovery", 23, None))
        self.assertEqual(self.db.execute(sql("SELECT account_id,operation_id", GC), ("999999999", "999999999")).fetchall(), [])

    def test_diagnostic_cleanup_claim_is_single_lease_and_does_not_claim_foreign_owner(self) -> None:
        self.reserve("op")
        self.frame("op")
        self.db.execute(sql("UPDATE provider_response_operations SET capacity_bytes="), ("a", "op", "diagnostic", "10", "1"))
        claim = sql("UPDATE provider_response_operations SET state='deleting'", GC)
        self.assertEqual(self.db.execute(claim, ("100", "b", "op", "11", "11")).fetchall(), [])
        self.assertEqual(self.db.execute(claim, ("100", "a", "op", "11", "11")).fetchall(), [(1,)])
        self.assertEqual(self.db.execute(claim, ("100", "a", "op", "11", "11")).fetchall(), [])
        self.assertEqual(self.db.execute(claim, ("200", "a", "op", "101", "101")).fetchall(), [(2,)])

    def test_descriptor_and_receipt_owner_are_immutable(self) -> None:
        self.reserve("op")
        self.frame("op")
        for statement in ["UPDATE provider_response_frames SET descriptor_json='{}'", "UPDATE provider_response_frames SET account_id='b'", "DELETE FROM provider_response_frames", "UPDATE provider_response_operations SET session_id='other'"]:
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(statement)


if __name__ == "__main__":
    unittest.main()
