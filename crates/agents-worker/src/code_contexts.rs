//! Owned context routing is a projection; the source actor validates live authority.

use crate::resources::ResourceMeter;
use agents_core::RuntimeProfile;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{D1PreparedStatement, D1Result, Env, Error, Result};

async fn query(meter: &ResourceMeter<'_>, statement: D1PreparedStatement) -> Result<D1Result> {
    let event = format!("code-catalog:{}", uuid::Uuid::new_v4());
    meter.require_d1(&event).await?;
    let result = statement.all().await?;
    meter.record_d1(&event, &result).await?;
    Ok(result)
}

pub(crate) const IDLE_MS: u64 = 20 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContextState {
    Reserved,
    Ready,
    Expired,
    Deleted,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Allocation {
    pub allocation_id: String,
    pub account_id: String,
    pub source_session_id: String,
    pub source_profile: RuntimeProfile,
    pub funding_run_id: String,
    pub container_id: Option<String>,
    pub daemon_boot_id: Option<String>,
    pub policy_revision: Option<String>,
    pub state: ContextState,
    pub created_at_ms: u64,
    pub last_used_at_ms: u64,
    pub expires_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub allocation_released_at_ms: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct OwnedContext {
    pub id: String,
    pub source_session: String,
    pub profile: RuntimeProfile,
    pub daemon_boot: String,
    pub policy_revision: String,
}

impl Allocation {
    pub fn owned(&self) -> Result<OwnedContext> {
        if self.state != ContextState::Ready {
            return Err(fail("code_container_not_found"));
        }
        Ok(OwnedContext {
            id: self
                .container_id
                .clone()
                .ok_or_else(|| fail("code_context_corrupt"))?,
            source_session: self.source_session_id.clone(),
            profile: self.source_profile,
            daemon_boot: self
                .daemon_boot_id
                .clone()
                .ok_or_else(|| fail("code_context_corrupt"))?,
            policy_revision: self
                .policy_revision
                .clone()
                .ok_or_else(|| fail("code_context_corrupt"))?,
        })
    }
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}

fn opaque(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

pub(crate) fn valid_cid(value: &str) -> bool {
    value
        .strip_prefix("cntr_")
        .is_some_and(|tail| tail.len() == 32 && tail.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

const COLUMNS: &str = "allocation_id,account_id,source_session_id,source_profile,funding_run_id,container_id,daemon_boot_id,policy_revision,state,created_at_ms,last_used_at_ms,expires_at_ms,ended_at_ms,allocation_released_at_ms";

pub(crate) async fn find(
    env: &Env,
    owner: &str,
    cid: &str,
    meter: &ResourceMeter<'_>,
) -> Result<Option<Allocation>> {
    if !valid_cid(cid) {
        return Ok(None);
    }
    let statement = env.d1("DB")?.prepare(format!("SELECT {COLUMNS} FROM code_contexts WHERE account_id=? AND container_id=? AND state='ready'"))
        .bind(&[text(owner),text(cid)])?;
    Ok(query(meter, statement)
        .await?
        .results::<Allocation>()?
        .into_iter()
        .next())
}

pub(crate) async fn find_any(
    env: &Env,
    owner: &str,
    cid: &str,
    meter: &ResourceMeter<'_>,
) -> Result<Option<Allocation>> {
    if !valid_cid(cid) {
        return Ok(None);
    }
    let statement = env
        .d1("DB")?
        .prepare(format!(
            "SELECT {COLUMNS} FROM code_contexts WHERE account_id=? AND container_id=?"
        ))
        .bind(&[text(owner), text(cid)])?;
    Ok(query(meter, statement)
        .await?
        .results::<Allocation>()?
        .into_iter()
        .next())
}

pub(crate) fn public(allocation: &Allocation, running: bool) -> serde_json::Value {
    serde_json::json!({"id":allocation.container_id,"object":"container","name":"Code Interpreter",
        "created_at":allocation.created_at_ms/1000,"last_active_at":allocation.last_used_at_ms/1000,
        "status":if running { "running" } else { "expired" },
        "memory_limit":match allocation.source_profile { RuntimeProfile::Basic1g=>"1g",RuntimeProfile::Standard4g=>"4g" },
        "expires_after":{"anchor":"last_active_at","minutes":20},
        "agents_cleanup_status":if allocation.allocation_released_at_ms.is_some() { "complete" } else if !running { "pending" } else { "not_requested" }})
}

pub(crate) async fn get(
    env: &Env,
    owner: &str,
    allocation: &str,
    meter: &ResourceMeter<'_>,
) -> Result<Option<Allocation>> {
    let statement = env
        .d1("DB")?
        .prepare(format!(
            "SELECT {COLUMNS} FROM code_contexts WHERE account_id=? AND allocation_id=?"
        ))
        .bind(&[text(owner), text(allocation)])?;
    Ok(query(meter, statement)
        .await?
        .results::<Allocation>()?
        .into_iter()
        .next())
}

pub(crate) async fn claim(
    env: &Env,
    allocation: &Allocation,
    capacity: u32,
    meter: &ResourceMeter<'_>,
) -> Result<()> {
    if allocation.state != ContextState::Reserved
        || allocation.container_id.is_some()
        || allocation.created_at_ms.checked_add(IDLE_MS) != Some(allocation.expires_at_ms)
        || allocation.last_used_at_ms != allocation.created_at_ms
        || capacity == 0
        || !opaque(&allocation.allocation_id)
        || !opaque(&allocation.source_session_id)
        || !opaque(&allocation.funding_run_id)
    {
        return Err(fail("invalid_code_context_allocation"));
    }
    let statement = env.d1("DB")?.prepare("INSERT INTO code_contexts(allocation_id,account_id,source_session_id,source_profile,funding_run_id,state,created_at_ms,last_used_at_ms,expires_at_ms) SELECT ?1,?2,?3,?4,?5,'reserved',CAST(?6 AS INTEGER),CAST(?6 AS INTEGER),CAST(?7 AS INTEGER) WHERE EXISTS(SELECT 1 FROM usage_reservations WHERE id=?5 AND account_id=?2 AND session_id=?3 AND state='reserved' AND execution_finished_at IS NULL) AND (EXISTS(SELECT 1 FROM code_contexts WHERE account_id=?2 AND source_session_id=?3 AND allocation_released_at_ms IS NULL) OR (SELECT COUNT(DISTINCT source_session_id) FROM code_contexts WHERE account_id=?2 AND allocation_released_at_ms IS NULL)<CAST(?8 AS INTEGER)) ON CONFLICT DO NOTHING")
        .bind(&[text(&allocation.allocation_id),text(&allocation.account_id),text(&allocation.source_session_id),text(allocation.source_profile.name()),text(&allocation.funding_run_id),text(&allocation.created_at_ms.to_string()),text(&allocation.expires_at_ms.to_string()),text(&capacity.to_string())])?;
    query(meter, statement).await?;
    let stored = get(
        env,
        &allocation.account_id,
        &allocation.allocation_id,
        meter,
    )
    .await?
    .ok_or_else(|| fail("code_context_capacity_exceeded"))?;
    if stored.source_session_id != allocation.source_session_id
        || stored.source_profile != allocation.source_profile
        || stored.funding_run_id != allocation.funding_run_id
        || stored.created_at_ms != allocation.created_at_ms
        || !matches!(stored.state, ContextState::Reserved | ContextState::Ready)
    {
        return Err(fail("code_context_allocation_conflict"));
    }
    Ok(())
}

pub(crate) async fn bind(
    env: &Env,
    owner: &str,
    allocation: &str,
    context: &OwnedContext,
    now: u64,
    meter: &ResourceMeter<'_>,
) -> Result<()> {
    if !valid_cid(&context.id) || !opaque(&context.daemon_boot) || !opaque(&context.policy_revision)
    {
        return Err(fail("invalid_code_context_identity"));
    }
    let expires = now
        .checked_add(IDLE_MS)
        .ok_or_else(|| fail("invalid_code_context_clock"))?;
    let statement = env.d1("DB")?.prepare("UPDATE code_contexts SET container_id=?1,daemon_boot_id=?2,policy_revision=?3,state='ready',last_used_at_ms=MAX(last_used_at_ms,CAST(?4 AS INTEGER)),expires_at_ms=MAX(expires_at_ms,CAST(?5 AS INTEGER)) WHERE allocation_id=?6 AND account_id=?7 AND source_session_id=?8 AND source_profile=?9 AND (state='reserved' OR (state='ready' AND container_id=?1 AND daemon_boot_id=?2 AND policy_revision=?3))")
        .bind(&[text(&context.id),text(&context.daemon_boot),text(&context.policy_revision),text(&now.to_string()),text(&expires.to_string()),text(allocation),text(owner),text(&context.source_session),text(context.profile.name())])?;
    query(meter, statement).await?;
    let stored = get(env, owner, allocation, meter)
        .await?
        .ok_or_else(|| fail("code_context_allocation_missing"))?;
    if stored.owned()? != *context {
        return Err(fail("code_context_identity_conflict"));
    }
    Ok(())
}

pub(crate) async fn touch(
    env: &Env,
    owner: &str,
    allocation: &str,
    used_at: u64,
    meter: &ResourceMeter<'_>,
) -> Result<()> {
    let expires = used_at
        .checked_add(IDLE_MS)
        .ok_or_else(|| fail("invalid_code_context_clock"))?;
    let statement = env.d1("DB")?.prepare("UPDATE code_contexts SET last_used_at_ms=MAX(last_used_at_ms,CAST(? AS INTEGER)),expires_at_ms=MAX(expires_at_ms,CAST(? AS INTEGER)) WHERE account_id=? AND allocation_id=? AND state='ready'")
        .bind(&[text(&used_at.to_string()),text(&expires.to_string()),text(owner),text(allocation)])?;
    query(meter, statement).await?;
    Ok(())
}

pub(crate) async fn end(
    env: &Env,
    owner: &str,
    allocation: &str,
    deleted: bool,
    now: u64,
    meter: &ResourceMeter<'_>,
) -> Result<()> {
    let state = if deleted { "deleted" } else { "expired" };
    let statement = env.d1("DB")?.prepare("UPDATE code_contexts SET state=?1,ended_at_ms=COALESCE(ended_at_ms,CAST(?2 AS INTEGER)) WHERE account_id=?3 AND allocation_id=?4 AND (state IN('reserved','ready') OR (?1='deleted' AND state='expired'))")
        .bind(&[text(state),text(&now.to_string()),text(owner),text(allocation)])?;
    query(meter, statement).await?;
    Ok(())
}

/// After source-confirmed cleanup, release capacity independently of any
/// unresolved financial observation. An elapsed TTL alone is not this proof.
pub(crate) async fn release_allocation(
    env: &Env,
    owner: &str,
    allocation: &str,
    now: u64,
    meter: &ResourceMeter<'_>,
) -> Result<()> {
    let statement = env.d1("DB")?.prepare("UPDATE code_contexts SET allocation_released_at_ms=CAST(? AS INTEGER) WHERE account_id=? AND allocation_id=? AND state IN('expired','deleted') AND allocation_released_at_ms IS NULL")
        .bind(&[text(&now.to_string()),text(owner),text(allocation)])?;
    query(meter, statement).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_context_ids_enter_owned_routing() {
        assert!(valid_cid(&format!("cntr_{}", "a".repeat(32))));
        for value in ["cntr_new", "cntr_../source", "", "session_123"] {
            assert!(!valid_cid(value));
        }
    }
}
