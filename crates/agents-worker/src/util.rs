use serde_json::json;
use worker::{Response, Result};

pub fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

pub fn json_error(status: u16, code: &str, message: &str) -> Result<Response> {
    Ok(Response::from_json(
        &json!({"error":{"message":message,"type":"agents_error","code":code}}),
    )?
    .with_status(status))
}

pub fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

pub fn emit(event: &str, fields: serde_json::Value) {
    // Never include user content, credentials, command text, or tool output.
    crate::telemetry::emit(event, fields);
}
