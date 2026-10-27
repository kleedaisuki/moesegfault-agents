//! Only confirmed no-charge diagnostics expire. Unknown paid replies never do.
use super::{StoredResponse, fail};
use crate::{
    image_artifacts::IoAttempt,
    resources::{ResourceMeter, ResourcePricing},
};
use agents_auth::billing::{ReservationState, Resource};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{Env, Result};

#[derive(Deserialize)]
struct Candidate {
    account_id: String,
    operation_id: String,
}
#[derive(Deserialize)]
struct Attempt {
    gc_attempt: u32,
}
#[derive(Deserialize)]
struct Lease {
    gc_attempt: u32,
    gc_lease_until_ms: u64,
    state: String,
}
#[derive(Deserialize, Serialize)]
struct Funding {
    owner: String,
    operation: String,
    attempt: u32,
    pricing: ResourcePricing,
}
#[derive(Deserialize)]
struct Previous {
    id: String,
    account_id: String,
    state: ReservationState,
    request_hash: String,
}
#[derive(Deserialize)]
struct Observations {
    total: u64,
    pending: u64,
}

#[derive(Deserialize)]
struct Frame {
    descriptor_json: String,
}

pub(crate) async fn run(env: &Env) -> Result<()> {
    reconcile_orphans(env).await?;
    let now = crate::util::now_ms();
    let rows=env.d1("DB")?.prepare("SELECT account_id,operation_id FROM provider_response_operations WHERE state IN ('diagnostic','deleting') AND expires_at_ms<=CAST(? AS INTEGER) AND gc_lease_until_ms<=CAST(? AS INTEGER) ORDER BY expires_at_ms,account_id,operation_id LIMIT 16")
        .bind(&[text(&now.to_string()),text(&now.to_string())])?.all().await?.results::<Candidate>()?;
    for row in rows {
        if let Err(error) = collect(env, &row).await {
            crate::util::emit(
                "provider.receipt_gc_deferred",
                serde_json::json!({"owner_id":row.account_id,"operation_id":row.operation_id,"stage":"diagnostic_cleanup","error_code":diagnostic_code(&error),"error_class":match &error {worker::Error::RustError(_) => "rust",_ => "platform"}}),
            );
        }
    }
    Ok(())
}

async fn collect(env: &Env, row: &Candidate) -> Result<()> {
    let now = crate::util::now_ms();
    let lease = load_lease(env, row)
        .await?
        .ok_or_else(|| fail("provider_receipt_gc_operation_missing"))?;
    if lease.gc_lease_until_ms > now || !matches!(lease.state.as_str(), "diagnostic" | "deleting") {
        return Ok(());
    }
    reconcile_previous(env, row, lease.gc_attempt).await?;
    let remaining:Option<serde_json::Value>=env.d1("DB")?.prepare("SELECT 1 AS live FROM provider_response_frames WHERE account_id=? AND operation_id=? AND deleted=0 LIMIT 1")
        .bind(&[text(&row.account_id),text(&row.operation_id)])?.first(None).await?;
    if remaining.is_none() {
        return super::release_capacity(env, &row.account_id, &row.operation_id).await;
    }
    let attempt:Option<Attempt>=env.d1("DB")?.prepare("UPDATE provider_response_operations SET state='deleting',gc_attempt=gc_attempt+1,gc_lease_until_ms=CAST(? AS INTEGER) WHERE account_id=? AND operation_id=? AND state IN ('diagnostic','deleting') AND expires_at_ms<=CAST(? AS INTEGER) AND gc_lease_until_ms<=CAST(? AS INTEGER) AND gc_attempt=CAST(? AS INTEGER) RETURNING gc_attempt")
        .bind(&[text(&now.saturating_add(300_000).to_string()),text(&row.account_id),text(&row.operation_id),text(&now.to_string()),text(&now.to_string()),text(&lease.gc_attempt.to_string())])?.first(None).await?;
    let Some(attempt) = attempt else {
        return Ok(());
    };
    let pricing = ResourcePricing::parse(&env.var("RESOURCE_PRICING_JSON")?.to_string())?;
    pricing.tariff(&Resource::R2Storage, "byte_ms")?;
    let budget = pricing
        .tariff(&Resource::R2Operation, "class_b")?
        .charge(96)
        .map_err(|_| fail("invalid_provider_receipt_gc_tariff"))?
        .amount_microusd
        .get();
    let run = run_id(&row.account_id, &row.operation_id, attempt.gc_attempt);
    let hash = serde_json::to_string(&Funding {
        owner: row.account_id.clone(),
        operation: row.operation_id.clone(),
        attempt: attempt.gc_attempt,
        pricing: pricing.clone(),
    })?;
    let hold = crate::metering::reserve(
        &env.d1("DB")?,
        &row.account_id,
        None,
        &run,
        &run,
        &hash,
        budget,
        (now / 1000) as i64,
    )
    .await?;
    if hold.state != ReservationState::Reserved {
        return Err(fail("provider_receipt_gc_already_settled"));
    }
    let current = load_lease(env, row)
        .await?
        .ok_or_else(|| fail("provider_receipt_gc_operation_missing"))?;
    if current.gc_attempt != attempt.gc_attempt
        || current.gc_lease_until_ms <= crate::util::now_ms()
        || current.state != "deleting"
    {
        reconcile_previous(env, row, attempt.gc_attempt).await?;
        return Err(fail("provider_receipt_gc_lease_lost"));
    }
    let meter = ResourceMeter::with_account_pricing(env, &row.account_id, &run, pricing)?;
    let frames=env.d1("DB")?.prepare("SELECT descriptor_json FROM provider_response_frames WHERE account_id=? AND operation_id=? AND deleted=0 ORDER BY receipt_id LIMIT 4")
        .bind(&[text(&row.account_id),text(&row.operation_id)])?.all().await?.results::<Frame>()?;
    let deletion = async {
        for (index, frame) in frames.iter().enumerate() {
            let receipt: StoredResponse = serde_json::from_str(&frame.descriptor_json)?;
            let event = format!("receipt_gc_frame_{index}");
            let io = IoAttempt {
                event: &event,
                attempt: attempt.gc_attempt,
            };
            super::delete(
                env,
                &meter,
                &row.account_id,
                &row.operation_id,
                &receipt,
                &io,
            )
            .await?;
        }
        Ok::<(), worker::Error>(())
    }
    .await;
    // Even failed free DELETEs have real, measured HEAD costs. Settle those;
    // an actually pending observation still keeps its original financial hold.
    let settlement = meter.settle_run().await;
    deletion?;
    settlement?;
    super::release_capacity(env, &row.account_id, &row.operation_id).await?;
    crate::util::emit(
        "provider.receipt_gc_completed",
        serde_json::json!({"owner_id":row.account_id,"operation_id":row.operation_id,"attempt":attempt.gc_attempt,"frames":frames.len()}),
    );
    Ok(())
}

fn run_id(owner: &str, operation: &str, attempt: u32) -> String {
    format!(
        "receipt-gc:{}:{attempt}",
        super::hash(format!("{owner}:{operation}").as_bytes())
    )
}
async fn load_lease(env: &Env, row: &Candidate) -> Result<Option<Lease>> {
    env.d1("DB")?.prepare("SELECT gc_attempt,gc_lease_until_ms,state FROM provider_response_operations WHERE account_id=? AND operation_id=?")
        .bind(&[text(&row.account_id),text(&row.operation_id)])?.first(None).await
}
async fn reconcile_previous(env: &Env, row: &Candidate, attempt: u32) -> Result<()> {
    if attempt == 0 {
        return Ok(());
    }
    let previous:Option<Previous>=env.d1("DB")?.prepare("SELECT id,account_id,state,request_hash FROM usage_reservations WHERE id=? AND account_id=?")
        .bind(&[text(&run_id(&row.account_id,&row.operation_id,attempt)),text(&row.account_id)])?.first(None).await?;
    if let Some(previous) = previous {
        close_previous(env, &previous).await?;
    }
    Ok(())
}
// Also discover a stale invocation's reservation created after a newer attempt
// already advanced or released capacity. The immutable funding snapshot is its
// discovery record; operation completion must not orphan that original money.
async fn reconcile_orphans(env: &Env) -> Result<()> {
    let rows=env.d1("DB")?.prepare("SELECT id,account_id,state,request_hash FROM usage_reservations WHERE state='reserved' AND id LIKE 'receipt-gc:%' AND NOT EXISTS(SELECT 1 FROM resource_observations WHERE run_id=usage_reservations.id AND status='pending') ORDER BY updated_at,id LIMIT 16")
        .all().await?.results::<Previous>()?;
    for previous in rows {
        if let Err(error) = close_previous(env, &previous).await {
            crate::util::emit(
                "provider.receipt_gc_reconciliation_deferred",
                serde_json::json!({"owner_id":previous.account_id,"run_id":previous.id,"error_code":diagnostic_code(&error)}),
            );
        }
    }
    Ok(())
}
async fn close_previous(env: &Env, previous: &Previous) -> Result<()> {
    if previous.state != ReservationState::Reserved {
        return Ok(());
    }
    let funding: Funding = serde_json::from_str(&previous.request_hash)?;
    if funding.owner != previous.account_id
        || run_id(&funding.owner, &funding.operation, funding.attempt) != previous.id
    {
        return Err(fail("provider_receipt_gc_funding_conflict"));
    }
    let row = Candidate {
        account_id: funding.owner.clone(),
        operation_id: funding.operation.clone(),
    };
    let lease = load_lease(env, &row)
        .await?
        .ok_or_else(|| fail("provider_receipt_gc_operation_missing"))?;
    let now = crate::util::now_ms();
    if lease.gc_attempt == funding.attempt
        && lease.gc_lease_until_ms > now
        && lease.state != "released"
    {
        return Err(fail("provider_receipt_gc_lease_active"));
    }
    let counts:Observations=env.d1("DB")?.prepare("SELECT COUNT(*) AS total,COALESCE(SUM(CASE WHEN status='pending' THEN 1 ELSE 0 END),0) AS pending FROM resource_observations WHERE run_id=? AND account_id=?")
        .bind(&[text(&previous.id),text(&funding.owner)])?.first(None).await?.ok_or_else(||fail("provider_receipt_gc_observation_missing"))?;
    if counts.pending != 0 {
        return Err(fail("provider_receipt_gc_reconciliation_pending"));
    }
    if counts.total != 0 {
        return ResourceMeter::with_account_pricing(
            env,
            &funding.owner,
            &previous.id,
            funding.pricing,
        )?
        .settle_run()
        .await
        .map(|_| ());
    }
    // Atomic with observation admission: if a paused old invocation has already
    // declared a HEAD this cannot release; if this wins its later declaration
    // cannot pass the reservation's Reserved-state fence.
    env.d1("DB")?.prepare("UPDATE usage_reservations SET state='released',updated_at=CAST(?1 AS INTEGER) WHERE id=?2 AND account_id=?3 AND state='reserved' AND NOT EXISTS(SELECT 1 FROM resource_observations WHERE run_id=?2 AND account_id=?3) AND EXISTS(SELECT 1 FROM provider_response_operations WHERE account_id=?3 AND operation_id=?4 AND (gc_attempt>CAST(?5 AS INTEGER) OR gc_lease_until_ms<=CAST(?1 AS INTEGER) OR state='released'))")
        .bind(&[text(&now.to_string()),text(&previous.id),text(&funding.owner),text(&funding.operation),text(&funding.attempt.to_string())])?.run().await?;
    #[derive(Deserialize)]
    struct State {
        state: ReservationState,
    }
    let state: State = env
        .d1("DB")?
        .prepare("SELECT state FROM usage_reservations WHERE id=? AND account_id=?")
        .bind(&[text(&previous.id), text(&funding.owner)])?
        .first(None)
        .await?
        .ok_or_else(|| fail("provider_receipt_gc_funding_missing"))?;
    if state.state == ReservationState::Reserved {
        return Err(fail("provider_receipt_gc_reconciliation_pending"));
    }
    Ok(())
}
fn text(s: &str) -> JsValue {
    JsValue::from_str(s)
}

// Keep our precise stage/chunk codes; never format arbitrary platform messages.
fn diagnostic_code(error: &worker::Error) -> &str {
    match error {
        worker::Error::RustError(code)
            if code.len() <= 128
                && (code.starts_with("provider_receipt_")
                    || code.starts_with("invalid_provider_receipt_"))
                && code.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b':' | b'=')
                }) =>
        {
            code
        }
        _ => "provider_receipt_cleanup_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gc_diagnostics_keep_structured_chunk_failures_without_raw_messages() {
        assert_eq!(
            diagnostic_code(&fail("provider_receipt_delete_failed:chunk=23")),
            "provider_receipt_delete_failed:chunk=23"
        );
        assert_eq!(
            diagnostic_code(&fail("provider_receipt_secret token=ABC")),
            "provider_receipt_cleanup_failed"
        );
        assert_eq!(
            diagnostic_code(&fail("upstream arbitrary response")),
            "provider_receipt_cleanup_failed"
        );
    }
}
