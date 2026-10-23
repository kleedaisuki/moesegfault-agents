//! Same-origin BFF boundary. OAuth tokens and provider credentials never reach React.
use futures::StreamExt;
use std::cell::RefCell;

use agents_auth::{
    Discovery, LoginTransaction, client_assertion, constant_eq, hash_secret, random_token, seal,
    unseal, verify_identity_token,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::*;

const SESSION_COOKIE: &str = "__Host-magents_session";
const LOGIN_COOKIE: &str = "__Host-magents_login";
const SESSION_SECONDS: i64 = 14 * 86400;

#[derive(Clone)]
pub struct AuthContext {
    pub account_id: String,
    pub csrf_token: Option<String>,
    pub via_api_key: bool,
}

#[derive(Deserialize)]
struct BrowserSession {
    account_id: String,
    tokens_encrypted: String,
    csrf_hash: String,
    expires_at: i64,
}

#[derive(Serialize, Deserialize)]
struct SessionSecrets {
    csrf_token: String,
    oauth: Value,
    access_expires_at: i64,
}

#[derive(Clone)]
struct Metadata {
    discovery: Discovery,
    jwks: Value,
    fetched_at: i64,
    last_key_refresh: i64,
}
thread_local! { static METADATA: RefCell<Option<Metadata>> = const { RefCell::new(None) }; }

fn now() -> i64 {
    (Date::now().as_millis() / 1000) as i64
}
fn failure(code: &str) -> Error {
    Error::RustError(code.to_owned())
}
fn crypto<T>(value: std::result::Result<T, agents_auth::AuthError>) -> Result<T> {
    value.map_err(|_| failure("authentication_failed"))
}
fn setting(env: &Env, name: &str) -> Result<String> {
    Ok(env.var(name)?.to_string())
}
fn encryption_key(env: &Env) -> Result<String> {
    Ok(env.secret("DATA_ENCRYPTION_KEY")?.to_string())
}
fn cookie(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .get("Cookie")
        .ok()
        .flatten()?
        .split(';')
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name && !value.is_empty()).then(|| value.to_owned())
        })
}
fn cookie_wire(name: &str, token: &str, age: i64) -> String {
    format!("{name}={token}; Path=/; Max-Age={age}; Secure; HttpOnly; SameSite=Lax")
}
fn secure_response(mut response: Response) -> Result<Response> {
    response.headers_mut().set("Cache-Control", "no-store")?;
    response
        .headers_mut()
        .set("Referrer-Policy", "no-referrer")?;
    Ok(response)
}
fn error_response(code: &str, status: u16) -> Result<Response> {
    secure_response(
        Response::from_json(
            &json!({"error":{"code":code,"message":code,"type":"authentication_error"}}),
        )?
        .with_status(status),
    )
}

async fn bounded_json(req: &mut Request) -> Result<Value> {
    if req
        .headers()
        .get("Content-Length")?
        .and_then(|s| s.parse::<usize>().ok())
        .is_some_and(|len| len > 16384)
    {
        return Err(failure("body_too_large"));
    }
    let mut stream = req.stream()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > 16384usize.saturating_sub(bytes.len()) {
            return Err(failure("body_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| failure("invalid_request_body"))
}

async fn get_json(endpoint: &str) -> Result<Value> {
    let mut response = Fetch::Url(endpoint.parse()?).send().await?;
    record_upstream(&mut response, "identity", "metadata").await;
    if response.status_code() != 200 {
        return Err(failure("identity_unavailable"));
    }
    response.json().await
}

async fn record_upstream(response: &mut Response, provider: &str, phase: &str) {
    let status = response.status_code();
    let correlation = response
        .headers()
        .get("x-moesegfault-correlation-id")
        .ok()
        .flatten();
    let error_code = if status >= 400 {
        response.json::<Value>().await.ok().and_then(|body| {
            body["error"]
                .as_str()
                .or_else(|| body["error_code"].as_str())
                .map(str::to_owned)
        })
    } else {
        None
    };
    crate::util::emit(
        if status >= 400 {
            "provider.failed"
        } else {
            "provider.completed"
        },
        json!({"provider":provider,"phase":phase,"status":status,"request_id":correlation,"error_code":error_code}),
    );
}

async fn metadata(env: &Env, refresh_keys: bool) -> Result<Metadata> {
    let issuer = setting(env, "IDENTITY_ISSUER")?;
    let cached = METADATA.with(|cache| cache.borrow().clone());
    if let Some(cached) = cached.as_ref() {
        if cached.discovery.issuer == issuer
            && cached.fetched_at + 300 > now()
            && (!refresh_keys || cached.last_key_refresh + 30 > now())
        {
            return Ok(cached.clone());
        }
    }
    // Reserve the refresh slot before yielding: unknown-kid floods cannot fan out requests.
    METADATA.with(|cache| {
        if let Some(entry) = cache.borrow_mut().as_mut() {
            entry.last_key_refresh = now();
        }
    });
    let discovery: Discovery = serde_json::from_value(
        get_json(&format!("{issuer}/.well-known/openid-configuration")).await?,
    )?;
    crypto(discovery.validate(&issuer))?;
    let jwks = get_json(&discovery.jwks_uri).await?;
    let result = Metadata {
        discovery,
        jwks,
        fetched_at: now(),
        last_key_refresh: now(),
    };
    METADATA.with(|cache| *cache.borrow_mut() = Some(result.clone()));
    Ok(result)
}

async fn verify_token(
    env: &Env,
    wire: &str,
    audience: &str,
    nonce: Option<&str>,
) -> Result<agents_auth::IdentityClaims> {
    let meta = metadata(env, false).await?;
    match verify_identity_token(
        wire,
        &meta.jwks,
        &meta.discovery.issuer,
        audience,
        nonce,
        now(),
    ) {
        Ok(claims) => Ok(claims),
        Err(_) => {
            let refreshed = metadata(env, true).await?;
            crypto(verify_identity_token(
                wire,
                &refreshed.jwks,
                &refreshed.discovery.issuer,
                audience,
                nonce,
                now(),
            ))
        }
    }
}

pub async fn authenticate(req: &Request, env: &Env) -> Result<AuthContext> {
    let db = env.d1("DB")?;
    if let Some(bearer) = req
        .headers()
        .get("Authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(str::to_owned))
    {
        if bearer.starts_with("mag_") {
            let key_hash = hash_secret(&bearer);
            let used_at = now();
            let row = db
                .prepare("SELECT account_id,last_used_at FROM api_keys WHERE key_hash=?1 AND revoked_at IS NULL")
                .bind(&[key_hash.clone().into()])?
                .first::<Value>(None)
                .await?
                .ok_or_else(|| failure("unauthorized"))?;
            let account_id = row["account_id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| failure("unauthorized"))?;
            // Revocation is checked on every request; only this informational timestamp is coarsened.
            if row["last_used_at"]
                .as_i64()
                .is_none_or(|last| last <= used_at - 300)
            {
                db.prepare("UPDATE api_keys SET last_used_at=?1 WHERE key_hash=?2 AND revoked_at IS NULL AND (last_used_at IS NULL OR last_used_at<=?3)")
                    .bind(&[used_at.into(), key_hash.into(), (used_at - 300).into()])?
                    .run()
                    .await?;
            }
            return Ok(AuthContext {
                account_id,
                csrf_token: None,
                via_api_key: true,
            });
        }
        let claims = verify_token(
            env,
            &bearer,
            &setting(env, "IDENTITY_NATIVE_CLIENT_ID")?,
            None,
        )
        .await?;
        let account_id = ensure_account(env, &claims.iss, &claims.sub).await?;
        synchronize_subscription(env, &account_id, &bearer).await?;
        return Ok(AuthContext {
            account_id,
            csrf_token: None,
            via_api_key: false,
        });
    }
    let token = cookie(req, SESSION_COOKIE).ok_or_else(|| failure("unauthorized"))?;
    let hash = hash_secret(&token);
    let stub = env
        .durable_object("AUTH_SESSIONS")?
        .id_from_name(&hash)?
        .get_stub()?;
    let init = RequestInit {
        method: Method::Post,
        body: Some(json!({"token_hash":hash}).to_string().into()),
        ..Default::default()
    };
    let mut reply = stub
        .fetch_with_request(Request::new_with_init(
            "https://auth-session.internal/session",
            &init,
        )?)
        .await?;
    if reply.status_code() != 200 {
        return Err(failure("unauthorized"));
    }
    let session: Value = reply.json().await?;
    let csrf_token = session["csrf_token"]
        .as_str()
        .ok_or_else(|| failure("unauthorized"))?;
    if !matches!(req.method(), Method::Get | Method::Head | Method::Options) {
        let origin = req
            .headers()
            .get("Origin")?
            .ok_or_else(|| failure("csrf_failed"))?;
        let csrf = req
            .headers()
            .get("X-CSRF-Token")?
            .ok_or_else(|| failure("csrf_failed"))?;
        if origin != setting(env, "APP_ORIGIN")? || !constant_eq(&csrf, csrf_token) {
            return Err(failure("csrf_failed"));
        }
    }
    Ok(AuthContext {
        account_id: session["account_id"]
            .as_str()
            .ok_or_else(|| failure("unauthorized"))?
            .to_owned(),
        csrf_token: Some(csrf_token.into()),
        via_api_key: false,
    })
}

async fn ensure_account(env: &Env, issuer: &str, subject: &str) -> Result<String> {
    let db = env.d1("DB")?;
    let lookup = || {
        db.prepare("SELECT a.id FROM accounts a JOIN usage_accounts u ON u.account_id=a.id WHERE a.issuer=?1 AND a.subject=?2")
        .bind(&[issuer.into(), subject.into()])
    };
    if let Some(row) = lookup()?.first::<Value>(None).await? {
        return row["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| failure("account_failed"));
    }
    let id = format!("acct_{}", crypto(random_token())?);
    let created_at = now();
    // Bootstrap the principal and wallet atomically, including repair of a partially migrated account.
    db.batch(vec![
        db.prepare("INSERT INTO accounts(id,issuer,subject,created_at,updated_at) VALUES(?1,?2,?3,?4,?4) ON CONFLICT(issuer,subject) DO NOTHING")
            .bind(&[id.into(), issuer.into(), subject.into(), created_at.into()])?,
        db.prepare("INSERT INTO usage_accounts(account_id,updated_at) SELECT id,?3 FROM accounts WHERE issuer=?1 AND subject=?2 ON CONFLICT(account_id) DO NOTHING")
            .bind(&[issuer.into(), subject.into(), created_at.into()])?,
    ]).await?;
    let row = lookup()?
        .first::<Value>(None)
        .await?
        .ok_or_else(|| failure("account_failed"))?;
    row["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| failure("account_failed"))
}

pub async fn handle_auth(mut req: Request, env: &Env) -> Result<Response> {
    let url = req.url()?;
    match (req.method(), url.path()) {
        (Method::Get, "/auth/config") => secure_response(Response::from_json(
            &json!({"issuer":setting(env,"IDENTITY_ISSUER")?,"native_client_id":setting(env,"IDENTITY_NATIVE_CLIENT_ID")?,"native_redirect_uri":setting(env,"IDENTITY_NATIVE_REDIRECT_URI")?}),
        )?),
        (Method::Get, "/auth/login") => {
            let return_to = url
                .query_pairs()
                .find(|(key, _)| key == "return_to")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_else(|| "/".into());
            let tx = crypto(LoginTransaction::new(
                &setting(env, "IDENTITY_ISSUER")?,
                &return_to,
                now(),
            ))?;
            let meta = metadata(env, false).await?;
            let redirect = crypto(tx.authorization_url(
                &meta.discovery,
                &setting(env, "IDENTITY_WEB_CLIENT_ID")?,
                &setting(env, "IDENTITY_REDIRECT_URI")?,
            ))?;
            let payload = crypto(seal(
                &encryption_key(env)?,
                &format!("login:{}", hash_secret(&tx.state)),
                &serde_json::to_vec(&tx)?,
            ))?;
            env.d1("DB")?.prepare("INSERT INTO oidc_transactions(state_hash,payload_encrypted,expires_at,created_at) VALUES(?1,?2,?3,?4)")
                .bind(&[hash_secret(&tx.state).into(),payload.into(),tx.expires_at.into(),now().into()])?.run().await?;
            let mut response = Response::redirect(redirect)?;
            response.headers_mut().append(
                "Set-Cookie",
                &cookie_wire(LOGIN_COOKIE, &tx.browser_binding, 300),
            )?;
            secure_response(response)
        }
        (Method::Get, "/auth/callback") => callback(&req, env).await,
        (Method::Post, "/auth/logout") => {
            if req.headers().get("Origin")?.as_deref() != Some(setting(env, "APP_ORIGIN")?.as_str())
            {
                return error_response("csrf_failed", 403);
            }
            if let Some(token) = cookie(&req, SESSION_COOKIE) {
                let hash = hash_secret(&token);
                let row = env
                    .d1("DB")?
                    .prepare("SELECT csrf_hash FROM browser_sessions WHERE token_hash=?1")
                    .bind(&[hash.clone().into()])?
                    .first::<Value>(None)
                    .await?;
                if let Some(row) = row {
                    let csrf = req.headers().get("X-CSRF-Token")?.unwrap_or_default();
                    if !constant_eq(
                        &hash_secret(&csrf),
                        row["csrf_hash"].as_str().unwrap_or_default(),
                    ) {
                        return error_response("csrf_failed", 403);
                    }
                }
                let stub = env
                    .durable_object("AUTH_SESSIONS")?
                    .id_from_name(&hash)?
                    .get_stub()?;
                let init = RequestInit {
                    method: Method::Post,
                    body: Some(json!({"token_hash":hash}).to_string().into()),
                    ..Default::default()
                };
                let _ = stub
                    .fetch_with_request(Request::new_with_init(
                        "https://auth-session.internal/logout",
                        &init,
                    )?)
                    .await;
                env.d1("DB")?
                    .prepare("DELETE FROM browser_sessions WHERE token_hash=?1")
                    .bind(&[hash.into()])?
                    .run()
                    .await?;
            }
            let mut response = Response::from_json(&json!({"ok":true}))?;
            response
                .headers_mut()
                .append("Set-Cookie", &cookie_wire(SESSION_COOKIE, "", 0))?;
            secure_response(response)
        }
        _ => {
            let _ = &mut req;
            error_response("not_found", 404)
        }
    }
}

async fn callback(req: &Request, env: &Env) -> Result<Response> {
    let url = req.url()?;
    let params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let state = params
        .get("state")
        .ok_or_else(|| failure("invalid_callback"))?;
    let state_hash = hash_secret(state);
    // DELETE RETURNING is a single-use claim, including failed/denied callbacks.
    let row = env.d1("DB")?.prepare("DELETE FROM oidc_transactions WHERE state_hash=?1 AND expires_at>?2 RETURNING payload_encrypted")
        .bind(&[state_hash.clone().into(),now().into()])?.first::<Value>(None).await?.ok_or_else(||failure("invalid_callback"))?;
    let decrypted = crypto(unseal(
        &encryption_key(env)?,
        &format!("login:{state_hash}"),
        row["payload_encrypted"]
            .as_str()
            .ok_or_else(|| failure("invalid_callback"))?,
    ))?;
    let tx: LoginTransaction = serde_json::from_slice(&decrypted)?;
    crypto(tx.validate_callback(
        state,
        params.get("iss").map(String::as_str).unwrap_or_default(),
        &cookie(req, LOGIN_COOKIE).unwrap_or_default(),
        now(),
    ))?;
    if params.contains_key("error") {
        return error_response("login_denied", 401);
    }
    let code = params
        .get("code")
        .ok_or_else(|| failure("invalid_callback"))?;
    let meta = metadata(env, false).await?;
    let client_id = setting(env, "IDENTITY_WEB_CLIENT_ID")?;
    let private_jwk: Value =
        serde_json::from_str(&env.secret("IDENTITY_CLIENT_PRIVATE_JWK")?.to_string())?;
    let assertion = crypto(client_assertion(
        &private_jwk,
        &client_id,
        &meta.discovery.token_endpoint,
        now(),
    ))?;
    let form = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", code.as_str()),
            (
                "redirect_uri",
                setting(env, "IDENTITY_REDIRECT_URI")?.as_str(),
            ),
            ("code_verifier", tx.verifier.as_str()),
            (
                "client_assertion_type",
                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
            ),
            ("client_assertion", assertion.as_str()),
        ])
        .finish();
    let headers = Headers::new();
    headers.set("Content-Type", "application/x-www-form-urlencoded")?;
    let init = RequestInit {
        method: Method::Post,
        headers,
        body: Some(form.into()),
        ..Default::default()
    };
    let mut response = Fetch::Request(Request::new_with_init(
        &meta.discovery.token_endpoint,
        &init,
    )?)
    .send()
    .await?;
    record_upstream(&mut response, "identity", "authorization_code").await;
    if response.status_code() != 200 {
        return error_response("code_exchange_failed", 401);
    }
    let tokens: Value = response.json().await?;
    let id_token = tokens["id_token"]
        .as_str()
        .ok_or_else(|| failure("invalid_id_token"))?;
    let claims = verify_token(env, id_token, &client_id, Some(&tx.nonce)).await?;
    let account_id = ensure_account(env, &claims.iss, &claims.sub).await?;
    synchronize_subscription(
        env,
        &account_id,
        tokens["access_token"]
            .as_str()
            .ok_or_else(|| failure("invalid_access_token"))?,
    )
    .await?;
    let session_token = crypto(random_token())?;
    let csrf = crypto(random_token())?;
    let access_expires_at = now() + tokens["expires_in"].as_i64().unwrap_or(300).min(3600);
    let secrets = SessionSecrets {
        csrf_token: csrf.clone(),
        oauth: tokens,
        access_expires_at,
    };
    let sealed = crypto(seal(
        &encryption_key(env)?,
        &format!("session:{}", hash_secret(&session_token)),
        &serde_json::to_vec(&secrets)?,
    ))?;
    let expires_at = now() + SESSION_SECONDS;
    env.d1("DB")?.prepare("INSERT INTO browser_sessions(token_hash,account_id,tokens_encrypted,csrf_hash,expires_at,created_at) VALUES(?1,?2,?3,?4,?5,?6)")
        .bind(&[hash_secret(&session_token).into(),account_id.into(),sealed.into(),hash_secret(&csrf).into(),expires_at.into(),now().into()])?.run().await?;
    let mut response =
        Response::redirect(format!("{}{}", setting(env, "APP_ORIGIN")?, tx.return_to).parse()?)?;
    response.headers_mut().append(
        "Set-Cookie",
        &cookie_wire(SESSION_COOKIE, &session_token, expires_at - now()),
    )?;
    response
        .headers_mut()
        .append("Set-Cookie", &cookie_wire(LOGIN_COOKIE, "", 0))?;
    secure_response(response)
}

async fn synchronize_subscription(env: &Env, account_id: &str, access_token: &str) -> Result<()> {
    fetch_subscriptions(env, account_id, access_token).await?;
    crate::allowances::sync(env, account_id).await
}

async fn fetch_subscriptions(env: &Env, account_id: &str, access_token: &str) -> Result<()> {
    let Some(origin) = env.var("BILLING_API_ORIGIN").ok().map(|v| v.to_string()) else {
        return Ok(());
    };
    let issuer = setting(env, "IDENTITY_ISSUER")?;
    let expected = if issuer == "https://identity-staging.moesegfault.dev" {
        "https://billing-staging.moesegfault.dev"
    } else {
        "https://billing.moesegfault.dev"
    };
    if origin != expected {
        return Err(failure("billing_environment_mismatch"));
    }
    let db = env.d1("DB")?;
    let existing = db.prepare("SELECT fetched_at FROM subscription_snapshots WHERE account_id=?1 AND product_id='agents' AND fetched_at>?2")
        .bind(&[account_id.into(),(now()-60).into()])?.first::<Value>(None).await?;
    if existing.is_some() {
        return Ok(());
    }
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {access_token}"))?;
    let request = Request::new_with_init(
        &format!("{origin}/v1/me"),
        &RequestInit {
            headers,
            ..Default::default()
        },
    )?;
    let mut response = Fetch::Request(request).send().await?;
    record_upstream(&mut response, "billing", "subscription").await;
    if response.status_code() != 200 {
        // Stale grants never extend access. A temporary outage does not block Free login.
        return Ok(());
    }
    let profile: Value = response.json().await?;
    let subscriptions = profile["subscriptions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let relevant: Vec<Value> = subscriptions
        .into_iter()
        .filter(|s| s["product_id"] == "agents")
        .collect();
    db.prepare("INSERT INTO subscription_snapshots(account_id,product_id,subscription_json,fetched_at,valid_until) VALUES(?1,'agents',?2,?3,?4) ON CONFLICT(account_id,product_id) DO UPDATE SET subscription_json=excluded.subscription_json,fetched_at=excluded.fetched_at,valid_until=excluded.valid_until")
        .bind(&[account_id.into(),serde_json::to_string(&relevant)?.into(),now().into(),(now()+300).into()])?.run().await?;
    Ok(())
}

pub async fn handle_account(
    mut req: Request,
    env: &Env,
    context: &AuthContext,
) -> Result<Response> {
    let path = req.path();
    if path == "/v1/account" && req.method() == Method::Get {
        return account_response(env, context).await;
    }
    if context.via_api_key {
        return error_response("human_authentication_required", 403);
    }
    if path.starts_with("/v1/api-keys") {
        return api_keys(&mut req, env, context).await;
    }
    if path.starts_with("/v1/provider-keys") {
        return provider_keys(&mut req, env, context).await;
    }
    if path == "/v1/diagnostics" && req.method() == Method::Post {
        return diagnostics(&mut req, env, context).await;
    }
    error_response("not_found", 404)
}

async fn account_response(env: &Env, context: &AuthContext) -> Result<Response> {
    crate::allowances::sync(env, &context.account_id).await?;
    let subscriptions = crate::store::Store::new(env)
        .subscriptions(&context.account_id)
        .await?;
    let policy = agents_auth::billing::PlanPolicy::default();
    let limits = agents_auth::billing::limits(&subscriptions, now(), &policy);
    let tier = agents_auth::billing::resolve_tier(&subscriptions, now(), &policy);
    let wallet = crate::store::Store::new(env)
        .usage(&context.account_id)
        .await?;
    let credit = wallet.budget_microusd - wallet.spent_microusd - wallet.reserved_microusd;
    let subscribe =
        if setting(env, "IDENTITY_ISSUER")? == "https://identity-staging.moesegfault.dev" {
            "https://subscribe-staging.moesegfault.dev/"
        } else {
            "https://subscribe.moesegfault.dev/"
        };
    secure_response(Response::from_json(
        &json!({"account_id":context.account_id,"plan":tier,"limits":{"agents":limits.agent_configs,"agent_configs":limits.agent_configs,"concurrent_sessions":limits.concurrent_sessions,"concurrent_runs":limits.concurrent_runs},"credit_microusd":credit,"csrf_token":context.csrf_token,"subscription_url":subscribe,"billing_mode":env.var("BILLING_MODE").ok().map(|v|v.to_string()).filter(|s|s=="test"||s=="live")}),
    )?)
}

async fn api_keys(req: &mut Request, env: &Env, context: &AuthContext) -> Result<Response> {
    let db = env.d1("DB")?;
    match req.method() {
        Method::Get if req.path() == "/v1/api-keys" => {
            let rows = db.prepare("SELECT id,name,key_prefix AS prefix,created_at,last_used_at FROM api_keys WHERE account_id=?1 AND revoked_at IS NULL ORDER BY created_at DESC LIMIT 100")
                .bind(&[context.account_id.clone().into()])?.all().await?.results::<Value>()?;
            secure_response(Response::from_json(&json!({"data":rows}))?)
        }
        Method::Post if req.path() == "/v1/api-keys" => {
            let body = bounded_json(req).await?;
            let name = body["name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.len() <= 100)
                .ok_or_else(|| failure("invalid_key_name"))?;
            let id = format!("key_{}", crypto(random_token())?);
            let key = format!("mag_{}", crypto(random_token())?);
            let prefix = &key[..12];
            let inserted = db.prepare("INSERT INTO api_keys(id,account_id,name,key_hash,key_prefix,created_at) SELECT ?1,?2,?3,?4,?5,?6 WHERE (SELECT COUNT(*) FROM api_keys WHERE account_id=?2 AND revoked_at IS NULL)<100 RETURNING id")
                .bind(&[id.clone().into(),context.account_id.clone().into(),name.into(),hash_secret(&key).into(),prefix.into(),now().into()])?.first::<Value>(None).await?;
            if inserted.is_none() {
                return error_response("api_key_limit", 409);
            }
            secure_response(
                Response::from_json(&json!({"id":id,"name":name,"key":key,"prefix":prefix}))?
                    .with_status(201),
            )
        }
        Method::Delete => {
            let path = req.path();
            let id = path
                .strip_prefix("/v1/api-keys/")
                .filter(|s| !s.is_empty() && !s.contains('/'))
                .ok_or_else(|| failure("invalid_key_id"))?;
            db.prepare("UPDATE api_keys SET revoked_at=?1 WHERE account_id=?2 AND id=?3 AND revoked_at IS NULL").bind(&[now().into(),context.account_id.clone().into(),id.into()])?.run().await?;
            secure_response(Response::from_json(&json!({"ok":true}))?)
        }
        _ => error_response("not_found", 404),
    }
}

pub struct ProviderCredential {
    pub provider: String,
    pub api_key: String,
}

pub async fn provider_identity(env: &Env, account_id: &str, credential_id: &str) -> Result<String> {
    let row = env
        .d1("DB")?
        .prepare("SELECT provider FROM provider_keys WHERE account_id=?1 AND id=?2")
        .bind(&[account_id.into(), credential_id.into()])?
        .first::<Value>(None)
        .await?
        .ok_or_else(|| failure("provider_key_not_found"))?;
    match row["provider"].as_str() {
        Some(provider @ ("openrouter" | "deepseek")) => Ok(provider.to_owned()),
        _ => Err(failure("invalid_provider")),
    }
}

pub async fn provider_credential(
    env: &Env,
    account_id: &str,
    credential_id: &str,
) -> Result<ProviderCredential> {
    let row = env
        .d1("DB")?
        .prepare("SELECT provider,key_encrypted FROM provider_keys WHERE account_id=?1 AND id=?2")
        .bind(&[account_id.into(), credential_id.into()])?
        .first::<Value>(None)
        .await?
        .ok_or_else(|| failure("provider_key_not_found"))?;
    let provider = row["provider"]
        .as_str()
        .ok_or_else(|| failure("invalid_provider"))?;
    if !["openrouter", "deepseek"].contains(&provider) {
        return Err(failure("invalid_provider"));
    }
    let bytes = crypto(unseal(
        &encryption_key(env)?,
        &format!("byok:{account_id}:{provider}"),
        row["key_encrypted"]
            .as_str()
            .ok_or_else(|| failure("invalid_provider_key"))?,
    ))?;
    Ok(ProviderCredential {
        provider: provider.into(),
        api_key: String::from_utf8(bytes).map_err(|_| failure("invalid_provider_key"))?,
    })
}

async fn provider_keys(req: &mut Request, env: &Env, context: &AuthContext) -> Result<Response> {
    let db = env.d1("DB")?;
    if req.method() == Method::Get && req.path() == "/v1/provider-keys" {
        let rows = db.prepare("SELECT id,provider,created_at,updated_at FROM provider_keys WHERE account_id=?1 ORDER BY provider").bind(&[context.account_id.clone().into()])?.all().await?.results::<Value>()?;
        return secure_response(Response::from_json(&json!({"data":rows}))?);
    }
    let path = req.path();
    let provider = path
        .strip_prefix("/v1/provider-keys/")
        .filter(|s| ["openrouter", "deepseek"].contains(s))
        .ok_or_else(|| failure("invalid_provider"))?;
    if req.method() == Method::Delete {
        db.prepare("DELETE FROM provider_keys WHERE account_id=?1 AND provider=?2")
            .bind(&[context.account_id.clone().into(), provider.into()])?
            .run()
            .await?;
        return secure_response(Response::from_json(&json!({"ok":true}))?);
    }
    if req.method() != Method::Put {
        return error_response("not_found", 404);
    }
    let body = bounded_json(req).await?;
    let secret = body["api_key"]
        .as_str()
        .filter(|s| s.len() >= 8 && s.len() <= 4096 && !s.chars().any(char::is_control))
        .ok_or_else(|| failure("invalid_provider_key"))?;
    let id = format!("cred_{}", crypto(random_token())?);
    let encrypted = crypto(seal(
        &encryption_key(env)?,
        &format!("byok:{}:{provider}", context.account_id),
        secret.as_bytes(),
    ))?;
    let row = db.prepare("INSERT INTO provider_keys(id,account_id,provider,key_encrypted,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(account_id,provider) DO UPDATE SET key_encrypted=excluded.key_encrypted,key_version=provider_keys.key_version+1,updated_at=excluded.updated_at RETURNING id,provider,updated_at")
        .bind(&[id.into(),context.account_id.clone().into(),provider.into(),encrypted.into(),now().into()])?.first::<Value>(None).await?;
    secure_response(Response::from_json(&row)?)
}

async fn diagnostics(req: &mut Request, env: &Env, context: &AuthContext) -> Result<Response> {
    crate::telemetry::receive_diagnostic(req, env, &context.account_id).await
}
/// One object per browser session; no rotating token can be submitted concurrently.
#[durable_object]
pub struct AuthSession {
    state: State,
    env: Env,
}

impl DurableObject for AuthSession {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let env = self.env.clone();
        let storage = self.state.storage();
        // The outer result is always successful: protocol failures are responses, not DO resets.
        self.state
            .block_concurrency_while(async move {
                Ok(match session_request(req, &env, &storage).await {
                    Ok(response) => response,
                    Err(_) => error_response("session_expired", 401)?,
                })
            })
            .await
    }

    async fn alarm(&self) -> Result<Response> {
        let storage = self.state.storage();
        if let Some(hash) = storage.get::<String>("session_hash").await? {
            self.env
                .d1("DB")?
                .prepare("DELETE FROM browser_sessions WHERE token_hash=?1 AND expires_at<=?2")
                .bind(&[hash.into(), now().into()])?
                .run()
                .await?;
        }
        storage.delete_all().await?;
        Response::empty()
    }
}

async fn session_request(mut req: Request, env: &Env, storage: &Storage) -> Result<Response> {
    if req.method() != Method::Post {
        return error_response("not_found", 404);
    }
    let body: Value = req.json().await?;
    let hash = body["token_hash"]
        .as_str()
        .filter(|s| s.len() == 43)
        .ok_or_else(|| failure("invalid_session"))?;
    let db = env.d1("DB")?;
    let row = db.prepare("SELECT account_id,tokens_encrypted,csrf_hash,expires_at FROM browser_sessions WHERE token_hash=?1 AND expires_at>?2")
        .bind(&[hash.into(),now().into()])?.first::<BrowserSession>(None).await?.ok_or_else(||failure("unauthorized"))?;
    if storage.get_alarm().await?.is_none() {
        storage.put("session_hash", hash).await?;
        storage.set_alarm(row.expires_at * 1000).await?;
    }
    let decrypted = crypto(unseal(
        &encryption_key(env)?,
        &format!("session:{hash}"),
        &row.tokens_encrypted,
    ))?;
    let mut secrets: SessionSecrets = serde_json::from_slice(&decrypted)?;
    if !constant_eq(&hash_secret(&secrets.csrf_token), &row.csrf_hash) {
        return Err(failure("invalid_session"));
    }
    if req.path() == "/logout" {
        // Local termination is committed before the best-effort remote revocation.
        db.prepare("DELETE FROM browser_sessions WHERE token_hash=?1")
            .bind(&[hash.into()])?
            .run()
            .await?;
        storage.delete_alarm().await?;
        storage.delete_all().await?;
        let _ = revoke_oauth(env, &secrets.oauth).await;
        return Response::from_json(&json!({"ok":true}));
    }
    if req.path() != "/session" {
        return error_response("not_found", 404);
    }
    if storage
        .get::<bool>("refresh_inflight")
        .await?
        .unwrap_or(false)
    {
        // A crash or unknown HTTP outcome can have consumed the predecessor. Never retry it.
        db.prepare("DELETE FROM browser_sessions WHERE token_hash=?1")
            .bind(&[hash.into()])?
            .run()
            .await?;
        storage.delete("refresh_inflight").await?;
        return error_response("session_expired", 401);
    }
    if secrets.access_expires_at <= now() + 30 {
        storage.put("refresh_inflight", true).await?;
        let result = refresh_oauth(env, &row.account_id, &secrets.oauth).await;
        match result {
            Ok(tokens) => {
                secrets.access_expires_at =
                    now() + tokens["expires_in"].as_i64().unwrap_or(300).clamp(1, 3600);
                secrets.oauth = tokens;
                let encrypted = crypto(seal(
                    &encryption_key(env)?,
                    &format!("session:{hash}"),
                    &serde_json::to_vec(&secrets)?,
                ))?;
                db.prepare("UPDATE browser_sessions SET tokens_encrypted=?1,refresh_version=refresh_version+1 WHERE token_hash=?2")
                    .bind(&[encrypted.into(),hash.into()])?.run().await?;
                storage.delete("refresh_inflight").await?;
            }
            Err(error) => {
                db.prepare("DELETE FROM browser_sessions WHERE token_hash=?1")
                    .bind(&[hash.into()])?
                    .run()
                    .await?;
                storage.delete("refresh_inflight").await?;
                return Err(error);
            }
        }
    }
    Response::from_json(&json!({"account_id":row.account_id,"csrf_token":secrets.csrf_token}))
}

async fn oauth_form(env: &Env, endpoint: &str, fields: &[(&str, &str)]) -> Result<Response> {
    let meta = metadata(env, false).await?;
    let client = setting(env, "IDENTITY_WEB_CLIENT_ID")?;
    let key: Value = serde_json::from_str(&env.secret("IDENTITY_CLIENT_PRIVATE_JWK")?.to_string())?;
    let assertion = crypto(client_assertion(
        &key,
        &client,
        &meta.discovery.token_endpoint,
        now(),
    ))?;
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.extend_pairs(fields.iter().copied());
    form.extend_pairs([
        ("client_id", client.as_str()),
        (
            "client_assertion_type",
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
        ),
        ("client_assertion", assertion.as_str()),
    ]);
    let headers = Headers::new();
    headers.set("Content-Type", "application/x-www-form-urlencoded")?;
    let mut response = Fetch::Request(Request::new_with_init(
        endpoint,
        &RequestInit {
            method: Method::Post,
            headers,
            body: Some(form.finish().into()),
            ..Default::default()
        },
    )?)
    .send()
    .await?;
    let phase = match fields
        .iter()
        .find(|(name, _)| *name == "grant_type")
        .map(|(_, value)| *value)
    {
        Some("refresh_token") => "refresh",
        Some("authorization_code") => "authorization_code",
        _ => "revocation",
    };
    record_upstream(&mut response, "identity", phase).await;
    Ok(response)
}

async fn refresh_oauth(env: &Env, account_id: &str, previous: &Value) -> Result<Value> {
    let refresh_token = previous["refresh_token"]
        .as_str()
        .ok_or_else(|| failure("refresh_unavailable"))?;
    let meta = metadata(env, false).await?;
    let mut response = oauth_form(
        env,
        &meta.discovery.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )
    .await?;
    if response.status_code() != 200 {
        return Err(failure("refresh_failed"));
    }
    let tokens: Value = response.json().await?;
    let next_refresh = tokens["refresh_token"]
        .as_str()
        .ok_or_else(|| failure("refresh_rotation_missing"))?;
    if constant_eq(next_refresh, refresh_token) {
        return Err(failure("refresh_rotation_missing"));
    }
    let access_token = tokens["access_token"]
        .as_str()
        .ok_or_else(|| failure("refresh_invalid"))?;
    let claims = verify_token(
        env,
        access_token,
        &setting(env, "IDENTITY_WEB_CLIENT_ID")?,
        None,
    )
    .await?;
    let owner = env
        .d1("DB")?
        .prepare("SELECT id FROM accounts WHERE id=?1 AND issuer=?2 AND subject=?3")
        .bind(&[account_id.into(), claims.iss.into(), claims.sub.into()])?
        .first::<Value>(None)
        .await?;
    if owner.is_none() {
        return Err(failure("refresh_principal_changed"));
    }
    synchronize_subscription(env, account_id, access_token).await?;
    Ok(tokens)
}

async fn revoke_oauth(env: &Env, tokens: &Value) -> Result<()> {
    let refresh = tokens["refresh_token"]
        .as_str()
        .ok_or_else(|| failure("refresh_unavailable"))?;
    let meta = metadata(env, false).await?;
    let _ = oauth_form(
        env,
        &meta.discovery.revocation_endpoint,
        &[("token", refresh), ("token_type_hint", "refresh_token")],
    )
    .await?;
    Ok(())
}

/// Bound each scheduled sweep; never touch conversations or immutable accounting history.
pub async fn cleanup_expired(env: &Env) -> Result<()> {
    let db = env.d1("DB")?;
    let cutoff = now();
    let login = db.prepare("DELETE FROM oidc_transactions WHERE state_hash IN (SELECT state_hash FROM oidc_transactions WHERE expires_at<=?1 LIMIT 500)").bind(&[cutoff.into()])?;
    let sessions = db.prepare("DELETE FROM browser_sessions WHERE token_hash IN (SELECT token_hash FROM browser_sessions WHERE expires_at<=?1 LIMIT 500)").bind(&[cutoff.into()])?;
    let diagnostics = db.prepare("DELETE FROM diagnostic_reports WHERE id IN (SELECT id FROM diagnostic_reports WHERE expires_at<=?1 LIMIT 500)").bind(&[cutoff.into()])?;
    db.batch(vec![login, sessions, diagnostics]).await?;
    Ok(())
}
