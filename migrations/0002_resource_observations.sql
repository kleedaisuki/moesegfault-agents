CREATE TABLE resource_observations (
  run_id TEXT NOT NULL REFERENCES usage_reservations(id),
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT NOT NULL REFERENCES sessions(id),
  event_key TEXT NOT NULL CHECK (length(event_key) BETWEEN 1 AND 256),
  resource TEXT NOT NULL CHECK (resource IN ('container','r2_storage','r2_operation','d1','worker','durable_object')),
  unit TEXT NOT NULL CHECK (length(unit) BETWEEN 1 AND 64),
  quantity TEXT CHECK (quantity IS NULL OR (
    length(quantity) BETWEEN 1 AND 20 AND quantity NOT GLOB '*[^0-9]*'
    AND (length(quantity)<20 OR quantity<='18446744073709551615')
  )),
  price_version TEXT NOT NULL CHECK (length(price_version) BETWEEN 1 AND 128),
  observed_at INTEGER,
  status TEXT NOT NULL CHECK (status IN ('pending','observed')),
  PRIMARY KEY (run_id,event_key),
  CHECK ((status='pending' AND quantity IS NULL AND observed_at IS NULL)
    OR (status='observed' AND quantity IS NOT NULL AND observed_at IS NOT NULL))
);
CREATE INDEX resource_observations_pending ON resource_observations(status,run_id);

CREATE TRIGGER observation_owner BEFORE INSERT ON resource_observations
BEGIN
  SELECT CASE WHEN NOT EXISTS (
    SELECT 1 FROM usage_reservations r JOIN sessions s ON s.id=NEW.session_id
    WHERE r.id=NEW.run_id AND r.account_id=NEW.account_id
      AND r.session_id=NEW.session_id AND s.account_id=NEW.account_id AND r.state='reserved'
  ) THEN RAISE(ABORT,'resource_observation_owner_mismatch') END;
END;
CREATE TRIGGER immutable_observation BEFORE UPDATE ON resource_observations
WHEN NEW.run_id!=OLD.run_id OR NEW.account_id!=OLD.account_id OR NEW.session_id!=OLD.session_id
  OR NEW.event_key!=OLD.event_key OR NEW.resource!=OLD.resource OR NEW.unit!=OLD.unit
  OR NEW.price_version!=OLD.price_version
  OR (OLD.status='observed' AND (NEW.status!=OLD.status OR NEW.quantity IS NOT OLD.quantity OR NEW.observed_at IS NOT OLD.observed_at))
BEGIN SELECT RAISE(ABORT,'immutable_resource_observation'); END;
CREATE TRIGGER immutable_observation_delete BEFORE DELETE ON resource_observations
BEGIN SELECT RAISE(ABORT,'immutable_resource_observation'); END;
CREATE TRIGGER require_observed_resources BEFORE UPDATE OF state ON usage_reservations
WHEN EXISTS (SELECT 1 FROM resource_observations WHERE run_id=OLD.id AND status='pending')
BEGIN SELECT RAISE(ABORT,'resource_observation_pending'); END;
