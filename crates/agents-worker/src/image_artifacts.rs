//! Image bytes are immutable owner blobs, not deletable Files backing objects.
//! The caller durably stores the candidate and each I/O attempt before calling
//! this module. Provider execution and accounting remain in the single harness.

use agents_auth::billing::Resource;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::{Env, Error, HttpMetadata, Result};

use crate::{
    image_generation::{MAX_IMAGE_BYTES, Raster},
    resources::ResourceMeter,
    workspace_blobs::{self as blobs, BlobKind, OwnedBlobRef},
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ImageCandidate {
    pub blob: OwnedBlobRef,
    pub width: u32,
    pub height: u32,
}

impl ImageCandidate {
    /// Persist the returned identity before any R2 write. Retrying must reuse it.
    pub(crate) fn prepare(owner: &str, image: &Raster) -> Result<Self> {
        let hash = image.sha256();
        let owner_hash = Sha256::digest(owner.as_bytes());
        let (width, height) = image.dimensions();
        let candidate = Self {
            blob: OwnedBlobRef {
                account_id: owner.into(),
                object_key: format!(
                    "owner-blobs/{owner_hash:x}/image/{hash}/{}",
                    uuid::Uuid::new_v4()
                ),
                sha256: hash,
                bytes: image.len() as u64,
                content_type: image.format().mime().into(),
                kind: BlobKind::ImageArtifact,
            },
            width,
            height,
        };
        candidate.blob.validate(owner)?;
        Ok(candidate)
    }

    fn check(&self, owner: &str, image: &Raster, bytes: &[u8]) -> Result<()> {
        self.blob.validate(owner)?;
        if self.blob.bytes != bytes.len() as u64
            || self.blob.sha256 != format!("{:x}", Sha256::digest(bytes))
            || self.blob.content_type != image.format().mime()
            || (self.width, self.height) != image.dimensions()
        {
            return Err(fail("image_artifact_identity_conflict"));
        }
        Ok(())
    }
}

/// The caller advances and persists an attempt before resuming ambiguous I/O.
/// Reusing an event for a second actual PUT would incorrectly hide resource use.
pub(crate) struct IoAttempt<'a> {
    pub event: &'a str,
    pub attempt: u32,
}

impl IoAttempt<'_> {
    pub(crate) fn key(&self, operation: &str) -> Result<String> {
        let key = format!("{}:{}:{operation}", self.event, self.attempt);
        if self.event.is_empty() || key.len() > 256 || self.event.chars().any(char::is_control) {
            return Err(fail("invalid_image_artifact_event"));
        }
        Ok(key)
    }
}

/// Materialized bytes become readable only after checksum verification, clock
/// registration and registry promotion. A concurrent dedup winner is returned.
/// No provider request, Files link or response-state mutation is performed here.
pub(crate) async fn store(
    env: &Env,
    owner: &str,
    operation: &str,
    candidate: &ImageCandidate,
    image: &Raster,
    meter: &ResourceMeter<'_>,
    io: &IoAttempt<'_>,
) -> Result<OwnedBlobRef> {
    let head_event = io.key("head")?;
    let put_event = io.key("put")?;
    meter.require_tariff(&Resource::R2Operation, "class_a")?;
    meter.require_tariff(&Resource::R2Operation, "class_b")?;
    meter.require_tariff(&Resource::R2Storage, "byte_ms")?;
    let bytes = image.bytes();
    candidate.check(owner, image, &bytes)?;
    if let Some(ready) = blobs::find_image(
        env,
        owner,
        &candidate.blob.sha256,
        candidate.blob.bytes,
        &candidate.blob.content_type,
    )
    .await?
    {
        // The caller first binds the returned blob to its canonical item, then
        // releases this operation's capacity hold. No retention gap is implied.
        return Ok(ready);
    }
    blobs::begin_image_reserved(env, &candidate.blob, operation).await?;
    let bucket = env.bucket("WORKSPACES")?;
    meter
        .require_observation(&head_event, Resource::R2Operation, "class_b")
        .await?;
    let existing = bucket.head(&candidate.blob.object_key).await;
    meter
        .record_observation(&head_event, Resource::R2Operation, "class_b", 1)
        .await?;
    let object = if let Some(existing) = existing? {
        existing
    } else {
        meter
            .require_observation(&put_event, Resource::R2Operation, "class_a")
            .await?;
        let uploaded = bucket
            .put(&candidate.blob.object_key, bytes)
            .sha256(decode_hash(&candidate.blob.sha256)?)
            .http_metadata(HttpMetadata {
                content_type: Some(candidate.blob.content_type.clone()),
                ..Default::default()
            })
            .execute()
            .await;
        meter
            .record_observation(&put_event, Resource::R2Operation, "class_a", 1)
            .await?;
        uploaded?.ok_or_else(|| fail("image_artifact_upload_rejected"))?
    };
    // Even an unexpected physical object retains bytes until its checked GC.
    // Register the actual result before rejecting identity or promoting it.
    crate::reconcile::register_account_storage_object(
        env,
        owner,
        &candidate.blob.object_key,
        object.size(),
        object.uploaded().as_millis(),
    )
    .await?;
    if object.size() != candidate.blob.bytes
        || object.checksum().sha256.as_deref()
            != Some(decode_hash(&candidate.blob.sha256)?.as_slice())
    {
        return Err(fail("image_artifact_checksum_mismatch"));
    }
    blobs::complete_image(env, &candidate.blob).await
}

/// Recover a persisted candidate after a possibly successful PUT. The caller
/// advances and durably saves this I/O attempt before each actual HEAD.
pub(crate) async fn recover_candidate(
    env: &Env,
    owner: &str,
    operation: &str,
    candidate: &ImageCandidate,
    meter: &ResourceMeter<'_>,
    io: &IoAttempt<'_>,
) -> Result<Option<OwnedBlobRef>> {
    candidate.blob.validate(owner)?;
    if let Some(ready) = blobs::find_image(
        env,
        owner,
        &candidate.blob.sha256,
        candidate.blob.bytes,
        &candidate.blob.content_type,
    )
    .await?
    {
        return Ok(Some(ready));
    }
    let event = io.key("recover_head")?;
    meter.require_tariff(&Resource::R2Storage, "byte_ms")?;
    meter
        .require_observation(&event, Resource::R2Operation, "class_b")
        .await?;
    let headed = env
        .bucket("WORKSPACES")?
        .head(&candidate.blob.object_key)
        .await;
    meter
        .record_observation(&event, Resource::R2Operation, "class_b", 1)
        .await?;
    let Some(object) = headed? else {
        return Ok(None);
    };
    // Mismatched physical objects retain bytes too; register their actual clock.
    crate::reconcile::register_account_storage_object(
        env,
        owner,
        &candidate.blob.object_key,
        object.size(),
        object.uploaded().as_millis(),
    )
    .await?;
    if object.size() != candidate.blob.bytes
        || object.checksum().sha256.as_deref()
            != Some(decode_hash(&candidate.blob.sha256)?.as_slice())
    {
        return Err(fail("image_artifact_checksum_mismatch"));
    }
    blobs::begin_image_reserved(env, &candidate.blob, operation).await?;
    blobs::complete_image(env, &candidate.blob).await.map(Some)
}

/// Call only after authorizing the containing live Response or other strong
/// holder. Blob knowledge alone does not authorize a public download endpoint.
pub(crate) async fn read(
    env: &Env,
    owner: &str,
    blob: &OwnedBlobRef,
    meter: &ResourceMeter<'_>,
    io: &IoAttempt<'_>,
) -> Result<Raster> {
    blob.validate(owner)?;
    let event = io.key("get")?;
    if blobs::find_image_key(env, owner, &blob.object_key)
        .await?
        .as_ref()
        != Some(blob)
    {
        return Err(fail("image_artifact_not_ready"));
    }
    meter
        .require_observation(&event, Resource::R2Operation, "class_b")
        .await?;
    let object = env
        .bucket("WORKSPACES")?
        .get(&blob.object_key)
        .execute()
        .await;
    meter
        .record_observation(&event, Resource::R2Operation, "class_b", 1)
        .await?;
    let object = object?.ok_or_else(|| fail("image_artifact_missing"))?;
    // R2's authoritative object size is checked before allocating the body.
    if object.size() != blob.bytes || object.size() > MAX_IMAGE_BYTES as u64 {
        return Err(fail("image_artifact_size_mismatch"));
    }
    let bytes = object
        .body()
        .ok_or_else(|| fail("image_artifact_missing"))?
        .bytes()
        .await?;
    if bytes.len() as u64 != blob.bytes || format!("{:x}", Sha256::digest(&bytes)) != blob.sha256 {
        return Err(fail("image_artifact_checksum_mismatch"));
    }
    let image = Raster::from_bytes(&bytes)?;
    if image.format().mime() != blob.content_type {
        return Err(fail("image_artifact_format_mismatch"));
    }
    Ok(image)
}

fn decode_hash(hash: &str) -> Result<Vec<u8>> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(fail("invalid_image_artifact_hash"));
    }
    (0..32)
        .map(|index| {
            u8::from_str_radix(&hash[index * 2..index * 2 + 2], 16)
                .map_err(|_| fail("invalid_image_artifact_hash"))
        })
        .collect()
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn raster() -> Raster {
        let mut buffer = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .unwrap();
        Raster::from_bytes(buffer.get_ref()).unwrap()
    }

    #[test]
    fn candidate_is_owner_scoped_and_survives_durable_round_trip() {
        let image = raster();
        let candidate = ImageCandidate::prepare("account-a", &image).unwrap();
        let restored: ImageCandidate =
            serde_json::from_str(&serde_json::to_string(&candidate).unwrap()).unwrap();
        restored.check("account-a", &image, &image.bytes()).unwrap();
        assert!(restored.check("account-b", &image, &image.bytes()).is_err());
        assert_eq!(candidate.blob, restored.blob);
        assert_eq!((restored.width, restored.height), (2, 3));
    }

    #[test]
    fn resumed_candidate_cannot_change_bytes_or_declared_shape() {
        let image = raster();
        let mut candidate = ImageCandidate::prepare("account-a", &image).unwrap();
        assert!(candidate.check("account-a", &image, b"different").is_err());
        candidate.width += 1;
        assert!(
            candidate
                .check("account-a", &image, &image.bytes())
                .is_err()
        );
    }

    #[test]
    fn retries_have_distinct_resource_observation_identity() {
        let first = IoAttempt {
            event: "image:call_a",
            attempt: 1,
        };
        let next = IoAttempt {
            event: "image:call_a",
            attempt: 2,
        };
        assert_ne!(first.key("put").unwrap(), next.key("put").unwrap());
        assert_ne!(first.key("head").unwrap(), first.key("put").unwrap());
        assert!(
            IoAttempt {
                event: "",
                attempt: 0
            }
            .key("get")
            .is_err()
        );
        assert!(
            IoAttempt {
                event: "bad\n",
                attempt: 0
            }
            .key("get")
            .is_err()
        );
    }
}
