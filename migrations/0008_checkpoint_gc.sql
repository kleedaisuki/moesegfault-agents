-- Write-ahead discovery of immutable checkpoint objects. This is maintenance
-- state, not a financial ledger or a source of truth for the current snapshot.
CREATE TABLE workspace_checkpoint_intents (
  object_key TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT NOT NULL REFERENCES sessions(id),
  manifest_key TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL CHECK (created_at_ms>=0),
  next_check_ms INTEGER NOT NULL CHECK (next_check_ms>=created_at_ms)
);
CREATE INDEX checkpoint_gc_due ON workspace_checkpoint_intents(next_check_ms,object_key);
CREATE TRIGGER checkpoint_intent_owner BEFORE INSERT ON workspace_checkpoint_intents
WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE id=NEW.session_id AND account_id=NEW.account_id)
BEGIN SELECT RAISE(ABORT,'checkpoint_intent_owner_mismatch'); END;
CREATE TRIGGER immutable_checkpoint_intent BEFORE UPDATE ON workspace_checkpoint_intents
WHEN NEW.object_key!=OLD.object_key OR NEW.account_id!=OLD.account_id
  OR NEW.session_id!=OLD.session_id OR NEW.manifest_key!=OLD.manifest_key
  OR NEW.created_at_ms!=OLD.created_at_ms OR NEW.next_check_ms<OLD.next_check_ms
BEGIN SELECT RAISE(ABORT,'immutable_checkpoint_intent'); END;
