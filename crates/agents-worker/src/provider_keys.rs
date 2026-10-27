//! Owner-scoped protocol credentials. Metadata is public to the owner; keys are
//! decrypted only for inference and never returned by the management API.

use agents_core::ProviderProtocol;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Env, Method, Request, Response, Result};

use super::{
    AuthContext, bounded_json_limit, crypto, d1_integer, encryption_key, error_response, failure,
    now, secure_response,
};

pub struct ProviderCredential {
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub api_key: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ProviderMetadata {
    pub provider: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
}

#[derive(Deserialize)]
struct CredentialRow {
    id: String,
    provider: String,
    protocol: ProviderProtocol,
    base_url: String,
    name: String,
    key_encrypted: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialInput {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    protocol: Option<ProviderProtocol>,
    #[serde(default)]
    base_url: Option<String>,
    api_key: String,
}

struct ValidatedInput {
    name: String,
    protocol: ProviderProtocol,
    base_url: String,
    api_key: String,
}

pub async fn metadata(env: &Env, account: &str, id: &str) -> Result<ProviderMetadata> {
    env.d1("DB")?
        .prepare(
            "SELECT provider,protocol,base_url FROM provider_keys WHERE account_id=?1 AND id=?2",
        )
        .bind(&[account.into(), id.into()])?
        .first::<ProviderMetadata>(None)
        .await?
        .ok_or_else(|| failure("provider_key_not_found"))
}

pub async fn credential(env: &Env, account: &str, id: &str) -> Result<ProviderCredential> {
    let row = find(env, account, id).await?;
    let context = encryption_context(account, &row.id, &row.provider);
    let bytes = crypto(agents_auth::unseal(
        &encryption_key(env)?,
        &context,
        &row.key_encrypted,
    ))?;
    let api_key = String::from_utf8(bytes).map_err(|_| failure("invalid_provider_key"))?;
    validate_secret(&api_key)?;
    Ok(ProviderCredential {
        protocol: row.protocol,
        base_url: normalize_base_url(row.protocol, &row.base_url)?,
        api_key,
    })
}

pub async fn handle(req: &mut Request, env: &Env, context: &AuthContext) -> Result<Response> {
    let path = req.path();
    if path == "/v1/provider-keys" {
        return collection(req, env, context).await;
    }
    let id = path
        .strip_prefix("/v1/provider-keys/")
        .filter(|id| valid_id(id))
        .ok_or_else(|| failure("invalid_key_id"))?;
    if req.method() == Method::Delete {
        return delete(env, &context.account_id, id).await;
    }
    if req.method() != Method::Put {
        return error_response("not_found", 404);
    }
    let input = read_input(req).await?;
    if let Some((base_url, name)) = legacy_defaults(id) {
        return put_legacy(env, &context.account_id, id, base_url, name, input).await;
    }
    put(env, &context.account_id, id, input).await
}

async fn collection(req: &mut Request, env: &Env, context: &AuthContext) -> Result<Response> {
    let db = env.d1("DB")?;
    if req.method() == Method::Get {
        let rows = db.prepare("SELECT id,name,CASE WHEN provider IN ('openrouter','deepseek') THEN provider ELSE protocol END AS provider,protocol,base_url,created_at,updated_at FROM provider_keys WHERE account_id=?1 ORDER BY created_at,id")
            .bind(&[context.account_id.clone().into()])?.all().await?.results::<Value>()?;
        return secure_response(Response::from_json(&json!({"data":rows}))?);
    }
    if req.method() != Method::Post {
        return error_response("not_found", 404);
    }
    let input = read_input(req).await?.validate(None)?;
    let id = format!("cred_{}", crypto(agents_auth::random_token())?);
    let protocol = protocol_name(input.protocol);
    let encrypted = encrypt(env, &context.account_id, &id, &id, &input.api_key)?;
    let row = db.prepare("INSERT INTO provider_keys(id,account_id,provider,protocol,base_url,name,key_encrypted,created_at,updated_at) SELECT ?1,?2,?1,?3,?4,?5,?6,?7,?7 WHERE (SELECT COUNT(*) FROM provider_keys WHERE account_id=?2)<100 RETURNING id,name,CASE WHEN provider IN ('openrouter','deepseek') THEN provider ELSE protocol END AS provider,protocol,base_url,created_at,updated_at")
        .bind(&[id.into(),context.account_id.clone().into(),protocol.into(),input.base_url.into(),input.name.into(),encrypted.into(),d1_integer(now())?])?.first::<Value>(None).await?;
    match row {
        Some(row) => secure_response(Response::from_json(&row)?.with_status(201)),
        None => error_response("provider_key_limit", 409),
    }
}

async fn find(env: &Env, account: &str, id: &str) -> Result<CredentialRow> {
    env.d1("DB")?
        .prepare("SELECT id,provider,protocol,base_url,name,key_encrypted FROM provider_keys WHERE account_id=?1 AND id=?2")
        .bind(&[account.into(), id.into()])?
        .first::<CredentialRow>(None)
        .await?
        .ok_or_else(|| failure("provider_key_not_found"))
}

async fn put(env: &Env, account: &str, id: &str, input: CredentialInput) -> Result<Response> {
    let existing = match find(env, account, id).await {
        Ok(row) => row,
        Err(worker::Error::RustError(code)) if code == "provider_key_not_found" => {
            return error_response("provider_key_not_found", 404);
        }
        Err(error) => return Err(error),
    };
    let input = input.validate(Some(&existing))?;
    preserve_route(&existing, &input)?;
    let encrypted = encrypt(env, account, id, &existing.provider, &input.api_key)?;
    let row = env.d1("DB")?.prepare("UPDATE provider_keys SET protocol=?3,base_url=?4,name=?5,key_encrypted=?6,key_version=key_version+1,updated_at=?7 WHERE account_id=?1 AND id=?2 RETURNING id,name,CASE WHEN provider IN ('openrouter','deepseek') THEN provider ELSE protocol END AS provider,protocol,base_url,created_at,updated_at")
        .bind(&[account.into(),id.into(),protocol_name(input.protocol).into(),input.base_url.into(),input.name.into(),encrypted.into(),d1_integer(now())?])?.first::<Value>(None).await?;
    match row {
        Some(row) => secure_response(Response::from_json(&row)?),
        None => error_response("provider_key_not_found", 404),
    }
}

async fn put_legacy(
    env: &Env,
    account: &str,
    provider: &str,
    base_url: &str,
    name: &str,
    input: CredentialInput,
) -> Result<Response> {
    if input
        .protocol
        .is_some_and(|value| value != ProviderProtocol::Openai)
        || input.base_url.as_deref().is_some_and(|value| {
            normalize_base_url(ProviderProtocol::Openai, value)
                .ok()
                .as_deref()
                != Some(base_url)
        })
    {
        return Err(failure("invalid_provider"));
    }
    validate_secret(&input.api_key)?;
    let name = validate_name(input.name.as_deref().unwrap_or(name))?;
    let id = format!("cred_{}", crypto(agents_auth::random_token())?);
    let encrypted = encrypt(env, account, &id, provider, &input.api_key)?;
    let row = env.d1("DB")?.prepare("INSERT INTO provider_keys(id,account_id,provider,protocol,base_url,name,key_encrypted,created_at,updated_at) SELECT ?1,?2,?3,'openai',?4,?5,?6,?7,?7 WHERE (SELECT COUNT(*) FROM provider_keys WHERE account_id=?2)<100 OR EXISTS(SELECT 1 FROM provider_keys WHERE account_id=?2 AND provider=?3) ON CONFLICT(account_id,provider) DO UPDATE SET protocol=excluded.protocol,base_url=excluded.base_url,name=excluded.name,key_encrypted=excluded.key_encrypted,key_version=provider_keys.key_version+1,updated_at=excluded.updated_at RETURNING id,name,CASE WHEN provider IN ('openrouter','deepseek') THEN provider ELSE protocol END AS provider,protocol,base_url,created_at,updated_at")
        .bind(&[id.into(),account.into(),provider.into(),base_url.into(),name.into(),encrypted.into(),d1_integer(now())?])?.first::<Value>(None).await?;
    match row {
        Some(row) => secure_response(Response::from_json(&row)?),
        None => error_response("provider_key_limit", 409),
    }
}

async fn delete(env: &Env, account: &str, id: &str) -> Result<Response> {
    let sql = if legacy_defaults(id).is_some() {
        "DELETE FROM provider_keys WHERE account_id=?1 AND provider=?2"
    } else {
        "DELETE FROM provider_keys WHERE account_id=?1 AND id=?2"
    };
    env.d1("DB")?
        .prepare(sql)
        .bind(&[account.into(), id.into()])?
        .run()
        .await?;
    secure_response(Response::from_json(&json!({"ok":true}))?)
}

async fn read_input(req: &mut Request) -> Result<CredentialInput> {
    serde_json::from_value(bounded_json_limit(req, 32 * 1024).await?)
        .map_err(|_| failure("invalid_provider_key"))
}

impl CredentialInput {
    fn validate(self, existing: Option<&CredentialRow>) -> Result<ValidatedInput> {
        validate_secret(&self.api_key)?;
        let protocol = self
            .protocol
            .or_else(|| existing.map(|row| row.protocol))
            .ok_or_else(|| failure("invalid_provider"))?;
        // Credential metadata selects an authentication family. Native Responses
        // is an explicit model transport using the same OpenAI-family key.
        if protocol == ProviderProtocol::Responses {
            return Err(failure("invalid_provider"));
        }
        let base_url = self
            .base_url
            .as_deref()
            .or_else(|| existing.map(|row| row.base_url.as_str()))
            .ok_or_else(|| failure("invalid_provider"))?;
        let base_url = normalize_base_url(protocol, base_url)?;
        let url = url::Url::parse(&base_url).map_err(|_| failure("invalid_provider"))?;
        let hostname = url.host_str().unwrap_or_default();
        let default_name = format!("{} · {:.80}", protocol_name(protocol), hostname);
        let name = self
            .name
            .as_deref()
            .or_else(|| existing.map(|row| row.name.as_str()))
            .unwrap_or(&default_name);
        Ok(ValidatedInput {
            name: validate_name(name)?,
            protocol,
            base_url,
            api_key: self.api_key,
        })
    }
}

pub(crate) fn normalize_base_url(protocol: ProviderProtocol, value: &str) -> Result<String> {
    if value.len() > 2048 || value.chars().any(char::is_control) || value.trim() != value {
        return Err(failure("invalid_provider"));
    }
    let mut url = url::Url::parse(value).map_err(|_| failure("invalid_provider"))?;
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !public_hostname(host)
    {
        return Err(failure("invalid_provider"));
    }
    let path = url.path().trim_end_matches('/').to_owned();
    let path = if protocol == ProviderProtocol::Anthropic && !path.ends_with("/v1") {
        format!("{path}/v1")
    } else {
        path
    };
    url.set_path(&path);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub(crate) fn public_hostname(host: &str) -> bool {
    host.contains('.')
        && host.parse::<std::net::IpAddr>().is_err()
        && !host.starts_with('[')
        && ![
            "localhost",
            "local",
            "internal",
            "home",
            "lan",
            "test",
            "invalid",
        ]
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        && !matches!(host, "metadata.google.internal" | "metadata.google")
}

fn preserve_route(existing: &CredentialRow, input: &ValidatedInput) -> Result<()> {
    if input.protocol != existing.protocol
        || input.base_url != normalize_base_url(existing.protocol, &existing.base_url)?
    {
        return Err(failure("credential_route_immutable"));
    }
    Ok(())
}

fn validate_secret(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 16 * 1024 || value.chars().any(char::is_control) {
        return Err(failure("invalid_provider_key"));
    }
    Ok(())
}

fn validate_name(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 100 || value.chars().any(char::is_control) {
        return Err(failure("invalid_key_name"));
    }
    Ok(value.to_owned())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn protocol_name(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::Openai => "openai",
        ProviderProtocol::Responses => "responses",
        ProviderProtocol::Anthropic => "anthropic",
    }
}

fn legacy_defaults(provider: &str) -> Option<(&'static str, &'static str)> {
    match provider {
        "openrouter" => Some(("https://openrouter.ai/api/v1", "OpenRouter")),
        "deepseek" => Some(("https://api.deepseek.com", "DeepSeek")),
        _ => None,
    }
}

fn encryption_context(account: &str, id: &str, provider: &str) -> String {
    // Legacy ciphertext used a vendor namespace; retaining it avoids re-encryption
    // during migration. New credentials bind to their unique immutable ID.
    let namespace = if legacy_defaults(provider).is_some() {
        provider
    } else {
        id
    };
    format!("byok:{account}:{namespace}")
}

fn encrypt(env: &Env, account: &str, id: &str, provider: &str, secret: &str) -> Result<String> {
    crypto(agents_auth::seal(
        &encryption_key(env)?,
        &encryption_context(account, id, provider),
        secret.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_protocol_cannot_be_changed_into_a_model_transport() {
        let input = CredentialInput {
            name: None,
            protocol: Some(ProviderProtocol::Responses),
            base_url: Some("https://provider.example/v1".into()),
            api_key: "test-only".into(),
        };
        assert!(input.validate(None).is_err());
        assert_eq!(
            normalize_base_url(ProviderProtocol::Responses, "https://API.example.com/v1/").unwrap(),
            normalize_base_url(ProviderProtocol::Openai, "https://API.example.com/v1/").unwrap()
        );
    }

    #[test]
    fn only_public_https_hostname_bases_are_accepted() {
        assert_eq!(
            normalize_base_url(ProviderProtocol::Openai, "https://API.example.com/v1/").unwrap(),
            "https://api.example.com/v1"
        );
        assert_eq!(
            normalize_base_url(ProviderProtocol::Anthropic, "https://api.anthropic.com").unwrap(),
            "https://api.anthropic.com/v1"
        );
        assert_eq!(
            normalize_base_url(
                ProviderProtocol::Anthropic,
                "https://api.deepseek.com/anthropic/"
            )
            .unwrap(),
            "https://api.deepseek.com/anthropic/v1"
        );
        assert_eq!(
            normalize_base_url(
                ProviderProtocol::Anthropic,
                "https://api.deepseek.com/anthropic/v1/"
            )
            .unwrap(),
            "https://api.deepseek.com/anthropic/v1"
        );
        assert_eq!(
            normalize_base_url(ProviderProtocol::Openai, "https://api.deepseek.com").unwrap(),
            "https://api.deepseek.com"
        );
        for invalid in [
            "http://api.example.com/v1",
            "https://user:secret@api.example.com",
            "https://api.example.com?v=1",
            "https://api.example.com/#x",
            "https://localhost",
            "https://api.localhost./v1",
            "https://api.internal",
            "https://api.local",
            "https://api",
            "https://127.0.0.1",
            "https://169.254.169.254",
            "https://2130706433",
            "https://[::1]",
            " https://api.example.com",
            "https://api.example.com\n",
        ] {
            assert!(
                normalize_base_url(ProviderProtocol::Openai, invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn credentials_bind_to_unique_ids_without_changing_legacy_aad() {
        assert_eq!(
            encryption_context("alice", "cred_old", "openrouter"),
            "byok:alice:openrouter"
        );
        assert_ne!(
            encryption_context("alice", "cred_a", "openai"),
            encryption_context("alice", "cred_b", "openai")
        );
        assert_ne!(
            encryption_context("alice", "cred_a", "openai"),
            encryption_context("bob", "cred_a", "openai")
        );
        assert_eq!(
            encryption_context("alice", "cred_a", "openai"),
            encryption_context("alice", "cred_a", "anthropic")
        );
    }

    #[test]
    fn legacy_ciphertext_remains_readable_but_generic_ciphertext_cannot_move() {
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let legacy = agents_auth::seal(key, "byok:alice:openrouter", b"synthetic-old-key").unwrap();
        assert_eq!(
            agents_auth::unseal(
                key,
                &encryption_context("alice", "cred_old", "openrouter"),
                &legacy
            )
            .unwrap(),
            b"synthetic-old-key"
        );
        let context = encryption_context("alice", "cred_a", "cred_a");
        let generic = agents_auth::seal(key, &context, b"synthetic-new-key").unwrap();
        assert_eq!(
            agents_auth::unseal(key, &context, &generic).unwrap(),
            b"synthetic-new-key"
        );
        for wrong_context in [
            encryption_context("bob", "cred_a", "cred_a"),
            encryption_context("alice", "cred_b", "cred_b"),
        ] {
            assert!(agents_auth::unseal(key, &wrong_context, &generic).is_err());
        }
    }

    #[test]
    fn generic_input_needs_protocol_base_and_bounded_secret_but_not_a_label() {
        let input: CredentialInput = serde_json::from_value(json!({"protocol":"anthropic","base_url":"https://api.anthropic.com/v1/","api_key":"secret"})).unwrap();
        let valid = input.validate(None).unwrap();
        assert_eq!(valid.base_url, "https://api.anthropic.com/v1");
        assert_eq!(valid.protocol, ProviderProtocol::Anthropic);
        assert!(valid.name.contains("api.anthropic.com"));
        assert!(validate_secret("").is_err());
        assert!(validate_secret("secret\r\nAuthorization: other").is_err());
        assert!(validate_secret(&"a".repeat(16 * 1024)).is_ok());
        assert!(validate_secret(&"a".repeat(16 * 1024 + 1)).is_err());
        assert!(
            serde_json::from_value::<CredentialInput>(
                json!({"protocol":"other","api_key":"secret"})
            )
            .is_err()
        );
    }

    #[test]
    fn rotation_can_rename_but_cannot_retarget_an_existing_session_credential() {
        let existing = CredentialRow {
            id: "cred-a".into(),
            provider: "anthropic".into(),
            protocol: ProviderProtocol::Anthropic,
            base_url: "https://gateway.example.com/anthropic/v1".into(),
            name: "Previous label".into(),
            key_encrypted: "ciphertext".into(),
        };
        let parse = |value: Value| serde_json::from_value::<CredentialInput>(value).unwrap();
        let rotated = parse(json!({"api_key":"new-secret","name":"New label"}))
            .validate(Some(&existing))
            .unwrap();
        assert!(preserve_route(&existing, &rotated).is_ok());
        assert_eq!(rotated.name, "New label");
        assert_eq!(rotated.protocol, existing.protocol);
        assert_eq!(rotated.base_url, existing.base_url);
        let canonical = parse(
            json!({"api_key":"new-secret","base_url":"https://gateway.example.com/anthropic/"}),
        )
        .validate(Some(&existing))
        .unwrap();
        assert!(preserve_route(&existing, &canonical).is_ok());
        let retargeted =
            parse(json!({"api_key":"new-secret","base_url":"https://other.example.com"}))
                .validate(Some(&existing))
                .unwrap();
        assert!(preserve_route(&existing, &retargeted).is_err());
        let retyped = parse(json!({"api_key":"new-secret","protocol":"openai"}))
            .validate(Some(&existing))
            .unwrap();
        assert!(preserve_route(&existing, &retyped).is_err());
    }
}
