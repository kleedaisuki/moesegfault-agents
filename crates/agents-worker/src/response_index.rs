//! Owner-scoped routing identities, never a substitute for canonical DO state.

use serde::Deserialize;
use worker::{Env, Result};

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ResponseRoute {
    pub id: String,
    pub session_id: String,
    pub agent_id: String,
    pub request_hash: String,
}

pub(crate) async fn get(env: &Env, owner: &str, id: &str) -> Result<Option<ResponseRoute>> {
    env.d1("DB")?
        .prepare("SELECT id,session_id,agent_id,request_hash FROM response_routes WHERE account_id=? AND id=?")
        .bind(&[owner.into(), id.into()])?
        .first(None)
        .await
}

pub(crate) async fn by_key(
    env: &Env,
    owner: &str,
    key_hash: &str,
) -> Result<Option<ResponseRoute>> {
    env.d1("DB")?
        .prepare("SELECT id,session_id,agent_id,request_hash FROM response_routes WHERE account_id=? AND idempotency_hash=?")
        .bind(&[owner.into(), key_hash.into()])?
        .first(None)
        .await
}

/// Reserve before dispatch: an ambiguous transport failure must not create a new run.
/// The optional key's uniqueness also selects one session under racing SDK retries.
pub(crate) async fn reserve(
    env: &Env,
    owner: &str,
    candidate: ResponseRoute,
    key_hash: Option<&str>,
) -> Result<ResponseRoute> {
    let key = key_hash.map_or(wasm_bindgen::JsValue::NULL, Into::into);
    env.d1("DB")?
        .prepare("INSERT INTO response_routes(id,account_id,session_id,agent_id,request_hash,idempotency_hash,created_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(account_id,idempotency_hash) DO NOTHING")
        .bind(&[
            candidate.id.as_str().into(), owner.into(), candidate.session_id.as_str().into(),
            candidate.agent_id.as_str().into(), candidate.request_hash.as_str().into(), key,
            wasm_bindgen::JsValue::from_f64(crate::util::now_ms() as f64),
        ])?
        .run()
        .await?;
    match key_hash {
        Some(key) => by_key(env, owner, key)
            .await?
            .ok_or_else(|| worker::Error::RustError("response_route_unavailable".into())),
        None => Ok(candidate),
    }
}
