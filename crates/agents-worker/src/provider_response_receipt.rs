//! Private paid-response recovery bytes, never public Files or telemetry payloads.
//! Persist `StoredResponse::prepare` before I/O and advance `IoAttempt` durably
//! before retries. The caller owns admission, capacity and the recovery lifecycle.

use agents_auth::billing::Resource;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::{Conditional, Env, Error, HttpMetadata, Object, Result};

use crate::{image_artifacts::IoAttempt, resources::ResourceMeter};

pub(crate) const MAX_RESPONSE_BYTES: usize = 24 * 1024 * 1024;
const CHUNK_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_ENCRYPTED_RESPONSE_BYTES: usize =
    (MAX_RESPONSE_BYTES / CHUNK_BYTES) * ((CHUNK_BYTES + 29) * 4).div_ceil(3);
const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponseStage {
    BodyReceived,
}

/// Only structural, bounded metadata. Never attach arbitrary headers or errors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResponseMeta {
    pub status: u16,
    pub content_type: Option<String>,
    pub request_id: Option<String>,
    pub generation_id: Option<String>,
    pub duration_ms: u64,
    pub stage: ResponseStage,
}

impl ResponseMeta {
    fn validate(&self) -> Result<()> {
        if !(100..=599).contains(&self.status)
            || self.content_type.as_ref().is_some_and(|s| !bounded(s, 256))
            || self.request_id.as_ref().is_some_and(|s| !identifier(s))
            || self.generation_id.as_ref().is_some_and(|s| !identifier(s))
        {
            return Err(fail("invalid_provider_receipt_metadata"));
        }
        Ok(())
    }
}

/// An actor-private capability descriptor, not an authorization grant. Keys are
/// derived internally; deserialization never permits a caller-selected R2 key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredResponse {
    version: u8,
    owner_sha256: String,
    operation_sha256: String,
    sha256: String,
    bytes: u64,
    chunks: u32,
    meta: ResponseMeta,
}

impl StoredResponse {
    pub(crate) fn prepare(
        owner: &str,
        operation: &str,
        meta: &ResponseMeta,
        body: &[u8],
    ) -> Result<Self> {
        check_identity(owner, operation)?;
        meta.validate()?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(fail("provider_receipt_body_too_large"));
        }
        Ok(Self {
            version: VERSION,
            owner_sha256: hash(owner.as_bytes()),
            operation_sha256: hash(operation.as_bytes()),
            sha256: hash(body),
            bytes: body.len() as u64,
            chunks: chunk_count(body.len()),
            meta: meta.clone(),
        })
    }

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn meta(&self) -> &ResponseMeta {
        &self.meta
    }

    pub(crate) fn storage_bytes(&self) -> u64 {
        // Each AES-GCM envelope adds 29 bytes before unpadded base64 encoding.
        let full = (self.bytes as usize / CHUNK_BYTES) * sealed_len(CHUNK_BYTES);
        let remainder = self.bytes as usize % CHUNK_BYTES;
        (full
            + if remainder > 0 || self.bytes == 0 {
                sealed_len(remainder)
            } else {
                0
            }) as u64
    }

    fn validate(&self, owner: &str, operation: &str) -> Result<()> {
        check_identity(owner, operation)?;
        self.meta.validate()?;
        if self.version != VERSION
            || self.owner_sha256 != hash(owner.as_bytes())
            || self.operation_sha256 != hash(operation.as_bytes())
            || !valid_hash(&self.sha256)
            || self.bytes > MAX_RESPONSE_BYTES as u64
            || self.chunks != chunk_count(self.bytes as usize)
        {
            return Err(fail("provider_receipt_identity_mismatch"));
        }
        Ok(())
    }

    fn base(&self) -> Result<String> {
        let descriptor_hash = hash(&serde_json::to_vec(self)?);
        Ok(format!(
            "provider-response-receipts/v1/{}/{}/{}/{descriptor_hash}",
            self.owner_sha256, self.operation_sha256, self.sha256
        ))
    }

    fn chunk_len(&self, index: u32) -> Result<usize> {
        if index >= self.chunks {
            return Err(fail("provider_receipt_chunk_out_of_range"));
        }
        Ok((self.bytes as usize - index as usize * CHUNK_BYTES).min(CHUNK_BYTES))
    }

    fn aad(&self, index: u32) -> Result<String> {
        let len = self.chunk_len(index)?;
        // The descriptor binds version, owner, operation, metadata, total length,
        // body hash and count; the suffix binds position and exact chunk length.
        Ok(format!("{}:chunk:{index}:{len}", self.base()?))
    }
}

/// At most 24 bounded encrypted objects. Save the prepared pointer before this
/// call: a partial upload remains discoverable without repeating the paid POST.
/// A stable create-only CAS never overwrites existing bytes, even during races.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store(
    env: &Env,
    meter: &ResourceMeter<'_>,
    owner: &str,
    operation: &str,
    meta: &ResponseMeta,
    body: &[u8],
    io: &IoAttempt<'_>,
) -> Result<StoredResponse> {
    let receipt = StoredResponse::prepare(owner, operation, meta, body)?;
    store_prepared(env, meter, owner, operation, &receipt, body, io).await?;
    Ok(receipt)
}

/// Use the original persisted descriptor; later observations must not change
/// its metadata/AAD, even when parsing reveals another provider identifier.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store_prepared(
    env: &Env,
    meter: &ResourceMeter<'_>,
    owner: &str,
    operation: &str,
    receipt: &StoredResponse,
    body: &[u8],
    io: &IoAttempt<'_>,
) -> Result<()> {
    receipt.validate(owner, operation)?;
    check_body(receipt, body)?;
    meter.require_tariff(&Resource::R2Storage, "byte_ms")?;
    meter.require_tariff(&Resource::R2Operation, "class_a")?;
    meter.require_tariff(&Resource::R2Operation, "class_b")?;
    let key = env.secret("DATA_ENCRYPTION_KEY")?.to_string();
    let base = receipt.base()?;
    for index in 0..receipt.chunks {
        store_chunk(env, meter, owner, receipt, &key, &base, body, index, io).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn store_chunk(
    env: &Env,
    meter: &ResourceMeter<'_>,
    owner: &str,
    receipt: &StoredResponse,
    encryption_key: &str,
    base: &str,
    body: &[u8],
    index: u32,
    io: &IoAttempt<'_>,
) -> Result<()> {
    let object_key = format!("{base}/{index:02}");
    let event = io.key(&format!("receipt_get_{index}"))?;
    let bucket = env.bucket("WORKSPACES")?;
    let start = index as usize * CHUNK_BYTES;
    let expected = &body[start..start + receipt.chunk_len(index)?];
    if let Some(object) = get(env, meter, &object_key, &event, index).await? {
        return check_existing(
            env,
            owner,
            &object_key,
            receipt,
            encryption_key,
            index,
            object,
            expected,
        )
        .await;
    }
    let ciphertext = agents_auth::seal(encryption_key, &receipt.aad(index)?, expected)
        .map_err(|_| fail("provider_receipt_encryption_failed"))?;
    let checksum = Sha256::digest(ciphertext.as_bytes()).to_vec();
    let event = io.key(&format!("receipt_put_{index}"))?;
    meter
        .require_observation(&event, Resource::R2Operation, "class_a")
        .await?;
    let uploaded = bucket
        .put(&object_key, ciphertext.into_bytes())
        .only_if(Conditional {
            etag_does_not_match: Some("*".into()),
            ..Default::default()
        })
        .sha256(checksum.clone())
        .http_metadata(HttpMetadata {
            content_type: Some("application/octet-stream".into()),
            ..Default::default()
        })
        .execute()
        .await;
    meter
        .record_observation(&event, Resource::R2Operation, "class_a", 1)
        .await?;
    let Some(object) = uploaded.map_err(|_| chunk_error("put_failed", index))? else {
        // A concurrent writer won. Its nonce may differ, so compare authenticated
        // plaintext, not this attempt's nondeterministic ciphertext checksum.
        let event = io.key(&format!("receipt_race_get_{index}"))?;
        let object = get(env, meter, &object_key, &event, index)
            .await?
            .ok_or_else(|| chunk_error("race_missing", index))?;
        return check_existing(
            env,
            owner,
            &object_key,
            receipt,
            encryption_key,
            index,
            object,
            expected,
        )
        .await;
    };
    register(env, owner, &object_key, &object).await?;
    if object.size() != sealed_len(expected.len()) as u64
        || object.checksum().sha256.as_deref() != Some(checksum.as_slice())
    {
        return Err(chunk_error("put_integrity_mismatch", index));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn check_existing(
    env: &Env,
    owner: &str,
    key: &str,
    receipt: &StoredResponse,
    encryption_key: &str,
    index: u32,
    object: Object,
    expected: &[u8],
) -> Result<()> {
    register(env, owner, key, &object).await?;
    let actual = decrypt_chunk(receipt, encryption_key, index, object).await?;
    if actual != expected {
        return Err(chunk_error("replay_mismatch", index));
    }
    Ok(())
}

/// Authorization must come from the owning actor/operator, not this pointer.
/// Return bytes only after every chunk authenticates and the whole hash agrees.
pub(crate) async fn read(
    env: &Env,
    meter: &ResourceMeter<'_>,
    owner: &str,
    operation: &str,
    receipt: &StoredResponse,
    io: &IoAttempt<'_>,
) -> Result<Vec<u8>> {
    receipt.validate(owner, operation)?;
    meter.require_tariff(&Resource::R2Storage, "byte_ms")?;
    let encryption_key = env.secret("DATA_ENCRYPTION_KEY")?.to_string();
    let base = receipt.base()?;
    let mut bytes = Vec::with_capacity(receipt.bytes as usize);
    for index in 0..receipt.chunks {
        let event = io.key(&format!("receipt_read_{index}"))?;
        let key = format!("{base}/{index:02}");
        let object = get(env, meter, &key, &event, index)
            .await?
            .ok_or_else(|| chunk_error("missing", index))?;
        // A crash may have preceded the original PUT's storage registration.
        register(env, owner, &key, &object).await?;
        bytes.extend_from_slice(&decrypt_chunk(receipt, &encryption_key, index, object).await?);
    }
    check_body(receipt, &bytes)?;
    Ok(bytes)
}

/// Explicit terminal cleanup only, after canonical publication or authorized
/// abandonment. No TTL deletes the only copy of an unpublished paid result.
/// The caller fences publication/re-store and funds this bounded I/O separately.
pub(crate) async fn delete(
    env: &Env,
    meter: &ResourceMeter<'_>,
    owner: &str,
    operation: &str,
    receipt: &StoredResponse,
    io: &IoAttempt<'_>,
) -> Result<()> {
    receipt.validate(owner, operation)?;
    let base = receipt.base()?;
    let bucket = env.bucket("WORKSPACES")?;
    for index in 0..receipt.chunks {
        let key = format!("{base}/{index:02}");
        let event = io.key(&format!("receipt_delete_head_{index}"))?;
        meter
            .require_observation(&event, Resource::R2Operation, "class_b")
            .await?;
        let object = bucket.head(&key).await;
        meter
            .record_observation(&event, Resource::R2Operation, "class_b", 1)
            .await?;
        if let Some(object) = object.map_err(|_| chunk_error("delete_head_failed", index))? {
            register(env, owner, &key, &object).await?;
        }
        // R2 DELETE itself is free; HEAD and retained storage are not.
        bucket
            .delete(&key)
            .await
            .map_err(|_| chunk_error("delete_failed", index))?;
        crate::reconcile::remove_storage_object(env, &key, crate::util::now_ms()).await?;
    }
    Ok(())
}

async fn get(
    env: &Env,
    meter: &ResourceMeter<'_>,
    key: &str,
    event: &str,
    index: u32,
) -> Result<Option<Object>> {
    meter
        .require_observation(event, Resource::R2Operation, "class_b")
        .await?;
    let object = env.bucket("WORKSPACES")?.get(key).execute().await;
    meter
        .record_observation(event, Resource::R2Operation, "class_b", 1)
        .await?;
    object.map_err(|_| chunk_error("get_failed", index))
}

async fn register(env: &Env, owner: &str, key: &str, object: &Object) -> Result<()> {
    crate::reconcile::register_account_storage_object(
        env,
        owner,
        key,
        object.size(),
        object.uploaded().as_millis(),
    )
    .await
}

async fn decrypt_chunk(
    receipt: &StoredResponse,
    key: &str,
    index: u32,
    object: Object,
) -> Result<Vec<u8>> {
    let expected_len = receipt.chunk_len(index)?;
    if object.size() != sealed_len(expected_len) as u64 {
        return Err(chunk_error("encoded_size_mismatch", index));
    }
    let ciphertext = object
        .body()
        .ok_or_else(|| chunk_error("body_missing", index))?
        .text()
        .await
        .map_err(|_| chunk_error("body_read_failed", index))?;
    if ciphertext.len() != sealed_len(expected_len) {
        return Err(chunk_error("encoded_size_mismatch", index));
    }
    open_chunk(receipt, key, index, &ciphertext)
}

fn open_chunk(
    receipt: &StoredResponse,
    key: &str,
    index: u32,
    ciphertext: &str,
) -> Result<Vec<u8>> {
    let bytes = agents_auth::unseal(key, &receipt.aad(index)?, ciphertext)
        .map_err(|_| chunk_error("authentication_failed", index))?;
    if bytes.len() != receipt.chunk_len(index)? {
        return Err(chunk_error("decoded_size_mismatch", index));
    }
    Ok(bytes)
}

fn check_identity(owner: &str, operation: &str) -> Result<()> {
    if !bounded(owner, 512) || !bounded(operation, 512) {
        return Err(fail("invalid_provider_receipt_identity"));
    }
    Ok(())
}

fn check_body(receipt: &StoredResponse, body: &[u8]) -> Result<()> {
    if body.len() as u64 != receipt.bytes || hash(body) != receipt.sha256 {
        return Err(fail("provider_receipt_body_integrity_mismatch"));
    }
    Ok(())
}

fn bounded(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

fn identifier(value: &str) -> bool {
    bounded(value, 256)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn chunk_count(bytes: usize) -> u32 {
    // Even an empty response gets an authenticated object proving receipt.
    bytes.div_ceil(CHUNK_BYTES).max(1) as u32
}

fn sealed_len(bytes: usize) -> usize {
    ((bytes + 29) * 4).div_ceil(3)
}

fn chunk_error(stage: &str, index: u32) -> Error {
    fail(&format!("provider_receipt_{stage}:chunk={index}"))
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn meta() -> ResponseMeta {
        ResponseMeta {
            status: 200,
            content_type: Some("application/json".into()),
            request_id: Some("request-1".into()),
            generation_id: None,
            duration_ms: 123,
            stage: ResponseStage::BodyReceived,
        }
    }

    fn receipt(body: &[u8]) -> StoredResponse {
        StoredResponse::prepare("owner-a", "image-operation-1", &meta(), body).unwrap()
    }

    #[test]
    fn descriptor_survives_actor_roundtrip_without_body_or_keys() {
        let original = receipt(b"paid-result-private-body");
        let json = serde_json::to_string(&original).unwrap();
        assert!(!json.contains("paid-result-private-body"));
        assert!(!json.contains("owner-a"));
        assert!(!json.contains("image-operation-1"));
        let restored: StoredResponse = serde_json::from_str(&json).unwrap();
        restored.validate("owner-a", "image-operation-1").unwrap();
        assert_eq!(restored, original);
        assert_eq!(restored.base().unwrap(), original.base().unwrap());
        assert!(restored.validate("owner-b", "image-operation-1").is_err());
        assert!(restored.validate("owner-a", "image-operation-2").is_err());
        assert!(
            serde_json::from_str::<StoredResponse>(
                &json.replace("{", "{\"object_key\":\"arbitrary\", ")
            )
            .is_err()
        );
        let mut changed = serde_json::to_value(meta()).unwrap();
        changed["authorization"] = serde_json::json!("never-allowed");
        assert!(serde_json::from_value::<ResponseMeta>(changed).is_err());
    }

    #[test]
    fn chunk_layout_is_bounded_and_empty_receipt_is_authenticated() {
        for size in [
            0,
            1,
            CHUNK_BYTES - 1,
            CHUNK_BYTES,
            CHUNK_BYTES + 1,
            MAX_RESPONSE_BYTES,
        ] {
            let mut descriptor = receipt(b"");
            descriptor.bytes = size as u64;
            descriptor.chunks = chunk_count(size);
            descriptor.validate("owner-a", "image-operation-1").unwrap();
            assert!(descriptor.chunks <= 24);
            let total: usize = (0..descriptor.chunks)
                .map(|i| descriptor.chunk_len(i).unwrap())
                .sum();
            assert_eq!(total, size);
            let stored: usize = (0..descriptor.chunks)
                .map(|i| sealed_len(descriptor.chunk_len(i).unwrap()))
                .sum();
            assert_eq!(descriptor.storage_bytes(), stored as u64);
            assert!(stored <= MAX_ENCRYPTED_RESPONSE_BYTES);
            assert!(descriptor.chunk_len(descriptor.chunks).is_err());
        }
        let empty = receipt(b"");
        let ciphertext = agents_auth::seal(KEY, &empty.aad(0).unwrap(), b"").unwrap();
        assert_eq!(ciphertext.len(), sealed_len(0));
        assert!(open_chunk(&empty, KEY, 0, &ciphertext).unwrap().is_empty());
        let mut oversized = empty;
        oversized.bytes = MAX_RESPONSE_BYTES as u64 + 1;
        oversized.chunks = 25;
        assert!(oversized.validate("owner-a", "image-operation-1").is_err());
    }

    #[test]
    fn encryption_binds_owner_operation_metadata_and_chunk_order() {
        let body = vec![7; CHUNK_BYTES * 2];
        let original = receipt(&body);
        let ciphertext =
            agents_auth::seal(KEY, &original.aad(0).unwrap(), &body[..CHUNK_BYTES]).unwrap();
        assert_eq!(ciphertext.len(), sealed_len(CHUNK_BYTES));
        assert!(open_chunk(&original, KEY, 1, &ciphertext).is_err());
        assert!(open_chunk(&original, KEY, 2, &ciphertext).is_err());
        let mut wrong_owner = original.clone();
        wrong_owner.owner_sha256 = hash(b"owner-b");
        assert!(open_chunk(&wrong_owner, KEY, 0, &ciphertext).is_err());
        let mut wrong_operation = original.clone();
        wrong_operation.operation_sha256 = hash(b"other-operation");
        assert!(open_chunk(&wrong_operation, KEY, 0, &ciphertext).is_err());
        let mut wrong_meta = original.clone();
        wrong_meta.meta.duration_ms += 1;
        assert!(open_chunk(&wrong_meta, KEY, 0, &ciphertext).is_err());
        let mut wrong_hash = original.clone();
        wrong_hash.sha256 = hash(b"different-body");
        assert!(open_chunk(&wrong_hash, KEY, 0, &ciphertext).is_err());
        let mut wrong_count = original;
        wrong_count.chunks += 1;
        assert!(open_chunk(&wrong_count, KEY, 0, &ciphertext).is_err());
    }

    #[test]
    fn receipt_body_identity_and_ciphertext_tampering_fail_closed() {
        let original = receipt(b"complete-paid-result");
        assert!(check_body(&original, b"complete-paid-result").is_ok());
        assert!(check_body(&original, b"complete-paid-resul").is_err());
        assert!(check_body(&original, b"complete-paid-resulX").is_err());
        let encrypted =
            agents_auth::seal(KEY, &original.aad(0).unwrap(), b"complete-paid-result").unwrap();
        for cut in 0..encrypted.len() {
            assert!(open_chunk(&original, KEY, 0, &encrypted[..cut]).is_err());
        }
        let mut changed = encrypted.into_bytes();
        changed[20] = if changed[20] == b'A' { b'B' } else { b'A' };
        assert!(open_chunk(&original, KEY, 0, std::str::from_utf8(&changed).unwrap()).is_err());
    }

    #[test]
    fn identity_metadata_and_untrusted_descriptors_are_bounded() {
        assert!(StoredResponse::prepare("", "operation", &meta(), b"body").is_err());
        assert!(StoredResponse::prepare("owner", "op\n", &meta(), b"body").is_err());
        for status in [0, 99, 600, u16::MAX] {
            let mut invalid = meta();
            invalid.status = status;
            assert!(StoredResponse::prepare("owner", "op", &invalid, b"body").is_err());
        }
        let mut invalid = meta();
        invalid.request_id = Some("request\r\nAuthorization: secret".into());
        assert!(invalid.validate().is_err());
        invalid.request_id = Some("a".repeat(257));
        assert!(invalid.validate().is_err());
        let mut invalid = receipt(b"body");
        invalid.sha256 = "é".repeat(32);
        assert!(invalid.validate("owner-a", "image-operation-1").is_err());
        invalid.sha256 = hash(b"body");
        invalid.chunks = 100;
        assert!(invalid.validate("owner-a", "image-operation-1").is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn arbitrary_paid_bytes_have_stable_identity_and_authenticated_roundtrip(body in prop::collection::vec(any::<u8>(), 0..8192)) {
            let original = receipt(&body);
            prop_assert_eq!(original.base().unwrap(), receipt(&body).base().unwrap());
            let ciphertext = agents_auth::seal(KEY, &original.aad(0).unwrap(), &body).unwrap();
            prop_assert_eq!(ciphertext.len(), sealed_len(body.len()));
            let restored = open_chunk(&original, KEY, 0, &ciphertext).unwrap();
            prop_assert_eq!(&restored, &body);
            prop_assert!(check_body(&original, &restored).is_ok());
            let other = StoredResponse::prepare("owner-a", "other-operation", &meta(), &body).unwrap();
            prop_assert_ne!(original.base().unwrap(), other.base().unwrap());
            prop_assert!(open_chunk(&other, KEY, 0, &ciphertext).is_err());
        }
    }
}
