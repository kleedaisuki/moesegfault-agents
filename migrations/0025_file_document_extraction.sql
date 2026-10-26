-- Recoverable derived text; source Files and financial evidence are independent.
CREATE TABLE file_search_extractions (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  vector_store_id TEXT NOT NULL,
  file_id TEXT NOT NULL,
  generation TEXT NOT NULL,
  source_sha256 TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('dispatched','completed','failed')),
  text TEXT,
  error_code TEXT,
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY(vector_store_id,file_id,generation),
  CHECK(length(source_sha256)=64),
  CHECK(created_at_ms>=0),
  CHECK((status='completed' AND text IS NOT NULL AND length(CAST(text AS BLOB)) BETWEEN 1 AND 1048576 AND error_code IS NULL)
     OR (status='dispatched' AND text IS NULL AND error_code IS NULL)
     OR (status='failed' AND text IS NULL AND error_code IS NOT NULL))
);
CREATE INDEX file_search_extractions_owner ON file_search_extractions(account_id,vector_store_id,file_id,generation);

CREATE TRIGGER file_search_extractions_identity_immutable
BEFORE UPDATE OF account_id,vector_store_id,file_id,generation,source_sha256,created_at_ms
ON file_search_extractions
BEGIN
  SELECT RAISE(ABORT,'file_search_extraction_identity_immutable');
END;
CREATE TRIGGER file_search_extractions_result_immutable
BEFORE UPDATE OF status,text,error_code ON file_search_extractions
WHEN OLD.status!='dispatched'
BEGIN
  SELECT RAISE(ABORT,'file_search_extraction_result_immutable');
END;
