-- Content-free native conversion receipt; source/result identities stay immutable.
ALTER TABLE file_search_extractions ADD COLUMN diagnostics_json TEXT
  CHECK(diagnostics_json IS NULL OR (json_valid(diagnostics_json) AND length(CAST(diagnostics_json AS BLOB))<=8192));
ALTER TABLE file_search_extractions ADD COLUMN provider_receipt_id TEXT
  CHECK(provider_receipt_id IS NULL OR length(provider_receipt_id) BETWEEN 1 AND 256);
CREATE TRIGGER file_search_extractions_diagnostics_immutable
BEFORE UPDATE OF diagnostics_json ON file_search_extractions
WHEN OLD.diagnostics_json IS NOT NULL
BEGIN
  SELECT RAISE(ABORT,'file_search_extraction_diagnostics_immutable');
END;
