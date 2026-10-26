"""New generation-capacity clock SQL; no historical runtime tests are replayed."""
from __future__ import annotations

import json
from pathlib import Path
import re
import sqlite3
import unittest

ROOT: Path = Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/file_index_storage.rs").read_text(encoding="utf-8")


def sql(prefix: str) -> str:
    return next(statement for statement in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if statement.startswith(prefix))


class VectorStorageClockTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.executescript("CREATE TABLE accounts(id TEXT PRIMARY KEY); CREATE TABLE vector_store_files(account_id TEXT,vector_store_id TEXT,file_id TEXT,generation TEXT,lease_id TEXT,status TEXT,source_staged INTEGER,lease_until INTEGER); CREATE TABLE file_search_chunks(account_id TEXT,vector_store_id TEXT,file_id TEXT,generation TEXT);")
        self.db.executescript((ROOT / "migrations/0024_vector_storage_clock.sql").read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('a')")
        self.db.execute("INSERT INTO vector_store_files VALUES('a','store','file','gen','writer','in_progress',1,10000)")

    def tearDown(self) -> None:
        self.db.close()

    def allocate(self, dimensions: int = 1024, pricing: str = '{"revision":"original"}') -> None:
        self.db.execute(sql("INSERT INTO file_search_storage_clocks"), ("clock", "a", "store", "file", "gen", str(dimensions), pricing, "100", "1000", "writer"))

    def test_duplicate_allocation_keeps_original_clock_and_tariff(self) -> None:
        self.allocate()
        self.allocate(4096, '{"revision":"changed"}')
        self.assertEqual(self.db.execute("SELECT COUNT(*),dimensions,pricing_json,created_at_ms,billed_through_ms FROM file_search_storage_clocks").fetchone(), (1, 1024, '{"revision":"original"}', 100, 100))
        self.db.execute(sql("UPDATE file_search_storage_clocks SET removed_at_ms"), ("clock", "a", "500", "store", "file", "gen"))
        self.allocate()
        self.assertEqual(self.db.execute("SELECT removed_at_ms FROM file_search_storage_clocks").fetchone()[0], 500)

    def test_expired_writer_cannot_allocate_unfunded_capacity(self) -> None:
        self.db.execute("UPDATE vector_store_files SET lease_until=50")
        self.allocate()
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM file_search_storage_clocks").fetchone()[0], 0)

    def test_journal_is_stable_and_stale_lease_cannot_advance_or_clear(self) -> None:
        self.allocate()
        claim: str = sql("UPDATE file_search_storage_clocks SET lease_id")
        self.assertEqual(len(self.db.execute(claim, ("clock", "first", "200", "100")).fetchall()), 1)
        pending: str = json.dumps({"from_ms": 100, "to_ms": 150, "quantity": 100, "carry": 0, "next_due_ms": 1000, "finalized": False})
        journal: str = sql("UPDATE file_search_storage_clocks SET pending_json")
        self.assertEqual(self.db.execute(journal, ("clock", "first", pending)).fetchall(), [("clock",)])
        self.db.execute(claim, ("clock", "second", "400", "200")).fetchall()
        finish: str = sql("UPDATE file_search_storage_clocks SET billed_through_ms")
        self.db.execute(finish, ("clock", "first", pending, "150", "0", 0, "1000"))
        self.assertEqual(self.db.execute("SELECT billed_through_ms,pending_json FROM file_search_storage_clocks").fetchone(), (100, pending))
        self.db.execute(sql("UPDATE file_search_storage_clocks SET debt"), ("clock", "first", "2000"))
        self.assertEqual(self.db.execute("SELECT lease_id,debt FROM file_search_storage_clocks").fetchone(), ("second", 0))
        self.db.execute(finish, ("clock", "second", pending, "150", "0", 0, "1000"))
        self.assertEqual(self.db.execute("SELECT billed_through_ms,pending_json FROM file_search_storage_clocks").fetchone(), (150, None))

    def test_unconfirmed_physical_generation_cannot_close_capacity_clock(self) -> None:
        self.allocate()
        self.db.execute("INSERT INTO file_search_chunks VALUES('a','store','file','gen')")
        close: str = sql("UPDATE file_search_storage_clocks SET removed_at_ms")
        self.db.execute(close, ("clock", "a", "500", "store", "file", "gen"))
        self.assertIsNone(self.db.execute("SELECT removed_at_ms FROM file_search_storage_clocks").fetchone()[0])
        self.db.execute("DELETE FROM file_search_chunks")
        self.db.execute(close, ("clock", "a", "500", "store", "file", "gen"))
        self.assertEqual(self.db.execute("SELECT removed_at_ms,next_due_ms FROM file_search_storage_clocks").fetchone(), (500, 0))


if __name__ == "__main__":
    unittest.main()
