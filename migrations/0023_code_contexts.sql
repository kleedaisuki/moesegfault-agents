-- A pending allocation has no public CID. Binding publishes the real daemon CID.
CREATE TABLE code_contexts (
  allocation_id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  source_session_id TEXT NOT NULL,
  source_profile TEXT NOT NULL CHECK(source_profile IN('basic1g','standard4g')),
  funding_run_id TEXT NOT NULL,
  container_id TEXT,
  daemon_boot_id TEXT,
  policy_revision TEXT,
  state TEXT NOT NULL CHECK(state IN('reserved','ready','expired','deleted')),
  created_at_ms INTEGER NOT NULL CHECK(typeof(created_at_ms)='integer' AND created_at_ms>=0),
  last_used_at_ms INTEGER NOT NULL CHECK(typeof(last_used_at_ms)='integer' AND last_used_at_ms>=created_at_ms),
  expires_at_ms INTEGER NOT NULL CHECK(typeof(expires_at_ms)='integer' AND expires_at_ms>=last_used_at_ms),
  ended_at_ms INTEGER,
  allocation_released_at_ms INTEGER CHECK(allocation_released_at_ms IS NULL
    OR(typeof(allocation_released_at_ms)='integer' AND allocation_released_at_ms>=created_at_ms)),
  UNIQUE(account_id,container_id),
  CHECK((container_id IS NULL AND daemon_boot_id IS NULL AND policy_revision IS NULL)
    OR(container_id IS NOT NULL AND daemon_boot_id IS NOT NULL AND policy_revision IS NOT NULL)),
  CHECK(state!='ready' OR container_id IS NOT NULL),
  CHECK(state!='reserved' OR container_id IS NULL),
  CHECK(allocation_released_at_ms IS NULL OR state IN('expired','deleted')),
  CHECK((state IN('reserved','ready') AND ended_at_ms IS NULL)
    OR(state IN('expired','deleted') AND typeof(ended_at_ms)='integer' AND ended_at_ms>=created_at_ms))
);
CREATE UNIQUE INDEX code_contexts_live_source
  ON code_contexts(account_id,source_session_id) WHERE state IN('reserved','ready');
CREATE INDEX code_contexts_owner_state ON code_contexts(account_id,state,allocation_id);
CREATE INDEX code_contexts_due ON code_contexts(state,expires_at_ms,allocation_id);

CREATE TRIGGER code_context_allocation_identity BEFORE INSERT ON code_contexts
WHEN NOT EXISTS(SELECT 1 FROM sessions WHERE id=NEW.source_session_id AND account_id=NEW.account_id)
BEGIN SELECT RAISE(ABORT,'code_context_owner_mismatch'); END;

CREATE TRIGGER code_context_immutable_identity BEFORE UPDATE ON code_contexts
WHEN NEW.allocation_id!=OLD.allocation_id OR NEW.account_id!=OLD.account_id
  OR NEW.source_session_id!=OLD.source_session_id OR NEW.source_profile!=OLD.source_profile
  OR NEW.funding_run_id!=OLD.funding_run_id OR NEW.created_at_ms!=OLD.created_at_ms
  OR(OLD.allocation_released_at_ms IS NOT NULL
    AND NEW.allocation_released_at_ms IS NOT OLD.allocation_released_at_ms)
  OR(OLD.container_id IS NOT NULL AND(NEW.container_id IS NOT OLD.container_id
    OR NEW.daemon_boot_id IS NOT OLD.daemon_boot_id OR NEW.policy_revision IS NOT OLD.policy_revision))
BEGIN SELECT RAISE(ABORT,'code_context_identity_changed'); END;

CREATE TRIGGER code_context_terminal BEFORE UPDATE ON code_contexts
WHEN OLD.state IN('expired','deleted') AND((NEW.state!=OLD.state
    AND NOT(OLD.state='expired' AND NEW.state='deleted'))
  OR NEW.container_id IS NOT OLD.container_id OR NEW.last_used_at_ms!=OLD.last_used_at_ms
  OR NEW.expires_at_ms!=OLD.expires_at_ms OR NEW.ended_at_ms IS NOT OLD.ended_at_ms)
BEGIN SELECT RAISE(ABORT,'code_context_expired'); END;

CREATE TRIGGER code_context_monotonic_clock BEFORE UPDATE ON code_contexts
WHEN NEW.last_used_at_ms<OLD.last_used_at_ms OR NEW.expires_at_ms<OLD.expires_at_ms
BEGIN SELECT RAISE(ABORT,'code_context_clock_reversed'); END;
