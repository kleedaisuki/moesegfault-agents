//! Write-ahead checkpoint discovery; collection runs inside the owning session
//! DO's idle fence, never concurrently with a checkpoint publisher.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{Env, Error, Method, Request, RequestInit, Result};

const RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
const BATCH_LIMIT: usize = 64;
const CURSOR_KEY: &str = "checkpoint_gc_cursor_v1";

#[derive(Deserialize)]
struct Intent {
    object_key: String,
    account_id: String,
    session_id: String,
    manifest_key: String,
    created_at_ms: u64,
    next_check_ms: u64,
}

#[derive(Deserialize)]
struct Cursor {
    value: String,
}

#[derive(Default, Deserialize, Serialize)]
struct Position {
    due_ms: u64,
    key: String,
}

#[derive(Deserialize)]
struct ManifestPointer {
    format_version: u8,
    object_key: String,
}

/// Must complete before the archive PUT. UUID-qualified immutable keys are never
/// reused, including after a failed upload or a collected intent.
pub async fn record_upload_intent(env: &Env, owner: &str, session: &str, key: &str) -> Result<()> {
    let manifest = manifest_key(owner, session, key)?;
    let now = crate::util::now_ms();
    let due = now.saturating_add(RETENTION_MS);
    env.d1("DB")?.prepare("INSERT INTO workspace_checkpoint_intents(object_key,account_id,session_id,manifest_key,created_at_ms,next_check_ms) VALUES(?,?,?,?,CAST(? AS INTEGER),CAST(? AS INTEGER)) ON CONFLICT(object_key) DO NOTHING")
        .bind(&[text(key),text(owner),text(session),text(&manifest),text(&now.to_string()),text(&due.to_string())])?.run().await?;
    Ok(())
}

/// Bounded keyset scan, not a bucket listing. Errors advance the scan as well:
/// poison rows cannot starve other sessions, and the next pass retries them.
pub async fn run(env: &Env) -> Result<()> {
    collect_owner_blobs(env).await?;
    let db = env.d1("DB")?;
    let now = crate::util::now_ms();
    let cursor = db
        .prepare("SELECT value FROM resource_system_state WHERE key=?")
        .bind(&[text(CURSOR_KEY)])?
        .first::<Cursor>(None)
        .await?
        .map(|row| serde_json::from_str::<Position>(&row.value))
        .transpose()?
        .unwrap_or_default();
    let rows = db.prepare("SELECT object_key,account_id,session_id,manifest_key,created_at_ms,next_check_ms FROM workspace_checkpoint_intents WHERE (next_check_ms,object_key)>(CAST(? AS INTEGER),?) AND next_check_ms<=CAST(? AS INTEGER) ORDER BY next_check_ms,object_key LIMIT ?")
        .bind(&[text(&cursor.due_ms.to_string()),text(&cursor.key),text(&now.to_string()),text(&BATCH_LIMIT.to_string())])?
        .all().await?.results::<Intent>()?;
    let mut completed = 0;
    for row in &rows {
        if matches!(dispatch(env, row).await, Ok(true)) {
            completed += 1;
        }
    }
    let next = if rows.len() == BATCH_LIMIT {
        rows.last()
            .map(|row| Position {
                due_ms: row.next_check_ms,
                key: row.object_key.clone(),
            })
            .unwrap_or_default()
    } else {
        Position::default()
    };
    db.prepare("INSERT INTO resource_system_state(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(&[text(CURSOR_KEY),text(&serde_json::to_string(&next)?)])?.run().await?;
    if !rows.is_empty() {
        // No object keys, file paths, owner identifiers or R2 error strings.
        crate::util::emit(
            "checkpoint.gc_sweep",
            serde_json::json!({"candidates":rows.len(),"completed":completed,"deferred":rows.len()-completed}),
        );
    }
    Ok(())
}

/// Shared archives have no single session fence. The registry's deleting claim
/// excludes new references before any external I/O, including on retry.
async fn collect_owner_blobs(env: &Env) -> Result<()> {
    let rows = crate::workspace_blobs::claim_expired(env, BATCH_LIMIT).await?;
    let mut completed = 0;
    for row in &rows {
        if delete_owner_blob(env, row).await.is_ok() {
            completed += 1;
        }
    }
    if !rows.is_empty() {
        crate::util::emit(
            "workspace.blob_gc_sweep",
            serde_json::json!({"candidates":rows.len(),"completed":completed,"deferred":rows.len()-completed}),
        );
    }
    Ok(())
}

async fn delete_owner_blob(
    env: &Env,
    archive: &crate::workspace_blobs::OwnedArchiveRef,
) -> Result<()> {
    let bucket = env.bucket("WORKSPACES")?;
    // An upload can outlive its publisher. Recover its physical clock before
    // deleting it; one object key still owns exactly one account-level clock.
    if let Some(object) = bucket.head(&archive.object_key).await? {
        crate::reconcile::register_account_storage_object(
            env,
            &archive.account_id,
            &archive.object_key,
            object.size(),
            object.uploaded().as_millis(),
        )
        .await?;
    }
    bucket.delete(&archive.object_key).await?;
    crate::reconcile::remove_storage_object(env, &archive.object_key, crate::util::now_ms())
        .await?;
    crate::workspace_blobs::finish_delete(env, &archive.object_key).await?;
    Ok(())
}

async fn dispatch(env: &Env, row: &Intent) -> Result<bool> {
    let namespace =
        crate::runtime_profiles::namespace_for(env, &row.account_id, &row.session_id).await?;
    let id = namespace.id_from_name(&format!("{}:{}", row.account_id, row.session_id))?;
    let stub = id.get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post).with_body(Some(
        serde_json::json!({"object_key":row.object_key})
            .to_string()
            .into(),
    ));
    let request = Request::new_with_init("https://session.internal/checkpoint-gc", &init)?;
    let response = stub.fetch_with_request(request).await?;
    match response.status_code() {
        200..=299 => Ok(true),
        409 => Ok(false),
        _ => Err(fail("checkpoint_gc_session_unavailable")),
    }
}

/// Called only under the owning DO's idle/closing fence. The caller must refuse
/// collection while a run, archive, or checkpoint can publish a manifest, and
/// keep new admission/alarm execution excluded until this future completes.
pub async fn collect(env: &Env, owner: &str, session: &str, key: &str) -> Result<()> {
    let expected = manifest_key(owner, session, key)?;
    let db = env.d1("DB")?;
    let row = db.prepare("SELECT object_key,account_id,session_id,manifest_key,created_at_ms,next_check_ms FROM workspace_checkpoint_intents WHERE object_key=? AND account_id=? AND session_id=?")
        .bind(&[text(key),text(owner),text(session)])?.first::<Intent>(None).await?;
    let Some(row) = row else { return Ok(()) };
    let now = crate::util::now_ms();
    if row.manifest_key != expected {
        return Err(fail("checkpoint_gc_manifest_mismatch"));
    }
    if row.next_check_ms > now || now.saturating_sub(row.created_at_ms) < RETENTION_MS {
        return Ok(());
    }
    if crate::workspace_versions::referenced(env, key).await?
        || crate::workspace_blobs::referenced(env, key).await?
    {
        db.prepare("UPDATE workspace_checkpoint_intents SET next_check_ms=MAX(next_check_ms,CAST(? AS INTEGER)) WHERE object_key=?")
            .bind(&[text(&now.saturating_add(RETENTION_MS).to_string()),text(key)])?.run().await?;
        return Ok(());
    }
    let bucket = env.bucket("WORKSPACES")?;
    // Maintenance reads are platform overhead; never invent a user budget hold.
    let current = bucket.get(&expected).execute().await?;
    if let Some(current) = current {
        if current.size() > 16 * 1024 {
            return Err(fail("checkpoint_gc_invalid_manifest"));
        }
        let body = current
            .body()
            .ok_or_else(|| fail("checkpoint_gc_invalid_manifest"))?;
        let pointer: ManifestPointer = serde_json::from_str(&body.text().await?)?;
        if pointer.format_version != 1 {
            return Err(fail("checkpoint_gc_invalid_manifest"));
        }
        if !manifest_key(owner, session, &pointer.object_key).is_ok_and(|key| key == expected)
            && crate::workspace_blobs::find_key(env, owner, &pointer.object_key)
                .await?
                .is_none()
        {
            return Err(fail("checkpoint_gc_invalid_manifest"));
        }
        if pointer.object_key == key {
            db.prepare("UPDATE workspace_checkpoint_intents SET next_check_ms=MAX(next_check_ms,CAST(? AS INTEGER)) WHERE object_key=?")
                .bind(&[text(&now.saturating_add(RETENTION_MS).to_string()),text(key)])?.run().await?;
            return Ok(());
        }
    }
    // R2 DELETE is idempotent, including when the upload never materialized.
    // Keep the intent until both deletion and storage-clock removal complete.
    bucket.delete(key).await?;
    crate::reconcile::remove_storage_object(env, key, crate::util::now_ms()).await?;
    db.prepare("DELETE FROM workspace_checkpoint_intents WHERE object_key=? AND account_id=? AND session_id=?")
        .bind(&[text(key),text(owner),text(session)])?.run().await?;
    Ok(())
}

fn manifest_key(owner: &str, session: &str, key: &str) -> Result<String> {
    let owner = Sha256::digest(owner.as_bytes());
    let session = Sha256::digest(session.as_bytes());
    let prefix = format!("workspaces/{owner:x}/{session:x}");
    let suffix = key
        .strip_prefix(&format!("{prefix}/snapshots/"))
        .ok_or_else(|| fail("checkpoint_gc_invalid_key"))?;
    let (digest, archive) = suffix
        .split_once('/')
        .ok_or_else(|| fail("checkpoint_gc_invalid_key"))?;
    let id = archive
        .strip_suffix(".tar.zst")
        .ok_or_else(|| fail("checkpoint_gc_invalid_key"))?;
    if digest.len() != 64
        || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        || uuid::Uuid::parse_str(id).is_err()
    {
        return Err(fail("checkpoint_gc_invalid_key"));
    }
    Ok(format!("{prefix}/manifest.json"))
}

fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_scope_is_bound_to_both_owner_and_session() {
        let owner = Sha256::digest(b"owner");
        let session = Sha256::digest(b"session");
        let key = format!(
            "workspaces/{owner:x}/{session:x}/snapshots/{}/00000000-0000-0000-0000-000000000001.tar.zst",
            "a".repeat(64)
        );
        assert!(manifest_key("owner", "session", &key).is_ok());
        assert!(manifest_key("other", "session", &key).is_err());
        assert!(manifest_key("owner", "other", &key).is_err());
        assert!(
            manifest_key(
                "owner",
                "session",
                &key.replace("snapshots/", "snapshots/../")
            )
            .is_err()
        );
        assert!(
            manifest_key(
                "owner",
                "session",
                &key.replace(".tar.zst", "/extra.tar.zst")
            )
            .is_err()
        );
    }
}
