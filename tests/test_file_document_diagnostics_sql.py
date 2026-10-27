"""The exact native HTTP metadata write is owner/generation bound and immutable."""
from __future__ import annotations

import json
from pathlib import Path
import re
import sqlite3
import unittest

ROOT: Path = Path(__file__).resolve().parents[1]

class DocumentObservationTests(unittest.TestCase):
    def test_known_http_observation_survives_failed_decode_without_identity_mutation(self) -> None:
        db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.addCleanup(db.close)
        db.executescript("CREATE TABLE accounts(id TEXT PRIMARY KEY); CREATE TABLE space_files(id TEXT PRIMARY KEY,account_id TEXT,filename TEXT,status TEXT,sha256 TEXT);")
        for migration in ("0017_file_search.sql", "0025_file_document_extraction.sql", "0030_document_conversion_diagnostics.sql"):
            db.executescript((ROOT / "migrations" / migration).read_text(encoding="utf-8"))
        db.execute("INSERT INTO accounts VALUES('owner')")
        digest: str = "a" * 64
        db.execute("INSERT INTO file_search_extractions(account_id,vector_store_id,file_id,generation,source_sha256,status,created_at_ms) VALUES('owner','store','file','generation',?,'dispatched',0)", (digest,))
        source: str = (ROOT / "crates/agents-worker/src/file_document.rs").read_text(encoding="utf-8")
        sql: str = next(statement for statement in re.findall(r'\.prepare\("([^"\n]+)"\)', source) if statement.startswith("UPDATE file_search_extractions SET diagnostics_json"))
        observed: str = json.dumps({"status":422,"request_id":"native-1","phase":"http_error","input_bytes":593,"response_bytes":127})
        arguments: dict[str, str] = {"1":"owner","2":"store","3":"file","4":"generation","5":digest,"6":observed}
        self.assertEqual(db.execute(sql, {**arguments,"1":"other"}).fetchall(), [])
        self.assertEqual(db.execute(sql, {**arguments,"4":"other"}).fetchall(), [])
        self.assertEqual(db.execute(sql, arguments).fetchall(), [("file",)])
        db.execute("UPDATE file_search_extractions SET status='failed',error_code='file_search_document_upstream_rejected'")
        self.assertEqual(db.execute("SELECT diagnostics_json FROM file_search_extractions").fetchone(), (observed,))
        self.assertEqual(db.execute(sql, arguments).fetchall(), [])
        with self.assertRaises(sqlite3.IntegrityError):
            db.execute("UPDATE file_search_extractions SET diagnostics_json='{}'")
        self.assertEqual(db.execute("SELECT generation,source_sha256,provider_receipt_id FROM file_search_extractions").fetchone(), ("generation",digest,None))

if __name__ == "__main__":
    unittest.main()
