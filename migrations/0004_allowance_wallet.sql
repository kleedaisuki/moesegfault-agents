-- Allowance windows are financial provenance, not expiring diagnostic state.
CREATE TABLE allowance_windows (
  account_id TEXT NOT NULL REFERENCES usage_accounts(account_id),
  period_start INTEGER NOT NULL,
  period_end INTEGER NOT NULL CHECK (period_end > period_start),
  source TEXT NOT NULL,
  ceiling_microusd INTEGER NOT NULL CHECK (typeof(ceiling_microusd)='integer' AND ceiling_microusd>=0),
  credited_microusd INTEGER NOT NULL CHECK (typeof(credited_microusd)='integer' AND credited_microusd>=0),
  PRIMARY KEY(account_id,period_start)
);
CREATE TABLE allowance_grants (
  account_id TEXT NOT NULL REFERENCES usage_accounts(account_id),
  period_start INTEGER NOT NULL,
  period_end INTEGER NOT NULL CHECK(period_end>period_start),
  source TEXT NOT NULL,
  ceiling_microusd INTEGER NOT NULL CHECK(typeof(ceiling_microusd)='integer' AND ceiling_microusd BETWEEN 0 AND 180000000),
  credited_microusd INTEGER NOT NULL CHECK(typeof(credited_microusd)='integer' AND credited_microusd>=0),
  created_at INTEGER NOT NULL CHECK(created_at>=period_start AND created_at<period_end),
  subsidy_limit_microusd INTEGER CHECK(subsidy_limit_microusd IS NULL OR (typeof(subsidy_limit_microusd)='integer' AND subsidy_limit_microusd>=0)),
  PRIMARY KEY(account_id,period_start,ceiling_microusd)
);
CREATE TABLE reservation_vintages (
  reservation_id TEXT PRIMARY KEY REFERENCES usage_reservations(id),
  period_start INTEGER,
  period_end INTEGER
);
CREATE INDEX allowance_daily_subsidy ON allowance_grants((created_at/86400)) WHERE subsidy_limit_microusd IS NOT NULL;
-- Pre-migration holds are deliberately expired vintages: never refund them into a new grant.
INSERT INTO reservation_vintages SELECT id,NULL,NULL FROM usage_reservations;

CREATE TRIGGER allowance_grant_apply BEFORE INSERT ON allowance_grants
BEGIN
  SELECT CASE WHEN NEW.subsidy_limit_microusd IS NOT NULL AND NEW.credited_microusd > NEW.subsidy_limit_microusd-COALESCE((SELECT SUM(credited_microusd) FROM allowance_grants WHERE subsidy_limit_microusd IS NOT NULL AND created_at/86400=NEW.created_at/86400),0)
    THEN RAISE(ABORT,'trial_capacity_exhausted') END;
  SELECT CASE WHEN NEW.period_start < COALESCE((SELECT MAX(period_start) FROM allowance_windows WHERE account_id=NEW.account_id),NEW.period_start)
    THEN RAISE(ABORT,'stale_allowance_period') END;
  SELECT CASE WHEN EXISTS(SELECT 1 FROM allowance_windows WHERE account_id=NEW.account_id AND period_start=NEW.period_start AND
    (period_end!=NEW.period_end OR source!=NEW.source OR ceiling_microusd>=NEW.ceiling_microusd))
    THEN RAISE(ABORT,'allowance_period_conflict') END;
  -- A new window burns unspent credit, but keeps every outstanding hold funded.
  UPDATE usage_accounts SET budget_microusd=spent_microusd+reserved_microusd,updated_at=NEW.created_at
    WHERE account_id=NEW.account_id AND NOT EXISTS(SELECT 1 FROM allowance_windows WHERE account_id=NEW.account_id AND period_start=NEW.period_start);
  UPDATE usage_accounts SET budget_microusd=budget_microusd+NEW.credited_microusd,updated_at=NEW.created_at WHERE account_id=NEW.account_id;
  INSERT INTO allowance_windows VALUES(NEW.account_id,NEW.period_start,NEW.period_end,NEW.source,NEW.ceiling_microusd,NEW.credited_microusd)
    ON CONFLICT(account_id,period_start) DO UPDATE SET ceiling_microusd=excluded.ceiling_microusd,credited_microusd=credited_microusd+excluded.credited_microusd;
END;
CREATE TRIGGER allowance_grants_no_update BEFORE UPDATE ON allowance_grants
BEGIN SELECT RAISE(ABORT,'immutable_allowance_grant'); END;
CREATE TRIGGER allowance_grants_no_delete BEFORE DELETE ON allowance_grants
BEGIN SELECT RAISE(ABORT,'immutable_allowance_grant'); END;
CREATE TRIGGER reservation_vintage_capture AFTER INSERT ON usage_reservations
BEGIN
  SELECT CASE WHEN EXISTS(SELECT 1 FROM allowance_windows WHERE account_id=NEW.account_id)
    AND (CASE WHEN NEW.created_at>100000000000 THEN NEW.created_at/1000 ELSE NEW.created_at END) >= (SELECT period_end FROM allowance_windows WHERE account_id=NEW.account_id ORDER BY period_start DESC LIMIT 1)
    THEN RAISE(ABORT,'allowance_expired') END;
  INSERT INTO reservation_vintages SELECT NEW.id,period_start,period_end FROM allowance_windows
    WHERE account_id=NEW.account_id ORDER BY period_start DESC LIMIT 1;
  INSERT OR IGNORE INTO reservation_vintages VALUES(NEW.id,NULL,NULL);
END;
CREATE TRIGGER reservation_expired_refund AFTER UPDATE OF state ON usage_reservations
WHEN OLD.state='reserved' AND NEW.state IN('released','settled')
  AND EXISTS(SELECT 1 FROM allowance_windows WHERE account_id=OLD.account_id)
  AND NOT EXISTS(SELECT 1 FROM reservation_vintages v JOIN allowance_windows w ON w.account_id=OLD.account_id AND w.period_start=v.period_start
    WHERE v.reservation_id=OLD.id AND w.period_end>(CASE WHEN NEW.updated_at>100000000000 THEN NEW.updated_at/1000 ELSE NEW.updated_at END)
    AND w.period_start=(SELECT MAX(period_start) FROM allowance_windows WHERE account_id=OLD.account_id))
BEGIN
  UPDATE usage_accounts SET budget_microusd=budget_microusd-(OLD.reserved_microusd-COALESCE(NEW.settled_microusd,0)) WHERE account_id=OLD.account_id;
END;
