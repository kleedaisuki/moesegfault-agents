//! Only confirmed no-charge diagnostics expire. Unknown paid replies never do.
use super::{StoredResponse, fail};
use crate::{
    image_artifacts::IoAttempt,
    resources::{ResourceMeter, ResourcePricing},
};
use agents_auth::billing::{ReservationState, Resource};
use serde::Deserialize;
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
struct Frame {
    descriptor_json: String,
}

pub(super) async fn run(env: &Env) -> Result<()> {
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
    let attempt:Option<Attempt>=env.d1("DB")?.prepare("UPDATE provider_response_operations SET state='deleting',gc_attempt=gc_attempt+1,gc_lease_until_ms=CAST(? AS INTEGER) WHERE account_id=? AND operation_id=? AND state IN ('diagnostic','deleting') AND expires_at_ms<=CAST(? AS INTEGER) AND gc_lease_until_ms<=CAST(? AS INTEGER) RETURNING gc_attempt")
        .bind(&[text(&now.saturating_add(300_000).to_string()),text(&row.account_id),text(&row.operation_id),text(&now.to_string()),text(&now.to_string())])?.first(None).await?;
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
    let run = format!(
        "receipt-gc:{}:{}",
        super::hash(format!("{}:{}", row.account_id, row.operation_id).as_bytes()),
        attempt.gc_attempt
    );
    let hash = serde_json::to_string(
        &serde_json::json!({"owner":row.account_id,"operation":row.operation_id,"attempt":attempt.gc_attempt,"pricing":pricing}),
    )?;
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
    let meter = ResourceMeter::with_account_pricing(env, &row.account_id, &run, pricing)?;
    let frames=env.d1("DB")?.prepare("SELECT descriptor_json FROM provider_response_frames WHERE account_id=? AND operation_id=? AND deleted=0 ORDER BY receipt_id LIMIT 4")
        .bind(&[text(&row.account_id),text(&row.operation_id)])?.all().await?.results::<Frame>()?;
    meter
        .record_observation("receipt_gc_attempt", Resource::R2Operation, "class_b", 0)
        .await?;
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
