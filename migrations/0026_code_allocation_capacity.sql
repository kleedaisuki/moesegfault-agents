-- Capacity counts only retained source allocations, never the account's history.
CREATE INDEX code_contexts_live_allocations
  ON code_contexts(account_id,source_session_id)
  WHERE allocation_released_at_ms IS NULL;
