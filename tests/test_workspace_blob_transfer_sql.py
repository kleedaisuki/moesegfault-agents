"""Transfer leases bridge archive export and financially admitted child import."""
from __future__ import annotations

import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/workspace_blobs.rs").read_text(encoding="utf-8")


def query(prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if sql.startswith(prefix))


BIND: str = query("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,lease_until_ms)")
RELEASE: str = query("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='fork-transfer'")
EXPIRE: str = query("DELETE FROM workspace_blob_refs WHERE rowid IN")
CLAIM: str = query("UPDATE workspace_blobs SET status='deleting'")
CHILD: str = query("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,session_id,object_key)")


class WorkspaceBlobTransferTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
        self.db.execute("INSERT INTO agents VALUES('agent','a','agent','{}',0,0,NULL)")
        self.db.execute("INSERT INTO sessions VALUES('child','a','agent','do-child','closed','{}','pin',0,0,NULL,NULL,NULL)")
        for key, digest in (("archive", "a"), ("other", "b")):
            self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES(?,'a',?,1,0,100,200,3,'ready',0)", (key, digest * 64))

    def tearDown(self) -> None:
        self.db.close()

    def bind(self, response: str, lease: int = 1000, *, owner: str = "a", key: str = "archive") -> None:
        self.db.execute(BIND, (owner, response, key, str(lease)))

    def claim(self, key: str = "archive") -> int:
        return self.db.execute(CLAIM, ("60000", key, "0")).rowcount

    def test_parent_deleted_and_current_moved_is_protected_until_child_handoff(self) -> None:
        self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('a','response','parent','archive')")
        self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('a','session-current','parent-current','archive')")
        self.bind("child-response")
        self.db.execute("DELETE FROM workspace_blob_refs WHERE kind IN ('response','session-current')")
        self.assertEqual(self.claim(), 0)
        self.db.execute(CHILD, ("a", "child:archive", "child", "archive"))
        self.db.execute(RELEASE, ("a", "child-response"))
        self.assertEqual(self.claim(), 0)
        self.assertEqual(self.db.execute("SELECT kind FROM workspace_blob_refs").fetchone()[0], "session-current")

    def test_same_child_renews_lease_without_retargeting(self) -> None:
        self.bind("child", 1000)
        self.bind("child", 2000)
        self.bind("child", 500)
        self.assertEqual(self.db.execute("SELECT lease_until_ms FROM workspace_blob_refs").fetchone()[0], 2000)
        with self.assertRaises(sqlite3.IntegrityError):
            self.bind("child", 3000, key="other")
        self.assertEqual(self.db.execute("SELECT object_key FROM workspace_blob_refs").fetchone()[0], "archive")

    def test_owner_and_deleting_object_fences(self) -> None:
        with self.assertRaises(sqlite3.IntegrityError):
            self.bind("foreign", owner="b")
        self.assertEqual(self.claim(), 1)
        with self.assertRaises(sqlite3.IntegrityError):
            self.bind("too-late")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_blob_refs").fetchone()[0], 0)

    def test_expired_cleanup_is_bounded_and_user_refs_never_expire(self) -> None:
        for index in range(130):
            self.bind(f"expired-{index}", 100)
        self.bind("future", 1000)
        for kind in ("response", "version", "session-current"):
            self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('a',?,?,'archive')", (kind, kind))
        removed: list[int] = [self.db.execute(EXPIRE, ("100",)).rowcount for _ in range(3)]
        self.assertEqual(removed, [64, 64, 2])
        remaining: list[str] = sorted(row[0] for row in self.db.execute("SELECT kind FROM workspace_blob_refs"))
        self.assertEqual(remaining, ["fork-transfer", "response", "session-current", "version"])
        plan: str = str(list(self.db.execute("EXPLAIN QUERY PLAN " + EXPIRE, ("100",))))
        self.assertIn("workspace_blob_transfer_due", plan)
        self.assertNotIn("TEMP B-TREE", plan)

    def test_only_transfer_refs_have_a_lease(self) -> None:
        for kind, lease in (("response", 100), ("version", 100), ("session-current", 100), ("fork-transfer", None), ("fork-transfer", -1)):
            with self.subTest(kind=kind, lease=lease), self.assertRaises(sqlite3.IntegrityError):
                self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,lease_until_ms) VALUES('a',?,'invalid','archive',?)", (kind, lease))


if __name__ == "__main__":
    unittest.main()
