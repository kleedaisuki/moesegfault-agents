//! Bounded generation/operation funding. Financial identities and tariffs never
//! change on retry; unknown observations retain only their own resource hold.
use crate::{
    file_index::IngestJob,
    file_vector::Allocation,
    metering,
    resources::{ResourceMeter, ResourcePricing},
    util,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use worker::{Env, Error, Result};

pub(crate) struct Scope<'a> {
    pub owner: &'a str,
    pub store: &'a str,
    pub file: &'a str,
    pub generation: &'a str,
    pub lease: Option<&'a str>,
}
impl<'a> From<&'a IngestJob> for Scope<'a> {
    fn from(job: &'a IngestJob) -> Self {
        Self {
            owner: &job.account_id,
            store: &job.vector_store_id,
            file: &job.file_id,
            generation: &job.generation,
            lease: Some(&job.lease_id),
        }
    }
}
#[derive(Deserialize)]
pub(crate) struct Funding {
    pub id: String,
    pub pricing_json: String,
    #[serde(deserialize_with = "amount")]
    pub amount_microusd: i64,
    quantity: String,
    calls: u64,
}
fn amount<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<i64, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

impl Funding {
    pub fn meter<'a>(&'a self, env: &'a Env, owner: &'a str) -> Result<ResourceMeter<'a>> {
        ResourceMeter::with_account_pricing(
            env,
            owner,
            &self.id,
            ResourcePricing::parse(&self.pricing_json)?,
        )
    }
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
pub(crate) fn io_budget(env: &Env) -> Result<i64> {
    let value = env
        .var("RUN_RESOURCE_RESERVATION_MICROUSD")?
        .to_string()
        .parse::<i64>()
        .map_err(|_| fail("invalid_resource_reservation"))?;
    if value <= 0 {
        return Err(fail("invalid_resource_reservation"));
    }
    Ok(value)
}
fn identity(scope: &Scope<'_>, operation: &str) -> Result<String> {
    Ok(format!(
        "file-ingest:{}:{:x}",
        scope.generation,
        Sha256::digest(serde_json::to_vec(&(
            scope.owner,
            scope.store,
            scope.file,
            scope.generation,
            operation
        ))?)
    ))
}
/// Quantity is an explicit platform allocation, never provider token usage.
pub(crate) async fn fund(
    env: &Env,
    scope: &Scope<'_>,
    operation: &str,
    allocation: Option<(&Allocation, u64, u64)>,
    resume_overhead: i64,
) -> Result<Funding> {
    let id = identity(scope, operation)?;
    let db = env.d1("DB")?;
    let mut saved=db.prepare("SELECT id,pricing_json,CAST(amount_microusd AS TEXT) AS amount_microusd,quantity,calls FROM file_search_funding WHERE id=?1 AND account_id=?2").bind(&[id.as_str().into(),scope.owner.into()])?.first::<Funding>(None).await?;
    let (quantity, calls) = allocation.map_or((0, 0), |(_, q, c)| (q, c));
    if saved.is_none() {
        let mut pricing = ResourcePricing::parse(&env.var("RESOURCE_PRICING_JSON")?.to_string())?;
        // Provider batches need one tariff, not repeated copies of the complete
        // platform catalog beside every chunk group. I/O turns retain preflight.
        if let Some((allocation, _, _)) = allocation {
            pricing.tariffs = vec![
                pricing
                    .tariff(&allocation.resource, &allocation.unit)?
                    .clone(),
            ];
        } else if operation.starts_with("stage:") {
            pricing.tariffs = pricing
                .tariffs
                .into_iter()
                .filter(|tariff| tariff.resource == agents_auth::billing::Resource::D1)
                .collect();
        }
        let amount = match allocation {
            Some((a, q, c)) => pricing
                .tariff(&a.resource, &a.unit)?
                .charge(q)
                .map_err(|_| fail("invalid_file_search_funding_quote"))?
                .amount_microusd
                .get()
                .checked_mul(
                    i64::try_from(c).map_err(|_| fail("invalid_file_search_funding_quote"))?,
                )
                .filter(|v| *v > 0)
                .ok_or_else(|| fail("invalid_file_search_funding_quote"))?,
            None => io_budget(env)?,
        };
        db.prepare("INSERT INTO file_search_funding(id,account_id,vector_store_id,file_id,generation,operation,quantity,calls,pricing_json,amount_microusd,created_at_ms) SELECT ?1,?2,?3,?4,?5,?6,?7,CAST(?8 AS INTEGER),?9,CAST(?10 AS INTEGER),CAST(?11 AS INTEGER) WHERE EXISTS(SELECT 1 FROM vector_store_files vf JOIN space_files sf ON sf.id=vf.file_id AND sf.account_id=vf.account_id JOIN vector_stores vs ON vs.id=vf.vector_store_id AND vs.account_id=vf.account_id WHERE vf.account_id=?2 AND vf.vector_store_id=?3 AND vf.file_id=?4 AND vf.generation=?5 AND vf.status='in_progress' AND vf.removed_at IS NULL AND vf.funding_blocked=0 AND (?12 IS NULL OR (vf.lease_id=?12 AND vf.lease_until>CAST(?11 AS INTEGER))) AND sf.status='processed' AND sf.sha256=vf.file_sha256 AND vs.status='completed') ON CONFLICT(id) DO NOTHING")
   .bind(&[id.as_str().into(),scope.owner.into(),scope.store.into(),scope.file.into(),scope.generation.into(),operation.into(),quantity.to_string().into(),calls.to_string().into(),serde_json::to_string(&pricing)?.into(),amount.to_string().into(),util::now_ms().to_string().into(),scope.lease.map(Into::into).unwrap_or(wasm_bindgen::JsValue::NULL)])?.run().await?;
        saved=db.prepare("SELECT id,pricing_json,CAST(amount_microusd AS TEXT) AS amount_microusd,quantity,calls FROM file_search_funding WHERE id=?1 AND account_id=?2").bind(&[id.as_str().into(),scope.owner.into()])?.first::<Funding>(None).await?;
    }
    let saved = saved.ok_or_else(|| fail("file_search_funding_lease_lost"))?;
    if saved.quantity != quantity.to_string() || saved.calls != calls {
        return Err(fail("file_search_funding_conflict"));
    }
    let valid=db.prepare("SELECT vf.file_id FROM vector_store_files vf JOIN space_files sf ON sf.id=vf.file_id AND sf.account_id=vf.account_id JOIN vector_stores vs ON vs.id=vf.vector_store_id AND vs.account_id=vf.account_id WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.file_id=?3 AND vf.generation=?4 AND vf.status='in_progress' AND vf.removed_at IS NULL AND vf.funding_blocked=0 AND (?5 IS NULL OR (vf.lease_id=?5 AND vf.lease_until>CAST(?6 AS INTEGER))) AND sf.status='processed' AND sf.sha256=vf.file_sha256 AND vs.status='completed' AND (vs.expires_after_days IS NULL OR vs.last_active_at+vs.expires_after_days*86400000>CAST(?6 AS INTEGER))")
        .bind(&[scope.owner.into(),scope.store.into(),scope.file.into(),scope.generation.into(),scope.lease.map(Into::into).unwrap_or(wasm_bindgen::JsValue::NULL),util::now_ms().to_string().into()])?.first::<serde_json::Value>(None).await?;
    if valid.is_none() {
        return Err(fail("file_search_funding_lease_lost"));
    }
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &saved.id,
            &saved.pricing_json,
            &saved.quantity,
            saved.calls,
            saved.amount_microusd
        ))?)
    );
    let outcome = metering::reserve(
        &db,
        scope.owner,
        None,
        &saved.id,
        &saved.id,
        &hash,
        saved.amount_microusd,
        util::now_ms() as i64,
    )
    .await;
    if let Err(error) = outcome {
        if error.to_string().contains("budget_exceeded") {
            pause(
                env,
                scope,
                saved
                    .amount_microusd
                    .checked_add(resume_overhead)
                    .ok_or_else(|| fail("invalid_file_search_funding_quote"))?,
            )
            .await?;
            return Err(fail("file_search_budget_required"));
        }
        return Err(error);
    }
    Ok(saved)
}
async fn pause(env: &Env, scope: &Scope<'_>, required: i64) -> Result<()> {
    let message=serde_json::json!({"code":"server_error","message":"file_search_budget_required: add resource credit and explicitly retry this membership; submitted effects are not purchased again"}).to_string();
    env.d1("DB")?.prepare("UPDATE vector_store_files SET funding_blocked=1,funding_required_microusd=CAST(?6 AS INTEGER),last_error_json=?7,lease_id=NULL,lease_until=0 WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND status='in_progress' AND (?5 IS NULL AND lease_until<=CAST(?8 AS INTEGER) OR lease_id=?5)")
  .bind(&[scope.owner.into(),scope.store.into(),scope.file.into(),scope.generation.into(),scope.lease.map(Into::into).unwrap_or(wasm_bindgen::JsValue::NULL),required.to_string().into(),message.into(),util::now_ms().to_string().into()])?.run().await?;
    Ok(())
}
/// Reconcile the original upsert allocation only after actual native visibility.
pub(crate) async fn settle_upsert(
    env: &Env,
    job: &IngestJob,
    id: &str,
    allocation: &Allocation,
    quantity: u64,
) -> Result<()> {
    let saved=env.d1("DB")?.prepare("SELECT id,pricing_json,CAST(amount_microusd AS TEXT) AS amount_microusd,quantity,calls FROM file_search_funding WHERE id=?1 AND account_id=?2 AND vector_store_id=?3 AND file_id=?4 AND generation=?5").bind(&[id.into(),job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into()])?.first::<Funding>(None).await?;
    if let Some(saved) = saved {
        let meter = saved.meter(env, &job.account_id)?;
        meter
            .record_observation(
                "provider",
                allocation.resource.clone(),
                &allocation.unit,
                quantity,
            )
            .await?;
        meter.settle_run().await?;
    } else {
        // Previously deployed generations retain their original immutable hold.
        let run = format!("file-ingest:{}", job.generation);
        let meter = ResourceMeter::account(env, &job.account_id, &run)?;
        meter
            .record_observation(id, allocation.resource.clone(), &allocation.unit, quantity)
            .await?;
    }
    Ok(())
}
pub(crate) async fn settle_known(env: &Env) -> Result<()> {
    let db = env.d1("DB")?;
    let rows=db.prepare("SELECT f.id,f.account_id AS owner,f.pricing_json,CAST(f.amount_microusd AS TEXT) AS amount_microusd,f.quantity,f.calls FROM file_search_funding f JOIN usage_reservations r ON r.id=f.id AND r.account_id=f.account_id WHERE r.state='reserved' AND NOT EXISTS(SELECT 1 FROM resource_observations o WHERE o.run_id=f.id AND o.status='pending') AND (EXISTS(SELECT 1 FROM resource_observations o WHERE o.run_id=f.id) OR (f.operation LIKE 'turn:%' AND f.created_at_ms<CAST(?1 AS INTEGER)) OR NOT EXISTS(SELECT 1 FROM vector_store_files vf WHERE vf.account_id=f.account_id AND vf.vector_store_id=f.vector_store_id AND vf.file_id=f.file_id AND vf.generation=f.generation AND vf.status='in_progress' AND vf.removed_at IS NULL)) LIMIT 32").bind(&[util::now_ms().saturating_sub(120000).to_string().into()])?.all().await?.results::<serde_json::Value>()?;
    for row in rows {
        let owner = row["owner"]
            .as_str()
            .ok_or_else(|| fail("invalid_file_search_funding"))?
            .to_string();
        let plan: Funding = serde_json::from_value(row)?;
        let meter = plan.meter(env, &owner)?;
        let observed = db
            .prepare("SELECT event_key FROM resource_observations WHERE run_id=?1 LIMIT 1")
            .bind(&[plan.id.as_str().into()])?
            .first::<serde_json::Value>(None)
            .await?;
        if observed.is_some() {
            let _ = meter.settle_run().await;
        } else {
            // Test absence atomically against a concurrent declaration of an
            // unknown effect; a read-then-release race would discard its hold.
            db.prepare("UPDATE usage_reservations SET state='released',updated_at=CAST(?3 AS INTEGER) WHERE id=?1 AND account_id=?2 AND state='reserved' AND NOT EXISTS(SELECT 1 FROM resource_observations WHERE run_id=?1) AND NOT EXISTS(SELECT 1 FROM usage_ledger WHERE reservation_id=?1)")
                .bind(&[plan.id.as_str().into(),owner.as_str().into(),util::now_ms().to_string().into()])?.run().await?;
        }
    }
    Ok(())
}
