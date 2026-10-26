//! Retired generations retain IDs until provider deletion is confirmed.

use serde::Deserialize;
use worker::{Env, Result};

use crate::{file_vector, resources::ResourceMeter, util};

#[derive(Deserialize)]
struct Job {
    account_id: String,
    vector_store_id: String,
    file_id: String,
    generation: String,
    service_profile_json: Option<String>,
}

#[derive(Deserialize)]
struct Chunk {
    chunk_id: String,
    vector_status: String,
}

pub(super) async fn run(env: &Env) -> Result<()> {
    env.d1("DB")?.prepare("UPDATE vector_stores SET status='expired' WHERE id IN (SELECT id FROM vector_stores WHERE status='completed' AND expires_after_days IS NOT NULL AND last_active_at+expires_after_days*86400000<=CAST(?1 AS INTEGER) LIMIT 64)")
        .bind(&[util::now_ms().to_string().into()])?.run().await?;
    let jobs=env.d1("DB")?.prepare("SELECT account_id,vector_store_id,file_id,generation,service_profile_json FROM file_search_cleanup_jobs WHERE retry_at<=CAST(?1 AS INTEGER) AND lease_until<=CAST(?1 AS INTEGER) ORDER BY retry_at LIMIT 4")
        .bind(&[util::now_ms().to_string().into()])?.all().await?.results::<Job>()?;
    for job in jobs {
        if cleanup(env, &job).await.is_err() {
            util::emit("file_search.cleanup_deferred", serde_json::json!({}));
        }
    }
    Ok(())
}

async fn cleanup(env: &Env, job: &Job) -> Result<()> {
    let lease = uuid::Uuid::new_v4().simple().to_string();
    let resource = format!("file-cleanup:{lease}");
    crate::allowances::sync(env, &job.account_id).await?;
    crate::reconcile::maintenance_account_reservation(env, &job.account_id, &resource).await?;
    let meter = ResourceMeter::account(env, &job.account_id, &resource)?;
    meter.require_d1("claim").await?;
    let claimed=env.d1("DB")?.prepare("UPDATE file_search_cleanup_jobs SET lease_id=?5,lease_until=CAST(?6 AS INTEGER),retry_at=CAST(?6 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND lease_until<=CAST(?7 AS INTEGER) RETURNING file_id")
        .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into(),lease.as_str().into(),(util::now_ms()+120000).to_string().into(),util::now_ms().to_string().into()])?.all().await?;
    meter.record_d1("claim", &claimed).await?;
    if claimed.results::<serde_json::Value>()?.is_empty() {
        meter.settle_run().await?;
        return Ok(());
    }
    let outcome = cleanup_batch(env, job, &lease, &meter).await;
    meter.require_d1("release").await?;
    let result=env.d1("DB")?.prepare("UPDATE file_search_cleanup_jobs SET lease_id=NULL,lease_until=0,retry_at=CAST(?6 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND lease_id=?5")
        .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into(),lease.as_str().into(),(util::now_ms()+30000).to_string().into()])?.run().await?;
    meter.record_d1("release", &result).await?;
    if outcome.is_ok() {
        meter.settle_run().await?;
    }
    outcome
}

async fn cleanup_batch(env: &Env, job: &Job, lease: &str, meter: &ResourceMeter<'_>) -> Result<()> {
    meter.require_d1("chunks").await?;
    let result=env.d1("DB")?.prepare("SELECT chunk_id,vector_status FROM file_search_chunks WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 ORDER BY ordinal LIMIT 128")
        .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into()])?.all().await?;
    meter.record_d1("chunks", &result).await?;
    let chunks = result.results::<Chunk>()?;
    if chunks.is_empty() {
        return finish_job(env, job, lease, meter).await;
    }
    let mut removable = chunks
        .iter()
        .filter(|chunk| {
            matches!(
                chunk.vector_status.as_str(),
                "pending" | "embedding" | "embedded"
            )
        })
        .map(|chunk| chunk.chunk_id.clone())
        .collect::<Vec<_>>();
    let uploaded = chunks
        .iter()
        .filter(|chunk| {
            !matches!(
                chunk.vector_status.as_str(),
                "pending" | "embedding" | "embedded"
            )
        })
        .map(|chunk| chunk.chunk_id.clone())
        .collect::<Vec<_>>();
    if !uploaded.is_empty() {
        let profile: file_vector::VectorServiceProfile =
            serde_json::from_str(job.service_profile_json.as_deref().ok_or_else(|| {
                worker::Error::RustError("file_search_cleanup_profile_missing".into())
            })?)?;
        profile.validate()?;
        let before = file_vector::existing_ids(env, &profile, &uploaded, meter, "existing").await?;
        let deletable = chunks
            .iter()
            .filter(|chunk| {
                uploaded.contains(&chunk.chunk_id)
                    && (chunk.vector_status != "upserting" || before.contains(&chunk.chunk_id))
            })
            .map(|chunk| chunk.chunk_id.clone())
            .collect::<Vec<_>>();
        if !deletable.is_empty() {
            file_vector::delete_ids(env, &profile, &deletable, meter, "delete").await?;
            let after =
                file_vector::existing_ids(env, &profile, &deletable, meter, "removed").await?;
            removable.extend(deletable.into_iter().filter(|id| !after.contains(id)));
        }
    }
    if !removable.is_empty() {
        meter.require_d1("forget").await?;
        let result=env.d1("DB")?.prepare("DELETE FROM file_search_chunks WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND chunk_id IN (SELECT value FROM json_each(?5)) AND EXISTS(SELECT 1 FROM file_search_cleanup_jobs j WHERE j.account_id=?1 AND j.vector_store_id=?2 AND j.file_id=?3 AND j.generation=?4 AND j.lease_id=?6)")
            .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into(),serde_json::to_string(&removable)?.into(),lease.into()])?.run().await?;
        meter.record_d1("forget", &result).await?;
    }
    Ok(())
}

async fn finish_job(env: &Env, job: &Job, lease: &str, meter: &ResourceMeter<'_>) -> Result<()> {
    meter.require_d1("finish").await?;
    let result=env.d1("DB")?.prepare("DELETE FROM file_search_cleanup_jobs WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND lease_id=?5 AND NOT EXISTS(SELECT 1 FROM file_search_chunks c WHERE c.account_id=?1 AND c.vector_store_id=?2 AND c.file_id=?3 AND c.generation=?4)")
        .bind(&[job.account_id.as_str().into(),job.vector_store_id.as_str().into(),job.file_id.as_str().into(),job.generation.as_str().into(),lease.into()])?.run().await?;
    meter.record_d1("finish", &result).await
}
