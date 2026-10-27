//! Repair a lost Chat observation by reading its original durable run result.

use std::{cell::Cell, collections::VecDeque, future::Future, rc::Rc, time::Duration};

use agents_core::SessionRecord;
use eventsource_stream::{Event, Eventsource};
use futures::{
    FutureExt, Stream, StreamExt,
    future::{AbortHandle, Either, LocalBoxFuture},
    stream::{Abortable, LocalBoxStream},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::{AbortSignal, Event as AbortEvent};
use worker::{Delay, Env, Response, Result};

use crate::{routes, session::sse, telemetry::TraceContext, util};

const MAX_READS: u8 = 32;
const RECONNECT_MS: u64 = 120_000;
// Recovery alone buffers text. This is generous relative to the provider's
// 16 MiB completion bound; ordinary streaming keeps only a digest and byte count.
const MAX_RECEIPT_BYTES: usize = 32 * 1024 * 1024;
const DONE: &[u8] = b"data: [DONE]\n\n";
const RECOVERY_PING: &[u8] = b":\n\n";

type Events = LocalBoxStream<'static, std::result::Result<Event, ()>>;
type RecoveryResult = std::result::Result<VecDeque<Vec<u8>>, Failure>;
type Recovery = LocalBoxFuture<'static, RecoveryResult>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Unavailable,
    Mismatch,
    Overloaded,
    Timeout,
    Cancelled,
}

impl Failure {
    fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "chat_observation_unavailable",
            Self::Mismatch => "chat_observation_mismatch",
            Self::Overloaded => "chat_observation_overloaded",
            Self::Timeout => "chat_observation_timeout",
            Self::Cancelled => "chat_observation_cancelled",
        }
    }
}

enum Action {
    Forward,
    Done,
    Recover,
}

#[derive(Clone, Default)]
struct Progress {
    bytes: usize,
    digest: Sha256,
    role: bool,
    terminal: bool,
}

impl Progress {
    fn accept(
        &mut self,
        run: &str,
        model: &str,
        data: &str,
    ) -> std::result::Result<Action, Failure> {
        if data == "[DONE]" {
            return Ok(if self.terminal {
                Action::Done
            } else {
                Action::Recover
            });
        }
        if self.terminal {
            return Err(Failure::Mismatch);
        }
        let value: Value = serde_json::from_str(data).map_err(|_| Failure::Mismatch)?;
        if value.get("error").is_some_and(Value::is_object) {
            if value.get("id").is_some_and(|id| id.as_str() != Some(run))
                || value
                    .get("model")
                    .is_some_and(|id| id.as_str() != Some(model))
            {
                return Err(Failure::Mismatch);
            }
            self.terminal = true;
            return Ok(Action::Forward);
        }
        if value.get("id").and_then(Value::as_str) != Some(run)
            || value.get("model").and_then(Value::as_str) != Some(model)
            || value.get("object").and_then(Value::as_str) != Some("chat.completion.chunk")
        {
            return Err(Failure::Mismatch);
        }
        let choices = value
            .get("choices")
            .and_then(Value::as_array)
            .ok_or(Failure::Mismatch)?;
        for choice in choices {
            if choice.get("index").and_then(Value::as_u64) != Some(0) {
                return Err(Failure::Mismatch);
            }
            self.delta(choice.get("delta").ok_or(Failure::Mismatch)?)?;
            if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
                if reason.as_str() != Some("stop") || !valid_terminal(&value, run, model) {
                    return Err(Failure::Mismatch);
                }
                self.terminal = true;
            }
        }
        Ok(Action::Forward)
    }

    fn delta(&mut self, delta: &Value) -> std::result::Result<(), Failure> {
        if !delta.is_object() {
            return Err(Failure::Mismatch);
        }
        if let Some(role) = delta.get("role").filter(|v| !v.is_null()) {
            if role.as_str() != Some("assistant") {
                return Err(Failure::Mismatch);
            }
            self.role = true;
        }
        if let Some(content) = delta.get("content").filter(|v| !v.is_null()) {
            let text = content.as_str().ok_or(Failure::Mismatch)?;
            self.bytes = self
                .bytes
                .checked_add(text.len())
                .ok_or(Failure::Mismatch)?;
            self.digest.update(text.as_bytes());
        }
        Ok(())
    }

    fn suffix<'a>(&self, text: &'a str) -> std::result::Result<&'a str, Failure> {
        let prefix = text.get(..self.bytes).ok_or(Failure::Mismatch)?;
        if Sha256::digest(prefix.as_bytes()) != self.digest.clone().finalize() {
            return Err(Failure::Mismatch);
        }
        text.get(self.bytes..).ok_or(Failure::Mismatch)
    }
}

#[derive(Deserialize)]
struct Receipt {
    run_id: String,
    status: String,
    text: String,
    terminal: Value,
}

struct Target {
    env: Env,
    session: SessionRecord,
    trace: TraceContext,
    run: String,
    signal: AbortSignal,
}

enum Read {
    Retry,
    Json(Response),
    Subscription(Response),
}

impl Target {
    async fn open(&self) -> std::result::Result<Read, Failure> {
        if self.signal.aborted() {
            return Err(Failure::Cancelled);
        }
        let response = routes::read_chat_run(&self.env, &self.session, &self.trace, &self.run)
            .await
            .map_err(|error| {
                if overloaded(&error) {
                    Failure::Overloaded
                } else {
                    Failure::Timeout
                }
            })?;
        match response.status_code() {
            500 | 502 | 503 | 504 => return Ok(Read::Retry),
            429 => return Err(Failure::Overloaded),
            200 => {}
            _ => return Err(Failure::Unavailable),
        }
        let marker = response
            .headers()
            .get("x-agents-internal-observer")
            .map_err(|_| Failure::Unavailable)?;
        let content_type = response
            .headers()
            .get("Content-Type")
            .map_err(|_| Failure::Unavailable)?
            .unwrap_or_default()
            .to_ascii_lowercase();
        match marker.as_deref() {
            Some("chat") if content_type.starts_with("text/event-stream") => {
                Ok(Read::Subscription(response))
            }
            None if content_type.starts_with("application/json") => Ok(Read::Json(response)),
            _ => Err(Failure::Unavailable),
        }
    }

    async fn recover(&self, progress: &Progress) -> RecoveryResult {
        let started = util::now_ms();
        let mut budget = Budget::default();
        let result = self.recover_until(progress, &mut budget).await;
        if self.signal.aborted() || result.as_ref().err() == Some(&Failure::Cancelled) {
            return Err(Failure::Cancelled);
        }
        util::emit(
            "observer.recovery",
            json!({
                "session_id":self.session.id,"run_id":self.run,"trace_id":self.trace.trace_id(),
                "attempt":budget.reads,"duration_ms":util::now_ms().saturating_sub(started),
                "success":result.is_ok(),
                "outcome":if result.is_ok() {"canonical_reconstructed"} else {"unavailable"},
                "error_code":result.as_ref().err().map(|error|error.code()).unwrap_or("none"),
            }),
        );
        result
    }

    async fn recover_until(&self, progress: &Progress, budget: &mut Budget) -> RecoveryResult {
        loop {
            if self.signal.aborted() {
                return Err(Failure::Cancelled);
            }
            let remaining = budget.begin()?;
            let opened_at = util::now_ms();
            let request = Box::pin(self.open());
            let timeout = Box::pin(Delay::from(Duration::from_millis(remaining)));
            let result = match futures::future::select(request, timeout).await {
                Either::Left((result, _)) => result,
                Either::Right(_) => return Err(Failure::Timeout),
            };
            budget.spend(util::now_ms().saturating_sub(opened_at))?;
            let receipt = match result {
                Ok(Read::Json(response)) => match read_finite_json(response, budget).await {
                    Ok(receipt) => Some(receipt),
                    Err(Failure::Timeout) => None,
                    Err(error) => return Err(error),
                },
                // An attached subscription is healthy execution, not another
                // reconnect attempt. Its body has no agent-task deadline.
                Ok(Read::Subscription(response)) => read_subscription(response).await?,
                Ok(Read::Retry) | Err(Failure::Timeout) => None,
                Err(error) => return Err(error),
            };
            if let Some(receipt) = receipt {
                return repair(progress, &self.run, &self.session.agent_id, receipt);
            }
            if self.signal.aborted() {
                return Err(Failure::Cancelled);
            }
            let delay = budget.delay()?;
            let backoff_at = util::now_ms();
            Delay::from(Duration::from_millis(delay)).await;
            budget.spend(util::now_ms().saturating_sub(backoff_at))?;
        }
    }
}

// An already-complete JSON reply is finite transport work. Only the active
// subscription below may wait for an arbitrarily long healthy agent task.
async fn read_finite_json(
    response: Response,
    budget: &mut Budget,
) -> std::result::Result<Receipt, Failure> {
    let started = util::now_ms();
    let remaining = budget.remaining()?;
    let timeout = Box::pin(Delay::from(Duration::from_millis(remaining)));
    let (result, minimum_spent) =
        match futures::future::select(Box::pin(read_json(response)), timeout).await {
            Either::Left((result, _)) => (result, 0),
            Either::Right(_) => (Err(Failure::Timeout), remaining),
        };
    budget.spend(util::now_ms().saturating_sub(started).max(minimum_spent))?;
    result
}

async fn read_json(mut response: Response) -> std::result::Result<Receipt, Failure> {
    let mut bytes = Vec::new();
    let mut source = response.stream().map_err(|_| Failure::Unavailable)?;
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|error| {
            if overloaded(&error) {
                Failure::Overloaded
            } else {
                Failure::Timeout
            }
        })?;
        if chunk.len() > MAX_RECEIPT_BYTES.saturating_sub(bytes.len()) {
            return Err(Failure::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Failure::Unavailable)
}

async fn read_subscription(
    mut response: Response,
) -> std::result::Result<Option<Receipt>, Failure> {
    use crate::observer_transport::{DecodeError, decode_detailed};
    let source = response.stream().map_err(|_| Failure::Unavailable)?;
    let is_overloaded = Rc::new(Cell::new(false));
    let flag = is_overloaded.clone();
    let source = source.map(move |chunk| {
        if chunk.as_ref().err().is_some_and(overloaded) {
            flag.set(true);
        }
        chunk
    });
    match decode_detailed(source).await {
        Ok(reply) if reply.status == 200 => serde_json::from_value(reply.body)
            .map(Some)
            .map_err(|_| Failure::Unavailable),
        Ok(reply) if reply.status == 429 => Err(Failure::Overloaded),
        Ok(_) | Err(DecodeError::Invalid) => Err(Failure::Unavailable),
        Err(DecodeError::Detached) if is_overloaded.get() => Err(Failure::Overloaded),
        Err(DecodeError::Detached) => Ok(None),
    }
}

#[derive(Default)]
struct Budget {
    reads: u8,
    transport_ms: u64,
}

impl Budget {
    fn remaining(&self) -> std::result::Result<u64, Failure> {
        RECONNECT_MS
            .checked_sub(self.transport_ms)
            .filter(|remaining| *remaining > 0)
            .ok_or(Failure::Timeout)
    }

    fn begin(&mut self) -> std::result::Result<u64, Failure> {
        if self.reads >= MAX_READS {
            return Err(Failure::Timeout);
        }
        let remaining = self.remaining()?;
        self.reads += 1;
        Ok(remaining)
    }

    fn spend(&mut self, elapsed: u64) -> std::result::Result<(), Failure> {
        self.transport_ms = self.transport_ms.saturating_add(elapsed);
        self.remaining().map(|_| ())
    }

    fn delay(&self) -> std::result::Result<u64, Failure> {
        if self.reads >= MAX_READS {
            return Err(Failure::Timeout);
        }
        Ok((250u64 << self.reads.saturating_sub(1).min(4)).min(self.remaining()?))
    }
}

fn overloaded(error: &worker::Error) -> bool {
    let original = match error {
        worker::Error::UnknownJsError { original, .. } | worker::Error::Internal(original) => {
            original
        }
        _ => return false,
    };
    js_sys::Reflect::get(original, &"overloaded".into())
        .ok()
        .and_then(|value| value.as_bool())
        == Some(true)
}

fn repair(
    progress: &Progress,
    run: &str,
    model: &str,
    receipt: Receipt,
) -> std::result::Result<VecDeque<Vec<u8>>, Failure> {
    if receipt.run_id != run {
        return Err(Failure::Mismatch);
    }
    if receipt.status == "failed"
        && receipt
            .terminal
            .pointer("/error/code")
            .is_some_and(Value::is_string)
        && receipt.terminal.get("choices").is_none()
    {
        return Ok(VecDeque::from([
            frame(&receipt.terminal.to_string()),
            DONE.to_vec(),
        ]));
    }
    let terminal = &receipt.terminal;
    if receipt.status != "completed" || !valid_terminal(terminal, run, model) {
        return Err(Failure::Mismatch);
    }
    let tail = progress.suffix(&receipt.text)?;
    let mut frames = VecDeque::new();
    if !progress.role {
        frames.push_back(delta_frame(terminal, json!({"role":"assistant"})));
    }
    if !tail.is_empty() {
        frames.push_back(delta_frame(terminal, json!({"content":tail})));
    }
    frames.push_back(frame(&terminal.to_string()));
    frames.push_back(DONE.to_vec());
    Ok(frames)
}

fn valid_terminal(value: &Value, run: &str, model: &str) -> bool {
    value.get("id").and_then(Value::as_str) == Some(run)
        && value.get("model").and_then(Value::as_str) == Some(model)
        && value.get("object").and_then(Value::as_str) == Some("chat.completion.chunk")
        && value.get("created").and_then(Value::as_u64).is_some()
        && value.get("error").is_none_or(Value::is_null)
        && value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| choices.len() == 1)
        && value.pointer("/choices/0/index").and_then(Value::as_u64) == Some(0)
        && value
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            == Some("stop")
        && value
            .pointer("/choices/0/delta")
            .and_then(Value::as_object)
            .is_some_and(|delta| delta.is_empty())
        && ["prompt_tokens", "completion_tokens", "total_tokens"]
            .into_iter()
            .all(|name| {
                value
                    .get("usage")
                    .and_then(|usage| usage.get(name))
                    .and_then(Value::as_u64)
                    .is_some()
            })
}

fn delta_frame(terminal: &Value, delta: Value) -> Vec<u8> {
    let mut value = terminal.clone();
    value.as_object_mut().unwrap().remove("usage");
    value["choices"][0]["delta"] = delta;
    value["choices"][0]["finish_reason"] = Value::Null;
    frame(&value.to_string())
}

fn frame(data: &str) -> Vec<u8> {
    let mut output = String::new();
    for line in data.split('\n') {
        output.push_str("data: ");
        output.push_str(line);
        output.push('\n');
    }
    output.push('\n');
    output.into_bytes()
}

struct Relay {
    source: Option<Events>,
    target: Rc<Target>,
    progress: Progress,
    recovery: Option<Recovery>,
    frames: VecDeque<Vec<u8>>,
    done: bool,
}

impl Relay {
    fn fail(&mut self, failure: Failure) {
        self.source = None;
        self.recovery = None;
        self.done = true;
        if failure == Failure::Cancelled {
            self.frames.clear();
            return;
        }
        self.frames.push_back(frame(&json!({"error":{
            "code":failure.code(),"type":"agents_error",
            "message":"Original run observation unavailable; inspect this session and run before retrying.",
            "session_id":self.target.session.id,"run_id":self.target.run,
        }}).to_string()));
        self.frames.push_back(DONE.to_vec());
    }

    async fn next(mut self) -> Option<(Result<Vec<u8>>, Self)> {
        loop {
            if let Some(bytes) = self.frames.pop_front() {
                return Some((Ok(bytes), self));
            }
            if self.done {
                return None;
            }
            if self.progress.terminal {
                self.source = None;
                self.done = true;
                return Some((Ok(DONE.to_vec()), self));
            }
            let event = match self.source.as_mut() {
                Some(source) => source.next().await,
                None => None,
            };
            match event {
                Some(Ok(event)) => match self.progress.accept(
                    &self.target.run,
                    &self.target.session.agent_id,
                    &event.data,
                ) {
                    Ok(Action::Forward) => return Some((Ok(frame(&event.data)), self)),
                    Ok(Action::Done) => {
                        self.source = None;
                        self.done = true;
                        return Some((Ok(DONE.to_vec()), self));
                    }
                    Ok(Action::Recover) => self.source = None,
                    Err(error) => {
                        self.fail(error);
                        continue;
                    }
                },
                Some(Err(())) | None => self.source = None,
            }
            if self.progress.terminal {
                self.done = true;
                return Some((Ok(DONE.to_vec()), self));
            }
            if self.recovery.is_none() {
                let target = self.target.clone();
                let progress = self.progress.clone();
                self.recovery = Some(async move { target.recover(&progress).await }.boxed_local());
            }
            let recovery = self.recovery.take().unwrap();
            let timer = Box::pin(Delay::from(Duration::from_millis(100)));
            match recovery_tick(recovery, timer).await {
                RecoveryTick::Ready(Ok(frames)) => {
                    self.frames = frames;
                    self.done = true;
                }
                RecoveryTick::Ready(Err(error)) => self.fail(error),
                RecoveryTick::Pending(recovery) => {
                    self.recovery = Some(recovery);
                    return Some((Ok(RECOVERY_PING.to_vec()), self));
                }
            }
        }
    }
}

pub(crate) async fn wrap(
    env: Env,
    session: SessionRecord,
    trace: TraceContext,
    mut response: Response,
    signal: AbortSignal,
) -> Result<Response> {
    if response.status_code() != 200
        || !response
            .headers()
            .get("Content-Type")?
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"))
    {
        return Ok(response);
    }
    let Some(run) = response.headers().get("X-Agents-Run")? else {
        return Ok(response);
    };
    if !run
        .strip_prefix("run_")
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return util::json_error(
            503,
            "chat_observation_unavailable",
            "Run observation identity is unavailable",
        );
    }
    let status = response.status_code();
    let headers = response.headers().clone();
    let source = response
        .stream()?
        .eventsource()
        .map(|item| item.map_err(|_| ()))
        .boxed_local();
    let relay = Relay {
        source: Some(source),
        target: Rc::new(Target {
            env,
            session,
            trace,
            run,
            signal: signal.clone(),
        }),
        progress: Progress::default(),
        recovery: None,
        frames: VecDeque::new(),
        done: false,
    };
    let output = futures::stream::unfold(relay, Relay::next).boxed_local();
    let (handle, registration) = AbortHandle::new_pair();
    let guard = Cancellation::new(signal, handle)?;
    let output = Abortable::new(sse::keep_alive(output).boxed_local(), registration);
    let output = scoped(output, guard).boxed_local();
    Ok(Response::from_stream(output)?
        .with_status(status)
        .with_headers(headers))
}

enum RecoveryTick {
    Ready(RecoveryResult),
    Pending(Recovery),
}

async fn recovery_tick<T>(recovery: Recovery, timer: T) -> RecoveryTick
where
    T: Future<Output = ()> + Unpin,
{
    // A quiet broken client may not notify Request.signal until another write.
    // Keep this one recovery future alive across comment-only liveness probes.
    match futures::future::select(recovery, timer).await {
        Either::Left((result, _)) => RecoveryTick::Ready(result),
        Either::Right(((), recovery)) => RecoveryTick::Pending(recovery),
    }
}

struct Cancellation {
    signal: AbortSignal,
    callback: Closure<dyn FnMut(AbortEvent)>,
}

impl Cancellation {
    fn new(signal: AbortSignal, handle: AbortHandle) -> Result<Self> {
        let callback_handle = handle.clone();
        let callback =
            Closure::wrap_assert_unwind_safe(
                Box::new(move |_: AbortEvent| callback_handle.abort())
                    as Box<dyn FnMut(AbortEvent)>,
            );
        let guard = Self { signal, callback };
        guard
            .signal
            .add_event_listener_with_callback("abort", guard.callback.as_ref().unchecked_ref())?;
        // Register before inspecting the state so an intervening abort cannot
        // leave a pending observer detached from its cancellation notification.
        if guard.signal.aborted() {
            handle.abort();
        }
        Ok(guard)
    }
}

impl Drop for Cancellation {
    fn drop(&mut self) {
        // Detach the native EventTarget before releasing its wasm trampoline.
        let _ = self
            .signal
            .remove_event_listener_with_callback("abort", self.callback.as_ref().unchecked_ref());
    }
}

fn scoped<S, G>(source: S, guard: G) -> impl Stream<Item = Result<Vec<u8>>>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin,
{
    // Abortable ends polling on abort, but still owns its inner stream. Release
    // that pending pull and the native listener as soon as EOF is observed.
    futures::stream::unfold(Some((source, guard)), |state| async move {
        let (mut source, guard) = state?;
        let item = source.next().await?;
        if item.as_ref().map_or(true, |bytes| bytes == DONE) {
            Some((item, None))
        } else {
            Some((item, Some((source, guard))))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt, executor::block_on};
    use proptest::prelude::*;
    use std::{
        cell::Cell,
        pin::Pin,
        rc::Rc,
        task::{Context, Poll},
    };

    struct PendingRead {
        polled: Rc<Cell<bool>>,
        dropped: Rc<Cell<bool>>,
    }

    impl Stream for PendingRead {
        type Item = Result<Vec<u8>>;

        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.polled.set(true);
            Poll::Pending
        }
    }

    impl Drop for PendingRead {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    struct ListenerProbe(Rc<Cell<bool>>);

    impl Drop for ListenerProbe {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    fn terminal(run: &str) -> Value {
        json!({"id":run,"object":"chat.completion.chunk","created":123,"model":"agent",
            "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}})
    }

    #[test]
    fn native_abort_releases_a_pending_pull_and_listener_before_stream_gc() {
        let polled = Rc::new(Cell::new(false));
        let dropped = Rc::new(Cell::new(false));
        let listener = Rc::new(Cell::new(false));
        let source = PendingRead {
            polled: polled.clone(),
            dropped: dropped.clone(),
        };
        let (handle, registration) = AbortHandle::new_pair();
        let mut output = scoped(
            Abortable::new(source, registration),
            ListenerProbe(listener.clone()),
        )
        .boxed_local();
        assert!(output.next().now_or_never().is_none());
        assert!(polled.get());
        assert!(!dropped.get() && !listener.get());
        handle.abort();
        assert!(block_on(output.next()).is_none());
        assert!(dropped.get() && listener.get());
    }

    #[test]
    fn preaborted_observation_never_starts_its_pending_read() {
        let polled = Rc::new(Cell::new(false));
        let dropped = Rc::new(Cell::new(false));
        let listener = Rc::new(Cell::new(false));
        let source = PendingRead {
            polled: polled.clone(),
            dropped: dropped.clone(),
        };
        let (handle, registration) = AbortHandle::new_pair();
        handle.abort();
        let mut output = scoped(
            Abortable::new(source, registration),
            ListenerProbe(listener.clone()),
        )
        .boxed_local();
        assert!(block_on(output.next()).is_none());
        assert!(!polled.get());
        assert!(dropped.get() && listener.get());
    }

    #[test]
    fn terminal_done_releases_its_listener_without_an_extra_pull() {
        let dropped = Rc::new(Cell::new(false));
        let listener = Rc::new(Cell::new(false));
        let probe = ListenerProbe(dropped.clone());
        let source = futures::stream::iter([Ok(DONE.to_vec())]).map(move |item| {
            let _ = &probe;
            item
        });
        let (_handle, registration) = AbortHandle::new_pair();
        let mut output = scoped(
            Abortable::new(source, registration),
            ListenerProbe(listener.clone()),
        )
        .boxed_local();
        assert_eq!(block_on(output.next()).unwrap().unwrap(), DONE);
        assert!(dropped.get() && listener.get());
    }

    #[test]
    fn recovery_comments_preserve_one_pending_read_budget_and_prefix() {
        let starts = Rc::new(Cell::new(0));
        let dropped = Rc::new(Cell::new(false));
        let mut progress = Progress::default();
        progress.delta(&json!({"content":"雪"})).unwrap();
        let progress = progress.clone();
        let (sender, receiver) = futures::channel::oneshot::channel::<String>();
        let started = starts.clone();
        let lifetime = ListenerProbe(dropped.clone());
        let mut recovery: Recovery = async move {
            let _lifetime = lifetime;
            started.set(started.get() + 1);
            let mut budget = Budget::default();
            budget.begin().unwrap();
            let text = receiver.await.unwrap();
            assert_eq!(budget.reads, 1);
            assert_eq!(progress.bytes, "雪".len());
            Ok(VecDeque::from([progress
                .suffix(&text)
                .unwrap()
                .as_bytes()
                .to_vec()]))
        }
        .boxed_local();
        for _ in 0..10 {
            recovery = match block_on(recovery_tick(recovery, futures::future::ready(()))) {
                RecoveryTick::Pending(recovery) => recovery,
                RecoveryTick::Ready(_) => {
                    panic!("a comment restarted or completed the pending read")
                }
            };
            assert_eq!(starts.get(), 1);
            assert!(!dropped.get());
        }
        sender.send("雪 tail".into()).unwrap();
        match block_on(recovery_tick(recovery, futures::future::pending::<()>())) {
            RecoveryTick::Ready(Ok(frames)) => assert_eq!(frames[0], b" tail"),
            _ => panic!("the original pending read must provide the terminal result"),
        }
        assert!(dropped.get());
    }

    proptest! {
        #[test]
        fn arbitrary_unicode_segments_restore_only_the_exact_missing_suffix(parts in prop::collection::vec(".*", 0..30), cut in any::<usize>()) {
            let text = parts.concat();
            let sent = if parts.is_empty() { 0 } else { cut % (parts.len()+1) };
            let mut progress = Progress::default();
            for part in &parts[..sent] {
                let data=json!({"id":"run","model":"agent","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":part},"finish_reason":null}]}).to_string();
                progress.accept("run", "agent", &data).unwrap();
            }
            let tail=progress.suffix(&text).unwrap();
            prop_assert_eq!(format!("{}{}",parts[..sent].concat(),tail),text.as_str());
            if progress.bytes > 0 {
                let changed=format!("{}{text}",if text.starts_with('x') {'y'} else {'x'});
                prop_assert_eq!(progress.suffix(&changed),Err(Failure::Mismatch));
            }
        }
    }

    #[test]
    fn wrong_identity_and_utf8_boundary_fail_closed() {
        let mut progress = Progress::default();
        assert!(
            progress
                .accept(
                    "right",
                    "agent",
                    &json!({"id":"wrong","choices":[]}).to_string()
                )
                .is_err()
        );
        progress.delta(&json!({"content":"x"})).unwrap();
        assert_eq!(progress.suffix("雪"), Err(Failure::Mismatch));
    }

    #[test]
    fn terminal_and_done_are_not_interchangeable() {
        let mut progress = Progress::default();
        assert!(matches!(
            progress.accept("run", "agent", "[DONE]"),
            Ok(Action::Recover)
        ));
        progress
            .accept("run", "agent", &terminal("run").to_string())
            .unwrap();
        assert!(matches!(
            progress.accept("run", "agent", "[DONE]"),
            Ok(Action::Done)
        ));
        assert!(progress.terminal);
    }

    #[test]
    fn invalid_terminal_identity_payload_and_usage_cannot_be_forwarded() {
        for (pointer, replacement) in [
            ("/id", json!("other")),
            ("/model", json!("other-agent")),
            ("/object", json!("response")),
            ("/choices/0/index", json!(1)),
            ("/choices/0/delta", json!({"content":"unverified suffix"})),
            ("/usage", Value::Null),
            ("/usage/prompt_tokens", json!(-1)),
        ] {
            let mut value = terminal("run");
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                Progress::default()
                    .accept("run", "agent", &value.to_string())
                    .is_err()
            );
        }
        let error = json!({"id":"other","error":{"code":"provider_error"}});
        assert!(
            Progress::default()
                .accept("run", "agent", &error.to_string())
                .is_err()
        );
    }

    #[test]
    fn repaired_terminal_preserves_original_usage_and_error_never_becomes_stop() {
        let progress = Progress::default();
        let value = terminal("run");
        let frames = repair(
            &progress,
            "run",
            "agent",
            Receipt {
                run_id: "run".into(),
                status: "completed".into(),
                text: "雪".into(),
                terminal: value.clone(),
            },
        )
        .unwrap();
        assert_eq!(frames[frames.len() - 2], frame(&value.to_string()));
        let error = json!({"error":{"code":"provider_error","message":"original"}});
        let frames = repair(
            &progress,
            "run",
            "agent",
            Receipt {
                run_id: "run".into(),
                status: "failed".into(),
                text: String::new(),
                terminal: error.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            frames,
            VecDeque::from([frame(&error.to_string()), DONE.to_vec()])
        );
    }

    #[test]
    fn reconnect_budget_does_not_expire_a_healthy_long_subscription() {
        let mut clock = 1000u64;
        let mut budget = Budget::default();
        assert_eq!(budget.begin(), Ok(RECONNECT_MS));
        let handshake = clock;
        clock += 25;
        budget.spend(clock - handshake).unwrap();
        let attached = clock;
        clock += 303_763;
        assert!(clock - attached > 303_762);
        assert_eq!(budget.remaining(), Ok(RECONNECT_MS - 25));
        assert_eq!(budget.reads, 1);
        assert_eq!(budget.delay(), Ok(250));
        let backoff = clock;
        clock += 250;
        budget.spend(clock - backoff).unwrap();
        assert_eq!(budget.begin(), Ok(RECONNECT_MS - 275));
        let handshake = clock;
        clock += 30;
        budget.spend(clock - handshake).unwrap();
        assert_eq!(budget.reads, 2);
        assert_eq!(budget.remaining(), Ok(RECONNECT_MS - 305));
        assert_eq!(budget.delay(), Ok(500));
        assert_eq!(budget.spend(RECONNECT_MS - 305), Err(Failure::Timeout));
        assert_eq!(budget.begin(), Err(Failure::Timeout));
        assert_eq!(budget.delay(), Err(Failure::Timeout));
    }

    #[test]
    fn reconnect_count_stays_bounded_without_charging_healthy_body_time() {
        let mut budget = Budget::default();
        for _ in 0..MAX_READS {
            assert!(budget.begin().is_ok());
            budget.spend(1).unwrap();
        }
        assert_eq!(budget.begin(), Err(Failure::Timeout));
        assert_eq!(budget.delay(), Err(Failure::Timeout));
    }

    #[test]
    fn finite_body_deadline_consumes_the_remaining_transport_allowance() {
        let mut budget = Budget::default();
        budget.begin().unwrap();
        budget.spend(25).unwrap();
        let remaining = budget.remaining().unwrap();
        // A clock sample may lag the timer notification; the expired timer is
        // itself proof that at least its full remaining allowance was spent.
        let sampled_elapsed = 0u64;
        assert_eq!(
            budget.spend(sampled_elapsed.max(remaining)),
            Err(Failure::Timeout)
        );
        assert_eq!(budget.begin(), Err(Failure::Timeout));
        assert_eq!(budget.delay(), Err(Failure::Timeout));
    }
}
