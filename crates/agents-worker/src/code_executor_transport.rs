//! A separately authorized compatible executor; never an extra model request.
use futures::StreamExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::{
    Env, Error, Fetch, Headers, Method, Request, RequestInit, RequestRedirect, Response, Result,
};

use crate::{
    auth::provider_keys,
    telemetry::TraceContext,
    workspace::{PythonArtifact, PythonOutcome, PythonRequest},
};

const MAX_REPLY: usize = 32 * 1024 * 1024;
const MAX_ARTIFACT: usize = 16 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutorRoute {
    pub credential_id: String,
    pub base_url: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Capabilities {
    pub protocol: String,
    pub context_scope: String,
    pub version: u32,
    pub memory_limits: Vec<ExecutorMemory>,
    pub network_disabled: bool,
    pub network_allowlist: bool,
    pub max_execution_ms: u64,
    pub max_artifact_bytes: u64,
    pub atomic_boot_header: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutorProfile {
    pub route: ExecutorRoute,
    pub capabilities: Capabilities,
}

/// Neither Debug nor Serialize: decrypted authority never enters persisted pins.
pub(crate) struct Executor {
    route: ExecutorRoute,
    key: String,
    trace: Option<TraceContext>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedContext {
    pub container_id: String,
    pub boot_id: String,
}

pub(crate) use agents_core::CodeMemory as ExecutorMemory;
pub(crate) type ExecutorRequest = PythonRequest;

pub(crate) enum Probe {
    Complete(PythonOutcome),
    Pending,
    Absent,
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}
fn cid(value: &str) -> bool {
    value.strip_prefix("cntr_").is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}
fn artifact_id(value: &str) -> bool {
    value.strip_prefix("pyart_").is_some_and(|suffix| {
        suffix.len() == 64 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

impl ExecutorRoute {
    async fn open(&self, env: &Env, owner: &str, trace: Option<TraceContext>) -> Result<Executor> {
        let credential = provider_keys::credential(env, owner, &self.credential_id).await?;
        if credential.base_url != self.base_url {
            return Err(fail("code_executor_route_changed"));
        }
        Ok(Executor {
            route: self.clone(),
            key: credential.api_key,
            trace,
        })
    }
}

impl ExecutorProfile {
    pub(crate) fn lease_key(&self) -> String {
        format!("{:x}", Sha256::digest(self.route.base_url.as_bytes()))
    }
    pub(crate) async fn resolve(env: &Env, owner: &str, credential_id: &str) -> Result<Self> {
        let metadata = provider_keys::metadata(env, owner, credential_id).await?;
        let route = ExecutorRoute {
            credential_id: credential_id.into(),
            base_url: metadata.base_url,
        };
        let capabilities = route.open(env, owner, None).await?.capabilities().await?;
        if capabilities.protocol != "workspace-daemon"
            || capabilities.context_scope != "instance"
            || capabilities.version != 1
            || !capabilities.atomic_boot_header
        {
            return Err(fail("code_executor_protocol_unsupported"));
        }
        Ok(Self {
            route,
            capabilities,
        })
    }
    pub(crate) async fn open(
        &self,
        env: &Env,
        owner: &str,
        trace: Option<TraceContext>,
    ) -> Result<Executor> {
        self.route.open(env, owner, trace).await
    }
}

impl Executor {
    /// This contract is the actual workspace-daemon API, not a vendor endpoint
    /// guessed from an OpenAI model URL. The configured base is its service root.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> Result<Response> {
        let body = body.map(|body| js_sys::Uint8Array::from(body.as_slice()).into());
        self.request_body(method, path, body, content_type, &[])
            .await
    }

    async fn request_body(
        &self,
        method: Method,
        path: &str,
        body: Option<wasm_bindgen::JsValue>,
        content_type: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Result<Response> {
        let headers = Headers::new();
        headers.set("Authorization", &format!("Bearer {}", self.key))?;
        for (name, value) in extra {
            headers.set(name, value)?;
        }
        if let Some(content_type) = content_type {
            headers.set("Content-Type", content_type)?;
        }
        if let Some(trace) = &self.trace {
            trace.child().inject(&headers)?;
        }
        let mut init = RequestInit::new();
        init.with_method(method)
            .with_headers(headers)
            .with_redirect(RequestRedirect::Manual);
        if let Some(body) = body {
            init.with_body(Some(body));
        }
        let request = Request::new_with_init(&format!("{}{path}", self.route.base_url), &init)?;
        let response = crate::provider_transport::handshake(Fetch::Request(request).send())
            .await
            .map_err(|_| fail("code_executor_outcome_unknown"))?;
        if (300..400).contains(&response.status_code()) {
            return Err(fail("code_executor_redirect_rejected"));
        }
        Ok(response)
    }

    async fn json(&self, method: Method, path: &str, value: &Value) -> Result<Response> {
        self.request(
            method,
            path,
            Some(serde_json::to_vec(value)?),
            Some("application/json"),
        )
        .await
    }

    pub(crate) async fn capabilities(&self) -> Result<Capabilities> {
        let response = self
            .request(Method::Get, "/v1/python/capabilities", None, None)
            .await?;
        if response.status_code() != 200 {
            return Err(fail("code_executor_capabilities_unavailable"));
        }
        serde_json::from_slice(&bounded(response, 16384).await?)
            .map_err(|_| fail("code_executor_protocol_error"))
    }

    pub(crate) async fn boot(&self) -> Result<String> {
        let response = self.request(Method::Get, "/health", None, None).await?;
        if response.status_code() != 200 {
            return Err(fail("code_executor_unavailable"));
        }
        let health: Value = decode(response).await?;
        health["boot_id"]
            .as_str()
            .filter(|id| identifier(id))
            .map(str::to_owned)
            .ok_or_else(|| fail("code_executor_protocol_error"))
    }

    pub(crate) async fn prepare(&self, request: &ExecutorRequest) -> Result<PreparedContext> {
        let response = self.json(Method::Post, "/v1/python/contexts", &json!({
            "container_id":request.container_id,"memory_limit":request.memory_limit,"network_policy":request.network_policy
        })).await?;
        match response.status_code() {
            404 => return Err(fail("code_container_not_found")),
            422 => return Err(fail("code_executor_memory_unsupported")),
            200 => {}
            _ => return Err(fail("code_executor_prepare_unavailable")),
        }
        let boot_id = response
            .headers()
            .get("X-Code-Boot-Id")?
            .filter(|id| identifier(id))
            .ok_or_else(|| fail("code_executor_boot_proof_missing"))?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Context {
            container_id: String,
        }
        let context: Context = decode(response).await?;
        if !cid(&context.container_id)
            || request
                .container_id
                .as_ref()
                .is_some_and(|id| id != &context.container_id)
        {
            return Err(fail("code_executor_protocol_error"));
        }
        Ok(PreparedContext {
            container_id: context.container_id,
            boot_id,
        })
    }

    /// The caller must have persisted its dispatch intent. A lost reply or an
    /// Absent probe after that intent is NOT permission to call submit again.
    pub(crate) async fn submit(
        &self,
        id: &str,
        request: &ExecutorRequest,
    ) -> Result<PythonOutcome> {
        if !identifier(id) {
            return Err(fail("invalid_code_execution_id"));
        }
        let mut wire = serde_json::to_value(request)?;
        wire["id"] = json!(id);
        let response = self.json(Method::Post, "/v1/python", &wire).await?;
        if response.status_code() != 200 {
            return Err(fail("code_executor_outcome_unknown"));
        }
        let outcome: PythonOutcome = decode(response).await?;
        validate(&outcome, id, request.container_id.as_deref())?;
        Ok(outcome)
    }

    pub(crate) async fn probe(&self, id: &str, context: &str) -> Result<Probe> {
        if !identifier(id) || !cid(context) {
            return Err(fail("invalid_code_execution_id"));
        }
        let response = self
            .request(Method::Get, &format!("/v1/python/{id}"), None, None)
            .await?;
        match response.status_code() {
            200 => {
                let outcome: PythonOutcome = decode(response).await?;
                validate(&outcome, id, Some(context))?;
                Ok(Probe::Complete(outcome))
            }
            202 => Ok(Probe::Pending),
            404 => Ok(Probe::Absent),
            _ => Err(fail("code_executor_outcome_unknown")),
        }
    }

    pub(crate) async fn cancel(&self, id: &str) -> Result<()> {
        if !identifier(id) {
            return Err(fail("invalid_code_execution_id"));
        }
        let response = self
            .request(Method::Delete, &format!("/v1/python/{id}"), None, None)
            .await?;
        if response.status_code() == 204 {
            Ok(())
        } else {
            Err(fail("code_executor_cancel_pending"))
        }
    }

    pub(crate) async fn artifact(&self, artifact: &PythonArtifact) -> Result<Vec<u8>> {
        if !artifact_id(&artifact.id) || artifact.bytes > MAX_ARTIFACT as u64 {
            return Err(fail("code_artifact_invalid"));
        }
        let response = self
            .request(
                Method::Get,
                &format!("/v1/python/artifacts/{}", artifact.id),
                None,
                None,
            )
            .await?;
        if response.status_code() != 200 {
            return Err(fail("code_artifact_unavailable"));
        }
        let bytes = bounded(response, MAX_ARTIFACT).await?;
        if bytes.len() as u64 != artifact.bytes
            || format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256
        {
            return Err(fail("code_artifact_integrity_error"));
        }
        Ok(bytes)
    }

    pub(crate) async fn acknowledge(&self, id: &str) -> Result<()> {
        if !artifact_id(id) {
            return Err(fail("code_artifact_invalid"));
        }
        let response = self
            .request(
                Method::Delete,
                &format!("/v1/python/artifacts/{id}"),
                None,
                None,
            )
            .await?;
        if response.status_code() == 204 {
            Ok(())
        } else {
            Err(fail("code_artifact_ack_failed"))
        }
    }

    pub(crate) async fn current_context(&self) -> Result<Option<String>> {
        let response = self
            .request(Method::Get, "/v1/python/contexts", None, None)
            .await?;
        if response.status_code() == 404 {
            return Ok(None);
        }
        if response.status_code() != 200 {
            return Err(fail("code_executor_inspect_unavailable"));
        }
        let value: Value = decode(response).await?;
        value["container_id"]
            .as_str()
            .filter(|id| cid(id))
            .map(|id| Some(id.into()))
            .ok_or_else(|| fail("code_executor_protocol_error"))
    }

    pub(crate) async fn inspect(&self, context: &str, boot: &str) -> Result<bool> {
        if !cid(context) || self.boot().await? != boot {
            return Ok(false);
        }
        let response = self
            .request(
                Method::Get,
                &format!("/v1/python/contexts/{context}"),
                None,
                None,
            )
            .await?;
        match response.status_code() {
            404 => Ok(false),
            200 => {
                let value: Value = decode(response).await?;
                Ok(value["container_id"] == context && value["status"] == "running")
            }
            _ => Err(fail("code_executor_inspect_unavailable")),
        }
    }

    pub(crate) async fn import(
        &self,
        file: &crate::space::OwnedFile,
        context: Option<&str>,
        stream: wasm_bindgen::JsValue,
    ) -> Result<crate::workspace::PythonInput> {
        if !identifier(&file.id) || context.is_some_and(|id| !cid(id)) {
            return Err(fail("invalid_code_input"));
        }
        let length = file.bytes.to_string();
        let mut extra = vec![
            ("Content-Length", length.as_str()),
            ("X-Content-Sha256", file.sha256.as_str()),
        ];
        if let Some(context) = context {
            extra.push(("X-Code-Container", context));
        }
        let response = self
            .request_body(
                Method::Put,
                &format!("/v1/python/files/{}", file.id),
                Some(stream),
                Some("application/octet-stream"),
                &extra,
            )
            .await?;
        if matches!(response.status_code(), 400 | 404 | 413 | 422) {
            return Err(fail("code_input_rejected"));
        }
        if response.status_code() != 200 {
            return Err(fail("code_executor_import_unavailable"));
        }
        let imported: crate::workspace::PythonInput = decode(response).await?;
        if imported.file_id != file.id
            || imported.bytes != file.bytes
            || imported.sha256 != file.sha256
            || !cid(&imported.container_id)
            || context.is_some_and(|id| id != imported.container_id)
        {
            return Err(fail("code_executor_protocol_error"));
        }
        Ok(imported)
    }

    pub(crate) async fn retire(&self, context: &str) -> Result<bool> {
        if !cid(context) {
            return Err(fail("invalid_code_context"));
        }
        let response = self
            .request(
                Method::Delete,
                &format!("/v1/python/contexts/{context}"),
                None,
                None,
            )
            .await?;
        match response.status_code() {
            204 | 404 => Ok(true),
            409 => Ok(false),
            _ => Err(fail("code_executor_release_pending")),
        }
    }
}

async fn bounded(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    let mut stream = crate::provider_transport::read(response.stream()?);
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| fail("code_executor_outcome_unknown"))?;
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(fail("code_executor_response_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    serde_json::from_slice(&bounded(response, MAX_REPLY).await?)
        .map_err(|_| fail("code_executor_protocol_error"))
}
fn validate(outcome: &PythonOutcome, id: &str, context: Option<&str>) -> Result<()> {
    if outcome.id == id
        && outcome.cancelled
        && outcome.error.as_deref() == Some("code_cancelled_before_dispatch")
        && outcome.container_id.is_none()
        && outcome.outputs.is_empty()
        && outcome.artifacts.is_empty()
        && outcome.artifact_error.is_none()
        && !outcome.python_error
        && !outcome.timed_out
        && !outcome.truncated
        && outcome.duration_ms == 0
    {
        return Err(fail("code_cancelled_before_dispatch"));
    }
    if outcome.id != id
        || !outcome.container_id.as_deref().is_some_and(cid)
        || context.is_some_and(|id| outcome.container_id.as_deref() != Some(id))
        || outcome.outputs.len() > 128
        || outcome.artifacts.len() > 128
        || outcome
            .artifacts
            .iter()
            .any(|artifact| !artifact_id(&artifact.id) || artifact.bytes > MAX_ARTIFACT as u64)
    {
        return Err(fail("code_executor_protocol_error"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_executor_memory_is_not_rewritten_to_a_cloudflare_sku() {
        let sixteen: ExecutorMemory = serde_json::from_str("\"16g\"").unwrap();
        let sixty_four: ExecutorMemory = serde_json::from_str("\"64g\"").unwrap();
        assert_eq!(serde_json::to_value(sixteen).unwrap(), json!("16g"));
        assert_eq!(serde_json::to_value(sixty_four).unwrap(), json!("64g"));
    }
    #[test]
    fn executor_context_and_spool_selectors_are_not_arbitrary_url_paths() {
        assert!(cid(&format!("cntr_{}", "a".repeat(32))));
        assert!(artifact_id(&format!("pyart_{}", "b".repeat(64))));
        for value in ["../health", "//other.host/key", "cntr_new", "pyart_%2f", ""] {
            assert!(!cid(value));
            assert!(!artifact_id(value));
        }
    }
}
