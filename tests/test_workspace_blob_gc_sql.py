"""Shared-archive deletion follows the physical clock, not individual references."""
from __future__ import annotations

import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
GC: str = (ROOT / "crates/agents-worker/src/checkpoint_gc.rs").read_text(encoding="utf-8")
REGISTRY: str = (ROOT / "crates/agents-worker/src/workspace_blobs.rs").read_text(encoding="utf-8")
RECONCILE: str = (ROOT / "crates/agents-worker/src/reconcile.rs").read_text(encoding="utf-8")


def query(source: str, prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', source) if sql.startswith(prefix))


CLAIM: str = query(REGISTRY, "UPDATE workspace_blobs SET status='deleting'")
SCAN: str = query(REGISTRY, "SELECT * FROM workspace_blobs WHERE lease_until_ms<=")
DEFER: str = query(REGISTRY, "UPDATE workspace_blobs SET lease_until_ms=")
BIND_SESSION: str = query(REGISTRY, "INSERT INTO workspace_blob_refs(account_id,kind,ref_id,session_id,object_key)")
RECONCILE_SESSION: str = query(REGISTRY, "DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='session-current' AND session_id=? AND object_key!=?")
FINISH: str = query(REGISTRY, "DELETE FROM workspace_blobs WHERE object_key=? AND status='deleting'")
REGISTER: str = query(RECONCILE, "INSERT INTO resource_storage_objects(")
REMOVE: str = query(RECONCILE, "UPDATE resource_storage_objects SET removed_at_ms")


class WorkspaceBlobGcTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("PRAGMA foreign_keys=ON")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('owner','issuer','owner',0,0)")
        self.bucket: set[str] = {"archive"}
        self.db.execute(
            "INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES('archive','owner',?,1,0,1024,2048,3,'ready',0)",
            ("a" * 64,),
        )

    def tearDown(self) -> None:
        self.db.close()

    def register(self) -> None:
        self.db.execute(REGISTER, ("archive", "owner", None, "1024", "0", "0"))

    def claim(self, now: int) -> int:
        return self.db.execute(CLAIM, (str(now + 60_000), "archive", str(now))).rowcount

    def delete(self, now: int, crash: str | None = None) -> None:
        if "archive" in self.bucket:
            self.register()
        if crash == "delete_failed":
            raise OSError("R2 unavailable")
        self.bucket.discard("archive")
        if crash == "after_delete":
            return
        self.db.execute(REMOVE, (str(now), "archive", str(now), str(now)))
        if crash == "after_clock":
            return
        self.db.execute(FINISH, ("archive",))

    def test_one_physical_clock_survives_multiple_references(self) -> None:
        self.register()
        for kind, reference in (("response", "response-a"), ("version", "version-a")):
            self.db.execute(
                "INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('owner',?,?, 'archive')",
                (kind, reference),
            )
        self.register()
        self.assertEqual(self.claim(86_400_000), 0)
        self.assertEqual(self.db.execute("SELECT COUNT(*),SUM(CAST(size_bytes AS INTEGER)) FROM resource_storage_objects").fetchone(), (1, 1024))
        self.assertEqual(self.bucket, {"archive"})

    def test_never_materialized_upload_has_no_invented_storage_clock(self) -> None:
        self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES('absent','owner',?,1,0,1024,2048,3,'uploading',100)", ("b" * 64,))
        self.assertEqual(self.db.execute(CLAIM, ("1000", "absent", "99")).rowcount, 0)
        self.assertEqual(self.db.execute(CLAIM, ("1000", "absent", "100")).rowcount, 1)
        self.db.execute(REMOVE, ("101", "absent", "101", "101"))
        self.db.execute(FINISH, ("absent",))
        self.assertIsNone(self.db.execute("SELECT object_key FROM workspace_blobs WHERE object_key='absent'").fetchone())
        self.assertIsNone(self.db.execute("SELECT object_key FROM resource_storage_objects").fetchone())

    def test_claim_fences_late_references_and_each_external_crash_is_retryable(self) -> None:
        for crash in ("delete_failed", "after_delete", "after_clock", None):
            with self.subTest(crash=crash):
                self.set_fixture()
                self.assertEqual(self.claim(86_400_000), 1)
                with self.assertRaises(sqlite3.IntegrityError):
                    self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('owner','response','late','archive')")
                try:
                    self.delete(86_400_001, crash)
                except OSError:
                    self.assertEqual(self.bucket, {"archive"})
                self.delete(86_400_002)
                self.delete(86_400_003)
                self.assertEqual(self.bucket, set())
                self.assertIsNone(self.db.execute("SELECT object_key FROM workspace_blobs").fetchone())
                self.assertEqual(self.db.execute("SELECT status FROM resource_storage_objects").fetchone(), ("removed",))

    def set_fixture(self) -> None:
        self.db.execute(FINISH, ("archive",))
        self.db.execute("DELETE FROM resource_storage_objects")
        self.db.execute("INSERT OR IGNORE INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES('archive','owner',?,1,0,1024,2048,3,'ready',0)", ("a" * 64,))
        self.bucket = {"archive"}

    def test_external_deletion_order_is_fail_closed(self) -> None:
        delete: str = GC[GC.index("async fn delete_owner_blob("):GC.index("async fn dispatch(")]
        self.assertLess(delete.index("bucket.head("), delete.index("register_account_storage_object"))
        self.assertLess(delete.index("register_account_storage_object"), delete.index("bucket.delete("))
        self.assertLess(delete.index("bucket.delete("), delete.index("remove_storage_object"))
        self.assertLess(delete.index("remove_storage_object"), delete.index("finish_delete"))
        collect: str = GC[GC.index("pub async fn collect("):]
        self.assertLess(collect.index("workspace_blobs::referenced"), collect.index("bucket.delete(key)"))
        self.assertIn("workspace_blobs::find_key(env, owner, &pointer.object_key)", collect)

    def test_indexed_scan_rotates_live_history_in_bounded_batches(self) -> None:
        for index in range(150):
            key: str = f"history-{index:03}"
            self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES(?,'owner',?,1,0,1024,2048,3,'ready',0)", (key, f"{index:064x}"))
            self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('owner','response',?,?)", (key, key))
        seen: set[str] = set()
        for _ in range(3):
            rows: list[tuple[object, ...]] = self.db.execute(SCAN, ("100", "64")).fetchall()
            self.assertLessEqual(len(rows), 64)
            for row in rows:
                key = str(row[0])
                self.assertNotIn(key, seen)
                seen.add(key)
                if self.db.execute(CLAIM, ("60100", key, "100")).rowcount == 0:
                    self.db.execute(DEFER, ("86400100", key, "100"))
        self.assertEqual(len(seen), 151)
        plan: str = str(self.db.execute("EXPLAIN QUERY PLAN " + SCAN, ("100", "64")).fetchall())
        self.assertIn("workspace_blob_due", plan)
        self.assertNotIn("TEMP B-TREE", plan)

    def seed_restore_archives(self) -> None:
        self.db.execute("INSERT INTO agents VALUES('agent','owner','test','{}',0,0,NULL)")
        self.db.execute("INSERT INTO sessions VALUES('s','owner','agent','do','closed','{}','pin',0,0,NULL,NULL,NULL)")
        for key, digest in (("current", "b"), ("target", "c")):
            self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES(?,'owner',?,1,0,1024,2048,3,'ready',0)", (key, digest * 64))

    def bind_session(self, key: str) -> None:
        self.db.execute(BIND_SESSION, ("owner", f"s:{key}", "s", key))

    def reconcile_session(self, key: str) -> None:
        self.db.execute(RECONCILE_SESSION, ("owner", "s", key, "owner", "s", key))

    def session_keys(self) -> set[str]:
        return {row[0] for row in self.db.execute("SELECT object_key FROM workspace_blob_refs WHERE kind='session-current'")}

    def test_restore_crash_keeps_candidates_until_pending_target_commits(self) -> None:
        self.seed_restore_archives()
        self.bind_session("archive")
        self.bind_session("current")
        self.bind_session("target")
        # The R2 pointer moved to current, while DO state still names archive;
        # a pending restore must next publish target, not prune it as an orphan.
        self.assertEqual(self.session_keys(), {"archive", "current", "target"})
        for key in self.session_keys():
            self.assertEqual(self.db.execute(CLAIM, ("60100", key, "100")).rowcount, 0)
        self.bind_session("target")
        self.reconcile_session("target")
        self.assertEqual(self.session_keys(), {"target"})
        self.assertEqual(self.db.execute(CLAIM, ("60100", "target", "100")).rowcount, 0)
        for key in ("archive", "current"):
            self.assertEqual(self.db.execute(CLAIM, ("60100", key, "100")).rowcount, 1)

    def test_read_only_recovery_releases_old_hold_without_new_checkpoint(self) -> None:
        self.seed_restore_archives()
        self.bind_session("archive")
        self.bind_session("current")
        self.bind_session("current")
        self.reconcile_session("current")
        self.assertEqual(self.session_keys(), {"current"})
        self.assertEqual(self.db.execute(CLAIM, ("60100", "archive", "100")).rowcount, 1)
        self.assertEqual(self.db.execute(CLAIM, ("60100", "current", "100")).rowcount, 0)

    def test_recovery_resumes_restore_before_reconciling_both_boot_paths(self) -> None:
        workspace: str = (ROOT / "crates/agents-worker/src/workspace.rs").read_text(encoding="utf-8")
        restore: str = workspace[workspace.index("pub async fn restore_if_needed("):workspace.index("async fn startup_pending(")]
        pairs: list[str] = re.findall(r"self\.resume_version_restore\(\)\.await\?;\s*(?://[^\n]*\n\s*)*self\.reconcile_current_archive\(\)\.await\?;", restore)
        self.assertEqual(len(pairs), 2)
        self.assertEqual(restore.count("self.reconcile_current_archive().await?;"), 2)
        archives: str = (ROOT / "crates/agents-worker/src/workspace_archives.rs").read_text(encoding="utf-8")
        helper: str = archives[archives.index("async fn reconcile_current_archive("):archives.index("async fn seal_response_archive(")]
        self.assertLess(helper.index("blobs::bind_session("), helper.index("blobs::reconcile_session("))


if __name__ == "__main__":
    unittest.main()
