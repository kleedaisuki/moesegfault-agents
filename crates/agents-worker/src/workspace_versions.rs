//! Published workspace versions retain immutable archives independently of a
//! session's moving checkpoint. Only the owning DO may publish a version.

use agents_auth::billing::Resource;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use wasm_bindgen::JsValue;
use worker::{Env, Error, Method, Request, Response, Result};

use crate::{
    metering,
    resources::{ResourceMeter, ResourcePricing},
    util,
};

const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct UploadPayload {
    pub version: Version,
    pub source_key: String,
}

#[derive(Deserialize)]
pub(crate) struct Upload {
    pub object_key: String,
    pub version_json: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Version {
    pub id: String,
    pub account_id: String,
    pub session_id: String,
    pub operation_id: String,
    pub name: String,
    pub object_key: String,
    pub sha256: String,
    pub format_version: u8,
    pub generation: u64,
    pub archive_bytes: u64,
    pub logical_bytes: u64,
    pub entries: u64,
    pub created_at_ms: u64,
}

impl Version {
    pub fn public(&self) -> Value {
        json!({"id":self.id,"object":"workspace.version","name":self.name,
            "session_id":self.session_id,"sha256":self.sha256,
            "format_version":self.format_version,"generation":self.generation,
            "archive_bytes":self.archive_bytes,"logical_bytes":self.logical_bytes,
            "entries":self.entries,"created_at_ms":self.created_at_ms})
    }
}

pub(crate) async fn find(env: &Env, owner: &str, id: &str) -> Result<Option<Version>> {
    env.d1("DB")?
        .prepare("SELECT * FROM workspace_versions WHERE account_id=? AND id=?")
        .bind(&[text(owner), text(id)])?
        .first::<Version>(None)
        .await
}

pub(crate) async fn find_operation(
    env: &Env,
    owner: &str,
    session: &str,
    operation: &str,
) -> Result<Option<Version>> {
    env.d1("DB")?.prepare("SELECT * FROM workspace_versions WHERE account_id=? AND session_id=? AND operation_id=?")
        .bind(&[text(owner),text(session),text(operation)])?.first::<Version>(None).await
}

pub(crate) async fn referenced(env: &Env, key: &str) -> Result<bool> {
    #[derive(Deserialize)]
    struct Reference {
        id: String,
    }
    Ok(env
        .d1("DB")?
        .prepare("SELECT id FROM workspace_versions WHERE object_key=? LIMIT 1")
        .bind(&[text(key)])?
        .first::<Reference>(None)
        .await?
        .is_some_and(|row| !row.id.is_empty()))
}

pub(crate) async fn find_content(
    env: &Env,
    owner: &str,
    sha256: &str,
    bytes: u64,
) -> Result<Option<Version>> {
    env.d1("DB")?.prepare("SELECT * FROM workspace_versions WHERE account_id=? AND sha256=? AND archive_bytes=CAST(? AS INTEGER) LIMIT 1")
        .bind(&[text(owner),text(sha256),text(&bytes.to_string())])?.first::<Version>(None).await
}

pub(crate) async fn pending_upload(
    env: &Env,
    owner: &str,
    session: &str,
    operation: &str,
) -> Result<Option<Upload>> {
    env.d1("DB")?.prepare("SELECT object_key,version_json,status,created_at_ms FROM workspace_version_uploads WHERE account_id=? AND session_id=? AND operation_id=?")
        .bind(&[text(owner),text(session),text(operation)])?.first::<Upload>(None).await
}

pub(crate) async fn activate_upload(env: &Env, key: &str) -> Result<()> {
    let result=env.d1("DB")?.prepare("UPDATE workspace_version_uploads SET created_at_ms=CAST(? AS INTEGER) WHERE object_key=? AND status='pending'")
        .bind(&[text(&util::now_ms().to_string()),text(key)])?.run().await?;
    if result.meta()?.and_then(|m| m.changes).unwrap_or(0) != 1 {
        return Err(fail("workspace_version_upload_expired"));
    }
    Ok(())
}

pub(crate) async fn admit_upload(env: &Env, payload: &UploadPayload) -> Result<Upload> {
    let v = &payload.version;
    let limits = crate::space::owner_limits(env, &v.account_id).await?;
    env.d1("DB")?.prepare("INSERT INTO workspace_version_uploads(object_key,account_id,session_id,operation_id,archive_bytes,version_json,created_at_ms,status) SELECT ?,?,?,?,CAST(? AS INTEGER),?,CAST(? AS INTEGER),'pending' WHERE (SELECT COALESCE(SUM(bytes),0) FROM space_files WHERE account_id=? AND status IN ('uploading','processed','deleting'))+(SELECT COALESCE(SUM(archive_bytes),0) FROM (SELECT object_key,MAX(archive_bytes) AS archive_bytes FROM workspace_versions WHERE account_id=? GROUP BY object_key))+(SELECT COALESCE(SUM(archive_bytes),0) FROM workspace_version_uploads u WHERE account_id=? AND status IN ('pending','abandoned') AND NOT EXISTS(SELECT 1 FROM workspace_versions v WHERE v.object_key=u.object_key))+CAST(? AS INTEGER)<=CAST(? AS INTEGER) AND (SELECT COUNT(*) FROM workspace_versions WHERE account_id=?)+(SELECT COUNT(*) FROM workspace_version_uploads u WHERE account_id=? AND status='pending' AND NOT EXISTS(SELECT 1 FROM workspace_versions v WHERE v.account_id=u.account_id AND v.session_id=u.session_id AND v.operation_id=u.operation_id))<CAST(? AS INTEGER) ON CONFLICT(account_id,session_id,operation_id) DO NOTHING")
        .bind(&[text(&v.object_key),text(&v.account_id),text(&v.session_id),text(&v.operation_id),text(&v.archive_bytes.to_string()),text(&serde_json::to_string(payload)?),text(&v.created_at_ms.to_string()),text(&v.account_id),text(&v.account_id),text(&v.account_id),text(&v.archive_bytes.to_string()),text(&limits.bytes.to_string()),text(&v.account_id),text(&v.account_id),text(&limits.versions.to_string())])?.run().await?;
    pending_upload(env, &v.account_id, &v.session_id, &v.operation_id)
        .await?
        .ok_or_else(|| fail("space_quota_exceeded"))
}

pub(crate) async fn publish(env: &Env, version: &Version) -> Result<Version> {
    let owner_hash = sha2::Sha256::digest(version.account_id.as_bytes());
    if version.name.len() > 100
        || version.name.chars().any(char::is_control)
        || !version.object_key.starts_with(&format!(
            "space-versions/{owner_hash:x}/{}/",
            version.sha256
        ))
        || version.operation_id.is_empty()
        || version.format_version != 1
        || version.archive_bytes == 0
        || version.archive_bytes > MAX_ARCHIVE_BYTES
        || version.sha256.len() != 64
        || !version.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(fail("invalid_workspace_version"));
    }
    let limits = crate::space::owner_limits(env, &version.account_id).await?;
    env.d1("DB")?.prepare("INSERT INTO workspace_versions(id,account_id,session_id,operation_id,name,object_key,sha256,format_version,generation,archive_bytes,logical_bytes,entries,created_at_ms) SELECT ?,?,?,?,?,?,?,CAST(? AS INTEGER),CAST(? AS INTEGER),CAST(? AS INTEGER),CAST(? AS INTEGER),CAST(? AS INTEGER),CAST(? AS INTEGER) WHERE (SELECT COUNT(*) FROM workspace_versions WHERE account_id=?)+(SELECT COUNT(*) FROM workspace_version_uploads u WHERE account_id=? AND status='pending' AND NOT(session_id=? AND operation_id=?) AND NOT EXISTS(SELECT 1 FROM workspace_versions v WHERE v.account_id=u.account_id AND v.session_id=u.session_id AND v.operation_id=u.operation_id))<CAST(? AS INTEGER) AND NOT EXISTS(SELECT 1 FROM workspace_version_uploads WHERE account_id=? AND session_id=? AND operation_id=? AND status='abandoned') ON CONFLICT(account_id,session_id,operation_id) DO NOTHING")
        .bind(&[text(&version.id),text(&version.account_id),text(&version.session_id),text(&version.operation_id),text(&version.name),text(&version.object_key),text(&version.sha256),text(&version.format_version.to_string()),text(&version.generation.to_string()),text(&version.archive_bytes.to_string()),text(&version.logical_bytes.to_string()),text(&version.entries.to_string()),text(&version.created_at_ms.to_string()),text(&version.account_id),text(&version.account_id),text(&version.session_id),text(&version.operation_id),text(&limits.versions.to_string()),text(&version.account_id),text(&version.session_id),text(&version.operation_id)])?
        .run().await?;
    let result = find_operation(
        env,
        &version.account_id,
        &version.session_id,
        &version.operation_id,
    )
    .await?
    .ok_or_else(|| fail("space_quota_exceeded"))?;
    env.d1("DB")?.prepare("UPDATE workspace_version_uploads SET status='published' WHERE object_key=? AND account_id=? AND session_id=? AND operation_id=? AND status='pending'")
        .bind(&[text(&result.object_key),text(&result.account_id),text(&result.session_id),text(&result.operation_id)])?.run().await?;
    Ok(result)
}

pub async fn cleanup_uploads(env: &Env) -> Result<()> {
    let now = util::now_ms();
    let rows=env.d1("DB")?.prepare("SELECT object_key,version_json,status,created_at_ms FROM workspace_version_uploads WHERE status IN ('pending','abandoned') AND created_at_ms<CAST(? AS INTEGER) ORDER BY created_at_ms,object_key LIMIT 64")
        .bind(&[text(&now.saturating_sub(86_400_000).to_string())])?.all().await?.results::<Upload>()?;
    for row in rows {
        if referenced(env, &row.object_key).await? {
            env.d1("DB")?
                .prepare(
                    "UPDATE workspace_version_uploads SET status='published' WHERE object_key=?",
                )
                .bind(&[text(&row.object_key)])?
                .run()
                .await?;
            continue;
        }
        let claim=env.d1("DB")?.prepare("UPDATE workspace_version_uploads SET status='abandoned' WHERE object_key=? AND status IN ('pending','abandoned') AND created_at_ms<CAST(? AS INTEGER) AND NOT EXISTS(SELECT 1 FROM workspace_versions WHERE object_key=?)")
            .bind(&[text(&row.object_key),text(&now.saturating_sub(86_400_000).to_string()),text(&row.object_key)])?.run().await?;
        if claim.meta()?.and_then(|m| m.changes).unwrap_or(0) == 0 {
            continue;
        }
        let payload: UploadPayload = serde_json::from_str(&row.version_json)?;
        if let Some(object) = env.bucket("WORKSPACES")?.head(&row.object_key).await? {
            crate::reconcile::register_storage_object(
                env,
                &payload.version.account_id,
                &payload.version.session_id,
                &row.object_key,
                object.size(),
                object.uploaded().as_millis(),
            )
            .await?;
        }
        env.bucket("WORKSPACES")?.delete(&row.object_key).await?;
        crate::reconcile::remove_storage_object(env, &row.object_key, util::now_ms()).await?;
        env.d1("DB")?
            .prepare(
                "DELETE FROM workspace_version_uploads WHERE object_key=? AND status='abandoned'",
            )
            .bind(&[text(&row.object_key)])?
            .run()
            .await?;
    }
    Ok(())
}

pub async fn list(env: &Env, owner: &str, after: Option<&str>, limit: usize) -> Result<Value> {
    if !(1..=100).contains(&limit) {
        return Err(fail("invalid_workspace_version_limit"));
    }
    let cursor = match after {
        Some(id) => Some(
            find(env, owner, id)
                .await?
                .ok_or_else(|| fail("workspace_version_cursor_not_found"))?,
        ),
        None => None,
    };
    let (created, id) = cursor
        .as_ref()
        .map(|v| (v.created_at_ms, v.id.as_str()))
        .unwrap_or((i64::MAX as u64, "~"));
    let mut rows = env.d1("DB")?.prepare("SELECT * FROM workspace_versions WHERE account_id=? AND (created_at_ms,id)<(CAST(? AS INTEGER),?) ORDER BY created_at_ms DESC,id DESC LIMIT ?")
        .bind(&[text(owner),text(&created.to_string()),text(id),text(&(limit+1).to_string())])?
        .all().await?.results::<Version>()?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let first = rows.first().map(|v| v.id.clone());
    let last = rows.last().map(|v| v.id.clone());
    Ok(
        json!({"object":"list","data":rows.iter().map(Version::public).collect::<Vec<_>>(),
        "has_more":has_more,"first_id":first,"last_id":last,
        "next_cursor":if has_more { last } else { None }}),
    )
}

pub async fn route(req: Request, env: &Env, owner: &str, path: &[&str]) -> Result<Response> {
    if req.method() != Method::Get {
        return util::json_error(
            405,
            "method_not_allowed",
            "Workspace versions are immutable; only reading and downloading are supported",
        );
    }
    match path {
        [] => {
            let url = req.url()?;
            let mut after = None;
            let mut limit = 50;
            for (key, value) in url.query_pairs() {
                match key.as_ref() {
                    "after" if after.is_none() => after = Some(value.into_owned()),
                    "limit" => match value.parse::<usize>() {
                        Ok(value) if (1..=100).contains(&value) => limit = value,
                        _ => {
                            return util::json_error(
                                400,
                                "invalid_limit",
                                "Limit must be between 1 and 100",
                            );
                        }
                    },
                    _ => {
                        return util::json_error(
                            400,
                            "invalid_query",
                            "Unsupported workspace version query",
                        );
                    }
                }
            }
            if let Some(id) = after.as_deref() {
                if find(env, owner, id).await?.is_none() {
                    return util::json_error(
                        400,
                        "invalid_cursor",
                        "Workspace version cursor is unavailable",
                    );
                }
            }
            Response::from_json(&list(env, owner, after.as_deref(), limit).await?)
        }
        [id] => match find(env, owner, id).await? {
            Some(version) => Response::from_json(&version.public()),
            None => util::json_error(404, "not_found", "Workspace version not found"),
        },
        [id, "content"] => match find(env, owner, id).await? {
            Some(version) => download(env, owner, &version).await,
            None => util::json_error(404, "not_found", "Workspace version not found"),
        },
        _ => util::json_error(404, "not_found", "Workspace version route not found"),
    }
}

async fn download(env: &Env, owner: &str, version: &Version) -> Result<Response> {
    crate::allowances::sync(env, owner).await?;
    let pricing = ResourcePricing::parse(&env.var("RESOURCE_PRICING_JSON")?.to_string())?;
    let charge = pricing
        .tariff(&Resource::R2Operation, "class_b")?
        .charge(1)
        .map_err(|_| fail("invalid_resource_tariff"))?;
    let download = format!("download:{}", uuid::Uuid::new_v4());
    metering::reserve(
        &env.d1("DB")?,
        owner,
        Some(&version.session_id),
        &download,
        &download,
        &format!("workspace-version:{}", version.id),
        charge.amount_microusd.get(),
        (util::now_ms() / 1000) as i64,
    )
    .await?;
    let meter = ResourceMeter::with_pricing(env, owner, &version.session_id, &download, pricing)?;
    meter
        .require_observation(
            "workspace.version.download",
            Resource::R2Operation,
            "class_b",
        )
        .await?;
    let object = env
        .bucket("WORKSPACES")?
        .get(&version.object_key)
        .execute()
        .await?;
    meter
        .record_observation(
            "workspace.version.download",
            Resource::R2Operation,
            "class_b",
            1,
        )
        .await?;
    meter.settle_run().await?;
    let Some(object) = object else {
        return unavailable();
    };
    if object.size() != version.archive_bytes {
        return unavailable();
    }
    let Some(body) = object.body() else {
        return unavailable();
    };
    let mut response = Response::from_body(body.response_body()?)?;
    let headers = response.headers_mut();
    headers.set("Content-Type", "application/zstd")?;
    headers.set("Content-Length", &version.archive_bytes.to_string())?;
    headers.set("X-Snapshot-Sha256", &version.sha256)?;
    headers.set("X-Snapshot-Generation", &version.generation.to_string())?;
    headers.set(
        "X-Snapshot-Format-Version",
        &version.format_version.to_string(),
    )?;
    headers.set(
        "X-Snapshot-Logical-Bytes",
        &version.logical_bytes.to_string(),
    )?;
    headers.set("X-Snapshot-Entries", &version.entries.to_string())?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    headers.set(
        "Content-Disposition",
        &format!(
            "attachment; filename=\"workspace-{}.tar.zst\"",
            crate::exports::filename_id(&version.id)
        ),
    )?;
    Ok(response)
}

fn unavailable() -> Result<Response> {
    util::json_error(
        409,
        "workspace_version_unavailable",
        "The persisted workspace version is unavailable; contact support without retrying execution",
    )
}
fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
