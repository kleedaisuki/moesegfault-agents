//! Derived retrieval state. Source ownership and publication state are checked on every read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result};

use crate::{
    file_search::{FileSearchOptions, Hit, lexical_query},
    resources::ResourceMeter,
};

const LIVE: &str = "c.account_id=?1 AND vs.account_id=?1 AND vf.account_id=?1 AND sf.account_id=?1 AND vs.status='completed' AND vf.status='completed' AND sf.status='processed' AND sf.sha256=c.file_sha256 AND vf.file_sha256=c.file_sha256 AND vf.generation=c.generation AND c.embedding_model=vf.embedding_model AND c.embedding_revision=vf.embedding_revision AND c.embedding_dimensions=vf.embedding_dimensions";
const JOINS: &str = "JOIN vector_store_files vf ON vf.vector_store_id=c.vector_store_id AND vf.file_id=c.file_id JOIN vector_stores vs ON vs.id=c.vector_store_id JOIN space_files sf ON sf.id=c.file_id";

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingProfile {
    pub model: String,
    pub revision: String,
    pub dimensions: usize,
    pub input_byte_limit: usize,
}

impl EmbeddingProfile {
    pub fn validate(&self) -> Result<()> {
        if self.model.is_empty()
            || self.revision.is_empty()
            || !(1..=1536).contains(&self.dimensions)
            || !(4..=32768).contains(&self.input_byte_limit)
        {
            return Err(fail("invalid_file_search_embedding_profile"));
        }
        Ok(())
    }

    pub fn validate_vector(&self, vector: &[f32]) -> Result<()> {
        self.validate()?;
        if vector.len() != self.dimensions
            || vector.iter().any(|v| !v.is_finite())
            || vector.iter().all(|v| *v == 0.0)
        {
            return Err(fail("invalid_file_search_embedding"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Chunk {
    pub ordinal: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChunkingStrategy {
    #[default]
    Auto,
    Static {
        r#static: StaticChunking,
    },
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StaticChunking {
    pub max_chunk_size_tokens: usize,
    pub chunk_overlap_tokens: usize,
}

impl ChunkingStrategy {
    pub fn parameters(&self) -> Result<(usize, usize)> {
        let (size, overlap) = match self {
            Self::Auto => (800, 400),
            Self::Static { r#static } => (
                r#static.max_chunk_size_tokens,
                r#static.chunk_overlap_tokens,
            ),
        };
        if !(100..=4096).contains(&size) || overlap > size / 2 {
            return Err(fail("invalid_vector_store_chunking_strategy"));
        }
        Ok((size, overlap))
    }
}

pub async fn pending_chunks(
    env: &Env,
    job: &IngestJob,
    profile: &EmbeddingProfile,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Vec<Chunk>> {
    let bindings = vec![
        job.account_id.clone(),
        job.vector_store_id.clone(),
        job.file_id.clone(),
        job.generation.clone(),
        profile.model.clone(),
        profile.dimensions.to_string(),
        profile.revision.clone(),
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("SELECT ordinal,byte_start,byte_end,text FROM file_search_chunks WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND embedding_model=?5 AND embedding_dimensions=CAST(?6 AS INTEGER) AND embedding_revision=?7 AND vector_status='pending' ORDER BY ordinal LIMIT 32")
        .bind(&values(&bindings))?.all().await?;
    meter.record_d1(event, &result).await?;
    result.results()
}

pub async fn renew_job(
    env: &Env,
    job: &IngestJob,
    now_ms: u64,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<bool> {
    let until = now_ms
        .checked_add(120_000)
        .ok_or_else(|| fail("file_search_clock_overflow"))?;
    let bindings = vec![
        job.account_id.clone(),
        job.vector_store_id.clone(),
        job.file_id.clone(),
        job.generation.clone(),
        job.lease_id.clone(),
        now_ms.to_string(),
        until.to_string(),
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_store_files SET lease_until=CAST(?7 AS INTEGER),updated_at=CAST(?6 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND lease_id=?5 AND status='in_progress' AND lease_until>CAST(?6 AS INTEGER) AND EXISTS(SELECT 1 FROM space_files sf WHERE sf.id=file_id AND sf.account_id=?1 AND sf.status='processed' AND sf.sha256=file_sha256) RETURNING file_id")
        .bind(&values(&bindings))?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(!result.results::<Value>()?.is_empty())
}

/// Called only after a successful native query visibility barrier for every ID.
pub async fn mark_visible(
    env: &Env,
    job: &IngestJob,
    ids: &[String],
    mutation: &str,
    now_ms: u64,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    if ids.is_empty() || ids.len() > 32 || mutation.is_empty() || mutation.len() > 256 {
        return Err(fail("invalid_file_search_visibility_batch"));
    }
    let bindings = vec![
        job.account_id.clone(),
        job.vector_store_id.clone(),
        job.file_id.clone(),
        job.generation.clone(),
        job.lease_id.clone(),
        now_ms.to_string(),
        serde_json::to_string(ids)?,
        mutation.to_string(),
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE file_search_chunks SET vector_status='visible',vector_mutation_id=?8 WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND chunk_id IN (SELECT value FROM json_each(?7)) AND EXISTS(SELECT 1 FROM vector_store_files vf WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.file_id=?3 AND vf.generation=?4 AND vf.lease_id=?5 AND vf.status='in_progress' AND vf.lease_until>CAST(?6 AS INTEGER))")
        .bind(&values(&bindings))?.run().await?;
    meter.record_d1(event, &result).await
}

/// Auto chunking is a versioned byte policy, not a claim about any model tokenizer.
pub fn auto_chunks(text: &str, byte_limit: usize, overlap_bytes: usize) -> Result<Vec<Chunk>> {
    if !(4..=32768).contains(&byte_limit) || overlap_bytes >= byte_limit / 2 {
        return Err(fail("invalid_file_search_chunking"));
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + byte_limit).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(Chunk {
            ordinal: chunks.len(),
            byte_start: start,
            byte_end: end,
            text: text[start..end].into(),
        });
        if end == text.len() {
            break;
        }
        let mut next = end.saturating_sub(overlap_bytes).max(start + 1);
        while !text.is_char_boundary(next) {
            next += 1;
        }
        start = next;
    }
    Ok(chunks)
}

pub fn vector_id(store: &str, file: &str, generation: &str, ordinal: usize) -> String {
    let identity = serde_json::to_vec(&(store, file, generation, ordinal))
        .expect("string tuple serialization");
    format!("{:x}", Sha256::digest(identity))
}

#[derive(Clone, Deserialize)]
pub struct IngestJob {
    pub account_id: String,
    pub vector_store_id: String,
    pub file_id: String,
    pub file_sha256: String,
    pub generation: String,
    pub lease_id: String,
    pub chunking_json: String,
    pub source_staged: u8,
    pub chunk_count: usize,
}

pub async fn seal_source(
    env: &Env,
    job: &IngestJob,
    expected: usize,
    now_ms: u64,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<bool> {
    if expected == 0 || expected > 65536 {
        return Err(fail("file_search_chunk_count_limit"));
    }
    let bindings = vec![
        job.account_id.clone(),
        job.vector_store_id.clone(),
        job.file_id.clone(),
        job.generation.clone(),
        job.lease_id.clone(),
        now_ms.to_string(),
        expected.to_string(),
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_store_files SET source_staged=1,chunk_count=CAST(?7 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND lease_id=?5 AND status='in_progress' AND lease_until>CAST(?6 AS INTEGER) AND (SELECT COUNT(*) FROM file_search_chunks c WHERE c.account_id=?1 AND c.vector_store_id=?2 AND c.file_id=?3 AND c.generation=?4)=CAST(?7 AS INTEGER) AND (SELECT MAX(ordinal) FROM file_search_chunks c WHERE c.account_id=?1 AND c.vector_store_id=?2 AND c.file_id=?3 AND c.generation=?4)=CAST(?7 AS INTEGER)-1 RETURNING file_id")
        .bind(&values(&bindings))?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(!result.results::<Value>()?.is_empty())
}

pub async fn release_job(
    env: &Env,
    job: &IngestJob,
    retry_at: u64,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    let bindings = vec![
        job.account_id.clone(),
        job.vector_store_id.clone(),
        job.file_id.clone(),
        job.generation.clone(),
        job.lease_id.clone(),
        retry_at.to_string(),
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_store_files SET lease_id=NULL,lease_until=0,retry_at=CAST(?6 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND lease_id=?5 AND status='in_progress'")
        .bind(&values(&bindings))?.run().await?;
    meter.record_d1(event, &result).await
}

pub async fn source_file(
    env: &Env,
    job: &IngestJob,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Option<crate::space::OwnedFile>> {
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("SELECT id,account_id,filename,purpose,content_type,object_key,bytes,sha256,status,reservation_id,created_at_ms,updated_at_ms FROM space_files WHERE id=?1 AND account_id=?2 AND sha256=?3 AND status='processed'")
        .bind(&[JsValue::from_str(&job.file_id),JsValue::from_str(&job.account_id),JsValue::from_str(&job.file_sha256)])?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(result
        .results::<crate::space::OwnedFile>()?
        .into_iter()
        .next())
}

#[derive(Deserialize)]
struct LastMutation {
    mutation: Option<String>,
}

pub async fn last_mutation(
    env: &Env,
    job: &IngestJob,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Option<String>> {
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("SELECT MAX(vector_mutation_id) AS mutation FROM file_search_chunks WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND vector_status='visible'")
        .bind(&[JsValue::from_str(&job.account_id),JsValue::from_str(&job.vector_store_id),JsValue::from_str(&job.file_id),JsValue::from_str(&job.generation)])?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(result
        .results::<LastMutation>()?
        .into_iter()
        .next()
        .and_then(|row| row.mutation))
}

/// A lease is acquired only while both source and store remain eligible. Expired
/// leases are recoverable; generation is unchanged so upserts remain idempotent.
pub async fn claim_job(
    env: &Env,
    owner: &str,
    store: &str,
    file: &str,
    now_ms: u64,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Option<IngestJob>> {
    let lease = uuid::Uuid::new_v4().to_string();
    let until = now_ms
        .checked_add(120_000)
        .ok_or_else(|| fail("file_search_clock_overflow"))?;
    let bindings = [
        owner.to_string(),
        store.to_string(),
        file.to_string(),
        now_ms.to_string(),
        until.to_string(),
        lease,
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_store_files SET lease_id=?6,lease_until=CAST(?5 AS INTEGER),updated_at=CAST(?4 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND status='in_progress' AND retry_at<=CAST(?4 AS INTEGER) AND lease_until<=CAST(?4 AS INTEGER) AND EXISTS(SELECT 1 FROM space_files sf WHERE sf.id=file_id AND sf.account_id=?1 AND sf.status='processed' AND sf.sha256=file_sha256) AND EXISTS(SELECT 1 FROM vector_stores vs WHERE vs.id=vector_store_id AND vs.account_id=?1 AND vs.status='completed') RETURNING account_id,vector_store_id,file_id,file_sha256,generation,lease_id,chunking_json,source_staged,chunk_count")
        .bind(&values(&bindings))?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(result.results::<IngestJob>()?.into_iter().next())
}

/// Every batch is source/lease fenced in SQL. An interrupted attempt can safely
/// replay identical ordinal IDs; stale workers cannot modify a later generation.
pub async fn stage_chunks(
    env: &Env,
    job: &IngestJob,
    chunks: &[Chunk],
    profile: &EmbeddingProfile,
    now_ms: u64,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    profile.validate()?;
    if chunks.is_empty() || chunks.len() > 32 {
        return Err(fail("file_search_chunk_batch_limit"));
    }
    let db = env.d1("DB")?;
    let mut statements = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        if chunk.text.is_empty()
            || chunk.text.len() > profile.input_byte_limit
            || chunk.byte_end.checked_sub(chunk.byte_start) != Some(chunk.text.len())
        {
            return Err(fail("invalid_file_search_chunk"));
        }
        let bindings = vec![
            job.account_id.clone(),
            job.vector_store_id.clone(),
            job.file_id.clone(),
            job.generation.clone(),
            job.lease_id.clone(),
            now_ms.to_string(),
            vector_id(
                &job.vector_store_id,
                &job.file_id,
                &job.generation,
                chunk.ordinal,
            ),
            job.file_sha256.clone(),
            chunk.ordinal.to_string(),
            chunk.text.clone(),
            chunk.byte_start.to_string(),
            chunk.byte_end.to_string(),
            profile.model.clone(),
            profile.dimensions.to_string(),
            profile.revision.clone(),
        ];
        statements.push(db.prepare("INSERT INTO file_search_chunks(chunk_id,account_id,vector_store_id,file_id,file_sha256,generation,ordinal,text,byte_start,byte_end,embedding_model,embedding_dimensions,embedding_revision) SELECT ?7,?1,?2,?3,?8,?4,CAST(?9 AS INTEGER),?10,CAST(?11 AS INTEGER),CAST(?12 AS INTEGER),?13,CAST(?14 AS INTEGER),?15 WHERE EXISTS(SELECT 1 FROM vector_store_files vf JOIN space_files sf ON sf.id=vf.file_id JOIN vector_stores vs ON vs.id=vf.vector_store_id WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.file_id=?3 AND vf.generation=?4 AND vf.lease_id=?5 AND vf.lease_until>CAST(?6 AS INTEGER) AND vf.status='in_progress' AND sf.account_id=?1 AND sf.status='processed' AND sf.sha256=?8 AND vs.account_id=?1 AND vs.status='completed') ON CONFLICT(chunk_id) DO NOTHING")
            .bind(&values(&bindings))?);
    }
    for ordinal in 0..statements.len() {
        meter.require_d1(&format!("{event}:{ordinal}")).await?;
    }
    let results = db.batch(statements).await?;
    for (ordinal, result) in results.iter().enumerate() {
        meter
            .record_d1(&format!("{event}:{ordinal}"), result)
            .await?;
    }
    Ok(())
}

pub async fn fail_job(
    env: &Env,
    job: &IngestJob,
    code: &str,
    now_ms: u64,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    if !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') || code.len() > 128 {
        return Err(fail("invalid_file_search_error_code"));
    }
    let error = serde_json::json!({"code":code,"message":code}).to_string();
    let bindings = vec![
        job.account_id.clone(),
        job.vector_store_id.clone(),
        job.file_id.clone(),
        job.generation.clone(),
        job.lease_id.clone(),
        now_ms.to_string(),
        error,
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_store_files SET status='failed',last_error_json=?7,lease_id=NULL,lease_until=0,updated_at=CAST(?6 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND lease_id=?5 AND status='in_progress'")
        .bind(&values(&bindings))?.run().await?;
    meter.record_d1(event, &result).await
}

/// Call only after every staged vector is query-visible for this generation.
/// Publication counts the entire staged generation and checks source/lease again.
pub async fn publish_job(
    env: &Env,
    job: &IngestJob,
    profile: &EmbeddingProfile,
    expected_chunks: usize,
    mutation_id: &str,
    now_ms: u64,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<bool> {
    profile.validate()?;
    if expected_chunks == 0
        || expected_chunks > 65536
        || mutation_id.is_empty()
        || mutation_id.len() > 256
    {
        return Err(fail("invalid_file_search_publication"));
    }
    let bindings = vec![
        job.account_id.clone(),
        job.vector_store_id.clone(),
        job.file_id.clone(),
        job.generation.clone(),
        job.lease_id.clone(),
        now_ms.to_string(),
        profile.model.clone(),
        profile.dimensions.to_string(),
        profile.revision.clone(),
        expected_chunks.to_string(),
        mutation_id.to_string(),
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_store_files SET status='completed',embedding_model=?7,embedding_dimensions=CAST(?8 AS INTEGER),embedding_revision=?9,chunk_count=CAST(?10 AS INTEGER),usage_bytes=(SELECT COALESCE(SUM(length(CAST(text AS BLOB))),0)+COUNT(*)*CAST(?8 AS INTEGER)*4 FROM file_search_chunks c WHERE c.vector_store_id=?2 AND c.file_id=?3 AND c.generation=?4),vector_mutation_id=?11,lease_id=NULL,lease_until=0,updated_at=CAST(?6 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND lease_id=?5 AND lease_until>CAST(?6 AS INTEGER) AND status='in_progress' AND EXISTS(SELECT 1 FROM space_files sf WHERE sf.id=file_id AND sf.account_id=?1 AND sf.status='processed' AND sf.sha256=file_sha256) AND EXISTS(SELECT 1 FROM vector_stores vs WHERE vs.id=vector_store_id AND vs.account_id=?1 AND vs.status='completed') AND (SELECT COUNT(*) FROM file_search_chunks c WHERE c.account_id=?1 AND c.vector_store_id=?2 AND c.file_id=?3 AND c.generation=?4 AND c.embedding_model=?7 AND c.embedding_dimensions=CAST(?8 AS INTEGER) AND c.embedding_revision=?9 AND c.vector_status='visible')=CAST(?10 AS INTEGER) AND (SELECT COUNT(*) FROM file_search_chunks c WHERE c.vector_store_id=?2 AND c.file_id=?3 AND c.generation=?4)=CAST(?10 AS INTEGER) AND (SELECT MAX(ordinal) FROM file_search_chunks c WHERE c.vector_store_id=?2 AND c.file_id=?3 AND c.generation=?4)=CAST(?10 AS INTEGER)-1 RETURNING file_id")
        .bind(&values(&bindings))?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(!result.results::<Value>()?.is_empty())
}

#[derive(Deserialize)]
struct Count {
    count: usize,
}

#[derive(Deserialize)]
struct Row {
    chunk_id: String,
    file_id: String,
    filename: String,
    attributes_json: String,
    text: String,
}

impl Row {
    fn hit(self) -> Result<Hit> {
        let attributes: BTreeMap<String, Value> = serde_json::from_str(&self.attributes_json)?;
        Ok(Hit {
            chunk_id: self.chunk_id,
            file_id: self.file_id,
            filename: self.filename,
            attributes,
            text: self.text,
            score: 0.0,
        })
    }
}

fn values(bindings: &[String]) -> Vec<JsValue> {
    bindings.iter().map(|v| JsValue::from_str(v)).collect()
}

fn store_scope(options: &FileSearchOptions, bindings: &mut Vec<String>) -> String {
    let slots = options
        .vector_store_ids
        .iter()
        .map(|id| {
            bindings.push(id.clone());
            format!("?{}", bindings.len())
        })
        .collect::<Vec<_>>();
    format!("c.vector_store_id IN ({})", slots.join(","))
}

async fn require_stores(
    env: &Env,
    owner: &str,
    options: &FileSearchOptions,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<()> {
    let stores = serde_json::to_string(&options.vector_store_ids)?;
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("SELECT COUNT(*) AS count FROM vector_stores WHERE account_id=?1 AND status='completed' AND id IN (SELECT value FROM json_each(?2))")
        .bind(&[JsValue::from_str(owner),JsValue::from_str(&stores)])?.all().await?;
    meter.record_d1(event, &result).await?;
    let rows: Vec<Count> = result.results()?;
    if rows.first().map(|r| r.count) != Some(options.vector_store_ids.len()) {
        return Err(fail("vector_store_not_found"));
    }
    Ok(())
}

pub async fn lexical_candidates(
    env: &Env,
    owner: &str,
    query: &str,
    options: &FileSearchOptions,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Vec<Hit>> {
    options.validate()?;
    let query = lexical_query(query)?;
    require_stores(env, owner, options, meter, &format!("{event}:stores")).await?;
    let mut bindings = vec![owner.to_string()];
    let scope = store_scope(options, &mut bindings);
    let filter = match &options.filters {
        Some(filter) => filter.compile_into(&mut bindings)?,
        None => "1".into(),
    };
    bindings.push(query);
    let query_slot = bindings.len();
    let sql = format!(
        "SELECT c.chunk_id,c.file_id,sf.filename,vf.attributes_json,c.text FROM file_search_fts JOIN file_search_chunks c ON c.id=file_search_fts.rowid {JOINS} WHERE {LIVE} AND {scope} AND ({filter}) AND file_search_fts MATCH ?{query_slot} ORDER BY bm25(file_search_fts),c.chunk_id LIMIT 100"
    );
    let operation = format!("{event}:lexical");
    meter.require_d1(&operation).await?;
    let result = env
        .d1("DB")?
        .prepare(sql)
        .bind(&values(&bindings))?
        .all()
        .await?;
    meter.record_d1(&operation, &result).await?;
    result.results::<Row>()?.into_iter().map(Row::hit).collect()
}

#[derive(Deserialize)]
pub struct EligibleFile {
    pub vector_store_id: String,
    pub file_id: String,
    pub embedding_model: String,
    pub embedding_dimensions: usize,
    pub embedding_revision: String,
}

pub async fn eligible_files(
    env: &Env,
    owner: &str,
    options: &FileSearchOptions,
    profile: &EmbeddingProfile,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Vec<EligibleFile>> {
    options.validate()?;
    require_stores(env, owner, options, meter, &format!("{event}:stores")).await?;
    let mut bindings = vec![
        owner.to_string(),
        serde_json::to_string(&options.vector_store_ids)?,
    ];
    let filter = match &options.filters {
        Some(filter) => filter.compile_into(&mut bindings)?,
        None => "1".into(),
    };
    let sql = format!(
        "SELECT vf.vector_store_id,vf.file_id,vf.embedding_model,vf.embedding_dimensions,vf.embedding_revision FROM vector_store_files vf JOIN vector_stores vs ON vs.id=vf.vector_store_id JOIN space_files sf ON sf.id=vf.file_id WHERE vf.account_id=?1 AND vs.account_id=?1 AND sf.account_id=?1 AND vs.status='completed' AND vf.status='completed' AND sf.status='processed' AND sf.sha256=vf.file_sha256 AND vf.vector_store_id IN (SELECT value FROM json_each(?2)) AND ({filter}) ORDER BY vf.vector_store_id,vf.file_id LIMIT 2049"
    );
    meter.require_d1(event).await?;
    let result = env
        .d1("DB")?
        .prepare(sql)
        .bind(&values(&bindings))?
        .all()
        .await?;
    meter.record_d1(event, &result).await?;
    let rows = result.results::<EligibleFile>()?;
    if rows.len() > 2048 {
        return Err(fail("file_search_eligible_file_budget_exceeded"));
    }
    if rows.iter().any(|row| {
        row.embedding_model != profile.model
            || row.embedding_dimensions != profile.dimensions
            || row.embedding_revision != profile.revision
    }) {
        return Err(fail("file_search_store_requires_reindex"));
    }
    Ok(rows)
}

/// Vector metadata is only an accelerator, never authorization or current-source evidence.
pub async fn live_vector_candidates(
    env: &Env,
    owner: &str,
    ids: &[String],
    options: &FileSearchOptions,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Vec<Hit>> {
    options.validate()?;
    if ids.len() > 100 {
        return Err(fail("file_search_candidate_limit"));
    }
    require_stores(env, owner, options, meter, &format!("{event}:stores")).await?;
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let mut bindings = vec![owner.to_string()];
    let scope = store_scope(options, &mut bindings);
    let filter = match &options.filters {
        Some(filter) => filter.compile_into(&mut bindings)?,
        None => "1".into(),
    };
    bindings.push(serde_json::to_string(ids)?);
    let slot = bindings.len();
    let sql = format!(
        "SELECT c.chunk_id,c.file_id,sf.filename,vf.attributes_json,c.text FROM file_search_chunks c {JOINS} WHERE {LIVE} AND {scope} AND ({filter}) AND c.chunk_id IN (SELECT value FROM json_each(?{slot}))"
    );
    meter.require_d1(event).await?;
    let result = env
        .d1("DB")?
        .prepare(sql)
        .bind(&values(&bindings))?
        .all()
        .await?;
    meter.record_d1(event, &result).await?;
    let mut hits: BTreeMap<String, Hit> = result
        .results::<Row>()?
        .into_iter()
        .map(|row| row.hit().map(|hit| (hit.chunk_id.clone(), hit)))
        .collect::<Result<_>>()?;
    Ok(ids.iter().filter_map(|id| hits.remove(id)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_chunks_advance_and_cover_source() {
        let text = "你好🌸!ab中文🦀".repeat(40);
        let chunks = auto_chunks(&text, 17, 4).unwrap();
        assert_eq!(chunks.first().unwrap().byte_start, 0);
        assert_eq!(chunks.last().unwrap().byte_end, text.len());
        for chunk in &chunks {
            assert_eq!(chunk.text, &text[chunk.byte_start..chunk.byte_end]);
            assert!(chunk.text.len() <= 17);
        }
        for pair in chunks.windows(2) {
            assert!(pair[1].byte_start > pair[0].byte_start);
            assert!(pair[1].byte_start <= pair[0].byte_end);
        }
    }

    #[test]
    fn vector_identity_separates_generation_and_delimiter_collisions() {
        assert_ne!(vector_id("a:b", "c", "d", 0), vector_id("a", "b:c", "d", 0));
        assert_ne!(vector_id("a", "b", "c", 0), vector_id("a", "b", "d", 0));
        assert_eq!(vector_id("a", "b", "c", 0).len(), 64);
    }

    #[test]
    fn vector_shape_finite_and_nonzero_required() {
        let profile = EmbeddingProfile {
            model: "fixture".into(),
            revision: "1".into(),
            dimensions: 2,
            input_byte_limit: 1024,
        };
        assert!(profile.validate_vector(&[0.1, 0.2]).is_ok());
        for vector in [&[0.0, 0.0][..], &[f32::NAN, 1.0][..], &[1.0][..]] {
            assert!(profile.validate_vector(vector).is_err());
        }
    }
}
