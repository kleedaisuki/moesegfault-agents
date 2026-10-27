-- Owner routing only; the ConversationState DO owns metadata, item visibility,
-- revision snapshots and append receipts. Model/session execution is independent.
CREATE TABLE conversation_routes (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  request_hash TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX conversation_routes_owner ON conversation_routes(account_id, created_at, id);

