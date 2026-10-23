-- Execution admission and uncertain financial reconciliation have independent lifetimes.
ALTER TABLE usage_reservations ADD COLUMN execution_finished_at INTEGER
  CHECK (execution_finished_at IS NULL OR (typeof(execution_finished_at) = 'integer' AND execution_finished_at >= 0));

CREATE INDEX usage_reservations_executing
  ON usage_reservations(account_id, state, execution_finished_at, idempotency_key);

-- Only definitive non-running session snapshots can release legacy concurrency leases.
UPDATE usage_reservations SET execution_finished_at = CAST(updated_at AS INTEGER) * 1000
WHERE idempotency_key LIKE 'run:%'
  AND (state != 'reserved' OR EXISTS (
    SELECT 1 FROM sessions s WHERE s.id = usage_reservations.session_id
      AND s.account_id = usage_reservations.account_id
      AND json_extract(s.config_snapshot_json, '$.status') IN ('idle', 'closed')
  ));

CREATE TRIGGER execution_finished_immutable BEFORE UPDATE OF execution_finished_at ON usage_reservations
WHEN OLD.execution_finished_at IS NOT NULL AND NEW.execution_finished_at IS NOT OLD.execution_finished_at
BEGIN SELECT RAISE(ABORT, 'execution_already_finished'); END;
