PRAGMA foreign_keys = ON;

CREATE TABLE accounts (
  id TEXT PRIMARY KEY,
  issuer TEXT NOT NULL,
  subject TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (issuer, subject)
);
CREATE TABLE browser_sessions (
  token_hash TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  tokens_encrypted TEXT NOT NULL,
  csrf_hash TEXT NOT NULL,
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  refresh_version INTEGER NOT NULL DEFAULT 0,
  refresh_started_at INTEGER
);
CREATE INDEX browser_sessions_expiry ON browser_sessions(expires_at);
CREATE TABLE oidc_transactions (
  state_hash TEXT PRIMARY KEY,
  payload_encrypted TEXT NOT NULL,
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX oidc_transactions_expiry ON oidc_transactions(expires_at);
CREATE TABLE api_keys (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  key_hash TEXT NOT NULL UNIQUE,
  key_prefix TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at INTEGER
);
CREATE INDEX api_keys_account ON api_keys(account_id);
CREATE TABLE provider_keys (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  provider TEXT NOT NULL CHECK (provider IN ('openrouter', 'deepseek')),
  key_encrypted TEXT NOT NULL,
  key_version INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (account_id, provider)
);
CREATE TABLE agents (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  config_json TEXT NOT NULL CHECK (json_valid(config_json)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
);
CREATE INDEX agents_account ON agents(account_id, deleted_at);
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  agent_id TEXT NOT NULL REFERENCES agents(id),
  do_id TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL CHECK (status IN ('active', 'idle', 'closed')),
  config_snapshot_json TEXT NOT NULL CHECK (json_valid(config_snapshot_json)),
  instructions_hash TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_input_at INTEGER NOT NULL,
  closed_at INTEGER,
  conversation_r2_key TEXT,
  workspace_r2_key TEXT
);
CREATE INDEX sessions_account_status ON sessions(account_id, status);
CREATE INDEX sessions_agent ON sessions(agent_id, created_at);
CREATE TABLE subscription_snapshots (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  product_id TEXT NOT NULL,
  subscription_json TEXT NOT NULL CHECK (json_valid(subscription_json)),
  fetched_at INTEGER NOT NULL,
  valid_until INTEGER NOT NULL,
  PRIMARY KEY (account_id, product_id)
);
CREATE TABLE usage_accounts (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  budget_microusd INTEGER NOT NULL DEFAULT 0 CHECK (typeof(budget_microusd) = 'integer' AND budget_microusd >= 0),
  spent_microusd INTEGER NOT NULL DEFAULT 0 CHECK (typeof(spent_microusd) = 'integer' AND spent_microusd >= 0),
  reserved_microusd INTEGER NOT NULL DEFAULT 0 CHECK (typeof(reserved_microusd) = 'integer' AND reserved_microusd >= 0),
  updated_at INTEGER NOT NULL,
  CHECK (spent_microusd <= budget_microusd - reserved_microusd)
);
CREATE TABLE usage_reservations (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES usage_accounts(account_id),
  idempotency_key TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  session_id TEXT REFERENCES sessions(id),
  reserved_microusd INTEGER NOT NULL CHECK (typeof(reserved_microusd) = 'integer' AND reserved_microusd >= 0),
  settled_microusd INTEGER CHECK (settled_microusd IS NULL OR (typeof(settled_microusd) = 'integer' AND settled_microusd >= 0 AND settled_microusd <= reserved_microusd)),
  state TEXT NOT NULL CHECK (state IN ('reserved', 'settled', 'released')),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (account_id, idempotency_key)
);
CREATE INDEX usage_reservations_recovery ON usage_reservations(state, updated_at);
CREATE TABLE usage_ledger (
  id TEXT PRIMARY KEY,
  reservation_id TEXT NOT NULL REFERENCES usage_reservations(id),
  event_key TEXT NOT NULL,
  resource TEXT NOT NULL CHECK (resource IN ('model', 'container', 'r2_storage', 'r2_operation', 'd1', 'worker', 'durable_object')),
  quantity INTEGER NOT NULL CHECK (quantity >= 0),
  unit TEXT NOT NULL,
  amount_microusd INTEGER NOT NULL CHECK (typeof(amount_microusd) = 'integer' AND amount_microusd >= 0),
  price_version TEXT NOT NULL,
  provider_request_id TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE (reservation_id, event_key)
);
CREATE TABLE diagnostics_consents (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
  updated_at INTEGER NOT NULL
);
CREATE TABLE diagnostic_reports (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  payload_json TEXT NOT NULL CHECK (json_valid(payload_json) AND length(payload_json) <= 65536),
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE INDEX diagnostic_reports_expiry ON diagnostic_reports(expires_at);

CREATE TRIGGER reserve_budget AFTER INSERT ON usage_reservations
BEGIN
  SELECT CASE WHEN NEW.state != 'reserved' OR NEW.settled_microusd IS NOT NULL
    THEN RAISE(ABORT, 'invalid_reservation') END;
  SELECT CASE WHEN NEW.reserved_microusd > (SELECT budget_microusd - spent_microusd - reserved_microusd
    FROM usage_accounts WHERE account_id = NEW.account_id)
    THEN RAISE(ABORT, 'budget_exceeded') END;
  UPDATE usage_accounts SET reserved_microusd = reserved_microusd + NEW.reserved_microusd,
    updated_at = NEW.updated_at WHERE account_id = NEW.account_id;
END;
CREATE TRIGGER immutable_reservation BEFORE UPDATE ON usage_reservations
WHEN NEW.id != OLD.id OR NEW.account_id != OLD.account_id
  OR NEW.request_hash != OLD.request_hash OR NEW.idempotency_key != OLD.idempotency_key
  OR NEW.reserved_microusd != OLD.reserved_microusd OR NEW.created_at != OLD.created_at
  OR NEW.session_id IS NOT OLD.session_id
  OR (NEW.settled_microusd IS NOT OLD.settled_microusd AND NEW.state = OLD.state)
BEGIN SELECT RAISE(ABORT, 'immutable_reservation'); END;
CREATE TRIGGER finalize_reservation BEFORE UPDATE OF state ON usage_reservations
BEGIN
  SELECT CASE WHEN OLD.state != 'reserved' OR NEW.state NOT IN ('settled', 'released')
    THEN RAISE(ABORT, 'reservation_finalized') END;
  SELECT CASE WHEN NEW.state = 'released' AND NEW.settled_microusd IS NOT NULL
    THEN RAISE(ABORT, 'invalid_release') END;
  SELECT CASE WHEN NEW.state = 'released' AND EXISTS (
    SELECT 1 FROM usage_ledger WHERE reservation_id = OLD.id)
    THEN RAISE(ABORT, 'recorded_usage_requires_settlement') END;
  SELECT CASE WHEN NEW.state = 'settled' AND (NEW.settled_microusd IS NULL
    OR NEW.settled_microusd != (SELECT COALESCE(SUM(amount_microusd), 0)
      FROM usage_ledger WHERE reservation_id = OLD.id))
    THEN RAISE(ABORT, 'settlement_mismatch') END;
  UPDATE usage_accounts SET reserved_microusd = reserved_microusd - OLD.reserved_microusd,
    spent_microusd = spent_microusd + COALESCE(NEW.settled_microusd, 0),
    updated_at = NEW.updated_at WHERE account_id = OLD.account_id;
END;
CREATE TRIGGER immutable_reservation_delete BEFORE DELETE ON usage_reservations
BEGIN SELECT RAISE(ABORT, 'immutable_reservation'); END;
CREATE TRIGGER append_usage BEFORE INSERT ON usage_ledger
BEGIN
  SELECT CASE WHEN (SELECT state FROM usage_reservations WHERE id = NEW.reservation_id) != 'reserved'
    THEN RAISE(ABORT, 'reservation_finalized') END;
  SELECT CASE WHEN NEW.amount_microusd > (
    SELECT reserved_microusd - COALESCE((SELECT SUM(amount_microusd) FROM usage_ledger
      WHERE reservation_id = NEW.reservation_id), 0)
    FROM usage_reservations WHERE id = NEW.reservation_id)
    THEN RAISE(ABORT, 'budget_exceeded') END;
END;
CREATE TRIGGER immutable_usage_update BEFORE UPDATE ON usage_ledger
BEGIN SELECT RAISE(ABORT, 'immutable_usage'); END;
CREATE TRIGGER immutable_usage_delete BEFORE DELETE ON usage_ledger
BEGIN SELECT RAISE(ABORT, 'immutable_usage'); END;
CREATE INDEX diagnostic_reports_account_time ON diagnostic_reports(account_id, created_at);
