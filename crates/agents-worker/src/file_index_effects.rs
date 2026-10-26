//! Durable derived-effect phases. Recovery never buys an embedding twice.

use serde_json::Value;
use worker::{Env, Error, Result};

use super::{EmbeddingProfile, IngestJob, PendingChunk, values, vector_id};
use crate::resources::ResourceMeter;

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

pub fn batch_state(chunks: &[PendingChunk]) -> Result<&str> {
    let first = chunks
        .first()
        .ok_or_else(|| fail("file_search_empty_effect_batch"))?;
    if chunks
        .iter()
        .any(|chunk| chunk.vector_status != first.vector_status)
    {
        return Err(fail("file_search_effect_batch_changed"));
    }
    Ok(&first.vector_status)
}

pub fn ids(job: &IngestJob, chunks: &[PendingChunk]) -> Vec<String> {
    chunks
        .iter()
        .map(|chunk| {
            vector_id(
                &job.vector_store_id,
                &job.file_id,
                &job.generation,
                chunk.ordinal,
            )
        })
        .collect()
}

pub async fn begin(
    env: &Env,
    job: &IngestJob,
    chunks: &[PendingChunk],
    phase: &str,
    effect: &str,
    meter: &ResourceMeter<'_>,
) -> Result<bool> {
    let previous = match phase {
        "embedding" => "pending",
        "upserting" => "embedded",
        _ => return Err(fail("invalid_file_search_effect_phase")),
    };
    let event = format!("{effect}:fence");
    let bindings = vec![
        job.account_id.clone(),
        job.vector_store_id.clone(),
        job.file_id.clone(),
        job.generation.clone(),
        job.lease_id.clone(),
        crate::util::now_ms().to_string(),
        serde_json::to_string(&ids(job, chunks))?,
        phase.into(),
        effect.into(),
        previous.into(),
    ];
    meter.require_d1(&event).await?;
    let result=env.d1("DB")?.prepare("UPDATE file_search_chunks SET vector_status=?8,vector_effect_event=?9,vector_effect_started_at=CAST(?6 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND chunk_id IN (SELECT value FROM json_each(?7)) AND vector_status=?10 AND EXISTS(SELECT 1 FROM vector_store_files vf JOIN space_files sf ON sf.id=vf.file_id AND sf.account_id=vf.account_id JOIN vector_stores vs ON vs.id=vf.vector_store_id AND vs.account_id=vf.account_id WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.file_id=?3 AND vf.generation=?4 AND vf.lease_id=?5 AND vf.status='in_progress' AND vf.lease_until>CAST(?6 AS INTEGER) AND sf.status='processed' AND sf.sha256=vf.file_sha256 AND vs.status='completed' AND (vs.expires_after_days IS NULL OR vs.last_active_at+vs.expires_after_days*86400000>unixepoch()*1000)) RETURNING chunk_id")
        .bind(&values(&bindings))?.all().await?;
    meter.record_d1(&event, &result).await?;
    Ok(result.results::<Value>()?.len() == chunks.len())
}

pub async fn save_vectors(
    env: &Env,
    job: &IngestJob,
    chunks: &[PendingChunk],
    vectors: &[Vec<f32>],
    profile: &EmbeddingProfile,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    if chunks.len() != vectors.len() || chunks.is_empty() {
        return Err(fail("invalid_file_search_embedding_shape"));
    }
    let db = env.d1("DB")?;
    let mut statements = Vec::with_capacity(chunks.len());
    for (chunk, vector) in chunks.iter().zip(vectors) {
        profile.validate_vector(vector)?;
        let bindings = vec![
            job.account_id.clone(),
            job.vector_store_id.clone(),
            job.file_id.clone(),
            job.generation.clone(),
            job.lease_id.clone(),
            crate::util::now_ms().to_string(),
            vector_id(
                &job.vector_store_id,
                &job.file_id,
                &job.generation,
                chunk.ordinal,
            ),
            serde_json::to_string(vector)?,
        ];
        statements.push(db.prepare("UPDATE file_search_chunks SET vector_status='embedded',vector_json=?8,vector_effect_event=NULL WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND chunk_id=?7 AND vector_status='embedding' AND EXISTS(SELECT 1 FROM vector_store_files vf WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.file_id=?3 AND vf.generation=?4 AND vf.lease_id=?5 AND vf.status='in_progress' AND vf.lease_until>CAST(?6 AS INTEGER)) RETURNING chunk_id").bind(&values(&bindings))?);
    }
    for ordinal in 0..statements.len() {
        meter.require_d1(&format!("{event}:{ordinal}")).await?;
    }
    let results = db.batch(statements).await?;
    for (ordinal, result) in results.iter().enumerate() {
        meter
            .record_d1(&format!("{event}:{ordinal}"), result)
            .await?;
        if result.results::<Value>()?.len() != 1 {
            return Err(fail("file_search_effect_lease_lost"));
        }
    }
    Ok(())
}

pub fn read_vectors(chunks: &[PendingChunk], profile: &EmbeddingProfile) -> Result<Vec<Vec<f32>>> {
    chunks
        .iter()
        .map(|chunk| {
            let vector: Vec<f32> = serde_json::from_str(
                chunk
                    .vector_json
                    .as_deref()
                    .ok_or_else(|| fail("file_search_embedding_receipt_missing"))?,
            )?;
            profile.validate_vector(&vector)?;
            Ok(vector)
        })
        .collect()
}

pub async fn save_mutation(
    env: &Env,
    job: &IngestJob,
    chunks: &[PendingChunk],
    mutation: &str,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    let bindings = vec![
        job.account_id.clone(),
        job.vector_store_id.clone(),
        job.file_id.clone(),
        job.generation.clone(),
        job.lease_id.clone(),
        crate::util::now_ms().to_string(),
        serde_json::to_string(&ids(job, chunks))?,
        mutation.into(),
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE file_search_chunks SET vector_status='submitted',vector_mutation_id=?8 WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND chunk_id IN (SELECT value FROM json_each(?7)) AND vector_status='upserting' AND EXISTS(SELECT 1 FROM vector_store_files vf WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.file_id=?3 AND vf.generation=?4 AND vf.lease_id=?5 AND vf.status='in_progress' AND vf.lease_until>CAST(?6 AS INTEGER)) RETURNING chunk_id")
        .bind(&values(&bindings))?.all().await?;
    meter.record_d1(event, &result).await?;
    if result.results::<Value>()?.len() != chunks.len() {
        return Err(fail("file_search_effect_lease_lost"));
    }
    Ok(())
}

pub fn original_effect(chunks: &[PendingChunk]) -> Result<&str> {
    let first = chunks
        .first()
        .and_then(|chunk| chunk.vector_effect_event.as_deref())
        .ok_or_else(|| fail("file_search_effect_receipt_missing"))?;
    if chunks
        .iter()
        .any(|chunk| chunk.vector_effect_event.as_deref() != Some(first))
    {
        return Err(fail("file_search_effect_batch_changed"));
    }
    Ok(first)
}
