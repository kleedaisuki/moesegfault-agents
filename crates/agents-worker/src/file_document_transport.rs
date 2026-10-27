//! The official native AI SDK's transformer envelope over its own binding.fetch.
//! Unlike toMarkdown(), this boundary retains HTTP metadata before decoding;
//! toMarkdown() currently throws an AiInternalError that discards status/headers.
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use futures::{StreamExt, future::Either};
use js_sys::{Function, Promise, Reflect};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::{Env, Response, Result};

use super::{Kind, TEXT_LIMIT, fail};
const ENDPOINT: &str = "https://workers-binding.ai/to-everything/markdown/transformer";
const RESPONSE_LIMIT: usize = TEXT_LIMIT * 6 + 4096;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Diagnostics {
    pub phase: String,
    pub codec: String,
    pub input_bytes: usize,
    pub input_sha256: String,
    pub request_bytes: usize,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub declared_response_bytes: Option<u64>,
    pub request_id: Option<String>,
    pub ray_id: Option<String>,
    pub response_bytes: Option<usize>,
    pub response_sha256: Option<String>,
    pub upstream_error_code: Option<u64>,
    pub error_type: Option<String>,
    pub elapsed_ms: u64,
    pub body_complete: bool,
    pub parser_line: Option<usize>,
    pub parser_column: Option<usize>,
}

pub(super) struct Prepared {
    ai: worker::Ai,
    method: Function,
    request: web_sys::Request,
    diagnostics: Diagnostics,
    kind: Kind,
}

pub(super) struct Observation {
    pub diagnostics: Diagnostics,
    pub body: Option<Vec<u8>>,
    kind: Kind,
}

fn request_body(kind: Kind, bytes: &[u8]) -> Result<String> {
    // Same envelope as workerd's ToMarkdownService.transform. Do not route this
    // internal endpoint through global Fetch or attach an external API token.
    // https://github.com/cloudflare/workerd/blob/main/src/cloudflare/internal/to-markdown-api.ts
    Ok(
        json!({"files":[{"name":kind.name(),"mimeType":kind.mime(),"data":STANDARD.encode(bytes)}],
        "options":{"conversionOptions":{"output":{"format":"text"},
            "pdf":{"metadata":false,"images":{"convert":false}},
            "docx":{"images":{"convert":false}}}}})
        .to_string(),
    )
}

pub(super) fn prepare(env: &Env, binding: &str, kind: Kind, bytes: &[u8]) -> Result<Prepared> {
    let ai = env.ai(binding)?;
    let method = Reflect::get(ai.as_ref(), &"fetch".into())
        .map_err(|_| fail("file_search_document_service_unavailable"))?
        .dyn_into::<Function>()
        .map_err(|_| fail("file_search_document_service_unavailable"))?;
    let body = request_body(kind, bytes)?;
    let diagnostics = Diagnostics {
        phase: "prepared".into(),
        codec: kind.mime().into(),
        input_bytes: bytes.len(),
        input_sha256: format!("{:x}", Sha256::digest(bytes)),
        request_bytes: body.len(),
        status: None,
        content_type: None,
        content_encoding: None,
        declared_response_bytes: None,
        request_id: None,
        ray_id: None,
        response_bytes: None,
        response_sha256: None,
        upstream_error_code: None,
        error_type: None,
        elapsed_ms: 0,
        body_complete: false,
        parser_line: None,
        parser_column: None,
    };
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_body(&JsValue::from_str(&body));
    let headers = worker::Headers::new();
    headers.set("Content-Type", "application/json")?;
    init.set_headers(headers.as_ref());
    let request = web_sys::Request::new_with_str_and_init(ENDPOINT, &init)?;
    Ok(Prepared {
        ai,
        method,
        request,
        diagnostics,
        kind,
    })
}

fn safe_id(value: Option<String>) -> Option<String> {
    value.filter(|value| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.:/".contains(&b))
    })
}

fn js_error(error: &JsValue) -> String {
    let name = Reflect::get(error, &"name".into())
        .ok()
        .and_then(|value| value.as_string());
    match name.as_deref() {
        Some(
            "TypeError" | "RangeError" | "AbortError" | "Error" | "AiInternalError"
            | "DOMException",
        ) => name.unwrap(),
        _ => "OtherJsError".into(),
    }
}

async fn dispatch(prepared: &Prepared, diagnostics: &mut Diagnostics) -> Option<Response> {
    diagnostics.phase = "dispatch".into();
    let result = match prepared
        .method
        .call1(prepared.ai.as_ref(), prepared.request.as_ref())
    {
        Ok(result) => result,
        Err(error) => {
            diagnostics.error_type = Some(js_error(&error));
            return None;
        }
    };
    diagnostics.phase = "promise".into();
    let promise = match result.dyn_into::<Promise>() {
        Ok(promise) => promise,
        Err(_) => {
            diagnostics.error_type = Some("NonPromise".into());
            return None;
        }
    };
    let response = match JsFuture::from(promise).await {
        Ok(response) => response,
        Err(error) => {
            diagnostics.error_type = Some(js_error(&error));
            return None;
        }
    };
    diagnostics.phase = "response_headers".into();
    match response.dyn_into::<web_sys::Response>() {
        Ok(response) => Some(response.into()),
        Err(_) => {
            diagnostics.error_type = Some("NonResponse".into());
            None
        }
    }
}

async fn receive(prepared: &Prepared, diagnostics: &mut Diagnostics) -> Option<Vec<u8>> {
    let mut response = dispatch(prepared, diagnostics).await?;
    diagnostics.status = Some(response.status_code());
    diagnostics.request_id = safe_id(response.headers().get("x-request-id").ok().flatten())
        .or_else(|| safe_id(response.headers().get("cf-ai-req-id").ok().flatten()));
    diagnostics.ray_id = safe_id(response.headers().get("cf-ray").ok().flatten());
    diagnostics.content_type = safe_id(response.headers().get("content-type").ok().flatten().map(
        |value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        },
    ));
    diagnostics.content_encoding =
        safe_id(response.headers().get("content-encoding").ok().flatten());
    diagnostics.declared_response_bytes = response
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok());
    if diagnostics
        .declared_response_bytes
        .is_some_and(|size| size > RESPONSE_LIMIT as u64)
    {
        diagnostics.error_type = Some("ResponseTooLarge".into());
        return None;
    }
    diagnostics.phase = "response_body".into();
    let mut stream = match response.stream() {
        Ok(stream) => stream,
        Err(_) => {
            diagnostics.error_type = Some("BodyUnavailable".into());
            return None;
        }
    };
    let mut bytes = vec![];
    let mut digest = Sha256::new();
    diagnostics.response_bytes = Some(0);
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                diagnostics.error_type = Some("BodyInterrupted".into());
                return None;
            }
        };
        if chunk.len() > RESPONSE_LIMIT.saturating_sub(bytes.len()) {
            diagnostics.error_type = Some("ResponseTooLarge".into());
            return None;
        }
        digest.update(&chunk);
        bytes.extend_from_slice(&chunk);
        diagnostics.response_bytes = Some(bytes.len());
        diagnostics.response_sha256 = Some(format!("{:x}", digest.clone().finalize()));
    }
    diagnostics.response_bytes = Some(bytes.len());
    diagnostics.response_sha256 = Some(format!("{:x}", Sha256::digest(&bytes)));
    diagnostics.body_complete = true;
    if diagnostics
        .status
        .is_some_and(|status| !(200..300).contains(&status))
    {
        diagnostics.phase = "http_error".into();
        diagnostics.error_type = Some("UpstreamHttpError".into());
        diagnostics.upstream_error_code = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| value["errors"][0]["code"].as_u64());
    }
    Some(bytes)
}

pub(super) async fn observe(prepared: Prepared) -> Observation {
    let _span = crate::telemetry::NativeSpan::start("tool.document_conversion", None);
    let started = crate::util::now_ms();
    let mut diagnostics = prepared.diagnostics.clone();
    let (body, timed_out) = match futures::future::select(
        Box::pin(receive(&prepared, &mut diagnostics)),
        Box::pin(worker::Delay::from(Duration::from_secs(20))),
    )
    .await
    {
        Either::Left((body, _)) => (body, false),
        Either::Right((_, pending)) => {
            drop(pending);
            (None, true)
        }
    };
    if timed_out {
        diagnostics.error_type = Some("DeadlineExceeded".into());
    }
    diagnostics.elapsed_ms = crate::util::now_ms().saturating_sub(started);
    Observation {
        diagnostics,
        body,
        kind: prepared.kind,
    }
}

#[cfg(test)]
fn decode_body(kind: Kind, bytes: &[u8]) -> Result<String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| fail("file_search_document_json_invalid"))?;
    decode_value(kind, &value)
}

fn decode_value(kind: Kind, value: &Value) -> Result<String> {
    let results = value["result"]
        .as_array()
        .filter(|results| results.len() == 1)
        .ok_or_else(|| fail("file_search_document_result_shape"))?;
    let result = &results[0];
    if result["format"] == "error" {
        return Err(fail("file_search_document_error_result"));
    }
    if result["format"] != "text" {
        return Err(fail("file_search_document_format_mismatch"));
    }
    let mime = result
        .get("mimeType")
        .or_else(|| result.get("mimetype"))
        .and_then(Value::as_str);
    if mime != Some(kind.mime())
        || (result.get("mimeType").is_some()
            && result.get("mimetype").is_some()
            && result["mimeType"] != result["mimetype"])
    {
        return Err(fail("file_search_document_mime_mismatch"));
    }
    let text = result["data"]
        .as_str()
        .ok_or_else(|| fail("file_search_document_text_invalid"))?;
    if text.len() > TEXT_LIMIT || text.contains('\0') || text.trim().is_empty() {
        return Err(fail("file_search_document_text_limit"));
    }
    Ok(text.into())
}

impl Observation {
    pub(super) fn decode(&mut self) -> Result<String> {
        if !self.diagnostics.body_complete {
            return Err(fail("file_search_document_outcome_unknown"));
        }
        if !self
            .diagnostics
            .status
            .is_some_and(|status| (200..300).contains(&status))
        {
            return Err(fail("file_search_document_upstream_rejected"));
        }
        self.diagnostics.phase = "decode_json".into();
        let bytes = self
            .body
            .as_deref()
            .ok_or_else(|| fail("file_search_document_outcome_unknown"))?;
        let value: Value = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(error) => {
                self.diagnostics.parser_line = Some(error.line());
                self.diagnostics.parser_column = Some(error.column());
                self.diagnostics.error_type = Some("JsonDecodeError".into());
                return Err(fail("file_search_document_json_invalid"));
            }
        };
        self.diagnostics.phase = "validate_document".into();
        decode_value(self.kind, &value)
    }

    pub(super) fn emit(&self, operation: Option<&str>, error: Option<&str>) {
        let d = &self.diagnostics;
        crate::util::emit(
            if error.is_some() {
                "provider.failed"
            } else {
                "provider.completed"
            },
            json!({
            "provider":"cloudflare_ai","tool":"document_conversion","operation_id":operation,
            "phase":d.phase,"status":d.status,"request_id":d.request_id,"bytes":d.input_bytes,
            "parser_line":d.parser_line,"parser_column":d.parser_column,"codec":d.codec,"ray_id":d.ray_id,"content_type":d.content_type,"content_encoding":d.content_encoding,
            "request_bytes":d.request_bytes,"response_bytes":d.response_bytes,"declared_response_bytes":d.declared_response_bytes,
            "input_sha256":d.input_sha256,"response_sha256":d.response_sha256,
            "upstream_error_code":d.upstream_error_code,"body_complete":d.body_complete,
            "duration_ms":d.elapsed_ms,"error_type":d.error_type,"error_code":error,"success":error.is_none()}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_sdk_envelope_sets_mime_and_decodes_the_actual_camel_case_result() {
        let bytes = include_bytes!("../../../tests/fixtures/retrieval/text-layer.pdf");
        let request: Value =
            serde_json::from_str(&request_body(Kind::Pdf, bytes).unwrap()).unwrap();
        assert_eq!(request["files"][0]["mimeType"], "application/pdf");
        assert_eq!(
            STANDARD
                .decode(request["files"][0]["data"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        assert_eq!(
            request["options"]["conversionOptions"]["pdf"]["images"]["convert"],
            false
        );
        assert_eq!(decode_body(Kind::Pdf,br#"{"result":[{"mimeType":"application/pdf","format":"text","data":"silver-lantern-513","tokens":999999999}]}"#).unwrap(),"silver-lantern-513");
        assert!(
            decode_body(
                Kind::Pdf,
                br#"{"result":[{"mimeType":"image/png","format":"text","data":"not this type"}]}"#
            )
            .is_err()
        );
        assert!(decode_body(Kind::Pdf,br#"{"result":[{"mimeType":"application/pdf","format":"error","error":"private file data"}]}"#).is_err());
    }
}
