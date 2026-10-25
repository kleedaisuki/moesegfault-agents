//! One owner-scoped Space contains user uploads; file names never become R2 keys.
//! Admission reserves bytes before upload. Publication follows verified object
//! storage and retained-byte registration; interrupted publication is recoverable.

use agents_auth::billing::Resource;
use agents_core::Plan;
use js_sys::{Array, Function, Reflect, Uint8Array};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::{Env, Error, Method, Request, Response, Result};

use crate::{reconcile, resources::ResourceMeter, store::Store, util};

#[path = "space_download.rs"]
mod download;
#[path = "space_pin_reconcile.rs"]
mod pin_reconcile;
#[path = "space_pins.rs"]
mod pins;
pub use pin_reconcile::run as reconcile_attachment_pins;
pub use pins::{
    acquire_response_files, list_response_pins, list_response_pins_after, release_response_files,
};

const MULTIPART_LIMIT: u64 = 16 * 1024 * 1024;
const RAW_LIMIT: u64 = 64 * 1024 * 1024;
const TEXT_LIMIT: u64 = 256 * 1024;
const UPLOAD_TIMEOUT_MS: u64 = 120_000;
const RECOVERY_GRACE_MS: u64 = 30_000;
const FILE_COLUMNS: &str = "id,account_id,filename,purpose,content_type,object_key,bytes,sha256,status,reservation_id,created_at_ms,updated_at_ms";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedFile {
    pub id: String,
    pub account_id: String,
    pub filename: String,
    pub purpose: String,
    pub content_type: String,
    pub object_key: String,
    pub bytes: u64,
    pub sha256: String,
    pub status: String,
    pub reservation_id: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Deserialize)]
struct RecoveryFile {
    #[serde(flatten)]
    file: OwnedFile,
    upload_outcome: String,
    upload_operations: Option<u64>,
}

impl OwnedFile {
    pub fn public(&self) -> Value {
        let status = match self.status.as_str() {
            "processed" => "processed",
            "uploading" => "uploaded",
            _ => "error",
        };
        let mut value = json!({"id":self.id,"object":"file","bytes":self.bytes,
            "created_at":self.created_at_ms/1000,"filename":self.filename,
            "purpose":self.purpose,"status":status,"sha256":self.sha256,
            "updated_at":self.updated_at_ms/1000});
        if self.status != "processed" {
            value["diagnostics"] =
                json!({"upload_status":self.status,"operation_id":self.reservation_id});
        }
        value
    }
}

#[derive(Deserialize)]
struct Space {
    id: String,
    name: String,
    created_at_ms: u64,
    updated_at_ms: u64,
}

#[derive(Default, Deserialize)]
struct Usage {
    bytes: u64,
    files: u64,
    versions: u64,
    version_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameSpace {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameFile {
    filename: String,
}

#[derive(Clone, Copy)]
pub(crate) struct SpaceLimits {
    pub bytes: u64,
    pub files: u64,
    pub file_bytes: u64,
    pub versions: u64,
}

pub(crate) fn space_limits(plan: Plan) -> SpaceLimits {
    let (bytes, files) = match plan {
        Plan::Free => (256 * 1024 * 1024, 100),
        Plan::Go => (1024 * 1024 * 1024, 1_000),
        Plan::Plus => (10 * 1024 * 1024 * 1024, 5_000),
        Plan::Pro => (100 * 1024 * 1024 * 1024, 50_000),
    };
    SpaceLimits {
        bytes,
        files,
        file_bytes: RAW_LIMIT,
        versions: files,
    }
}

pub(crate) async fn owner_limits(env: &Env, owner: &str) -> Result<SpaceLimits> {
    Ok(space_limits(Store::new(env).account_plan(owner).await?))
}

fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

fn owner_key(owner: &str) -> String {
    format!("{:x}", Sha256::digest(owner.as_bytes()))
}

async fn ensure_space(env: &Env, owner: &str) -> Result<Space> {
    let db = env.d1("DB")?;
    let id = format!("space_{}", owner_key(owner));
    let now = util::now_ms().to_string();
    db.prepare("INSERT INTO spaces(id,account_id,name,created_at_ms,updated_at_ms) VALUES(?,?,'My Space',CAST(? AS INTEGER),CAST(? AS INTEGER)) ON CONFLICT(account_id) DO NOTHING")
        .bind(&[text(&id),text(owner),text(&now),text(&now)])?.run().await?;
    db.prepare("SELECT id,name,created_at_ms,updated_at_ms FROM spaces WHERE account_id=?")
        .bind(&[text(owner)])?
        .first(None)
        .await?
        .ok_or_else(|| fail("space_not_found"))
}

async fn usage(env: &Env, owner: &str) -> Result<Usage> {
    Ok(env.d1("DB")?.prepare("SELECT (SELECT COALESCE(SUM(bytes),0) FROM space_files WHERE account_id=?1 AND status IN ('uploading','processed','deleting'))+(SELECT COALESCE(SUM(archive_bytes),0) FROM (SELECT object_key,MAX(archive_bytes) AS archive_bytes FROM workspace_versions WHERE account_id=?1 GROUP BY object_key))+(SELECT COALESCE(SUM(archive_bytes),0) FROM workspace_version_uploads u WHERE account_id=?1 AND status IN ('pending','abandoned') AND NOT EXISTS(SELECT 1 FROM workspace_versions v WHERE v.object_key=u.object_key AND v.account_id=u.account_id)) AS bytes,(SELECT COUNT(*) FROM space_files WHERE account_id=?1 AND status IN ('uploading','processed','deleting')) AS files,(SELECT COUNT(*) FROM workspace_versions WHERE account_id=?1) AS versions,(SELECT COALESCE(SUM(archive_bytes),0) FROM (SELECT object_key,MAX(archive_bytes) AS archive_bytes FROM workspace_versions WHERE account_id=?1 GROUP BY object_key)) AS version_bytes")
        .bind(&[text(owner)])?.first(None).await?.unwrap_or_default())
}

pub async fn handle(mut req: Request, env: &Env, owner: &str, path: &str) -> Result<Response> {
    if path == "/v1/space" {
        return space(&mut req, env, owner).await;
    }
    let tail = path
        .strip_prefix("/v1/space/files")
        .or_else(|| path.strip_prefix("/v1/files"));
    let Some(tail) = tail else {
        return util::json_error(404, "not_found", "Resource not found");
    };
    if tail.is_empty() || tail == "/" {
        return match req.method() {
            Method::Get => list(&req, env, owner).await,
            Method::Post => upload(req, env, owner).await,
            _ => util::json_error(405, "method_not_allowed", "Method not allowed"),
        };
    }
    let parts: Vec<_> = tail.trim_start_matches('/').split('/').collect();
    let id = parts[0];
    if parts.len() == 2 && parts[1] == "content" && req.method() == Method::Get {
        return download::content(&req, env, owner, id).await;
    }
    if parts.len() != 1 {
        return util::json_error(404, "not_found", "Resource not found");
    }
    match req.method() {
        Method::Get => match visible_file(env, owner, id).await? {
            Some(file) => Response::from_json(&file.public()),
            None => not_found(),
        },
        Method::Patch => rename_file(&mut req, env, owner, id).await,
        Method::Delete => delete_file(env, owner, id).await,
        _ => util::json_error(405, "method_not_allowed", "Method not allowed"),
    }
}

async fn space(req: &mut Request, env: &Env, owner: &str) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Patch) {
        return util::json_error(405, "method_not_allowed", "Method not allowed");
    }
    let mut space = ensure_space(env, owner).await?;
    if req.method() == Method::Patch {
        let rename: RenameSpace = match small_json(req).await {
            Ok(value) => value,
            Err(_) => return invalid("Provide a valid Space name"),
        };
        if rename.name.trim().is_empty()
            || rename.name.len() > 100
            || rename.name.chars().any(char::is_control)
        {
            return invalid("Space name must contain 1 to 100 bytes without control characters");
        }
        let now = util::now_ms();
        env.d1("DB")?
            .prepare("UPDATE spaces SET name=?,updated_at_ms=CAST(? AS INTEGER) WHERE account_id=?")
            .bind(&[text(&rename.name), text(&now.to_string()), text(owner)])?
            .run()
            .await?;
        space.name = rename.name;
        space.updated_at_ms = now;
    }
    let usage = usage(env, owner).await?;
    let limits = space_limits(Store::new(env).account_plan(owner).await?);
    Response::from_json(&json!({"id":space.id,"object":"space","name":space.name,
        "created_at":space.created_at_ms/1000,"updated_at":space.updated_at_ms/1000,
        "usage":{"bytes":usage.bytes,"files":usage.files,"versions":usage.versions,"version_bytes":usage.version_bytes,"limit_bytes":limits.bytes,"limit_files":limits.files,"file_limit_bytes":limits.file_bytes,"limit_versions":limits.versions}}))
}

async fn small_json<T: serde::de::DeserializeOwned>(req: &mut Request) -> Result<T> {
    use futures::StreamExt;
    let mut body = Vec::new();
    let mut stream = req.stream()?;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > 4096 {
            return Err(fail("invalid_space_request"));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| fail("invalid_space_request"))
}

fn query(req: &Request, key: &str) -> Result<Option<String>> {
    Ok(req
        .url()?
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned()))
}

async fn list(req: &Request, env: &Env, owner: &str) -> Result<Response> {
    let limit = match query(req, "limit")?.map(|s| s.parse::<usize>()).transpose() {
        Ok(Some(value)) if (1..=100).contains(&value) => value,
        Ok(None) => 20,
        _ => return invalid("limit must be between 1 and 100"),
    };
    let direction = query(req, "order")?.unwrap_or_else(|| "desc".into());
    if !matches!(direction.as_str(), "asc" | "desc") {
        return invalid("order must be asc or desc");
    }
    let purpose = query(req, "purpose")?.unwrap_or_default();
    if !purpose.is_empty() && !valid_purpose(&purpose) {
        return invalid("Unsupported file purpose");
    }
    let after = query(req, "after")?.unwrap_or_default();
    let cursor = if after.is_empty() {
        None
    } else {
        visible_file(env, owner, &after).await?
    };
    if !after.is_empty() && cursor.is_none() {
        return invalid("File cursor is not available in your Space");
    }
    let (time, id) = cursor
        .as_ref()
        .map(|file| (file.created_at_ms, file.id.as_str()))
        .unwrap_or((0, ""));
    let cmp = if direction == "asc" { ">" } else { "<" };
    let sql = format!(
        "SELECT {FILE_COLUMNS} FROM space_files WHERE account_id=? AND status!='deleted' AND (?='' OR purpose=?) AND (?='' OR (created_at_ms,id) {cmp} (CAST(? AS INTEGER),?)) ORDER BY created_at_ms {direction},id {direction} LIMIT CAST(? AS INTEGER)"
    );
    let mut files = env
        .d1("DB")?
        .prepare(sql)
        .bind(&[
            text(owner),
            text(&purpose),
            text(&purpose),
            text(&after),
            text(&time.to_string()),
            text(id),
            text(&(limit + 1).to_string()),
        ])?
        .all()
        .await?
        .results::<OwnedFile>()?;
    let more = files.len() > limit;
    files.truncate(limit);
    Response::from_json(
        &json!({"object":"list","data":files.iter().map(OwnedFile::public).collect::<Vec<_>>(),
        "has_more":more,"first_id":files.first().map(|f|f.id.as_str()),"last_id":files.last().map(|f|f.id.as_str())}),
    )
}

pub async fn get_owned_file(env: &Env, owner: &str, id: &str) -> Result<Option<OwnedFile>> {
    if !valid_file_id(id) {
        return Ok(None);
    }
    env.d1("DB")?.prepare(format!("SELECT {FILE_COLUMNS} FROM space_files WHERE id=? AND account_id=? AND status='processed'"))
        .bind(&[text(id),text(owner)])?.first(None).await
}

async fn visible_file(env: &Env, owner: &str, id: &str) -> Result<Option<OwnedFile>> {
    if !valid_file_id(id) {
        return Ok(None);
    }
    env.d1("DB")?.prepare(format!("SELECT {FILE_COLUMNS} FROM space_files WHERE id=? AND account_id=? AND status!='deleted'"))
        .bind(&[text(id),text(owner)])?.first(None).await
}

fn valid_file_id(id: &str) -> bool {
    id.strip_prefix("file-")
        .is_some_and(|tail| tail.len() == 32 && tail.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub fn valid_filename(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|ch| ch.is_control() || ch == '/' || ch == '\\')
}

fn valid_purpose(value: &str) -> bool {
    matches!(
        value,
        "user_data" | "assistants" | "agents" | "vision" | "batch" | "fine-tune" | "evals"
    )
}

fn valid_mime(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 127
        && value.is_ascii()
        && !value.chars().any(char::is_control)
}

fn decode_sha(value: &str) -> Result<Vec<u8>> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(fail("invalid_file_checksum"));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).unwrap_or(""), 16)
                .map_err(|_| fail("invalid_file_checksum"))
        })
        .collect()
}

struct Upload {
    filename: String,
    purpose: String,
    content_type: String,
    bytes: u64,
    sha256: String,
    body: web_sys::ReadableStream,
    inline_key: Option<String>,
}

fn method(target: &JsValue, name: &str) -> Result<Function> {
    Reflect::get(target, &text(name))?
        .dyn_into::<Function>()
        .map_err(|_| fail("file_platform_unavailable"))
}

async fn native_digest(blob: &JsValue) -> Result<String> {
    let buffer = JsFuture::from(
        method(blob, "arrayBuffer")?
            .call0(blob)?
            .dyn_into::<js_sys::Promise>()?,
    )
    .await?;
    let crypto = Reflect::get(&js_sys::global(), &text("crypto"))?;
    let subtle = Reflect::get(&crypto, &text("subtle"))?;
    let result = JsFuture::from(
        method(&subtle, "digest")?
            .call2(&subtle, &text("SHA-256"), &buffer)?
            .dyn_into::<js_sys::Promise>()?,
    )
    .await?;
    Ok(Uint8Array::new(&result)
        .to_vec()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

async fn multipart(req: &Request, length: u64) -> Result<Upload> {
    if length > MULTIPART_LIMIT + 64 * 1024 {
        return Err(fail("file_too_large"));
    }
    let form: JsValue = JsFuture::from(req.inner().form_data()?).await?;
    let keys = method(&form, "keys")?.call0(&form)?;
    for key in Array::from(&keys).iter() {
        if !key
            .as_string()
            .is_some_and(|name| matches!(name.as_str(), "file" | "purpose"))
        {
            return Err(fail("invalid_file_upload"));
        }
    }
    let files = method(&form, "getAll")?.call1(&form, &text("file"))?;
    let files = Array::from(&files);
    if files.length() != 1 {
        return Err(fail("invalid_file_upload"));
    }
    let blob = files.get(0);
    let filename = Reflect::get(&blob, &text("name"))?
        .as_string()
        .ok_or_else(|| fail("invalid_file_upload"))?;
    let bytes = Reflect::get(&blob, &text("size"))?
        .as_f64()
        .ok_or_else(|| fail("invalid_file_upload"))? as u64;
    if bytes > MULTIPART_LIMIT {
        return Err(fail("file_too_large"));
    }
    let purposes = Array::from(&method(&form, "getAll")?.call1(&form, &text("purpose"))?);
    let purpose = match purposes.length() {
        0 => "user_data".into(),
        1 => purposes
            .get(0)
            .as_string()
            .ok_or_else(|| fail("invalid_file_upload"))?,
        _ => return Err(fail("invalid_file_upload")),
    };
    let content_type = Reflect::get(&blob, &text("type"))?
        .as_string()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "application/octet-stream".into());
    let sha256 = native_digest(&blob).await?;
    let body = method(&blob, "stream")?
        .call0(&blob)?
        .dyn_into::<web_sys::ReadableStream>()?;
    Ok(Upload {
        filename,
        purpose,
        content_type,
        bytes,
        sha256,
        body,
        inline_key: None,
    })
}

async fn parse_upload(req: &Request) -> Result<Upload> {
    let length = req
        .headers()
        .get("Content-Length")?
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| fail("file_length_required"))?;
    let mime = req
        .headers()
        .get("Content-Type")?
        .unwrap_or_else(|| "application/octet-stream".into());
    if mime
        .to_ascii_lowercase()
        .starts_with("multipart/form-data;")
    {
        return multipart(req, length).await;
    }
    if length > RAW_LIMIT {
        return Err(fail("file_too_large"));
    }
    let filename = query(req, "filename")?.ok_or_else(|| fail("invalid_file_upload"))?;
    let purpose = query(req, "purpose")?.unwrap_or_else(|| "user_data".into());
    let sha256 = req
        .headers()
        .get("X-Content-Sha256")?
        .ok_or_else(|| fail("invalid_file_checksum"))?;
    decode_sha(&sha256)?;
    let body = match req.inner().body() {
        Some(body) => body,
        None if length == 0 => web_sys::Response::new_with_opt_str(Some(""))?
            .body()
            .ok_or_else(|| fail("invalid_file_upload"))?,
        None => return Err(fail("invalid_file_upload")),
    };
    Ok(Upload {
        filename,
        purpose,
        content_type: mime,
        bytes: length,
        sha256,
        body,
        inline_key: None,
    })
}

async fn upload(req: Request, env: &Env, owner: &str) -> Result<Response> {
    let upload = match parse_upload(&req).await {
        Ok(upload) => upload,
        Err(Error::RustError(code)) if code == "file_too_large" => {
            return util::json_error(413, &code, "The file exceeds the upload size limit");
        }
        Err(Error::RustError(code)) if code == "file_length_required" => {
            return util::json_error(411, &code, "Provide Content-Length for the upload");
        }
        Err(_) => {
            return invalid(
                "Provide one file and a valid purpose; raw uploads require filename, length and SHA-256",
            );
        }
    };
    store_upload(env, owner, upload).await
}

async fn store_upload(env: &Env, owner: &str, upload: Upload) -> Result<Response> {
    if !valid_filename(&upload.filename)
        || !valid_purpose(&upload.purpose)
        || !valid_mime(&upload.content_type)
    {
        return invalid("Invalid filename, content type or file purpose");
    }
    crate::allowances::sync(env, owner).await?;
    let blocked: Option<Value> = env
        .d1("DB")?
        .prepare("SELECT account_id FROM resource_account_blocks WHERE account_id=?")
        .bind(&[text(owner)])?
        .first(None)
        .await?;
    if blocked.is_some() {
        return util::json_error(
            402,
            "storage_debt",
            "Resolve outstanding storage charges before adding files",
        );
    }
    let space = ensure_space(env, owner).await?;
    let limit = space_limits(Store::new(env).account_plan(owner).await?);
    if upload.bytes > limit.file_bytes {
        return util::json_error(
            413,
            "file_too_large",
            "The file exceeds your upload size limit",
        );
    }
    let id = format!("file-{}", uuid::Uuid::new_v4().simple());
    let key = format!("spaces/{}/files/{id}", owner_key(owner));
    let run = format!("space-upload:{id}");
    let now = util::now_ms();
    let db = env.d1("DB")?;
    let inserted: Option<Value>=db.prepare("INSERT INTO space_files(id,account_id,space_id,filename,purpose,content_type,object_key,bytes,sha256,status,reservation_id,created_at_ms,updated_at_ms,recovery_at_ms,inline_key) SELECT ?1,?2,?3,?4,?5,?6,?7,CAST(?8 AS INTEGER),?9,'uploading',?10,CAST(?11 AS INTEGER),CAST(?11 AS INTEGER),CAST(?11 AS INTEGER)+150000,?14 WHERE (SELECT COALESCE(SUM(bytes),0) FROM space_files WHERE account_id=?2 AND status IN ('uploading','processed','deleting'))+(SELECT COALESCE(SUM(archive_bytes),0) FROM (SELECT object_key,MAX(archive_bytes) AS archive_bytes FROM workspace_versions WHERE account_id=?2 GROUP BY object_key))+(SELECT COALESCE(SUM(archive_bytes),0) FROM workspace_version_uploads u WHERE account_id=?2 AND status IN ('pending','abandoned') AND NOT EXISTS(SELECT 1 FROM workspace_versions v WHERE v.object_key=u.object_key AND v.account_id=u.account_id))<=CAST(?12 AS INTEGER)-CAST(?8 AS INTEGER) AND (SELECT COUNT(*) FROM space_files WHERE account_id=?2 AND status IN ('uploading','processed','deleting'))<CAST(?13 AS INTEGER) AND (?14 IS NULL OR NOT EXISTS(SELECT 1 FROM space_files WHERE account_id=?2 AND inline_key=?14 AND status IN ('uploading','processed'))) ON CONFLICT DO NOTHING RETURNING id")
        .bind(&[text(&id),text(owner),text(&space.id),text(&upload.filename),text(&upload.purpose),text(&upload.content_type),text(&key),text(&upload.bytes.to_string()),text(&upload.sha256),text(&run),text(&now.to_string()),text(&limit.bytes.to_string()),text(&limit.files.to_string()),upload.inline_key.as_deref().map(text).unwrap_or(JsValue::NULL)])?.first(None).await?;
    if inserted.is_none() {
        if let Some(key) = &upload.inline_key {
            if let Some(file) = inline_file(env, owner, key).await? {
                if file.status == "processed" {
                    return Response::from_json(&file.public());
                }
                return util::json_error(
                    409,
                    "file_upload_pending",
                    "An identical inline file is still being stored",
                );
            }
        }
        return util::json_error(
            409,
            "space_limit_exceeded",
            "Your Space storage or file-count limit has been reached",
        );
    }
    if let Err(error) = reconcile::maintenance_account_reservation(env, owner, &run).await {
        db.prepare("UPDATE space_files SET status='failed',updated_at_ms=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND status='uploading'")
            .bind(&[text(&util::now_ms().to_string()),text(&id),text(owner)])?.run().await?;
        return Err(error);
    }
    let meter = ResourceMeter::account(env, owner, &run)?;
    meter
        .require_observation("file.upload", Resource::R2Operation, "class_a")
        .await?;
    let object = put_upload(env, &key, &upload, now.saturating_add(UPLOAD_TIMEOUT_MS)).await;
    let operations = u64::from(object.is_ok());
    if !object.is_ok_and(|object| object.is_some_and(|object| object.size() == upload.bytes)) {
        // PUT has definitively returned, including after input deadline abort.
        // Its dispatched operation is known, unlike a crashed invocation.
        set_upload_outcome(env, owner, &id, "failed", operations).await?;
        fail_known_upload(env, owner, &id, &key, &run, operations).await?;
        return util::json_error(
            400,
            "file_upload_failed",
            "File upload did not complete or its checksum did not match; upload it again as a new file",
        );
    }
    set_upload_outcome(env, owner, &id, "stored", 1).await?;
    finish_upload(env, owner, &id).await?;
    let file = get_owned_file(env, owner, &id)
        .await?
        .ok_or_else(|| fail("file_upload_pending"))?;
    Ok(Response::from_json(&file.public())?.with_status(200))
}

pub async fn save_bytes_idempotent(
    env: &Env,
    owner: &str,
    key: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<OwnedFile> {
    if bytes.len() as u64 > MULTIPART_LIMIT {
        return Err(fail("file_too_large"));
    }
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let inline_key = format!("{:x}", Sha256::digest(key.as_bytes()));
    if let Some(file) = inline_file(env, owner, &inline_key).await? {
        if file.sha256 != sha256
            || file.bytes != bytes.len() as u64
            || file.content_type != content_type
        {
            return Err(fail("file_upload_conflict"));
        }
        if file.status == "processed" {
            return Ok(file);
        }
        return Err(fail("file_upload_pending"));
    }
    let array = Uint8Array::from(bytes);
    let native = web_sys::Response::new_with_opt_js_u8_array(Some(&array))?;
    let body = native.body().ok_or_else(|| fail("invalid_file_upload"))?;
    let response = store_upload(
        env,
        owner,
        Upload {
            filename: filename.into(),
            purpose: "user_data".into(),
            content_type: content_type.into(),
            bytes: bytes.len() as u64,
            sha256,
            body,
            inline_key: Some(inline_key),
        },
    )
    .await?;
    if response.status_code() != 200 {
        let status = response.status_code();
        let (_, body) = response.into_parts();
        let code = match body {
            worker::ResponseBody::Body(bytes) => local_upload_error_code(status, &bytes),
            _ => "file_upload_failed",
        };
        return Err(fail(code));
    }
    let mut response = response;
    let value: Value = response.json().await?;
    let id = value["id"]
        .as_str()
        .ok_or_else(|| fail("file_upload_failed"))?;
    get_owned_file(env, owner, id)
        .await?
        .ok_or_else(|| fail("file_upload_failed"))
}

fn local_upload_error_code(status: u16, body: &[u8]) -> &'static str {
    if body.len() > 4096 {
        return "file_upload_failed";
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return "file_upload_failed";
    };
    match (status, value["error"]["code"].as_str()) {
        (400, Some("invalid_file_request")) => "invalid_file_request",
        (402, Some("storage_debt")) => "storage_debt",
        (402, Some("insufficient_credits")) => "insufficient_credits",
        (409, Some("space_limit_exceeded")) => "space_limit_exceeded",
        (409, Some("file_upload_pending")) => "file_upload_pending",
        (413, Some("file_too_large")) => "file_too_large",
        _ => "file_upload_failed",
    }
}

async fn inline_file(env: &Env, owner: &str, key: &str) -> Result<Option<OwnedFile>> {
    env.d1("DB")?.prepare(format!("SELECT {FILE_COLUMNS} FROM space_files WHERE account_id=? AND inline_key=? AND status IN ('uploading','processed')"))
        .bind(&[text(owner),text(key)])?.first(None).await
}

async fn set_upload_outcome(
    env: &Env,
    owner: &str,
    id: &str,
    outcome: &str,
    operations: u64,
) -> Result<()> {
    env.d1("DB")?.prepare("UPDATE space_files SET upload_outcome=?,upload_operations=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND upload_outcome='pending'")
        .bind(&[text(outcome),text(&operations.to_string()),text(id),text(owner)])?.run().await?;
    Ok(())
}

async fn fail_known_upload(
    env: &Env,
    owner: &str,
    id: &str,
    key: &str,
    run: &str,
    operations: u64,
) -> Result<()> {
    let meter = ResourceMeter::account(env, owner, run)?;
    meter
        .record_observation("file.upload", Resource::R2Operation, "class_a", operations)
        .await?;
    meter.settle_run().await?;
    abort_upload(env, owner, id, key).await
}

struct NativeUpload {
    controller: JsValue,
}

impl NativeUpload {
    fn abort(&self) {
        if let Ok(abort) = method(&self.controller, "abort") {
            let _ = abort.call0(&self.controller);
        }
    }
}

impl Drop for NativeUpload {
    fn drop(&mut self) {
        self.abort();
    }
}

async fn put_upload(
    env: &Env,
    key: &str,
    upload: &Upload,
    deadline: u64,
) -> Result<Option<worker::Object>> {
    use futures::future::{Either, select};
    if !upload_may_dispatch(util::now_ms(), deadline) {
        return Err(fail("file_upload_timeout"));
    }
    let bucket = env.bucket("WORKSPACES")?;
    let sha256 = decode_sha(&upload.sha256)?;
    let global = js_sys::global();
    let fixed_constructor: Function =
        Reflect::get(&global, &text("FixedLengthStream"))?.dyn_into()?;
    let fixed = Reflect::construct(
        &fixed_constructor,
        &Array::of1(&JsValue::from_f64(upload.bytes as f64)),
    )?;
    let readable =
        Reflect::get(&fixed, &text("readable"))?.dyn_into::<web_sys::ReadableStream>()?;
    let writable = Reflect::get(&fixed, &text("writable"))?;
    let controller_constructor: Function =
        Reflect::get(&global, &text("AbortController"))?.dyn_into()?;
    let controller = Reflect::construct(&controller_constructor, &Array::new())?;
    let options = js_sys::Object::new();
    Reflect::set(
        &options,
        &text("signal"),
        &Reflect::get(&controller, &text("signal"))?,
    )?;
    if !upload_may_dispatch(util::now_ms(), deadline) {
        return Err(fail("file_upload_timeout"));
    }
    let body: JsValue = upload.body.clone().into();
    let pipe = method(&body, "pipeTo")?
        .call2(&body, &writable, &options)?
        .dyn_into::<js_sys::Promise>()?;
    let native = NativeUpload {
        controller: controller.into(),
    };
    let put = bucket
        .put(key, readable)
        .sha256(sha256)
        .custom_metadata(std::collections::HashMap::from([(
            "sha256".into(),
            upload.sha256.clone(),
        )]))
        .execute();
    let transfer = Box::pin(async { futures::join!(put, JsFuture::from(pipe)) });
    let timeout = Box::pin(worker::Delay::from(std::time::Duration::from_millis(
        deadline.saturating_sub(util::now_ms()),
    )));
    let (result, piped) = match select(transfer, timeout).await {
        Either::Left((result, _)) => result,
        Either::Right((_, transfer)) => {
            native.abort();
            transfer.await
        }
    };
    // Reaching here proves the native PUT API was invoked. Its failure is a
    // known dispatched attempt, not a setup failure or an ambiguous crash.
    Ok(if piped.is_ok() {
        result.ok().flatten()
    } else {
        None
    })
}

fn upload_may_dispatch(now: u64, deadline: u64) -> bool {
    now < deadline
}

async fn abort_upload(env: &Env, owner: &str, id: &str, key: &str) -> Result<()> {
    env.d1("DB")?.prepare("UPDATE space_files SET status='deleting',deletion_reason='upload_abort',updated_at_ms=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND status='uploading' AND upload_outcome!='stored' AND NOT EXISTS(SELECT 1 FROM file_attachment_pins WHERE file_id=space_files.id)")
        .bind(&[text(&util::now_ms().to_string()),text(id),text(owner)])?.run().await?;
    let candidate:Option<OwnedFile>=env.d1("DB")?.prepare(format!("SELECT {FILE_COLUMNS} FROM space_files WHERE id=? AND account_id=? AND object_key=? AND status='deleting' AND deletion_reason='upload_abort'"))
        .bind(&[text(id),text(owner),text(key)])?.first(None).await?;
    match candidate {
        Some(file) => finish_delete(env, &file).await,
        None => Ok(()),
    }
}

async fn finish_upload(env: &Env, owner: &str, id: &str) -> Result<()> {
    let db = env.d1("DB")?;
    let file: OwnedFile=db.prepare(format!("SELECT {FILE_COLUMNS} FROM space_files WHERE id=? AND account_id=? AND status='uploading'"))
        .bind(&[text(id),text(owner)])?.first(None).await?.ok_or_else(||fail("file_upload_pending"))?;
    reconcile::register_account_storage_object(
        env,
        owner,
        &file.object_key,
        file.bytes,
        file.created_at_ms,
    )
    .await?;
    let meter = ResourceMeter::account(env, owner, &file.reservation_id)?;
    meter
        .record_observation("file.upload", Resource::R2Operation, "class_a", 1)
        .await?;
    meter.settle_run().await?;
    db.prepare("UPDATE space_files SET status='processed',updated_at_ms=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND status='uploading'")
        .bind(&[text(&util::now_ms().to_string()),text(id),text(owner)])?.run().await?;
    Ok(())
}

async fn rename_file(req: &mut Request, env: &Env, owner: &str, id: &str) -> Result<Response> {
    let Some(mut file) = get_owned_file(env, owner, id).await? else {
        return not_found();
    };
    let rename: RenameFile = match small_json(req).await {
        Ok(value) => value,
        Err(_) => return invalid("Provide a valid filename"),
    };
    if !valid_filename(&rename.filename) {
        return invalid(
            "Filename must be 1 to 255 bytes without path separators or control characters",
        );
    }
    let now = util::now_ms();
    env.d1("DB")?.prepare("UPDATE space_files SET filename=?,updated_at_ms=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND status='processed'")
        .bind(&[text(&rename.filename),text(&now.to_string()),text(id),text(owner)])?.run().await?;
    file.filename = rename.filename;
    file.updated_at_ms = now;
    Response::from_json(&file.public())
}

async fn delete_file(env: &Env, owner: &str, id: &str) -> Result<Response> {
    if !valid_file_id(id) {
        return not_found();
    }
    let file: Option<OwnedFile>=env.d1("DB")?.prepare(format!("SELECT {FILE_COLUMNS} FROM space_files WHERE id=? AND account_id=? AND status IN ('processed','deleting','deleted','failed')"))
        .bind(&[text(id),text(owner)])?.first(None).await?;
    let Some(file) = file else {
        return not_found();
    };
    if file.status != "deleted" {
        if file.status == "processed" {
            let claimed:Option<Value>=env.d1("DB")?.prepare("UPDATE space_files SET status='deleting',deletion_reason='user',updated_at_ms=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND status='processed' AND NOT EXISTS(SELECT 1 FROM file_attachment_pins WHERE file_id=space_files.id) RETURNING id")
                .bind(&[text(&util::now_ms().to_string()),text(id),text(owner)])?.first(None).await?;
            if claimed.is_none() {
                return util::json_error(
                    409,
                    "file_in_use",
                    "The file is in use by an active response; retry after it finishes or is cancelled",
                );
            }
        }
        if file.status == "failed" {
            env.d1("DB")?.prepare("UPDATE space_files SET deletion_reason='user' WHERE id=? AND account_id=? AND status='failed'")
                .bind(&[text(id),text(owner)])?.run().await?;
        }
        finish_delete(env, &file).await?;
    }
    Response::from_json(&json!({"id":id,"object":"file","deleted":true}))
}

async fn finish_delete(env: &Env, file: &OwnedFile) -> Result<()> {
    let permitted:Option<Value>=env.d1("DB")?.prepare("SELECT id FROM space_files WHERE id=? AND account_id=? AND status IN ('deleting','failed') AND NOT EXISTS(SELECT 1 FROM file_attachment_pins WHERE file_id=space_files.id)")
        .bind(&[text(&file.id),text(&file.account_id)])?.first(None).await?;
    if permitted.is_none() {
        return Err(fail("file_in_use"));
    }
    env.bucket("WORKSPACES")?.delete(&file.object_key).await?;
    reconcile::remove_storage_object(env, &file.object_key, util::now_ms()).await?;
    env.d1("DB")?.prepare("UPDATE space_files SET status=CASE deletion_reason WHEN 'upload_abort' THEN 'failed' ELSE 'deleted' END,updated_at_ms=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND status IN ('deleting','failed')")
        .bind(&[text(&util::now_ms().to_string()),text(&file.id),text(&file.account_id)])?.run().await?;
    Ok(())
}

pub async fn file_object(env: &Env, owner: &str, file: &OwnedFile) -> Result<worker::Object> {
    file_object_range(env, owner, file, None).await
}

async fn file_object_range(
    env: &Env,
    owner: &str,
    file: &OwnedFile,
    range: Option<worker::Range>,
) -> Result<worker::Object> {
    if file.account_id != owner || file.status != "processed" {
        return Err(fail("file_not_found"));
    }
    crate::allowances::sync(env, owner).await?;
    let run = format!("space-download:{}", uuid::Uuid::new_v4());
    reconcile::maintenance_account_reservation(env, owner, &run).await?;
    let meter = ResourceMeter::account(env, owner, &run)?;
    meter
        .require_observation("file.download", Resource::R2Operation, "class_b")
        .await?;
    let bucket = env.bucket("WORKSPACES")?;
    let get = bucket.get(&file.object_key);
    let get = match range {
        Some(range) => get.range(range),
        None => get,
    };
    let object = get.execute().await?;
    meter
        .record_observation("file.download", Resource::R2Operation, "class_b", 1)
        .await?;
    meter.settle_run().await?;
    object
        .filter(|object| object.size() == file.bytes)
        .ok_or_else(|| fail("file_not_ready"))
}

pub async fn read_text(env: &Env, owner: &str, id: &str) -> Result<Option<String>> {
    let Some(bytes) = read_bytes(env, owner, id, TEXT_LIMIT).await? else {
        return Ok(None);
    };
    Ok(String::from_utf8(bytes)
        .ok()
        .filter(|value| !value.contains('\0')))
}

pub async fn read_bytes(
    env: &Env,
    owner: &str,
    id: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>> {
    let file = get_owned_file(env, owner, id)
        .await?
        .ok_or_else(|| fail("file_not_found"))?;
    if file.bytes > max_bytes.min(MULTIPART_LIMIT) {
        return Ok(None);
    }
    let object = file_object(env, owner, &file).await?;
    let bytes = object
        .body()
        .ok_or_else(|| fail("file_not_ready"))?
        .bytes()
        .await?;
    if bytes.len() as u64 != file.bytes || format!("{:x}", Sha256::digest(&bytes)) != file.sha256 {
        return Err(fail("file_integrity_error"));
    }
    Ok(Some(bytes))
}

fn content_disposition(filename: &str) -> String {
    let fallback: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = filename
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

/// A bounded scheduled pass repairs publication/deletion seams, not user content.
pub async fn recover(env: &Env) -> Result<()> {
    let cutoff = util::now_ms();
    let files=env.d1("DB")?.prepare(format!("SELECT {FILE_COLUMNS},upload_outcome,upload_operations FROM space_files WHERE status IN ('uploading','deleting') AND recovery_at_ms<=CAST(? AS INTEGER) ORDER BY recovery_at_ms,id LIMIT 32"))
        .bind(&[text(&cutoff.to_string())])?.all().await?.results::<RecoveryFile>()?;
    for recovering in files {
        let file = &recovering.file;
        // Move each claim ahead before I/O so a poison candidate cannot starve
        // other owners, even if this invocation exits partway through the batch.
        env.d1("DB")?.prepare("UPDATE space_files SET recovery_at_ms=CAST(? AS INTEGER),recovery_attempts=recovery_attempts+1 WHERE id=? AND account_id=? AND status IN ('uploading','deleting')")
            .bind(&[text(&cutoff.saturating_add(300_000).to_string()),text(&file.id),text(&file.account_id)])?.run().await?;
        if recover_file(env, &recovering).await.is_err() {
            util::emit(
                "space.recovery_pending",
                json!({"file_id":file.id,"account_id":file.account_id}),
            );
        }
    }
    Ok(())
}

async fn recover_file(env: &Env, recovering: &RecoveryFile) -> Result<()> {
    let file = &recovering.file;
    if file.status == "deleting" {
        return finish_delete(env, file).await;
    }
    match recovering.upload_outcome.as_str() {
        "stored" => return finish_upload(env, &file.account_id, &file.id).await,
        "failed" => {
            return fail_known_upload(
                env,
                &file.account_id,
                &file.id,
                &file.object_key,
                &file.reservation_id,
                recovering.upload_operations.unwrap_or(0),
            )
            .await;
        }
        _ => {}
    }
    let reservation: Option<Value> = env
        .d1("DB")?
        .prepare("SELECT id FROM usage_reservations WHERE id=? AND account_id=?")
        .bind(&[text(&file.reservation_id), text(&file.account_id)])?
        .first(None)
        .await?;
    if reservation.is_none() {
        // A PUT cannot precede its funding row. No native operation exists to
        // cancel or charge in this crash window.
        env.d1("DB")?.prepare("UPDATE space_files SET status='failed',upload_outcome='failed',upload_operations=0,updated_at_ms=CAST(? AS INTEGER) WHERE id=? AND account_id=? AND status='uploading' AND upload_outcome='pending'")
            .bind(&[text(&util::now_ms().to_string()),text(&file.id),text(&file.account_id)])?.run().await?;
        return Ok(());
    }
    let meter = ResourceMeter::account(env, &file.account_id, &file.reservation_id)?;
    let dispatch:Option<Value>=env.d1("DB")?.prepare("SELECT event_key FROM resource_observations WHERE run_id=? AND account_id=? AND session_id IS NULL AND event_key='file.upload'")
        .bind(&[text(&file.reservation_id),text(&file.account_id)])?.first(None).await?;
    if dispatch.is_none() {
        set_upload_outcome(env, &file.account_id, &file.id, "failed", 0).await?;
        return fail_known_upload(
            env,
            &file.account_id,
            &file.id,
            &file.object_key,
            &file.reservation_id,
            0,
        )
        .await;
    }
    let event = format!("file.recovery:{}", uuid::Uuid::new_v4());
    meter
        .require_observation(&event, Resource::R2Operation, "class_b")
        .await?;
    let object = env.bucket("WORKSPACES")?.head(&file.object_key).await?;
    meter
        .record_observation(&event, Resource::R2Operation, "class_b", 1)
        .await?;
    if object.is_some_and(|object| {
        object.size() == file.bytes
            && object
                .custom_metadata()
                .is_ok_and(|metadata| metadata.get("sha256") == Some(&file.sha256))
    }) {
        set_upload_outcome(env, &file.account_id, &file.id, "stored", 1).await?;
        return finish_upload(env, &file.account_id, &file.id).await;
    }
    if util::now_ms()
        < file
            .created_at_ms
            .saturating_add(UPLOAD_TIMEOUT_MS + RECOVERY_GRACE_MS)
    {
        return Ok(());
    }
    // The bounded input transfer is over. Free the content quota after deleting
    // any candidate; an unknowable crashed operation retains its financial hold
    // and its owner-visible operation ID rather than inventing zero consumption.
    abort_upload(env, &file.account_id, &file.id, &file.object_key).await
}

fn invalid(message: &str) -> Result<Response> {
    util::json_error(400, "invalid_file_request", message)
}
fn not_found() -> Result<Response> {
    util::json_error(404, "file_not_found", "File not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_display_names_not_paths() {
        for name in ["notes.md", "研究笔记.txt", "a b.txt", "a\"b.txt"] {
            assert!(valid_filename(name));
        }
        for name in [
            "",
            " ",
            ".",
            "..",
            "../secret",
            "a/b",
            "a\\b",
            "x\r\ny",
            "x\0",
        ] {
            assert!(!valid_filename(name), "{name:?}");
        }
        assert!(!valid_filename(&"a".repeat(256)));
    }

    #[test]
    fn disposition_cannot_inject_headers_and_preserves_unicode() {
        let value = content_disposition("研究\".txt");
        assert!(!value.contains('\r') && !value.contains('\n'));
        assert!(value.contains("filename*=UTF-8''%E7%A0%94%E7%A9%B6%22.txt"));
    }

    #[test]
    fn checksums_and_identifiers_have_one_canonical_representation() {
        assert_eq!(decode_sha(&"ab".repeat(32)).unwrap(), vec![0xab; 32]);
        for value in ["A".repeat(64), "g".repeat(64), "0".repeat(63)] {
            assert!(decode_sha(&value).is_err());
        }
        assert!(valid_file_id(&format!("file-{}", "a".repeat(32))));
        assert!(!valid_file_id("file-../../other"));
    }

    #[test]
    fn quotas_are_monotone_and_uploads_bounded() {
        let plans = [Plan::Free, Plan::Go, Plan::Plus, Plan::Pro];
        for pair in plans.windows(2) {
            assert!(space_limits(pair[0]).bytes < space_limits(pair[1]).bytes);
        }
        assert_eq!(space_limits(Plan::Pro).file_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn expired_admission_cannot_dispatch_late_upload() {
        assert!(upload_may_dispatch(119_999, 120_000));
        assert!(!upload_may_dispatch(120_000, 120_000));
        assert!(!upload_may_dispatch(180_000, 120_000));
        assert!(!upload_may_dispatch(u64::MAX, u64::MAX));
    }

    #[test]
    fn inline_upload_preserves_only_known_local_errors() {
        for (status, code) in [
            (400, "invalid_file_request"),
            (402, "storage_debt"),
            (402, "insufficient_credits"),
            (409, "space_limit_exceeded"),
            (409, "file_upload_pending"),
            (413, "file_too_large"),
        ] {
            let body = serde_json::to_vec(
                &json!({"error":{"code":code,"message":"private contents must not propagate"}}),
            )
            .unwrap();
            assert_eq!(local_upload_error_code(status, &body), code);
            assert_eq!(local_upload_error_code(500, &body), "file_upload_failed");
        }
        for body in [
            b"not json".as_slice(),
            br#"{"error":{"code":"file_integrity_error","message":"private"}}"#.as_slice(),
            br#"{"error":{"code":"file_upload_failed"}}"#.as_slice(),
        ] {
            assert_eq!(local_upload_error_code(400, body), "file_upload_failed");
        }
        assert_eq!(
            local_upload_error_code(409, &vec![b' '; 4097]),
            "file_upload_failed"
        );
    }
}
