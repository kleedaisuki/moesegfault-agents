//! Retained R2 data is billed independently of active runs. Debt never deletes data.
//! A durable, tariff-pinned interval journal precedes any funding attempt; crash
//! retries settle that same interval before advancing its monotonic object clock.

use agents_auth::billing::{Resource, ResourceTariff};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{D1Database, Env, Error, Result};

use crate::{metering, resources::ResourcePricing};

const BATCH_LIMIT: usize = 64;
const PERIOD_MS: u64 = 900_000;
const CURSOR_KEY: &str = "r2_storage_cursor";

/// Reserve maintenance I/O independently of an already-settled agent run.
/// Callers must persist/reuse `id` across retries and settle through ResourceMeter.
pub async fn maintenance_reservation(
    env: &Env,
    owner: &str,
    session: &str,
    id: &str,
) -> Result<metering::ReservationRow> {
    let meter = crate::resources::ResourceMeter::new(env, owner, session, id)?;
    meter.require_tariff(&Resource::R2Operation, "class_a")?;
    meter.require_tariff(&Resource::R2Operation, "class_b")?;
    let amount = env
        .var("RUN_RESOURCE_RESERVATION_MICROUSD")?
        .to_string()
        .parse::<i64>()
        .map_err(|_| fail("invalid_resource_reservation"))?;
    if amount <= 0 || id.is_empty() {
        return Err(fail("resource_reservation_required"));
    }
    let hash = format!("{}:{owner}:{}:{session}:{id}", owner.len(), session.len());
    metering::reserve(
        &env.d1("DB")?,
        owner,
        Some(session),
        id,
        &format!("maintenance:{id}"),
        &hash,
        amount,
        crate::util::now_ms() as i64,
    )
    .await
}

#[derive(Deserialize)]
struct Object {
    object_key: String,
    account_id: String,
    session_id: String,
    size_bytes: String,
    billed_through_ms: u64,
    removed_at_ms: Option<u64>,
}

#[derive(Deserialize)]
struct Interval {
    interval_start_ms: u64,
    interval_end_ms: u64,
    quantity: String,
    amount_microusd: String,
    tariff_json: String,
    reservation_id: String,
}

#[derive(Deserialize)]
struct State {
    value: String,
}

/// Called only after a successful PUT (or using a write-ahead storage lifecycle).
/// For mutable physical keys, pass a generation-qualified billing identity and
/// close the previous generation only after the overwrite succeeds.
pub async fn register_storage_object(
    env: &Env,
    owner: &str,
    session: &str,
    key: &str,
    bytes: u64,
    created_ms: u64,
) -> Result<()> {
    if key.is_empty() || bytes > i64::MAX as u64 {
        return Err(fail("invalid_storage_observation"));
    }
    let db = env.d1("DB")?;
    db.prepare("INSERT INTO resource_storage_objects(object_key,account_id,session_id,size_bytes,retained_from_ms,billed_through_ms,removed_at_ms,status) VALUES(?,?,?, ?,CAST(? AS INTEGER),CAST(? AS INTEGER),NULL,'active') ON CONFLICT(object_key) DO NOTHING")
        .bind(&[text(key),text(owner),text(session),text(&bytes.to_string()),text(&created_ms.to_string()),text(&created_ms.to_string())])?.run().await?;
    let object: Object = db.prepare("SELECT object_key,account_id,session_id,size_bytes,billed_through_ms,removed_at_ms FROM resource_storage_objects WHERE object_key=?")
        .bind(&[text(key)])?.first(None).await?.ok_or_else(||fail("storage_object_missing"))?;
    if object.account_id != owner
        || object.session_id != session
        || object.size_bytes != bytes.to_string()
    {
        return Err(fail("storage_object_conflict"));
    }
    Ok(())
}

/// Call after actual deletion, not merely when a snapshot stops being current.
pub async fn remove_storage_object(env: &Env, key: &str, removed_ms: u64) -> Result<()> {
    env.d1("DB")?.prepare("UPDATE resource_storage_objects SET removed_at_ms=CAST(? AS INTEGER),status='removed' WHERE object_key=? AND removed_at_ms IS NULL AND retained_from_ms<=CAST(? AS INTEGER) AND billed_through_ms<=CAST(? AS INTEGER)")
        .bind(&[text(&removed_ms.to_string()),text(key),text(&removed_ms.to_string()),text(&removed_ms.to_string())])?.run().await?;
    Ok(())
}

pub async fn run(env: Env) -> Result<()> {
    let pricing = ResourcePricing::parse(&env.var("RESOURCE_PRICING_JSON")?.to_string())?;
    let tariff = pricing.tariff(&Resource::R2Storage, "byte_ms")?;
    let now = crate::util::now_ms();
    let cutoff = now - now % PERIOD_MS;
    let db = env.d1("DB")?;
    let cursor = db
        .prepare("SELECT value FROM resource_system_state WHERE key=?")
        .bind(&[text(CURSOR_KEY)])?
        .first::<State>(None)
        .await?
        .map(|row| row.value)
        .unwrap_or_default();
    let objects = db.prepare("SELECT object_key,account_id,session_id,size_bytes,billed_through_ms,removed_at_ms FROM resource_storage_objects WHERE object_key>? AND billed_through_ms<MIN(CAST(? AS INTEGER),COALESCE(removed_at_ms,CAST(? AS INTEGER))) ORDER BY object_key LIMIT ?")
        .bind(&[text(&cursor),text(&cutoff.to_string()),text(&cutoff.to_string()),text(&BATCH_LIMIT.to_string())])?
        .all().await?.results::<Object>()?;
    for object in &objects {
        let interval = interval(&db, object, cutoff, tariff).await?;
        match charge(&db, object, &interval, now).await {
            Ok(()) => complete_interval(&db, object, &interval, now).await?,
            Err(_) => {
                db.prepare("INSERT INTO resource_account_blocks(account_id,reason,updated_at) VALUES(?,'storage_debt',CAST(? AS INTEGER)) ON CONFLICT(account_id) DO UPDATE SET reason='storage_debt',updated_at=excluded.updated_at")
                    .bind(&[text(&object.account_id),text(&now.to_string())])?.run().await?;
                crate::util::emit(
                    "storage.reconciliation_pending",
                    serde_json::json!({"account_id":object.account_id,"session_id":object.session_id}),
                );
            }
        }
    }
    let next = if objects.len() == BATCH_LIMIT {
        objects
            .last()
            .map(|object| object.object_key.as_str())
            .unwrap_or("")
    } else {
        ""
    };
    db.prepare("INSERT INTO resource_system_state(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(&[text(CURSOR_KEY),text(next)])?.run().await?;
    Ok(())
}

async fn interval(
    db: &D1Database,
    object: &Object,
    cutoff: u64,
    tariff: &ResourceTariff,
) -> Result<Interval> {
    if let Some(interval) = pending(db, &object.object_key).await? {
        return Ok(interval);
    }
    let bytes = object
        .size_bytes
        .parse::<u64>()
        .map_err(|_| fail("invalid_storage_size"))?;
    let (end, quantity) = interval_quantity(
        bytes,
        object.billed_through_ms,
        cutoff.min(object.removed_at_ms.unwrap_or(cutoff)),
    )?;
    let charge = tariff
        .charge(quantity)
        .map_err(|_| fail("invalid_resource_tariff"))?;
    let id = interval_id(&object.object_key, object.billed_through_ms, end);
    db.prepare("INSERT INTO resource_storage_deficits(object_key,account_id,session_id,interval_start_ms,interval_end_ms,quantity,amount_microusd,price_version,tariff_json,reservation_id,status) SELECT ?,?,?,CAST(? AS INTEGER),CAST(? AS INTEGER),?,?,?,?,?,'pending' WHERE EXISTS(SELECT 1 FROM resource_storage_objects WHERE object_key=? AND billed_through_ms=CAST(? AS INTEGER)) AND NOT EXISTS(SELECT 1 FROM resource_storage_deficits WHERE object_key=? AND status='pending') ON CONFLICT DO NOTHING")
        .bind(&[text(&object.object_key),text(&object.account_id),text(&object.session_id),text(&object.billed_through_ms.to_string()),text(&end.to_string()),text(&quantity.to_string()),text(&charge.amount_microusd.get().to_string()),text(&tariff.price_version),text(&serde_json::to_string(tariff)?),text(&id),text(&object.object_key),text(&object.billed_through_ms.to_string()),text(&object.object_key)])?
        .run().await?;
    pending(db, &object.object_key)
        .await?
        .ok_or_else(|| fail("storage_interval_race"))
}

async fn pending(db: &D1Database, key: &str) -> Result<Option<Interval>> {
    db.prepare("SELECT interval_start_ms,interval_end_ms,quantity,amount_microusd,tariff_json,reservation_id FROM resource_storage_deficits WHERE object_key=? AND status='pending' ORDER BY interval_start_ms LIMIT 1")
        .bind(&[text(key)])?.first(None).await
}

async fn charge(db: &D1Database, object: &Object, interval: &Interval, now: u64) -> Result<()> {
    let quantity = interval
        .quantity
        .parse::<u64>()
        .map_err(|_| fail("invalid_storage_interval"))?;
    let amount = interval
        .amount_microusd
        .parse::<i64>()
        .map_err(|_| fail("invalid_storage_interval"))?;
    let tariff: ResourceTariff = serde_json::from_str(&interval.tariff_json)?;
    let id = &interval.reservation_id;
    metering::reserve(
        db,
        &object.account_id,
        Some(&object.session_id),
        id,
        &format!("retention:{id}"),
        id,
        amount,
        now as i64,
    )
    .await?;
    metering::record_resource(db, id, "retention", quantity, &tariff, now as i64).await?;
    metering::settle(db, id, now as i64).await?;
    Ok(())
}

async fn complete_interval(
    db: &D1Database,
    object: &Object,
    interval: &Interval,
    now: u64,
) -> Result<()> {
    let statements = vec![
        db.prepare("UPDATE resource_storage_objects SET billed_through_ms=CAST(? AS INTEGER) WHERE object_key=? AND billed_through_ms=CAST(? AS INTEGER) AND EXISTS(SELECT 1 FROM usage_reservations WHERE id=? AND state='settled')")
            .bind(&[text(&interval.interval_end_ms.to_string()),text(&object.object_key),text(&interval.interval_start_ms.to_string()),text(&interval.reservation_id)])?,
        db.prepare("UPDATE resource_storage_deficits SET status='settled' WHERE object_key=? AND reservation_id=? AND EXISTS(SELECT 1 FROM resource_storage_objects WHERE object_key=? AND billed_through_ms>=CAST(? AS INTEGER))")
            .bind(&[text(&object.object_key),text(&interval.reservation_id),text(&object.object_key),text(&interval.interval_end_ms.to_string())])?,
        db.prepare("DELETE FROM resource_account_blocks WHERE account_id=? AND reason='storage_debt' AND NOT EXISTS(SELECT 1 FROM resource_storage_deficits WHERE account_id=? AND status='pending')")
            .bind(&[text(&object.account_id),text(&object.account_id)])?,
    ];
    db.batch(statements).await?;
    crate::util::emit(
        "storage.reconciled",
        serde_json::json!({"account_id":object.account_id,"session_id":object.session_id,"timestamp_ms":now}),
    );
    Ok(())
}

fn interval_quantity(bytes: u64, start: u64, requested_end: u64) -> Result<(u64, u64)> {
    let duration = requested_end
        .checked_sub(start)
        .ok_or_else(|| fail("invalid_storage_interval"))?;
    // Ledger uses signed SQLite INTEGER; split long intervals instead of overflow.
    let duration = duration.min((i64::MAX as u64) / bytes.max(1));
    if duration == 0 && requested_end > start {
        return Err(fail("storage_quantity_too_large"));
    }
    Ok((
        start + duration,
        bytes
            .checked_mul(duration)
            .ok_or_else(|| fail("storage_quantity_overflow"))?,
    ))
}

fn interval_id(key: &str, start: u64, end: u64) -> String {
    let digest = Sha256::digest(format!("{}:{key}:{start}:{end}", key.len()).as_bytes());
    format!("storage_{digest:x}")
}
fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quantities_split_without_gaps_or_overflow() {
        assert_eq!(
            interval_quantity(1024, 1000, 2000).unwrap(),
            (2000, 1_024_000)
        );
        let (end, quantity) = interval_quantity(i64::MAX as u64, 0, 2).unwrap();
        assert_eq!((end, quantity), (1, i64::MAX as u64));
        assert!(interval_quantity(1, 2000, 1000).is_err());
        assert_eq!(interval_quantity(0, 1000, 2000).unwrap(), (2000, 0));
    }
    #[test]
    fn retry_identity_is_stable_and_binds_interval() {
        assert_eq!(interval_id("key", 1, 2), interval_id("key", 1, 2));
        assert_ne!(interval_id("key", 1, 2), interval_id("key", 1, 3));
        assert_ne!(interval_id("key", 1, 2), interval_id("other", 1, 2));
    }
}
