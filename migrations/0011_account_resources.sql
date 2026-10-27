-- Account-owned files share the existing metering journal without a synthetic session.
-- D1 keeps foreign keys enabled during migrations; replace children before parents.
-- https://developers.cloudflare.com/d1/sql-api/foreign-keys/
PRAGMA defer_foreign_keys = ON;

CREATE TABLE resource_observations_next (
  run_id TEXT NOT NULL REFERENCES usage_reservations(id),
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT REFERENCES sessions(id),
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

CREATE TABLE resource_storage_objects_next (
  object_key TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT REFERENCES sessions(id),
  size_bytes TEXT NOT NULL CHECK (length(size_bytes) BETWEEN 1 AND 20
    AND size_bytes NOT GLOB '*[^0-9]*'
    AND (length(size_bytes)<20 OR size_bytes<='18446744073709551615')),
  retained_from_ms INTEGER NOT NULL CHECK (retained_from_ms>=0),
  billed_through_ms INTEGER NOT NULL CHECK (billed_through_ms>=retained_from_ms),
  removed_at_ms INTEGER CHECK (removed_at_ms IS NULL OR removed_at_ms>=retained_from_ms),
  status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','removed')),
  CHECK ((status='active' AND removed_at_ms IS NULL) OR (status='removed' AND removed_at_ms IS NOT NULL))
);

-- This journal precedes the reservation. Its reservation_id is intentionally not
-- a foreign key: insufficient credit must leave a recoverable pending charge.
CREATE TABLE resource_storage_deficits_next (
  object_key TEXT NOT NULL REFERENCES resource_storage_objects_next(object_key),
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT REFERENCES sessions(id),
  interval_start_ms INTEGER NOT NULL CHECK (interval_start_ms>=0),
  interval_end_ms INTEGER NOT NULL CHECK (interval_end_ms>interval_start_ms),
  quantity TEXT NOT NULL CHECK (length(quantity) BETWEEN 1 AND 20
    AND quantity NOT GLOB '*[^0-9]*'
    AND (length(quantity)<20 OR quantity<='18446744073709551615')),
  amount_microusd TEXT NOT NULL CHECK (length(amount_microusd) BETWEEN 1 AND 19
    AND amount_microusd NOT GLOB '*[^0-9]*'
    AND (length(amount_microusd)<19 OR amount_microusd<='9223372036854775807')),
  price_version TEXT NOT NULL,
  tariff_json TEXT NOT NULL CHECK (json_valid(tariff_json)),
  reservation_id TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','settled','write_off')),
  PRIMARY KEY (object_key,interval_start_ms,interval_end_ms)
);

INSERT INTO resource_observations_next SELECT * FROM resource_observations;
INSERT INTO resource_storage_objects_next SELECT * FROM resource_storage_objects;
INSERT INTO resource_storage_deficits_next SELECT * FROM resource_storage_deficits;

DROP TRIGGER require_observed_resources;
DROP TABLE resource_observations;
DROP TABLE resource_storage_deficits;
DROP TABLE resource_storage_objects;
ALTER TABLE resource_observations_next RENAME TO resource_observations;
ALTER TABLE resource_storage_objects_next RENAME TO resource_storage_objects;
ALTER TABLE resource_storage_deficits_next RENAME TO resource_storage_deficits;

CREATE INDEX resource_observations_pending ON resource_observations(status,run_id);

CREATE TRIGGER observation_owner BEFORE INSERT ON resource_observations
BEGIN
  SELECT CASE WHEN NOT EXISTS (
    SELECT 1 FROM usage_reservations r 
    WHERE r.id=NEW.run_id AND r.account_id=NEW.account_id
      AND r.session_id IS NEW.session_id
      AND (NEW.session_id IS NULL OR EXISTS(SELECT 1 FROM sessions s WHERE s.id=NEW.session_id AND s.account_id=NEW.account_id)) AND r.state='reserved'
  ) THEN RAISE(ABORT,'resource_observation_owner_mismatch') END;
END;
CREATE TRIGGER immutable_observation BEFORE UPDATE ON resource_observations
WHEN NEW.run_id!=OLD.run_id OR NEW.account_id!=OLD.account_id OR NEW.session_id IS NOT OLD.session_id
  OR NEW.event_key!=OLD.event_key OR NEW.resource!=OLD.resource OR NEW.unit!=OLD.unit
  OR NEW.price_version!=OLD.price_version
  OR (OLD.status='observed' AND (NEW.status!=OLD.status OR NEW.quantity IS NOT OLD.quantity OR NEW.observed_at IS NOT OLD.observed_at))
BEGIN SELECT RAISE(ABORT,'immutable_resource_observation'); END;
CREATE TRIGGER immutable_observation_delete BEFORE DELETE ON resource_observations
BEGIN SELECT RAISE(ABORT,'immutable_resource_observation'); END;
CREATE TRIGGER require_observed_resources BEFORE UPDATE OF state ON usage_reservations
WHEN EXISTS (SELECT 1 FROM resource_observations WHERE run_id=OLD.id AND status='pending')
BEGIN SELECT RAISE(ABORT,'resource_observation_pending'); END;

CREATE INDEX storage_objects_account ON resource_storage_objects(account_id,status);
CREATE INDEX storage_objects_cursor ON resource_storage_objects(status,object_key);
CREATE TRIGGER storage_object_owner BEFORE INSERT ON resource_storage_objects
WHEN NEW.session_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM sessions WHERE id=NEW.session_id AND account_id=NEW.account_id)
BEGIN SELECT RAISE(ABORT,'storage_object_owner_mismatch'); END;


CREATE UNIQUE INDEX storage_one_pending_interval ON resource_storage_deficits(object_key) WHERE status='pending';
CREATE INDEX storage_deficit_accounts ON resource_storage_deficits(account_id,status);
CREATE TRIGGER storage_deficit_owner BEFORE INSERT ON resource_storage_deficits
WHEN NOT EXISTS (SELECT 1 FROM resource_storage_objects
  WHERE object_key=NEW.object_key AND account_id=NEW.account_id AND session_id IS NEW.session_id)
BEGIN SELECT RAISE(ABORT,'storage_deficit_owner_mismatch'); END;
CREATE TRIGGER immutable_storage_deficit BEFORE UPDATE ON resource_storage_deficits
WHEN NEW.object_key!=OLD.object_key OR NEW.account_id!=OLD.account_id OR NEW.session_id IS NOT OLD.session_id
  OR NEW.interval_start_ms!=OLD.interval_start_ms OR NEW.interval_end_ms!=OLD.interval_end_ms
  OR NEW.quantity!=OLD.quantity OR NEW.amount_microusd!=OLD.amount_microusd
  OR NEW.price_version!=OLD.price_version OR NEW.tariff_json!=OLD.tariff_json
  OR NEW.reservation_id!=OLD.reservation_id
  OR (OLD.status!='pending' AND NEW.status!=OLD.status)
BEGIN SELECT RAISE(ABORT,'immutable_storage_charge'); END;
CREATE TRIGGER immutable_storage_deficit_delete BEFORE DELETE ON resource_storage_deficits
BEGIN SELECT RAISE(ABORT,'immutable_storage_charge'); END;


PRAGMA defer_foreign_keys = OFF;
