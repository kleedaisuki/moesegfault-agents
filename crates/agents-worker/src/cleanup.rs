//! A conversation may outlive its model budget. Its final export is funded before
//! inference admission, with the original allowance vintage and tariff snapshot.

use agents_auth::billing::{ReservationState, Resource};
use serde::{Deserialize, Serialize};
use worker::{Env, Error, Result};

use crate::{metering, resources::ResourcePricing};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupHold {
    pub id: String,
    pub pricing: ResourcePricing,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    owner: String,
    session: String,
    generation: u64,
    pricing: ResourcePricing,
}

#[derive(Deserialize)]
struct HoldRow {
    id: String,
    request_hash: String,
    state: ReservationState,
}

/// Loading does not consult current deployment prices or available credit.
pub async fn load(
    env: &Env,
    owner: &str,
    session: &str,
    generation: u64,
) -> Result<Option<CleanupHold>> {
    let id = hold_id(session, generation);
    let row: Option<HoldRow> = env.d1("DB")?.prepare("SELECT id,request_hash,state FROM usage_reservations WHERE id=? AND account_id=? AND session_id=?")
        .bind(&[id.as_str().into(),owner.into(),session.into()])?.first(None).await?;
    row.map(|row| {
        if row.state != ReservationState::Reserved {
            return Err(fail("cleanup_already_finalized"));
        }
        let snapshot: Snapshot = serde_json::from_str(&row.request_hash)?;
        if snapshot.owner != owner
            || snapshot.session != session
            || snapshot.generation != generation
        {
            return Err(fail("cleanup_hold_conflict"));
        }
        let pricing = ResourcePricing::parse(&serde_json::to_string(&snapshot.pricing)?)?;
        Ok(CleanupHold {
            id: row.id,
            pricing,
        })
    })
    .transpose()
}

/// Idempotent for an archive generation, and independent of the concurrent-run quota.
pub async fn ensure(env: &Env, owner: &str, session: &str, generation: u64) -> Result<CleanupHold> {
    if let Some(hold) = load(env, owner, session, generation).await? {
        return Ok(hold);
    }
    let pricing = ResourcePricing::parse(&env.var("RESOURCE_PRICING_JSON")?.to_string())?;
    let amount = budget(&pricing)?;
    let id = hold_id(session, generation);
    let snapshot = Snapshot {
        owner: owner.into(),
        session: session.into(),
        generation,
        pricing,
    };
    let hash = serde_json::to_string(&snapshot)?;
    let now = (worker::Date::now().as_millis() / 1000) as i64;
    let row = metering::reserve(
        &env.d1("DB")?,
        owner,
        Some(session),
        &id,
        &id,
        &hash,
        amount,
        now,
    )
    .await?;
    if row.state != ReservationState::Reserved {
        return Err(fail("cleanup_already_finalized"));
    }
    Ok(CleanupHold {
        id: row.id,
        pricing: snapshot.pricing,
    })
}

/// The caller must have completed export using another funded lease. An unknown
/// cleanup side effect keeps this hold reserved; it is never refunded as unused.
pub async fn release_unused(env: &Env, owner: &str, session: &str, generation: u64) -> Result<()> {
    let id = hold_id(session, generation);
    let row: Option<HoldRow> = env.d1("DB")?.prepare("SELECT id,request_hash,state FROM usage_reservations WHERE id=? AND account_id=? AND session_id=?")
        .bind(&[id.as_str().into(),owner.into(),session.into()])?.first(None).await?;
    let row = row.ok_or_else(|| fail("cleanup_hold_missing"))?;
    let snapshot: Snapshot = serde_json::from_str(&row.request_hash)?;
    if snapshot.owner != owner || snapshot.session != session || snapshot.generation != generation {
        return Err(fail("cleanup_hold_conflict"));
    }
    if row.state == ReservationState::Released {
        return Ok(());
    }
    if row.state != ReservationState::Reserved {
        return Err(fail("cleanup_already_finalized"));
    }
    #[derive(Deserialize)]
    struct Pending {
        count: i64,
    }
    let pending: Pending = env
        .d1("DB")?
        .prepare("SELECT COUNT(*) AS count FROM resource_observations WHERE run_id=?")
        .bind(&[id.as_str().into()])?
        .first(None)
        .await?
        .ok_or_else(|| fail("cleanup_observation_missing"))?;
    if pending.count != 0 {
        return Err(fail("cleanup_usage_requires_reconciliation"));
    }
    metering::release(
        &env.d1("DB")?,
        &id,
        (worker::Date::now().as_millis() / 1000) as i64,
    )
    .await?;
    Ok(())
}

fn hold_id(session: &str, generation: u64) -> String {
    format!("maintenance:{session}:{generation}")
}

fn budget(pricing: &ResourcePricing) -> Result<i64> {
    let mut amount = 0_i64;
    for (resource, unit, quantity) in [
        (Resource::R2Operation, "class_a", 1),
        (Resource::D1, "rows_read", 32),
        (Resource::D1, "rows_written", 8),
    ] {
        let charge = pricing
            .tariff(&resource, unit)?
            .charge(quantity)
            .map_err(|_| fail("invalid_cleanup_tariff"))?;
        amount = amount
            .checked_add(charge.amount_microusd.get())
            .ok_or_else(|| fail("cleanup_budget_overflow"))?;
    }
    amount
        .checked_add(amount / 4 + i64::from(amount % 4 != 0))
        .ok_or_else(|| fail("cleanup_budget_overflow"))
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agents_auth::billing::ResourceTariff;

    #[test]
    fn cleanup_budget_is_tariff_derived_not_a_run_hold() {
        let tariffs = [
            (Resource::R2Operation, "class_a", 5),
            (Resource::D1, "rows_read", 1),
            (Resource::D1, "rows_written", 2),
        ]
        .into_iter()
        .map(|(resource, unit, rate)| ResourceTariff {
            resource,
            unit: unit.into(),
            microusd_numerator: rate,
            quantity_denominator: 1,
            price_version: "v1".into(),
        })
        .collect();
        let pricing = ResourcePricing {
            version: "v1".into(),
            tariffs,
        };
        assert_eq!(budget(&pricing).unwrap(), 67);
    }

    #[test]
    fn cleanup_requires_every_archive_tariff() {
        assert!(
            budget(&ResourcePricing {
                version: "v1".into(),
                tariffs: vec![]
            })
            .is_err()
        );
    }
}
