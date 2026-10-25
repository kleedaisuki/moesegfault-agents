-- A Session's physical application is immutable. Old payloads are basic1g.
ALTER TABLE sessions ADD COLUMN runtime_profile TEXT NOT NULL DEFAULT 'basic1g'
  CHECK (runtime_profile IN ('basic1g','standard4g'));

CREATE TRIGGER session_runtime_profile_insert BEFORE INSERT ON sessions
WHEN COALESCE(json_extract(NEW.config_snapshot_json,'$.runtime_profile'),'basic1g') != NEW.runtime_profile
BEGIN SELECT RAISE(ABORT,'session_runtime_profile_mismatch'); END;

CREATE TRIGGER session_runtime_profile_update BEFORE UPDATE ON sessions
WHEN NEW.runtime_profile != OLD.runtime_profile
  OR COALESCE(json_extract(NEW.config_snapshot_json,'$.runtime_profile'),'basic1g') != NEW.runtime_profile
BEGIN SELECT RAISE(ABORT,'immutable_session_runtime_profile'); END;
