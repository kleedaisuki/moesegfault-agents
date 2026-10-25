use serde_json::json;
use worker::{Env, Response, Result};

pub fn build_id(env: &Env) -> String {
    let configured = env.var("BUILD_ID").ok().map(|value| value.to_string());
    crate::telemetry::validated_build_id(configured.as_deref()).to_owned()
}

pub fn health(env: &Env) -> Result<Response> {
    Response::from_json(&json!({"status":"ok","build_id":build_id(env)}))
}

pub fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

pub fn alarm_at(epoch_ms: u64) -> worker::ScheduledTime {
    // worker-rs interprets an i64 as a relative offset, unlike the JS API's
    // numeric epoch. An explicit Date preserves absolute deadline semantics.
    worker::ScheduledTime::new(js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(
        epoch_ms as f64,
    )))
}

pub fn json_error(status: u16, code: &str, message: &str) -> Result<Response> {
    Ok(Response::from_json(&error_body(code, message))?.with_status(status))
}

pub(crate) fn error_body(code: &str, message: &str) -> serde_json::Value {
    json!({"error":{"message":message,"type":"agents_error","code":code}})
}

pub fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

pub fn emit(event: &str, fields: serde_json::Value) {
    // Never include user content, credentials, command text, or tool output.
    crate::telemetry::emit(event, fields);
}
