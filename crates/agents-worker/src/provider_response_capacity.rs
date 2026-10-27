//! Owner-scoped recovery capacity; these are not user-uploaded Space objects.
use super::{MAX_ENCRYPTED_RESPONSE_BYTES, StoredResponse, fail};
use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{Env, Result};

#[derive(Clone, Copy)]
pub(crate) enum Retention {
    Recovery,
    Diagnostic,
}

#[derive(Deserialize)]
struct Operation {
    session_id: Option<String>,
    response_id: Option<String>,
    max_frames: u16,
    state: String,
    capacity_bytes: u64,
}

pub(crate) async fn reserve_capacity(
    env: &Env,
    owner: &str,
    session: Option<&str>,
    response: Option<&str>,
    operation: &str,
    max_frames: u16,
) -> Result<()> {
    super::check_identity(owner, operation)?;
    if !(1..=4).contains(&max_frames) {
        return Err(fail("invalid_provider_receipt_capacity"));
    }
    let bytes = MAX_ENCRYPTED_RESPONSE_BYTES as u64 * u64::from(max_frames);
    let limit = crate::space::owner_limits(env, owner).await?.bytes;
    env.d1("DB")?.prepare("INSERT INTO provider_response_operations(account_id,operation_id,session_id,response_id,max_frames,capacity_bytes,state,updated_at_ms) SELECT ?1,?2,?3,?4,CAST(?5 AS INTEGER),CAST(?6 AS INTEGER),'reserved',CAST(?7 AS INTEGER) WHERE (SELECT COALESCE(SUM(capacity_bytes),0) FROM provider_response_operations WHERE account_id=?1)+CAST(?6 AS INTEGER)<=CAST(?8 AS INTEGER) ON CONFLICT(account_id,operation_id) DO NOTHING")
        .bind(&[text(owner),text(operation),optional(session),optional(response),text(&max_frames.to_string()),text(&bytes.to_string()),text(&crate::util::now_ms().to_string()),text(&limit.to_string())])?.run().await?;
    let row:Operation=env.d1("DB")?.prepare("SELECT session_id,response_id,max_frames,state,capacity_bytes FROM provider_response_operations WHERE account_id=? AND operation_id=?")
        .bind(&[text(owner),text(operation)])?.first(None).await?.ok_or_else(||fail("provider_receipt_capacity_exceeded"))?;
    if row.session_id.as_deref() != session
        || row.response_id.as_deref() != response
        || row.max_frames != max_frames
        || row.state != "reserved"
        || row.capacity_bytes != bytes
    {
        return Err(fail("provider_receipt_capacity_conflict"));
    }
    Ok(())
}

pub(crate) async fn register_prepared(
    env: &Env,
    owner: &str,
    operation: &str,
    receipt: &StoredResponse,
) -> Result<()> {
    receipt.validate(owner, operation)?;
    let id = receipt.id()?;
    let descriptor = serde_json::to_string(receipt)?;
    let existing = lookup(env, owner, operation, &id).await?;
    if let Some(existing) = existing {
        if existing != *receipt {
            return Err(fail("provider_receipt_descriptor_conflict"));
        }
        let live:Option<serde_json::Value>=env.d1("DB")?.prepare("SELECT 1 AS live FROM provider_response_frames f JOIN provider_response_operations o USING(account_id,operation_id) WHERE f.account_id=? AND f.operation_id=? AND f.receipt_id=? AND f.deleted=0 AND o.state IN ('reserved','recovery','diagnostic')")
            .bind(&[text(owner),text(operation),text(&id)])?.first(None).await?;
        return live
            .map(|_| ())
            .ok_or_else(|| fail("provider_receipt_retention_closed"));
    }
    env.d1("DB")?.prepare("INSERT INTO provider_response_frames(account_id,operation_id,receipt_id,descriptor_json,storage_bytes) VALUES(?,?,?,?,CAST(? AS INTEGER)) ON CONFLICT DO NOTHING")
        .bind(&[text(owner),text(operation),text(&id),text(&descriptor),text(&receipt.storage_bytes().to_string())])?.run().await?;
    Ok(())
}

pub(crate) async fn lookup(
    env: &Env,
    owner: &str,
    operation: &str,
    id: &str,
) -> Result<Option<StoredResponse>> {
    if !super::valid_hash(id) {
        return Err(fail("invalid_provider_receipt_id"));
    }
    #[derive(Deserialize)]
    struct Row {
        descriptor_json: String,
    }
    let row:Option<Row>=env.d1("DB")?.prepare("SELECT descriptor_json FROM provider_response_frames WHERE account_id=? AND operation_id=? AND receipt_id=? AND deleted=0")
        .bind(&[text(owner),text(operation),text(id)])?.first(None).await?;
    row.map(|row| {
        let receipt: StoredResponse = serde_json::from_str(&row.descriptor_json)?;
        receipt.validate(owner, operation)?;
        if receipt.id()? != id {
            return Err(fail("provider_receipt_descriptor_conflict"));
        }
        Ok(receipt)
    })
    .transpose()
}

pub(crate) async fn finalize_capacity(
    env: &Env,
    owner: &str,
    operation: &str,
    retention: Retention,
) -> Result<()> {
    let (state, expiry) = match retention {
        Retention::Recovery => ("recovery", JsValue::NULL),
        Retention::Diagnostic => (
            "diagnostic",
            text(&crate::util::now_ms().saturating_add(86_400_000).to_string()),
        ),
    };
    env.d1("DB")?.prepare("UPDATE provider_response_operations SET capacity_bytes=(SELECT COALESCE(SUM(storage_bytes),0) FROM provider_response_frames WHERE account_id=?1 AND operation_id=?2 AND deleted=0),state=?3,expires_at_ms=CAST(?4 AS INTEGER),updated_at_ms=CAST(?5 AS INTEGER) WHERE account_id=?1 AND operation_id=?2 AND state='reserved'")
        .bind(&[text(owner),text(operation),text(state),expiry,text(&crate::util::now_ms().to_string())])?.run().await?;
    Ok(())
}

pub(crate) async fn release_capacity(env: &Env, owner: &str, operation: &str) -> Result<()> {
    env.d1("DB")?.prepare("UPDATE provider_response_operations SET state='released',capacity_bytes=0,expires_at_ms=NULL,updated_at_ms=CAST(? AS INTEGER) WHERE account_id=? AND operation_id=?")
        .bind(&[text(&crate::util::now_ms().to_string()),text(owner),text(operation)])?.run().await?;
    Ok(())
}

pub(super) async fn deleted(
    env: &Env,
    owner: &str,
    operation: &str,
    receipt: &StoredResponse,
) -> Result<()> {
    env.d1("DB")?.prepare("UPDATE provider_response_frames SET deleted=1 WHERE account_id=? AND operation_id=? AND receipt_id=?")
        .bind(&[text(owner),text(operation),text(&receipt.id()?)])?.run().await?;
    Ok(())
}
fn text(s: &str) -> JsValue {
    JsValue::from_str(s)
}
fn optional(s: Option<&str>) -> JsValue {
    s.map(text).unwrap_or(JsValue::NULL)
}
