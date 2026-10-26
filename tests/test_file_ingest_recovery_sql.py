"""Execute the actual ingest phase SQL against the unpublished 0017 schema."""
from __future__ import annotations

import json
from pathlib import Path
import re
import sqlite3
import unittest

ROOT: Path = Path(__file__).resolve().parents[1]
INDEX: str = (ROOT / "crates/agents-worker/src/file_index.rs").read_text(encoding="utf-8")
EFFECTS: str = (ROOT / "crates/agents-worker/src/file_index_effects.rs").read_text(encoding="utf-8")


def statement(source: str, prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', source) if sql.startswith(prefix))


class IngestRecoveryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.executescript("CREATE TABLE accounts(id TEXT PRIMARY KEY); CREATE TABLE space_files(id TEXT PRIMARY KEY,account_id TEXT,filename TEXT,status TEXT,sha256 TEXT);")
        self.db.executescript((ROOT / "migrations/0017_file_search.sql").read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('a')")
        self.db.execute("INSERT INTO vector_stores(id,account_id,created_at,last_active_at) VALUES('store','a',0,0)")
        self.db.execute("INSERT INTO space_files VALUES('file','a','data.txt','processed','digest')")
        self.db.execute("INSERT INTO vector_store_files(vector_store_id,file_id,account_id,file_sha256,status,created_at,updated_at,generation,lease_id,lease_until,source_staged,chunk_count) VALUES('store','file','a','digest','in_progress',0,0,'generation','lease',10000,1,1)")
        self.db.execute("INSERT INTO file_search_chunks(chunk_id,account_id,vector_store_id,file_id,file_sha256,generation,ordinal,text,byte_start,byte_end,embedding_model,embedding_dimensions,embedding_revision) VALUES('chunk','a','store','file','digest','generation',0,'hello',0,5,'model',2,'revision')")
        self.identity: tuple[str, str, str, str, str, str] = ("a", "store", "file", "generation", "lease", "1")

    def tearDown(self) -> None:
        self.db.close()

    def phase(self, phase: str, effect: str, previous: str) -> list[tuple[str]]:
        sql: str = statement(EFFECTS, "UPDATE file_search_chunks SET vector_status=?8")
        return self.db.execute(sql, (*self.identity, '["chunk"]', phase, effect, previous)).fetchall()

    def save_embedding(self) -> None:
        self.db.execute(statement(EFFECTS, "UPDATE file_search_chunks SET vector_status='embedded'"), (*self.identity, "chunk", "[1,0]")).fetchall()

    def pending(self) -> sqlite3.Row | None:
        self.db.row_factory = sqlite3.Row
        row: sqlite3.Row | None = self.db.execute(statement(INDEX, "SELECT ordinal,byte_start,byte_end,text,vector_status"), ("a", "store", "file", "generation", "model", "2", "revision")).fetchone()
        self.db.row_factory = None
        return row

    def test_known_embedding_and_mutation_survive_visibility_retry(self) -> None:
        self.assertEqual(self.phase("embedding", "embed", "pending"), [("chunk",)])
        self.save_embedding()
        cached: sqlite3.Row | None = self.pending()
        self.assertIsNotNone(cached)
        assert cached is not None
        self.assertEqual(cached["vector_status"], "embedded")
        self.assertEqual(json.loads(cached["vector_json"]), [1, 0])
        self.assertEqual(self.phase("upserting", "upsert", "embedded"), [("chunk",)])
        self.db.execute(statement(EFFECTS, "UPDATE file_search_chunks SET vector_status='submitted'"), (*self.identity, '["chunk"]', "actual-mutation")).fetchall()
        pending: sqlite3.Row | None = self.pending()
        assert pending is not None
        self.assertEqual(pending["vector_mutation_id"], "actual-mutation")
        self.assertEqual(pending["vector_status"], "submitted")
        self.assertEqual(self.phase("embedding", "must-not-embed-again", "pending"), [])

    def test_query_verified_ambiguous_upsert_can_publish_without_fabricated_mutation(self) -> None:
        self.phase("embedding", "embed", "pending")
        self.save_embedding()
        self.phase("upserting", "original-upsert", "embedded")
        self.db.execute(statement(INDEX, "UPDATE file_search_chunks SET vector_status='visible'"), (*self.identity, '["chunk"]'))
        completed: list[tuple[str]] = self.db.execute(statement(INDEX, "UPDATE vector_store_files SET status='completed'"), (*self.identity, "model", "2", "revision", "1")).fetchall()
        self.assertEqual(completed, [("file",)])
        row: tuple[str, str | None] = self.db.execute("SELECT status,vector_mutation_id FROM vector_store_files").fetchone()
        self.assertEqual(row, ("completed", None))
        self.assertIsNone(self.db.execute("SELECT vector_json FROM file_search_chunks").fetchone()[0])

    def test_old_lease_or_deleted_source_cannot_start_embedding(self) -> None:
        self.db.execute("UPDATE vector_store_files SET lease_id='new-lease'")
        self.assertEqual(self.phase("embedding", "old-effect", "pending"), [])
        self.db.execute("UPDATE vector_store_files SET lease_id='lease'")
        self.db.execute("UPDATE space_files SET status='deleted'")
        self.assertEqual(self.phase("embedding", "deleted-effect", "pending"), [])
        self.assertEqual(self.db.execute("SELECT vector_status FROM file_search_chunks").fetchone()[0], "pending")

    def test_unknown_embedding_is_distinct_from_fresh_pending_work(self) -> None:
        self.phase("embedding", "unknown-original", "pending")
        row: sqlite3.Row | None = self.pending()
        assert row is not None
        self.assertEqual(row["vector_status"], "embedding")
        self.assertIsNone(row["vector_json"])
        self.assertEqual(row["vector_effect_event"], "unknown-original")
        self.assertEqual(self.phase("embedding", "replay", "pending"), [])

    def test_source_or_store_deletion_fences_writer_and_queues_cleanup(self) -> None:
        self.db.execute("UPDATE space_files SET status='deleted' WHERE id='file'")
        self.assertEqual(self.db.execute("SELECT status,lease_id FROM vector_store_files").fetchone(), ("cancelled", None))
        self.assertEqual(self.db.execute("SELECT account_id,vector_store_id,file_id,generation FROM file_search_cleanup_jobs").fetchall(), [("a", "store", "file", "generation")])
        self.assertEqual(self.phase("embedding", "late-write", "pending"), [])
        self.db.execute("UPDATE vector_stores SET status='deleting' WHERE id='store'")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM file_search_cleanup_jobs").fetchone()[0], 1)
        self.assertIsNotNone(self.db.execute("SELECT id FROM space_files WHERE id='file'").fetchone())

    def test_reattach_preserves_old_generation_cleanup_and_new_source(self) -> None:
        self.db.execute("UPDATE vector_store_files SET service_profile_json=?", ('{"binding":"old"}',))
        self.db.execute("UPDATE vector_store_files SET generation='new-generation',service_profile_json=?", ('{"binding":"new"}',))
        self.assertEqual(self.db.execute("SELECT generation,service_profile_json FROM file_search_cleanup_jobs").fetchone(), ("generation", '{"binding":"old"}'))
        self.db.execute("INSERT INTO file_search_chunks(chunk_id,account_id,vector_store_id,file_id,file_sha256,generation,ordinal,text,byte_start,byte_end,embedding_model,embedding_dimensions,embedding_revision) VALUES('new-chunk','a','store','file','digest','new-generation',0,'newtopic',0,8,'model',2,'revision')")
        gc: str = (ROOT / "crates/agents-worker/src/file_index_gc.rs").read_text(encoding="utf-8")
        self.db.execute("UPDATE file_search_cleanup_jobs SET lease_id='gc-lease'")
        self.db.execute(statement(gc, "DELETE FROM file_search_chunks WHERE"), ("a", "store", "file", "generation", '["chunk","new-chunk"]', "gc-lease"))
        self.assertEqual(self.db.execute("SELECT chunk_id FROM file_search_chunks").fetchall(), [("new-chunk",)])
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM file_search_fts WHERE file_search_fts MATCH 'hello'").fetchone()[0], 0)


if __name__ == "__main__":
    unittest.main()
