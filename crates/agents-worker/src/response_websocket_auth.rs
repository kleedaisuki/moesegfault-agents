//! Verified WebSocket credentials retain hashes or expiry, never bearer secrets.

use agents_auth::hash_secret;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use worker::{Env, Method, Request, RequestInit, Result};

use crate::auth::AuthContext;

#[derive(Clone)]
pub(crate) struct Grant {
    owner: String,
    credential: Credential,
}

#[derive(Clone)]
enum Credential {
    ApiKey(String),
    Browser(String),
    Identity { expires_at: i64 },
}

fn unauthorized() -> worker::Error {
    worker::Error::RustError("unauthorized".into())
}

/// Call only after authenticate succeeds for this exact request and principal.
pub(crate) fn verified(req: &Request, principal: &AuthContext) -> Result<Grant> {
    let credential = if let Some(bearer) = req
        .headers()
        .get("Authorization")?
        .and_then(|value| value.strip_prefix("Bearer ").map(str::to_owned))
    {
        if principal.via_api_key {
            Credential::ApiKey(hash_secret(&bearer))
        } else {
            #[derive(Deserialize)]
            struct Expiry {
                exp: i64,
            }
            // Authentication already checked signature, issuer, audience and subject.
            // Retain only its expiry; parsing is not a second authentication path.
            let payload = bearer.split('.').nth(1).ok_or_else(unauthorized)?;
            let bytes = URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| unauthorized())?;
            let claims: Expiry = serde_json::from_slice(&bytes).map_err(|_| unauthorized())?;
            Credential::Identity {
                expires_at: claims.exp,
            }
        }
    } else {
        let cookies = req.headers().get("Cookie")?.ok_or_else(unauthorized)?;
        let token = cookies
            .split(';')
            .find_map(|part| {
                let (name, value) = part.trim().split_once('=')?;
                (name == "__Host-magents_session" && !value.is_empty()).then_some(value)
            })
            .ok_or_else(unauthorized)?;
        if principal.csrf_token.is_none() {
            return Err(unauthorized());
        }
        Credential::Browser(hash_secret(token))
    };
    Ok(Grant {
        owner: principal.account_id.clone(),
        credential,
    })
}

impl Grant {
    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }

    /// Every mutating command rechecks revocable authority, even after queueing.
    pub(crate) async fn validate(&self, env: &Env) -> Result<()> {
        let valid = match &self.credential {
            Credential::ApiKey(hash) => env.d1("DB")?
                .prepare("SELECT account_id FROM api_keys WHERE key_hash=?1 AND account_id=?2 AND revoked_at IS NULL")
                .bind(&[hash.as_str().into(), self.owner.as_str().into()])?
                .first::<Value>(None).await?.is_some(),
            Credential::Browser(hash) => {
                let stub = env.durable_object("AUTH_SESSIONS")?.id_from_name(hash)?.get_stub()?;
                let init = RequestInit { method: Method::Post,
                    body: Some(json!({"token_hash":hash,"refresh_billing":false}).to_string().into()),
                    ..Default::default() };
                let mut response = stub.fetch_with_request(Request::new_with_init(
                    "https://auth-session.internal/session", &init)?).await?;
                response.status_code() == 200 && response.json::<Value>().await?["account_id"] == self.owner
            }
            Credential::Identity { expires_at } => *expires_at > (crate::util::now_ms() / 1000) as i64,
        };
        if !valid {
            return Err(unauthorized());
        }
        Ok(())
    }
}
