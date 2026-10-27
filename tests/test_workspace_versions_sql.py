"""Immutable versions exercise production SQL, not a second metadata model."""

from __future__ import annotations

import pathlib
import hashlib
import random
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/workspace_versions.rs").read_text(encoding="utf-8")


def query(prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if sql.startswith(prefix))


INSERT: str = query("INSERT INTO workspace_versions")
LOOKUP = query("SELECT * FROM workspace_versions WHERE account_id=? AND id=?")
OPERATION = query("SELECT * FROM workspace_versions WHERE account_id=? AND session_id=?")
LIST = query("SELECT * FROM workspace_versions WHERE account_id=? AND (created_at_ms,id)")
REFERENCE = query("SELECT id FROM workspace_versions WHERE object_key=?")
ADMIT = query("INSERT INTO workspace_version_uploads")
ACTIVATE = query("UPDATE workspace_version_uploads SET created_at_ms")
CLAIM = query("UPDATE workspace_version_uploads SET status='abandoned'")


class WorkspaceVersionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        for path in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(path.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
            self.db.execute("INSERT INTO agents VALUES(?,?,?,'{}',0,0,NULL)", (f"agent-{owner}", owner, "test"))
            self.db.execute("INSERT INTO sessions VALUES(?,?,?,?,'closed','{}','pin',0,0,NULL,NULL,NULL)", (f"session-{owner}", owner, f"agent-{owner}",f"do-{owner}"))

    def tearDown(self) -> None:
        self.db.close()

    def test_adopting_protected_legacy_version_keeps_original_physical_clock(self) -> None:
        registry: str = (ROOT / "crates/agents-worker/src/workspace_blobs.rs").read_text(encoding="utf-8")
        queries: list[str] = re.findall(r'\.prepare\("([^"\n]+)"\)', registry)
        begin: str = next(sql for sql in queries if sql.startswith("INSERT INTO workspace_blobs("))
        complete: str = next(sql for sql in queries if sql.startswith("UPDATE workspace_blobs SET status='ready'"))
        key: str = f"space-versions/{hashlib.sha256(b'a').hexdigest()}/{'ab' * 32}/9f231001-d773-4286-afba-80f1dd9b483b.tar.zst"
        self.insert("legacy", 1, key=key)
        self.db.execute("INSERT INTO resource_storage_objects VALUES(?,'a','session-a','123',0,0,NULL,'active')", (key,))
        self.db.execute(begin, (key, "a", "ab" * 32, "1", "1", "123", "456", "2", "100", "workspace", "application/zstd", "a", key, "123", "a", "123", "1000", "", "a", "", "123", "50", "a", key))
        self.db.execute(complete, ("200", key, "a", "50", "a", "1", "ab" * 32, "123", "workspace", "application/zstd"))
        self.assertEqual(tuple(self.db.execute("SELECT status FROM workspace_blobs WHERE object_key=?", (key,)).fetchone()), ("ready",))
        row: sqlite3.Row = self.db.execute("SELECT session_id,size_bytes FROM resource_storage_objects WHERE object_key=?", (key,)).fetchone()
        self.assertEqual(tuple(row), ("session-a", "123"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM resource_storage_objects").fetchone()[0], 1)
        archives: str = (ROOT / "crates/agents-worker/src/workspace_archives.rs").read_text(encoding="utf-8")
        adoption: str = archives[archives.index("if let Some(version)"):archives.index("let mut candidate")]
        self.assertNotIn(".put(", adoption)
        self.assertNotIn("register_account_storage_object", adoption)

    def insert(self, identity: str, timestamp: int, *, owner: str = "a", operation: str | None = None, key: str | None = None) -> None:
        operation=operation or identity
        self.db.execute(INSERT, (identity, owner, f"session-{owner}", operation, "Named workspace", key or f"archive-{identity}", "ab" * 32, "1", "1", "123", "456", "2", str(timestamp),owner,owner,f"session-{owner}",operation,"50000",owner,f"session-{owner}",operation))

    def test_owner_scoping_and_idempotent_operation(self) -> None:
        self.insert("first", 100, operation="stable-call")
        self.insert("retry", 200, operation="stable-call")
        row = self.db.execute(OPERATION, ("a", "session-a", "stable-call")).fetchone()
        self.assertEqual(row["id"], "first")
        self.assertEqual(row["created_at_ms"], 100)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_versions").fetchone()[0], 1)
        self.assertIsNone(self.db.execute(LOOKUP, ("b", "first")).fetchone())
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(INSERT, ("foreign", "b", "session-a", "bad", "", "key", "ab" * 32, "1", "1", "1", "1", "1", "0","b","b","session-a","bad","50000","b","session-a","bad"))

    def test_versions_are_immutable_and_references_outlive_current_pointer(self) -> None:
        self.insert("version", 1, key="durable-archive")
        for field, value in (("name", "changed"), ("object_key", "replacement"), ("account_id", "b")):
            with self.subTest(field=field), self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(f"UPDATE workspace_versions SET {field}=? WHERE id='version'", (value,))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("DELETE FROM workspace_versions WHERE id='version'")
        self.assertEqual(self.db.execute(REFERENCE, ("durable-archive",)).fetchone()[0], "version")
        self.assertIsNone(self.db.execute(REFERENCE, ("unreferenced",)).fetchone())

    def test_indexed_pagination_is_complete_under_timestamp_ties(self) -> None:
        rng = random.Random(3317)
        expected: list[tuple[int, str]] = []
        for index in range(257):
            timestamp = rng.randrange(10)
            identity = f"version-{index:04}"
            self.insert(identity, timestamp)
            expected.append((timestamp, identity))
        self.insert("foreign", 1000, owner="b")
        cursor: tuple[int, str] = (2**63 - 1, "~")
        observed: list[tuple[int, str]] = []
        while True:
            rows = self.db.execute(LIST, ("a", str(cursor[0]), cursor[1], "17")).fetchall()
            observed.extend((row["created_at_ms"], row["id"]) for row in rows)
            if len(rows) < 17:
                break
            cursor = (rows[-1]["created_at_ms"], rows[-1]["id"])
        self.assertEqual(observed, sorted(expected, reverse=True))
        plan = str([tuple(row) for row in self.db.execute("EXPLAIN QUERY PLAN " + LIST, ("a", str(2**63-1), "~", "17"))])
        self.assertIn("workspace_versions_owner", plan)

    def test_isolated_version_namespace_survives_legacy_checkpoint_gc(self) -> None:
        owner_hash = "ownerhash"
        session_hash = "sessionhash"
        bucket = {
            f"workspaces/{owner_hash}/{session_hash}/snapshots/old/archive.tar.zst",
            f"workspaces/{owner_hash}/{session_hash}/snapshots/current/archive.tar.zst",
            f"space-versions/{owner_hash}/sha/version.tar.zst",
        }
        # v0.2's intent discovery and eager cleanup reach session checkpoints
        # only, irrespective of whether that binary knows version references.
        legacy_intents = {key for key in bucket if key.startswith(f"workspaces/{owner_hash}/{session_hash}/snapshots/")}
        current = f"workspaces/{owner_hash}/{session_hash}/snapshots/current/archive.tar.zst"
        bucket.difference_update(legacy_intents - {current})
        self.assertIn(f"space-versions/{owner_hash}/sha/version.tar.zst", bucket)
        workspace = (ROOT / "crates/agents-worker/src/workspace.rs").read_text(encoding="utf-8")
        self.assertIn('"space-versions/{}/"', workspace)
        archives: str = (ROOT / "crates/agents-worker/src/workspace_archives.rs").read_text(encoding="utf-8")
        self.assertIn('"workspace-blobs/{}/{format}/{sha}/{}.tar.zst"', archives)
        publish = workspace[workspace.index("async fn finish_version_upload("):workspace.index("pub async fn restore_version(")]
        self.assertLess(publish.index(".sha256("), publish.index("reconcile::register_storage_object"))
        self.assertLess(publish.index("reconcile::register_storage_object"), publish.index("workspace_versions::publish"))

    def test_pending_upload_quota_and_collection_lease(self) -> None:
        def admit(key: str, operation: str, size: int, quota: int) -> None:
            self.db.execute(ADMIT, (key,"a","session-a",operation,str(size),'{}',"0","a",str(size),str(quota),"a","a","100"))
        admit("candidate-a","op-a",60,100)
        admit("candidate-b","op-b",50,100)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_version_uploads").fetchone()[0],1)
        self.db.execute(ACTIVATE,("200","candidate-a"))
        self.db.execute(CLAIM,("candidate-a","100","candidate-a"))
        self.assertEqual(self.db.execute("SELECT status FROM workspace_version_uploads").fetchone()[0],"pending")
        self.db.execute(CLAIM,("candidate-a","300","candidate-a"))
        self.assertEqual(self.db.execute("SELECT status FROM workspace_version_uploads").fetchone()[0],"abandoned")
        self.db.execute(ACTIVATE,("400","candidate-a"))
        self.assertEqual(self.db.execute("SELECT created_at_ms FROM workspace_version_uploads").fetchone()[0],200)
        admit("candidate-c","op-c",50,100)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_version_uploads").fetchone()[0],1)

    def test_shared_archive_consumes_physical_bytes_once(self) -> None:
        self.insert("one",1,key="shared-archive")
        self.insert("two",2,key="shared-archive")
        self.db.execute(ADMIT,("new-candidate","a","session-a","new-op","77",'{}',"3","a","77","200","a","a","100"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_version_uploads").fetchone()[0],1)
        self.db.execute(ADMIT,("over-quota","a","session-a","over-op","1",'{}',"4","a","1","200","a","a","100"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_version_uploads").fetchone()[0],1)

    def ready_blob(self, key: str, size: int, *, owner: str = "a", status: str = "ready") -> None:
        digest: str = format(size, "064x")
        self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES(?,?,?,1,1,?,?,2,?,0)",
                        (key, owner, digest, size, size * 2, status))

    def shared(self, identity: str, key: str, size: int, *, owner: str = "a",
               session: str | None = None, operation: str | None = None,
               quota: int = 1000, limit: int = 100) -> int:
        statement: str = query("INSERT INTO workspace_versions(id,account_id,session_id,operation_id,name,object_key,sha256,format_version,generation,archive_bytes,logical_bytes,entries,created_at_ms) SELECT ?1")
        return self.db.execute(statement, (identity, owner, session or f"session-{owner}",
            operation or identity, "Named workspace", key, format(size, "064x"),
            "1", "1", str(size), str(size * 2), "2", "1", str(limit), str(quota))).rowcount

    def test_shared_publish_atomic_byte_quota_and_distinct_objects(self) -> None:
        self.ready_blob("canonical-one", 60)
        self.ready_blob("canonical-two", 40)
        self.assertEqual(self.shared("first", "canonical-one", 60, quota=100), 1)
        self.assertEqual(self.shared("alias", "canonical-one", 60, quota=100), 1)
        self.assertEqual(self.shared("second", "canonical-two", 40, quota=100), 1)
        self.ready_blob("canonical-three", 1)
        self.assertEqual(self.shared("over", "canonical-three", 1, quota=100), 0)
        retained: int = self.db.execute("SELECT SUM(size) FROM (SELECT MAX(archive_bytes) AS size FROM workspace_versions GROUP BY object_key)").fetchone()[0]
        self.assertEqual(retained, 100)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_versions").fetchone()[0], 3)

    def test_shared_publish_preserves_account_file_bytes_and_ignores_failed_files(self) -> None:
        self.ready_blob("canonical", 60)
        self.db.execute("INSERT INTO spaces VALUES('space-a','a','My Space',0,0)")
        for status, size in (("processed", 20), ("uploading", 10), ("deleting", 11), ("failed", 1000)):
            self.db.execute("INSERT INTO space_files(id,account_id,space_id,filename,purpose,content_type,object_key,bytes,sha256,status,reservation_id,created_at_ms,updated_at_ms,recovery_at_ms) VALUES(?,'a','space-a',?,'user_data','application/octet-stream',?,?,?,?,?,0,0,0)",
                            (status, status, f"file-{status}", size, "ab" * 32, status, f"reservation-{status}"))
        self.assertEqual(self.shared("over", "canonical", 60, quota=100), 0)
        self.assertEqual(self.shared("exact", "canonical", 60, quota=101), 1)

    def test_shared_publish_owner_session_and_ready_fence(self) -> None:
        self.ready_blob("owned", 10)
        self.ready_blob("foreign", 20, owner="b")
        self.ready_blob("unfinished", 30, status="uploading")
        self.ready_blob("collected", 40, status="deleting")
        self.assertEqual(self.shared("foreign", "foreign", 20), 0)
        self.assertEqual(self.shared("unfinished", "unfinished", 30), 0)
        self.assertEqual(self.shared("collected", "collected", 40), 0)
        with self.assertRaises(sqlite3.IntegrityError):
            self.shared("wrong-session", "owned", 10, session="session-b")
        self.assertEqual(self.shared("owned", "owned", 10), 1)

    def test_shared_publish_preserves_pending_and_abandoned_legacy_quota(self) -> None:
        self.ready_blob("canonical", 60)
        self.db.execute(ADMIT, ("legacy", "a", "session-a", "legacy-op", "41", '{}', "0",
                              "a", "41", "1000", "a", "a", "100"))
        self.assertEqual(self.shared("too-large", "canonical", 60, quota=100), 0)
        self.db.execute("UPDATE workspace_version_uploads SET status='abandoned' WHERE object_key='legacy'")
        self.assertEqual(self.shared("still-large", "canonical", 60, quota=100), 0)
        self.assertEqual(self.shared("fits", "canonical", 60, quota=101), 1)

    def test_shared_publish_version_count_reserves_pending_legacy_slot(self) -> None:
        self.ready_blob("canonical", 60)
        self.db.execute(ADMIT, ("legacy", "a", "session-a", "legacy-op", "1", '{}', "0",
                              "a", "1", "1000", "a", "a", "100"))
        self.assertEqual(self.shared("no-slot", "canonical", 60, limit=1), 0)
        self.assertEqual(self.shared("one-slot", "canonical", 60, limit=2), 1)
        self.assertEqual(self.shared("full", "canonical", 60, limit=2), 0)

    def test_shared_publish_retry_identity_and_crash_before_reference_gc_guard(self) -> None:
        self.ready_blob("canonical", 60)
        self.assertEqual(self.shared("first", "canonical", 60, operation="stable"), 1)
        self.assertEqual(self.shared("retry", "canonical", 60, operation="stable"), 0)
        self.assertEqual(self.db.execute(OPERATION, ("a", "session-a", "stable")).fetchone()["id"], "first")
        registry: str = (ROOT / "crates/agents-worker/src/workspace_blobs.rs").read_text(encoding="utf-8")
        claim: str = next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', registry)
                          if sql.startswith("UPDATE workspace_blobs SET status='deleting'"))
        # A process may die after immutable version INSERT but before strong-ref
        # registration. The version row itself must prevent physical collection.
        self.assertEqual(self.db.execute(claim, ("60000", "canonical", "0")).rowcount, 0)
        self.assertEqual(self.db.execute(REFERENCE, ("canonical",)).fetchone()[0], "first")

    def test_snapshot_cleanup_checks_references_and_restore_fence_precedes_execution(self) -> None:
        workspace = (ROOT / "crates/agents-worker/src/workspace.rs").read_text(encoding="utf-8")
        gc = (ROOT / "crates/agents-worker/src/checkpoint_gc.rs").read_text(encoding="utf-8")
        publish = workspace[workspace.index("async fn publish_manifest("):]
        self.assertLess(publish.index("workspace_versions::referenced"), publish.index("bucket.delete(&previous.object_key)"))
        collect = gc[gc.index("pub async fn collect("):]
        self.assertLess(collect.index("workspace_versions::referenced"), collect.index("bucket.delete(key)"))
        execute = workspace[workspace.index("pub async fn execute_bash_with_id("):]
        self.assertLess(execute.index("self.restore_if_needed().await?"), execute.index('"/v1/exec"'))
        restore = workspace[workspace.index("pub async fn restore_version("):workspace.index("async fn resume_version_restore(")]
        self.assertLess(re.search(r"\.commit_version\s*\(",restore).start(), re.search(r"\.put\s*\(\s*VERSION_RESTORE",restore).start())
        recover = workspace[workspace.index("async fn resume_version_restore("):workspace.index("pub async fn checkpoint(")]
        self.assertLess(recover.index('"/v1/restore"'), recover.index("self.publish_manifest("))
        self.assertLess(recover.index("self.publish_manifest("), recover.rindex(".delete(VERSION_RESTORE)"))


if __name__ == "__main__":
    unittest.main()
