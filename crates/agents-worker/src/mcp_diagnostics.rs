//! Structural evidence survives protocol/catalog rejection. Credentials and
//! remote descriptions stay outside telemetry, not HTTP status and error codes.
use std::{cell::Cell, rc::Rc};

use serde_json::json;
use worker::{Error, Result};

use super::{PreparedRpc, RpcOutcome, timestamp};

pub(super) struct RpcTrace {
    server: String,
    method: String,
    request: String,
    started: u64,
    pub phase: &'static str,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub request_id: Option<String>,
    pub ray_id: Option<String>,
    pub bytes: Rc<Cell<u64>>,
    error: Option<String>,
    protocol_error: Option<i32>,
    error_type: Option<&'static str>,
    unknown: bool,
}

impl RpcTrace {
    pub(super) fn new(request: &PreparedRpc) -> Self {
        Self {
            server: request.server_label.clone(),
            method: request.rpc_method.clone(),
            request: request.id.clone(),
            started: timestamp(),
            phase: "prepare_request",
            status: None,
            content_type: None,
            request_id: None,
            ray_id: None,
            bytes: Rc::new(Cell::new(0)),
            error: Some("mcp_transport_incomplete".into()),
            protocol_error: None,
            error_type: None,
            unknown: request.side_effect,
        }
    }

    pub(super) fn finish(&mut self, outcome: &Result<RpcOutcome>) {
        match outcome {
            Ok(outcome) => {
                self.error = outcome.failure.as_ref().map(|f| f.code.clone());
                self.protocol_error = outcome.failure.as_ref().and_then(|f| f.protocol_code);
                self.unknown = outcome.failure.as_ref().is_some_and(|f| f.outcome_unknown);
                if self.error.is_none() {
                    self.phase = "complete";
                }
            }
            Err(error) => {
                self.error = Some(error_code(error).into());
                self.error_type = Some(error_type(error));
            }
        }
    }
}

impl Drop for RpcTrace {
    fn drop(&mut self) {
        crate::util::emit(
            "mcp.transport",
            json!({
                "server_label":self.server,"mcp_method":self.method,"request_id":self.request,
                "phase":self.phase,"status":self.status,"content_type":self.content_type,
                "provider_request_id":self.request_id,"ray_id":self.ray_id,
                "response_bytes":self.bytes.get(),"duration_ms":timestamp().saturating_sub(self.started),
                "error_code":self.error,"mcp_protocol_error_code":self.protocol_error,
                "error_type":self.error_type,"outcome_unknown":self.unknown,
            }),
        );
    }
}

pub(super) fn error_code(error: &Error) -> &str {
    match error {
        Error::RustError(code)
            if code.len() <= 128
                && (code.starts_with("mcp_") || code.starts_with("invalid_mcp_"))
                && code
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_') =>
        {
            code
        }
        _ => "mcp_transport_error",
    }
}

pub(super) fn error_type(error: &Error) -> &'static str {
    match error {
        Error::RustError(_) => "RustError",
        Error::JsError(_) => "JsError",
        Error::Internal(_) => "Internal",
        Error::BindingError(_) => "BindingError",
        Error::SerdeJsonError(_) => "SerdeJsonError",
        _ => "WorkerError",
    }
}

pub(super) fn header(value: Option<String>) -> Option<String> {
    value.filter(|v| {
        !v.is_empty()
            && v.len() <= 128
            && v.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.:/-".contains(&c))
    })
}
