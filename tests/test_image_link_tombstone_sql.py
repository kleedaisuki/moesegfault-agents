"""Regression: deferred image metadata must not resurrect a deleted publication."""
from __future__ import annotations

import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/space.rs").read_text(encoding="utf-8")
LINK: str = next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE)
                 if sql.startswith("INSERT INTO space_files(id,account_id,space_id,filename,purpose,content_type,object_key,blob_key"))
GUARD: str = " AND NOT EXISTS(SELECT 1 FROM space_files WHERE account_id=?2 AND inline_key=?12)"


class ImageLinkTombstoneTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("PRAGMA foreign_keys=ON")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('owner','issuer','owner',0,0)")
        self.db.execute("INSERT INTO spaces VALUES('space','owner','Space',0,0)")
        self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms,kind,content_type) VALUES('image','owner',?,0,0,60,60,1,'ready',0,'image','image/png')", ("ab" * 32,))

    def tearDown(self) -> None:
        self.db.close()

    def link(self, identity: str, *, statement: str = LINK, publication: str = "publication") -> int:
        return self.db.execute(statement, (identity, "owner", "space", "generated.png", "image/png", f"space-links/owner/{identity}", "image", "60", "ab" * 32, "image-result", "0", publication, "1000", "100")).rowcount

    def test_atomic_publication_guard_closes_lookup_delete_insert_race(self) -> None:
        self.assertIn(GUARD, LINK)
        self.assertEqual(self.link("first"), 1)
        self.db.execute("UPDATE space_files SET status='deleting',deletion_reason='user' WHERE id='first'")
        self.assertEqual(self.link("while-deleting"), 0)
        self.db.execute("UPDATE space_files SET status='deleted' WHERE id='first'")
        self.assertEqual(self.link("after-delete"), 0)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM space_files").fetchone()[0], 1)
        # Ablating only the new atomic predicate reproduces the pre-fix bug:
        # the old partial inline-key unique index does not cover tombstones.
        self.assertEqual(self.link("negative-control", statement=LINK.replace(GUARD, "")), 1)

    def test_image_lookup_includes_tombstones_without_changing_normal_inline_files(self) -> None:
        image_lookup: str = SOURCE[SOURCE.index("async fn image_link_file("):SOURCE.index("async fn inline_file(")]
        self.assertIn("ORDER BY created_at_ms,id LIMIT 1", image_lookup)
        self.assertNotIn("status IN", image_lookup)
        self.assertIn('fail("file_publication_deleted")', SOURCE)
        normal_lookup: str = SOURCE[SOURCE.index("async fn inline_file("):SOURCE.index("async fn set_upload_outcome(")]
        self.assertIn("status IN ('uploading','processed')", normal_lookup)


if __name__ == "__main__":
    unittest.main()
