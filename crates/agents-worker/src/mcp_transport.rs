//! Native Workers transport; rmcp owns the protocol envelopes and result DTOs.
//! A prepared request is consumed once after its caller's durable dispatch fence.
use std::{collections::BTreeMap, time::Duration};

use eventsource_stream::Eventsource;
use futures::{StreamExt, future::Either};
use rmcp::model::{CallToolResult, ErrorCode, JsonRpcError, JsonRpcResponse, ListToolsResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{
    AbortController, Delay, Env, Fetch, Headers, Method, Request, RequestInit, RequestRedirect,
    Result,
};

use crate::mcp_bindings::{Protocol, ServerBinding, fail};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 512 * 1024;
const HEADER_TIMEOUT_MS: u64 = 30_000;
const STALL_TIMEOUT_MS: u64 = 90_000;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RpcFailure {
    pub code: String,
    pub http_status: Option<u16>,
    pub protocol_code: Option<i32>,
    pub outcome_unknown: bool,
}

impl RpcFailure {
    pub(crate) fn permits_legacy_initialize(&self, method: &str) -> bool {
        if method != "server/discover" || self.outcome_unknown {
            return false;
        }
        if self.protocol_code.is_some_and(modern_error) {
            return false;
        }
        self.http_status == Some(400)
            || self.protocol_code == Some(-32601)
            || (self.protocol_code.is_none() && matches!(self.http_status, Some(404 | 405)))
    }

    pub(crate) fn public(&self) -> Value {
        if let Some(code) = self.protocol_code {
            return json!({"type":"mcp_protocol_error","code":code,"message":"Remote MCP protocol error"});
        }
        match self.http_status {
            Some(code) => json!({"type":"http_error","code":code,"message":self.code}),
            None => json!({"type":"mcp_protocol_error","code":-32000,"message":self.code}),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RpcOutcome {
    pub result: Option<Value>,
    pub failure: Option<RpcFailure>,
    pub session_id: Option<String>,
    /// Decoded JSON/SSE data payload bytes, not a network-invoice measurement.
    pub payload_bytes: u64,
    pub started_ms: u64,
    pub ended_ms: u64,
}

/// Not Clone, Debug or Serialize: plaintext credentials only live across dispatch.
pub(crate) struct PreparedRpc {
    endpoint: String,
    headers: BTreeMap<String, String>,
    body: Option<String>,
    method: Method,
    id: String,
    side_effect: bool,
}

struct AbortOnDrop(Option<AbortController>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(controller) = self.0.take() {
            controller.abort();
        }
    }
}

pub(crate) fn prepare(
    env: &Env,
    owner: &str,
    server: &ServerBinding,
    method: &str,
    mut params: Value,
    id: &str,
    schema: Option<&Value>,
) -> Result<PreparedRpc> {
    if id.is_empty() || id.len() > 512 || !params.is_object() {
        return Err(fail("invalid_mcp_rpc"));
    }
    let mut headers = server.headers(owner, &env.secret("DATA_ENCRYPTION_KEY")?.to_string())?;
    headers.insert(
        "accept".into(),
        "application/json, text/event-stream".into(),
    );
    headers.insert("content-type".into(), "application/json".into());
    // Legacy initialization offers its version in the body, before negotiation.
    if method != "initialize" {
        headers.insert(
            "mcp-protocol-version".into(),
            server.protocol.version().into(),
        );
    }
    if let Some(session) = &server.session_id {
        if server.protocol != Protocol::Current {
            headers.insert("mcp-session-id".into(), session.clone());
        }
    }
    if server.protocol == Protocol::Current {
        headers.insert("mcp-method".into(), method.into());
        if let Some(name) = params
            .get("name")
            .or_else(|| params.get("uri"))
            .and_then(Value::as_str)
        {
            headers.insert("mcp-name".into(), crate::mcp_headers::encode(name));
        }
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion":server.protocol.version(),
            "io.modelcontextprotocol/clientInfo":{"name":"magents","version":env!("CARGO_PKG_VERSION")},
            "io.modelcontextprotocol/clientCapabilities":{},"progressToken":id,
        });
        if let Some(schema) = schema {
            let projected = crate::mcp_headers::discover(schema).map_err(fail)?;
            for (name, value) in
                crate::mcp_headers::extract(&projected, &params["arguments"]).map_err(fail)?
            {
                headers.insert(name, value);
            }
        }
    } else {
        params["_meta"] = json!({"progressToken":id});
    }
    let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
    if body.len() > MAX_REQUEST_BYTES {
        return Err(fail("mcp_request_limit"));
    }
    Ok(PreparedRpc {
        endpoint: server.url.clone(),
        headers,
        body: Some(body),
        method: Method::Post,
        id: id.into(),
        side_effect: method == "tools/call",
    })
}

pub(crate) fn notification(env: &Env, owner: &str, server: &ServerBinding) -> Result<PreparedRpc> {
    let mut prepared = prepare(
        env,
        owner,
        server,
        "notifications/initialized",
        json!({}),
        "initialized",
        None,
    )?;
    prepared.body = Some(json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string());
    prepared.id.clear();
    Ok(prepared)
}

impl PreparedRpc {
    pub(crate) async fn dispatch_cancellable(
        self,
        cancelled: impl std::future::Future<Output = ()>,
    ) -> Result<RpcOutcome> {
        let started = timestamp();
        match futures::future::select(Box::pin(cancelled), Box::pin(self.dispatch())).await {
            Either::Left(_) => Ok(failed("mcp_cancelled", None, true, started)),
            Either::Right((result, _)) => result,
        }
    }
    pub(crate) async fn dispatch(self) -> Result<RpcOutcome> {
        let started = timestamp();
        let controller = AbortOnDrop(Some(AbortController::default()));
        let headers = Headers::new();
        for (name, value) in &self.headers {
            headers.set(name, value)?;
        }
        let mut init = RequestInit::new();
        init.with_method(self.method)
            .with_headers(headers)
            .with_redirect(RequestRedirect::Manual);
        if let Some(body) = self.body {
            init.with_body(Some(body.into()));
        }
        let request = Request::new_with_init(&self.endpoint, &init)?;
        let fetch = Fetch::Request(request);
        let signal = controller.0.as_ref().unwrap().signal();
        let send = fetch.send_with_signal(&signal);
        let mut response = match timed(send, HEADER_TIMEOUT_MS).await {
            Ok(response) => response,
            Err(_) => {
                return Ok(failed(
                    "mcp_transport_unknown",
                    None,
                    self.side_effect,
                    started,
                ));
            }
        };
        let status = response.status_code();
        let session_id = response.headers().get("mcp-session-id")?.filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
        });
        if self.id.is_empty() {
            return Ok(if status == 202 {
                RpcOutcome {
                    result: Some(json!({})),
                    failure: None,
                    session_id,
                    payload_bytes: 0,
                    started_ms: started,
                    ended_ms: timestamp(),
                }
            } else {
                failed("mcp_notification_rejected", Some(status), false, started)
            });
        }
        let content_type = response.headers().get("content-type")?.unwrap_or_default();
        if !(200..300).contains(&status) && !matches!(status, 400 | 404) {
            return Ok(failed(
                "mcp_http_error",
                Some(status),
                self.side_effect,
                started,
            ));
        }
        let stream = response
            .stream()
            .map_err(|_| fail("mcp_transport_unknown"))?;
        let counted = futures::stream::unfold(
            (stream, 0usize, false),
            |(mut stream, bytes, stopped)| async move {
                if stopped {
                    return None;
                }
                let chunk = match timed_next(stream.next()).await {
                    Ok(Some(Ok(chunk))) => chunk,
                    Ok(None) => return None,
                    _ => return Some((Err(fail("mcp_transport_stalled")), (stream, bytes, true))),
                };
                if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes) {
                    return Some((Err(fail("mcp_response_limit")), (stream, bytes, true)));
                }
                let bytes = bytes + chunk.len();
                Some((Ok(chunk), (stream, bytes, false)))
            },
        );
        let (value, bytes) = if content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            let mut events = Box::pin(counted.eventsource());
            let mut bytes = 0usize;
            loop {
                let event = match events.next().await {
                    Some(Ok(event)) => event,
                    _ => {
                        return Ok(failed(
                            "mcp_stream_outcome_unknown",
                            Some(status),
                            self.side_effect,
                            started,
                        ));
                    }
                };
                bytes = bytes.saturating_add(event.data.len());
                if event.data.len() > MAX_RESPONSE_BYTES
                    || (!event.event.is_empty() && event.event != "message")
                {
                    return Ok(failed(
                        "mcp_stream_protocol_error",
                        Some(status),
                        self.side_effect,
                        started,
                    ));
                }
                let value: Value = match serde_json::from_str(&event.data) {
                    Ok(value) => value,
                    Err(_) => {
                        return Ok(failed(
                            "mcp_invalid_json",
                            Some(status),
                            self.side_effect,
                            started,
                        ));
                    }
                };
                if value.get("method").is_some() {
                    if !notification_matches(&value, &self.id) {
                        return Ok(failed(
                            "mcp_unexpected_server_request",
                            Some(status),
                            self.side_effect,
                            started,
                        ));
                    }
                    continue;
                }
                break (value, bytes);
            }
        } else if content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        {
            let mut stream = Box::pin(counted);
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => bytes.extend_from_slice(&chunk),
                    Err(_) => {
                        return Ok(failed(
                            "mcp_body_outcome_unknown",
                            Some(status),
                            self.side_effect,
                            started,
                        ));
                    }
                }
            }
            let length = bytes.len();
            let value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => {
                    return Ok(failed(
                        "mcp_invalid_json",
                        Some(status),
                        self.side_effect,
                        started,
                    ));
                }
            };
            (value, length)
        } else {
            return Ok(failed(
                "mcp_invalid_content_type",
                Some(status),
                self.side_effect,
                started,
            ));
        };
        let mut outcome = parse_reply(value, &self.id, self.side_effect, status, started);
        outcome.payload_bytes = bytes as u64;
        outcome.session_id = session_id;
        Ok(outcome)
    }
}

pub(crate) fn parse_reply(
    value: Value,
    id: &str,
    side_effect: bool,
    status: u16,
    started: u64,
) -> RpcOutcome {
    if value.get("result").is_some() && value.get("error").is_some() {
        return failed("mcp_ambiguous_reply", Some(status), side_effect, started);
    }
    if value.get("error").is_some() {
        let error: JsonRpcError = match serde_json::from_value(value) {
            Ok(error) => error,
            Err(_) => return failed("mcp_invalid_reply", Some(status), side_effect, started),
        };
        if error
            .id
            .as_ref()
            .and_then(|id| serde_json::to_value(id).ok())
            != Some(json!(id))
        {
            let mut outcome = failed(
                "mcp_reply_identity_mismatch",
                Some(status),
                side_effect,
                started,
            );
            // Modern transport validation may reject before assigning a request ID.
            // Preserve only the typed discriminator, never the remote diagnostics.
            if status == 400 && modern_error(error.error.code.0) {
                outcome.failure.as_mut().unwrap().protocol_code = Some(error.error.code.0);
            }
            return outcome;
        }
        let mut outcome = failed("mcp_protocol_error", Some(status), false, started);
        outcome.failure.as_mut().unwrap().protocol_code = Some(error.error.code.0);
        return outcome;
    }
    let response: JsonRpcResponse<Value> = match serde_json::from_value(value) {
        Ok(response) => response,
        Err(_) => return failed("mcp_invalid_reply", Some(status), side_effect, started),
    };
    if serde_json::to_value(response.id).ok() != Some(json!(id))
        || !response.result.is_object()
        || !(200..300).contains(&status)
    {
        return failed(
            "mcp_reply_identity_mismatch",
            Some(status),
            side_effect,
            started,
        );
    }
    RpcOutcome {
        result: Some(response.result),
        failure: None,
        session_id: None,
        payload_bytes: 0,
        started_ms: started,
        ended_ms: timestamp(),
    }
}

fn modern_error(code: i32) -> bool {
    [
        ErrorCode::HEADER_MISMATCH.0,
        ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY.0,
        ErrorCode::UNSUPPORTED_PROTOCOL_VERSION.0,
    ]
    .contains(&code)
}

pub(crate) fn tools(value: Value) -> Result<(Vec<rmcp::model::Tool>, Option<String>)> {
    let result: ListToolsResult =
        serde_json::from_value(value).map_err(|_| fail("mcp_invalid_tool_list"))?;
    if result.tools.len() > crate::mcp_bindings::MAX_TOOLS
        || result
            .next_cursor
            .as_ref()
            .is_some_and(|value| value.len() > 4096)
    {
        return Err(fail("mcp_tool_limit"));
    }
    Ok((result.tools, result.next_cursor))
}

pub(crate) fn tool_result(value: Value) -> Result<(String, bool)> {
    let result: CallToolResult =
        serde_json::from_value(value).map_err(|_| fail("mcp_invalid_tool_result"))?;
    let error = result.is_error == Some(true);
    // Transport metadata is private. Preserve the typed content and structured result.
    let mut value = json!({"content":result.content,"isError":error});
    if let Some(structured) = result.structured_content {
        value["structuredContent"] = structured;
    }
    let output = value.to_string();
    if output.len() > crate::mcp_items::MAX_OUTPUT_BYTES {
        return Err(fail("mcp_output_limit"));
    }
    Ok((output, error))
}

/// Model content is typed, not base64 hidden inside an ordinary text result.
/// User-only content and protocol `_meta` never become model input.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ModelContent(Vec<Value>);
impl ModelContent {
    pub(crate) fn into_value(self) -> Value {
        Value::Array(self.0)
    }
}
pub(crate) fn tool_content(output: &str) -> Result<ModelContent> {
    let value: Value = serde_json::from_str(output).map_err(|_| fail("mcp_invalid_tool_result"))?;
    let mut parts = Vec::new();
    for part in value["content"].as_array().into_iter().flatten() {
        if part
            .pointer("/annotations/audience")
            .and_then(Value::as_array)
            .is_some_and(|audience| !audience.iter().any(|role| role == "assistant"))
        {
            continue;
        }
        match part["type"].as_str() {
            Some("text") => parts.push(json!({"type":"text","text":part["text"]})),
            Some("image") => {
                let encoded = part["data"].as_str().ok_or_else(|| fail("mcp_invalid_image"))?;
                let raster = crate::image_generation::Raster::from_base64(encoded).map_err(|_| fail("mcp_invalid_image"))?;
                if !part["mimeType"].as_str().is_some_and(|mime| mime.eq_ignore_ascii_case(raster.format().mime())) {
                    return Err(fail("mcp_invalid_image"));
                }
                parts.push(json!({"type":"image_url","image_url":{"url":raster.data_url(),"detail":"auto"}}));
            }
            Some("resource") => {
                let resource = &part["resource"];
                if let Some(text) = resource["text"].as_str() {
                    parts.push(json!({"type":"text","text":format!("Untrusted MCP resource {}:\n{text}", resource["uri"])}));
                } else { return Err(fail("mcp_result_modality_unavailable")); }
            }
            Some("resource_link") => parts.push(json!({"type":"text","text":json!({"type":"resource_link","uri":part["uri"],"name":part["name"],"description":part["description"],"mimeType":part["mimeType"],"size":part["size"]}).to_string()})),
            _ => return Err(fail("mcp_result_modality_unavailable")),
        }
    }
    if let Some(structured) = value.get("structuredContent") {
        parts.push(
            json!({"type":"text","text":format!("Untrusted MCP structured result:\n{structured}")}),
        );
    }
    if parts.is_empty() {
        parts.push(json!({"type":"text","text":"The MCP tool returned no model-visible content."}));
    }
    Ok(ModelContent(parts))
}
fn notification_matches(value: &Value, id: &str) -> bool {
    if value["jsonrpc"] != "2.0" || value.get("id").is_some() {
        return false;
    }
    match value["method"].as_str() {
        Some("notifications/progress") => value["params"]["progressToken"] == id,
        Some("notifications/message") => true,
        _ => false,
    }
}

async fn timed<T>(future: impl std::future::Future<Output = Result<T>>, millis: u64) -> Result<T> {
    match futures::future::select(
        Box::pin(future),
        Box::pin(Delay::from(Duration::from_millis(millis))),
    )
    .await
    {
        Either::Left((value, _)) => value,
        Either::Right(_) => Err(fail("mcp_transport_stalled")),
    }
}

async fn timed_next<T>(future: impl std::future::Future<Output = Option<T>>) -> Result<Option<T>> {
    timed(async move { Ok(future.await) }, STALL_TIMEOUT_MS).await
}

fn failed(code: &str, http_status: Option<u16>, outcome_unknown: bool, started: u64) -> RpcOutcome {
    RpcOutcome {
        result: None,
        failure: Some(RpcFailure {
            code: code.into(),
            http_status,
            protocol_code: None,
            outcome_unknown,
        }),
        session_id: None,
        payload_bytes: 0,
        started_ms: started,
        ended_ms: timestamp(),
    }
}

fn timestamp() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        crate::util::now_ms()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_negotiation_is_metadata_only_and_modern_errors_never_downgrade() {
        for code in [None, Some(-32600), Some(-32601)] {
            let mut outcome = failed("mcp_protocol_error", Some(400), false, 0);
            let failure = outcome.failure.as_mut().unwrap();
            failure.protocol_code = code;
            assert!(failure.permits_legacy_initialize("server/discover"));
            for method in ["initialize", "tools/list", "tools/call", "unknown/effect"] {
                assert!(!failure.permits_legacy_initialize(method));
            }
            failure.outcome_unknown = true;
            assert!(!failure.permits_legacy_initialize("server/discover"));
        }
        for code in [-32020, -32021, -32022] {
            for id in [json!("probe"), Value::Null] {
                let outcome = parse_reply(
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":"private"}}),
                    "probe",
                    false,
                    400,
                    0,
                );
                let failure = outcome.failure.unwrap();
                assert_eq!(failure.protocol_code, Some(code));
                assert!(!failure.permits_legacy_initialize("server/discover"));
            }
        }
    }

    #[test]
    fn typed_mcp_content_preserves_images_resources_and_audience_without_binary_text() {
        let image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aOSkAAAAASUVORK5CYII=";
        let output = json!({"content":[
            {"type":"text","text":"visible"},
            {"type":"text","text":"user-only","annotations":{"audience":["user"]}},
            {"type":"image","mimeType":"image/png","data":image},
            {"type":"resource","resource":{"uri":"memo:owned","text":"memo text"}},
            {"type":"resource_link","uri":"https://example.com/report","name":"Report"}
        ],"structuredContent":{"value":7}})
        .to_string();
        let content = tool_content(&output).unwrap().into_value();
        let parts = content.as_array().unwrap();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[1]["type"], "image_url");
        assert!(
            parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .all(|text| !text.contains(image) && !text.contains("user-only"))
        );
        assert!(
            tool_content(
                &json!({"content":[{"type":"audio","mimeType":"audio/wav","data":"AAAA"}]})
                    .to_string()
            )
            .is_err()
        );
    }

    #[test]
    fn rpc_identity_and_ambiguous_terminal_never_authorize_retry() {
        let wrong = parse_reply(
            json!({"jsonrpc":"2.0","id":"foreign","result":{}}),
            "owned",
            true,
            200,
            0,
        );
        assert!(wrong.failure.unwrap().outcome_unknown);
        let ambiguous = parse_reply(
            json!({"jsonrpc":"2.0","id":"owned","result":{},"error":{"code":1,"message":"private-key"}}),
            "owned",
            true,
            200,
            0,
        );
        assert!(ambiguous.failure.unwrap().outcome_unknown);
        let valid = parse_reply(
            json!({"jsonrpc":"2.0","id":"owned","result":{"content":[]}}),
            "owned",
            true,
            200,
            0,
        );
        assert!(valid.failure.is_none());
    }

    #[test]
    fn remote_errors_are_safe_codes_not_credential_diagnostics() {
        let failed = parse_reply(
            json!({"jsonrpc":"2.0","id":"owned","error":{"code":-32601,"message":"echo-private-secret"}}),
            "owned",
            false,
            404,
            0,
        );
        let public = failed.failure.unwrap().public().to_string();
        assert!(!public.contains("private-secret"));
        assert!(public.contains("-32601"));
    }

    #[test]
    fn heartbeats_and_own_progress_do_not_become_tool_results() {
        assert!(notification_matches(
            &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"id","progress":1}}),
            "id"
        ));
        assert!(!notification_matches(
            &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"foreign"}}),
            "id"
        ));
        assert!(!notification_matches(
            &json!({"jsonrpc":"2.0","method":"sampling/createMessage","id":1}),
            "id"
        ));
        assert!(!notification_matches(
            &json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
            "id"
        ));
    }
}
