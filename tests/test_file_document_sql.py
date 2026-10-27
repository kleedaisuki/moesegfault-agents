"""Exercise the converter's durable dispatch/result SQL and cache invariants."""
from __future__ import annotations

from pathlib import Path
import re
import sqlite3
import unittest

ROOT: Path = Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/file_document.rs").read_text(encoding="utf-8")
DIGEST: str = "a" * 64


def statement(prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if sql.startswith(prefix))


class DocumentExtractionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.executescript("CREATE TABLE accounts(id TEXT PRIMARY KEY); CREATE TABLE space_files(id TEXT PRIMARY KEY,account_id TEXT,filename TEXT,status TEXT,sha256 TEXT);")
        for migration in ("0017_file_search.sql", "0025_file_document_extraction.sql"):
            self.db.executescript((ROOT / "migrations" / migration).read_text(encoding="utf-8"))
        self.db.executemany("INSERT INTO accounts VALUES(?)", [("a",), ("other",)])
        self.db.execute("INSERT INTO vector_stores(id,account_id,created_at,last_active_at) VALUES('store','a',0,0)")
        self.db.execute("INSERT INTO space_files VALUES('file','a','notes.pdf','processed',?)", (DIGEST,))
        self.db.execute("INSERT INTO vector_store_files(vector_store_id,file_id,account_id,file_sha256,status,created_at,updated_at,generation,lease_id,lease_until) VALUES('store','file','a',?,'in_progress',0,0,'generation','lease',10000)", (DIGEST,))

    def tearDown(self) -> None:
        self.db.close()

    def dispatch(self, owner: str = "a", lease: str = "lease", now: int = 1, digest: str = DIGEST) -> list[tuple[str]]:
        return self.db.execute(statement("INSERT INTO file_search_extractions"), (owner,"store","file","generation",digest,lease,str(now))).fetchall()

    def finish(self, lease: str = "lease", now: int = 1) -> list[tuple[str]]:
        return self.db.execute(statement("UPDATE file_search_extractions SET status"), ("a","store","file","generation",lease,str(now),"completed","雪 text",None)).fetchall()

    def test_cold_retry_keeps_unknown_dispatch_instead_of_buying_another_conversion(self) -> None:
        self.assertEqual(self.dispatch(), [("file",)])
        self.assertEqual(self.dispatch(), [])
        self.assertEqual(self.db.execute("SELECT status,text FROM file_search_extractions").fetchall(), [("dispatched",None)])

    def test_actual_result_is_cached_once_and_not_mutable(self) -> None:
        self.dispatch()
        self.assertEqual(self.finish(), [("file",)])
        self.assertEqual(self.finish(), [])
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE file_search_extractions SET text='other'")
        self.assertEqual(self.db.execute("SELECT text FROM file_search_extractions").fetchone(), ("雪 text",))

    def test_foreign_owner_digest_and_expired_lease_cannot_dispatch(self) -> None:
        self.assertEqual(self.dispatch(owner="other"), [])
        self.assertEqual(self.dispatch(digest="b"*64), [])
        self.assertEqual(self.dispatch(lease="foreign"), [])
        self.assertEqual(self.dispatch(now=10000), [])

    def test_replaced_lease_or_cancelled_membership_rejects_late_result(self) -> None:
        self.dispatch()
        self.db.execute("UPDATE vector_store_files SET lease_id='new'")
        self.assertEqual(self.finish(), [])
        self.db.execute("UPDATE vector_store_files SET lease_id='lease',status='cancelled'")
        self.assertEqual(self.finish(), [])

    def test_deleted_source_cannot_start_a_conversion(self) -> None:
        self.db.execute("UPDATE space_files SET status='deleted'")
        self.assertEqual(self.dispatch(), [])

    def test_identity_and_completed_text_bounds_are_enforced(self) -> None:
        self.dispatch()
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE file_search_extractions SET account_id='other'")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE file_search_extractions SET status='completed',text='' ")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE file_search_extractions SET status='completed',text=?", ("雪"*400000,))


if __name__ == "__main__":
    unittest.main()
