//! Tariff-pinned index allocation clock. Fractional units carry across ticks;
//! only final closure rounds the residual, never every maintenance invocation.

use agents_auth::billing::{Resource, ResourceTariff};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::{Env, Error, Result};

use crate::{
    file_index::IngestJob,
    metering,
    resources::{ResourceMeter, ResourcePricing},
    util,
};

pub(crate) const UNIT: &str = "allocated_vector_dimension_ms";

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

fn identity(owner: &str, store: &str, file: &str, generation: &str) -> Result<String> {
    Ok(format!(
        "vclock_{:x}",
        Sha256::digest(serde_json::to_vec(&(owner, store, file, generation))?)
    ))
}

fn quantum(tariff: &ResourceTariff) -> Result<u64> {
    let (mut left, mut right) = (tariff.microusd_numerator, tariff.quantity_denominator);
    if left == 0 || right == 0 {
        return Err(fail("invalid_vector_storage_tariff"));
    }
    while right != 0 {
        (left, right) = (right, left % right);
    }
    let quantum = tariff.quantity_denominator / left;
    if quantum > i64::MAX as u64 {
        return Err(fail("invalid_vector_storage_tariff"));
    }
    Ok(quantum)
}

#[derive(Deserialize)]
struct Clock {
    id: String,
    account_id: String,
    dimensions: u64,
    pricing_json: String,
    billed_through_ms: u64,
    carry_dimension_ms: String,
    removed_at_ms: Option<u64>,
    pending_json: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Tick {
    from_ms: u64,
    to_ms: u64,
    quantity: u64,
    carry: u64,
    next_due_ms: u64,
    finalized: bool,
}

fn tick(
    dimensions: u64,
    from_ms: u64,
    requested_ms: u64,
    carry: u64,
    removed_ms: Option<u64>,
    quantum: u64,
) -> Result<Tick> {
    if dimensions == 0 || quantum == 0 || carry >= quantum {
        return Err(fail("invalid_vector_storage_clock"));
    }
    let requested = removed_ms
        .map_or(requested_ms, |end| end.min(requested_ms))
        .max(from_ms);
    let duration = (requested - from_ms).min((i64::MAX as u64 - carry) / dimensions);
    if duration == 0 && requested > from_ms {
        return Err(fail("vector_storage_quantity_too_large"));
    }
    let to_ms = from_ms + duration;
    let total = carry + duration * dimensions;
    let finalized = removed_ms.is_some_and(|end| to_ms >= end);
    let quantity = if finalized {
        total
    } else {
        total - total % quantum
    };
    let carry = total - quantity;
    let delay = (quantum - carry).div_ceil(dimensions);
    let next_due_ms = if finalized || to_ms < requested {
        0
    } else {
        to_ms.saturating_add(delay)
    };
    Ok(Tick {
        from_ms,
        to_ms,
        quantity,
        carry,
        next_due_ms,
        finalized,
    })
}

pub(crate) fn preflight(meter: &ResourceMeter<'_>) -> Result<()> {
    meter.require_tariff(&Resource::Worker, UNIT)
}

pub(crate) async fn allocate(
    env: &Env,
    job: &IngestJob,
    dimensions_per_vector: usize,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    let dimensions = (job.chunk_count as u64)
        .checked_mul(dimensions_per_vector as u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| fail("invalid_vector_storage_dimensions"))?;
    let pricing = ResourcePricing::parse(&env.var("RESOURCE_PRICING_JSON")?.to_string())?;
    let tariff = pricing.tariff(&Resource::Worker, UNIT)?;
    let quantum = quantum(tariff)?;
    let now = util::now_ms();
    let id = identity(
        &job.account_id,
        &job.vector_store_id,
        &job.file_id,
        &job.generation,
    )?;
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("INSERT INTO file_search_storage_clocks(id,account_id,vector_store_id,file_id,generation,dimensions,pricing_json,created_at_ms,billed_through_ms,next_due_ms) SELECT ?1,?2,?3,?4,?5,CAST(?6 AS INTEGER),?7,CAST(?8 AS INTEGER),CAST(?8 AS INTEGER),CAST(?9 AS INTEGER) WHERE EXISTS(SELECT 1 FROM vector_store_files vf WHERE vf.account_id=?2 AND vf.vector_store_id=?3 AND vf.file_id=?4 AND vf.generation=?5 AND vf.lease_id=?10 AND vf.status='in_progress' AND vf.source_staged=1 AND vf.lease_until>CAST(?8 AS INTEGER)) ON CONFLICT(id) DO NOTHING")
        .bind(&[id.into(),job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into(),dimensions.to_string().into(),serde_json::to_string(&pricing)?.into(),now.to_string().into(),now.saturating_add(quantum.div_ceil(dimensions)).to_string().into(),job.lease_id.as_str().into()])?.run().await?;
    meter.record_d1(event, &result).await
}

pub(crate) async fn close(
    env: &Env,
    owner: &str,
    store: &str,
    file: &str,
    generation: &str,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    let id = identity(owner, store, file, generation)?;
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE file_search_storage_clocks SET removed_at_ms=MAX(billed_through_ms,CAST(?3 AS INTEGER)),next_due_ms=0 WHERE id=?1 AND account_id=?2 AND removed_at_ms IS NULL AND NOT EXISTS(SELECT 1 FROM file_search_chunks c WHERE c.account_id=?2 AND c.vector_store_id=?4 AND c.file_id=?5 AND c.generation=?6)")
        .bind(&[id.into(),owner.into(),util::now_ms().to_string().into(),store.into(),file.into(),generation.into()])?.run().await?;
    meter.record_d1(event, &result).await
}

pub(crate) async fn require_paid_store(env: &Env, owner: &str, store: &str) -> Result<()> {
    let row=env.d1("DB")?.prepare("SELECT id FROM file_search_storage_clocks WHERE account_id=?1 AND vector_store_id=?2 AND debt=1 LIMIT 1")
        .bind(&[owner.into(),store.into()])?.first::<serde_json::Value>(None).await?;
    if row.is_some() {
        return Err(fail("file_search_storage_payment_pending"));
    }
    Ok(())
}

pub(crate) async fn run(env: &Env) -> Result<()> {
    let rows=env.d1("DB")?.prepare("SELECT id FROM file_search_storage_clocks WHERE finalized=0 AND next_due_ms<=CAST(?1 AS INTEGER) AND lease_until<=CAST(?1 AS INTEGER) ORDER BY next_due_ms,id LIMIT 32")
        .bind(&[util::now_ms().to_string().into()])?.all().await?.results::<serde_json::Value>()?;
    for row in rows {
        if let Some(id) = row["id"].as_str() {
            if reconcile(env, id).await.is_err() {
                util::emit("file_search.storage_pending", serde_json::json!({}));
            }
        }
    }
    Ok(())
}

async fn reconcile(env: &Env, id: &str) -> Result<()> {
    let lease = uuid::Uuid::new_v4().simple().to_string();
    let row=env.d1("DB")?.prepare("UPDATE file_search_storage_clocks SET lease_id=?2,lease_until=CAST(?3 AS INTEGER) WHERE id=?1 AND finalized=0 AND lease_until<=CAST(?4 AS INTEGER) RETURNING id,account_id,vector_store_id,dimensions,pricing_json,billed_through_ms,carry_dimension_ms,removed_at_ms,pending_json")
        .bind(&[id.into(),lease.as_str().into(),(util::now_ms()+120000).to_string().into(),util::now_ms().to_string().into()])?.all().await?.results::<Clock>()?.into_iter().next();
    let Some(clock) = row else {
        return Ok(());
    };
    let outcome = reconcile_claimed(env, &clock, &lease).await;
    if outcome.is_err() {
        env.d1("DB")?.prepare("UPDATE file_search_storage_clocks SET debt=1,lease_id=NULL,lease_until=0,next_due_ms=CAST(?3 AS INTEGER) WHERE id=?1 AND lease_id=?2 AND finalized=0")
            .bind(&[clock.id.as_str().into(),lease.as_str().into(),(util::now_ms()+300000).to_string().into()])?.run().await?;
    }
    outcome
}

async fn reconcile_claimed(env: &Env, clock: &Clock, lease: &str) -> Result<()> {
    let pricing = ResourcePricing::parse(&clock.pricing_json)?;
    let tariff = pricing.tariff(&Resource::Worker, UNIT)?;
    let tick = match &clock.pending_json {
        Some(encoded) => serde_json::from_str::<Tick>(encoded)?,
        None => tick(
            clock.dimensions,
            clock.billed_through_ms,
            util::now_ms(),
            clock
                .carry_dimension_ms
                .parse()
                .map_err(|_| fail("invalid_vector_storage_carry"))?,
            clock.removed_at_ms,
            quantum(tariff)?,
        )?,
    };
    let encoded = serde_json::to_string(&tick)?;
    let pending=env.d1("DB")?.prepare("UPDATE file_search_storage_clocks SET pending_json=?3 WHERE id=?1 AND lease_id=?2 AND (pending_json IS NULL OR pending_json=?3) RETURNING id")
        .bind(&[clock.id.as_str().into(),lease.into(),encoded.as_str().into()])?.all().await?;
    if pending.results::<serde_json::Value>()?.is_empty() {
        return Err(fail("vector_storage_lease_lost"));
    }
    if tick.quantity > 0 {
        charge(env, clock, &tick, pricing).await?;
    }
    env.d1("DB")?.prepare("UPDATE file_search_storage_clocks SET billed_through_ms=CAST(?4 AS INTEGER),carry_dimension_ms=?5,finalized=?6,next_due_ms=CASE WHEN removed_at_ms IS NOT NULL AND ?6=0 THEN 0 ELSE CAST(?7 AS INTEGER) END,pending_json=NULL,debt=0,lease_id=NULL,lease_until=0 WHERE id=?1 AND lease_id=?2 AND pending_json=?3")
        .bind(&[clock.id.as_str().into(),lease.into(),encoded.into(),tick.to_ms.to_string().into(),tick.carry.to_string().into(),i32::from(tick.finalized).into(),tick.next_due_ms.to_string().into()])?.run().await?;
    Ok(())
}

async fn charge(env: &Env, clock: &Clock, tick: &Tick, pricing: ResourcePricing) -> Result<()> {
    let tariff = pricing.tariff(&Resource::Worker, UNIT)?;
    let amount = tariff
        .charge(tick.quantity)
        .map_err(|_| fail("invalid_vector_storage_charge"))?
        .amount_microusd
        .get();
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(&clock.id, tick, &pricing.version))?)
    );
    let id = format!("vector-storage:{hash}");
    metering::reserve(
        &env.d1("DB")?,
        &clock.account_id,
        None,
        &id,
        &id,
        &hash,
        amount,
        util::now_ms() as i64,
    )
    .await?;
    let meter = ResourceMeter::with_account_pricing(env, &clock.account_id, &id, pricing)?;
    meter
        .record_observation("capacity", Resource::Worker, UNIT, tick.quantity)
        .await?;
    meter.settle_run().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequent_observers_preserve_fractional_capacity_without_roundup() {
        let mut carry = 0;
        let mut charged = 0;
        for time in 1..=100 {
            let next = tick(1, time - 1, time, carry, None, 1000).unwrap();
            carry = next.carry;
            charged += next.quantity;
        }
        assert_eq!((charged, carry), (0, 100));
        let final_tick = tick(1, 100, 100, carry, Some(100), 1000).unwrap();
        assert!(final_tick.finalized);
        assert_eq!((final_tick.quantity, final_tick.carry), (100, 0));
    }

    #[test]
    fn capacity_intervals_split_at_signed_ledger_limit_without_gaps() {
        let first = tick(i64::MAX as u64, 0, 2, 0, None, 1).unwrap();
        assert_eq!(
            (first.to_ms, first.quantity, first.next_due_ms),
            (1, i64::MAX as u64, 0)
        );
        let last = tick(i64::MAX as u64, first.to_ms, 2, 0, Some(2), 1).unwrap();
        assert_eq!(
            (last.from_ms, last.to_ms, last.quantity),
            (1, 2, i64::MAX as u64)
        );
        assert!(last.finalized);
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_observation_boundaries_charge_only_the_same_final_allocation(
            dimensions in 1u64..10000,
            steps in proptest::collection::vec(0u64..10000, 1..100)
        ) {
            let mut at=0;
            let mut carry=0;
            let mut charged=0;
            let quantum=10000;
            for step in steps {
                let next=tick(dimensions,at,at+step,carry,None,quantum).unwrap();
                at=next.to_ms;
                carry=next.carry;
                charged+=next.quantity;
            }
            let end=tick(dimensions,at,at,carry,Some(at),quantum).unwrap();
            proptest::prop_assert_eq!(charged+end.quantity,dimensions*at);
            proptest::prop_assert_eq!(end.carry,0);
        }
    }
}
