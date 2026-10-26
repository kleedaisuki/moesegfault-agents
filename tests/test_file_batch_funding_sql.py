"""Execute the batch-funding, admission and cursor SQL from the Worker sources."""
from __future__ import annotations

import json
from pathlib import Path
import re
import sqlite3
import unittest

ROOT: Path = Path(__file__).resolve().parents[1]
FUND: str = (ROOT / "crates/agents-worker/src/file_index_funding.rs").read_text(encoding="utf-8")
ROUTES: str = (ROOT / "crates/agents-worker/src/file_search_routes.rs").read_text(encoding="utf-8")
INDEX: str = (ROOT / "crates/agents-worker/src/file_index.rs").read_text(encoding="utf-8")
SCHEDULER: str = (ROOT / "crates/agents-worker/src/file_index_scheduler.rs").read_text(encoding="utf-8")


def sql(source: str, prefix: str) -> str:
    return next(value for value in re.findall(r'"([^"\n]+)"', source) if value.startswith(prefix))


class BatchFundingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.executescript("""CREATE TABLE accounts(id TEXT PRIMARY KEY);
        CREATE TABLE space_files(id TEXT PRIMARY KEY,account_id TEXT,filename TEXT,status TEXT,sha256 TEXT);
        CREATE TABLE usage_accounts(account_id TEXT PRIMARY KEY,budget_microusd INTEGER,spent_microusd INTEGER,reserved_microusd INTEGER);
        CREATE TABLE usage_reservations(id TEXT PRIMARY KEY,account_id TEXT,state TEXT,updated_at INTEGER);
        CREATE TABLE resource_observations(run_id TEXT,account_id TEXT,event_key TEXT,status TEXT);
        CREATE TABLE usage_ledger(reservation_id TEXT);""")
        for name in ("0017_file_search.sql", "0028_file_batch_funding.sql"):
            self.db.executescript((ROOT / "migrations" / name).read_text(encoding="utf-8"))
        self.db.executemany("INSERT INTO accounts VALUES(?)", [("a",), ("other",)])
        self.db.execute("INSERT INTO usage_accounts VALUES('a',100000,0,0)")
        self.db.execute("INSERT INTO vector_stores(id,account_id,created_at,last_active_at) VALUES('store','a',0,0)")
        self.db.execute("INSERT INTO space_files VALUES('file','a','notes.txt','processed','digest')")
        self.db.execute("INSERT INTO vector_store_files(vector_store_id,file_id,account_id,file_sha256,status,created_at,updated_at,generation,lease_id,lease_until,lease_turn) VALUES('store','file','a','digest','in_progress',0,0,'generation','lease',10000,'turn')")

    def tearDown(self) -> None:
        self.db.close()

    def plan(self, owner: str = "a", lease: str | None = "lease", pricing: str = '{"version":"first"}') -> None:
        self.db.execute(sql(FUND, "INSERT INTO file_search_funding"), ("fund",owner,"store","file","generation","embed:0:32","32000","1",pricing,"32000","1",lease))

    def pause(self, lease: str | None = "lease") -> None:
        self.db.execute(sql(FUND, "UPDATE vector_store_files SET funding_blocked=1"), ("a","store","file","generation",lease,"52000",'{"code":"server_error","message":"file_search_budget_required"}',"1"))

    def retry(self, owner: str = "a") -> list[tuple[str]]:
        return self.db.execute(sql(ROUTES, "UPDATE vector_store_files SET funding_blocked=0"), (owner,"store","file")).fetchall()

    def chunk(self, phase: str = "pending") -> None:
        self.db.execute("INSERT INTO file_search_chunks(chunk_id,account_id,vector_store_id,file_id,file_sha256,generation,ordinal,text,byte_start,byte_end,embedding_model,embedding_dimensions,embedding_revision,vector_status) VALUES('chunk','a','store','file','digest','generation',0,'text',0,4,'model',2,'revision',?)", (phase,))

    def test_scope_and_lease_fence_the_original_pinned_funding_plan(self) -> None:
        self.plan(owner="other")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM file_search_funding").fetchone(), (0,))
        self.plan(lease="stale")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM file_search_funding").fetchone(), (0,))
        self.plan()
        self.plan(pricing='{"version":"replacement"}')
        self.assertEqual(json.loads(self.db.execute("SELECT pricing_json FROM file_search_funding").fetchone()[0])["version"], "first")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE file_search_funding SET amount_microusd=40000")

    def test_pause_releases_only_its_lease_and_removes_it_from_background_discovery(self) -> None:
        self.pause(lease="foreign")
        self.assertEqual(self.db.execute("SELECT funding_blocked FROM vector_store_files").fetchone(), (0,))
        self.pause()
        self.assertEqual(self.db.execute("SELECT funding_blocked,lease_id,lease_until FROM vector_store_files").fetchone(), (1,None,0))
        rows: list[tuple[object, ...]] = self.db.execute(sql(SCHEDULER, "SELECT vf.account_id,vf.vector_store_id"), ("10001",)).fetchall()
        self.assertEqual(rows, [])

    def test_retry_requires_credit_and_preserves_paid_batch_generation(self) -> None:
        self.pause()
        self.db.execute("UPDATE usage_accounts SET budget_microusd=30000")
        self.assertEqual(self.retry(), [])
        self.db.execute("UPDATE usage_accounts SET budget_microusd=52000")
        self.assertEqual(self.retry(owner="other"), [])
        self.assertEqual(self.retry(), [("file",)])
        self.assertEqual(self.db.execute("SELECT generation,funding_blocked,retry_at FROM vector_store_files").fetchone(), ("generation",0,0))

    def test_unknown_embedding_or_conversion_cannot_be_resubmitted_by_retry(self) -> None:
        self.chunk("embedding")
        self.pause()
        self.assertEqual(self.retry(), [])
        self.db.execute("DELETE FROM file_search_chunks")
        self.db.execute("UPDATE vector_store_files SET funding_blocked=0,lease_id='lease',lease_until=10000")
        self.plan()
        self.db.execute("INSERT INTO file_search_funding VALUES('convert','a','store','file','generation','convert','100',1,'{}',100,0)")
        self.db.execute("INSERT INTO resource_observations VALUES('convert','a','provider','pending')")
        self.pause()
        self.assertEqual(self.retry(), [])

    def test_unfunded_owner_can_cancel_but_cannot_delete_independent_source(self) -> None:
        self.pause()
        self.db.execute("UPDATE usage_accounts SET budget_microusd=0")
        cancel: str = sql(ROUTES, "UPDATE vector_store_files SET status='cancelled',removed_at=COALESCE")
        self.assertEqual(self.db.execute(cancel, ("other","store","file","2")).fetchall(), [])
        self.assertEqual(self.db.execute(cancel, ("a","store","file","2")).fetchall(), [("file",)])
        self.assertEqual(self.db.execute("SELECT status FROM space_files").fetchone(), ("processed",))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM file_search_cleanup_jobs").fetchone(), (1,))

    def test_cursor_and_failed_turn_release_are_lease_fenced(self) -> None:
        cursor: str = sql(INDEX, "UPDATE vector_store_files SET staged_chunks=")
        self.assertEqual(self.db.execute(cursor, ("a","store","file","generation","foreign","1","32","0")).fetchall(), [])
        self.assertEqual(self.db.execute(cursor, ("a","store","file","generation","lease","1","32","0")).fetchall(), [("file",)])
        self.assertEqual(self.db.execute(cursor, ("a","store","file","generation","lease","1","64","0")).fetchall(), [])
        release: str = sql(SCHEDULER, "UPDATE vector_store_files SET lease_id=NULL,lease_until=0,retry_at=")
        self.db.execute(release, ("a","store","file","generation","foreign","2"))
        self.assertEqual(self.db.execute("SELECT lease_id FROM vector_store_files").fetchone(), ("lease",))

    def test_public_failed_admission_preserves_internal_generation_and_standard_counts(self) -> None:
        self.pause()
        self.assertEqual(self.db.execute("SELECT status,public_status FROM vector_store_public_files").fetchone(), ("in_progress","failed"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM file_search_cleanup_jobs").fetchone(), (0,))
        stores: str = (ROOT / "crates/agents-worker/src/vector_store_routes.rs").read_text(encoding="utf-8")
        self.db.row_factory = sqlite3.Row
        row: sqlite3.Row = self.db.execute(sql(stores,"SELECT vs.id,vs.name")+" WHERE vs.account_id='a' GROUP BY vs.id").fetchone()
        self.assertEqual((row["in_progress"],row["failed"]), (0,1))
        self.db.row_factory = None
        self.retry()
        self.assertEqual(self.db.execute("SELECT public_status FROM vector_store_public_files").fetchone(), ("in_progress",))

    def test_unknown_observation_cannot_race_with_unused_reservation_release(self) -> None:
        self.db.execute("INSERT INTO usage_reservations VALUES('fund','a','reserved',0)")
        release: str = sql(FUND, "UPDATE usage_reservations SET state='released'")
        self.db.execute("INSERT INTO resource_observations VALUES('fund','a','provider','pending')")
        self.db.execute(release, ("fund","a","2"))
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone(), ("reserved",))
        self.db.execute("DELETE FROM resource_observations")
        self.db.execute(release, ("fund","a","2"))
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone(), ("released",))


if __name__ == "__main__":
    unittest.main()
