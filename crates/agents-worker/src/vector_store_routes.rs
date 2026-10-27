//! Owner-scoped Vector Store management primitives; callers supply admitted resource meters.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result};

use crate::{resources::ResourceMeter, util};

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
fn values(values: &[String]) -> Vec<JsValue> {
    values.iter().map(|v| JsValue::from_str(v)).collect()
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateStore {
    pub name: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    pub expires_after: Option<Expiry>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expiry {
    pub anchor: String,
    pub days: u16,
}

#[derive(Deserialize)]
pub struct StoreRow {
    pub id: String,
    pub name: Option<String>,
    pub metadata_json: String,
    pub created_at: u64,
    pub last_active_at: u64,
    pub expires_after_days: Option<u16>,
    pub status: String,
    pub usage_bytes: u64,
    pub in_progress: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
}

impl StoreRow {
    pub fn public(&self) -> Result<Value> {
        let metadata: BTreeMap<String, String> = serde_json::from_str(&self.metadata_json)?;
        let total = self.in_progress + self.completed + self.failed + self.cancelled;
        Ok(
            json!({"id":self.id,"object":"vector_store","name":self.name.as_deref().unwrap_or("Untitled vector store"),"metadata":metadata,
            "created_at":self.created_at/1000,"last_active_at":self.last_active_at/1000,
            "expires_after":self.expires_after_days.map(|days|Expiry {anchor:"last_active_at".into(),days}),
            "expires_at":self.expires_after_days.map(|days|self.last_active_at/1000+u64::from(days)*86400),
            "status":if self.status=="completed" && self.in_progress>0 {"in_progress"}else{self.status.as_str()},
            "usage_bytes":self.usage_bytes,"file_counts":{"in_progress":self.in_progress,"completed":self.completed,"failed":self.failed,"cancelled":self.cancelled,"total":total}}),
        )
    }
}

pub(crate) const STORE_SELECT: &str = "SELECT vs.id,vs.name,vs.metadata_json,vs.created_at,vs.last_active_at,vs.expires_after_days,vs.status,COALESCE(SUM(vf.usage_bytes),0) AS usage_bytes,COALESCE(SUM(vf.public_status='in_progress'),0) AS in_progress,COALESCE(SUM(vf.public_status='completed'),0) AS completed,COALESCE(SUM(vf.public_status='failed'),0) AS failed,COALESCE(SUM(vf.public_status='cancelled'),0) AS cancelled FROM vector_stores vs LEFT JOIN vector_store_public_files vf ON vf.vector_store_id=vs.id AND vf.account_id=vs.account_id AND vf.removed_at IS NULL";

fn validate_metadata(metadata: &BTreeMap<String, String>) -> Result<()> {
    if metadata.len() > 16
        || metadata.iter().any(|(key, value)| {
            key.is_empty()
                || key.chars().count() > 64
                || value.chars().count() > 512
                || key.chars().any(char::is_control)
        })
    {
        return Err(fail("invalid_vector_store_metadata"));
    }
    Ok(())
}

pub async fn create_store(
    env: &Env,
    owner: &str,
    input: &CreateStore,
    store_limit: u32,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<StoreRow> {
    validate_update(input)?;
    create_validated_store(env, owner, input, store_limit, meter, event).await
}

pub(crate) fn validate_update(input: &CreateStore) -> Result<()> {
    validate_metadata(&input.metadata)?;
    if input
        .name
        .as_ref()
        .is_some_and(|v| v.chars().count() > 256 || v.contains('\0'))
    {
        return Err(fail("invalid_vector_store_name"));
    }
    if input
        .expires_after
        .as_ref()
        .is_some_and(|v| v.anchor != "last_active_at" || !(1..=365).contains(&v.days))
    {
        return Err(fail("invalid_vector_store_expiry"));
    }
    Ok(())
}

async fn create_validated_store(
    env: &Env,
    owner: &str,
    input: &CreateStore,
    store_limit: u32,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<StoreRow> {
    let id = util::id("vs");
    let now = util::now_ms().to_string();
    let metadata = serde_json::to_string(&input.metadata)?;
    let name = input
        .name
        .as_deref()
        .map(JsValue::from_str)
        .unwrap_or(JsValue::NULL);
    let days = input
        .expires_after
        .as_ref()
        .map(|v| JsValue::from_f64(f64::from(v.days)))
        .unwrap_or(JsValue::NULL);
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("INSERT INTO vector_stores(id,account_id,name,metadata_json,created_at,last_active_at,expires_after_days) SELECT ?1,?2,?3,?4,CAST(?5 AS INTEGER),CAST(?5 AS INTEGER),?6 WHERE (SELECT COUNT(*) FROM vector_stores WHERE account_id=?2 AND status IN ('in_progress','completed'))<CAST(?7 AS INTEGER)")
        .bind(&[JsValue::from_str(&id),JsValue::from_str(owner),name,JsValue::from_str(&metadata),JsValue::from_str(&now),days,JsValue::from_f64(f64::from(store_limit))])?.run().await?;
    meter.record_d1(event, &result).await?;
    get_store(env, owner, &id, meter, &format!("{event}:read"))
        .await?
        .ok_or_else(|| fail("vector_store_limit_reached"))
}

pub async fn get_store(
    env: &Env,
    owner: &str,
    id: &str,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Option<StoreRow>> {
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare(format!("{STORE_SELECT} WHERE vs.account_id=?1 AND vs.id=?2 AND vs.status NOT IN ('deleted','deleting') GROUP BY vs.id"))
        .bind(&[JsValue::from_str(owner),JsValue::from_str(id)])?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(result.results::<StoreRow>()?.into_iter().next())
}

pub async fn list_stores(
    env: &Env,
    owner: &str,
    limit: u16,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<Vec<StoreRow>> {
    if !(1..=101).contains(&limit) {
        return Err(fail("invalid_vector_store_list_limit"));
    }
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare(format!("{STORE_SELECT} WHERE vs.account_id=?1 AND vs.status NOT IN ('deleted','deleting') GROUP BY vs.id ORDER BY vs.created_at DESC,vs.id DESC LIMIT ?2"))
        .bind(&[JsValue::from_str(owner),JsValue::from_f64(f64::from(limit))])?.all().await?;
    meter.record_d1(event, &result).await?;
    result.results()
}

pub async fn delete_store(
    env: &Env,
    owner: &str,
    id: &str,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<bool> {
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_stores SET status='deleting' WHERE account_id=?1 AND id=?2 AND status NOT IN ('deleted','deleting') RETURNING id")
        .bind(&[JsValue::from_str(owner),JsValue::from_str(id)])?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(!result.results::<Value>()?.is_empty())
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachFile {
    pub file_id: String,
    #[serde(default)]
    pub attributes: BTreeMap<String, Value>,
    pub chunking_strategy: Option<crate::file_index::ChunkingStrategy>,
}

pub(crate) fn validate_attributes(attributes: &BTreeMap<String, Value>) -> Result<()> {
    if attributes.len() > 16
        || attributes.iter().any(|(key, value)| {
            key.is_empty()
                || key.chars().count() > 64
                || key.chars().any(char::is_control)
                || match value {
                    Value::Bool(_) => false,
                    Value::Number(n) => n.as_f64().is_none_or(|n| !n.is_finite()),
                    Value::String(v) => v.chars().count() > 512,
                    _ => true,
                }
        })
    {
        return Err(fail("invalid_vector_store_attributes"));
    }
    Ok(())
}

/// The source is not copied or pinned as a response attachment. A deleted source
/// immediately ceases to be queryable even while derived cleanup is pending.
pub async fn attach_file(
    env: &Env,
    owner: &str,
    store: &str,
    input: &AttachFile,
    file_limit: u32,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<String> {
    validate_attributes(&input.attributes)?;
    let profile = crate::file_ingest::store_profile(
        env,
        owner,
        store,
        meter,
        &format!("{event}:service"),
        true,
    )
    .await?;
    crate::file_ingest::preflight(&profile, meter)?;
    let strategy = input.chunking_strategy.clone().unwrap_or_default();
    strategy.parameters()?;
    let generation = uuid::Uuid::new_v4().to_string();
    let attributes = serde_json::to_string(&input.attributes)?;
    let bindings = vec![
        store.to_string(),
        owner.to_string(),
        input.file_id.clone(),
        attributes,
        util::now_ms().to_string(),
        generation.clone(),
        file_limit.to_string(),
        serde_json::to_string(&strategy)?,
        serde_json::to_string(&profile)?,
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("INSERT INTO vector_store_files(vector_store_id,file_id,account_id,file_sha256,attributes_json,status,created_at,updated_at,generation,chunking_json,service_profile_json) SELECT ?1,sf.id,?2,sf.sha256,?4,'in_progress',CAST(?5 AS INTEGER),CAST(?5 AS INTEGER),?6,?8,?9 FROM space_files sf JOIN vector_stores vs ON vs.id=?1 AND vs.account_id=?2 AND vs.status='completed' WHERE sf.id=?3 AND sf.account_id=?2 AND sf.status='processed' AND (SELECT COUNT(*) FROM vector_store_files WHERE vector_store_id=?1 AND removed_at IS NULL)<CAST(?7 AS INTEGER) ON CONFLICT(vector_store_id,file_id) DO UPDATE SET generation=excluded.generation,attributes_json=excluded.attributes_json,chunking_json=excluded.chunking_json,service_profile_json=excluded.service_profile_json,status='in_progress',created_at=excluded.created_at,updated_at=excluded.updated_at,removed_at=NULL,source_staged=0,chunk_count=0,usage_bytes=0,last_error_json=NULL,lease_id=NULL,lease_until=0,retry_at=0,embedding_model=NULL,embedding_dimensions=NULL,embedding_revision=NULL,vector_mutation_id=NULL,batch_id=NULL WHERE vector_store_files.removed_at IS NOT NULL RETURNING generation")
        .bind(&values(&bindings))?.all().await?;
    meter.record_d1(event, &result).await?;
    if result.results::<Value>()?.is_empty() {
        return Err(fail("vector_store_source_unavailable_or_limit"));
    }
    Ok(generation)
}

pub async fn detach_file(
    env: &Env,
    owner: &str,
    store: &str,
    file: &str,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<bool> {
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("UPDATE vector_store_files SET status='cancelled',lease_id=NULL,lease_until=0,removed_at=unixepoch()*1000 WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND removed_at IS NULL AND EXISTS(SELECT 1 FROM vector_stores WHERE id=?2 AND account_id=?1 AND status='completed') RETURNING file_id")
        .bind(&[JsValue::from_str(owner),JsValue::from_str(store),JsValue::from_str(file)])?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(!result.results::<Value>()?.is_empty())
}

pub(crate) async fn attach_batch(
    env: &Env,
    owner: &str,
    store: &str,
    files: &[String],
    attributes: &BTreeMap<String, Value>,
    strategy: &crate::file_index::ChunkingStrategy,
    meter: &ResourceMeter<'_>,
    event: &str,
) -> Result<String> {
    validate_attributes(attributes)?;
    let profile = crate::file_ingest::store_profile(
        env,
        owner,
        store,
        meter,
        &format!("{event}:service"),
        true,
    )
    .await?;
    crate::file_ingest::preflight(&profile, meter)?;
    strategy.parameters()?;
    if files.is_empty()
        || files.len() > 500
        || files
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != files.len()
    {
        return Err(fail("invalid_vector_store_file_ids"));
    }
    let batch = util::id("vsfb");
    let bindings = vec![
        owner.to_string(),
        store.to_string(),
        batch.clone(),
        util::now_ms().to_string(),
        serde_json::to_string(files)?,
        serde_json::to_string(attributes)?,
        serde_json::to_string(strategy)?,
        serde_json::to_string(&profile)?,
    ];
    let db = env.d1("DB")?;
    let insert_batch=db.prepare("INSERT INTO vector_store_file_batches(id,account_id,vector_store_id,created_at,status) SELECT ?3,?1,?2,CAST(?4 AS INTEGER),'in_progress' WHERE EXISTS(SELECT 1 FROM vector_stores WHERE id=?2 AND account_id=?1 AND status='completed') AND (SELECT COUNT(*) FROM space_files sf WHERE sf.account_id=?1 AND sf.status='processed' AND sf.id IN (SELECT value FROM json_each(?5)))=json_array_length(?5) AND NOT EXISTS(SELECT 1 FROM vector_store_files vf WHERE vf.vector_store_id=?2 AND vf.removed_at IS NULL AND vf.file_id IN (SELECT value FROM json_each(?5))) AND (SELECT COUNT(*) FROM vector_store_files WHERE vector_store_id=?2 AND removed_at IS NULL)+json_array_length(?5)<=10000").bind(&values(&bindings[..5]))?;
    let insert_files=db.prepare("INSERT INTO vector_store_files(vector_store_id,file_id,account_id,file_sha256,attributes_json,status,created_at,updated_at,generation,batch_id,chunking_json,service_profile_json) SELECT ?2,sf.id,?1,sf.sha256,?6,'in_progress',CAST(?4 AS INTEGER),CAST(?4 AS INTEGER),lower(hex(randomblob(16))),?3,?7,?8 FROM space_files sf JOIN json_each(?5) f ON f.value=sf.id WHERE sf.account_id=?1 AND sf.status='processed' AND EXISTS(SELECT 1 FROM vector_store_file_batches b WHERE b.id=?3 AND b.account_id=?1 AND b.vector_store_id=?2) ON CONFLICT(vector_store_id,file_id) DO UPDATE SET generation=excluded.generation,attributes_json=excluded.attributes_json,chunking_json=excluded.chunking_json,service_profile_json=excluded.service_profile_json,status='in_progress',created_at=excluded.created_at,updated_at=excluded.updated_at,removed_at=NULL,source_staged=0,chunk_count=0,usage_bytes=0,last_error_json=NULL,lease_id=NULL,lease_until=0,retry_at=0,embedding_model=NULL,embedding_dimensions=NULL,embedding_revision=NULL,vector_mutation_id=NULL,batch_id=excluded.batch_id WHERE vector_store_files.removed_at IS NOT NULL").bind(&values(&bindings))?;
    meter.require_d1(&format!("{event}:batch")).await?;
    meter.require_d1(&format!("{event}:files")).await?;
    let results = db.batch(vec![insert_batch, insert_files]).await?;
    meter
        .record_d1(&format!("{event}:batch"), &results[0])
        .await?;
    meter
        .record_d1(&format!("{event}:files"), &results[1])
        .await?;
    let rows = results[0]
        .meta()?
        .and_then(|meta| meta.changes)
        .unwrap_or(0);
    if rows != 1 {
        return Err(fail("vector_store_source_unavailable_or_limit"));
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unnamed_store_keeps_the_sdk_required_string_name() {
        let store = StoreRow {
            id: "vs_test".into(),
            name: None,
            metadata_json: "{}".into(),
            created_at: 1000,
            last_active_at: 1000,
            expires_after_days: None,
            status: "completed".into(),
            usage_bytes: 0,
            in_progress: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
        };
        assert_eq!(store.public().unwrap()["name"], "Untitled vector store");
    }

    #[test]
    fn attributes_are_typed_bounded_and_do_not_accept_objects() {
        let valid = BTreeMap::from([
            ("category".into(), json!("你好")),
            ("year".into(), json!(2026)),
            ("active".into(), json!(true)),
        ]);
        assert!(validate_attributes(&valid).is_ok());
        assert!(
            validate_attributes(&BTreeMap::from([("nested".into(), json!({"secret":1}))])).is_err()
        );
        assert!(
            validate_attributes(&BTreeMap::from([("name".into(), json!("a".repeat(513)))]))
                .is_err()
        );
    }
}
