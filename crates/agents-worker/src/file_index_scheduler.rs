//! Recoverable D1 ingest queue; no model or index is created by token counting.

use crate::{
    file_ingest,
    file_vector::{self, IngestProgress},
    resources::ResourceMeter,
    util,
};
use serde::Deserialize;
use worker::{Env, Result};

#[path = "file_index_gc.rs"]
mod gc;

#[derive(Deserialize)]
struct Job {
    account_id: String,
    vector_store_id: String,
    file_id: String,
    generation: String,
    service_profile_json: Option<String>,
}

pub(crate) async fn run(env: &Env) -> Result<()> {
    gc::run(env).await?;
    if env.var("FILE_SEARCH_PROFILE_JSON").is_err() {
        return Ok(());
    }
    // This bounded cross-account discovery is platform maintenance, not an
    // owner query. Each selected job funds its own effects before claiming work.
    let jobs=env.d1("DB")?.prepare("SELECT vf.account_id,vf.vector_store_id,vf.file_id,vf.generation,vf.service_profile_json FROM vector_store_files vf WHERE vf.status='in_progress' AND vf.removed_at IS NULL AND vf.retry_at<=CAST(?1 AS INTEGER) AND vf.lease_until<=CAST(?1 AS INTEGER) ORDER BY vf.retry_at,vf.created_at LIMIT 4")
        .bind(&[util::now_ms().to_string().into()])?.all().await?.results::<Job>()?;
    for job in jobs {
        if run_job(env, &job, util::now_ms() + 25000).await.is_err() {
            util::emit("file_search.ingest_deferred", serde_json::json!({}));
            // Unfunded/operator-blocked jobs must not monopolize the bounded
            // cross-account queue. This scheduling update is platform overhead;
            // it cannot authorize a provider effect or release a resource hold.
            env.d1("DB")?.prepare("UPDATE vector_store_files SET retry_at=CAST(?5 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND status='in_progress'")
                .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into(),(util::now_ms()+300000).to_string().into()])?.run().await?;
        }
    }
    settle_terminal(env, None).await?;
    Ok(())
}

pub(crate) async fn kickoff(
    env: Env,
    owner: String,
    store: String,
    file: Option<String>,
) -> Result<()> {
    let deadline = util::now_ms() + 26000;
    let file = file
        .map(wasm_bindgen::JsValue::from)
        .unwrap_or(wasm_bindgen::JsValue::NULL);
    let jobs=env.d1("DB")?.prepare("SELECT account_id,vector_store_id,file_id,generation,service_profile_json FROM vector_store_files WHERE account_id=?1 AND vector_store_id=?2 AND (?3 IS NULL OR file_id=?3) AND status='in_progress' AND removed_at IS NULL AND retry_at<=CAST(?4 AS INTEGER) AND lease_until<=CAST(?4 AS INTEGER) ORDER BY retry_at,created_at LIMIT 4")
        .bind(&[owner.as_str().into(),store.as_str().into(),file,util::now_ms().to_string().into()])?.all().await?.results::<Job>()?;
    for job in jobs {
        if util::now_ms() + 2000 >= deadline {
            break;
        }
        if run_job(&env, &job, deadline).await.is_err() {
            util::emit("file_search.kickoff_deferred", serde_json::json!({}));
        }
    }
    settle_terminal(&env, Some(&owner)).await
}

async fn run_job(env: &Env, job: &Job, deadline: u64) -> Result<()> {
    let profile: file_vector::VectorServiceProfile =
        serde_json::from_str(job.service_profile_json.as_deref().ok_or_else(|| {
            worker::Error::RustError("file_search_ingest_profile_missing".into())
        })?)?;
    profile.validate()?;
    profile.validate_bindings(env)?;
    let run = format!("file-ingest:{}", job.generation);
    let meter = ResourceMeter::account(env, &job.account_id, &run)?;
    file_ingest::preflight(&profile, &meter)?;
    crate::allowances::sync(env, &job.account_id).await?;
    crate::reconcile::maintenance_account_reservation(env, &job.account_id, &run).await?;
    if util::now_ms() + 1000 >= deadline {
        return Ok(());
    }
    let attempt = uuid::Uuid::new_v4().simple().to_string();
    let event = format!("ingest:{attempt}");
    let wall = format!("{event}:worker");
    meter
        .require_observation(&wall, agents_auth::billing::Resource::Worker, "wall_ms")
        .await?;
    let started = util::now_ms();
    invalidate_source(env, job, &meter, &format!("{event}:invalidate")).await?;
    let task = Box::pin(file_vector::ingest_turn(
        env,
        &profile,
        &job.account_id,
        &job.vector_store_id,
        &job.file_id,
        1,
        &meter,
        &event,
    ));
    let timer = Box::pin(worker::Delay::from(std::time::Duration::from_millis(
        deadline.saturating_sub(util::now_ms()),
    )));
    let outcome = match futures::future::select(task, timer).await {
        futures::future::Either::Left((outcome, _)) => outcome,
        futures::future::Either::Right(_) => Err(worker::Error::RustError(
            "file_search_ingest_turn_timeout".into(),
        )),
    };
    meter
        .record_wall_time(
            &wall,
            agents_auth::billing::Resource::Worker,
            started,
            util::now_ms(),
        )
        .await?;
    match outcome {
        Ok(IngestProgress::Completed) => {
            meter.settle_run().await?;
        }
        Ok(_) => {}
        Err(_) => util::emit(
            "file_search.ingest_deferred",
            serde_json::json!({"operation":run}),
        ),
    }
    Ok(())
}

async fn invalidate_source(
    env: &Env,
    job: &Job,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_store_files SET status='cancelled',lease_id=NULL,lease_until=0 WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND status='in_progress' AND (NOT EXISTS(SELECT 1 FROM space_files sf WHERE sf.id=file_id AND sf.account_id=?1 AND sf.status='processed' AND sf.sha256=file_sha256) OR NOT EXISTS(SELECT 1 FROM vector_stores vs WHERE vs.id=vector_store_id AND vs.account_id=?1 AND vs.status='completed' AND (vs.expires_after_days IS NULL OR vs.last_active_at+vs.expires_after_days*86400000>unixepoch()*1000)))")
        .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into()])?.run().await?;
    meter.record_d1(event, &result).await
}

async fn settle_terminal(env: &Env, owner: Option<&str>) -> Result<()> {
    #[derive(Deserialize)]
    struct Held {
        account_id: String,
        run_id: String,
    }
    let jobs=env.d1("DB")?.prepare("SELECT r.account_id,r.id AS run_id FROM usage_reservations r WHERE (?1 IS NULL OR r.account_id=?1) AND r.id LIKE 'file-ingest:%' AND r.session_id IS NULL AND r.state='reserved' AND EXISTS(SELECT 1 FROM resource_observations o WHERE o.run_id=r.id AND o.account_id=r.account_id) AND NOT EXISTS(SELECT 1 FROM resource_observations o WHERE o.run_id=r.id AND o.account_id=r.account_id AND o.status='pending') AND NOT EXISTS(SELECT 1 FROM vector_store_files vf WHERE vf.account_id=r.account_id AND r.id='file-ingest:'||vf.generation AND vf.status='in_progress') LIMIT 16")
        .bind(&[owner.map(wasm_bindgen::JsValue::from).unwrap_or(wasm_bindgen::JsValue::NULL)])?.all().await?.results::<Held>()?;
    for job in jobs {
        // Pending/unknown observations deliberately prevent settlement. They are
        // financial evidence, not an invitation to discard or fabricate usage.
        let meter = ResourceMeter::account(env, &job.account_id, &job.run_id);
        if let Ok(meter) = meter {
            let _ = meter.settle_run().await;
        }
    }
    Ok(())
}
