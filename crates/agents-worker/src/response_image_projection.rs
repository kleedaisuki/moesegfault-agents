//! Canonical Responses retain image identities, never duplicated base64 bodies.
//! Only an already-authorized response observation may hydrate these references.

use agents_auth::billing::Resource;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use worker::{Env, Error, Result};

use crate::{
    image_artifacts::{self, IoAttempt},
    resources::{ResourceMeter, ResourcePricing},
    workspace_blobs::OwnedBlobRef,
};

pub(crate) const MAX_ASSETS: usize = 128;
const MAX_PUBLIC_BYTES: usize = crate::observer_transport::MAX_BYTES;
const NON_IMAGE_ALLOWANCE: usize = 4 * 1024 * 1024;
pub(crate) const ORIGINAL_IMAGE_READ_TIMEOUT_MS: u64 = 30_000;
pub(crate) const ORIGINAL_IMAGE_READS_PER_SLOT: u64 = 8;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedImage {
    item_id: String,
    partial_index: Option<u8>,
    blob: OwnedBlobRef,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ResponseImages(Vec<RetainedImage>);

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ImageReadScope {
    Account,
    Session { session_id: String },
}

/// Constructed by the trusted lifecycle controller after verifying the original
/// hold and its pinned tariff. This is never accepted from public request JSON.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageReadFunding {
    pub hold_id: String,
    pub pricing: ResourcePricing,
    pub event_prefix: String,
    pub scope: ImageReadScope,
    pub max_reads: u16,
}

pub(crate) type ImageReadFundingMap = BTreeMap<String, ImageReadFunding>;

pub(crate) enum ResponseFrame {
    Encoded(Vec<u8>),
    Close(crate::session::image_observation::ImageObservationCallback),
    Image {
        event: Value,
        images: ResponseImages,
        funding: Option<ImageReadFundingMap>,
    },
}

impl ResponseFrame {
    pub(crate) fn with_funding_snapshot(self, funding: Option<&ImageReadFundingMap>) -> Self {
        match (self, funding) {
            (frame @ Self::Image { .. }, Some(funding)) => frame.with_funding(funding.clone()),
            (frame, _) => frame,
        }
    }
    pub(crate) fn with_funding(self, funding: ImageReadFundingMap) -> Self {
        match self {
            Self::Image { event, images, .. } => Self::Image {
                event,
                images,
                funding: Some(funding),
            },
            encoded => encoded,
        }
    }

    pub(crate) async fn bytes(self, env: &Env, owner: &str) -> Result<Vec<u8>> {
        match self {
            Self::Encoded(bytes) => Ok(bytes),
            Self::Close(callback) => {
                callback.close(env).await?;
                Ok(Vec::new())
            }
            Self::Image {
                mut event,
                images,
                funding,
            } => {
                images
                    .hydrate_with_funding(env, owner, &mut event, funding.as_ref())
                    .await?;
                Ok(crate::responses_wire::event_frame(&event))
            }
        }
    }
}

impl ResponseImages {
    pub(crate) fn assets_for_item(
        &self,
        item: &str,
    ) -> Vec<crate::workspace_blobs::ImageExecutionAsset> {
        self.0
            .iter()
            .filter(|asset| asset.item_id == item)
            .map(|asset| crate::workspace_blobs::ImageExecutionAsset {
                artifact_id: match asset.partial_index {
                    Some(index) => format!("{item}:partial:{index}"),
                    None => format!("{item}:result"),
                },
                item_id: asset.item_id.clone(),
                partial_index: asset.partial_index,
                blob: asset.blob.clone(),
            })
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn insert_final(
        &mut self,
        owner: &str,
        item_id: &str,
        blob: OwnedBlobRef,
    ) -> Result<()> {
        self.insert(owner, item_id, None, blob)
    }

    pub(crate) fn insert_partial(
        &mut self,
        owner: &str,
        item_id: &str,
        index: u8,
        blob: OwnedBlobRef,
    ) -> Result<()> {
        if index > 2 {
            return Err(fail("invalid_image_partial_index"));
        }
        self.insert(owner, item_id, Some(index), blob)
    }

    fn insert(
        &mut self,
        owner: &str,
        item_id: &str,
        partial_index: Option<u8>,
        blob: OwnedBlobRef,
    ) -> Result<()> {
        blob.validate(owner)?;
        if !identifier(item_id) {
            return Err(fail("invalid_image_item_id"));
        }
        if let Some(existing) = self.find(item_id, partial_index) {
            return if existing == &blob {
                Ok(())
            } else {
                Err(fail("image_projection_identity_conflict"))
            };
        }
        if self.0.len() >= MAX_ASSETS {
            return Err(fail("image_projection_limit"));
        }
        self.0.push(RetainedImage {
            item_id: item_id.into(),
            partial_index,
            blob,
        });
        Ok(())
    }

    fn find(&self, item_id: &str, partial: Option<u8>) -> Option<&OwnedBlobRef> {
        self.0
            .iter()
            .find(|image| image.item_id == item_id && image.partial_index == partial)
            .map(|image| &image.blob)
    }

    /// Check the worst case before another paid generation, not after its bytes
    /// exist. Previews occupy individual events, not the terminal output array.
    pub(crate) fn admit_final(&self, max_raw_bytes: usize) -> Result<()> {
        self.admit_final_with_metadata(max_raw_bytes, NON_IMAGE_ALLOWANCE)
    }

    pub(crate) fn admit_final_with_metadata(
        &self,
        max_raw_bytes: usize,
        non_image_bytes: usize,
    ) -> Result<()> {
        let used = self
            .0
            .iter()
            .filter(|image| image.partial_index.is_none())
            .try_fold(encoded_len(max_raw_bytes)?, |sum, image| {
                sum.checked_add(encoded_len(image.blob.bytes as usize)?)
                    .ok_or_else(|| fail("image_projection_limit"))
            })?;
        if used > MAX_PUBLIC_BYTES.saturating_sub(non_image_bytes) {
            return Err(fail("image_response_capacity_exceeded"));
        }
        Ok(())
    }

    pub(crate) fn needs_hydration(&self, value: &Value) -> bool {
        let mut locations = Vec::new();
        locate(value, "", &mut locations);
        !locations.is_empty()
    }

    pub(crate) async fn hydrate(&self, env: &Env, owner: &str, value: &mut Value) -> Result<()> {
        self.hydrate_with_funding(env, owner, value, None).await
    }

    pub(crate) async fn hydrate_with_funding(
        &self,
        env: &Env,
        owner: &str,
        value: &mut Value,
        funding: Option<&ImageReadFundingMap>,
    ) -> Result<()> {
        let mut locations = Vec::new();
        locate(value, "", &mut locations);
        if locations.is_empty() {
            return Ok(());
        }
        let mut bytes = serde_json::to_vec(value)?.len();
        for location in &locations {
            if funding.is_some_and(|map| !map.contains_key(&location.item_id)) {
                return Err(fail("image_delivery_funding_missing"));
            }
            let blob = self
                .find(&location.item_id, location.partial_index)
                .ok_or_else(|| fail("image_result_not_retained"))?;
            blob.validate(owner)?;
            bytes = bytes
                .checked_add(encoded_len(blob.bytes as usize)?)
                .ok_or_else(|| fail("image_projection_limit"))?;
        }
        if bytes > MAX_PUBLIC_BYTES {
            return Err(fail("image_projection_limit"));
        }
        // No cache of hydrated values: it would turn a reference-only replay
        // journal into a second long-lived copy of every image body.
        for location in locations {
            let blob = self
                .find(&location.item_id, location.partial_index)
                .unwrap();
            let prepaid = funding
                .map(|map| {
                    map.get(&location.item_id)
                        .ok_or_else(|| fail("image_delivery_funding_missing"))
                })
                .transpose()?;
            let encoded = read_encoded(env, owner, blob, prepaid)
                .await
                .map_err(observation_error)?;
            *value
                .pointer_mut(&location.pointer)
                .ok_or_else(|| fail("invalid_image_projection"))? = Value::String(encoded);
        }
        Ok(())
    }
}

struct Location {
    item_id: String,
    partial_index: Option<u8>,
    pointer: String,
}

fn locate(value: &Value, prefix: &str, found: &mut Vec<Location>) {
    if value["type"] == "response.image_generation_call.partial_image" {
        if let (Some(item), Some(index)) = (
            value["item_id"].as_str(),
            value["partial_image_index"].as_u64(),
        ) && value["partial_image_b64"].is_null()
            && let Ok(index) = u8::try_from(index)
        {
            found.push(Location {
                item_id: item.into(),
                partial_index: Some(index),
                pointer: format!("{prefix}/partial_image_b64"),
            });
        }
    }
    if value["type"] == "image_generation_call"
        && value["status"] == "completed"
        && value["result"].is_null()
        && let Some(item) = value["id"].as_str()
    {
        found.push(Location {
            item_id: item.into(),
            partial_index: None,
            pointer: format!("{prefix}/result"),
        });
    }
    // Wire envelopes carry references. Arbitrary client tool JSON is data,
    // even when an array happens to resemble an image-generation item.
    if value["type"]
        .as_str()
        .is_some_and(|kind| kind.starts_with("response."))
    {
        for field in ["response", "item"] {
            if let Some(child) = value.get(field) {
                locate(child, &format!("{prefix}/{field}"), found);
            }
        }
    }
    let array = match value["object"].as_str() {
        Some("response") => Some("output"),
        Some("list") => Some("data"),
        _ => None,
    };
    if let Some(field) = array
        && let Some(output) = value.get(field).and_then(Value::as_array)
    {
        for (index, item) in output.iter().enumerate() {
            locate(item, &format!("{prefix}/{field}/{index}"), found);
        }
    }
}

async fn read_encoded(
    env: &Env,
    owner: &str,
    blob: &OwnedBlobRef,
    funding: Option<&ImageReadFunding>,
) -> Result<String> {
    if let Some(funding) = funding {
        return read_prepaid(env, owner, blob, funding).await;
    }
    let id = format!("image-observation:{}", uuid::Uuid::new_v4());
    crate::reconcile::maintenance_account_reservation(env, owner, &id).await?;
    let meter = ResourceMeter::account(env, owner, &id)?;
    let result = image_artifacts::read(
        env,
        owner,
        blob,
        &meter,
        &IoAttempt {
            event: "image",
            attempt: 0,
        },
    )
    .await;
    meter.settle_run().await?;
    result.map(|image| image.into_encoded())
}

async fn read_prepaid(
    env: &Env,
    owner: &str,
    blob: &OwnedBlobRef,
    funding: &ImageReadFunding,
) -> Result<String> {
    if funding.hold_id.is_empty()
        || funding.event_prefix.is_empty()
        || funding.event_prefix.len() > 128
        || !funding
            .event_prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-:.".contains(&byte))
    {
        return Err(fail("invalid_image_delivery_funding"));
    }
    let meter = match &funding.scope {
        ImageReadScope::Account => ResourceMeter::with_account_pricing(
            env,
            owner,
            &funding.hold_id,
            funding.pricing.clone(),
        )?,
        ImageReadScope::Session { session_id } => ResourceMeter::with_pricing(
            env,
            owner,
            session_id,
            &funding.hold_id,
            funding.pricing.clone(),
        )?,
    };
    // Each physical GET has its own durable observation, including a retry of
    // the same public event. The lifecycle owner decides when this hold drains.
    let event = format!("{}:{}", funding.event_prefix, uuid::Uuid::new_v4().simple());
    let io = IoAttempt {
        event: &event,
        attempt: 0,
    };
    if matches!(funding.scope, ImageReadScope::Account) {
        claim_original_read(env, owner, funding, &io.key("get")?).await?;
    }
    let wall_event = format!("{event}:do");
    meter
        .require_observation(&wall_event, Resource::DurableObject, "wall_ms")
        .await?;
    let started = crate::util::now_ms();
    let read = image_artifacts::read(env, owner, blob, &meter, &io);
    let result = if matches!(funding.scope, ImageReadScope::Account) {
        match futures::future::select(
            Box::pin(read),
            Box::pin(worker::Delay::from(std::time::Duration::from_millis(
                ORIGINAL_IMAGE_READ_TIMEOUT_MS,
            ))),
        )
        .await
        {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(_) => Err(fail("image_original_read_timeout")),
        }
    } else {
        read.await
    };
    meter
        .record_wall_time(
            &wall_event,
            Resource::DurableObject,
            started,
            crate::util::now_ms(),
        )
        .await?;
    result.map(|image| image.into_encoded())
}

async fn claim_original_read(
    env: &Env,
    owner: &str,
    funding: &ImageReadFunding,
    event: &str,
) -> Result<()> {
    if funding.max_reads == 0
        || usize::from(funding.max_reads) > MAX_ASSETS * ORIGINAL_IMAGE_READS_PER_SLOT as usize
    {
        return Err(fail("invalid_image_delivery_funding"));
    }
    let tariff = funding.pricing.tariff(&Resource::R2Operation, "class_b")?;
    #[derive(Deserialize)]
    struct Claim {
        event_key: String,
    }
    let claimed: Option<Claim> = env.d1("DB")?.prepare(
        "INSERT INTO resource_observations(run_id,account_id,session_id,event_key,resource,unit,quantity,price_version,observed_at,status) SELECT ?,?,NULL,?,'r2_operation','class_b',NULL,?,NULL,'pending' WHERE EXISTS(SELECT 1 FROM usage_reservations WHERE id=? AND account_id=? AND session_id IS NULL AND state='reserved') AND (SELECT COUNT(*) FROM resource_observations WHERE run_id=? AND account_id=? AND session_id IS NULL AND resource='r2_operation' AND unit='class_b')<CAST(? AS INTEGER) RETURNING event_key"
    ).bind(&[funding.hold_id.as_str().into(),owner.into(),event.into(),tariff.price_version.as_str().into(),funding.hold_id.as_str().into(),owner.into(),funding.hold_id.as_str().into(),owner.into(),funding.max_reads.to_string().into()])?.first(None).await?;
    if claimed.is_none_or(|claim| claim.event_key != event) {
        return Err(fail("image_original_read_limit"));
    }
    Ok(())
}

fn encoded_len(raw: usize) -> Result<usize> {
    raw.div_ceil(3)
        .checked_mul(4)
        .ok_or_else(|| fail("image_projection_limit"))
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

fn observation_error(error: Error) -> Error {
    let text = match &error {
        Error::RustError(code) => code.clone(),
        _ => error.to_string(),
    };
    if crate::routes::classify_error(&text).1 == "insufficient_credits" {
        return fail("budget_exceeded");
    }
    match error {
        Error::RustError(code)
            if matches!(
                code.as_str(),
                "insufficient_credits"
                    | "resource_reservation_required"
                    | "image_original_read_limit"
                    | "image_original_read_timeout"
            ) =>
        {
            Error::RustError(code)
        }
        _ => fail("image_projection_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_blobs::BlobKind;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    fn blob(owner: &str, byte: char) -> OwnedBlobRef {
        let digest = byte.to_string().repeat(64);
        OwnedBlobRef {
            account_id: owner.into(),
            object_key: format!(
                "owner-blobs/{:x}/image/{digest}/00000000-0000-4000-8000-000000000000",
                Sha256::digest(owner.as_bytes())
            ),
            sha256: digest,
            bytes: 16 * 1024 * 1024,
            content_type: "image/png".into(),
            kind: BlobKind::ImageArtifact,
        }
    }

    #[test]
    fn canonical_assets_are_reference_only_and_each_preview_has_its_own_identity() {
        let mut images = ResponseImages::default();
        images
            .insert_partial("owner", "ig_known", 0, blob("owner", 'a'))
            .unwrap();
        images
            .insert_partial("owner", "ig_known", 1, blob("owner", 'b'))
            .unwrap();
        images
            .insert_final("owner", "ig_known", blob("owner", 'c'))
            .unwrap();
        let encoded = serde_json::to_vec(&images).unwrap();
        assert!(encoded.len() < 2048);
        assert_ne!(
            images.find("ig_known", Some(0)),
            images.find("ig_known", None)
        );
        assert!(
            images
                .insert_final("owner", "ig_known", blob("owner", 'a'))
                .is_err()
        );
        assert!(
            images
                .insert_final("different", "ig_new", blob("owner", 'a'))
                .is_err()
        );
    }

    #[test]
    fn completed_and_partial_locations_do_not_hydrate_in_progress_or_unrelated_data() {
        let images = ResponseImages::default();
        let final_item = json!({"id":"ig_known","type":"image_generation_call","status":"completed","result":null});
        assert!(images.needs_hydration(&json!({"type":"response.completed","response":{"object":"response","output":[final_item]}})));
        assert!(images.needs_hydration(&json!({"object":"list","data":[final_item]})));
        assert!(
            !images
                .needs_hydration(&json!({"type":"custom_tool_call_output","output":[final_item]}))
        );
        assert!(images.needs_hydration(&json!({"type":"response.image_generation_call.partial_image","item_id":"ig_known","partial_image_index":1,"partial_image_b64":null})));
        assert!(!images.needs_hydration(&json!({"item":{"id":"ig_known","type":"image_generation_call","status":"in_progress","result":null}})));
        assert!(!images.needs_hydration(
            &json!({"type":"message","content":[{"type":"output_text","text":"result:null"}]})
        ));
    }

    #[test]
    fn paid_generation_admission_accounts_for_base64_expansion_before_dispatch() {
        let mut images = ResponseImages::default();
        images.admit_final(16 * 1024 * 1024).unwrap();
        images
            .insert_final("owner", "ig_known", blob("owner", 'a'))
            .unwrap();
        assert!(images.admit_final(16 * 1024 * 1024).is_err());
        assert_eq!(encoded_len(16 * 1024 * 1024).unwrap(), 22_369_624);
    }

    #[test]
    fn admission_includes_the_actual_request_metadata_instead_of_a_fixed_allowance() {
        let images = ResponseImages::default();
        assert!(
            images
                .admit_final_with_metadata(16 * 1024 * 1024, 2 * 1024 * 1024)
                .is_ok()
        );
        assert!(
            images
                .admit_final_with_metadata(16 * 1024 * 1024, 12 * 1024 * 1024)
                .is_err()
        );
    }

    #[test]
    fn wrapped_credit_denials_stay_actionable_without_backend_error_disclosure() {
        for text in [
            "budget_exceeded",
            "D1_ERROR: budget_exceeded: SQLITE_CONSTRAINT",
        ] {
            let Error::RustError(code) = observation_error(fail(text)) else {
                panic!("credit denial lost its safe machine code");
            };
            assert_eq!(code, "budget_exceeded");
            assert_eq!(
                crate::routes::classify_error(&format!("Error: {code}")).0,
                402
            );
        }
        assert!(
            matches!(observation_error(fail("super_budget_exceeded_secret")), Error::RustError(code) if code == "image_projection_failed")
        );
    }
}
