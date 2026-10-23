CREATE TABLE resource_storage_objects (
  object_key TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT NOT NULL REFERENCES sessions(id),
  size_bytes TEXT NOT NULL CHECK (length(size_bytes) BETWEEN 1 AND 20
    AND size_bytes NOT GLOB '*[^0-9]*'
    AND (length(size_bytes)<20 OR size_bytes<='18446744073709551615')),
  retained_from_ms INTEGER NOT NULL CHECK (retained_from_ms>=0),
  billed_through_ms INTEGER NOT NULL CHECK (billed_through_ms>=retained_from_ms),
  removed_at_ms INTEGER CHECK (removed_at_ms IS NULL OR removed_at_ms>=retained_from_ms),
  status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','removed')),
  CHECK ((status='active' AND removed_at_ms IS NULL) OR (status='removed' AND removed_at_ms IS NOT NULL))
);
CREATE INDEX storage_objects_account ON resource_storage_objects(account_id,status);
CREATE INDEX storage_objects_cursor ON resource_storage_objects(status,object_key);
CREATE TRIGGER storage_object_owner BEFORE INSERT ON resource_storage_objects
WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE id=NEW.session_id AND account_id=NEW.account_id)
BEGIN SELECT RAISE(ABORT,'storage_object_owner_mismatch'); END;

-- This journal precedes the reservation. Its reservation_id is intentionally not
-- a foreign key: insufficient credit must leave a recoverable pending charge.
CREATE TABLE resource_storage_deficits (
  object_key TEXT NOT NULL REFERENCES resource_storage_objects(object_key),
  account_id TEXT NOT NULL REFERENCES accounts(id),
  session_id TEXT NOT NULL REFERENCES sessions(id),
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
CREATE UNIQUE INDEX storage_one_pending_interval ON resource_storage_deficits(object_key) WHERE status='pending';
CREATE INDEX storage_deficit_accounts ON resource_storage_deficits(account_id,status);
CREATE TRIGGER storage_deficit_owner BEFORE INSERT ON resource_storage_deficits
WHEN NOT EXISTS (SELECT 1 FROM resource_storage_objects
  WHERE object_key=NEW.object_key AND account_id=NEW.account_id AND session_id=NEW.session_id)
BEGIN SELECT RAISE(ABORT,'storage_deficit_owner_mismatch'); END;
CREATE TRIGGER immutable_storage_deficit BEFORE UPDATE ON resource_storage_deficits
WHEN NEW.object_key!=OLD.object_key OR NEW.account_id!=OLD.account_id OR NEW.session_id!=OLD.session_id
  OR NEW.interval_start_ms!=OLD.interval_start_ms OR NEW.interval_end_ms!=OLD.interval_end_ms
  OR NEW.quantity!=OLD.quantity OR NEW.amount_microusd!=OLD.amount_microusd
  OR NEW.price_version!=OLD.price_version OR NEW.tariff_json!=OLD.tariff_json
  OR NEW.reservation_id!=OLD.reservation_id
  OR (OLD.status!='pending' AND NEW.status!=OLD.status)
BEGIN SELECT RAISE(ABORT,'immutable_storage_charge'); END;
CREATE TRIGGER immutable_storage_deficit_delete BEFORE DELETE ON resource_storage_deficits
BEGIN SELECT RAISE(ABORT,'immutable_storage_charge'); END;

CREATE TABLE resource_system_state (key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE resource_account_blocks (
  account_id TEXT PRIMARY KEY REFERENCES accounts(id),
  reason TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
