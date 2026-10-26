-- Additive capacity schema after deployed Conversation routing (0015).
-- Independent logical Conversation capacity; not R2 physical storage or money.
CREATE TABLE conversation_capacity_accounts (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  reserved_bytes INTEGER NOT NULL DEFAULT 0 CHECK (reserved_bytes>=0),
  retained_bytes INTEGER NOT NULL DEFAULT 0 CHECK (retained_bytes>=0)
);
CREATE TABLE conversation_capacity_objects (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  conversation_id TEXT NOT NULL,
  object_id TEXT NOT NULL,
  digest TEXT NOT NULL CHECK (length(digest)=64 AND digest NOT GLOB '*[^0-9a-f]*'),
  bytes INTEGER NOT NULL CHECK (bytes>0),
  state TEXT NOT NULL CHECK (state IN ('reserved','retained','released')),
  release_proof TEXT CHECK (release_proof IN ('never_published','content_removed')),
  updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms>=0),
  escrow_response_id TEXT,
  escrow_generation TEXT,
  PRIMARY KEY(account_id,conversation_id,object_id),
  CHECK ((escrow_response_id IS NULL)=(escrow_generation IS NULL)),
  CHECK ((state='released')=(release_proof IS NOT NULL))
);
CREATE INDEX conversation_capacity_state ON conversation_capacity_objects(account_id,state);
CREATE TRIGGER conversation_capacity_object_insert BEFORE INSERT ON conversation_capacity_objects
WHEN NEW.state!='reserved' OR NOT EXISTS(SELECT 1 FROM conversation_capacity_accounts WHERE account_id=NEW.account_id)
BEGIN SELECT RAISE(ABORT,'invalid_conversation_capacity_reservation'); END;
CREATE TRIGGER conversation_capacity_object_identity BEFORE UPDATE ON conversation_capacity_objects
WHEN NEW.account_id!=OLD.account_id OR NEW.conversation_id!=OLD.conversation_id OR NEW.object_id!=OLD.object_id
  OR NEW.digest!=OLD.digest OR NEW.bytes!=OLD.bytes
  OR (OLD.state!='released' AND (NEW.escrow_response_id IS NOT OLD.escrow_response_id OR NEW.escrow_generation IS NOT OLD.escrow_generation))
  OR (OLD.state='retained' AND NEW.state='reserved')
  OR (OLD.state='retained' AND NEW.state='released' AND NEW.release_proof!='content_removed')
BEGIN SELECT RAISE(ABORT,'immutable_conversation_capacity_object'); END;
CREATE TRIGGER conversation_capacity_object_added AFTER INSERT ON conversation_capacity_objects
BEGIN UPDATE conversation_capacity_accounts SET reserved_bytes=reserved_bytes+NEW.bytes WHERE account_id=NEW.account_id; END;
CREATE TRIGGER conversation_capacity_object_changed AFTER UPDATE OF state ON conversation_capacity_objects
WHEN NEW.state!=OLD.state
BEGIN
  UPDATE conversation_capacity_accounts SET
    reserved_bytes=reserved_bytes+(CASE WHEN NEW.state='reserved' THEN NEW.bytes ELSE 0 END)-(CASE WHEN OLD.state='reserved' THEN OLD.bytes ELSE 0 END),
    retained_bytes=retained_bytes+(CASE WHEN NEW.state='retained' THEN NEW.bytes ELSE 0 END)-(CASE WHEN OLD.state='retained' THEN OLD.bytes ELSE 0 END)
  WHERE account_id=NEW.account_id;
END;
CREATE TRIGGER conversation_capacity_object_delete BEFORE DELETE ON conversation_capacity_objects
WHEN OLD.state!='released' AND EXISTS(SELECT 1 FROM accounts WHERE id=OLD.account_id)
BEGIN SELECT RAISE(ABORT,'conversation_capacity_payload_still_live'); END;


-- A Response owns prepaid future logical capacity, not model-money credit.
CREATE TABLE conversation_response_capacity (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  conversation_id TEXT NOT NULL,
  response_id TEXT NOT NULL,
  generation TEXT NOT NULL,
  session_id TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  terms_json TEXT NOT NULL CHECK (json_valid(terms_json)),
  state TEXT NOT NULL CHECK (state IN ('reserved','converted','released')),
  remaining_bytes INTEGER NOT NULL CHECK (remaining_bytes>=0),
  action TEXT NOT NULL DEFAULT 'none' CHECK (action IN ('none','allocate','rollback','release')),
  plan_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(plan_json)),
  updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms>=0),
  PRIMARY KEY(account_id,conversation_id,response_id),
  CHECK (state='reserved' OR remaining_bytes=0)
);
CREATE TRIGGER conversation_response_capacity_insert BEFORE INSERT ON conversation_response_capacity
WHEN NEW.state!='reserved' OR NOT EXISTS(SELECT 1 FROM conversation_capacity_accounts WHERE account_id=NEW.account_id)
BEGIN SELECT RAISE(ABORT,'invalid_conversation_capacity_escrow'); END;
CREATE TRIGGER conversation_response_capacity_identity BEFORE UPDATE ON conversation_response_capacity
WHEN NEW.account_id!=OLD.account_id OR NEW.conversation_id!=OLD.conversation_id OR NEW.response_id!=OLD.response_id
  OR NEW.generation!=OLD.generation OR NEW.session_id!=OLD.session_id OR NEW.fingerprint!=OLD.fingerprint
  OR (OLD.state!='reserved' AND (NEW.state!=OLD.state OR NEW.remaining_bytes!=OLD.remaining_bytes))
BEGIN SELECT RAISE(ABORT,'immutable_conversation_capacity_escrow'); END;
CREATE TRIGGER conversation_response_capacity_added AFTER INSERT ON conversation_response_capacity
BEGIN UPDATE conversation_capacity_accounts SET reserved_bytes=reserved_bytes+NEW.remaining_bytes WHERE account_id=NEW.account_id; END;
CREATE TRIGGER conversation_response_capacity_changed AFTER UPDATE OF remaining_bytes,state ON conversation_response_capacity
BEGIN
  UPDATE conversation_capacity_accounts SET reserved_bytes=reserved_bytes+
    (CASE WHEN NEW.state='reserved' THEN NEW.remaining_bytes ELSE 0 END)-
    (CASE WHEN OLD.state='reserved' THEN OLD.remaining_bytes ELSE 0 END)
  WHERE account_id=NEW.account_id;
END;
-- These object transitions and the escrow debit/credit are ONE SQLite statement.
CREATE TRIGGER conversation_response_capacity_allocate BEFORE UPDATE OF plan_json ON conversation_response_capacity
WHEN OLD.state='reserved' AND NEW.state='reserved' AND NEW.action='allocate'
BEGIN
  SELECT CASE WHEN EXISTS(SELECT 1 FROM json_each(NEW.plan_json) WHERE json_extract(value,'$.conversation_id')!=NEW.conversation_id)
    THEN RAISE(ABORT,'conversation_capacity_escrow_owner') END;
  INSERT INTO conversation_capacity_objects(account_id,conversation_id,object_id,digest,bytes,state,updated_at_ms,escrow_response_id,escrow_generation)
  SELECT NEW.account_id,NEW.conversation_id,json_extract(value,'$.object_id'),json_extract(value,'$.digest'),CAST(json_extract(value,'$.bytes') AS INTEGER),'reserved',NEW.updated_at_ms,NEW.response_id,NEW.generation
  FROM json_each(NEW.plan_json) WHERE 1
  ON CONFLICT(account_id,conversation_id,object_id) DO UPDATE SET state='reserved',release_proof=NULL,updated_at_ms=excluded.updated_at_ms,
    escrow_response_id=excluded.escrow_response_id,escrow_generation=excluded.escrow_generation WHERE conversation_capacity_objects.state='released';
END;
CREATE TRIGGER conversation_response_capacity_rollback BEFORE UPDATE OF plan_json ON conversation_response_capacity
WHEN OLD.state='reserved' AND NEW.state='reserved' AND NEW.action='rollback'
BEGIN
  UPDATE conversation_capacity_objects SET state='released',release_proof='never_published',updated_at_ms=NEW.updated_at_ms
  WHERE account_id=NEW.account_id AND conversation_id=NEW.conversation_id AND state='reserved'
    AND escrow_response_id=NEW.response_id AND escrow_generation=NEW.generation
    AND EXISTS(SELECT 1 FROM json_each(NEW.plan_json) j WHERE object_id=json_extract(j.value,'$.object_id') AND digest=json_extract(j.value,'$.digest') AND bytes=CAST(json_extract(j.value,'$.bytes') AS INTEGER));
END;
CREATE TRIGGER conversation_response_capacity_release BEFORE UPDATE OF state ON conversation_response_capacity
WHEN OLD.state='reserved' AND NEW.state='released' AND NEW.action='release'
BEGIN
  UPDATE conversation_capacity_objects SET state='released',release_proof='never_published',updated_at_ms=NEW.updated_at_ms
  WHERE account_id=NEW.account_id AND conversation_id=NEW.conversation_id AND state='reserved'
    AND escrow_response_id=NEW.response_id AND escrow_generation=NEW.generation;
END;
