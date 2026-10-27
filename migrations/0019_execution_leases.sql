-- Local execution capacity and uncertain monetary holds have separate lifetimes.
-- Legacy reservations without generation metadata retain conservative admission.
ALTER TABLE usage_reservations ADD COLUMN execution_generation TEXT
  CHECK (execution_generation IS NULL OR
    (typeof(execution_generation) = 'text' AND length(execution_generation) BETWEEN 1 AND 128));

CREATE TABLE execution_leases (
  reservation_id TEXT PRIMARY KEY REFERENCES usage_reservations(id),
  account_id TEXT NOT NULL REFERENCES usage_accounts(account_id),
  generation TEXT NOT NULL CHECK (length(generation) BETWEEN 1 AND 128),
  state TEXT NOT NULL CHECK (state IN ('active', 'no_local_execution')),
  created_at INTEGER NOT NULL,
  released_at_ms INTEGER CHECK (released_at_ms IS NULL OR
    (typeof(released_at_ms) = 'integer' AND released_at_ms >= 0)),
  CHECK ((state = 'active') = (released_at_ms IS NULL))
);
CREATE INDEX execution_leases_account_state ON execution_leases(account_id, state, reservation_id);

CREATE TRIGGER create_execution_lease AFTER INSERT ON usage_reservations
WHEN NEW.execution_generation IS NOT NULL
BEGIN
  SELECT CASE WHEN NEW.session_id IS NOT NULL OR NEW.idempotency_key NOT LIKE 'run:%'
    THEN RAISE(ABORT, 'invalid_execution_lease_scope') END;
  INSERT INTO execution_leases(reservation_id, account_id, generation, state, created_at)
    VALUES (NEW.id, NEW.account_id, NEW.execution_generation, 'active', NEW.created_at);
END;

CREATE TRIGGER execution_generation_immutable BEFORE UPDATE OF execution_generation ON usage_reservations
WHEN NEW.execution_generation IS NOT OLD.execution_generation
BEGIN SELECT RAISE(ABORT, 'immutable_execution_generation'); END;

CREATE TRIGGER execution_lease_identity BEFORE INSERT ON execution_leases
WHEN NOT EXISTS (SELECT 1 FROM usage_reservations r WHERE r.id=NEW.reservation_id
  AND r.account_id=NEW.account_id AND r.execution_generation=NEW.generation
  AND r.session_id IS NULL AND r.idempotency_key LIKE 'run:%')
BEGIN SELECT RAISE(ABORT, 'invalid_execution_lease_identity'); END;

CREATE TRIGGER execution_lease_immutable BEFORE UPDATE ON execution_leases
WHEN NEW.reservation_id != OLD.reservation_id OR NEW.account_id != OLD.account_id
  OR NEW.generation != OLD.generation OR NEW.created_at != OLD.created_at
  OR OLD.state != 'active' OR NEW.state != 'no_local_execution'
BEGIN SELECT RAISE(ABORT, 'immutable_execution_lease'); END;
CREATE TRIGGER execution_lease_no_delete BEFORE DELETE ON execution_leases
BEGIN SELECT RAISE(ABORT, 'immutable_execution_lease'); END;
