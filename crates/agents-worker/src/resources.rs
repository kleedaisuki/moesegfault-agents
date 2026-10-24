//! Observed resource accounting; missing measurements remain durable budget holds.
//!
//! Runtime Containers expose no `getMetrics` API. CPU/allocation reconciliation is
//! delayed GraphQL analytics, not elapsed wall time:
//! <https://developers.cloudflare.com/analytics/graphql-api/tutorials/querying-container-metrics/>
//! D1 row quantities come from result metadata, never affected-row approximations:
//! <https://developers.cloudflare.com/d1/worker-api/return-object/>

use agents_auth::billing::{Resource, ResourceTariff};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{D1Database, D1Result, Env, Error, Result};

use crate::metering::{self, ReservationRow};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcePricing {
    pub version: String,
    pub tariffs: Vec<ResourceTariff>,
}

impl ResourcePricing {
    pub fn parse(json: &str) -> Result<Self> {
        let pricing: Self =
            serde_json::from_str(json).map_err(|_| fail("invalid_resource_pricing"))?;
        if pricing.version.is_empty() || pricing.tariffs.is_empty() {
            return Err(fail("invalid_resource_pricing"));
        }
        for (index, tariff) in pricing.tariffs.iter().enumerate() {
            if tariff.price_version != pricing.version
                || tariff.microusd_numerator == 0
                || tariff.quantity_denominator == 0
                || tariff.unit.is_empty()
                || tariff.resource == Resource::Model
                || pricing.tariffs[..index].iter().any(|previous| {
                    previous.resource == tariff.resource && previous.unit == tariff.unit
                })
                || tariff.charge(1).is_err()
            {
                return Err(fail("invalid_resource_pricing"));
            }
        }
        Ok(pricing)
    }

    pub fn tariff(&self, resource: &Resource, unit: &str) -> Result<&ResourceTariff> {
        self.tariffs
            .iter()
            .find(|tariff| &tariff.resource == resource && tariff.unit == unit)
            .ok_or_else(|| fail("resource_tariff_missing"))
    }
}

pub struct ResourceMeter<'a> {
    env: &'a Env,
    owner: &'a str,
    session: Option<&'a str>,
    run: &'a str,
    pricing: ResourcePricing,
}

#[derive(Deserialize)]
struct Observation {
    resource: String,
    unit: String,
    quantity: Option<String>,
    price_version: String,
    status: String,
}

impl<'a> ResourceMeter<'a> {
    pub fn with_pricing(
        env: &'a Env,
        owner: &'a str,
        session: &'a str,
        run: &'a str,
        pricing: ResourcePricing,
    ) -> Result<Self> {
        let pricing = ResourcePricing::parse(&serde_json::to_string(&pricing)?)?;
        Ok(Self {
            env,
            owner,
            session: Some(session),
            run,
            pricing,
        })
    }

    /// Validate pricing before admission or creating any chargeable resources.
    pub fn new(env: &'a Env, owner: &'a str, session: &'a str, run: &'a str) -> Result<Self> {
        let json = env
            .var("RESOURCE_PRICING_JSON")
            .map_err(|_| fail("resource_pricing_not_configured"))?
            .to_string();
        let pricing = ResourcePricing::parse(&json)?;
        Ok(Self {
            env,
            owner,
            session: Some(session),
            run,
            pricing,
        })
    }

    pub fn with_account_pricing(
        env: &'a Env,
        owner: &'a str,
        run: &'a str,
        pricing: ResourcePricing,
    ) -> Result<Self> {
        let pricing = ResourcePricing::parse(&serde_json::to_string(&pricing)?)?;
        Ok(Self {
            env,
            owner,
            session: None,
            run,
            pricing,
        })
    }

    pub fn account(env: &'a Env, owner: &'a str, run: &'a str) -> Result<Self> {
        let json = env
            .var("RESOURCE_PRICING_JSON")
            .map_err(|_| fail("resource_pricing_not_configured"))?
            .to_string();
        Self::with_account_pricing(env, owner, run, ResourcePricing::parse(&json)?)
    }

    fn session_binding(&self) -> JsValue {
        self.session.map(text).unwrap_or(JsValue::NULL)
    }

    pub fn require_tariff(&self, resource: &Resource, unit: &str) -> Result<()> {
        self.pricing.tariff(resource, unit).map(|_| ())
    }

    /// Validate every tariff the run may consume before reserving model credit.
    pub fn preflight(&self, workspace_enabled: bool) -> Result<()> {
        for (resource, unit) in [
            (Resource::Worker, "wall_ms"),
            (Resource::DurableObject, "wall_ms"),
            (Resource::D1, "rows_read"),
            (Resource::D1, "rows_written"),
        ] {
            self.require_tariff(&resource, unit)?;
        }
        if workspace_enabled {
            for (resource, unit) in [
                (Resource::Container, "wall_ms"),
                (Resource::R2Operation, "class_a"),
                (Resource::R2Operation, "class_b"),
                (Resource::R2Storage, "byte_ms"),
            ] {
                self.require_tariff(&resource, unit)?;
            }
        }
        Ok(())
    }

    fn db(&self) -> Result<D1Database> {
        self.env.d1("DB")
    }

    /// Declare a measurement BEFORE its side effect. A crash thereafter leaves a
    /// pending row and its budget reservation held for explicit reconciliation.
    pub async fn require_observation(
        &self,
        event: &str,
        resource: Resource,
        unit: &str,
    ) -> Result<()> {
        if event.is_empty() || event.len() > 256 {
            return Err(fail("invalid_resource_event"));
        }
        let tariff = self.pricing.tariff(&resource, unit)?;
        let resource = resource_name(&resource)?;
        let db = self.db()?;
        db.prepare("INSERT INTO resource_observations (run_id,account_id,session_id,event_key,resource,unit,quantity,price_version,observed_at,status) SELECT ?,?,?,?,?,?,NULL,?,NULL,'pending' WHERE EXISTS (SELECT 1 FROM usage_reservations WHERE id=? AND account_id=? AND session_id IS ? AND state='reserved') ON CONFLICT(run_id,event_key) DO NOTHING")
            .bind(&[text(self.run),text(self.owner),self.session_binding(),text(event),text(&resource),text(unit),text(&tariff.price_version),text(self.run),text(self.owner),self.session_binding()])?
            .run().await?;
        let row = self.observation(event).await?;
        if row.resource != resource || row.unit != unit || row.price_version != tariff.price_version
        {
            return Err(fail("resource_observation_conflict"));
        }
        Ok(())
    }

    async fn observation(&self, event: &str) -> Result<Observation> {
        self.db()?.prepare("SELECT resource,unit,quantity,price_version,status FROM resource_observations WHERE run_id=? AND account_id=? AND session_id IS ? AND event_key=?")
            .bind(&[text(self.run),text(self.owner),self.session_binding(),text(event)])?
            .first(None).await?.ok_or_else(|| fail("resource_observation_missing"))
    }

    pub async fn record_observation(
        &self,
        event: &str,
        resource: Resource,
        unit: &str,
        quantity: u64,
    ) -> Result<()> {
        self.require_observation(event, resource.clone(), unit)
            .await?;
        let row = self.observation(event).await?;
        let quantity_text = quantity.to_string();
        if row
            .quantity
            .as_ref()
            .is_some_and(|previous| previous != &quantity_text)
        {
            return Err(fail("resource_observation_conflict"));
        }
        if row.status == "observed" {
            return Ok(());
        }
        let db = self.db()?;
        let tariff = self.pricing.tariff(&resource, unit)?;
        let now = crate::util::now_ms() as i64;
        // Append first. A crash between writes remains pending; retrying is safe.
        metering::record_resource(
            &db,
            self.run,
            &format!("resource:{event}"),
            quantity,
            tariff,
            now,
        )
        .await?;
        db.prepare("UPDATE resource_observations SET quantity=?,status='observed',observed_at=CAST(? AS INTEGER) WHERE run_id=? AND account_id=? AND event_key=? AND (quantity IS NULL OR quantity=?)")
            .bind(&[text(&quantity_text),text(&now.to_string()),text(self.run),text(self.owner),text(event),text(&quantity_text)])?
            .run().await?;
        if self.observation(event).await?.status != "observed" {
            return Err(fail("resource_observation_pending"));
        }
        Ok(())
    }

    /// Explicit wall time is a published platform tariff, never represented as CPU.
    pub async fn record_wall_time(
        &self,
        event: &str,
        resource: Resource,
        started_ms: u64,
        ended_ms: u64,
    ) -> Result<()> {
        if !matches!(
            resource,
            Resource::Worker | Resource::DurableObject | Resource::Container
        ) {
            return Err(fail("invalid_wall_time_resource"));
        }
        let elapsed = ended_ms
            .checked_sub(started_ms)
            .ok_or_else(|| fail("invalid_measurement_interval"))?;
        self.record_observation(event, resource, "wall_ms", elapsed)
            .await
    }

    pub async fn require_d1(&self, event: &str) -> Result<()> {
        self.require_observation(&format!("{event}:read"), Resource::D1, "rows_read")
            .await?;
        self.require_observation(&format!("{event}:write"), Resource::D1, "rows_written")
            .await
    }

    /// Call require_d1 before executing the query. Missing metadata remains held.
    pub async fn record_d1(&self, event: &str, result: &D1Result) -> Result<()> {
        let meta = result
            .meta()?
            .ok_or_else(|| fail("d1_usage_metadata_missing"))?;
        let read = meta
            .rows_read
            .ok_or_else(|| fail("d1_usage_metadata_missing"))? as u64;
        let written = meta
            .rows_written
            .ok_or_else(|| fail("d1_usage_metadata_missing"))? as u64;
        self.record_observation(&format!("{event}:read"), Resource::D1, "rows_read", read)
            .await?;
        self.record_observation(
            &format!("{event}:write"),
            Resource::D1,
            "rows_written",
            written,
        )
        .await
    }

    /// Bill observed byte-time without hour rounding. Caller tracks object size
    /// over [start,end); a checkpoint's archive size alone is NOT storage duration.
    pub async fn record_storage_interval(
        &self,
        event: &str,
        bytes: u64,
        start_ms: u64,
        end_ms: u64,
    ) -> Result<()> {
        let quantity = byte_milliseconds(bytes, start_ms, end_ms)?;
        self.record_observation(event, Resource::R2Storage, "byte_ms", quantity)
            .await
    }

    /// No external reconciler is assumed. Pending observations forbid release of
    /// the budget hold; operators may fill them with actual delayed measurements.
    pub async fn settle_run(&self) -> Result<ReservationRow> {
        #[derive(Deserialize)]
        struct Counts {
            total: u64,
            pending: u64,
        }
        let counts: Counts = self.db()?.prepare("SELECT COUNT(*) AS total,COALESCE(SUM(CASE WHEN status='pending' THEN 1 ELSE 0 END),0) AS pending FROM resource_observations WHERE run_id=? AND account_id=?")
            .bind(&[text(self.run),text(self.owner)])?.first(None).await?
            .ok_or_else(|| fail("resource_observation_missing"))?;
        if counts.total == 0 || counts.pending != 0 {
            return Err(fail("resource_reconciliation_pending"));
        }
        let db = self.db()?;
        db.prepare("UPDATE usage_reservations SET state='settled',settled_microusd=(SELECT COALESCE(SUM(amount_microusd),0) FROM usage_ledger WHERE reservation_id=?),updated_at=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND state='reserved' AND EXISTS(SELECT 1 FROM resource_observations WHERE run_id=?) AND NOT EXISTS(SELECT 1 FROM resource_observations WHERE run_id=? AND status='pending')")
            .bind(&[text(self.run),text(&crate::util::now_ms().to_string()),text(self.run),text(self.owner),text(self.run),text(self.run)])?.run().await?;
        let row: ReservationRow = db.prepare("SELECT id,account_id,request_hash,CAST(reserved_microusd AS TEXT) AS reserved_microusd,CAST(settled_microusd AS TEXT) AS settled_microusd,state FROM usage_reservations WHERE id=? AND account_id=?")
            .bind(&[text(self.run),text(self.owner)])?.first(None).await?
            .ok_or_else(|| fail("reservation_not_found"))?;
        if row.state != agents_auth::billing::ReservationState::Settled {
            return Err(fail("resource_reconciliation_pending"));
        }
        Ok(row)
    }
}

pub fn byte_milliseconds(bytes: u64, start_ms: u64, end_ms: u64) -> Result<u64> {
    let duration = end_ms
        .checked_sub(start_ms)
        .ok_or_else(|| fail("invalid_measurement_interval"))?;
    bytes
        .checked_mul(duration)
        .ok_or_else(|| fail("resource_quantity_overflow"))
}

fn resource_name(resource: &Resource) -> Result<String> {
    serde_json::to_value(resource)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| fail("invalid_resource"))
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
    use serde_json::json;

    fn pricing() -> serde_json::Value {
        json!({"version":"2026-10-v1","tariffs":[{"resource":"container","unit":"wall_ms","microusd_numerator":1,"quantity_denominator":1000,"price_version":"2026-10-v1"}]})
    }

    #[test]
    fn exact_tariffs_require_nonzero_rates_and_version_match() {
        let valid = pricing();
        assert!(ResourcePricing::parse(&valid.to_string()).is_ok());
        for field in ["microusd_numerator", "quantity_denominator"] {
            let mut invalid = valid.clone();
            invalid["tariffs"][0][field] = json!(0);
            assert!(ResourcePricing::parse(&invalid.to_string()).is_err());
        }
        let mut invalid = valid.clone();
        invalid["version"] = json!("new-version");
        assert!(ResourcePricing::parse(&invalid.to_string()).is_err());
        let mut invalid = valid.clone();
        invalid["tariffs"]
            .as_array_mut()
            .unwrap()
            .push(valid["tariffs"][0].clone());
        assert!(ResourcePricing::parse(&invalid.to_string()).is_err());
    }

    #[test]
    fn storage_integrals_are_exact_and_overflow_checked() {
        assert_eq!(byte_milliseconds(1024, 1000, 2000).unwrap(), 1_024_000);
        assert_eq!(byte_milliseconds(1024, 2000, 2000).unwrap(), 0);
        assert!(byte_milliseconds(1, 2000, 1000).is_err());
        assert!(byte_milliseconds(u64::MAX, 0, 2).is_err());
    }
}
