-- Keep opaque credential IDs and ciphertext unchanged so existing session pins
-- and legacy account/provider encryption contexts remain readable.
CREATE TABLE provider_keys_v2 (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  provider TEXT NOT NULL CHECK (provider IN ('openrouter', 'deepseek') OR provider=id),
  protocol TEXT NOT NULL DEFAULT 'openai' CHECK (protocol IN ('openai', 'anthropic')),
  base_url TEXT NOT NULL DEFAULT '',
  name TEXT NOT NULL DEFAULT '',
  key_encrypted TEXT NOT NULL,
  key_version INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

INSERT INTO provider_keys_v2
  (id,account_id,provider,protocol,base_url,name,key_encrypted,key_version,created_at,updated_at)
SELECT id,account_id,provider,'openai',
  CASE provider
    WHEN 'openrouter' THEN 'https://openrouter.ai/api/v1'
    WHEN 'deepseek' THEN 'https://api.deepseek.com'
  END,
  CASE provider WHEN 'openrouter' THEN 'OpenRouter' WHEN 'deepseek' THEN 'DeepSeek' END,
  key_encrypted,key_version,created_at,updated_at
FROM provider_keys;

DROP TABLE provider_keys;
ALTER TABLE provider_keys_v2 RENAME TO provider_keys;

CREATE INDEX provider_keys_account ON provider_keys(account_id, created_at);
-- The old binary targets these exact conflict columns without a partial-index
-- predicate. Generic credentials use their unique ID as the storage discriminator.
CREATE UNIQUE INDEX provider_keys_account_provider ON provider_keys(account_id,provider);

-- Old binaries omit new metadata columns during the migration-to-upload window.
-- Fill only their known vendor defaults; ciphertext and encryption AAD stay intact.
CREATE TRIGGER provider_keys_legacy_metadata AFTER INSERT ON provider_keys
WHEN NEW.provider IN ('openrouter', 'deepseek') AND (NEW.base_url='' OR NEW.name='')
BEGIN
  UPDATE provider_keys SET
    base_url=CASE WHEN base_url='' THEN
      CASE provider WHEN 'openrouter' THEN 'https://openrouter.ai/api/v1'
        WHEN 'deepseek' THEN 'https://api.deepseek.com' END
      ELSE base_url END,
    name=CASE WHEN name='' THEN
      CASE provider WHEN 'openrouter' THEN 'OpenRouter' WHEN 'deepseek' THEN 'DeepSeek' END
      ELSE name END
  WHERE id=NEW.id;
END;
