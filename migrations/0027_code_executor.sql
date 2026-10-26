-- The actor route is not a claim about an externally owned executor's memory.
ALTER TABLE code_contexts ADD COLUMN executor_kind TEXT NOT NULL DEFAULT 'platform'
  CHECK(executor_kind IN('platform','workspace_daemon'));
ALTER TABLE code_contexts ADD COLUMN memory_limit TEXT
  CHECK(memory_limit IS NULL OR memory_limit IN('1g','4g','16g','64g'));
ALTER TABLE code_contexts ADD COLUMN executor_lease_key TEXT;
CREATE UNIQUE INDEX code_contexts_executor_lease
  ON code_contexts(executor_lease_key)
  WHERE executor_lease_key IS NOT NULL AND allocation_released_at_ms IS NULL;
ALTER TABLE code_contexts ADD COLUMN cleanup_run_id TEXT REFERENCES usage_reservations(id);
UPDATE code_contexts SET memory_limit=CASE source_profile WHEN 'basic1g' THEN '1g' ELSE '4g' END;
CREATE TRIGGER code_context_executor_identity BEFORE UPDATE ON code_contexts
WHEN NEW.executor_kind!=OLD.executor_kind OR NEW.memory_limit IS NOT OLD.memory_limit
  OR NEW.cleanup_run_id IS NOT OLD.cleanup_run_id
  OR NEW.executor_lease_key IS NOT OLD.executor_lease_key
BEGIN SELECT RAISE(ABORT,'code_context_executor_changed'); END;
