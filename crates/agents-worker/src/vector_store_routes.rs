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
            json!({"id":self.id,"object":"vector_store","name":self.name,"metadata":metadata,
            "created_at":self.created_at/1000,"last_active_at":self.last_active_at/1000,
            "expires_after":self.expires_after_days.map(|days|Expiry {anchor:"last_active_at".into(),days}),
            "expires_at":self.expires_after_days.map(|days|self.last_active_at/1000+u64::from(days)*86400),
            "status":if self.status=="completed" && self.in_progress>0 {"in_progress"}else{self.status.as_str()},
            "usage_bytes":self.usage_bytes,"file_counts":{"in_progress":self.in_progress,"completed":self.completed,"failed":self.failed,"cancelled":self.cancelled,"total":total}}),
        )
    }
}

const STORE_SELECT: &str = "SELECT vs.id,vs.name,vs.metadata_json,vs.created_at,vs.last_active_at,vs.expires_after_days,vs.status,COALESCE(SUM(vf.usage_bytes),0) AS usage_bytes,COALESCE(SUM(vf.status='in_progress'),0) AS in_progress,COALESCE(SUM(vf.status='completed'),0) AS completed,COALESCE(SUM(vf.status='failed'),0) AS failed,COALESCE(SUM(vf.status='cancelled'),0) AS cancelled FROM vector_stores vs LEFT JOIN vector_store_files vf ON vf.vector_store_id=vs.id AND vf.account_id=vs.account_id";

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
    if !(1..=100).contains(&limit) {
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

fn validate_attributes(attributes: &BTreeMap<String, Value>) -> Result<()> {
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
    let strategy = input.chunking_strategy.clone().unwrap_or_default();
    strategy.parameters()?;
    if !matches!(strategy, crate::file_index::ChunkingStrategy::Auto) {
        return Err(fail("static_chunking_tokenizer_not_configured"));
    }
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
    ];
    meter.require_d1(event).await?;
    let result=env.d1("DB")?.prepare("INSERT INTO vector_store_files(vector_store_id,file_id,account_id,file_sha256,attributes_json,status,created_at,updated_at,generation,chunking_json) SELECT ?1,sf.id,?2,sf.sha256,?4,'in_progress',CAST(?5 AS INTEGER),CAST(?5 AS INTEGER),?6,?8 FROM space_files sf JOIN vector_stores vs ON vs.id=?1 AND vs.account_id=?2 AND vs.status='completed' WHERE sf.id=?3 AND sf.account_id=?2 AND sf.status='processed' AND (SELECT COUNT(*) FROM vector_store_files WHERE vector_store_id=?1)<CAST(?7 AS INTEGER) RETURNING generation")
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
    let result=env.d1("DB")?.prepare("UPDATE vector_store_files SET status='cancelled',lease_id=NULL,lease_until=0 WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND status!='cancelled' AND EXISTS(SELECT 1 FROM vector_stores WHERE id=?2 AND account_id=?1 AND status='completed') RETURNING file_id")
        .bind(&[JsValue::from_str(owner),JsValue::from_str(store),JsValue::from_str(file)])?.all().await?;
    meter.record_d1(event, &result).await?;
    Ok(!result.results::<Value>()?.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

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
