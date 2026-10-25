//! Native Workers AI / Vectorize transport. Operator allocation tariffs are explicit;
//! embedding bytes are never presented as observed provider token usage.

use agents_auth::billing::Resource;
use js_sys::{Function, Promise, Reflect};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::{Env, Error, Result};

use crate::{
    file_index::{self, EmbeddingProfile},
    file_search::{FileSearchOptions, Hit, Ranker, fuse},
    resources::ResourceMeter,
};

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub resource: Resource,
    pub unit: String,
}

impl Allocation {
    fn validate(&self, unit: &str) -> Result<()> {
        if self.unit != unit || self.resource == Resource::Model {
            return Err(fail("invalid_retrieval_resource_allocation"));
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VectorServiceProfile {
    pub embedding: EmbeddingProfile,
    pub ai_binding: String,
    pub vector_binding: String,
    pub namespace: String,
    pub embedding_allocation: Allocation,
    pub query_allocation: Allocation,
    pub upsert_allocation: Allocation,
}

impl VectorServiceProfile {
    pub fn validate(&self) -> Result<()> {
        self.embedding.validate()?;
        if self.ai_binding.is_empty()
            || self.vector_binding.is_empty()
            || self.namespace.is_empty()
            || self.namespace.len() > 64
        {
            return Err(fail("invalid_file_search_service_profile"));
        }
        self.embedding_allocation
            .validate("embedding_input_bytes")?;
        self.query_allocation.validate("queried_dimensions")?;
        self.upsert_allocation.validate("written_dimensions")
    }
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<Vec<f32>>,
    shape: Vec<usize>,
}

#[derive(Serialize)]
struct EmbeddingInput<'a> {
    text: &'a [String],
    truncate_inputs: bool,
}

pub async fn embed(
    env: &Env,
    profile: &VectorServiceProfile,
    texts: &[String],
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Vec<Vec<f32>>> {
    profile.validate()?;
    if texts.is_empty()
        || texts.len() > 32
        || texts
            .iter()
            .any(|text| text.is_empty() || text.len() > profile.embedding.input_byte_limit)
    {
        return Err(fail("file_search_embedding_batch_limit"));
    }
    let bytes = texts
        .iter()
        .try_fold(0u64, |sum, text| sum.checked_add(text.len() as u64))
        .ok_or_else(|| fail("file_search_usage_overflow"))?;
    let allocation = &profile.embedding_allocation;
    meter
        .require_observation(event, allocation.resource.clone(), &allocation.unit)
        .await?;
    let response: EmbeddingResponse = env
        .ai(&profile.ai_binding)?
        .run(
            &profile.embedding.model,
            EmbeddingInput {
                text: texts,
                truncate_inputs: false,
            },
        )
        .await?;
    meter
        .record_observation(event, allocation.resource.clone(), &allocation.unit, bytes)
        .await?;
    if response.shape != [texts.len(), profile.embedding.dimensions]
        || response.data.len() != texts.len()
    {
        return Err(fail("invalid_file_search_embedding_shape"));
    }
    for vector in &response.data {
        profile.embedding.validate_vector(vector)?;
    }
    Ok(response.data)
}

pub fn owner_hash(owner: &str) -> String {
    format!("{:x}", Sha256::digest(owner.as_bytes()))
}

async fn invoke(env: &Env, binding: &str, method: &str, args: &[Value]) -> Result<Value> {
    let service = Reflect::get(env.as_ref(), &JsValue::from_str(binding))
        .map_err(|_| fail("file_search_vector_binding_missing"))?;
    if service.is_undefined() || service.is_null() {
        return Err(fail("file_search_vector_binding_missing"));
    }
    let function = Reflect::get(&service, &JsValue::from_str(method))
        .map_err(|_| fail("file_search_vector_method_missing"))?
        .dyn_into::<Function>()
        .map_err(|_| fail("file_search_vector_method_missing"))?;
    let arguments = js_sys::Array::new();
    for argument in args {
        arguments.push(&argument.serialize(&serde_wasm_bindgen::Serializer::json_compatible())?);
    }
    let promise = function
        .apply(&service, &arguments)
        .map_err(|_| fail("file_search_vector_request_failed"))?
        .dyn_into::<Promise>()
        .map_err(|_| fail("invalid_file_search_vector_response"))?;
    let response = JsFuture::from(promise)
        .await
        .map_err(|_| fail("file_search_vector_request_failed"))?;
    Ok(serde_wasm_bindgen::from_value(response)?)
}

#[derive(Deserialize)]
struct Matches {
    matches: Vec<VectorMatch>,
}

#[derive(Deserialize)]
pub struct VectorMatch {
    pub id: String,
    pub score: f64,
}

/// Eligible IDs are resolved from owner-scoped live D1 state before this call.
/// Each batch stays below Vectorize's metadata filter byte limit. Caller merges
/// real similarity scores across batches and still revalidates returned IDs.
pub async fn query(
    env: &Env,
    profile: &VectorServiceProfile,
    owner: &str,
    store: &str,
    eligible_files: &[String],
    vector: &[f32],
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Vec<VectorMatch>> {
    profile.validate()?;
    profile.embedding.validate_vector(vector)?;
    if eligible_files.is_empty() {
        return Ok(vec![]);
    }
    if eligible_files.len() > 32 || eligible_files.iter().any(|id| id.len() > 64) {
        return Err(fail("file_search_vector_filter_batch_limit"));
    }
    let filter = json!({"owner_hash":{"$eq":owner_hash(owner)},"store_id":{"$eq":store},"file_id":{"$in":eligible_files}});
    if filter.to_string().len() > 2048 {
        return Err(fail("file_search_vector_filter_batch_limit"));
    }
    let allocation = &profile.query_allocation;
    meter
        .require_observation(event, allocation.resource.clone(), &allocation.unit)
        .await?;
    let result=invoke(env,&profile.vector_binding,"query",&[json!(vector),json!({"namespace":profile.namespace,"topK":100,"returnValues":false,"returnMetadata":"none","filter":filter})]).await?;
    meter
        .record_observation(
            event,
            allocation.resource.clone(),
            &allocation.unit,
            profile.embedding.dimensions as u64,
        )
        .await?;
    let matches: Matches = serde_json::from_value(result)?;
    if matches.matches.len() > 100
        || matches
            .matches
            .iter()
            .any(|m| m.id.len() > 64 || !m.score.is_finite())
    {
        return Err(fail("invalid_file_search_vector_matches"));
    }
    Ok(matches.matches)
}

#[derive(Serialize)]
pub struct VectorRecord {
    pub id: String,
    pub values: Vec<f32>,
    namespace: String,
    metadata: Value,
}

impl VectorRecord {
    pub fn new(
        profile: &VectorServiceProfile,
        owner: &str,
        store: &str,
        file: &str,
        generation: &str,
        id: String,
        values: Vec<f32>,
    ) -> Result<Self> {
        profile.validate()?;
        profile.embedding.validate_vector(&values)?;
        if id.len() != 64
            || !id.bytes().all(|b| b.is_ascii_hexdigit())
            || store.len() > 64
            || file.len() > 64
        {
            return Err(fail("invalid_file_search_vector_identity"));
        }
        Ok(Self {
            id: id.clone(),
            values,
            namespace: profile.namespace.clone(),
            metadata: json!({"owner_hash":owner_hash(owner),"store_id":store,"file_id":file,"generation":generation,"chunk_id":id,"model_revision":profile.embedding.revision}),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Mutation {
    mutation_id: String,
}

pub async fn upsert(
    env: &Env,
    profile: &VectorServiceProfile,
    records: &[VectorRecord],
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<String> {
    profile.validate()?;
    if records.is_empty() || records.len() > 32 {
        return Err(fail("file_search_vector_batch_limit"));
    }
    for record in records {
        profile.embedding.validate_vector(&record.values)?;
    }
    let allocation = &profile.upsert_allocation;
    meter
        .require_observation(event, allocation.resource.clone(), &allocation.unit)
        .await?;
    let result = invoke(
        env,
        &profile.vector_binding,
        "upsert",
        &[serde_json::to_value(records)?],
    )
    .await?;
    let dimensions = (profile.embedding.dimensions as u64)
        .checked_mul(records.len() as u64)
        .ok_or_else(|| fail("file_search_usage_overflow"))?;
    meter
        .record_observation(
            event,
            allocation.resource.clone(),
            &allocation.unit,
            dimensions,
        )
        .await?;
    let mutation: Mutation = serde_json::from_value(result)?;
    if mutation.mutation_id.is_empty() || mutation.mutation_id.len() > 256 {
        return Err(fail("invalid_file_search_vector_mutation"));
    }
    Ok(mutation.mutation_id)
}

/// Query visibility, rather than a successful asynchronous upsert acknowledgement,
/// is the publication barrier. chunk_id must have a native string metadata index.
pub async fn visible(
    env: &Env,
    profile: &VectorServiceProfile,
    records: &[VectorRecord],
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<bool> {
    profile.validate()?;
    if records.is_empty() || records.len() > 32 {
        return Err(fail("file_search_vector_batch_limit"));
    }
    for (ordinal, record) in records.iter().enumerate() {
        let allocation = &profile.query_allocation;
        let operation = format!("{event}:{ordinal}");
        meter
            .require_observation(&operation, allocation.resource.clone(), &allocation.unit)
            .await?;
        let filter = json!({"owner_hash":{"$eq":record.metadata["owner_hash"]},"store_id":{"$eq":record.metadata["store_id"]},"file_id":{"$eq":record.metadata["file_id"]},"chunk_id":{"$eq":record.id}});
        let result=invoke(env,&profile.vector_binding,"query",&[json!(record.values),json!({"namespace":profile.namespace,"topK":1,"returnValues":false,"returnMetadata":"none","filter":filter})]).await?;
        meter
            .record_observation(
                &operation,
                allocation.resource.clone(),
                &allocation.unit,
                profile.embedding.dimensions as u64,
            )
            .await?;
        let matches: Matches = serde_json::from_value(result)?;
        if !matches
            .matches
            .iter()
            .any(|candidate| candidate.id == record.id && candidate.score.is_finite())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Debug, PartialEq, Eq)]
pub enum IngestProgress {
    Unavailable,
    Pending,
    Completed,
}

/// One recoverable, resource-admitted maintenance turn. Sealed source chunks
/// prevent repeated R2 downloads; completed vector batches are never re-embedded.
pub async fn ingest_turn(
    env: &Env,
    profile: &VectorServiceProfile,
    owner: &str,
    store: &str,
    file: &str,
    max_batches: usize,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<IngestProgress> {
    profile.validate()?;
    if !(1..=8).contains(&max_batches) {
        return Err(fail("invalid_file_search_ingest_budget"));
    }
    let Some(mut job) = file_index::claim_job(
        env,
        owner,
        store,
        file,
        crate::util::now_ms(),
        meter,
        &format!("{event}:claim"),
    )
    .await?
    else {
        return Ok(IngestProgress::Unavailable);
    };
    // Attempt identity is part of resource event keys: a recovery that genuinely
    // repeats an upstream operation must not reuse its predecessor's measurement.
    let attempt = format!("{event}:{}", job.lease_id);
    if job.source_staged == 0 {
        let Some(source) =
            file_index::source_file(env, &job, meter, &format!("{attempt}:source")).await?
        else {
            return Ok(IngestProgress::Unavailable);
        };
        if source.bytes > 16 * 1024 * 1024 {
            file_index::fail_job(
                env,
                &job,
                "file_extraction_byte_limit",
                crate::util::now_ms(),
                meter,
                &format!("{attempt}:failure"),
            )
            .await?;
            return Ok(IngestProgress::Unavailable);
        }
        let object = crate::space::file_object(env, owner, &source).await?;
        let bytes = object
            .body()
            .ok_or_else(|| fail("file_not_ready"))?
            .bytes()
            .await?;
        if bytes.len() as u64 != source.bytes
            || format!("{:x}", Sha256::digest(&bytes)) != job.file_sha256
        {
            return Err(fail("file_integrity_error"));
        }
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) if !text.contains('\0') && !text.trim().is_empty() => text,
            _ => {
                file_index::fail_job(
                    env,
                    &job,
                    "unsupported_or_empty_document",
                    crate::util::now_ms(),
                    meter,
                    &format!("{attempt}:failure"),
                )
                .await?;
                return Ok(IngestProgress::Unavailable);
            }
        };
        let chunks = file_index::auto_chunks(
            text,
            profile.embedding.input_byte_limit,
            profile.embedding.input_byte_limit / 8,
        )?;
        for (ordinal, batch) in chunks.chunks(32).enumerate() {
            if !file_index::renew_job(
                env,
                &job,
                crate::util::now_ms(),
                meter,
                &format!("{attempt}:stage_renew:{ordinal}"),
            )
            .await?
            {
                return Ok(IngestProgress::Unavailable);
            }
            file_index::stage_chunks(
                env,
                &job,
                batch,
                &profile.embedding,
                crate::util::now_ms(),
                meter,
                &format!("{attempt}:stage:{ordinal}"),
            )
            .await?;
        }
        if !file_index::seal_source(
            env,
            &job,
            chunks.len(),
            crate::util::now_ms(),
            meter,
            &format!("{attempt}:seal"),
        )
        .await?
        {
            return Ok(IngestProgress::Unavailable);
        }
        job.chunk_count = chunks.len();
        job.source_staged = 1;
    }
    for batch in 0..max_batches {
        if !file_index::renew_job(
            env,
            &job,
            crate::util::now_ms(),
            meter,
            &format!("{attempt}:renew:{batch}"),
        )
        .await?
        {
            return Ok(IngestProgress::Unavailable);
        }
        let chunks = file_index::pending_chunks(
            env,
            &job,
            &profile.embedding,
            meter,
            &format!("{attempt}:pending:{batch}"),
        )
        .await?;
        if chunks.is_empty() {
            return complete_ingest(env, profile, &job, meter, &attempt).await;
        }
        let texts = chunks
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>();
        let vectors = embed(
            env,
            profile,
            &texts,
            meter,
            &format!("{attempt}:embed:{batch}"),
        )
        .await?;
        let records = chunks
            .iter()
            .zip(vectors)
            .map(|(chunk, vector)| {
                VectorRecord::new(
                    profile,
                    owner,
                    store,
                    file,
                    &job.generation,
                    file_index::vector_id(store, file, &job.generation, chunk.ordinal),
                    vector,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let mutation = upsert(
            env,
            profile,
            &records,
            meter,
            &format!("{attempt}:upsert:{batch}"),
        )
        .await?;
        let mut found = false;
        for poll in 0..5 {
            worker::Delay::from(std::time::Duration::from_secs(1u64 << poll)).await;
            if visible(
                env,
                profile,
                &records,
                meter,
                &format!("{attempt}:visibility:{batch}:{poll}"),
            )
            .await?
            {
                found = true;
                break;
            }
        }
        if !found {
            file_index::release_job(
                env,
                &job,
                crate::util::now_ms() + 30_000,
                meter,
                &format!("{attempt}:defer"),
            )
            .await?;
            return Ok(IngestProgress::Pending);
        }
        let ids = records
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>();
        file_index::mark_visible(
            env,
            &job,
            &ids,
            &mutation,
            crate::util::now_ms(),
            meter,
            &format!("{attempt}:visible:{batch}"),
        )
        .await?;
    }
    let pending = file_index::pending_chunks(
        env,
        &job,
        &profile.embedding,
        meter,
        &format!("{attempt}:remaining"),
    )
    .await?;
    if pending.is_empty() {
        return complete_ingest(env, profile, &job, meter, &attempt).await;
    }
    file_index::release_job(
        env,
        &job,
        crate::util::now_ms(),
        meter,
        &format!("{attempt}:yield"),
    )
    .await?;
    Ok(IngestProgress::Pending)
}

async fn complete_ingest(
    env: &Env,
    profile: &VectorServiceProfile,
    job: &file_index::IngestJob,
    meter: &ResourceMeter<'_>,
    attempt: &str,
) -> Result<IngestProgress> {
    let mutation = file_index::last_mutation(env, job, meter, &format!("{attempt}:mutation"))
        .await?
        .ok_or_else(|| fail("file_search_vector_visibility_missing"))?;
    let published = file_index::publish_job(
        env,
        job,
        &profile.embedding,
        job.chunk_count,
        &mutation,
        crate::util::now_ms(),
        meter,
        &format!("{attempt}:publish"),
    )
    .await?;
    Ok(if published {
        IngestProgress::Completed
    } else {
        IngestProgress::Unavailable
    })
}

pub async fn search(
    env: &Env,
    profile: &VectorServiceProfile,
    owner: &str,
    text: &str,
    options: &FileSearchOptions,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Vec<Hit>> {
    options.validate()?;
    profile.validate()?;
    if matches!(options.ranking_options.ranker, Ranker::Default20241115) {
        return Err(fail("requested_file_search_ranker_unavailable"));
    }
    let weights = &options.ranking_options.hybrid_search;
    let lexical = if weights.text_weight > 0.0 {
        file_index::lexical_candidates(
            env,
            owner,
            text,
            options,
            meter,
            &format!("{event}:lexical"),
        )
        .await?
    } else {
        vec![]
    };
    let mut semantic = vec![];
    if weights.embedding_weight > 0.0 {
        let eligible = file_index::eligible_files(
            env,
            owner,
            options,
            &profile.embedding,
            meter,
            &format!("{event}:eligible"),
        )
        .await?;
        if !eligible.is_empty() {
            let vectors = embed(
                env,
                profile,
                &[text.to_string()],
                meter,
                &format!("{event}:embedding"),
            )
            .await?;
            let mut stores = BTreeMap::<String, Vec<String>>::new();
            for file in eligible {
                stores
                    .entry(file.vector_store_id)
                    .or_default()
                    .push(file.file_id);
            }
            let mut matches = vec![];
            let mut ordinal = 0;
            for (store, files) in stores {
                for files in files.chunks(24) {
                    matches.extend(
                        query(
                            env,
                            profile,
                            owner,
                            &store,
                            files,
                            &vectors[0],
                            meter,
                            &format!("{event}:vector:{ordinal}"),
                        )
                        .await?,
                    );
                    ordinal += 1;
                }
            }
            matches.sort_by(|left, right| {
                right
                    .score
                    .total_cmp(&left.score)
                    .then(left.id.cmp(&right.id))
            });
            let mut seen = std::collections::BTreeSet::new();
            let ids = matches
                .into_iter()
                .filter_map(|m| seen.insert(m.id.clone()).then_some(m.id))
                .take(100)
                .collect::<Vec<_>>();
            semantic = file_index::live_vector_candidates(
                env,
                owner,
                &ids,
                options,
                meter,
                &format!("{event}:live"),
            )
            .await?;
        }
    }
    let lexical_ids = lexical
        .iter()
        .map(|hit| hit.chunk_id.clone())
        .collect::<Vec<_>>();
    let semantic_ids = semantic
        .iter()
        .map(|hit| hit.chunk_id.clone())
        .collect::<Vec<_>>();
    let ranked = fuse(
        &lexical_ids,
        &semantic_ids,
        &options.ranking_options,
        options.max_num_results,
    )?;
    let final_ids = ranked.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
    let mut hits = file_index::live_vector_candidates(
        env,
        owner,
        &final_ids,
        options,
        meter,
        &format!("{event}:final_live"),
    )
    .await?
    .into_iter()
    .map(|hit| (hit.chunk_id.clone(), hit))
    .collect::<BTreeMap<_, _>>();
    Ok(ranked
        .into_iter()
        .filter_map(|(id, score)| {
            hits.remove(&id).map(|mut hit| {
                hit.score = score;
                hit
            })
        })
        .collect())
}
