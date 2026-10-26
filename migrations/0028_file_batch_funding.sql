-- Each bounded effect pins its own immutable financial identity and tariff.
ALTER TABLE vector_store_files ADD COLUMN funding_blocked INTEGER NOT NULL DEFAULT 0 CHECK(funding_blocked IN (0,1));
ALTER TABLE vector_store_files ADD COLUMN funding_required_microusd INTEGER NOT NULL DEFAULT 0 CHECK(funding_required_microusd>=0);
ALTER TABLE vector_store_files ADD COLUMN lease_turn TEXT;
ALTER TABLE vector_store_files ADD COLUMN staged_chunks INTEGER NOT NULL DEFAULT 0 CHECK(staged_chunks>=0);
UPDATE vector_store_files SET staged_chunks=(SELECT COUNT(*) FROM file_search_chunks c WHERE c.account_id=vector_store_files.account_id AND c.vector_store_id=vector_store_files.vector_store_id AND c.file_id=vector_store_files.file_id AND c.generation=vector_store_files.generation);
CREATE TABLE file_search_funding (
 id TEXT PRIMARY KEY,
 account_id TEXT NOT NULL REFERENCES accounts(id),
 vector_store_id TEXT NOT NULL,
 file_id TEXT NOT NULL,
 generation TEXT NOT NULL,
 operation TEXT NOT NULL,
 quantity TEXT NOT NULL,
 calls INTEGER NOT NULL CHECK(calls>=0),
 pricing_json TEXT NOT NULL CHECK(json_valid(pricing_json)),
 amount_microusd INTEGER NOT NULL CHECK(amount_microusd>0),
 created_at_ms INTEGER NOT NULL,
 UNIQUE(account_id,vector_store_id,file_id,generation,operation)
);
CREATE INDEX file_search_funding_generation ON file_search_funding(account_id,vector_store_id,file_id,generation);
CREATE TRIGGER file_search_funding_immutable BEFORE UPDATE ON file_search_funding BEGIN SELECT RAISE(ABORT,'file_search_funding_immutable'); END;
CREATE TRIGGER file_search_new_generation_funding AFTER UPDATE OF generation ON vector_store_files WHEN OLD.generation!=NEW.generation BEGIN
 UPDATE vector_store_files SET funding_blocked=0,funding_required_microusd=0,staged_chunks=0 WHERE vector_store_id=NEW.vector_store_id AND file_id=NEW.file_id;
END;
