//! Hosted-tool artifacts use the same owner/quota/publication boundary as uploads.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Env, Error, Result};

use crate::space;

pub const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ARTIFACTS: u16 = 128;

pub enum ArtifactIdentity<'a> {
    ExecutionSlot {
        execution_id: &'a str,
        ordinal: u16,
    },
    InterpreterVersion {
        container_id: &'a str,
        artifact_id: &'a str,
    },
}

impl ArtifactIdentity<'_> {
    pub fn key(self) -> Result<String> {
        match self {
            Self::ExecutionSlot {
                execution_id,
                ordinal,
            } => artifact_key(execution_id, ordinal),
            Self::InterpreterVersion {
                container_id,
                artifact_id,
            } => {
                if !component(container_id, 256) || !component(artifact_id, 256) {
                    return Err(failure());
                }
                // The root supervisor binds this immutable descriptor to code
                // context, relative path and digest. Unchanged files in later
                // cells reuse their existing publication instead of new PUTs.
                Ok(format!("hosted-code-artifact:{container_id}:{artifact_id}"))
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub file_id: String,
    pub filename: String,
    pub content_type: String,
    pub bytes: u64,
    pub sha256: String,
}

impl ArtifactRef {
    pub fn download_path(&self) -> String {
        format!("/v1/files/{}/content", self.file_id)
    }

    pub fn value(&self) -> Value {
        json!({
            "file_id":self.file_id,"filename":self.filename,
            "content_type":self.content_type,"bytes":self.bytes,
            "sha256":self.sha256,"download_url":self.download_path(),
        })
    }
}

fn failure() -> Error {
    Error::RustError("invalid_hosted_artifact".into())
}

pub fn artifact_key(execution_id: &str, ordinal: u16) -> Result<String> {
    if !component(execution_id, 512) || ordinal >= MAX_ARTIFACTS {
        return Err(failure());
    }
    // Content is deliberately absent: changing a known slot must conflict with
    // its original publication, not create another file after an uncertain reply.
    Ok(format!("hosted-artifact:{execution_id}:{ordinal}"))
}

fn component(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-:".contains(&byte))
}

pub async fn publish_bytes(
    env: &Env,
    owner: &str,
    identity: ArtifactIdentity<'_>,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<ArtifactRef> {
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(Error::RustError("hosted_artifact_too_large".into()));
    }
    let key = identity.key()?;
    let file =
        space::save_artifact_bytes_idempotent(env, owner, &key, filename, content_type, bytes)
            .await?;
    Ok(ArtifactRef {
        file_id: file.id,
        filename: file.filename,
        content_type: file.content_type,
        bytes: file.bytes,
        sha256: file.sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_execution_slot_has_one_stable_identity_independent_of_content() {
        assert_eq!(
            artifact_key("run_a:round:2:call:b", 1).unwrap(),
            "hosted-artifact:run_a:round:2:call:b:1",
        );
        assert_ne!(
            artifact_key("run_a:round:2:call:b", 1).unwrap(),
            artifact_key("run_a:round:2:call:b", 2).unwrap(),
        );
        for invalid in [
            "",
            "../other",
            "request?secret=x",
            "a\nheader",
            "space here",
        ] {
            assert!(artifact_key(invalid, 0).is_err());
        }
        assert!(artifact_key("known", MAX_ARTIFACTS).is_err());
    }

    #[test]
    fn public_artifact_reference_contains_no_storage_or_owner_identity() {
        let reference = ArtifactRef {
            file_id: "file-known".into(),
            filename: "plot-雪.png".into(),
            content_type: "image/png".into(),
            bytes: 123,
            sha256: "a".repeat(64),
        };
        assert_eq!(
            reference.value()["download_url"],
            "/v1/files/file-known/content"
        );
        let value = reference.value().to_string();
        for private in ["object_key", "account_id", "reservation_id", "R2", "token"] {
            assert!(!value.contains(private));
        }
    }

    #[test]
    fn immutable_code_versions_reuse_publication_across_execution_cells() {
        let key = || {
            ArtifactIdentity::InterpreterVersion {
                container_id: "cntr_known",
                artifact_id: "artifact_immutable_digest",
            }
            .key()
            .unwrap()
        };
        assert_eq!(key(), key());
        assert_ne!(
            key(),
            ArtifactIdentity::InterpreterVersion {
                container_id: "cntr_known",
                artifact_id: "artifact_changed_digest",
            }
            .key()
            .unwrap()
        );
    }
}
