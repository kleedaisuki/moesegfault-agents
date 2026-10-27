-- Private runtime recovery capacity is independent of user-managed Space bytes.
CREATE TABLE provider_response_operations (
 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 operation_id TEXT NOT NULL,
 session_id TEXT,
 response_id TEXT,
 max_frames INTEGER NOT NULL CHECK(max_frames BETWEEN 1 AND 4),
 capacity_bytes INTEGER NOT NULL CHECK(capacity_bytes>=0),
 state TEXT NOT NULL CHECK(state IN ('reserved','recovery','diagnostic','deleting','released')),
 expires_at_ms INTEGER,
 updated_at_ms INTEGER NOT NULL,
 PRIMARY KEY(account_id,operation_id),
 CHECK((state IN ('diagnostic','deleting'))=(expires_at_ms IS NOT NULL)),
 CHECK(state!='released' OR capacity_bytes=0)
);
CREATE TABLE provider_response_frames (
 account_id TEXT NOT NULL,
 operation_id TEXT NOT NULL,
 receipt_id TEXT NOT NULL CHECK(length(receipt_id)=64 AND receipt_id NOT GLOB '*[^0-9a-f]*'),
 descriptor_json TEXT NOT NULL CHECK(json_valid(descriptor_json)),
 storage_bytes INTEGER NOT NULL CHECK(storage_bytes>0),
 deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN (0,1)),
 PRIMARY KEY(account_id,operation_id,receipt_id),
 FOREIGN KEY(account_id,operation_id) REFERENCES provider_response_operations(account_id,operation_id)
);
CREATE INDEX provider_response_gc ON provider_response_operations(state,expires_at_ms);
CREATE TRIGGER provider_response_identity BEFORE UPDATE ON provider_response_operations
WHEN NEW.account_id!=OLD.account_id OR NEW.operation_id!=OLD.operation_id OR NEW.session_id IS NOT OLD.session_id OR NEW.response_id IS NOT OLD.response_id OR NEW.max_frames!=OLD.max_frames
 OR (OLD.state='released' AND NEW.state!='released')
 OR (NEW.state='released' AND EXISTS(SELECT 1 FROM provider_response_frames WHERE account_id=OLD.account_id AND operation_id=OLD.operation_id AND deleted=0))
BEGIN SELECT RAISE(ABORT,'provider_receipt_operation_conflict'); END;
CREATE TRIGGER provider_response_frame_insert BEFORE INSERT ON provider_response_frames
WHEN NOT EXISTS(SELECT 1 FROM provider_response_operations o WHERE o.account_id=NEW.account_id AND o.operation_id=NEW.operation_id AND o.state='reserved'
 AND (SELECT COUNT(*) FROM provider_response_frames f WHERE f.account_id=o.account_id AND f.operation_id=o.operation_id)<o.max_frames
 AND (SELECT COALESCE(SUM(storage_bytes),0) FROM provider_response_frames f WHERE f.account_id=o.account_id AND f.operation_id=o.operation_id)+NEW.storage_bytes<=o.capacity_bytes)
BEGIN SELECT RAISE(ABORT,'provider_receipt_capacity_exceeded'); END;
CREATE TRIGGER provider_response_frame_identity BEFORE UPDATE ON provider_response_frames
WHEN NEW.account_id!=OLD.account_id OR NEW.operation_id!=OLD.operation_id OR NEW.receipt_id!=OLD.receipt_id OR NEW.descriptor_json!=OLD.descriptor_json OR NEW.storage_bytes!=OLD.storage_bytes OR NEW.deleted<OLD.deleted
BEGIN SELECT RAISE(ABORT,'immutable_provider_response_frame'); END;
CREATE TRIGGER provider_response_frame_delete BEFORE DELETE ON provider_response_frames
WHEN OLD.deleted=0
BEGIN SELECT RAISE(ABORT,'provider_receipt_body_still_live'); END;
