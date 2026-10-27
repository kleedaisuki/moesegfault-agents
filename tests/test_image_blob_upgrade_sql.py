"""Upgrade populated deployed blob references without the future quota schema."""
from __future__ import annotations

import hashlib
from pathlib import Path
import sqlite3
import unittest

ROOT: Path = Path(__file__).resolve().parents[1]


class ImageBlobUpgradeSqlTests(unittest.TestCase):
    def test_populated_upgrade_preserves_bytes_references_views_and_guards(self) -> None:
        db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.addCleanup(db.close)
        db.execute("PRAGMA foreign_keys=ON")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            if int(migration.name.split("_")[0]) <= 20:
                db.executescript(migration.read_text(encoding="utf-8"))
        db.execute("INSERT INTO accounts VALUES('owner','issuer','subject',0,0)")
        db.execute("INSERT INTO conversation_routes VALUES('conv_old','owner','hash',1)")
        db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES('archive','owner',?,1,0,4096,32,1,'ready',1000)",
                   (hashlib.sha256(b"archive").hexdigest(),))
        rows: list[tuple[str, str, int | None, str | None]] = [
            ("response", "resp_old", None, None),
            ("version", "version_old", None, None),
            ("fork-transfer", "transfer_old", 99999, None),
            ("response-artifact", "resp_old:artifact", None, "resp_old"),
        ]
        for kind, identity, lease, response in rows:
            db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,lease_until_ms,response_id) VALUES('owner',?,?,'archive',?,?)",
                       (kind, identity, lease, response))
        db.commit()
        columns: str = "account_id,kind,ref_id,session_id,object_key,lease_until_ms,response_id,maintenance_due_ms"
        before: list[tuple[object, ...]] = db.execute(f"SELECT {columns} FROM workspace_blob_refs ORDER BY kind,ref_id").fetchall()
        route: list[tuple[object, ...]] = db.execute("SELECT * FROM conversation_routes").fetchall()
        blob: list[tuple[object, ...]] = db.execute("SELECT * FROM workspace_blobs").fetchall()
        capacity: list[tuple[object, ...]] = db.execute("SELECT * FROM space_storage_capacity").fetchall()
        upgrade: str = (ROOT / "migrations/0022_workspace_blob_lifecycle.sql").read_text(encoding="utf-8")
        db.executescript("BEGIN;\n" + upgrade + "\nCOMMIT;")
        self.assertEqual(db.execute(f"SELECT {columns} FROM workspace_blob_refs ORDER BY kind,ref_id").fetchall(), before)
        self.assertEqual(db.execute("SELECT * FROM conversation_routes").fetchall(), route)
        self.assertEqual(db.execute("SELECT * FROM workspace_blobs").fetchall(), blob)
        self.assertEqual(db.execute("SELECT * FROM space_storage_capacity").fetchall(), capacity)
        self.assertEqual(db.execute("PRAGMA foreign_key_check").fetchall(), [])
        self.assertEqual(db.execute("PRAGMA legacy_alter_table").fetchone()[0], 0)
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_workspace_blob"):
            db.execute("UPDATE workspace_blobs SET status='deleting' WHERE object_key='archive'")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_workspace_blob_reference"):
            db.execute("DELETE FROM workspace_blob_refs WHERE kind='version'")
        db.execute("INSERT INTO response_blob_tombstones VALUES('owner','resp_deleted')")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "response_blob_deleted"):
            db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id) VALUES('owner','response-artifact','late','archive','resp_deleted')")
        db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('owner','session-current','still-compatible','archive')")


if __name__ == "__main__":
    unittest.main()
