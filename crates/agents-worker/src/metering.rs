//! D1-trigger-enforced accounting. Monetary bindings use decimal strings, not JS doubles.

use agents_auth::billing::{Charge, MicroUsd, ReservationState, ResourceTariff};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{D1Database, Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReservationRow {
    pub id: String,
    pub account_id: String,
    pub request_hash: String,
    #[serde(deserialize_with = "decimal_money")]
    pub reserved_microusd: MicroUsd,
    #[serde(deserialize_with = "optional_decimal_money")]
    pub settled_microusd: Option<MicroUsd>,
    pub state: ReservationState,
}

fn decimal_money<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<MicroUsd, D::Error> {
    let value = String::deserialize(d)?
        .parse::<i64>()
        .map_err(serde::de::Error::custom)?;
    MicroUsd::new(value).map_err(serde::de::Error::custom)
}

fn optional_decimal_money<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<MicroUsd>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|s| {
            let value = s.parse::<i64>().map_err(serde::de::Error::custom)?;
            MicroUsd::new(value).map_err(serde::de::Error::custom)
        })
        .transpose()
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}

const ROW: &str = "SELECT id, account_id, request_hash, CAST(reserved_microusd AS TEXT) AS reserved_microusd, CAST(settled_microusd AS TEXT) AS settled_microusd, state FROM usage_reservations";

async fn read(db: &D1Database, id: &str) -> Result<ReservationRow> {
    db.prepare(format!("{ROW} WHERE id = ?"))
        .bind(&[text(id)])?
        .first(None)
        .await?
        .ok_or_else(|| fail("reservation_not_found"))
}

/// The unique account/key pair replays the original reservation, never creates credit.
#[allow(clippy::too_many_arguments)]
pub async fn reserve(
    db: &D1Database,
    account: &str,
    session: Option<&str>,
    id: &str,
    key: &str,
    hash: &str,
    amount: i64,
    now: i64,
) -> Result<ReservationRow> {
    reserve_inner(db, account, session, id, key, hash, amount, now, None).await
}

/// Run admission and budget hold share one serialized INSERT, eliminating count-then-act races.
#[allow(clippy::too_many_arguments)]
pub async fn reserve_run(
    db: &D1Database,
    account: &str,
    session: Option<&str>,
    id: &str,
    key: &str,
    hash: &str,
    amount: i64,
    now: i64,
    max_runs: u32,
) -> Result<ReservationRow> {
    if !key.starts_with("run:") {
        return Err(fail("invalid_run_key"));
    }
    let row = reserve_inner(
        db,
        account,
        session,
        id,
        key,
        hash,
        amount,
        now,
        Some(max_runs),
    )
    .await?;
    #[derive(Deserialize)]
    struct Execution {
        execution_finished_at: Option<String>,
    }
    let execution: Execution = db.prepare("SELECT CAST(execution_finished_at AS TEXT) AS execution_finished_at FROM usage_reservations WHERE id=? AND account_id=?")
        .bind(&[text(&row.id),text(account)])?.first(None).await?
        .ok_or_else(||fail("reservation_not_found"))?;
    if execution.execution_finished_at.is_some() {
        return Err(fail("execution_already_finished"));
    }
    Ok(row)
}

/// Release execution capacity without changing a still-uncertain financial hold.
pub async fn end_execution(db: &D1Database, account: &str, run: &str, ended_ms: u64) -> Result<()> {
    if ended_ms > i64::MAX as u64 {
        return Err(fail("invalid_execution_timestamp"));
    }
    db.prepare("UPDATE usage_reservations SET execution_finished_at=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND idempotency_key LIKE 'run:%' AND execution_finished_at IS NULL")
        .bind(&[text(&ended_ms.to_string()),text(run),text(account)])?.run().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn reserve_inner(
    db: &D1Database,
    account: &str,
    session: Option<&str>,
    id: &str,
    key: &str,
    hash: &str,
    amount: i64,
    now: i64,
    max_runs: Option<u32>,
) -> Result<ReservationRow> {
    MicroUsd::new(amount).map_err(|_| fail("invalid_amount"))?;
    if key.is_empty() || hash.is_empty() {
        return Err(fail("invalid_idempotency_key"));
    }
    db.prepare("INSERT INTO usage_reservations (id, account_id, session_id, idempotency_key, request_hash, reserved_microusd, state, created_at, updated_at) SELECT ?,?,?,?,?,CAST(? AS INTEGER),'reserved',CAST(? AS INTEGER),CAST(? AS INTEGER) WHERE NOT EXISTS (SELECT 1 FROM usage_reservations WHERE account_id=? AND idempotency_key=?) AND (? IS NULL OR (SELECT COUNT(*) FROM usage_reservations WHERE account_id=? AND state='reserved' AND execution_finished_at IS NULL AND idempotency_key LIKE 'run:%') < CAST(? AS INTEGER))")
        .bind(&[text(id),text(account),session.map(text).unwrap_or(JsValue::NULL),text(key),text(hash),text(&amount.to_string()),text(&now.to_string()),text(&now.to_string()),text(account),text(key),max_runs.map(|n|text(&n.to_string())).unwrap_or(JsValue::NULL),text(account),max_runs.map(|n|text(&n.to_string())).unwrap_or(JsValue::NULL)])?
        .run().await?;
    let row: ReservationRow = db
        .prepare(format!("{ROW} WHERE account_id=? AND idempotency_key=?"))
        .bind(&[text(account), text(key)])?
        .first(None)
        .await?
        .ok_or_else(|| fail("run_limit_exceeded"))?;
    if row.request_hash != hash || row.reserved_microusd.get() != amount {
        return Err(fail("idempotency_conflict"));
    }
    Ok(row)
}

#[derive(Deserialize)]
struct ChargeRow {
    resource: String,
    quantity: String,
    unit: String,
    amount_microusd: String,
    price_version: String,
    provider_request_id: Option<String>,
}

/// Meter each upstream/resource event once; conflicting retries are rejected.
pub async fn append(
    db: &D1Database,
    reservation: &str,
    event: &str,
    charge: &Charge,
    now: i64,
) -> Result<()> {
    if event.is_empty() || charge.quantity > i64::MAX as u64 {
        return Err(fail("invalid_usage_event"));
    }
    let resource = serde_json::to_value(&charge.resource)?
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let quantity = charge.quantity.to_string();
    let amount = charge.amount_microusd.get().to_string();
    let id = uuid::Uuid::new_v4().to_string();
    db.prepare("INSERT INTO usage_ledger (id,reservation_id,event_key,resource,quantity,unit,amount_microusd,price_version,provider_request_id,created_at) SELECT ?,?,?,?,CAST(? AS INTEGER),?,CAST(? AS INTEGER),?,?,CAST(? AS INTEGER) WHERE NOT EXISTS (SELECT 1 FROM usage_ledger WHERE reservation_id=? AND event_key=?)")
        .bind(&[text(&id),text(reservation),text(event),text(&resource),text(&quantity),text(&charge.unit),text(&amount),text(&charge.price_version),charge.provider_request_id.as_deref().map(text).unwrap_or(JsValue::NULL),text(&now.to_string()),text(reservation),text(event)])?
        .run().await?;
    let row: ChargeRow = db.prepare("SELECT resource,CAST(quantity AS TEXT) AS quantity,unit,CAST(amount_microusd AS TEXT) AS amount_microusd,price_version,provider_request_id FROM usage_ledger WHERE reservation_id=? AND event_key=?")
        .bind(&[text(reservation),text(event)])?.first(None).await?
        .ok_or_else(|| fail("usage_event_not_found"))?;
    if row.resource != resource
        || row.quantity != quantity
        || row.unit != charge.unit
        || row.amount_microusd != amount
        || row.price_version != charge.price_version
        || row.provider_request_id != charge.provider_request_id
    {
        return Err(fail("idempotency_conflict"));
    }
    Ok(())
}

/// Record an observed resource quantity against a pinned, reviewed tariff; never infer usage.
pub async fn record_resource(
    db: &D1Database,
    reservation: &str,
    event: &str,
    quantity: u64,
    tariff: &ResourceTariff,
    now: i64,
) -> Result<()> {
    let charge = tariff
        .charge(quantity)
        .map_err(|_| fail("invalid_resource_tariff"))?;
    append(db, reservation, event, &charge, now).await
}

pub async fn settle(db: &D1Database, id: &str, now: i64) -> Result<ReservationRow> {
    db.prepare("UPDATE usage_reservations SET state='settled',settled_microusd=(SELECT COALESCE(SUM(amount_microusd),0) FROM usage_ledger WHERE reservation_id=?),updated_at=CAST(? AS INTEGER) WHERE id=? AND state='reserved'")
        .bind(&[text(id),text(&now.to_string()),text(id)])?.run().await?;
    let row = read(db, id).await?;
    if row.state != ReservationState::Settled {
        return Err(fail("reservation_finalized"));
    }
    Ok(row)
}

/// Only reservations with no recorded usage may be released. Unknown provider outcomes stay held.
pub async fn release(db: &D1Database, id: &str, now: i64) -> Result<ReservationRow> {
    db.prepare("UPDATE usage_reservations SET state='released',updated_at=CAST(? AS INTEGER) WHERE id=? AND state='reserved'")
        .bind(&[text(&now.to_string()),text(id)])?.run().await?;
    let row = read(db, id).await?;
    if row.state != ReservationState::Released {
        return Err(fail("reservation_finalized"));
    }
    Ok(row)
}
