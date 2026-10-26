//! Public file inputs use the existing credential-free global Fetch reader.
//! Per-response preparation receipts are temporary Session data, not user Files
//! or telemetry. An interrupted dispatch cannot authorize a second download.
use std::{cell::Cell, rc::Rc};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::{Env, ListOptions, Result, Storage};

use super::{
    MAX_INLINE_BYTES, MAX_TEXT_BYTES, OwnedFile, fail, inline_text, present, workspace_path,
    workspace_text,
};

#[derive(Serialize, Deserialize)]
pub(super) struct Frozen {
    pub part: serde_json::Value,
    pub attachment: Option<OwnedFile>,
    pub bytes: usize,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    digest: String,
    status: String,
    error: Option<String>,
}

pub(super) fn validate_url(url: &str) -> Result<String> {
    crate::public_image_input::validate_public_input(url, MAX_INLINE_BYTES)
        .map_err(|_| fail("invalid_file_url"))
}

fn source(part: &serde_json::Value) -> Result<(&str, &str)> {
    let url = part["file_url"]
        .as_str()
        .ok_or_else(|| fail("invalid_file_input"))?;
    validate_url(url)?;
    let filename = match part.get("filename") {
        None => "remote-input",
        Some(name) => name
            .as_str()
            .filter(|name| crate::space::valid_filename(name))
            .ok_or_else(|| fail("invalid_file_name"))?,
    };
    Ok((url, filename))
}

async fn download(
    env: &Env,
    owner: &str,
    part: &serde_json::Value,
    key: Option<&str>,
    workspace: bool,
    max_bytes: usize,
) -> Result<Frozen> {
    let (url, filename) = source(part)?;
    let host = validate_url(url)?;
    let fetched = crate::image_generation::fetch_public_image_bytes(url, &[host], max_bytes, "*/*")
        .await
        .map_err(|error| match error {
            worker::Error::RustError(code)
                if matches!(
                    code.as_str(),
                    "image_generation_image_too_large" | "image_generation_response_too_large"
                ) =>
            {
                fail("attachments_too_large")
            }
            _ => fail("remote_file_unavailable"),
        })?;
    if fetched.bytes.is_empty() {
        return Err(fail("remote_file_unavailable"));
    }
    let bytes = fetched.bytes.len();
    let converted = crate::file_vector::document::attachment_text(env, &fetched.bytes)
        .await
        .map_err(|_| fail("remote_file_unavailable"))?;
    let text = converted.as_deref().or_else(|| {
        if fetched
            .declared_mime
            .as_deref()
            .is_some_and(|mime| mime.starts_with("image/"))
        {
            return None;
        }
        std::str::from_utf8(&fetched.bytes)
            .ok()
            .filter(|text| !text.contains('\0'))
    });
    if let Some(text) = text.filter(|text| text.len() <= MAX_TEXT_BYTES) {
        return Ok(Frozen {
            part: inline_text(filename, text),
            attachment: None,
            bytes: bytes.max(text.len()),
        });
    }
    if !workspace {
        return Err(fail("file_requires_workspace"));
    }
    let key = key.ok_or_else(|| fail("auxiliary_binary_attachment_unavailable"))?;
    // This is a real owner/quota/funded R2 publication, never a synthetic path.
    let media = fetched
        .declared_mime
        .as_deref()
        .filter(|mime| mime.len() <= 128 && mime.is_ascii())
        .unwrap_or("application/octet-stream");
    let file =
        crate::space::save_bytes_idempotent(env, owner, key, filename, media, &fetched.bytes)
            .await?;
    let path = workspace_path(&file.id)?;
    Ok(Frozen {
        part: workspace_text(&file, &path),
        attachment: Some(file),
        bytes,
    })
}

async fn claim(storage: &Storage, key: &str, digest: &str) -> Result<bool> {
    let claimed = Rc::new(Cell::new(false));
    let output = claimed.clone();
    let key = key.to_owned();
    let digest = digest.to_owned();
    storage
        .transaction(move |transaction| async move {
            let entries = transaction.get_multiple(vec![key.clone()]).await?;
            if !entries.has(&key.clone().into()) {
                let receipt = Receipt {
                    digest,
                    status: "dispatched".into(),
                    error: None,
                };
                transaction
                    .put(&key, serde_json::to_string(&receipt)?)
                    .await?;
                output.set(true);
            }
            Ok(())
        })
        .await?;
    Ok(claimed.get())
}

pub(super) async fn resolve(
    env: &Env,
    owner: &str,
    part: &serde_json::Value,
    inline_key: Option<&str>,
    storage: Option<&Storage>,
    workspace: bool,
    max_bytes: usize,
) -> Result<Frozen> {
    source(part)?;
    if max_bytes == 0 || max_bytes > MAX_INLINE_BYTES {
        return Err(fail("attachments_too_large"));
    }
    if present(part, "file_id").is_some() || present(part, "file_data").is_some() {
        return Err(fail("invalid_file_input"));
    }
    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&(owner, part))?));
    let key = inline_key.map(|key| format!("remote-file:{key}"));
    if let Some((storage, key)) = storage.zip(key.as_deref()) {
        if let Some(receipt) = storage.get::<String>(key).await? {
            let receipt: Receipt = serde_json::from_str(&receipt)?;
            if receipt.digest != digest {
                return Err(fail("idempotency_conflict"));
            }
            if receipt.status == "completed" {
                let bytes = crate::session_storage::read_chunks(storage, &format!("{key}:result"))
                    .await?
                    .ok_or_else(|| fail("remote_file_outcome_unknown"))?;
                return crate::session_storage::decode_json(&bytes);
            }
            return Err(fail(
                receipt
                    .error
                    .as_deref()
                    .unwrap_or("remote_file_outcome_unknown"),
            ));
        }
    }
    // Count has no Session. Both modes fund their actual execution resource
    // before any external read; model money is not used as an I/O overdraft.
    let resource = if storage.is_some() {
        agents_auth::billing::Resource::DurableObject
    } else {
        agents_auth::billing::Resource::Worker
    };
    let operation =
        crate::conversation_meter::ConversationOperation::account_resource(env, owner, resource)
            .await?;
    let outcome = async {
        if let Some((storage, key)) = storage.zip(key.as_deref()) {
            if !claim(storage, key, &digest).await? {
                return Err(fail("remote_file_outcome_unknown"));
            }
        }
        let result = download(env, owner, part, inline_key, workspace, max_bytes).await;
        if let Some((storage, key)) = storage.zip(key.as_deref()) {
            let (status, error) = match &result {
                Ok(frozen) => {
                    crate::session_storage::write_chunks(
                        storage,
                        &format!("{key}:result"),
                        &serde_json::to_vec(frozen)?,
                    )
                    .await?;
                    ("completed", None)
                }
                Err(worker::Error::RustError(code)) => ("failed", Some(code.clone())),
                Err(_) => ("failed", Some("remote_file_unavailable".into())),
            };
            storage
                .put(
                    key,
                    serde_json::to_string(&Receipt {
                        digest,
                        status: status.into(),
                        error,
                    })?,
                )
                .await?;
        }
        result
    }
    .await;
    operation.finish(env, owner).await?;
    outcome
}

/// Accepted Response/native storage owns the frozen execution view from here.
/// Remove only the disposable preparation receipt, never independently owned Files.
pub(crate) async fn erase(storage: &Storage, response_id: &str) -> Result<()> {
    let prefix = format!("remote-file:{response_id}:");
    loop {
        let entries = storage
            .list_with_options(ListOptions::new().prefix(&prefix).limit(128))
            .await?;
        let keys = js_sys::Array::from(&entries.keys())
            .iter()
            .filter_map(|key| key.as_string())
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return Ok(());
        }
        storage.delete_multiple(keys).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remote_files_share_public_origin_policy_without_leaking_signed_urls_into_text() {
        let part = json!({"type":"input_file","file_url":"https://files.example.com/notes?secret=opaque","filename":"notes.txt"});
        assert_eq!(source(&part).unwrap().1, "notes.txt");
        assert_eq!(
            source(&json!({"file_url":"https://files.example.com/a"}))
                .unwrap()
                .1,
            "remote-input"
        );
        for url in [
            "http://files.example.com/a",
            "https://127.0.0.1/a",
            "https://[::1]/a",
            "https://localhost/a",
            "https://host.internal/a",
            "https://user:secret@files.example.com/a",
            "https://files.example.com/a#fragment",
        ] {
            assert!(validate_url(url).is_err(), "{url}");
        }
        assert!(
            source(&json!({"file_url":"https://files.example.com/a", "filename":"../a"})).is_err()
        );
        let rendered = inline_text("notes.txt", "actual content 雪");
        assert!(!rendered.to_string().contains("secret"));
        assert!(!rendered.to_string().contains("files.example.com"));
    }
}
