use agents_auth::billing::{ReservationState, Resource};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result, State, Storage};

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

#[derive(Deserialize)]
#[serde(untagged)]
enum LeaseWire {
    Json(String),
    Legacy(Lease),
}

impl LeaseWire {
    fn decode(self) -> Result<Lease> {
        match self {
            Self::Json(json) => {
                serde_json::from_str(&json).map_err(|_| fail("workspace_lease_corrupt"))
            }
            Self::Legacy(lease) => Ok(lease),
        }
    }
}

fn encode_lease(lease: &Lease) -> Result<String> {
    // The default JS serializer rejects integers above 2^53-1. Whole-document
    // JSON text preserves exact tariffs and money without changing their types
    // or the canonical Lease representation used by reservation hashes.
    Ok(serde_json::to_string(lease)?)
}

async fn read_lease(storage: &Storage, key: &str) -> Result<Option<Lease>> {
    storage
        .get::<LeaseWire>(key)
        .await?
        .map(LeaseWire::decode)
        .transpose()
}

async fn write_lease(storage: &Storage, key: &str, lease: &Lease) -> Result<()> {
    storage.put(key, encode_lease(lease)?).await
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
        read_lease(&self.state.storage(), "workspace:lease").await
    }

    pub async fn prepare(&self, minimum_until: Option<u64>) -> Result<Option<Renewal>> {
        let previous = self.current().await?;
        let now = crate::util::now_ms();
        if minimum_until.is_some_and(|deadline| deadline > now.saturating_add(DURATION_MS)) {
            return Err(fail("workspace_lease_horizon_unsupported"));
        }
        if previous
            .as_ref()
            .is_some_and(|lease| !renew_covering(lease.expires_ms, now, minimum_until))
        {
            return Ok(None);
        }
        let storage = self.state.storage();
        let mut candidate = read_lease(&storage, "workspace:lease-next").await?;
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
            .filter(|next| renew_covering(next.expires_ms, now, minimum_until))
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
                let profile =
                    crate::runtime_profiles::profile_for(self.env, self.owner, self.session)
                        .await?;
                let pricing = crate::runtime_profiles::pricing(self.env, profile)?;
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
                write_lease(&storage, "workspace:lease-next", &lease).await?;
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
        write_lease(&storage, &format!("workspace:lease:{}", next.id), &next).await?;
        Ok(Some(Renewal { previous, next }))
    }

    pub async fn activate(&self, lease: &Lease) -> Result<()> {
        write_lease(&self.state.storage(), "workspace:lease", lease).await?;
        self.state.storage().delete("workspace:lease-next").await?;
        Ok(())
    }

    pub async fn meter<'a>(&'a self, id: &'a str) -> Result<ResourceMeter<'a>> {
        let lease = read_lease(&self.state.storage(), &format!("workspace:lease:{id}"))
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

fn renew_covering(expires_ms: u64, now_ms: u64, minimum_until: Option<u64>) -> bool {
    renew_due(expires_ms, now_ms) || minimum_until.is_some_and(|deadline| expires_ms < deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agents_auth::billing::ResourceTariff;

    fn lease_fixture() -> Lease {
        Lease {
            id: "codec-regression".into(),
            expires_ms: 1_800_000,
            pricing: ResourcePricing {
                version: "test-v1".into(),
                tariffs: vec![ResourceTariff {
                    resource: Resource::R2Storage,
                    unit: "byte_ms".into(),
                    microusd_numerator: 15_000,
                    quantity_denominator: 2_592_000_000_000_000_000,
                    price_version: "test-v1".into(),
                }],
            },
            amount: 9_007_199_254_740_993,
            created_ms: 1,
        }
    }

    #[test]
    fn json_storage_preserves_large_tariffs_and_money_without_changing_hashes() {
        let lease = lease_fixture();
        let canonical = serde_json::to_string(&lease).unwrap();
        let wire: LeaseWire =
            serde_json::from_value(serde_json::Value::String(encode_lease(&lease).unwrap()))
                .unwrap();
        let decoded = wire.decode().unwrap();
        assert_eq!(
            decoded.pricing.tariffs[0].quantity_denominator,
            2_592_000_000_000_000_000
        );
        assert_eq!(decoded.amount, 9_007_199_254_740_993);
        assert_eq!(serde_json::to_string(&decoded).unwrap(), canonical);
        assert_eq!(
            agents_auth::hash_secret(&serde_json::to_string(&decoded).unwrap()),
            agents_auth::hash_secret(&canonical)
        );
    }

    #[test]
    fn legacy_structured_leases_remain_readable() {
        let mut lease = lease_fixture();
        lease.pricing.tariffs[0].quantity_denominator = 1000;
        lease.amount = 38_000;
        let legacy = serde_json::to_value(&lease).unwrap();
        let wire: LeaseWire = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(wire.decode().unwrap()).unwrap(),
            legacy
        );
    }

    #[test]
    fn corrupt_lease_storage_is_rejected() {
        for invalid in ["not json", "[]", "{}", "null"] {
            assert!(LeaseWire::Json(invalid.into()).decode().is_err());
        }
        assert!(
            serde_json::from_value::<LeaseWire>(serde_json::json!({"id":"incomplete"})).is_err()
        );
        assert!(serde_json::from_value::<LeaseWire>(serde_json::json!(42)).is_err());
    }

    #[test]
    fn renewal_funds_idle_and_one_bounded_step_before_expiry() {
        assert!(!renew_due(DURATION_MS, 0));
        assert!(!renew_due(DURATION_MS, DURATION_MS - RENEW_MARGIN_MS));
        assert!(renew_due(DURATION_MS, DURATION_MS - RENEW_MARGIN_MS + 1));
        assert!(renew_due(DURATION_MS, DURATION_MS + 1));
        assert!(RENEW_MARGIN_MS >= 600_000 + 300_000);
    }
    #[test]
    fn code_use_covers_its_idle_window_before_the_normal_renewal_margin() {
        let now = 12 * 60 * 1000;
        let required = now + 22 * 60 * 1000;
        assert!(!renew_due(DURATION_MS, now));
        assert!(renew_covering(DURATION_MS, now, Some(required)));
        assert!(!renew_covering(DURATION_MS, now, None));
        assert!(!renew_covering(required, now, Some(required)));
        assert!(!renew_covering(
            DURATION_MS,
            5 * 60 * 1000,
            Some(27 * 60 * 1000)
        ));
    }
}
