//! Trusted actor evidence releases execution capacity, never a monetary hold.
use serde::Deserialize;
use worker::{D1Database, Error, Result};

// Used by the shared reservation engine; funding and lease creation are one
// INSERT transaction, including the reserve_budget and lease metadata triggers.
pub(crate) const ADMIT_SQL: &str = "INSERT INTO usage_reservations (id, account_id, session_id, idempotency_key, request_hash, reserved_microusd, state, created_at, updated_at, execution_generation) SELECT ?,?,?,?,?,CAST(? AS INTEGER),'reserved',CAST(? AS INTEGER),CAST(? AS INTEGER),? WHERE NOT EXISTS (SELECT 1 FROM usage_reservations WHERE account_id=? AND idempotency_key=?) AND (? IS NULL OR (SELECT COUNT(*) FROM usage_reservations r WHERE r.account_id=? AND r.state='reserved' AND r.execution_finished_at IS NULL AND r.idempotency_key LIKE 'run:%' AND NOT EXISTS (SELECT 1 FROM execution_leases l WHERE l.reservation_id=r.id AND l.account_id=r.account_id AND l.generation=r.execution_generation AND l.state='no_local_execution')) < CAST(? AS INTEGER))";
const RELEASE_SQL: &str = "UPDATE execution_leases SET state='no_local_execution',released_at_ms=CAST(? AS INTEGER) WHERE reservation_id=? AND account_id=? AND generation=? AND state='active'";

#[derive(Deserialize)]
struct Lease {
    generation: String,
    state: String,
}

/// Called only after the actor durably fences further dispatch for this identity.
/// A cold actor is evidence that local I/O no longer exists, not that its provider
/// request failed or that its funding can be refunded. There is no TTL release.
pub(crate) async fn release(
    db: &D1Database,
    account: &str,
    run: &str,
    generation: &str,
    now_ms: u64,
) -> Result<()> {
    if generation.is_empty() || generation.len() > 128 || now_ms > i64::MAX as u64 {
        return Err(Error::RustError("invalid_execution_lease".into()));
    }
    db.prepare(RELEASE_SQL)
        .bind(&[
            now_ms.to_string().into(),
            run.into(),
            account.into(),
            generation.into(),
        ])?
        .run()
        .await?;
    let lease: Option<Lease> = db
        .prepare(
            "SELECT generation,state FROM execution_leases WHERE reservation_id=? AND account_id=?",
        )
        .bind(&[run.into(), account.into()])?
        .first(None)
        .await?;
    let Some(lease) = lease else {
        return Ok(());
    };
    if lease.generation != generation || lease.state != "no_local_execution" {
        return Err(Error::RustError(
            "execution_lease_generation_mismatch".into(),
        ));
    }
    Ok(())
}

/// Dispatch requires the original active generation, not merely a funded row.
pub(crate) async fn ensure_active(
    db: &D1Database,
    account: &str,
    run: &str,
    generation: &str,
) -> Result<()> {
    let lease: Option<Lease> = db
        .prepare(
            "SELECT generation,state FROM execution_leases WHERE reservation_id=? AND account_id=?",
        )
        .bind(&[run.into(), account.into()])?
        .first(None)
        .await?;
    if !lease.is_some_and(|lease| lease.generation == generation && lease.state == "active") {
        return Err(Error::RustError("execution_lease_not_active".into()));
    }
    Ok(())
}

const UNUSED_SQL: &str = "UPDATE usage_reservations SET state='released',updated_at=CAST(? AS INTEGER) WHERE account_id=? AND session_id IS NULL AND state='reserved' AND (id=? OR idempotency_key=?) AND EXISTS (SELECT 1 FROM execution_leases l WHERE l.reservation_id=? AND l.account_id=? AND l.generation=?) AND NOT EXISTS (SELECT 1 FROM usage_ledger u WHERE u.reservation_id=usage_reservations.id) AND NOT EXISTS (SELECT 1 FROM resource_observations o WHERE o.run_id=usage_reservations.id AND o.account_id=usage_reservations.account_id)";

/// Durable pre-dispatch rejection proves no provider was polled. Only empty,
/// unobserved holds of the exact operation can be released; pending observations
/// or a ledger entry preserve financial uncertainty for reconciliation.
pub(crate) async fn release_undispatched(
    db: &D1Database,
    account: &str,
    run: &str,
    generation: &str,
    now_ms: u64,
) -> Result<()> {
    db.prepare(UNUSED_SQL)
        .bind(&[
            (now_ms / 1000).to_string().into(),
            account.into(),
            run.into(),
            format!("completion:{run}:compact").into(),
            run.into(),
            account.into(),
            generation.into(),
        ])?
        .run()
        .await?;
    Ok(())
}
