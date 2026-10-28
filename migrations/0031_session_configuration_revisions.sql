-- Agent edits are user configuration, not expiring execution/telemetry state.
CREATE TABLE agent_configuration_revisions (
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  revision INTEGER NOT NULL CHECK(revision >= 1),
  payload TEXT NOT NULL CHECK(json_valid(payload)),
  created_at INTEGER NOT NULL,
  PRIMARY KEY(account_id, agent_id, revision)
);
