//! Structural execution evidence. Response bodies live in encrypted recovery
//! storage, not exception strings or telemetry attributes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Format, ImageResult, ImageSource};
use crate::provider_response_receipt::ResponseMeta;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImagePhase {
    #[default]
    Prepare,
    Send,
    Headers,
    ReadBody,
    RetainBody,
    ParseJson,
    CheckStatus,
    DecodeImage,
    ValidateImage,
    StoreImage,
    FetchImage,
    BindImage,
    SettleUsage,
    Complete,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageObservation {
    pub phase: ImagePhase,
    pub upstream_status: Option<u16>,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub declared_content_length: Option<u64>,
    pub cf_ray: Option<String>,
    pub request_id: Option<String>,
    pub generation_id: Option<String>,
    pub received_bytes: u64,
    pub duration_ms: u64,
    pub parser_line: Option<usize>,
    pub parser_column: Option<usize>,
    pub expected_format: Option<Format>,
    pub actual_format: Option<Format>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub image_bytes: Option<u64>,
    pub reported_cost_usd: Option<Value>,
    pub provider_completed: bool,
    pub confirmed_no_charge: bool,
    pub provider_error_code: Option<String>,
}

pub(crate) struct ProviderReply {
    pub sequence: u16,
    pub meta: ResponseMeta,
    pub body: Vec<u8>,
    pub expected_format: Option<Format>,
    pub requested_size: Option<String>,
    pub event_stream: bool,
    pub observation: ImageObservation,
}

pub(crate) struct ImageExchange {
    pub result: ImageResult,
    pub observation: ImageObservation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ImageFailure {
    pub code: String,
    pub error_type: String,
    pub observation: ImageObservation,
}

impl ImageFailure {
    pub(crate) fn new(error: worker::Error, observation: ImageObservation) -> Self {
        Self::from_error(&error, observation)
    }

    pub(crate) fn from_error(error: &worker::Error, observation: ImageObservation) -> Self {
        let code = match &error {
            worker::Error::RustError(code)
                if code.len() <= 128
                    && [
                        "image_",
                        "provider_receipt_",
                        "workspace_blob_",
                        "owner_blob_",
                        "hosted_image_",
                        "resource_",
                        "service_",
                        "invalid_generated_image",
                    ]
                    .iter()
                    .any(|prefix| code.starts_with(prefix))
                    && code.bytes().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || b"_:=".contains(&c)
                    }) =>
            {
                code.clone()
            }
            _ => "image_generation_transport_error".into(),
        };
        let error_type = match error {
            worker::Error::RustError(_) => "RustError",
            worker::Error::JsError(_) => "JsError",
            worker::Error::Internal(_) => "Internal",
            worker::Error::Io(_) => "IoError",
            worker::Error::SerdeJsonError(_) => "SerdeJsonError",
            worker::Error::BindingError(_) => "BindingError",
            worker::Error::D1(_) => "D1Error",
            _ => "WorkerError",
        }
        .into();
        Self {
            code,
            error_type,
            observation,
        }
    }
}

impl ImageObservation {
    pub(crate) fn observe_json(&mut self, value: &Value, openrouter: bool) {
        if let Some(id) = value["id"].as_str().filter(|id| identifier(id)) {
            self.generation_id = Some(id.into());
        }
        if let Some(cost) = value["usage"].get("cost").filter(|v| decimal(v)) {
            self.reported_cost_usd = Some(cost.clone());
        }
        let output = value
            .get("data")
            .and_then(Value::as_array)
            .and_then(|data| (data.len() == 1).then(|| &data[0]))
            .or_else(|| (value["type"] == "image_generation.completed").then_some(value));
        self.provider_completed = self
            .upstream_status
            .is_some_and(|s| (200..300).contains(&s))
            && value.get("error").is_none_or(Value::is_null)
            && output.is_some_and(|item| {
                ["b64_json", "url"]
                    .into_iter()
                    .any(|key| item[key].as_str().is_some_and(|s| !s.is_empty()))
            });
        if let Some(error) = value.get("error").filter(|e| e.is_object()) {
            self.provider_error_code = error.get("code").and_then(|v| {
                v.as_str()
                    .filter(|s| identifier(s))
                    .map(str::to_owned)
                    .or_else(|| v.as_u64().map(|v| v.to_string()))
            });
            // OpenRouter's documented Images contract bills only completed
            // images. This is complete authenticated error evidence, not a
            // timeout, a proxy HTML page, or a client-side codec rejection.
            self.confirmed_no_charge = openrouter
                && !self.provider_completed
                && self.provider_error_code.is_some()
                && (self.upstream_status.is_some_and(|s| s >= 400) || value["type"] == "error");
        }
    }

    pub(crate) fn observe_result(&mut self, result: &ImageResult) {
        if let ImageSource::Inline { image } = &result.source {
            let (width, height) = image.dimensions();
            self.actual_format = Some(image.format());
            self.width = Some(width);
            self.height = Some(height);
            self.image_bytes = Some(image.len() as u64);
        }
    }
}

pub(crate) fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c))
}

fn decimal(value: &Value) -> bool {
    value.is_number()
        || value.as_str().is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || b".eE+-".contains(&c))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn received_success_cost_survives_a_later_local_codec_failure() {
        let mut observed = ImageObservation {
            upstream_status: Some(200),
            ..Default::default()
        };
        observed.observe_json(&json!({"id":"gen-img-original","data":[{"b64_json":"provider-bytes"}],"usage":{"cost":0.018}}), true);
        let failed = ImageFailure::new(
            worker::Error::RustError("image_generation_format_mismatch".into()),
            observed,
        );
        assert!(failed.observation.provider_completed);
        assert!(!failed.observation.confirmed_no_charge);
        assert_eq!(
            failed.observation.generation_id.as_deref(),
            Some("gen-img-original")
        );
        assert_eq!(failed.observation.reported_cost_usd, Some(json!(0.018)));
    }

    #[test]
    fn only_documented_complete_image_errors_confirm_zero_not_transport_gaps() {
        let mut rejected = ImageObservation {
            upstream_status: Some(502),
            ..Default::default()
        };
        rejected.observe_json(
            &json!({"error":{"code":502,"message":"provider failed"}}),
            true,
        );
        assert!(rejected.confirmed_no_charge);
        let mut other = ImageObservation {
            upstream_status: Some(502),
            ..Default::default()
        };
        other.observe_json(&json!({"error":{"code":502}}), false);
        assert!(!other.confirmed_no_charge);
        let failed = ImageFailure::new(
            worker::Error::RustError("private key and prompt must not enter telemetry".into()),
            ImageObservation::default(),
        );
        assert_eq!(failed.code, "image_generation_transport_error");
        assert!(
            !serde_json::to_string(&failed)
                .unwrap()
                .contains("private key")
        );
    }
}
