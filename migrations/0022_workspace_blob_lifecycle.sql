-- Extend the deployed 0016 reference engine without rewriting its migration.
-- Requires execution metadata from 0019 before image execution can be published.
-- D1 supports transaction-scoped legacy_alter_table; keep parent guards/views
-- referring to the canonical table while replacing this child table.
-- https://developers.cloudflare.com/d1/sql-api/sql-statements/#pragma-legacy_alter_table--onoff
PRAGMA legacy_alter_table=ON;

CREATE TABLE workspace_blob_refs_v2 (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  kind TEXT NOT NULL CHECK (kind IN ('session-current','conversation-current','response','version','fork-transfer','response-artifact','event-artifact','space-file','image-execution','image-delivery','conversation-artifact')),
  ref_id TEXT NOT NULL CHECK (length(ref_id)>0),
  session_id TEXT REFERENCES sessions(id),
  object_key TEXT NOT NULL REFERENCES workspace_blobs(object_key),
  lease_until_ms INTEGER,
  response_id TEXT,
  execution_id TEXT,
  artifact_id TEXT,
  execution_generation TEXT,
  maintenance_due_ms INTEGER NOT NULL DEFAULT 0 CHECK(maintenance_due_ms>=0),
  CHECK ((kind='image-execution' AND session_id IS NOT NULL AND response_id IS NOT NULL
      AND execution_id IS NOT NULL AND length(execution_id)>0
      AND artifact_id IS NOT NULL AND length(artifact_id)>0
      AND execution_generation IS NOT NULL AND length(execution_generation) BETWEEN 1 AND 128)
    OR (kind='conversation-artifact' AND execution_id IS NULL AND artifact_id IS NOT NULL AND length(artifact_id)>0 AND execution_generation IS NULL)
    OR (kind NOT IN ('image-execution','conversation-artifact') AND execution_id IS NULL AND artifact_id IS NULL AND execution_generation IS NULL)),
  CHECK ((kind IN ('response-artifact','event-artifact','image-execution','image-delivery') AND response_id IS NOT NULL AND length(response_id)>0) OR (kind NOT IN ('response-artifact','event-artifact','image-execution','image-delivery') AND response_id IS NULL)),
  CHECK ((kind IN ('fork-transfer','image-delivery') AND lease_until_ms IS NOT NULL AND lease_until_ms>=0)
    OR (kind NOT IN ('fork-transfer','image-delivery') AND lease_until_ms IS NULL)),
  PRIMARY KEY(account_id,kind,ref_id)
);

INSERT INTO workspace_blob_refs_v2(account_id,kind,ref_id,session_id,object_key,lease_until_ms,response_id,maintenance_due_ms) SELECT account_id,kind,ref_id,session_id,object_key,lease_until_ms,response_id,maintenance_due_ms FROM workspace_blob_refs;

DROP TABLE workspace_blob_refs;

ALTER TABLE workspace_blob_refs_v2 RENAME TO workspace_blob_refs;

CREATE INDEX workspace_blob_execution_scope ON workspace_blob_refs(account_id,session_id,response_id,execution_id,kind);

CREATE INDEX workspace_blob_file_maintenance ON workspace_blob_refs(kind,maintenance_due_ms);

CREATE INDEX workspace_blob_response_artifacts ON workspace_blob_refs(account_id,response_id,kind);

CREATE INDEX workspace_blob_references ON workspace_blob_refs(object_key);

CREATE INDEX workspace_blob_transfer_due ON workspace_blob_refs(kind,lease_until_ms);

-- This is a physical-publication fence, not Response retrieval authority.
-- The owning DO must prove its canonical tombstone before inserting this mark.
CREATE TABLE response_blob_tombstones (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  response_id TEXT NOT NULL CHECK(length(response_id)>0),
  PRIMARY KEY(account_id,response_id)
);

CREATE TRIGGER immutable_response_blob_tombstone BEFORE UPDATE ON response_blob_tombstones
BEGIN SELECT RAISE(ABORT,'immutable_response_blob_tombstone'); END;

CREATE TRIGGER retained_response_blob_tombstone BEFORE DELETE ON response_blob_tombstones
BEGIN SELECT RAISE(ABORT,'immutable_response_blob_tombstone'); END;

CREATE TRIGGER response_blob_publication_insert BEFORE INSERT ON workspace_blob_refs
WHEN NEW.kind IN ('response','response-artifact','image-delivery') AND EXISTS(SELECT 1 FROM response_blob_tombstones WHERE account_id=NEW.account_id AND response_id=CASE WHEN NEW.kind='response' THEN NEW.ref_id ELSE NEW.response_id END)
BEGIN SELECT RAISE(ABORT,'response_blob_deleted'); END;

CREATE TRIGGER response_blob_publication_update BEFORE UPDATE ON workspace_blob_refs
WHEN NEW.kind IN ('response','response-artifact','image-delivery') AND EXISTS(SELECT 1 FROM response_blob_tombstones WHERE account_id=NEW.account_id AND response_id=CASE WHEN NEW.kind='response' THEN NEW.ref_id ELSE NEW.response_id END)
BEGIN SELECT RAISE(ABORT,'response_blob_deleted'); END;

CREATE TABLE conversation_blob_tombstones (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  conversation_id TEXT NOT NULL CHECK(length(conversation_id)>0),
  PRIMARY KEY(account_id,conversation_id)
);

CREATE TRIGGER immutable_conversation_blob_tombstone BEFORE UPDATE ON conversation_blob_tombstones
BEGIN SELECT RAISE(ABORT,'immutable_conversation_blob_tombstone'); END;

CREATE TRIGGER retained_conversation_blob_tombstone BEFORE DELETE ON conversation_blob_tombstones
BEGIN SELECT RAISE(ABORT,'immutable_conversation_blob_tombstone'); END;

CREATE TABLE conversation_image_tombstones (
  account_id TEXT NOT NULL REFERENCES accounts(id),
  conversation_id TEXT NOT NULL CHECK(length(conversation_id)>0),
  item_id TEXT NOT NULL CHECK(length(item_id)>0),
  PRIMARY KEY(account_id,conversation_id,item_id)
);

CREATE TRIGGER immutable_conversation_image_tombstone BEFORE UPDATE ON conversation_image_tombstones
BEGIN SELECT RAISE(ABORT,'immutable_conversation_image_tombstone'); END;

CREATE TRIGGER retained_conversation_image_tombstone BEFORE DELETE ON conversation_image_tombstones
BEGIN SELECT RAISE(ABORT,'immutable_conversation_image_tombstone'); END;

CREATE TRIGGER conversation_image_publication_insert BEFORE INSERT ON workspace_blob_refs
WHEN NEW.kind='conversation-artifact' AND EXISTS(SELECT 1 FROM conversation_image_tombstones WHERE account_id=NEW.account_id AND conversation_id=substr(NEW.ref_id,1,instr(NEW.ref_id,':')-1) AND item_id=NEW.artifact_id)
BEGIN SELECT RAISE(ABORT,'conversation_image_deleted'); END;

CREATE TRIGGER conversation_image_publication_update BEFORE UPDATE ON workspace_blob_refs
WHEN NEW.kind='conversation-artifact' AND EXISTS(SELECT 1 FROM conversation_image_tombstones WHERE account_id=NEW.account_id AND conversation_id=substr(NEW.ref_id,1,instr(NEW.ref_id,':')-1) AND item_id=NEW.artifact_id)
BEGIN SELECT RAISE(ABORT,'conversation_image_deleted'); END;

CREATE TRIGGER conversation_blob_publication_insert BEFORE INSERT ON workspace_blob_refs
WHEN NEW.kind='conversation-current' AND EXISTS(SELECT 1 FROM conversation_blob_tombstones WHERE account_id=NEW.account_id AND conversation_id=substr(NEW.ref_id,1,instr(NEW.ref_id,':')-1))
BEGIN SELECT RAISE(ABORT,'conversation_blob_deleted'); END;

CREATE TRIGGER conversation_blob_publication_update BEFORE UPDATE ON workspace_blob_refs
WHEN NEW.kind='conversation-current' AND EXISTS(SELECT 1 FROM conversation_blob_tombstones WHERE account_id=NEW.account_id AND conversation_id=substr(NEW.ref_id,1,instr(NEW.ref_id,':')-1))
BEGIN SELECT RAISE(ABORT,'conversation_blob_deleted'); END;

CREATE TRIGGER image_execution_live_insert BEFORE INSERT ON workspace_blob_refs
WHEN NEW.kind='image-execution' AND NOT EXISTS(SELECT 1 FROM usage_reservations r WHERE r.id=NEW.response_id AND r.account_id=NEW.account_id AND r.session_id=NEW.session_id AND r.state='reserved' AND r.idempotency_key LIKE 'run:%' AND r.execution_finished_at IS NULL AND COALESCE(r.execution_generation,'legacy')=NEW.execution_generation AND (r.execution_generation IS NULL OR EXISTS(SELECT 1 FROM execution_leases l WHERE l.reservation_id=r.id AND l.account_id=r.account_id AND l.generation=r.execution_generation AND l.state='active')))
BEGIN SELECT RAISE(ABORT,'image_execution_not_active'); END;

CREATE TRIGGER image_execution_live_update BEFORE UPDATE ON workspace_blob_refs
WHEN NEW.kind='image-execution' AND NOT EXISTS(SELECT 1 FROM usage_reservations r WHERE r.id=NEW.response_id AND r.account_id=NEW.account_id AND r.session_id=NEW.session_id AND r.state='reserved' AND r.idempotency_key LIKE 'run:%' AND r.execution_finished_at IS NULL AND COALESCE(r.execution_generation,'legacy')=NEW.execution_generation AND (r.execution_generation IS NULL OR EXISTS(SELECT 1 FROM execution_leases l WHERE l.reservation_id=r.id AND l.account_id=r.account_id AND l.generation=r.execution_generation AND l.state='active')))
BEGIN SELECT RAISE(ABORT,'image_execution_not_active'); END;

CREATE TRIGGER workspace_blob_ref_owner BEFORE INSERT ON workspace_blob_refs
WHEN (NEW.session_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM sessions WHERE id=NEW.session_id AND account_id=NEW.account_id))
  OR NOT EXISTS (SELECT 1 FROM workspace_blobs WHERE object_key=NEW.object_key AND account_id=NEW.account_id AND status='ready')
BEGIN SELECT RAISE(ABORT,'workspace_blob_reference_unavailable'); END;

CREATE TRIGGER workspace_blob_ref_update BEFORE UPDATE ON workspace_blob_refs
WHEN NEW.account_id!=OLD.account_id OR NEW.kind!=OLD.kind OR NEW.ref_id!=OLD.ref_id OR NEW.session_id IS NOT OLD.session_id OR NEW.response_id IS NOT OLD.response_id OR NEW.execution_id IS NOT OLD.execution_id OR NEW.artifact_id IS NOT OLD.artifact_id OR NEW.execution_generation IS NOT OLD.execution_generation
  OR (OLD.kind IN ('response','version','conversation-current','fork-transfer','response-artifact','event-artifact','space-file','image-execution','image-delivery','conversation-artifact') AND NEW.object_key!=OLD.object_key)
  OR NOT EXISTS (SELECT 1 FROM workspace_blobs WHERE object_key=NEW.object_key AND account_id=NEW.account_id AND status='ready')
BEGIN SELECT RAISE(ABORT,'immutable_workspace_blob_reference'); END;

CREATE TRIGGER workspace_blob_ref_delete BEFORE DELETE ON workspace_blob_refs
WHEN OLD.kind IN ('version','event-artifact')
BEGIN SELECT RAISE(ABORT,'immutable_workspace_blob_reference'); END;

PRAGMA legacy_alter_table=OFF;
