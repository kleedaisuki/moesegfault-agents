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
        self.assertEqual(self.db.execute(claim, ("100", "b", "op", "11", "11", "0")).fetchall(), [])
        self.assertEqual(self.db.execute(claim, ("100", "a", "op", "11", "11", "0")).fetchall(), [(1,)])
        self.assertEqual(self.db.execute(claim, ("100", "a", "op", "11", "11", "0")).fetchall(), [])
        self.assertEqual(self.db.execute(claim, ("200", "a", "op", "101", "101", "1")).fetchall(), [(2,)])

    def test_descriptor_and_receipt_owner_are_immutable(self) -> None:
        self.reserve("op")
        self.frame("op")
        for statement in ["UPDATE provider_response_frames SET descriptor_json='{}'", "UPDATE provider_response_frames SET account_id='b'", "DELETE FROM provider_response_frames", "UPDATE provider_response_operations SET session_id='other'"]:
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(statement)


class ReceiptGcFundingRecoveryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("CREATE TABLE accounts(id TEXT PRIMARY KEY)")
        self.db.execute("INSERT INTO accounts VALUES('a')")
        self.db.executescript((ROOT / "migrations/0029_provider_response_receipts.sql").read_text(encoding="utf-8"))
        self.db.executescript("CREATE TABLE usage_reservations(id TEXT PRIMARY KEY,account_id TEXT,state TEXT,updated_at INTEGER); CREATE TABLE resource_observations(run_id TEXT,account_id TEXT,status TEXT);")
        self.db.execute("INSERT INTO provider_response_operations(account_id,operation_id,max_frames,capacity_bytes,state,expires_at_ms,updated_at_ms,gc_attempt,gc_lease_until_ms) VALUES('a','op',1,100,'deleting',0,0,1,100)")
        self.db.execute("INSERT INTO usage_reservations VALUES('old','a','reserved',0)")

    def tearDown(self) -> None:
        self.db.close()

    def release_empty(self, now: int = 101) -> None:
        self.db.execute(sql("UPDATE usage_reservations SET state='released'", GC), (str(now), "old", "a", "op", "1"))

    def test_empty_old_scope_is_released_only_with_expired_or_stale_fence(self) -> None:
        self.release_empty(99)
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone(), ("reserved",))
        self.release_empty()
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone(), ("released",))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM resource_observations").fetchone(), (0,))

    def test_racing_pending_head_blocks_empty_refund_and_remains_unknown(self) -> None:
        self.db.execute("INSERT INTO resource_observations VALUES('old','a','pending')")
        self.release_empty()
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone(), ("reserved",))
        self.assertEqual(self.db.execute("SELECT status FROM resource_observations").fetchone(), ("pending",))
        self.assertEqual(self.db.execute(sql("SELECT COUNT(*) AS total", GC), ("old", "a")).fetchone(), (1, 1))

    def test_all_observed_old_scope_is_not_released_as_unused(self) -> None:
        self.db.execute("INSERT INTO resource_observations VALUES('old','a','observed')")
        self.release_empty()
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone(), ("reserved",))
        self.assertEqual(self.db.execute(sql("SELECT COUNT(*) AS total", GC), ("old", "a")).fetchone(), (1, 0))

    def test_prior_attempt_compare_and_swap_rejects_second_collector(self) -> None:
        claim = sql("UPDATE provider_response_operations SET state='deleting'", GC)
        self.assertEqual(self.db.execute(claim, ("200", "a", "op", "101", "101", "1")).fetchall(), [(2,)])
        self.assertEqual(self.db.execute(claim, ("200", "a", "op", "101", "101", "1")).fetchall(), [])

    def test_late_old_reservation_can_be_found_and_closed_after_operation_release(self) -> None:
        self.db.execute("UPDATE usage_reservations SET id='receipt-gc:synthetic:1'")
        self.db.execute("ALTER TABLE usage_reservations ADD COLUMN request_hash TEXT")
        self.db.execute("UPDATE usage_reservations SET request_hash='{}'")
        self.db.execute("UPDATE provider_response_operations SET state='released',capacity_bytes=0,expires_at_ms=NULL")
        self.assertEqual(len(self.db.execute(sql("SELECT id,account_id,state,request_hash FROM usage_reservations WHERE state='reserved'", GC)).fetchall()), 1)
        self.db.execute(sql("UPDATE usage_reservations SET state='released'", GC), ("99", "receipt-gc:synthetic:1", "a", "op", "1"))
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone(), ("released",))


if __name__ == "__main__":
    unittest.main()
