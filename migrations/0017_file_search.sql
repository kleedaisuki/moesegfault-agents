-- Owner-scoped retrieval metadata, recoverable ingestion, and derived FTS5 state.
CREATE TABLE vector_stores (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  name TEXT,
  metadata_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(metadata_json)),
  service_profile_json TEXT CHECK(service_profile_json IS NULL OR json_valid(service_profile_json)),
  created_at INTEGER NOT NULL,
  last_active_at INTEGER NOT NULL,
  expires_after_days INTEGER CHECK(expires_after_days BETWEEN 1 AND 365),
  status TEXT NOT NULL DEFAULT 'completed' CHECK(status IN ('in_progress','completed','expired','deleting','deleted')),
  UNIQUE(account_id,id)
);
CREATE INDEX vector_stores_owner ON vector_stores(account_id,status,created_at,id);
CREATE TABLE vector_store_files (
  vector_store_id TEXT NOT NULL REFERENCES vector_stores(id),
  file_id TEXT NOT NULL REFERENCES space_files(id),
  account_id TEXT NOT NULL REFERENCES accounts(id),
  file_sha256 TEXT NOT NULL,
  attributes_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(attributes_json)),
  chunking_json TEXT NOT NULL DEFAULT '{"type":"auto"}' CHECK(json_valid(chunking_json)),
  chunking_revision TEXT NOT NULL DEFAULT 'deepseek-recipe-0.2.0/plain-v1:b64f58e1ec67577c2fc4b6b93689b12da288a974e46c088bad52ae6e5defe5f8:text-splitter-0.33.1:v1',
  service_profile_json TEXT CHECK(service_profile_json IS NULL OR json_valid(service_profile_json)),
  status TEXT NOT NULL CHECK(status IN ('in_progress','completed','cancelled','failed')),
  last_error_json TEXT CHECK(last_error_json IS NULL OR json_valid(last_error_json)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  generation TEXT NOT NULL,
  lease_id TEXT,
  lease_until INTEGER NOT NULL DEFAULT 0,
  retry_at INTEGER NOT NULL DEFAULT 0,
  embedding_model TEXT,
  embedding_dimensions INTEGER,
  embedding_revision TEXT,
  vector_mutation_id TEXT,
  usage_bytes INTEGER NOT NULL DEFAULT 0 CHECK(usage_bytes>=0),
  chunk_count INTEGER NOT NULL DEFAULT 0 CHECK(chunk_count>=0),
  source_staged INTEGER NOT NULL DEFAULT 0 CHECK(source_staged IN (0,1)),
  batch_id TEXT,
  removed_at INTEGER,
  PRIMARY KEY(vector_store_id,file_id)
);
CREATE INDEX vector_store_files_owner ON vector_store_files(account_id,vector_store_id,status,created_at,file_id);
CREATE INDEX vector_store_files_jobs ON vector_store_files(status,retry_at,lease_until);
CREATE INDEX vector_store_files_source ON vector_store_files(account_id,file_id,status);
CREATE TRIGGER vector_store_file_identity BEFORE UPDATE OF account_id,vector_store_id,file_id,file_sha256 ON vector_store_files
WHEN NEW.account_id!=OLD.account_id OR NEW.vector_store_id!=OLD.vector_store_id OR NEW.file_id!=OLD.file_id OR NEW.file_sha256!=OLD.file_sha256
BEGIN SELECT RAISE(ABORT,'vector_store_file_identity_immutable'); END;
CREATE TRIGGER vector_store_file_owner BEFORE INSERT ON vector_store_files
WHEN NOT EXISTS(SELECT 1 FROM vector_stores WHERE id=NEW.vector_store_id AND account_id=NEW.account_id AND status IN ('in_progress','completed'))
  OR NOT EXISTS(SELECT 1 FROM space_files WHERE id=NEW.file_id AND account_id=NEW.account_id AND status='processed' AND sha256=NEW.file_sha256)
BEGIN SELECT RAISE(ABORT,'vector_store_file_owner_mismatch'); END;
CREATE TABLE file_search_chunks (
  id INTEGER PRIMARY KEY,
  chunk_id TEXT NOT NULL UNIQUE,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  vector_store_id TEXT NOT NULL,
  file_id TEXT NOT NULL,
  file_sha256 TEXT NOT NULL,
  generation TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  text TEXT NOT NULL,
  byte_start INTEGER NOT NULL CHECK(byte_start>=0),
  byte_end INTEGER NOT NULL CHECK(byte_end>=byte_start),
  embedding_model TEXT NOT NULL,
  embedding_dimensions INTEGER NOT NULL,
  embedding_revision TEXT NOT NULL,
  vector_status TEXT NOT NULL DEFAULT 'pending' CHECK(vector_status IN ('pending','embedding','embedded','upserting','submitted','visible')),
  vector_json TEXT CHECK(vector_json IS NULL OR json_valid(vector_json)),
  vector_effect_event TEXT,
  vector_effect_started_at INTEGER,
  vector_mutation_id TEXT,
  UNIQUE(vector_store_id,file_id,generation,ordinal),
  FOREIGN KEY(vector_store_id,file_id) REFERENCES vector_store_files(vector_store_id,file_id) ON DELETE CASCADE
);
CREATE INDEX file_search_chunks_source ON file_search_chunks(account_id,vector_store_id,file_id,generation,ordinal);
CREATE TRIGGER file_search_chunk_identity BEFORE UPDATE OF chunk_id,account_id,vector_store_id,file_id,file_sha256,generation,ordinal,text,byte_start,byte_end,embedding_model,embedding_dimensions,embedding_revision ON file_search_chunks
BEGIN SELECT RAISE(ABORT,'file_search_chunk_immutable'); END;
CREATE TRIGGER file_search_chunk_owner BEFORE INSERT ON file_search_chunks
WHEN NOT EXISTS(SELECT 1 FROM vector_store_files vf WHERE vf.vector_store_id=NEW.vector_store_id AND vf.file_id=NEW.file_id AND vf.account_id=NEW.account_id AND vf.file_sha256=NEW.file_sha256 AND vf.generation=NEW.generation AND vf.status='in_progress')
BEGIN SELECT RAISE(ABORT,'file_search_chunk_owner_mismatch'); END;
CREATE VIRTUAL TABLE file_search_fts USING fts5(text,content='file_search_chunks',content_rowid='id',tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER file_search_chunk_fts_insert AFTER INSERT ON file_search_chunks
BEGIN INSERT INTO file_search_fts(rowid,text) VALUES(NEW.id,NEW.text); END;
CREATE TRIGGER file_search_chunk_fts_delete AFTER DELETE ON file_search_chunks
BEGIN INSERT INTO file_search_fts(file_search_fts,rowid,text) VALUES('delete',OLD.id,OLD.text); END;
CREATE TRIGGER file_search_chunk_fts_update AFTER UPDATE OF text ON file_search_chunks
BEGIN
  INSERT INTO file_search_fts(file_search_fts,rowid,text) VALUES('delete',OLD.id,OLD.text);
  INSERT INTO file_search_fts(rowid,text) VALUES(NEW.id,NEW.text);
END;
CREATE TABLE vector_store_file_batches (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  vector_store_id TEXT NOT NULL REFERENCES vector_stores(id),
  created_at INTEGER NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('in_progress','completed','cancelling','cancelled','failed'))
);
CREATE INDEX vector_store_batches_owner ON vector_store_file_batches(account_id,vector_store_id,created_at,id);

CREATE TABLE file_search_cleanup_jobs (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  vector_store_id TEXT NOT NULL,
  file_id TEXT NOT NULL,
  generation TEXT NOT NULL,
  service_profile_json TEXT CHECK(service_profile_json IS NULL OR json_valid(service_profile_json)),
  lease_id TEXT,
  lease_until INTEGER NOT NULL DEFAULT 0,
  retry_at INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(vector_store_id,file_id,generation)
);
CREATE INDEX file_search_cleanup_due ON file_search_cleanup_jobs(retry_at,lease_until);
CREATE TRIGGER file_search_old_generation BEFORE UPDATE OF generation ON vector_store_files
WHEN OLD.generation!=NEW.generation
BEGIN
  INSERT OR IGNORE INTO file_search_cleanup_jobs(account_id,vector_store_id,file_id,generation,service_profile_json)
    VALUES(OLD.account_id,OLD.vector_store_id,OLD.file_id,OLD.generation,OLD.service_profile_json);
END;
CREATE TRIGGER file_search_member_removed AFTER UPDATE OF status,removed_at ON vector_store_files
WHEN NEW.status IN ('cancelled','failed') OR NEW.removed_at IS NOT NULL
BEGIN
  INSERT OR IGNORE INTO file_search_cleanup_jobs(account_id,vector_store_id,file_id,generation,service_profile_json)
    VALUES(NEW.account_id,NEW.vector_store_id,NEW.file_id,NEW.generation,NEW.service_profile_json);
END;
CREATE TRIGGER file_search_source_removed AFTER UPDATE OF status ON space_files
WHEN NEW.status!='processed'
BEGIN
  UPDATE vector_store_files SET status='cancelled',lease_id=NULL,lease_until=0
    WHERE account_id=NEW.account_id AND file_id=NEW.id AND status IN ('in_progress','completed');
END;
CREATE TRIGGER file_search_store_removed AFTER UPDATE OF status ON vector_stores
WHEN NEW.status IN ('expired','deleting','deleted')
BEGIN
  UPDATE vector_store_files SET status='cancelled',lease_id=NULL,lease_until=0
    WHERE account_id=NEW.account_id AND vector_store_id=NEW.id AND status IN ('in_progress','completed');
END;
