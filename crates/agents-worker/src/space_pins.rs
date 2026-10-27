//! Attachment liveness is owned by the canonical Session DO, not a wall clock.

use serde::Deserialize;
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result};

use super::OwnedFile;

const MAX_ATTACHMENTS: usize = 128;
const RESPONSE_PAGE: usize = 128;

fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

/// Call under the owning DO's admission/persistence mutex and retain it through
/// canonical acceptance. The statement and its validation triggers are atomic:
/// one missing/stale attachment aborts every insert, including new references.
pub async fn acquire_response_files(
    env: &Env,
    owner: &str,
    session: &str,
    response: &str,
    files: &[OwnedFile],
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    if files.len() > MAX_ATTACHMENTS
        || files
            .iter()
            .any(|file| file.account_id != owner || file.status != "processed")
    {
        return Err(fail("file_not_ready"));
    }
    let references:Vec<_>=files.iter().map(|file|json!({"id":file.id,"object_key":file.object_key,"sha256":file.sha256,"bytes":file.bytes})).collect();
    env.d1("DB")?.prepare("INSERT INTO file_attachment_pins(response_id,file_id,account_id,session_id,object_key,sha256,bytes,created_at_ms) SELECT ?1,json_extract(value,'$.id'),?2,?3,json_extract(value,'$.object_key'),json_extract(value,'$.sha256'),CAST(json_extract(value,'$.bytes') AS INTEGER),CAST(?5 AS INTEGER) FROM json_each(?4) WHERE 1 ON CONFLICT(response_id,file_id) DO NOTHING")
        .bind(&[text(response),text(owner),text(session),text(&serde_json::to_string(&references)?),text(&crate::util::now_ms().to_string())])?.run().await.map_err(pin_error)?;
    Ok(())
}

fn pin_error(error: Error) -> Error {
    if error
        .to_string()
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .any(|token| token == "file_not_ready")
    {
        return fail("file_not_ready");
    }
    error
}

/// Never infer permission to release from age, a failed read, or a routing row.
/// The caller must first commit terminal/cancelled state, or prove non-admission
/// from the authoritative DO while holding its admission/persistence mutex.
pub async fn release_response_files(
    env: &Env,
    owner: &str,
    session: &str,
    response: &str,
) -> Result<()> {
    env.d1("DB")?.prepare("DELETE FROM file_attachment_pins WHERE account_id=? AND session_id=? AND response_id=?")
        .bind(&[text(owner),text(session),text(response)])?.run().await?;
    Ok(())
}

#[derive(Deserialize)]
struct PinnedResponse {
    response_id: String,
}

pub async fn list_response_pins(env: &Env, owner: &str, session: &str) -> Result<Vec<String>> {
    list_response_pins_after(env, owner, session, "").await
}

/// A keyset page lets recovery progress even when an earlier response stays live.
pub async fn list_response_pins_after(
    env: &Env,
    owner: &str,
    session: &str,
    after: &str,
) -> Result<Vec<String>> {
    let rows=env.d1("DB")?.prepare("SELECT DISTINCT response_id FROM file_attachment_pins WHERE account_id=? AND session_id=? AND response_id>? ORDER BY response_id LIMIT CAST(? AS INTEGER)")
        .bind(&[text(owner),text(session),text(after),text(&RESPONSE_PAGE.to_string())])?.all().await?.results::<PinnedResponse>()?;
    Ok(rows.into_iter().map(|row| row.response_id).collect())
}
