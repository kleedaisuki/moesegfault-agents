"""Space capacity follows unique owned bytes, including hidden response blobs."""
from __future__ import annotations

import pathlib
import hashlib
import json
import os
import platform
import re
import sqlite3
import time
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/space.rs").read_text(encoding="utf-8")
REGISTRY: str = (ROOT / "crates/agents-worker/src/workspace_blobs.rs").read_text(encoding="utf-8")


def query(prefix: str, source: str = SOURCE) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', source) if sql.startswith(prefix))


USAGE: str = query("SELECT (SELECT COALESCE(SUM(bytes),0) FROM (SELECT MAX(bytes) AS bytes FROM space_storage_capacity")
ADMIT: str = query("INSERT INTO space_files(")
LINK: str = query("INSERT INTO space_files(id,account_id,space_id,filename,purpose,content_type,object_key,blob_key")
FINISH_DELETE: str = query("UPDATE space_files SET status=CASE deletion_reason")
BIND_FILE: str = query("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) SELECT ?,'space-file'", REGISTRY)
RELEASE_FILE: str = query("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='space-file'", REGISTRY)
CLAIM_BLOB: str = query("UPDATE workspace_blobs SET status='deleting'", REGISTRY)


class SpaceCapacityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA foreign_keys=ON")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
            self.db.execute("INSERT INTO agents VALUES(?,?,?,'{}',0,0,NULL)", (f"agent-{owner}", owner, "test"))
            self.db.execute("INSERT INTO sessions VALUES(?,?,?,?,'closed','{}','pin',0,0,NULL,NULL,NULL)",
                            (f"session-{owner}", owner, f"agent-{owner}", f"do-{owner}"))
            self.db.execute("INSERT INTO spaces VALUES(?,?,'Space',0,0)", (f"space-{owner}", owner))

    def tearDown(self) -> None:
        self.db.close()

    def blob(self, key: str, size: int, *, owner: str = "a", status: str = "ready") -> None:
        digest: str = f"{sum(key.encode()) % 256:02x}" * 32
        self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES(?,?,?,1,0,?,?,1,?,1000)",
                        (key, owner, digest, size, size, status))

    def version(self, identity: str, key: str, size: int) -> None:
        self.db.execute("INSERT INTO workspace_versions(id,account_id,session_id,operation_id,name,object_key,sha256,format_version,generation,archive_bytes,logical_bytes,entries,created_at_ms) VALUES(?,'a','session-a',?,'',?,?,1,0,?,?,1,0)",
                        (identity, identity, key, "ab" * 32, size, size))

    def legacy_upload(self, key: str, size: int, status: str) -> None:
        self.db.execute("INSERT INTO workspace_version_uploads(object_key,account_id,session_id,operation_id,archive_bytes,version_json,created_at_ms,status) VALUES(?,'a','session-a',?,?,'{}',0,?)",
                        (key, key, size, status))

    def file(self, identity: str, size: int, status: str = "processed") -> None:
        self.db.execute("INSERT INTO space_files(id,account_id,space_id,filename,purpose,content_type,object_key,bytes,sha256,status,reservation_id,created_at_ms,updated_at_ms,recovery_at_ms) VALUES(?,'a','space-a',?,'user_data','application/octet-stream',?,?,?,?,?,0,0,0)",
                        (identity, identity, f"file/{identity}", size, "cd" * 32, status, f"upload:{identity}"))

    def admit(self, identity: str, size: int, limit: int, *, files: int = 100, owner: str = "a") -> bool:
        row: sqlite3.Row | None = self.db.execute(ADMIT, (
            identity, owner, f"space-{owner}", identity, "user_data", "application/octet-stream",
            f"file/{identity}", str(size), "cd" * 32, f"upload:{identity}", "0", str(limit), str(files), None,
        )).fetchone()
        return row is not None

    def usage(self, owner: str = "a") -> tuple[int, int, int, int]:
        row: sqlite3.Row = self.db.execute(USAGE, (owner,)).fetchone()
        return row["bytes"], row["files"], row["versions"], row["version_bytes"]

    def image(self, key: str = "image", size: int = 60) -> None:
        self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms,kind,content_type) VALUES(?,'a',?,0,0,?,?,1,'ready',0,'image','image/png')", (key, "ef" * 32, size, size))

    def link(self, identity: str, key: str = "image", *, owner: str = "a", size: int = 60, limit: int = 100, files: int = 100, inline_key: str | None = None) -> bool:
        self.db.execute(LINK, (identity, owner, f"space-{owner}", "generated.png", "image/png", f"space-links/{owner}/{identity}", key, str(size), "ef" * 32,
                               f"image-link:{identity}", "0", inline_key or identity, str(limit), str(files)))
        return self.db.execute("SELECT id FROM space_files WHERE id=?", (identity,)).fetchone() is not None

    def test_hidden_response_blob_blocks_overcommit_at_exact_capacity_boundary(self) -> None:
        self.blob("hidden-response", 60)
        self.assertTrue(self.admit("exact", 40, 100))
        self.assertFalse(self.admit("overflow", 1, 100))
        self.assertEqual(self.usage(), (100, 1, 0, 0))

    def test_current_and_multiple_versions_share_capacity_not_version_count(self) -> None:
        self.blob("shared", 60)
        self.version("saved-a", "shared", 60)
        self.version("saved-b", "shared", 60)
        self.assertEqual(self.usage(), (60, 0, 2, 60))
        self.assertTrue(self.admit("remainder", 40, 100))
        self.assertEqual(self.usage(), (100, 1, 2, 60))

    def test_all_materialized_blob_lifecycle_states_hold_capacity(self) -> None:
        for status in ("uploading", "ready", "deleting"):
            with self.subTest(status=status):
                self.blob(status, 10, status=status)
        self.assertEqual(self.usage(), (30, 0, 0, 0))
        self.assertFalse(self.admit("too-large", 71, 100))
        self.assertTrue(self.admit("fits", 70, 100))

    def test_generated_image_bytes_share_the_same_space_capacity(self) -> None:
        self.image()
        self.assertEqual(self.usage(), (60, 0, 0, 0))
        self.assertFalse(self.admit("too-large", 41, 100))
        self.assertTrue(self.admit("fits", 40, 100))

    def test_legacy_upload_reservations_and_unique_keys_remain_in_capacity(self) -> None:
        self.legacy_upload("pending", 20, "pending")
        self.legacy_upload("abandoned", 30, "abandoned")
        self.legacy_upload("published", 999, "published")
        self.version("saved", "pending", 20)
        self.blob("pending", 20)
        self.assertEqual(self.usage(), (50, 0, 1, 20))
        self.assertTrue(self.admit("fits", 50, 100))
        self.assertFalse(self.admit("over", 1, 100))

    def test_file_states_owner_isolation_and_count_limit_are_preserved(self) -> None:
        for status in ("uploading", "processed", "deleting", "failed", "deleted"):
            self.file(status, 10, status)
        self.blob("foreign", 10_000, owner="b")
        self.assertEqual(self.usage(), (30, 3, 0, 0))
        self.assertFalse(self.admit("count-overflow", 1, 100, files=3))
        self.assertTrue(self.admit("count-fits", 1, 100, files=4))
        self.assertEqual(self.usage("b"), (10_000, 0, 0, 0))

    def test_inconsistent_legacy_metadata_reserves_maximum_bytes_once(self) -> None:
        self.blob("shared", 70)
        self.version("legacy", "shared", 60)
        self.legacy_upload("shared", 80, "abandoned")
        self.assertEqual(self.usage(), (80, 0, 1, 60))
        self.assertTrue(self.admit("fits", 20, 100))
        self.assertFalse(self.admit("over", 1, 100))

    def test_owner_capacity_query_uses_owner_indexes(self) -> None:
        plan: str = str([tuple(row) for row in self.db.execute("EXPLAIN QUERY PLAN " + USAGE, ("a",))])
        self.assertIn("space_files_owner", plan)
        self.assertIn("workspace_blob_capacity", plan)
        self.assertNotIn("SCAN workspace_blobs", plan)

    def test_owner_capacity_query_cost_receipt(self) -> None:
        owners: int = 250
        rows_per_owner: int = 16
        account_rows: list[tuple[str, str, str]] = []
        blob_rows: list[tuple[str, str, str, int, int, int, int, int, str, int]] = []
        target: str = "bench-0125"
        expected_bytes: int = 0
        for owner_index in range(owners):
            owner: str = f"bench-{owner_index:04}"
            account_rows.append((owner, "benchmark", owner))
            owner_hash: str = hashlib.sha256(owner.encode()).hexdigest()
            for slot in range(rows_per_owner):
                digest: str = hashlib.sha256(f"{owner}:{slot}".encode()).hexdigest()
                key: str = f"workspace-blobs/{owner_hash}/1/{digest}/00000000-0000-4000-8000-{slot:012x}.tar.zst"
                size: int = 1024 + (owner_index * 19 + slot * 23) % 8192
                blob_rows.append((key, owner, digest, 1, slot, size, size * 2, 4, "ready", 0))
                if owner == target:
                    expected_bytes += size
        self.db.executemany("INSERT INTO accounts(id,issuer,subject,created_at,updated_at) VALUES(?,?,?,0,0)", account_rows)
        self.db.executemany("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES(?,?,?,?,?,?,?,?,?,?)", blob_rows)
        self.db.execute("CREATE VIEW capacity_grouped_baseline AS SELECT account_id,object_key,MAX(bytes) AS bytes FROM space_storage_capacity GROUP BY account_id,object_key")
        fixed_capacity: str = "(SELECT COALESCE(SUM(bytes),0) FROM (SELECT MAX(bytes) AS bytes FROM space_storage_capacity WHERE account_id=?1 GROUP BY object_key))"
        baseline_capacity: str = "(SELECT COALESCE(SUM(bytes),0) FROM capacity_grouped_baseline WHERE account_id=?1)"
        self.assertIn(fixed_capacity, USAGE)
        baseline: str = USAGE.replace(fixed_capacity, baseline_capacity)
        queries: dict[str, str] = {"baseline": baseline, "fixed": USAGE}
        plans: dict[str, list[str]] = {
            name: [row[3] for row in self.db.execute("EXPLAIN QUERY PLAN " + sql, (target,))]
            for name, sql in queries.items()
        }
        self.assertTrue(any("SCAN workspace_blobs" in step for step in plans["baseline"]))
        self.assertTrue(any("SEARCH workspace_blobs" in step and "workspace_blob_capacity" in step and "account_id=?" in step for step in plans["fixed"]))
        self.assertFalse(any("SCAN workspace_blobs" in step for step in plans["fixed"]))
        expected: tuple[int, int, int, int] = (expected_bytes, 0, 0, 0)
        # Both representations run on identical in-memory pages, warmed before
        # alternating samples. Timings diagnose this query stage, not a D1 SLO.
        for sql in queries.values():
            self.assertEqual(tuple(self.db.execute(sql, (target,)).fetchone()), expected)
        timings: dict[str, list[int]] = {name: [] for name in queries}
        for sample in range(5):
            order: tuple[str, str] = ("baseline", "fixed") if sample % 2 == 0 else ("fixed", "baseline")
            for name in order:
                started: int = time.perf_counter_ns()
                row: tuple[int, ...] = tuple(self.db.execute(queries[name], (target,)).fetchone())
                timings[name].append(time.perf_counter_ns() - started)
                self.assertEqual(row, expected)
        actual_rows: int = self.db.execute("SELECT COUNT(*) FROM space_storage_capacity").fetchone()[0]
        owner_rows: int = self.db.execute("SELECT COUNT(*) FROM space_storage_capacity WHERE account_id=?", (target,)).fetchone()[0]
        self.assertEqual(actual_rows, owners * rows_per_owner)
        self.assertEqual(owner_rows, rows_per_owner)
        receipt: dict[str, object] = {
            "scope": "local-in-memory-sqlite-owner-capacity-query-only",
            "source_sha": os.getenv("GITHUB_SHA"),
            "python_version": platform.python_version(),
            "sqlite_version": sqlite3.sqlite_version,
            "owners": owners, "rows_per_owner": rows_per_owner,
            "available_rows": actual_rows, "selected_owner_rows": owner_rows,
            "selected_owner_bytes": expected_bytes,
            "cache": "warm-identical-connection-and-pages",
            "samples": 5, "elapsed_ns": timings, "plans": plans,
            "equivalent_usage": list(expected),
            "latency_threshold_asserted": False,
        }
        directory: pathlib.Path = ROOT / ".cache/perf/v3"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "space-capacity-query.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")

    def test_image_file_links_count_metadata_not_duplicate_physical_bytes(self) -> None:
        self.image()
        self.assertTrue(self.link("first"))
        self.assertTrue(self.link("second"))
        self.assertFalse(self.link("count-over", files=2))
        self.assertEqual(self.usage(), (60, 2, 0, 0))
        row: sqlite3.Row = self.db.execute("SELECT status,upload_outcome,upload_operations,blob_key FROM space_files WHERE id='first'").fetchone()
        self.assertEqual(tuple(row), ("processed", "stored", 0, "image"))
        # The publication key, not the randomly allocated metadata ID, is stable.
        self.assertFalse(self.link("retry-new-id", inline_key="first"))
        self.assertEqual(self.usage(), (60, 2, 0, 0))

    def test_image_link_owner_metadata_and_readiness_are_guarded(self) -> None:
        self.image()
        self.assertFalse(self.link("wrong-owner", owner="b"))
        self.assertFalse(self.link("wrong-bytes", size=59))
        self.db.execute("UPDATE workspace_blobs SET status='deleting' WHERE object_key='image'")
        self.assertFalse(self.link("deleting"))
        self.assertEqual(self.usage()[1], 0)

    def test_link_insert_fences_gc_before_reference_and_immutable_key_cannot_move(self) -> None:
        self.image()
        self.assertTrue(self.link("linked"))
        self.assertEqual(self.db.execute(CLAIM_BLOB, ("1000", "image", "100")).rowcount, 0)
        self.assertEqual(self.db.execute(BIND_FILE, ("a", "linked", "image", "a", "linked", "image")).rowcount, 1)
        self.assertEqual(self.db.execute(RELEASE_FILE, ("a", "linked", "a", "linked")).rowcount, 0)
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE space_files SET blob_key=NULL WHERE id='linked'")

    def test_file_delete_drops_only_link_and_response_image_stays_replayable(self) -> None:
        self.image()
        self.assertTrue(self.link("linked"))
        self.db.execute(BIND_FILE, ("a", "linked", "image", "a", "linked", "image"))
        self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id) VALUES('a','response-artifact','artifact','image','response')")
        self.db.execute("UPDATE space_files SET status='deleting' WHERE id='linked'")
        self.db.execute(FINISH_DELETE, ("100", "linked", "a"))
        self.assertEqual(self.db.execute(RELEASE_FILE, ("a", "linked", "a", "linked")).rowcount, 1)
        self.assertEqual(self.db.execute(CLAIM_BLOB, ("1000", "image", "100")).rowcount, 0)
        self.assertEqual(self.usage(), (60, 0, 0, 0))
        self.assertEqual(self.db.execute("SELECT kind FROM workspace_blob_refs").fetchone()[0], "response-artifact")
        deletion: str = SOURCE[SOURCE.index("async fn finish_delete("):SOURCE.index("pub async fn file_object(")]
        self.assertLess(deletion.index("if file.blob_key.is_none()"), deletion.index(".delete(&file.object_key)"))
        self.assertLess(deletion.index("UPDATE space_files SET status=CASE"), deletion.index("release_image_file"))

    def test_image_link_path_neither_uploads_nor_creates_resource_reservations(self) -> None:
        publication: str = SOURCE[SOURCE.index("pub(crate) async fn link_image_blob("):SOURCE.index("async fn inline_file(")]
        for operation in (".put(", ".get(", "ResourceMeter", "register_account_storage_object", "maintenance_account_reservation"):
            self.assertNotIn(operation, publication)
        self.assertIn("bucket.get(file.content_key())", SOURCE)


if __name__ == "__main__":
    unittest.main()
