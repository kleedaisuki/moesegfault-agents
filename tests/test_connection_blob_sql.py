"""Only the new private-connection holder; sealed baseline suites are not rerun."""
from __future__ import annotations

import hashlib
import json
import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/workspace_blobs.rs").read_text(encoding="utf-8")
QUERIES: list[str] = re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE)
BIND: str = next(sql for sql in QUERIES if "lease_until_ms=MIN(" in sql)
RELEASE: str = next(sql for sql in QUERIES if sql.startswith("DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='fork-transfer'"))
EXPIRE: str = next(sql for sql in QUERIES if sql.startswith("DELETE FROM workspace_blob_refs WHERE rowid IN"))
CLAIM: str = next(sql for sql in QUERIES if sql.startswith("UPDATE workspace_blobs SET status='deleting'"))


def ref_id(connection: str, response: str) -> str:
    encoded: bytes = json.dumps([connection, response], separators=(",", ":"), ensure_ascii=False).encode()
    return "connection:" + hashlib.sha256(encoded).hexdigest()


class ConnectionBlobTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("PRAGMA foreign_keys=ON")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
        for key, digest in (("archive", "a"), ("other", "b")):
            self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms) VALUES(?,'a',?,1,0,100,200,3,'ready',0)", (key, digest * 64))

    def tearDown(self) -> None:
        self.db.close()

    def bind(self, connection: str, response: str, expiry: int, *, owner: str = "a", key: str = "archive") -> None:
        self.db.execute(BIND, (owner, ref_id(connection, response), key, str(expiry)))

    def test_exact_connection_expiry_is_not_extended_to_fork_day_or_retry_hour(self) -> None:
        self.bind("connection", "response", 3_600_000)
        self.bind("connection", "response", 3_600_001)
        self.assertEqual(self.db.execute("SELECT lease_until_ms FROM workspace_blob_refs").fetchone()[0], 3_600_000)
        self.assertEqual(self.db.execute(EXPIRE, ("3599999",)).rowcount, 0)
        self.assertEqual(self.db.execute(EXPIRE, ("3600000",)).rowcount, 1)
        self.assertEqual(self.db.execute(CLAIM, ("3660000", "archive", "3600000")).rowcount, 1)

    def test_release_is_owner_connection_response_scoped_and_preserves_user_holders(self) -> None:
        self.bind("first", "response", 1000)
        self.bind("second", "response", 1000)
        for kind in ("response", "version", "session-current"):
            self.db.execute("INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key) VALUES('a',?,?,'archive')", (kind, kind))
        self.assertEqual(self.db.execute(RELEASE, ("b", ref_id("first", "response"))).rowcount, 0)
        self.assertEqual(self.db.execute(RELEASE, ("a", ref_id("first", "response"))).rowcount, 1)
        remaining: list[str] = sorted(row[0] for row in self.db.execute("SELECT kind FROM workspace_blob_refs"))
        self.assertEqual(remaining, ["fork-transfer", "response", "session-current", "version"])
        self.assertEqual(self.db.execute(CLAIM, ("60000", "archive", "0")).rowcount, 0)
        with self.assertRaises(sqlite3.IntegrityError):
            self.bind("second", "response", 1000, key="other")
        with self.assertRaises(sqlite3.IntegrityError):
            self.bind("foreign", "response", 1000, owner="b")

    def test_private_seal_never_uses_a_permanent_response_holder(self) -> None:
        archives: str = (ROOT / "crates/agents-worker/src/workspace_archives.rs").read_text(encoding="utf-8")
        private: str = archives[archives.index("pub(crate) async fn seal_connection_archive("):archives.index("/// Admission has not reserved")]
        self.assertIn("blobs::bind_connection", private)
        self.assertIn("self.seal_archive().await?", private)
        self.assertNotIn("RefKind::Response", private)
        self.assertLess(private.index("validate_connection_lease"), private.index("self.seal_archive"))


if __name__ == "__main__":
    unittest.main()
