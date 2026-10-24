"""Immutable versions exercise production SQL, not a second metadata model."""

from __future__ import annotations

import pathlib
import random
import re
import sqlite3
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/workspace_versions.rs").read_text(encoding="utf-8")


def query(prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if sql.startswith(prefix))


INSERT = query("INSERT INTO workspace_versions")
LOOKUP = query("SELECT * FROM workspace_versions WHERE account_id=? AND id=?")
OPERATION = query("SELECT * FROM workspace_versions WHERE account_id=? AND session_id=?")
LIST = query("SELECT * FROM workspace_versions WHERE account_id=? AND (created_at_ms,id)")
REFERENCE = query("SELECT id FROM workspace_versions WHERE object_key=?")
ADMIT = query("INSERT INTO workspace_version_uploads")
ACTIVATE = query("UPDATE workspace_version_uploads SET created_at_ms")
CLAIM = query("UPDATE workspace_version_uploads SET status='abandoned'")


class WorkspaceVersionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        for path in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(path.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
            self.db.execute("INSERT INTO agents VALUES(?,?,?,'{}',0,0,NULL)", (f"agent-{owner}", owner, "test"))
            self.db.execute("INSERT INTO sessions VALUES(?,?,?,?,'closed','{}','pin',0,0,NULL,NULL,NULL)", (f"session-{owner}", owner, f"agent-{owner}",f"do-{owner}"))

    def tearDown(self) -> None:
        self.db.close()

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
        self.assertIn('"space-versions/{}/{}/{}.tar.zst"', workspace)
        publish = workspace[workspace.index("async fn finish_version_upload("):workspace.index("pub async fn restore_version(")]
        self.assertLess(publish.index(".sha256("), publish.index("reconcile::register_storage_object"))
        self.assertLess(publish.index("reconcile::register_storage_object"), publish.index("workspace_versions::publish"))

    def test_pending_upload_quota_and_collection_lease(self) -> None:
        def admit(key: str, operation: str, size: int, quota: int) -> None:
            self.db.execute(ADMIT, (key,"a","session-a",operation,str(size),'{}',"0","a","a","a",str(size),str(quota),"a","a","100"))
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
        self.db.execute(ADMIT,("new-candidate","a","session-a","new-op","77",'{}',"3","a","a","a","77","200","a","a","100"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_version_uploads").fetchone()[0],1)
        self.db.execute(ADMIT,("over-quota","a","session-a","over-op","1",'{}',"4","a","a","a","1","200","a","a","100"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_version_uploads").fetchone()[0],1)

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
