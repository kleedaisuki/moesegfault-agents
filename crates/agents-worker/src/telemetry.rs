//! Content-free operational metadata. Application correlation IDs are not Cloudflare span IDs.
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use worker::{Env, Headers, Request, Response};

pub const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
pub const DIAGNOSTIC_RETENTION_DAYS: u32 = 7;

/// Deployment metadata is a Git SHA, never an arbitrary environment string.
pub fn validated_build_id(value: Option<&str>) -> &str {
    match value {
        Some(value) if value.len() == 40 && hex(value.as_bytes()) => value,
        _ => "unknown",
    }
}

pub fn run_provenance(
    record: &agents_core::SessionRecord,
    run_id: &str,
    build_id: &str,
    trace_id: &str,
) -> Value {
    let registry_version = record.pinned.tools.first().map_or("none", |first| {
        if record
            .pinned
            .tools
            .iter()
            .all(|tool| tool.version == first.version)
        {
            first.version.as_str()
        } else {
            "mixed"
        }
    });
    json!({
        "agent_id":record.agent_id,"session_id":record.id,"run_id":run_id,
        "build_id":validated_build_id(Some(build_id)),"trace_id":trace_id,
        "config_revision":record.pinned.agent_revision,"prefix_hash":record.pinned.prefix_hash(),
        "registry_version":registry_version
    })
}

pub fn checkpoint_provenance(
    session_id: &str,
    run_id: Option<&str>,
    snapshot: &agents_core::WorkspaceSnapshot,
) -> Value {
    json!({"session_id":session_id,"run_id":run_id,"snapshot_id":snapshot.sha256,
        "workspace_revision":snapshot.generation.to_string(),"snapshot_bytes":snapshot.archive_bytes})
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceContext {
    trace_id: String,
    span_id: String,
    flags: u8,
}

impl TraceContext {
    /// Accept only bounded, valid W3C context; never accept caller-supplied application IDs.
    pub fn from_traceparent(header: Option<&str>) -> Self {
        let incoming = header.and_then(Self::parse);
        Self {
            trace_id: incoming
                .as_ref()
                .map(|v| v.trace_id.clone())
                .unwrap_or_else(random_trace_id),
            span_id: random_span_id(),
            flags: incoming.map(|v| v.flags & 1).unwrap_or(0),
        }
    }

    pub fn parse(header: &str) -> Option<Self> {
        let b = header.as_bytes();
        if b.len() < 55 || b.len() > 512 || b[2] != b'-' || b[35] != b'-' || b[52] != b'-' {
            return None;
        }
        if !hex(&b[..2])
            || &b[..2] == b"ff"
            || (&b[..2] == b"00" && b.len() != 55)
            || (b.len() > 55 && b[55] != b'-')
            || !hex(&b[3..35])
            || !hex(&b[36..52])
            || !hex(&b[53..55])
            || b[3..35].iter().all(|c| *c == b'0')
            || b[36..52].iter().all(|c| *c == b'0')
        {
            return None;
        }
        Some(Self {
            trace_id: header[3..35].into(),
            span_id: header[36..52].into(),
            flags: u8::from_str_radix(&header[53..55], 16).ok()?,
        })
    }

    pub fn child(&self) -> Self {
        Self {
            trace_id: self.trace_id.clone(),
            span_id: random_span_id(),
            flags: self.flags & 1,
        }
    }

    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id,
            self.span_id,
            self.flags & 1
        )
    }

    /// Propagate only to trusted internal hops. Do not expose trace baggage to model vendors.
    pub fn inject(&self, headers: &Headers) -> worker::Result<()> {
        headers.set("traceparent", &self.traceparent())
    }
}

fn hex(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}
fn random_trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
fn random_span_id() -> String {
    random_trace_id()[..16].into()
}

/// Drop unknown fields rather than attempting to redact arbitrary user text.
pub fn sanitize_fields(fields: Value) -> Value {
    sanitize_fields_at(fields, 1)
}

fn sanitize_fields_at(fields: Value, depth: u8) -> Value {
    let Some(fields) = fields.as_object() else {
        return json!({});
    };
    let mut safe = Map::new();
    for (key, value) in fields {
        if key == "observation" && depth > 0 && value.is_object() {
            safe.insert(key.clone(), sanitize_fields_at(value.clone(), depth - 1));
            continue;
        }
        let numeric = matches!(
            key.as_str(),
            "parser_line"
                | "upstream_status"
                | "declared_content_length"
                | "received_bytes"
                | "encrypted_storage_bytes"
                | "image_bytes"
                | "width"
                | "height"
                | "receipt_count"
                | "provider_requests"
                | "frames"
                | "reasoning_tokens"
                | "rendered_input_tokens"
                | "parser_column"
                | "request_bytes"
                | "response_bytes"
                | "declared_response_bytes"
                | "upstream_error_code"
                | "lock_wait_ms"
                | "funding_ms"
                | "rpc_ms"
                | "observation_ms"
                | "pending_deadline_ms"
                | "duration_ms"
                | "prompt_tokens"
                | "completion_tokens"
                | "cached_tokens"
                | "total_tokens"
                | "input_tokens"
                | "output_tokens"
                | "bytes"
                | "snapshot_bytes"
                | "turn"
                | "round"
                | "sequence"
                | "status"
                | "attempt"
                | "count"
                | "config_revision"
        );
        let identifier = matches!(
            key.as_str(),
            "codec"
                | "execution_id"
                | "owner_id"
                | "model"
                | "api"
                | "cf_ray"
                | "generation_id"
                | "provider_request_id"
                | "provider_generation_id"
                | "provider_error_code"
                | "expected_format"
                | "actual_format"
                | "price_revision"
                | "error.type"
                | "error_class"
                | "stage"
                | "protocol"
                | "thinking_mode"
                | "finish_reason"
                | "ray_id"
                | "content_type"
                | "content_encoding"
                | "input_sha256"
                | "response_sha256"
                | "conversation_id"
                | "operation_id"
                | "response_id"
                | "grant_event"
                | "request_path"
                | "error_type"
                | "server_label"
                | "mcp_method"
                | "cause_code"
                | "capacity_generation"
                | "request_id"
                | "session_id"
                | "agent_id"
                | "run_id"
                | "trace_id"
                | "span_id"
                | "parent_span_id"
                | "snapshot_id"
                | "tool_call_id"
                | "error_code"
                | "provider"
                | "tool"
                | "tool_name"
                | "phase"
                | "outcome"
                | "registry_version"
                | "prefix_hash"
                | "config_hash"
                | "workspace_revision"
                | "build_id"
        );
        if key == "mcp_protocol_error_code" && value.as_i64().is_some() {
            safe.insert(key.clone(), value.clone());
        } else if numeric && value.as_u64().is_some() {
            safe.insert(key.clone(), value.clone());
        } else if key == "content_type"
            && value.as_str().is_some_and(|v| {
                v.len() <= 256
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b" /;=._+-\"".contains(&b))
                    && v.split(';')
                        .next()
                        .is_some_and(|mime| mime.trim().contains('/'))
            })
        {
            safe.insert(key.clone(), value.clone());
        } else if identifier && value.as_str().is_some_and(|v| token(v, 256)) {
            safe.insert(key.clone(), value.clone());
        } else if key == "error_code"
            && value.as_str().is_some_and(|v| {
                v.len() <= 128
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-.:/=".contains(&b))
            })
        {
            safe.insert(key.clone(), value.clone());
        } else if key == "reported_cost_usd"
            && (value.as_number().is_some()
                || value.as_str().is_some_and(|v| {
                    v.len() <= 128 && v.parse::<f64>().is_ok_and(|n| n.is_finite() && n >= 0.0)
                }))
        {
            safe.insert(key.clone(), value.clone());
        } else if matches!(
            key.as_str(),
            "body_complete"
                | "provider_completed"
                | "confirmed_no_charge"
                | "receipt_stored"
                | "stream"
                | "activated"
                | "pending"
                | "byok"
                | "usage_reported"
                | "restored"
                | "success"
                | "outcome_unknown"
        ) && value.is_boolean()
        {
            safe.insert(key.clone(), value.clone());
        }
    }
    Value::Object(safe)
}

fn token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:/".contains(&b))
}

fn event_name(event: &str) -> &str {
    match event {
        "request.completed"
        | "request.failed"
        | "observer.completed"
        | "observer.recovery"
        | "run.started"
        | "run.completed"
        | "run.failed"
        | "provider.completed"
        | "provider.failed"
        | "provider.compaction_started"
        | "provider.compaction_completed"
        | "mcp.transport"
        | "mcp.discovery_failed"
        | "tool.completed"
        | "tool.failed"
        | "session.checkpoint"
        | "session.archived"
        | "session.restored"
        | "session.projection_pending"
        | "workspace.checkpoint"
        | "workspace.restored"
        | "diagnostics.accepted" => event,
        event
            if (event.starts_with("conversation.") || event.starts_with("file_search."))
                && token(event, 96) =>
        {
            event
        }
        event if event.starts_with("tool.image_generation.") && token(event, 96) => event,
        "provider.receipt_gc_deferred" | "provider.receipt_gc_completed" => event,
        _ => "operation",
    }
}

pub fn emit(event: &str, fields: Value) {
    let event = event_name(event);
    worker::console_log!(
        "{}",
        json!({"event":event,"timestamp_ms":crate::util::now_ms(),
        "schema_version":1,"fields":sanitize_fields(fields)})
    );
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsSubmission {
    pub consent: bool,
    pub client: String,
    pub version: String,
    pub platform: String,
    pub error_code: Option<String>,
    pub request_id: Option<String>,
}

impl DiagnosticsSubmission {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.consent {
            return Err("explicit diagnostic consent is required");
        }
        if !matches!(self.client.as_str(), "magents" | "adash") {
            return Err("unknown client");
        }
        if !token(&self.version, 64)
            || !token(&self.platform, 64)
            || self.error_code.as_ref().is_some_and(|v| !token(v, 64))
            || self.request_id.as_ref().is_some_and(|v| !token(v, 128))
        {
            return Err("diagnostic fields must be bounded identifiers");
        }
        Ok(())
    }
}

async fn write_diagnostics(
    env: &Env,
    owner_id: &str,
    submission: DiagnosticsSubmission,
    id: &str,
) -> worker::Result<()> {
    submission
        .validate()
        .map_err(|e| worker::Error::RustError(e.into()))?;
    let now = crate::util::now_ms();
    let body = serde_json::to_vec(&json!({"id":id,"owner_id":owner_id,"received_at_ms":now,
        "expires_at_ms":now + u64::from(DIAGNOSTIC_RETENTION_DAYS)*86_400_000,
        "schema_version":1,"submission":submission}))?;
    if body.len() > MAX_DIAGNOSTIC_BYTES {
        return Err(worker::Error::RustError("diagnostic too large".into()));
    }
    let bytes = body.len();
    env.bucket("DIAGNOSTICS")?
        .put(format!("diagnostics/{id}.json"), body)
        .execute()
        .await?;
    emit("diagnostics.accepted", json!({"bytes":bytes}));
    Ok(())
}

/// Authenticated callers only. Rate reservations are atomic D1 writes, not process-local counters.
pub async fn receive_diagnostic(
    req: &mut Request,
    env: &Env,
    account: &str,
) -> worker::Result<Response> {
    if env.bucket("DIAGNOSTICS").is_err() {
        return crate::util::json_error(
            503,
            "diagnostics_unavailable",
            "Diagnostics storage is unavailable",
        );
    }
    if req.headers().get("content-length")?.is_some_and(|v| {
        v.parse::<usize>()
            .map_or(true, |n| n > MAX_DIAGNOSTIC_BYTES)
    }) {
        return crate::util::json_error(
            413,
            "diagnostic_too_large",
            "Diagnostics exceed the size limit",
        );
    }
    let mut stream = match req.stream() {
        Ok(stream) => stream,
        Err(_) => {
            return crate::util::json_error(
                400,
                "invalid_diagnostic",
                "A JSON diagnostic body is required",
            );
        }
    };
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return crate::util::json_error(
                    400,
                    "invalid_diagnostic",
                    "Could not read diagnostics",
                );
            }
        };
        if !append_diagnostic_chunk(&mut body, &chunk) {
            return crate::util::json_error(
                413,
                "diagnostic_too_large",
                "Diagnostics exceed the size limit",
            );
        }
    }
    let submission: DiagnosticsSubmission = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return crate::util::json_error(
                400,
                "invalid_diagnostic",
                "Diagnostics contain invalid or unsupported fields",
            );
        }
    };
    if submission.validate().is_err() {
        return crate::util::json_error(
            400,
            "invalid_diagnostic",
            "Diagnostics require consent and bounded client metadata",
        );
    }
    let id = crate::util::id("diag");
    let now = (crate::util::now_ms() / 1000) as f64;
    let db = env.d1("DB")?;
    // Store no client payload in D1; this row is an expiring rate-limit/audit reference.
    let result = db.prepare("INSERT INTO diagnostic_reports(id,account_id,payload_json,created_at,expires_at) SELECT ?1,?2,'{}',?3,?4 WHERE (SELECT count(*) FROM diagnostic_reports WHERE account_id=?2 AND created_at>?5)<5")
        .bind(&[id.clone().into(),account.into(),now.into(),(now+7.0*86400.0).into(),(now-3600.0).into()])?
        .run().await?;
    if result.meta()?.and_then(|m| m.changes).unwrap_or(0) != 1 {
        return crate::util::json_error(
            429,
            "diagnostics_rate_limited",
            "Diagnostics are limited to five submissions per hour",
        );
    }
    if write_diagnostics(env, account, submission, &id)
        .await
        .is_err()
    {
        // Keep the rate reservation: repeated storage errors must not bypass the abuse limit.
        return crate::util::json_error(
            503,
            "diagnostics_unavailable",
            "Diagnostics storage is unavailable",
        );
    }
    Ok(Response::from_json(
        &json!({"id":id,"accepted":true,"retention_days":DIAGNOSTIC_RETENTION_DAYS}),
    )?
    .with_status(202))
}

fn append_diagnostic_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> bool {
    if chunk.len() > MAX_DIAGNOSTIC_BYTES.saturating_sub(body.len()) {
        return false;
    }
    body.extend_from_slice(chunk);
    true
}

// worker-rs 0.8.7 has no tracing binding. This is a native platform API bridge, not a JS harness.
#[cfg(target_arch = "wasm32")]
mod native {
    use wasm_bindgen::prelude::*;
    #[wasm_bindgen(
        inline_js = "import { tracing } from 'cloudflare:workers'; export function agentsStartSpan(name) { return tracing.startSpan(name); } export function agentsAnnotate(span, trace) { span.setAttribute('app.trace_id', trace); } export function agentsEndSpan(span) { span.end(); }"
    )]
    extern "C" {
        #[wasm_bindgen(catch)]
        pub fn agentsStartSpan(name: &str) -> Result<JsValue, JsValue>;
        #[wasm_bindgen(catch)]
        pub fn agentsAnnotate(span: &JsValue, trace: &str) -> Result<(), JsValue>;
        #[wasm_bindgen(catch)]
        pub fn agentsEndSpan(span: &JsValue) -> Result<(), JsValue>;
    }
}

/// Timed native span, attached to the active Cloudflare invocation, ended on every exit path.
/// It does not become the active parent; platform fetch spans retain their native ancestry.
pub struct NativeSpan {
    #[cfg(target_arch = "wasm32")]
    inner: Option<wasm_bindgen::JsValue>,
}

impl NativeSpan {
    pub fn start(name: &'static str, context: Option<&TraceContext>) -> Self {
        #[cfg(target_arch = "wasm32")]
        {
            let inner = native::agentsStartSpan(name).ok();
            if let (Some(span), Some(context)) = (&inner, context) {
                let _ = native::agentsAnnotate(span, context.trace_id());
            }
            Self { inner }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (name, context);
            Self {}
        }
    }
}

impl Drop for NativeSpan {
    fn drop(&mut self) {
        #[cfg(target_arch = "wasm32")]
        if let Some(span) = &self.inner {
            let _ = native::agentsEndSpan(span);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const VALID: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    #[test]
    fn image_diagnostics_survive_the_actual_emission_filter_without_raw_payloads() {
        let fields = sanitize_fields(
            json!({"execution_id":"image:original", "error.type":"RustError",
            "error_code":"provider_receipt_delete_failed:chunk=23",
            "observation":{"phase":"validate_image","upstream_status":200,
            "generation_id":"gen-img-original","request_id":"request-original",
            "received_bytes":4096,"actual_format":"jpeg","expected_format":"png",
            "width":1024,"height":1024,"reported_cost_usd":"0.018",
            "provider_completed":true,"confirmed_no_charge":false,
            "prompt":"not a metric","authorization":"never a metric",
            "observation":{"received_bytes":999}},"body":"not a metric"}),
        );
        assert_eq!(
            fields["error_code"],
            "provider_receipt_delete_failed:chunk=23"
        );
        assert_eq!(fields["observation"]["upstream_status"], 200);
        assert_eq!(fields["observation"]["actual_format"], "jpeg");
        assert_eq!(fields["observation"]["reported_cost_usd"], "0.018");
        assert_eq!(fields["observation"]["provider_completed"], true);
        assert_eq!(
            event_name("tool.image_generation.failed"),
            "tool.image_generation.failed"
        );
        assert_eq!(
            event_name("provider.receipt_gc_deferred"),
            "provider.receipt_gc_deferred"
        );
        assert!(!fields.to_string().contains("metric"));
        assert!(fields["observation"].get("observation").is_none());
    }

    #[test]
    fn mcp_diagnostics_preserve_remote_codes_and_catalog_stage_without_descriptions() {
        let fields = sanitize_fields(
            json!({"server_label":"owned", "mcp_method":"server/discover",
            "phase":"parse_envelope", "status":400,"mcp_protocol_error_code":-32600,
            "request_id":"list-owned-0","provider_request_id":"upstream-owned","ray_id":"cf-owned",
            "response_bytes":1622,"duration_ms":5134,"outcome_unknown":false,
            "description":"private remote description","authorization":"private"}),
        );
        assert_eq!(fields["mcp_protocol_error_code"], -32600);
        assert_eq!(fields["server_label"], "owned");
        assert_eq!(fields["mcp_method"], "server/discover");
        assert_eq!(fields["outcome_unknown"], false);
        assert_eq!(event_name("mcp.transport"), "mcp.transport");
        assert_eq!(event_name("mcp.discovery_failed"), "mcp.discovery_failed");
        assert!(!fields.to_string().contains("private"));
    }
    #[test]
    fn automatic_compaction_diagnostics_keep_transport_and_terminal_evidence() {
        let fields = sanitize_fields(json!({"phase":"compaction","protocol":"anthropic",
            "thinking_mode":"disabled","finish_reason":"length","reasoning_tokens":512,
            "rendered_input_tokens":135,"provider_request_id":"original-summary",
            "output_tokens":512,"success":false,"summary":"private context"}));
        assert_eq!(fields["thinking_mode"], "disabled");
        assert_eq!(fields["finish_reason"], "length");
        assert_eq!(fields["reasoning_tokens"], 512);
        assert_eq!(fields["provider_request_id"], "original-summary");
        assert_eq!(
            event_name("provider.compaction_completed"),
            "provider.compaction_completed"
        );
        assert!(fields.get("summary").is_none());
    }
    #[test]
    fn deployment_build_id_is_only_a_full_validated_git_sha() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(validated_build_id(Some(sha)), sha);
        for invalid in [
            None,
            Some("unknown"),
            Some("abc123"),
            Some("private instructions"),
            Some("0123456789ABCDEF0123456789ABCDEF01234567"),
            Some("0123456789abcdef0123456789abcdef01234567\n"),
        ] {
            assert_eq!(validated_build_id(invalid), "unknown");
        }
    }
    #[test]
    fn run_provenance_joins_pinned_inputs_and_build_without_logging_their_contents() {
        let agent = serde_json::from_value::<agents_core::CreateAgent>(json!({
            "name":"private display name", "instructions":"private instructions and user content"
        }))
        .unwrap()
        .into_config("agent_123".into(), "private-owner-subject".into(), 0);
        let tool = agents_core::RegisteredTool {
            name: "time".into(),
            version: "pinned-v1".into(),
            execution: agents_core::ToolExecution::Harness,
            description: "private tool description".into(),
            parameters: json!({"type":"object"}),
        };
        let record = agents_core::SessionRecord {
            id: "session_123".into(),
            owner_id: agent.owner_id.clone(),
            agent_id: agent.id.clone(),
            status: agents_core::SessionStatus::Running,
            created_at_ms: 0,
            last_input_at_ms: 0,
            input_revision: 1,
            pinned: agents_core::PinnedContext::resolve(&agent, vec![tool], vec![]),
            workspace_snapshot: None,
            runtime_profile: agents_core::RuntimeProfile::Basic1g,
        };
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let fields = sanitize_fields(run_provenance(
            &record,
            "run_123",
            sha,
            "0123456789abcdef0123456789abcdef",
        ));
        assert_eq!(fields["build_id"], sha);
        assert_eq!(fields["agent_id"], "agent_123");
        assert_eq!(fields["session_id"], "session_123");
        assert_eq!(fields["run_id"], "run_123");
        assert_eq!(fields["config_revision"], 1);
        assert_eq!(fields["registry_version"], "pinned-v1");
        assert_eq!(fields["prefix_hash"], record.pinned.prefix_hash());
        assert_eq!(fields["prefix_hash"].as_str().unwrap().len(), 64);
        assert!(!fields.to_string().contains("private"));
        assert!(fields.get("config_hash").is_none());
    }
    #[test]
    fn checkpoint_provenance_retains_only_snapshot_references_and_counters() {
        let snapshot = agents_core::WorkspaceSnapshot {
            generation: 3,
            manifest_key: "private/path/manifest.json".into(),
            sha256: "ab".repeat(32),
            format_version: 1,
            created_at_ms: 0,
            logical_bytes: 123,
            archive_bytes: 45,
            entries: 2,
        };
        let fields = sanitize_fields(checkpoint_provenance(
            "session_123",
            Some("run_123"),
            &snapshot,
        ));
        assert_eq!(
            fields,
            json!({"session_id":"session_123","run_id":"run_123",
            "snapshot_id":"ab".repeat(32),"workspace_revision":"3","snapshot_bytes":45})
        );
        let maintenance = sanitize_fields(checkpoint_provenance("session_123", None, &snapshot));
        assert!(maintenance.get("run_id").is_none());
        assert!(!maintenance.to_string().contains("private"));
    }
    #[test]
    fn traceparent_validation() {
        assert!(TraceContext::parse(VALID).is_some());
        for bad in [
            VALID.to_uppercase(),
            VALID.replace("4bf92f3577b34da6a3ce929d0e0e4736", &"0".repeat(32)),
            format!("{VALID}-extra"),
            VALID.replace("00f067aa0ba902b7", &"0".repeat(16)),
            VALID.replacen("00-", "ff-", 1),
            "😺".repeat(40),
        ] {
            assert!(TraceContext::parse(&bad).is_none(), "{bad}");
        }
        assert!(TraceContext::parse(&VALID.replacen("00-", "01-", 1)).is_some());
    }
    #[test]
    fn child_keeps_trace_but_changes_span() {
        let parent = TraceContext::from_traceparent(Some(VALID));
        let child = parent.child();
        assert_eq!(parent.trace_id(), child.trace_id());
        assert_ne!(parent.traceparent(), child.traceparent());
        assert!(TraceContext::parse(&child.traceparent()).is_some());
    }
    #[test]
    fn all_truncations_and_non_ascii_positions_are_rejected_without_panicking() {
        for len in 0..VALID.len() {
            assert!(TraceContext::parse(&VALID[..len]).is_none());
        }
        for position in 0..VALID.len() {
            let mut bad = VALID.to_string();
            bad.replace_range(position..position + 1, "😺");
            assert!(TraceContext::parse(&bad).is_none());
        }
        for flags in 0..=255 {
            let header = format!("{}{:02x}", &VALID[..53], flags);
            let context = TraceContext::from_traceparent(Some(&header));
            assert!(
                context
                    .traceparent()
                    .ends_with(if flags & 1 == 1 { "01" } else { "00" })
            );
            assert_eq!(context.trace_id(), &VALID[3..35]);
        }
    }
    #[test]
    fn unknown_or_freeform_fields_never_enter_logs() {
        assert_eq!(event_name("observer.completed"), "observer.completed");
        assert_eq!(event_name("observer.recovery"), "observer.recovery");
        assert_eq!(
            event_name("session.projection_pending"),
            "session.projection_pending"
        );
        assert_eq!(event_name("private event content"), "operation");
        let result = sanitize_fields(json!({"api_key":"secret", "command":"echo secret",
            "error":"user content", "duration_ms":12,"session_id":"ses_123",
            "error_code":"error with secret", "bytes":-1,"tool":"bash"}));
        assert_eq!(
            result,
            json!({"duration_ms":12,"session_id":"ses_123","tool":"bash"})
        );
    }
    #[test]
    fn diagnostics_require_consent_and_reject_unknown_fields() {
        let mut value =
            json!({"consent":false,"client":"magents","version":"0.1.0","platform":"linux"});
        let submission: DiagnosticsSubmission = serde_json::from_value(value.clone()).unwrap();
        assert!(submission.validate().is_err());
        value["consent"] = json!(true);
        assert!(
            serde_json::from_value::<DiagnosticsSubmission>(value.clone())
                .unwrap()
                .validate()
                .is_ok()
        );
        value["logs"] = json!("private data");
        assert!(serde_json::from_value::<DiagnosticsSubmission>(value).is_err());
    }
    #[test]
    fn diagnostic_cap_is_independent_of_chunk_boundaries() {
        let data = vec![b'x'; MAX_DIAGNOSTIC_BYTES];
        for chunk_size in [1, 7, 512, MAX_DIAGNOSTIC_BYTES - 1, MAX_DIAGNOSTIC_BYTES] {
            let mut body = Vec::new();
            for chunk in data.chunks(chunk_size) {
                assert!(append_diagnostic_chunk(&mut body, chunk));
            }
            assert_eq!(body, data);
            assert!(!append_diagnostic_chunk(&mut body, b"x"));
            assert_eq!(body.len(), MAX_DIAGNOSTIC_BYTES);
        }
        assert!(!append_diagnostic_chunk(
            &mut Vec::new(),
            &vec![0; MAX_DIAGNOSTIC_BYTES + 1]
        ));
    }
}

#[cfg(test)]
mod document_diagnostic_tests {
    use super::*;
    #[test]
    fn native_document_fields_keep_http_and_parser_evidence_without_content() {
        let safe = sanitize_fields(
            json!({"codec":"application/pdf","request_id":"native-1","ray_id":"abc-SIN","status":422,"response_bytes":127,"input_sha256":"abcd","response_sha256":"ef01","body_complete":true,"upstream_error_code":10042,"parser_line":3,"parser_column":9,"body":"private document","authorization":"Bearer secret"}),
        );
        assert_eq!(safe["status"], 422);
        assert_eq!(safe["codec"], "application/pdf");
        assert_eq!(safe["parser_line"], 3);
        assert_eq!(safe["body_complete"], true);
        assert!(safe.get("body").is_none());
        assert!(safe.get("authorization").is_none());
    }
}
