//! Private Code Interpreter transport on the session's existing workspace lease.

use std::{collections::BTreeSet, time::Duration};

use futures::StreamExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{Delay, Error, Method, ResponseBody, Result};

use super::{Workspace, digest_key, transport::TimedResponse};
use crate::space::OwnedFile;

const PENDING: &str = "workspace:python-publication-pending";
const ARTIFACT_LIMIT: usize = 16 * 1024 * 1024;
const REPLY_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PythonMemoryTier {
    #[serde(rename = "1g")]
    OneGiB,
    #[serde(rename = "4g")]
    FourGiB,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonDomainSecret {
    pub domain: String,
    pub name: String,
    pub value: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PythonNetworkPolicy {
    #[default]
    Disabled,
    Allowlist {
        allowed_domains: Vec<String>,
        #[serde(default)]
        domain_secrets: Vec<PythonDomainSecret>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonRequest {
    pub code: String,
    pub container_id: Option<String>,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub memory_limit: PythonMemoryTier,
    pub network_policy: Option<PythonNetworkPolicy>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PythonArtifact {
    pub id: String,
    pub filename: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub mime_type: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PythonOutcome {
    pub id: String,
    pub container_id: Option<String>,
    pub outputs: Vec<Value>,
    pub artifacts: Vec<PythonArtifact>,
    pub artifact_error: Option<String>,
    pub error: Option<String>,
    pub python_error: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub truncated: bool,
    pub duration_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PythonInput {
    pub container_id: String,
    pub file_id: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PythonFilePage {
    pub container_id: String,
    pub files: Vec<Value>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Dispatch {
    boot: String,
    request_hash: String,
    #[serde(default)]
    cancelled_before_dispatch: bool,
}

fn cancellation_proves_unsubmitted(cancelled: bool, dispatch: Option<&Dispatch>) -> bool {
    cancelled && dispatch.is_none_or(|dispatch| dispatch.cancelled_before_dispatch)
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

fn import_rejection(status: u16) -> Option<&'static str> {
    match status {
        404 => Some("code_container_not_found"),
        400 | 413 | 422 => Some("code_input_rejected"),
        _ => None,
    }
}

fn component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn container_id(value: &str) -> bool {
    value
        .strip_prefix("cntr_")
        .is_some_and(|tail| tail.len() == 32 && tail.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn artifact_id(value: &str) -> bool {
    value
        .strip_prefix("pyart_")
        .is_some_and(|tail| tail.len() == 64 && tail.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn cursor(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8192
        && value.len().is_multiple_of(2)
        && value.bytes().all(|b| b.is_ascii_hexdigit())
}

impl PythonRequest {
    fn validate(&self) -> Result<()> {
        if serde_json::to_vec(self)?.len() > 240 * 1024 {
            return Err(fail("invalid_code_request"));
        }
        if self.code.is_empty()
            || self.code.len() > 65536
            || self.code.contains('\0')
            || !(1..=300_000).contains(&self.timeout_ms)
            || !(1..=4 * 1024 * 1024).contains(&self.max_output_bytes)
            || self
                .container_id
                .as_deref()
                .is_some_and(|id| !container_id(id))
        {
            return Err(fail("invalid_code_request"));
        }
        Ok(())
    }
}

impl PythonArtifact {
    fn validate(&self) -> Result<()> {
        if !artifact_id(&self.id)
            || self.bytes > ARTIFACT_LIMIT as u64
            || super::decode_sha256(&self.sha256).is_err()
            || !crate::space::valid_filename(&self.filename)
            || self.mime_type.len() > 128
        {
            return Err(fail("code_artifact_invalid"));
        }
        Ok(())
    }
}

async fn bytes(mut response: TimedResponse, maximum: usize) -> Result<Vec<u8>> {
    if response.status_code() != 200 {
        return Err(fail(if response.status_code() == 404 {
            "code_container_not_found"
        } else {
            "code_transport_error"
        }));
    }
    let mut stream = response.response.stream()?;
    response
        .deadline
        .wait(async move {
            let mut output = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if chunk.len() > maximum.saturating_sub(output.len()) {
                    return Err(fail("code_response_exceeds_limit"));
                }
                output.extend_from_slice(&chunk);
            }
            Ok(output)
        })
        .await
}

async fn decode<T: DeserializeOwned>(response: TimedResponse) -> Result<T> {
    serde_json::from_slice(&bytes(response, REPLY_LIMIT).await?)
        .map_err(|_| fail("code_protocol_error"))
}

impl Workspace<'_> {
    /// Establish the real kernel identity before publishing a native tool item.
    /// This starts no cell; a lost reply can safely observe the same auto context.
    pub async fn prepare_python_context(&self, request: &PythonRequest) -> Result<String> {
        request.validate()?;
        let expected = match request.memory_limit {
            PythonMemoryTier::OneGiB => agents_core::RuntimeProfile::Basic1g,
            PythonMemoryTier::FourGiB => agents_core::RuntimeProfile::Standard4g,
        };
        if self.runtime_profile != expected {
            return Err(fail("code_memory_profile_mismatch"));
        }
        self.restore_if_needed().await?;
        let value = json!({"container_id":request.container_id,
            "network_policy":request.network_policy,"memory_limit":request.memory_limit});
        let response = self
            .request_until(
                Method::Post,
                "/v1/python/contexts",
                Some(JsValue::from_str(&value.to_string())),
                &[("Content-Type", "application/json")],
                crate::util::now_ms().saturating_add(120_000),
            )
            .await?;
        if response.status_code() == 404 {
            return Err(fail("code_container_not_found"));
        }
        if response.status_code() == 422 {
            return Err(fail("code_memory_profile_mismatch"));
        }
        if response.status_code() == 409 {
            let error: Value = decode(response).await?;
            return Err(fail(if error["error"] == "code_network_policy_conflict" {
                "code_network_policy_conflict"
            } else {
                "code_context_prepare_unavailable"
            }));
        }
        if response.status_code() != 200 {
            return Err(fail("code_context_prepare_unavailable"));
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Prepared {
            container_id: String,
        }
        let result: Prepared = decode(response).await?;
        if !container_id(&result.container_id)
            || request
                .container_id
                .as_ref()
                .is_some_and(|id| *id != result.container_id)
        {
            return Err(fail("code_protocol_error"));
        }
        Ok(result.container_id)
    }

    /// The caller selects the physical profile before this method. Restoration
    /// acquires/renews the ordinary compute lease even when Bash is not exposed.
    pub async fn execute_python(
        &self,
        execution_id: &str,
        request: &PythonRequest,
    ) -> Result<PythonOutcome> {
        request.validate()?;
        let expected = match request.memory_limit {
            PythonMemoryTier::OneGiB => agents_core::RuntimeProfile::Basic1g,
            PythonMemoryTier::FourGiB => agents_core::RuntimeProfile::Standard4g,
        };
        if self.runtime_profile != expected {
            return Err(fail("code_memory_profile_mismatch"));
        }
        if execution_id.is_empty() || execution_id.len() > 512 {
            return Err(fail("invalid_code_execution_id"));
        }
        let id = digest_key(execution_id);
        let dispatch_key = format!("workspace:python-dispatch:{id}");
        let mut wire = serde_json::to_value(request)?;
        wire["id"] = json!(id);
        let request_hash = digest_key(&wire.to_string());
        let previous = self.state.storage().get::<Dispatch>(&dispatch_key).await?;
        if previous
            .as_ref()
            .is_some_and(|old| old.request_hash == request_hash && old.cancelled_before_dispatch)
        {
            self.complete_python_publication(execution_id).await?;
            return Err(fail("code_cancelled_before_dispatch"));
        }
        if previous
            .as_ref()
            .is_some_and(|old| old.request_hash != request_hash)
        {
            return Err(fail("code_outcome_unknown"));
        }
        // An accepted dispatch belongs to its original live incarnation.
        // Observation must not restore a replacement VM before finding that
        // its predecessor's Python state/result has been lost.
        if previous.is_none() {
            self.restore_if_needed().await?;
        } else if !self.container()?.running() {
            return Err(fail("code_outcome_unknown"));
        }
        let boot = self
            .state
            .storage()
            .get::<String>("workspace:boot")
            .await?
            .ok_or_else(|| fail("workspace_boot_missing"))?;
        if previous.as_ref().is_some_and(|old| old.boot != boot) {
            return Err(fail("code_outcome_unknown"));
        }
        let end = crate::util::now_ms()
            .saturating_add(request.timeout_ms)
            .saturating_add(40_000);
        let path = format!("/v1/python/{id}");
        let probe = self
            .request_until(Method::Get, &path, None, &[], end)
            .await?;
        let response = match probe.status_code() {
            200 => probe,
            202 => self.wait_python(&id, end).await?,
            404 if previous.is_none() => {
                if self.python_cancel_requested(&id).await? {
                    return Err(fail("code_cancelled_before_dispatch"));
                }
                self.python_pending_add(&id).await?;
                self.state
                    .storage()
                    .put(
                        &dispatch_key,
                        Dispatch {
                            boot: boot.clone(),
                            request_hash: request_hash.clone(),
                            cancelled_before_dispatch: false,
                        },
                    )
                    .await?;
                // Bootstrap and durable writes yield to cancellation requests.
                // This local path has not POSTed: commit that proof before
                // removing its empty publication fence.
                if self.python_cancel_requested(&id).await? {
                    self.state
                        .storage()
                        .put(
                            &dispatch_key,
                            Dispatch {
                                boot,
                                request_hash,
                                cancelled_before_dispatch: true,
                            },
                        )
                        .await?;
                    self.complete_python_publication(execution_id).await?;
                    return Err(fail("code_cancelled_before_dispatch"));
                }
                match self
                    .request_until(
                        Method::Post,
                        "/v1/python",
                        Some(JsValue::from_str(&wire.to_string())),
                        &[("Content-Type", "application/json")],
                        end,
                    )
                    .await
                {
                    Ok(response) if response.status_code() == 200 => response,
                    Ok(response) if matches!(response.status_code(), 400 | 404 | 413 | 422) => {
                        // These statuses precede daemon admission. There cannot
                        // be an artifact spool to protect for this execution.
                        self.complete_python_publication(execution_id).await?;
                        return Err(fail(if response.status_code() == 404 {
                            "code_container_not_found"
                        } else {
                            "code_request_rejected"
                        }));
                    }
                    _ => self.wait_python(&id, end).await?,
                }
            }
            _ => return Err(fail("code_outcome_unknown")),
        };
        let outcome: PythonOutcome = decode(response).await?;
        validate_outcome(&outcome, &id, request.container_id.as_deref())?;
        Ok(outcome)
    }

    async fn wait_python(&self, id: &str, end: u64) -> Result<TimedResponse> {
        while crate::util::now_ms() < end {
            let response = self
                .request_until(
                    Method::Get,
                    &format!("/v1/python/{id}"),
                    None,
                    &[],
                    end.min(crate::util::now_ms().saturating_add(10_000)),
                )
                .await?;
            if response.status_code() != 202 {
                return Ok(response);
            }
            Delay::from(Duration::from_millis(250)).await;
        }
        Err(fail("code_outcome_unknown"))
    }

    /// Polling never boots a replacement VM or replays an accepted cell.
    pub async fn poll_python(&self, execution_id: &str) -> Result<Option<PythonOutcome>> {
        let id = digest_key(execution_id);
        let dispatch = self
            .state
            .storage()
            .get::<Dispatch>(&format!("workspace:python-dispatch:{id}"))
            .await?;
        if cancellation_proves_unsubmitted(
            self.python_cancel_requested(&id).await?,
            dispatch.as_ref(),
        ) {
            self.complete_python_publication(execution_id).await?;
            return Err(fail("code_cancelled_before_dispatch"));
        }
        let response = self
            .request(Method::Get, &format!("/v1/python/{id}"), None, &[])
            .await?;
        if response.status_code() == 202 {
            return Ok(None);
        }
        let result: PythonOutcome = decode(response).await?;
        validate_outcome(&result, &id, None)?;
        Ok(Some(result))
    }

    async fn python_cancel_requested(&self, id: &str) -> Result<bool> {
        Ok(self
            .state
            .storage()
            .get::<bool>(&format!("workspace:python-cancel:{id}"))
            .await?
            .unwrap_or(false))
    }

    /// Record cancellation even while the original call is bootstrapping. The
    /// root daemon DELETE is best effort; status reconciliation owns the result.
    pub async fn cancel_python(&self, execution_id: &str) -> Result<()> {
        let id = digest_key(execution_id);
        self.state
            .storage()
            .put(&format!("workspace:python-cancel:{id}"), true)
            .await?;
        let _ = self
            .request(Method::Delete, &format!("/v1/python/{id}"), None, &[])
            .await;
        Ok(())
    }

    async fn python_pending_add(&self, id: &str) -> Result<()> {
        let mut pending = self
            .state
            .storage()
            .get::<BTreeSet<String>>(PENDING)
            .await?
            .unwrap_or_default();
        if pending.len() >= 128 && !pending.contains(id) {
            return Err(fail("code_publication_backlog"));
        }
        pending.insert(id.into());
        self.state.storage().put(PENDING, pending).await
    }

    /// Session calls this only after canonical Space publications and all daemon
    /// spool acknowledgements commit, including a known empty-artifact outcome.
    pub async fn complete_python_publication(&self, execution_id: &str) -> Result<()> {
        let mut pending = self
            .state
            .storage()
            .get::<BTreeSet<String>>(PENDING)
            .await?
            .unwrap_or_default();
        pending.remove(&digest_key(execution_id));
        if pending.is_empty() {
            self.state.storage().delete(PENDING).await?;
            Ok(())
        } else {
            self.state.storage().put(PENDING, pending).await
        }
    }

    pub async fn import_python_file(
        &self,
        file: &OwnedFile,
        context: Option<&str>,
    ) -> Result<PythonInput> {
        if context.is_some_and(|id| !container_id(id))
            || file.bytes > 64 * 1024 * 1024
            || !component(&file.id)
            || file.account_id != self.owner
        {
            return Err(fail("invalid_code_input"));
        }
        let owned = crate::space::get_owned_file(self.env, &self.owner, &file.id)
            .await?
            .filter(|owned| {
                owned.content_key() == file.content_key()
                    && owned.sha256 == file.sha256
                    && owned.bytes == file.bytes
            })
            .ok_or_else(|| fail("file_not_found"))?;
        self.restore_if_needed().await?;
        let observation = self.require_r2("class_b").await?;
        let object = self
            .env
            .bucket("WORKSPACES")?
            .get(owned.content_key())
            .execute()
            .await?;
        self.record_r2(&observation, "class_b").await?;
        let object = object
            .filter(|object| object.size() == owned.bytes)
            .ok_or_else(|| fail("file_not_ready"))?;
        let ResponseBody::Stream(stream) = object
            .body()
            .ok_or_else(|| fail("file_not_ready"))?
            .response_body()?
        else {
            return Err(fail("file_not_streamable"));
        };
        let length = owned.bytes.to_string();
        let mut headers = vec![
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", length.as_str()),
            ("X-Content-Sha256", owned.sha256.as_str()),
        ];
        if let Some(context) = context {
            headers.push(("X-Code-Container", context));
        }
        let response = self
            .request_until(
                Method::Put,
                &format!("/v1/python/files/{}", owned.id),
                Some(stream.into()),
                &headers,
                crate::util::now_ms().saturating_add(120_000),
            )
            .await?;
        if let Some(code) = import_rejection(response.status_code()) {
            return Err(fail(code));
        }
        if response.status_code() != 200 {
            return Err(fail("code_input_unavailable"));
        }
        let result: PythonInput = decode(response).await?;
        if !container_id(&result.container_id)
            || context.is_some_and(|id| id != result.container_id)
            || result.file_id != owned.id
            || result.bytes != owned.bytes
            || result.sha256 != owned.sha256
            || result.path != format!("/mnt/inputs/{}/content", owned.id)
        {
            return Err(fail("code_input_protocol_error"));
        }
        Ok(result)
    }

    pub async fn python_artifact(&self, artifact: &PythonArtifact) -> Result<Vec<u8>> {
        artifact.validate()?;
        let response = self
            .request(
                Method::Get,
                &format!("/v1/python/artifacts/{}", artifact.id),
                None,
                &[],
            )
            .await?;
        if response.headers().get("X-Content-Sha256")?.as_deref() != Some(&artifact.sha256)
            || response
                .headers()
                .get("Content-Length")?
                .and_then(|v| v.parse::<u64>().ok())
                != Some(artifact.bytes)
        {
            return Err(fail("code_artifact_integrity_error"));
        }
        let output = bytes(response, ARTIFACT_LIMIT).await?;
        if output.len() as u64 != artifact.bytes
            || format!("{:x}", Sha256::digest(&output)) != artifact.sha256
        {
            return Err(fail("code_artifact_integrity_error"));
        }
        Ok(output)
    }

    pub async fn ack_python_artifact(&self, id: &str) -> Result<()> {
        if !artifact_id(id) {
            return Err(fail("code_artifact_invalid"));
        }
        let response = self
            .request(
                Method::Delete,
                &format!("/v1/python/artifacts/{id}"),
                None,
                &[],
            )
            .await?;
        if response.status_code() == 204 {
            Ok(())
        } else {
            Err(fail("code_artifact_ack_failed"))
        }
    }

    /// Source export is serialized with other workspace operations. Check the
    /// explicit context first: a stale selector must not export a newer namespace.
    pub async fn python_export_file(
        &self,
        execution_id: &str,
        context: &str,
        source: &str,
    ) -> Result<(String, Vec<u8>)> {
        if !cursor(source) {
            return Err(fail("invalid_code_file_cursor"));
        }
        self.python_files(context, None, 1).await?;
        self.python_pending_add(&digest_key(execution_id)).await?;
        let response = self
            .request(
                Method::Get,
                &format!("/v1/python/source/{source}"),
                None,
                &[],
            )
            .await?;
        let id = response
            .headers()
            .get("X-Code-Artifact")?
            .filter(|id| artifact_id(id))
            .ok_or_else(|| fail("code_artifact_invalid"))?;
        let hash = response
            .headers()
            .get("X-Content-Sha256")?
            .ok_or_else(|| fail("code_artifact_integrity_error"))?;
        if super::decode_sha256(&hash).is_err() {
            return Err(fail("code_artifact_integrity_error"));
        }
        let output = bytes(response, ARTIFACT_LIMIT).await?;
        if format!("{:x}", Sha256::digest(&output)) != hash {
            return Err(fail("code_artifact_integrity_error"));
        }
        Ok((id, output))
    }

    pub async fn python_files(
        &self,
        context: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<PythonFilePage> {
        if !container_id(context)
            || !(1..=100).contains(&limit)
            || after.is_some_and(|value| !cursor(value))
        {
            return Err(fail("invalid_code_file_page"));
        }
        let mut path = format!("/v1/python/files?container_id={context}&limit={limit}");
        if let Some(after) = after {
            path.push_str(&format!("&after={after}"));
        }
        let page: PythonFilePage =
            decode(self.request(Method::Get, &path, None, &[]).await?).await?;
        if page.container_id != context
            || page.files.len() > usize::from(limit)
            || page
                .next_cursor
                .as_deref()
                .is_some_and(|value| !cursor(value))
        {
            return Err(fail("code_protocol_error"));
        }
        Ok(page)
    }
}

fn validate_outcome(outcome: &PythonOutcome, id: &str, context: Option<&str>) -> Result<()> {
    let cancelled_before_dispatch = outcome.cancelled
        && outcome.error.as_deref() == Some("code_cancelled_before_dispatch")
        && outcome.container_id.is_none()
        && outcome.outputs.is_empty()
        && outcome.artifacts.is_empty()
        && outcome.artifact_error.is_none()
        && !outcome.python_error
        && !outcome.timed_out
        && !outcome.truncated
        && outcome.duration_ms == 0;
    if outcome.id != id
        || outcome.outputs.len() > 128
        || outcome.artifacts.len() > 128
        || outcome
            .container_id
            .as_deref()
            .is_some_and(|value| !container_id(value))
        || (!cancelled_before_dispatch
            && context.is_some_and(|context| outcome.container_id.as_deref() != Some(context)))
    {
        return Err(fail("code_protocol_error"));
    }
    let mut total = 0_u64;
    let mut ids = BTreeSet::new();
    for artifact in &outcome.artifacts {
        artifact.validate()?;
        total = total.saturating_add(artifact.bytes);
        if !ids.insert(&artifact.id) || total > 64 * 1024 * 1024 {
            return Err(fail("code_protocol_error"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_input_rejections_do_not_retry_and_busy_remains_transient() {
        for status in [400, 413, 422] {
            assert_eq!(import_rejection(status), Some("code_input_rejected"));
        }
        assert_eq!(import_rejection(404), Some("code_container_not_found"));
        for status in [200, 409, 429, 500, 503] {
            assert!(import_rejection(status).is_none());
        }
    }

    #[test]
    fn pre_dispatch_cancellation_receipt_does_not_claim_a_container_was_created() {
        let mut result = PythonOutcome {
            id: "known_cancel".into(),
            container_id: None,
            outputs: vec![],
            artifacts: vec![],
            artifact_error: None,
            error: Some("code_cancelled_before_dispatch".into()),
            python_error: false,
            timed_out: false,
            cancelled: true,
            truncated: false,
            duration_ms: 0,
        };
        assert!(
            validate_outcome(
                &result,
                "known_cancel",
                Some("cntr_0123456789abcdef0123456789abcdef")
            )
            .is_ok()
        );
        result
            .outputs
            .push(json!({"type":"logs","logs":"unproven execution"}));
        assert!(
            validate_outcome(
                &result,
                "known_cancel",
                Some("cntr_0123456789abcdef0123456789abcdef")
            )
            .is_err()
        );
        result.outputs.clear();
        result.error = Some("code_outcome_unknown".into());
        assert!(
            validate_outcome(
                &result,
                "known_cancel",
                Some("cntr_0123456789abcdef0123456789abcdef")
            )
            .is_err()
        );
    }

    #[test]
    fn cancellation_during_bootstrap_cannot_reclassify_an_unknown_post() {
        assert!(cancellation_proves_unsubmitted(true, None));
        assert!(!cancellation_proves_unsubmitted(false, None));
        let mut dispatch = Dispatch {
            boot: "physical".into(),
            request_hash: "request".into(),
            cancelled_before_dispatch: false,
        };
        assert!(!cancellation_proves_unsubmitted(true, Some(&dispatch)));
        dispatch.cancelled_before_dispatch = true;
        assert!(cancellation_proves_unsubmitted(true, Some(&dispatch)));
        let legacy: Dispatch =
            serde_json::from_str(r#"{"boot":"old","request_hash":"request"}"#).unwrap();
        assert!(!cancellation_proves_unsubmitted(true, Some(&legacy)));
    }

    #[test]
    fn private_request_rejects_unsupported_memory_and_unknown_fields() {
        let value = json!({"code":"x = 42","container_id":null,"timeout_ms":1000,"max_output_bytes":1024,"memory_limit":"4g","network_policy":{"type":"disabled"}});
        let request: PythonRequest = serde_json::from_value(value.clone()).unwrap();
        assert!(request.validate().is_ok());
        for memory in ["16g", "64g"] {
            let mut invalid = value.clone();
            invalid["memory_limit"] = json!(memory);
            assert!(serde_json::from_value::<PythonRequest>(invalid).is_err());
        }
        let mut invalid = value;
        invalid["secret_env"] = json!({"KEY":"not allowed"});
        assert!(serde_json::from_value::<PythonRequest>(invalid).is_err());
    }

    #[test]
    fn outcomes_reject_wrong_identity_duplicates_and_invalid_artifact_bounds() {
        let artifact = PythonArtifact {
            id: format!("pyart_{}", "a".repeat(64)),
            filename: "output.csv".into(),
            path: "output.csv".into(),
            bytes: 3,
            sha256: "b".repeat(64),
            mime_type: "text/csv".into(),
        };
        let mut result = PythonOutcome {
            id: "expected".into(),
            container_id: Some("cntr_0123456789abcdef0123456789abcdef".into()),
            outputs: vec![],
            artifacts: vec![artifact.clone()],
            artifact_error: None,
            error: None,
            python_error: false,
            timed_out: false,
            cancelled: false,
            truncated: false,
            duration_ms: 10,
        };
        assert!(validate_outcome(&result, "expected", result.container_id.as_deref()).is_ok());
        assert!(validate_outcome(&result, "other", None).is_err());
        assert!(
            validate_outcome(
                &result,
                "expected",
                Some("cntr_ffffffffffffffffffffffffffffffff")
            )
            .is_err()
        );
        result.artifacts.push(artifact);
        assert!(validate_outcome(&result, "expected", None).is_err());
        result.artifacts.pop();
        result.artifacts[0].bytes = ARTIFACT_LIMIT as u64 + 1;
        assert!(validate_outcome(&result, "expected", None).is_err());
    }

    #[test]
    fn allowlist_policy_is_private_tagged_wire_not_secret_environment() {
        let policy: PythonNetworkPolicy = serde_json::from_value(json!({
            "type":"allowlist", "allowed_domains":["api.example.com"],
            "domain_secrets":[{"domain":"api.example.com","name":"Authorization","value":"Bearer private"}]
        })).unwrap();
        let wire = serde_json::to_value(policy).unwrap();
        assert_eq!(wire["type"], "allowlist");
        assert!(wire.get("secret_env").is_none());
        assert!(serde_json::from_value::<PythonNetworkPolicy>(json!({"type":"enabled"})).is_err());
    }

    #[test]
    fn daemon_urls_require_opaque_ids_and_hex_cursors() {
        assert!(container_id("cntr_0123456789abcdef0123456789abcdef"));
        assert!(!container_id("cntr_../../etc/shadow"));
        assert!(!artifact_id("pyart_../secret"));
        assert!(cursor("646174612f61"));
        assert!(!cursor("aa?container_id=other"));
        assert!(!cursor("f"));
        assert!(component("file-0123456789abcdef"));
        assert!(!component("../file"));
    }
}
