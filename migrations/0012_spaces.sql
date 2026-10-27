CREATE TABLE spaces (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL UNIQUE REFERENCES accounts(id),
  name TEXT NOT NULL DEFAULT 'My Space' CHECK (length(name) BETWEEN 1 AND 100),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

CREATE TABLE space_files (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  space_id TEXT NOT NULL REFERENCES spaces(id),
  filename TEXT NOT NULL CHECK (length(filename) BETWEEN 1 AND 255),
  purpose TEXT NOT NULL CHECK (purpose IN ('user_data','assistants','agents','vision','batch','fine-tune','evals')),
  content_type TEXT NOT NULL,
  object_key TEXT NOT NULL UNIQUE,
  bytes INTEGER NOT NULL CHECK (bytes >= 0 AND bytes <= 67108864),
  sha256 TEXT NOT NULL CHECK (length(sha256)=64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
  status TEXT NOT NULL CHECK (status IN ('uploading','processed','deleting','deleted','failed')),
  reservation_id TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  recovery_at_ms INTEGER NOT NULL,
  recovery_attempts INTEGER NOT NULL DEFAULT 0 CHECK (recovery_attempts>=0),
  upload_outcome TEXT NOT NULL DEFAULT 'pending' CHECK (upload_outcome IN ('pending','stored','failed')),
  upload_operations INTEGER CHECK (upload_operations IN (0,1)),
  inline_key TEXT
);
CREATE INDEX space_files_owner ON space_files(account_id,status,created_at_ms,id);
CREATE INDEX space_files_recovery ON space_files(status,recovery_at_ms);
CREATE UNIQUE INDEX space_files_inline ON space_files(account_id,inline_key) WHERE inline_key IS NOT NULL AND status IN ('uploading','processed');
CREATE TRIGGER space_file_owner BEFORE INSERT ON space_files
WHEN NOT EXISTS (SELECT 1 FROM spaces WHERE id=NEW.space_id AND account_id=NEW.account_id)
BEGIN SELECT RAISE(ABORT,'space_file_owner_mismatch'); END;
CREATE TRIGGER space_file_identity BEFORE UPDATE ON space_files
WHEN NEW.id!=OLD.id OR NEW.account_id!=OLD.account_id OR NEW.space_id!=OLD.space_id
  OR NEW.object_key!=OLD.object_key OR NEW.bytes!=OLD.bytes OR NEW.sha256!=OLD.sha256
  OR NEW.content_type!=OLD.content_type OR NEW.purpose!=OLD.purpose
  OR NEW.reservation_id!=OLD.reservation_id OR NEW.created_at_ms!=OLD.created_at_ms
  OR NEW.inline_key IS NOT OLD.inline_key
  OR (OLD.upload_outcome!='pending' AND NEW.upload_outcome!=OLD.upload_outcome)
  OR (OLD.upload_operations IS NOT NULL AND NEW.upload_operations IS NOT OLD.upload_operations)
  OR (OLD.status='deleted' AND NEW.status!=OLD.status)
  OR (OLD.status='failed' AND NEW.status NOT IN ('failed','deleted'))
BEGIN SELECT RAISE(ABORT,'immutable_space_file_identity'); END;

CREATE TABLE workspace_versions (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT NOT NULL REFERENCES sessions(id),
  operation_id TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '',
  object_key TEXT NOT NULL,
  sha256 TEXT NOT NULL CHECK (length(sha256)=64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
  format_version INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  archive_bytes INTEGER NOT NULL,
  logical_bytes INTEGER NOT NULL,
  entries INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  UNIQUE(account_id,session_id,operation_id)
);
CREATE INDEX workspace_versions_owner ON workspace_versions(account_id,created_at_ms,id);
CREATE INDEX workspace_versions_objects ON workspace_versions(object_key);
CREATE INDEX workspace_versions_dedup ON workspace_versions(account_id,sha256,archive_bytes);
CREATE TRIGGER workspace_version_owner BEFORE INSERT ON workspace_versions
WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE id=NEW.session_id AND account_id=NEW.account_id)
BEGIN SELECT RAISE(ABORT,'workspace_version_owner_mismatch'); END;
CREATE TRIGGER immutable_workspace_version BEFORE UPDATE ON workspace_versions
BEGIN SELECT RAISE(ABORT,'immutable_workspace_version'); END;
CREATE TRIGGER immutable_workspace_version_delete BEFORE DELETE ON workspace_versions
BEGIN SELECT RAISE(ABORT,'immutable_workspace_version'); END;

CREATE TABLE workspace_version_uploads (
  object_key TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT NOT NULL REFERENCES sessions(id),
  operation_id TEXT NOT NULL,
  archive_bytes INTEGER NOT NULL CHECK (archive_bytes>=0 AND archive_bytes<=268435456),
  version_json TEXT NOT NULL CHECK (json_valid(version_json)),
  created_at_ms INTEGER NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending','published','abandoned')),
  UNIQUE(account_id,session_id,operation_id)
);
CREATE INDEX workspace_version_uploads_owner ON workspace_version_uploads(account_id,status);
CREATE TRIGGER workspace_version_upload_owner BEFORE INSERT ON workspace_version_uploads
WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE id=NEW.session_id AND account_id=NEW.account_id)
BEGIN SELECT RAISE(ABORT,'workspace_version_upload_owner_mismatch'); END;
