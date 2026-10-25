"""The same owner blob engine accounts for workspace archives and image artifacts."""
from __future__ import annotations
import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/workspace_blobs.rs").read_text(encoding="utf-8")


def query(prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\(\s*"([^"\n]+)"', SOURCE) if sql.startswith(prefix))


BEGIN: str = query("INSERT INTO workspace_blobs(")
COMPLETE: str = query("UPDATE workspace_blobs SET status='ready'")
RESERVE: str = query("INSERT INTO owner_blob_capacity_holds(")
CONSUME: str = query("DELETE FROM owner_blob_capacity_holds WHERE account_id=? AND operation_id=? AND EXISTS")
BIND_FILE: str = query("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) SELECT ?,'space-file'")
RELEASE_FILE: str = query("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='space-file' AND ref_id=? AND NOT EXISTS")
FILE_SCAN: str = query("SELECT account_id,ref_id,object_key FROM workspace_blob_refs WHERE kind='space-file'")
FILE_RELEASE: str = query("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='space-file' AND ref_id=? AND NOT EXISTS(SELECT 1 FROM space_files WHERE account_id=? AND id=? AND blob_key=?")
BIND: str = query("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id)")
RELEASE: str = query("DELETE FROM workspace_blob_refs WHERE account_id=? AND ((kind='response'")
FIND_IMAGE: str = query("SELECT * FROM workspace_blobs WHERE account_id=? AND kind='image'")
FIND_WORKSPACE: str = query("SELECT * FROM workspace_blobs WHERE account_id=? AND format_version=CAST(? AS INTEGER) AND sha256=? AND archive_bytes=CAST(? AS INTEGER) AND kind='workspace'")
CLAIM: str = query("UPDATE workspace_blobs SET status='deleting'")
FINISH: str = query("DELETE FROM workspace_blobs WHERE object_key=? AND status='deleting'")
EXPIRE_HOLDS: str = query("DELETE FROM owner_blob_capacity_holds WHERE rowid IN")


class OwnedBlobSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))

    def tearDown(self) -> None:
        self.db.close()

    def reserve(self, operation: str, size: int, quota: int, owner: str = "a") -> int:
        return self.db.execute(RESERVE, (owner, operation, str(size), "1000", owner, operation, str(size), owner, str(size), str(quota))).rowcount

    def begin(self, key: str, size: int, quota: int, *, image: bool = True, operation: str = "", owner: str = "a") -> int:
        kind: str = "image" if image else "workspace"
        mime: str = "image/png" if image else "application/zstd"
        params: tuple[object, ...] = (key, owner, "ab" * 32, "0" if image else "1", "0", str(size), str(size), "1", "1000", kind, mime,
            owner, key, str(size), owner, str(size), str(quota), operation, owner, operation, str(size), "0", owner, key)
        return self.db.execute(BEGIN, params).rowcount

    def complete(self, key: str, size: int, *, image: bool = True) -> int:
        return self.db.execute(COMPLETE, ("1000", key, "a", "0", "a", "0" if image else "1", "ab" * 32, str(size), "image" if image else "workspace", "image/png" if image else "application/zstd")).rowcount

    def capacity(self) -> int:
        return self.db.execute("SELECT COALESCE(SUM(bytes),0) FROM (SELECT MAX(bytes) AS bytes FROM space_storage_capacity WHERE account_id='a' GROUP BY object_key)").fetchone()[0]

    def test_candidate_capacity_is_atomic_across_kinds_and_all_states(self) -> None:
        self.assertEqual(self.begin("workspace", 60, 100, image=False), 1)
        self.assertEqual(self.begin("image", 40, 100), 1)
        self.assertEqual(self.begin("over", 1, 100), 0)
        self.assertEqual(self.capacity(), 100)
        self.db.execute("UPDATE workspace_blobs SET status='deleting' WHERE object_key='workspace'")
        self.assertEqual(self.begin("still-over", 1, 100), 0)
        self.assertEqual(self.begin("workspace", 60, 1, image=False), 0)

    def test_reserved_provider_capacity_is_consumed_only_by_matching_candidate(self) -> None:
        self.assertEqual(self.reserve("paid", 80, 100), 1)
        self.assertEqual(self.reserve("second", 21, 100), 0)
        self.assertEqual(self.reserve("paid", 80, 1), 1)
        self.assertEqual(self.begin("image", 60, 100, operation="paid"), 1)
        self.db.execute(CONSUME, ("a", "paid", "a", "image", "ab" * 32, "60", "image/png"))
        self.assertEqual(self.capacity(), 60)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM owner_blob_capacity_holds").fetchone()[0], 0)
        self.assertEqual(self.begin("unreserved", 1, 100, operation="missing"), 0)
        self.assertEqual(self.reserve("too-small", 10, 100), 1)
        self.assertEqual(self.begin("oversized", 11, 100, operation="too-small"), 0)
        self.assertEqual(self.db.execute("SELECT bytes FROM owner_blob_capacity_holds").fetchone()[0], 10)

    def test_kind_filtered_dedup_and_multi_item_response_release(self) -> None:
        for key, image in (("workspace", False), ("image", True), ("duplicate", True)):
            self.assertEqual(self.begin(key, 10, 100, image=image), 1)
        self.assertEqual(self.complete("workspace", 10, image=False), 1)
        self.assertEqual(self.complete("image", 10), 1)
        self.assertEqual(self.complete("duplicate", 10), 0)
        self.assertEqual(self.db.execute(FIND_IMAGE, ("a", "ab" * 32, "10", "image/png")).fetchone()[0], "image")
        self.assertEqual(self.db.execute(FIND_WORKSPACE, ("a", "1", "ab" * 32, "10")).fetchone()[0], "workspace")
        for item, response in (("one", "parent"), ("two", "parent"), ("child", "descendant")):
            self.db.execute(BIND, ("a", item, "image", response))
        self.db.execute(RELEASE, ("a", "parent", "parent"))
        self.assertEqual(self.db.execute("SELECT ref_id FROM workspace_blob_refs").fetchone()[0], "child")
        self.assertEqual(self.db.execute(CLAIM, ("60000", "image", "1000")).rowcount, 0)

    def test_linked_file_metadata_fences_gc_before_ref_binding(self) -> None:
        self.begin("image", 10, 100)
        self.complete("image", 10)
        self.db.execute("INSERT INTO spaces VALUES('space','a','My Space',0,0)")
        self.db.execute("INSERT INTO space_files(id,account_id,space_id,filename,purpose,content_type,object_key,bytes,sha256,status,reservation_id,created_at_ms,updated_at_ms,recovery_at_ms,blob_key) VALUES('file','a','space','image.png','user_data','image/png','metadata-file',10,?,'processed','reservation',0,0,0,'image')", ("ab" * 32,))
        self.db.execute(BIND_FILE, ("a", "file", "image", "a", "file", "image"))
        self.assertEqual(self.capacity(), 10)
        self.assertEqual(self.db.execute(CLAIM, ("60000", "image", "1000")).rowcount, 0)
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE space_files SET blob_key=NULL WHERE id='file'")
        self.db.execute("UPDATE space_files SET status='deleted' WHERE id='file'")
        self.assertEqual(self.db.execute(CLAIM, ("60000", "image", "1000")).rowcount, 0)
        self.db.execute(RELEASE_FILE, ("a", "file", "a", "file"))
        self.assertEqual(self.db.execute(CLAIM, ("60000", "image", "1000")).rowcount, 1)
        self.assertEqual(self.db.execute(FINISH, ("image",)).rowcount, 1)
        self.assertEqual(self.db.execute("SELECT status,blob_key FROM space_files WHERE id='file'").fetchone(), ("deleted", "image"))

    def test_stale_file_reference_cleanup_is_indexed_and_bounded(self) -> None:
        self.begin("image", 10, 100)
        self.complete("image", 10)
        for index in range(130):
            self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('a','space-file',?,'image')", (f"ghost-{index}",))
        removed: list[int] = []
        for _ in range(3):
            rows: list[tuple[str, str, str]] = self.db.execute(FILE_SCAN, ("0",)).fetchall()
            self.assertLessEqual(len(rows), 64)
            removed.append(sum(self.db.execute(FILE_RELEASE, (owner, ref_id, owner, ref_id, key)).rowcount for owner, ref_id, key in rows))
        self.assertEqual(removed, [64, 64, 2])
        plan: str = str(list(self.db.execute("EXPLAIN QUERY PLAN " + FILE_SCAN, ("0",))))
        self.assertIn("workspace_blob_file_maintenance", plan)
        self.assertNotIn("TEMP B-TREE", plan)

    def test_capacity_hold_cleanup_is_bounded(self) -> None:
        for index in range(130):
            self.assertEqual(self.reserve(f"op-{index}", 1, 1000), 1)
        removed: list[int] = [self.db.execute(EXPIRE_HOLDS, ("1000",)).rowcount for _ in range(3)]
        self.assertEqual(removed, [64, 64, 2])


if __name__ == "__main__":
    unittest.main()
