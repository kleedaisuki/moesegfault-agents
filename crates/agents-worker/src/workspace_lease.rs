use agents_auth::billing::{ReservationState, Resource};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result, State};

use crate::{
    metering,
    resources::{ResourceMeter, ResourcePricing},
};

const DURATION_MS: u64 = 30 * 60 * 1000;
const RENEW_MARGIN_MS: u64 = 15 * 60 * 1000;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Lease {
    pub id: String,
    pub expires_ms: u64,
    pub pricing: ResourcePricing,
    amount: i64,
    created_ms: u64,
}

pub(super) struct Renewal {
    pub previous: Option<Lease>,
    pub next: Lease,
}

pub(super) struct ComputeLease<'a> {
    pub state: &'a State,
    pub env: &'a Env,
    pub owner: &'a str,
    pub session: &'a str,
}

impl ComputeLease<'_> {
    pub async fn current(&self) -> Result<Option<Lease>> {
        self.state.storage().get("workspace:lease").await
    }

    pub async fn prepare(&self) -> Result<Option<Renewal>> {
        let previous = self.current().await?;
        let now = crate::util::now_ms();
        if previous
            .as_ref()
            .is_some_and(|lease| !renew_due(lease.expires_ms, now))
        {
            return Ok(None);
        }
        let storage = self.state.storage();
        let mut candidate = storage.get::<Lease>("workspace:lease-next").await?;
        if candidate
            .as_ref()
            .is_some_and(|next| previous.as_ref().is_some_and(|old| old.id == next.id))
        {
            // Activation committed before a crash interrupted pending-key cleanup.
            storage.delete("workspace:lease-next").await?;
            candidate = None;
        }
        if let Some(stale) = candidate
            .as_ref()
            .filter(|next| renew_due(next.expires_ms, now))
        {
            // A wallet rejection may leave admission pending for arbitrarily
            // long. An unactivated hold owns no compute and can be replaced.
            #[derive(Deserialize)]
            struct Reservation {
                state: ReservationState,
            }
            let db = self.env.d1("DB")?;
            let row = db
                .prepare("SELECT state FROM usage_reservations WHERE id=? AND account_id=?")
                .bind(&[JsValue::from_str(&stale.id), JsValue::from_str(self.owner)])?
                .first::<Reservation>(None)
                .await?;
            if row.is_some_and(|row| row.state == ReservationState::Reserved) {
                metering::release(&db, &stale.id, now as i64).await?;
            }
            storage.delete("workspace:lease-next").await?;
            candidate = None;
        }
        let next = match candidate {
            Some(lease) => lease,
            None => {
                let pricing =
                    ResourcePricing::parse(&self.env.var("RESOURCE_PRICING_JSON")?.to_string())?;
                let compute = pricing
                    .tariff(&Resource::Container, "wall_ms")?
                    .charge(DURATION_MS)
                    .map_err(|_| fail("workspace_compute_tariff_invalid"))?
                    .amount_microusd
                    .get();
                let extra = self
                    .env
                    .var("RUN_RESOURCE_RESERVATION_MICROUSD")?
                    .to_string()
                    .parse::<i64>()
                    .map_err(|_| fail("workspace_resource_budget_invalid"))?;
                if extra <= 0 {
                    return Err(fail("workspace_resource_budget_invalid"));
                }
                let lease = Lease {
                    id: uuid::Uuid::new_v4().to_string(),
                    expires_ms: now
                        .checked_add(DURATION_MS)
                        .ok_or_else(|| fail("workspace_lease_time_overflow"))?,
                    pricing,
                    amount: compute
                        .checked_add(extra)
                        .ok_or_else(|| fail("workspace_resource_budget_overflow"))?,
                    created_ms: now,
                };
                storage.put("workspace:lease-next", &lease).await?;
                lease
            }
        };
        let key = format!("workspace:{}:{}", self.session, next.id);
        let hash = agents_auth::hash_secret(&serde_json::to_string(&next)?);
        let row = metering::reserve(
            &self.env.d1("DB")?,
            self.owner,
            Some(self.session),
            &next.id,
            &key,
            &hash,
            next.amount,
            next.created_ms as i64,
        )
        .await?;
        if row.state != ReservationState::Reserved {
            return Err(fail("workspace_lease_already_finalized"));
        }
        storage
            .put(&format!("workspace:lease:{}", next.id), &next)
            .await?;
        Ok(Some(Renewal { previous, next }))
    }

    pub async fn activate(&self, lease: &Lease) -> Result<()> {
        self.state.storage().put("workspace:lease", lease).await?;
        self.state.storage().delete("workspace:lease-next").await?;
        Ok(())
    }

    pub async fn meter<'a>(&'a self, id: &'a str) -> Result<ResourceMeter<'a>> {
        let lease = self
            .state
            .storage()
            .get::<Lease>(&format!("workspace:lease:{id}"))
            .await?
            .ok_or_else(|| fail("workspace_lease_missing"))?;
        ResourceMeter::with_pricing(self.env, self.owner, self.session, id, lease.pricing)
    }

    pub async fn settle(&self, id: &str) -> Result<()> {
        self.meter(id).await?.settle_run().await?;
        Ok(())
    }

    pub async fn clear(&self) -> Result<()> {
        self.state.storage().delete("workspace:lease").await?;
        Ok(())
    }
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

fn renew_due(expires_ms: u64, now_ms: u64) -> bool {
    expires_ms.saturating_sub(now_ms) < RENEW_MARGIN_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renewal_funds_idle_and_one_bounded_step_before_expiry() {
        assert!(!renew_due(DURATION_MS, 0));
        assert!(!renew_due(DURATION_MS, DURATION_MS - RENEW_MARGIN_MS));
        assert!(renew_due(DURATION_MS, DURATION_MS - RENEW_MARGIN_MS + 1));
        assert!(renew_due(DURATION_MS, DURATION_MS + 1));
        assert!(RENEW_MARGIN_MS >= 600_000 + 300_000);
    }
}
