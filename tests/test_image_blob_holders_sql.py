"""Image holders distinguish public lineage, private observers and export leases."""
from __future__ import annotations
import hashlib
import json
import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/workspace_blobs.rs").read_text(encoding="utf-8")
QUERIES: list[str] = re.findall(r'\.prepare\(\s*"([^"\n]+)"', SOURCE)


def query(prefix: str) -> str:
    return next(sql for sql in QUERIES if sql.startswith(prefix))


def digest(value: object) -> str:
    return hashlib.sha256(json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()


def image_id(response: str, item: str) -> str:
    return "image:" + digest([response, item])


def connection_id(connection: str, response: str) -> str:
    return "connection:" + digest([connection, response])


BIND: str = query("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id)")
LEASE_MIN: str = next(sql for sql in QUERIES if sql.startswith("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,lease_until_ms)") and "=MIN(" in sql)
LEASE_MAX: str = next(sql for sql in QUERIES if sql.startswith("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,lease_until_ms)") and "=MAX(" in sql)
MARK: str = query("INSERT INTO response_blob_tombstones(")
REMOVE: str = query("DELETE FROM workspace_blob_refs WHERE account_id=? AND ((kind='response'")
DISCONNECT: str = query("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='fork-transfer' AND (ref_id=? OR")
RELEASE_IMAGES: str = next(sql for sql in QUERIES if sql == "DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='fork-transfer' AND ref_id>=? AND ref_id<?")
EXPIRE: str = query("DELETE FROM workspace_blob_refs WHERE rowid IN")
CLAIM: str = query("UPDATE workspace_blobs SET status='deleting'")


class ImageBlobHolderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
            self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms,kind,content_type) VALUES(?,?,?,0,0,100,100,1,'ready',0,'image','image/png')", (f"image-{owner}", owner, "ab" * 32))

    def tearDown(self) -> None:
        self.db.close()

    def bind(self, response: str, item: str, owner: str = "a") -> None:
        self.db.execute(BIND, (owner, image_id(response, item), f"image-{owner}", response))

    def delete(self, response: str, owner: str = "a") -> None:
        with self.db:
            self.db.execute(MARK, (owner, response))
            self.db.execute(REMOVE, (owner, response, response))

    def transfer(self, child: str, item: str) -> str:
        identity: str = "image-transfer:" + digest(child) + ":" + image_id(child, item)
        self.db.execute(LEASE_MAX, ("a", identity, "image-a", "86400000"))
        return identity

    def test_fork_reusing_item_id_has_independent_public_holder(self) -> None:
        self.bind("parent", "same-item")
        self.bind("child", "same-item")
        self.assertNotEqual(image_id("parent", "same-item"), image_id("child", "same-item"))
        self.delete("parent")
        self.assertEqual(self.db.execute("SELECT response_id FROM workspace_blob_refs").fetchone()[0], "child")
        self.assertEqual(self.db.execute(CLAIM, ("60000", "image-a", "0")).rowcount, 0)

    def test_canonical_delete_fences_late_insert_and_update(self) -> None:
        self.bind("parent", "image")
        self.db.execute(MARK, ("a", "parent"))
        with self.assertRaises(sqlite3.IntegrityError):
            self.bind("parent", "image")
        self.db.execute(REMOVE, ("a", "parent", "parent"))
        with self.assertRaises(sqlite3.IntegrityError):
            self.bind("parent", "late-image")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('a','response','parent','image-a')")
        self.bind("parent", "same-name-other-owner", "b")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("DELETE FROM response_blob_tombstones")

    def test_private_deadline_only_shortens_and_disconnect_preserves_children(self) -> None:
        base: str = connection_id("socket", "parent")
        private: str = base + ":image:" + image_id("parent", "item")
        for expiry in (1000, 2000, 500):
            self.db.execute(LEASE_MIN, ("a", private, "image-a", str(expiry)))
        self.assertEqual(self.db.execute("SELECT lease_until_ms FROM workspace_blob_refs").fetchone()[0], 500)
        self.db.execute(LEASE_MIN, ("a", base, "image-a", "500"))
        self.bind("child", "item")
        child_transfer: str = self.transfer("child", "item")
        other: str = connection_id("other-socket", "parent") + ":image:" + image_id("parent", "item")
        self.db.execute(LEASE_MIN, ("a", other, "image-a", "500"))
        self.db.execute(DISCONNECT, ("a", base, base + ":image:", base + ":image;"))
        refs: set[str] = {row[0] for row in self.db.execute("SELECT ref_id FROM workspace_blob_refs")}
        self.assertNotIn(private, refs)
        self.assertNotIn(base, refs)
        self.assertIn(image_id("child", "item"), refs)
        self.assertIn(child_transfer, refs)
        self.assertIn(other, refs)
        # The runtime API validates the one-hour authority before any reference write.
        body: str = SOURCE[SOURCE.index("pub(crate) async fn bind_image_connection("):SOURCE.index("pub(crate) async fn bind_image_transfer(")]
        self.assertLess(body.index("validate_connection_lease("), body.index(".prepare("))
        self.assertIn("=MIN(", LEASE_MIN)

    def test_transfer_bridges_parent_delete_until_child_owns_its_holder(self) -> None:
        self.bind("parent", "image")
        self.transfer("child", "image")
        self.delete("parent")
        self.assertEqual(self.db.execute(CLAIM, ("60000", "image-a", "0")).rowcount, 0)
        self.bind("child", "image")
        prefix: str = "image-transfer:" + digest("child")
        self.db.execute(RELEASE_IMAGES, ("a", prefix + ":", prefix + ";"))
        self.assertEqual(self.db.execute("SELECT kind FROM workspace_blob_refs").fetchone()[0], "response-artifact")
        self.assertEqual(self.db.execute(CLAIM, ("60000", "image-a", "0")).rowcount, 0)

    def test_private_expiry_and_owner_scoped_release_do_not_remove_user_refs(self) -> None:
        self.bind("user-response", "image")
        for owner in ("a", "b"):
            base: str = connection_id("socket", "private")
            self.db.execute(LEASE_MIN, (owner, base + ":image:" + image_id("private", "item"), f"image-{owner}", "500"))
        prefix: str = "image-transfer:" + digest("child")
        self.transfer("child", "image")
        self.db.execute(RELEASE_IMAGES, ("b", prefix + ":", prefix + ";"))
        self.assertEqual(self.db.execute(EXPIRE, ("500",)).rowcount, 2)
        kinds: list[str] = sorted(row[0] for row in self.db.execute("SELECT kind FROM workspace_blob_refs"))
        self.assertEqual(kinds, ["fork-transfer", "response-artifact"])
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(BIND, ("b", image_id("foreign", "item"), "image-a", "foreign"))


if __name__ == "__main__":
    unittest.main()
