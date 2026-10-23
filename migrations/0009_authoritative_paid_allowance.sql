-- A Free window's identity is retained so its holds remain traceable. The immutable
-- handoff records the authoritative paid anchor and every changed funding field.
CREATE TABLE allowance_replacements (
  account_id TEXT NOT NULL REFERENCES usage_accounts(account_id),
  window_start INTEGER NOT NULL,
  previous_end INTEGER NOT NULL,
  previous_source TEXT NOT NULL CHECK(previous_source='free'),
  previous_ceiling_microusd INTEGER NOT NULL,
  previous_credited_microusd INTEGER NOT NULL,
  paid_start INTEGER NOT NULL,
  paid_end INTEGER NOT NULL CHECK(paid_end>paid_start),
  paid_source TEXT NOT NULL CHECK(paid_source LIKE 'billing:%'),
  verified_ceiling_microusd INTEGER NOT NULL CHECK(typeof(verified_ceiling_microusd)='integer' AND verified_ceiling_microusd BETWEEN 0 AND 180000000),
  target_microusd INTEGER NOT NULL CHECK(typeof(target_microusd)='integer' AND target_microusd BETWEEN 0 AND 180000000),
  delta_microusd INTEGER NOT NULL CHECK(typeof(delta_microusd)='integer' AND delta_microusd>=0),
  created_at INTEGER NOT NULL CHECK(created_at>=paid_start AND created_at<paid_end),
  PRIMARY KEY(account_id,window_start),
  CHECK(delta_microusd=target_microusd-previous_credited_microusd)
);
CREATE TRIGGER allowance_replacement_apply BEFORE INSERT ON allowance_replacements
BEGIN
  SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM allowance_windows WHERE account_id=NEW.account_id
    AND period_start=NEW.window_start AND period_end=NEW.previous_end AND source=NEW.previous_source
    AND ceiling_microusd=NEW.previous_ceiling_microusd AND credited_microusd=NEW.previous_credited_microusd
    AND period_start=(SELECT MAX(period_start) FROM allowance_windows WHERE account_id=NEW.account_id))
    OR NEW.paid_start>=NEW.previous_end
    THEN RAISE(ABORT,'allowance_replacement_conflict') END;
  UPDATE usage_accounts SET budget_microusd=budget_microusd+NEW.delta_microusd,updated_at=NEW.created_at
    WHERE account_id=NEW.account_id;
  UPDATE allowance_windows SET period_end=NEW.paid_end,source=NEW.paid_source,
    ceiling_microusd=NEW.verified_ceiling_microusd,credited_microusd=NEW.target_microusd
    WHERE account_id=NEW.account_id AND period_start=NEW.window_start;
END;
CREATE TRIGGER allowance_replacement_no_update BEFORE UPDATE ON allowance_replacements
BEGIN SELECT RAISE(ABORT,'immutable_allowance_replacement'); END;
CREATE TRIGGER allowance_replacement_no_delete BEFORE DELETE ON allowance_replacements
BEGIN SELECT RAISE(ABORT,'immutable_allowance_replacement'); END;
