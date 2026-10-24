-- Routing and retry identity only. The Session DO owns Response contents/state.
-- session_id deliberately has no FK: reserve identity before session admission.
CREATE TABLE response_routes (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  idempotency_hash TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE (account_id, idempotency_hash)
);
CREATE INDEX response_routes_owner_session ON response_routes(account_id, session_id, created_at, id);
