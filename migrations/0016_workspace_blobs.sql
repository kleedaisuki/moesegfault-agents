-- Owner-scoped immutable archive objects; reference count is not copied into R2.
CREATE TABLE workspace_blobs (
  object_key TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  sha256 TEXT NOT NULL CHECK (length(sha256)=64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
  format_version INTEGER NOT NULL CHECK (format_version IN (0,1)),
  generation INTEGER NOT NULL CHECK (generation>=0),
  archive_bytes INTEGER NOT NULL CHECK (archive_bytes>0 AND archive_bytes<=268435456),
  logical_bytes INTEGER NOT NULL CHECK (logical_bytes>=0),
  entries INTEGER NOT NULL CHECK (entries>=0),
  status TEXT NOT NULL CHECK (status IN ('uploading','ready','deleting')),
  lease_until_ms INTEGER NOT NULL CHECK (lease_until_ms>=0),
  kind TEXT NOT NULL DEFAULT 'workspace' CHECK (kind IN ('workspace','image')),
  content_type TEXT NOT NULL DEFAULT 'application/zstd',
  CHECK ((kind='workspace' AND format_version=1 AND content_type='application/zstd')
    OR (kind='image' AND format_version=0 AND archive_bytes<=16777216
      AND content_type IN ('image/png','image/jpeg','image/webp')))
);
-- Retired file rows retain provenance after the physical object is collected.
-- Only active metadata and strong refs hold blob lifetime, not this tombstone.
ALTER TABLE space_files ADD COLUMN blob_key TEXT;
CREATE INDEX space_files_blob ON space_files(blob_key,status);
CREATE TRIGGER space_file_blob_owner BEFORE INSERT ON space_files
WHEN NEW.blob_key IS NOT NULL AND NOT EXISTS(SELECT 1 FROM workspace_blobs WHERE object_key=NEW.blob_key AND account_id=NEW.account_id AND kind='image' AND status='ready' AND archive_bytes=NEW.bytes AND sha256=NEW.sha256 AND content_type=NEW.content_type)
BEGIN SELECT RAISE(ABORT,'space_file_blob_owner_mismatch'); END;
CREATE TRIGGER immutable_space_file_blob BEFORE UPDATE ON space_files
WHEN NEW.blob_key IS NOT OLD.blob_key
BEGIN SELECT RAISE(ABORT,'immutable_space_file_blob'); END;

CREATE UNIQUE INDEX workspace_blob_content ON workspace_blobs(account_id,kind,format_version,sha256,archive_bytes,content_type) WHERE status='ready';
CREATE INDEX workspace_blob_capacity ON workspace_blobs(account_id,object_key,archive_bytes);
CREATE INDEX workspace_blob_due ON workspace_blobs(lease_until_ms,object_key);

CREATE TABLE workspace_blob_refs (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  kind TEXT NOT NULL CHECK (kind IN ('session-current','response','version','fork-transfer','response-artifact','event-artifact','space-file')),
  ref_id TEXT NOT NULL CHECK (length(ref_id)>0),
  session_id TEXT REFERENCES sessions(id),
  object_key TEXT NOT NULL REFERENCES workspace_blobs(object_key),
  lease_until_ms INTEGER,
  response_id TEXT,
  maintenance_due_ms INTEGER NOT NULL DEFAULT 0 CHECK(maintenance_due_ms>=0),
  CHECK ((kind IN ('response-artifact','event-artifact') AND response_id IS NOT NULL AND length(response_id)>0) OR (kind NOT IN ('response-artifact','event-artifact') AND response_id IS NULL)),
  CHECK ((kind='fork-transfer' AND lease_until_ms IS NOT NULL AND lease_until_ms>=0)
    OR (kind!='fork-transfer' AND lease_until_ms IS NULL)),
  PRIMARY KEY(account_id,kind,ref_id)
);
CREATE INDEX workspace_blob_file_maintenance ON workspace_blob_refs(kind,maintenance_due_ms);
CREATE INDEX workspace_blob_response_artifacts ON workspace_blob_refs(account_id,response_id,kind);
CREATE INDEX workspace_blob_references ON workspace_blob_refs(object_key);
CREATE INDEX workspace_blob_transfer_due ON workspace_blob_refs(kind,lease_until_ms);
CREATE TRIGGER workspace_blob_ref_owner BEFORE INSERT ON workspace_blob_refs
WHEN (NEW.session_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM sessions WHERE id=NEW.session_id AND account_id=NEW.account_id))
  OR NOT EXISTS (SELECT 1 FROM workspace_blobs WHERE object_key=NEW.object_key AND account_id=NEW.account_id AND status='ready')
BEGIN SELECT RAISE(ABORT,'workspace_blob_reference_unavailable'); END;
CREATE TRIGGER workspace_blob_ref_update BEFORE UPDATE ON workspace_blob_refs
WHEN NEW.account_id!=OLD.account_id OR NEW.kind!=OLD.kind OR NEW.ref_id!=OLD.ref_id OR NEW.session_id IS NOT OLD.session_id OR NEW.response_id IS NOT OLD.response_id
  OR (OLD.kind IN ('response','version','fork-transfer','response-artifact','event-artifact','space-file') AND NEW.object_key!=OLD.object_key)
  OR NOT EXISTS (SELECT 1 FROM workspace_blobs WHERE object_key=NEW.object_key AND account_id=NEW.account_id AND status='ready')
BEGIN SELECT RAISE(ABORT,'immutable_workspace_blob_reference'); END;
CREATE TRIGGER workspace_blob_ref_delete BEFORE DELETE ON workspace_blob_refs
WHEN OLD.kind IN ('version','event-artifact')
BEGIN SELECT RAISE(ABORT,'immutable_workspace_blob_reference'); END;
CREATE TRIGGER workspace_blob_identity BEFORE UPDATE ON workspace_blobs
WHEN NEW.object_key!=OLD.object_key OR NEW.account_id!=OLD.account_id OR NEW.sha256!=OLD.sha256
  OR NEW.kind!=OLD.kind OR NEW.content_type!=OLD.content_type
  OR NEW.format_version!=OLD.format_version OR NEW.generation!=OLD.generation
  OR NEW.archive_bytes!=OLD.archive_bytes OR NEW.logical_bytes!=OLD.logical_bytes OR NEW.entries!=OLD.entries
  OR (OLD.status='deleting' AND NEW.status!='deleting') OR (OLD.status='ready' AND NEW.status='uploading')
  OR (NEW.status='deleting' AND (EXISTS(SELECT 1 FROM workspace_blob_refs WHERE object_key=OLD.object_key) OR EXISTS(SELECT 1 FROM space_files WHERE blob_key=OLD.object_key AND status IN ('uploading','processed','deleting'))))
BEGIN SELECT RAISE(ABORT,'immutable_workspace_blob'); END;
CREATE TRIGGER workspace_blob_delete BEFORE DELETE ON workspace_blobs
WHEN OLD.status!='deleting' OR EXISTS(SELECT 1 FROM workspace_blob_refs WHERE object_key=OLD.object_key) OR EXISTS(SELECT 1 FROM space_files WHERE blob_key=OLD.object_key AND status IN ('uploading','processed','deleting'))
BEGIN SELECT RAISE(ABORT,'workspace_blob_still_live'); END;


-- Capacity is physical ownership, not the number or visibility of references.
-- archive_bytes is the shared engine's physical size for every blob kind.
CREATE TABLE owner_blob_capacity_holds (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  operation_id TEXT NOT NULL CHECK(length(operation_id)>0),
  bytes INTEGER NOT NULL CHECK(bytes>0 AND bytes<=16777216),
  expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms>=0),
  PRIMARY KEY(account_id,operation_id)
);
CREATE INDEX owner_blob_capacity_due ON owner_blob_capacity_holds(expires_at_ms);

-- Consumers filter by owner before GROUP BY object_key/MAX(bytes); grouping in
-- the view prevents SQLite from pushing that owner filter into every branch.
CREATE VIEW space_storage_capacity AS
  SELECT account_id,COALESCE(blob_key,object_key) AS object_key,bytes FROM space_files WHERE status IN ('uploading','processed','deleting')
  UNION ALL SELECT account_id,object_key,archive_bytes FROM workspace_versions
  UNION ALL SELECT account_id,object_key,archive_bytes FROM workspace_version_uploads WHERE status IN ('pending','abandoned')
  UNION ALL SELECT account_id,object_key,archive_bytes FROM workspace_blobs
  UNION ALL SELECT account_id,'capacity-hold:'||operation_id,bytes FROM owner_blob_capacity_holds;
