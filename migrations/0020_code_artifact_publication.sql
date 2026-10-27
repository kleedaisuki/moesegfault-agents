-- Additive for databases where the original blob migration is already applied.
CREATE INDEX IF NOT EXISTS space_files_publication
  ON space_files(account_id,inline_key,upload_outcome,status);
