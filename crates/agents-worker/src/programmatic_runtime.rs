//! A platform V8 isolate receives code and data, never a storage, account or
//! provider binding. The caller owns durable actor state and resource funding.

use std::{cell::RefCell, rc::Rc};

use futures::future::Either;
use js_sys::{Array, Function, Promise, Reflect, Uint8Array};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::{
    AbortController, Delay, Env, Error, Headers, Method, RequestInit, ResponseBody, Result,
};

use crate::{
    programmatic_metering::FundedTurn,
    programmatic_tools::{ProgramStart, ProgramState, RuntimeCall},
};

const TEMPLATE: &str = include_str!("programmatic_runtime.js");
const MARKER: &str = "/*__MAGENTS_PROGRAM_BODY__*/";
const MAX_SOURCE: usize = 256 * 1024;
const MAX_BODY: usize = 1024 * 1024;
pub(crate) const LOADER_BINDING: &str = "PROGRAM_LOADER";

/// Freeze the exact module with its owning operation. A deployment must never
/// resume an old fingerprint using a different wrapper under the same cache ID.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeSpec {
    worker_id: String,
    module: String,
    module_sha256: String,
    program_call_id: String,
    cpu_ms: u32,
}

impl RuntimeSpec {
    pub(crate) fn new(program: &ProgramState, cpu_ms: u32) -> Result<Self> {
        if !(1..=100).contains(&cpu_ms) || TEMPLATE.matches(MARKER).count() != 1 {
            return Err(fail("invalid_program_runtime_profile"));
        }
        let body = serde_json::to_string(&format!("'use strict';\n{}", program.code()))
            .map_err(|_| fail("invalid_program_source"))?;
        let module = TEMPLATE.replacen(MARKER, &body, 1);
        if module.len() > MAX_SOURCE {
            return Err(fail("program_source_limit"));
        }
        let module_sha256 = hash(module.as_bytes());
        let worker_id = format!(
            "{}-{}",
            program.worker_id().map_err(fail)?,
            &module_sha256[..16]
        );
        Ok(Self {
            worker_id,
            module,
            module_sha256,
            program_call_id: program.call_id().into(),
            cpu_ms,
        })
    }

    pub(crate) fn worker_id(&self) -> &str {
        &self.worker_id
    }
    pub(crate) fn cpu_limit_ms(&self) -> u32 {
        self.cpu_ms
    }

    pub(crate) fn code_revision(&self) -> &str {
        &self.module_sha256
    }

    fn validate(&self) -> Result<()> {
        if !(1..=100).contains(&self.cpu_ms)
            || self.module.len() > MAX_SOURCE
            || hash(self.module.as_bytes()) != self.module_sha256
            || !self.worker_id.starts_with("program-")
            || self.worker_id.len() > 128
            || !self
                .worker_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            || self.program_call_id.len() > 128
            || !self.program_call_id.starts_with("call_prog_")
        {
            return Err(fail("program_runtime_identity_conflict"));
        }
        Ok(())
    }

    fn worker_code(&self) -> Result<JsValue> {
        self.validate()?;
        // No `nodejs_compat`, bindings or tails are inherited. Omitting
        // globalOutbound would inherit Internet access and is forbidden.
        json!({
            "compatibilityDate":"2026-10-25",
            "compatibilityFlags":["no_nodejs_compat","no_nodejs_compat_v2","allow_eval_during_startup"],
            "mainModule":"program.js",
            "modules":{"program.js":self.module},
            "globalOutbound":null,
            "env":{},
            "tails":[],
            "limits":{"cpuMs":self.cpu_ms,"subRequests":0},
        })
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|_| fail("invalid_program_runtime_profile"))
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeMedia {
    #[serde(rename = "type")]
    pub kind: String,
    pub image_url: String,
}

/// User JavaScript can forge a handler payload; these are proposals, never
/// authority. ProgramState rechecks every call and the actor verifies images.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeReply {
    pub status: String,
    pub program_id: Option<String>,
    #[serde(default)]
    pub calls: Vec<RuntimeCall>,
    pub result: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub media: Vec<RuntimeMedia>,
    // The disabled future replay profile may include this empty field on a
    // fixed failure. Live execution never interprets or applies it.
    #[serde(default)]
    entropy: Vec<Value>,
}

impl RuntimeReply {
    pub(crate) fn validate(&self, program_id: &str) -> Result<()> {
        if self
            .program_id
            .as_deref()
            .is_some_and(|id| id != program_id)
            || self.calls.len() > 64
            || self.media.len() > 8
            || !self.entropy.is_empty()
            || self
                .result
                .as_ref()
                .is_some_and(|result| result.len() > MAX_BODY)
        {
            return Err(fail("invalid_program_runtime_reply"));
        }
        match self.status.as_str() {
            "paused" if !self.calls.is_empty() && self.result.is_none() && self.error.is_none() => {
                Ok(())
            }
            "completed"
                if self.calls.is_empty() && self.result.is_some() && self.error.is_none() =>
            {
                Ok(())
            }
            "failed" if self.calls.is_empty() && self.result.is_none() && self.error.is_some() => {
                Ok(())
            }
            _ => Err(fail("invalid_program_runtime_reply")),
        }
    }

    pub(crate) fn safe_error(&self) -> &'static str {
        match self.error.as_deref() {
            Some("runtime_missing") => "program_runtime_expired",
            Some("program_cancelled") => "program_cancelled",
            Some("program_call_limit") => "program_call_limit",
            Some("program_output_limit" | "program_result_limit" | "program_argument_limit") => {
                "program_value_limit"
            }
            Some("program_conflict" | "program_result_conflict" | "program_result_unknown") => {
                "program_runtime_conflict"
            }
            _ => "program_execution_failed",
        }
    }
}

pub(crate) fn available(env: &Env) -> bool {
    Reflect::get(env.as_ref(), &JsValue::from_str(LOADER_BINDING)).is_ok_and(|binding| {
        !binding.is_null() && !binding.is_undefined() && method(&binding, "get").is_ok()
    })
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeReceipt {
    pub worker_id: String,
    pub code_revision: String,
    pub started_at_ms: u64,
    pub response_at_ms: u64,
}

struct ChannelLease(RefCell<Option<AbortController>>);
impl ChannelLease {
    fn abort(&self) {
        if let Some(controller) = self.0.borrow_mut().take() {
            controller.abort();
        }
    }
}
impl Drop for ChannelLease {
    fn drop(&mut self) {
        self.abort();
    }
}

/// The two halves are separate so an actor may await output while its trusted
/// client-result handler writes input. They must remain in the original live
/// actor I/O invocation; these handles cannot be serialized across eviction.
pub(crate) struct ProgramInput {
    writer: JsValue,
    program_id: String,
    frames: usize,
    lease: Rc<ChannelLease>,
}

pub(crate) struct ProgramOutput {
    reader: JsValue,
    program_id: String,
    buffered: Vec<u8>,
    frames: usize,
    finished: bool,
    valid_frame_seen: bool,
    budget_worker_id: String,
    budget_code_revision: String,
    budget_cpu_ms: u32,
    lease: Rc<ChannelLease>,
}

/// A native single-origin request executed with this configured capacity cap.
/// This is not an observed CPU duration. Neither a caller JSON bool nor a
/// JavaScript error/result object can construct this financial scope witness.
pub(crate) struct EnforcedProgramBudget {
    worker_id: String,
    code_revision: String,
    cpu_limit_ms: u32,
}

impl EnforcedProgramBudget {
    pub(crate) fn worker_id(&self) -> &str {
        &self.worker_id
    }
    pub(crate) fn code_revision(&self) -> &str {
        &self.code_revision
    }
    pub(crate) fn cpu_limit_ms(&self) -> u32 {
        self.cpu_limit_ms
    }
}

pub(crate) struct ProgramChannel {
    pub input: ProgramInput,
    pub output: ProgramOutput,
    /// Persist this before recording request/creation fees. It proves a native
    /// response, not measured CPU, successful JavaScript or a fresh cold start.
    pub receipt: NativeReceipt,
}

pub(crate) enum OpenProgram {
    Streaming(ProgramChannel),
    /// Keep native request evidence even when HTTP/protocol validation fails.
    /// The actor persists it before charging the known request; creation/CPU
    /// uncertainty remains a separate held observation.
    Rejected {
        receipt: NativeReceipt,
        code: &'static str,
    },
}

/// Call only after the actor's immutable start intent and TurnAdmission are
/// durable. There is exactly one native fetch, not one request per tool await.
pub(crate) async fn open_funded(
    env: &Env,
    spec: &RuntimeSpec,
    start: ProgramStart,
    funding: &FundedTurn,
    deadline_ms: u64,
) -> Result<OpenProgram> {
    spec.validate()?;
    if start.message()["op"] != "start"
        || start.message()["program_id"].as_str() != Some(spec.program_call_id.as_str())
        || spec.worker_id != format!("{}-{}", start.worker_identity(), &spec.module_sha256[..16])
    {
        return Err(fail("program_runtime_identity_conflict"));
    }
    funding
        .verify_before_dispatch(
            env,
            spec.worker_id(),
            spec.code_revision(),
            spec.cpu_limit_ms(),
            crate::util::now_ms(),
        )
        .await?;
    // No await follows these checks before the Loader/fetch platform effects.
    funding.verify_dispatch_time(crate::util::now_ms())?;
    start.verify_time(crate::util::now_ms()).map_err(fail)?;
    let loader = Reflect::get(env.as_ref(), &JsValue::from_str(LOADER_BINDING))
        .map_err(|_| fail("program_runtime_unavailable"))?;
    let code = spec.worker_code()?;
    let promise_constructor = Reflect::get(&js_sys::global(), &JsValue::from_str("Promise"))
        .map_err(|_| fail("program_runtime_unavailable"))?;
    // A native bound callback remains valid if the loader retains it for a cold
    // start; a dropped wasm closure would not.
    let callback = method(&promise_constructor, "resolve")?.bind1(&promise_constructor, &code);
    let stub = method(&loader, "get")?
        .call2(&loader, &JsValue::from_str(spec.worker_id()), &callback)
        .map_err(|_| fail("program_runtime_outcome_unknown"))?;
    let entrypoint = method(&stub, "getEntrypoint")?
        .call0(&stub)
        .map_err(|_| fail("program_runtime_outcome_unknown"))?;
    let constructor = Reflect::get(&js_sys::global(), &JsValue::from_str("TransformStream"))
        .map_err(|_| fail("program_runtime_unavailable"))?
        .dyn_into::<Function>()
        .map_err(|_| fail("program_runtime_unavailable"))?;
    let pipe = Reflect::construct(&constructor, &Array::new())
        .map_err(|_| fail("program_runtime_unavailable"))?;
    let readable = Reflect::get(&pipe, &JsValue::from_str("readable"))
        .map_err(|_| fail("program_runtime_unavailable"))?;
    let writable = Reflect::get(&pipe, &JsValue::from_str("writable"))
        .map_err(|_| fail("program_runtime_unavailable"))?;
    let writer = method(&writable, "getWriter")?
        .call0(&writable)
        .map_err(|_| fail("program_runtime_unavailable"))?;
    let lease = Rc::new(ChannelLease(RefCell::new(Some(AbortController::default()))));
    let headers = Headers::new();
    headers.set("Content-Type", "application/x-ndjson")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(readable));
    let native = web_sys::RequestInit::from(&init);
    native.set_signal(Some(
        &lease.0.borrow().as_ref().expect("active channel").signal(),
    ));
    let request = web_sys::Request::new_with_str_and_init("https://program.invalid/run", &native)
        .map_err(|_| fail("invalid_program_runtime_input"))?;
    let started_at_ms = crate::util::now_ms();
    funding.verify_dispatch_time(started_at_ms)?;
    start.verify_time(started_at_ms).map_err(fail)?;
    let response = method(&entrypoint, "fetch")?
        .call1(&entrypoint, request.as_ref())
        .map_err(|_| fail("program_runtime_outcome_unknown"))?
        .dyn_into::<Promise>()
        .map_err(|_| fail("program_runtime_outcome_unknown"))?;
    let mut input = ProgramInput {
        writer,
        program_id: spec.program_call_id.clone(),
        frames: 0,
        lease: lease.clone(),
    };
    input.send(start.message(), deadline_ms).await?;
    let native_response = bounded(JsFuture::from(response), deadline_ms)
        .await?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| fail("program_runtime_outcome_unknown"))?;
    let response = worker::Response::from(native_response);
    let receipt = NativeReceipt {
        worker_id: spec.worker_id.clone(),
        code_revision: spec.module_sha256.clone(),
        started_at_ms,
        response_at_ms: crate::util::now_ms(),
    };
    if response.status_code() != 200 {
        lease.abort();
        return Ok(OpenProgram::Rejected {
            receipt,
            code: "program_runtime_http_error",
        });
    }
    if !response
        .headers()
        .get("Content-Type")?
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/x-ndjson"))
        })
    {
        lease.abort();
        return Ok(OpenProgram::Rejected {
            receipt,
            code: "invalid_program_runtime_reply",
        });
    }
    let (_, body) = response.into_parts();
    let ResponseBody::Stream(stream) = body else {
        lease.abort();
        return Ok(OpenProgram::Rejected {
            receipt,
            code: "invalid_program_runtime_reply",
        });
    };
    let reader = method(stream.as_ref(), "getReader")?
        .call0(stream.as_ref())
        .map_err(|_| fail("program_runtime_outcome_unknown"))?;
    Ok(OpenProgram::Streaming(ProgramChannel {
        input,
        output: ProgramOutput {
            reader,
            program_id: spec.program_call_id.clone(),
            buffered: vec![],
            frames: 0,
            finished: false,
            valid_frame_seen: false,
            budget_worker_id: spec.worker_id.clone(),
            budget_code_revision: spec.module_sha256.clone(),
            budget_cpu_ms: spec.cpu_ms,
            lease,
        },
        receipt,
    }))
}

impl ProgramInput {
    pub(crate) async fn send(&mut self, message: &Value, deadline_ms: u64) -> Result<()> {
        let result = self.write_frame(message, deadline_ms).await;
        if result.is_err() {
            self.lease.abort();
        }
        result
    }

    async fn write_frame(&mut self, message: &Value, deadline_ms: u64) -> Result<()> {
        if message["program_id"].as_str() != Some(self.program_id.as_str())
            || !matches!(message["op"].as_str(), Some("start" | "resume" | "cancel"))
            || self.frames >= 514
            || (self.frames == 0 && message["op"] != "start")
            || (self.frames > 0 && message["op"] == "start")
        {
            return Err(fail("invalid_program_runtime_input"));
        }
        let mut bytes =
            serde_json::to_vec(message).map_err(|_| fail("invalid_program_runtime_input"))?;
        if bytes.len() > MAX_BODY {
            return Err(fail("program_input_limit"));
        }
        bytes.push(b'\n');
        let value = Uint8Array::from(bytes.as_slice());
        let pending = method(&self.writer, "write")?
            .call1(&self.writer, value.as_ref())
            .map_err(|_| fail("program_channel_closed"))?
            .dyn_into::<Promise>()
            .map_err(|_| fail("program_channel_closed"))?;
        bounded(JsFuture::from(pending), deadline_ms).await?;
        self.frames += 1;
        Ok(())
    }

    pub(crate) async fn close(&mut self, deadline_ms: u64) -> Result<()> {
        let pending = method(&self.writer, "close")?
            .call0(&self.writer)
            .map_err(|_| fail("program_channel_closed"))?
            .dyn_into::<Promise>()
            .map_err(|_| fail("program_channel_closed"))?;
        let result = bounded(JsFuture::from(pending), deadline_ms)
            .await
            .map(|_| ());
        if result.is_err() {
            self.lease.abort();
        }
        result
    }

    pub(crate) fn abort(&self) {
        self.lease.abort();
    }
}

impl ProgramOutput {
    pub(crate) async fn next(&mut self, deadline_ms: u64) -> Result<Option<RuntimeReply>> {
        let result = self.read_frame(deadline_ms).await;
        if result.is_err() {
            self.lease.abort();
        }
        result
    }

    async fn read_frame(&mut self, deadline_ms: u64) -> Result<Option<RuntimeReply>> {
        if self.finished {
            return Ok(None);
        }
        loop {
            if let Some(index) = self.buffered.iter().position(|byte| *byte == b'\n') {
                if self.frames >= 514 {
                    self.lease.abort();
                    return Err(fail("program_frame_limit"));
                }
                let tail = self.buffered.split_off(index + 1);
                self.buffered.truncate(index);
                let reply: RuntimeReply = serde_json::from_slice(&self.buffered)
                    .map_err(|_| fail("invalid_program_runtime_reply"))?;
                self.buffered = tail;
                self.frames += 1;
                reply.validate(&self.program_id)?;
                self.valid_frame_seen = true;
                if reply.status != "paused" {
                    self.finished = true;
                }
                return Ok(Some(reply));
            }
            let pending = method(&self.reader, "read")?
                .call0(&self.reader)
                .map_err(|_| fail("program_channel_closed"))?
                .dyn_into::<Promise>()
                .map_err(|_| fail("program_channel_closed"))?;
            let value = bounded(JsFuture::from(pending), deadline_ms).await?;
            if Reflect::get(&value, &JsValue::from_str("done"))
                .ok()
                .and_then(|value| value.as_bool())
                == Some(true)
            {
                if self.buffered.is_empty() {
                    self.finished = true;
                    return Ok(None);
                }
                return Err(fail("invalid_program_runtime_reply"));
            }
            let chunk = Reflect::get(&value, &JsValue::from_str("value"))
                .map_err(|_| fail("invalid_program_runtime_reply"))?
                .dyn_into::<Uint8Array>()
                .map_err(|_| fail("invalid_program_runtime_reply"))?;
            // Check the native chunk length before copying it into wasm memory.
            if self.buffered.len().saturating_add(chunk.length() as usize) > MAX_BODY + 1 {
                self.lease.abort();
                return Err(fail("program_output_limit"));
            }
            self.buffered.extend_from_slice(&chunk.to_vec());
        }
    }

    pub(crate) fn abort(&self) {
        self.lease.abort();
    }

    pub(crate) fn enforced_budget(&self) -> Option<EnforcedProgramBudget> {
        self.valid_frame_seen.then(|| EnforcedProgramBudget {
            worker_id: self.budget_worker_id.clone(),
            code_revision: self.budget_code_revision.clone(),
            cpu_limit_ms: self.budget_cpu_ms,
        })
    }
}

async fn bounded(future: JsFuture, deadline_ms: u64) -> Result<JsValue> {
    let remaining = deadline_ms.saturating_sub(crate::util::now_ms());
    if remaining == 0 {
        return Err(fail("program_channel_expired"));
    }
    match futures::future::select(
        Box::pin(future),
        Box::pin(Delay::from(std::time::Duration::from_millis(remaining))),
    )
    .await
    {
        Either::Left((result, _)) => result.map_err(|_| fail("program_channel_closed")),
        Either::Right(_) => Err(fail("program_channel_expired")),
    }
}
fn method(target: &JsValue, name: &str) -> Result<Function> {
    Reflect::get(target, &JsValue::from_str(name))
        .map_err(|_| fail("program_runtime_unavailable"))?
        .dyn_into()
        .map_err(|_| fail("program_runtime_unavailable"))
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_reply_is_not_a_self_authorized_caller_or_error_channel() {
        let reply:RuntimeReply=serde_json::from_value(json!({"status":"failed","program_id":"program","error":"customer-secret-full-stack","calls":[]})).unwrap();
        reply.validate("program").unwrap();
        assert_eq!(reply.safe_error(), "program_execution_failed");
        assert!(reply.validate("other").is_err());
        assert!(
            serde_json::from_value::<RuntimeReply>(
                json!({"status":"paused","calls":[],"owner":"forged"})
            )
            .is_err()
        );
    }

    #[test]
    fn module_template_and_telemetry_egress_profile_have_explicit_security_controls() {
        assert_eq!(TEMPLATE.matches(MARKER).count(), 1);
        assert!(TEMPLATE.contains("const ENABLE_REPLAY_PROFILE = false;"));
        assert!(TEMPLATE.contains("'reportError'"));
        assert!(TEMPLATE.contains("runtime_missing"));
    }
}
