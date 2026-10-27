use serde_json::{Value, json};
use worker::{Error, Method, ResponseBody, Result};

use super::{Workspace, digest_key};
use crate::space::OwnedFile;

impl Workspace<'_> {
    /// The owner lookup precedes this call. Recheck the complete object identity
    /// so an attachment can never select another account or arbitrary R2 key.
    pub async fn materialize_file(&self, file: &OwnedFile) -> Result<Value> {
        validate_file(file, &self.owner)?;
        if let Some(blob) = linked_image(file) {
            if crate::workspace_blobs::find_image_key(self.env, &self.owner, &blob.object_key)
                .await?
                .as_ref()
                != Some(&blob)
            {
                return Err(fail("file_not_ready"));
            }
        }
        self.restore_if_needed().await?;
        let observation = self.require_r2("class_b").await?;
        let object = self
            .env
            .bucket("WORKSPACES")?
            .get(file.content_key())
            .execute()
            .await?;
        self.record_r2(&observation, "class_b").await?;
        let object = object
            .filter(|object| object.size() == file.bytes)
            .ok_or_else(|| fail("file_not_ready"))?;
        let body = object
            .body()
            .ok_or_else(|| fail("file_not_ready"))?
            .response_body()?;
        let ResponseBody::Stream(stream) = body else {
            return Err(fail("file_not_streamable"));
        };
        let path = format!("/v1/files/{}", file.id);
        let mut response = self
            .request_until(
                Method::Put,
                &path,
                Some(stream.into()),
                &[
                    ("Content-Type", "application/octet-stream"),
                    ("Content-Length", &file.bytes.to_string()),
                    ("X-Content-Sha256", &file.sha256),
                ],
                worker::Date::now().as_millis().saturating_add(120_000),
            )
            .await?;
        if response.status_code() == 409 {
            return Err(fail("file_workspace_conflict"));
        }
        if response.status_code() != 200 {
            return Err(fail("file_materialization_failed"));
        }
        let result: Value = response.json().await?;
        let expected_path = format!("uploads/{}/content", file.id);
        if result["file_id"].as_str() != Some(file.id.as_str())
            || result["path"].as_str() != Some(expected_path.as_str())
            || result["sha256"].as_str() != Some(file.sha256.as_str())
            || result["bytes"].as_u64() != Some(file.bytes)
        {
            return Err(fail("file_materialization_invalid"));
        }
        Ok(json!({"file_id":file.id,"filename":file.filename,
            "path":expected_path,"bytes":file.bytes,"sha256":file.sha256}))
    }
}

fn validate_file(file: &OwnedFile, owner: &str) -> Result<()> {
    if file.account_id != owner
        || file.status != "processed"
        || file.bytes > 64 * 1024 * 1024
        || file.id.is_empty()
        || file.id.len() > 128
        || !file
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        || super::decode_sha256(&file.sha256).is_err()
    {
        return Err(fail("file_not_found"));
    }
    let expected = if let Some(blob) = linked_image(file) {
        blob.validate(owner)?;
        format!("space-links/{}/{}", digest_key(owner), file.id)
    } else {
        format!("spaces/{}/files/{}", digest_key(owner), file.id)
    };
    if file.object_key != expected {
        return Err(fail("file_not_found"));
    }
    Ok(())
}

fn linked_image(file: &OwnedFile) -> Option<crate::workspace_blobs::OwnedBlobRef> {
    file.blob_key
        .as_ref()
        .map(|key| crate::workspace_blobs::OwnedBlobRef {
            account_id: file.account_id.clone(),
            object_key: key.clone(),
            sha256: file.sha256.clone(),
            bytes: file.bytes,
            content_type: file.content_type.clone(),
            kind: crate::workspace_blobs::BlobKind::ImageArtifact,
        })
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialization_binds_owner_object_identity_and_bounded_paths() {
        let mut file = OwnedFile {
            id: "file-abc".into(),
            account_id: "owner".into(),
            filename: "../../label.pdf".into(),
            purpose: "user_data".into(),
            content_type: "application/pdf".into(),
            object_key: format!("spaces/{}/files/file-abc", digest_key("owner")),
            blob_key: None,
            bytes: 12,
            sha256: "ab".repeat(32),
            status: "processed".into(),
            reservation_id: "upload".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        assert!(validate_file(&file, "owner").is_ok());
        assert!(validate_file(&file, "other").is_err());
        file.object_key = "private/key".into();
        assert!(validate_file(&file, "owner").is_err());
        file.object_key = format!("spaces/{}/files/file-abc", digest_key("owner"));
        file.id = "../file-abc".into();
        assert!(validate_file(&file, "owner").is_err());
    }
}
