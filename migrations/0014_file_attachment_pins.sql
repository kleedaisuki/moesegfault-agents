-- A Response's accepted attachment outlives its HTTP observer. Only its owning
-- Session DO may release this reference after canonical completion/cancellation,
-- or after proving that a pre-acceptance reference has no accepted run.
ALTER TABLE space_files ADD COLUMN deletion_reason TEXT NOT NULL DEFAULT 'user'
  CHECK (deletion_reason IN ('user','upload_abort'));

CREATE TABLE file_attachment_pins (
  response_id TEXT NOT NULL REFERENCES response_routes(id),
  file_id TEXT NOT NULL REFERENCES space_files(id),
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT NOT NULL REFERENCES sessions(id),
  object_key TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  bytes INTEGER NOT NULL CHECK (bytes>=0 AND bytes<=67108864),
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY(response_id,file_id)
);
CREATE INDEX file_attachment_pins_file ON file_attachment_pins(file_id);
CREATE INDEX file_attachment_pins_session ON file_attachment_pins(account_id,session_id,response_id);

CREATE TRIGGER file_attachment_pin_owner BEFORE INSERT ON file_attachment_pins
WHEN NOT EXISTS (
  SELECT 1 FROM space_files f
  JOIN sessions s ON s.id=NEW.session_id AND s.account_id=NEW.account_id
  JOIN response_routes r ON r.id=NEW.response_id AND r.account_id=NEW.account_id AND r.session_id=NEW.session_id
  WHERE f.id=NEW.file_id AND f.account_id=NEW.account_id AND f.status='processed'
    AND f.object_key=NEW.object_key AND f.sha256=NEW.sha256 AND f.bytes=NEW.bytes
)
BEGIN SELECT RAISE(ABORT,'file_not_ready'); END;

CREATE TRIGGER immutable_file_attachment_pin BEFORE UPDATE ON file_attachment_pins
BEGIN SELECT RAISE(ABORT,'immutable_file_attachment_pin'); END;

-- Enforce the interlock for every writer, including an older Worker still
-- finishing an upload/deletion during a rolling release.
CREATE TRIGGER file_attachment_delete_interlock BEFORE UPDATE OF status ON space_files
WHEN NEW.status IN ('deleting','deleted','failed')
  AND EXISTS(SELECT 1 FROM file_attachment_pins WHERE file_id=OLD.id)
BEGIN SELECT RAISE(ABORT,'file_in_use'); END;

CREATE TRIGGER file_deletion_claim_is_terminal BEFORE UPDATE OF status ON space_files
WHEN OLD.status='deleting' AND NEW.status NOT IN ('deleting','deleted','failed')
BEGIN SELECT RAISE(ABORT,'invalid_file_lifecycle'); END;
