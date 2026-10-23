-- User-authored instructions are account data, never executable tool definitions.
CREATE TABLE skill_versions (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 64 AND name NOT GLOB '*[^a-z0-9_-]*'),
  version TEXT NOT NULL CHECK(length(version) = 64),
  instructions TEXT NOT NULL CHECK(length(CAST(instructions AS BLOB)) BETWEEN 1 AND 49152),
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY (account_id, name, version)
);
CREATE TABLE skills (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  version TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (account_id, name),
  FOREIGN KEY (account_id, name, version) REFERENCES skill_versions(account_id, name, version) ON DELETE CASCADE
);
CREATE TRIGGER skills_account_limit BEFORE INSERT ON skills
WHEN (SELECT COUNT(*) FROM skills WHERE account_id = NEW.account_id) >= 50
BEGIN
  SELECT RAISE(ABORT, 'skill_limit');
END;
CREATE TRIGGER skill_versions_immutable BEFORE UPDATE ON skill_versions
BEGIN
  SELECT RAISE(ABORT, 'immutable_skill_version');
END;
