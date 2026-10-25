//! Connection-local Responses ordering; execution remains in Session DOs.

use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use futures::{FutureExt, StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
use serde_json::{Map, Value, json};
use worker::{Delay, Response, WebSocket, WebSocketPair, WebsocketEvent};

pub(crate) const MAX_ACTIVE: usize = 16;
pub(crate) const MAX_NAMED_LANES: usize = 32;
pub(crate) const MAX_CONNECTION_MS: u64 = 60 * 60 * 1000;
const MAX_QUEUED: usize = 64;
const MAX_QUEUE_BYTES: usize = 4 * 1024 * 1024;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_PARENT_TOKEN_BYTES: usize = 4096;
const MAX_OUTBOUND_BUFFER_BYTES: u32 = 32 * 1024 * 1024;
const MAX_PENDING_CONTROLS: usize = 16;

pub(crate) type ServerEvents =
    futures::stream::LocalBoxStream<'static, Result<ServerEvent, RequestFailure>>;

pub(crate) struct ServerEvent {
    pub(crate) value: Value,
    pub(crate) parent: Option<Parent>,
}

/// The harness supplies only public, sanitized errors, never raw provider errors.
pub(crate) struct RequestFailure {
    pub(crate) status: u16,
    pub(crate) error: Value,
}

impl RequestFailure {
    fn value(&self, lane: &LaneId) -> Value {
        let mut value = json!({"type":"error", "status":self.status, "error":self.error});
        lane.tag(&mut value);
        value
    }
}

pub(crate) trait Executor {
    fn create(
        &self,
        dispatch: Dispatch,
    ) -> LocalBoxFuture<'static, Result<ServerEvents, RequestFailure>>;

    fn steer(
        &self,
        request: Steer,
    ) -> LocalBoxFuture<'static, Result<Vec<ServerEvent>, RequestFailure>>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LaneId {
    #[default]
    Default,
    Named(String),
}

impl LaneId {
    fn parse(value: Option<&Value>) -> Result<Self, ProtocolError> {
        match value {
            None | Some(Value::Null) => Ok(Self::Default),
            Some(Value::String(value)) if valid_lane(value) => Ok(Self::Named(value.clone())),
            _ => Err(ProtocolError::invalid_stream()),
        }
    }

    pub(crate) fn tag(&self, event: &mut Value) {
        let Some(object) = event.as_object_mut() else {
            return;
        };
        object.remove("stream_id");
        if let Self::Named(name) = self {
            object.insert("stream_id".into(), Value::String(name.clone()));
        }
    }
}

fn valid_lane(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

fn valid_response_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

#[derive(Clone)]
pub(crate) struct Create {
    pub(crate) lane: LaneId,
    pub(crate) generate: bool,
    pub(crate) body: Value,
    bytes: usize,
}

impl Create {
    pub(crate) fn previous(&self) -> Option<&str> {
        self.body
            .get("previous_response_id")
            .and_then(Value::as_str)
    }
}

pub(crate) struct Steer {
    pub(crate) previous_response_id: String,
    pub(crate) input: Value,
}

pub(crate) enum ClientEvent {
    Create(Create),
    Steer(Steer),
}

#[derive(Debug)]
pub(crate) struct ProtocolError {
    pub(crate) code: &'static str,
    pub(crate) status: u16,
    param: Option<&'static str>,
    message: &'static str,
    lane: LaneId,
}

impl ProtocolError {
    fn new(code: &'static str, param: Option<&'static str>, message: &'static str) -> Self {
        Self {
            code,
            status: 400,
            param,
            message,
            lane: LaneId::Default,
        }
    }

    fn invalid_stream() -> Self {
        Self::new(
            "invalid_stream_id",
            Some("stream_id"),
            "stream_id must contain 1 to 256 ASCII letters, numbers, underscores, hyphens or periods",
        )
    }

    fn in_lane(mut self, lane: &LaneId) -> Self {
        self.lane = lane.clone();
        self
    }

    pub(crate) fn value(&self) -> Value {
        let mut value = json!({
            "type":"error", "status":self.status,
            "error":{"type":"invalid_request_error", "code":self.code,
                     "message":self.message, "param":self.param},
        });
        self.lane.tag(&mut value);
        value
    }
}

pub(crate) fn parse(text: &str) -> Result<ClientEvent, ProtocolError> {
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::new(
            "request_too_large",
            None,
            "WebSocket message exceeds its byte limit",
        ));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| ProtocolError::new("invalid_json", None, "Send a JSON client event"))?;
    let object = value.as_object().ok_or_else(|| {
        ProtocolError::new(
            "invalid_request_error",
            None,
            "A client event must be an object",
        )
    })?;
    match object.get("type").and_then(Value::as_str) {
        Some("response.create") => create(object, text.len()).map(ClientEvent::Create),
        Some("response.steer") => steer(object).map(ClientEvent::Steer),
        _ => Err(ProtocolError::new(
            "invalid_event_type",
            Some("type"),
            "Unknown Responses client event",
        )),
    }
}

fn create(object: &Map<String, Value>, bytes: usize) -> Result<Create, ProtocolError> {
    let lane = LaneId::parse(object.get("stream_id"))?;
    let generate = match object.get("generate") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(value)) => *value,
        _ => {
            return Err(ProtocolError::new(
                "invalid_request_error",
                Some("generate"),
                "generate must be a boolean",
            )
            .in_lane(&lane));
        }
    };
    for field in ["stream", "background"] {
        if object.get(field).is_some_and(|value| !value.is_null()) {
            return Err(ProtocolError::new(
                "invalid_request_error",
                Some(field),
                "Do not send transport fields in a WebSocket create event",
            )
            .in_lane(&lane));
        }
    }
    let mut body = object.clone();
    for field in ["type", "stream_id", "generate", "stream", "background"] {
        body.remove(field);
    }
    // The existing harness streams canonical events; WebSocket framing owns the transport.
    body.insert("stream".into(), Value::Bool(true));
    Ok(Create {
        lane,
        generate,
        body: Value::Object(body),
        bytes,
    })
}

fn steer(object: &Map<String, Value>) -> Result<Steer, ProtocolError> {
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "type" | "previous_response_id" | "input"))
    {
        return Err(ProtocolError::new(
            "invalid_input",
            None,
            "Steering accepts only type, previous_response_id and input",
        ));
    }
    let previous = object
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| {
            ProtocolError::new(
                "invalid_input",
                Some("previous_response_id"),
                "Steering requires the target response ID",
            )
        })?;
    let input = object
        .get("input")
        .filter(|value| steering_input(value))
        .ok_or_else(|| {
            ProtocolError::new(
                "invalid_input",
                Some("input"),
                "Steering requires user message input",
            )
        })?;
    Ok(Steer {
        previous_response_id: previous.into(),
        input: input.clone(),
    })
}

fn steering_input(value: &Value) -> bool {
    match value {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => {
            !items.is_empty()
                && items.iter().all(|item| {
                    item.get("role").and_then(Value::as_str) == Some("user")
                        && item.get("type").is_none_or(|kind| kind == "message")
                        && item
                            .get("content")
                            .is_some_and(|content| content.is_string() || content.is_array())
                })
        }
        _ => false,
    }
}

/// An opaque, bounded reference. The execution owner supplies immutable branch state.
#[derive(Clone)]
pub(crate) struct Parent {
    pub(crate) response_id: String,
    pub(crate) token: String,
}

#[derive(Default)]
struct Lane {
    queue: VecDeque<Create>,
    active: bool,
    parent: Option<Parent>,
}

struct Running {
    lane: LaneId,
    previous: Option<String>,
    created: bool,
    terminal: bool,
}

pub(crate) struct Dispatch {
    pub(crate) id: u64,
    pub(crate) create: Create,
    pub(crate) parent: Option<Parent>,
}

pub(crate) struct Connection {
    opened_at_ms: u64,
    lanes: BTreeMap<LaneId, Lane>,
    ready: VecDeque<LaneId>,
    running: BTreeMap<u64, Running>,
    active_responses: BTreeMap<String, u64>,
    queued: usize,
    queued_bytes: usize,
    next: u64,
}

impl Connection {
    pub(crate) fn new(opened_at_ms: u64) -> Self {
        Self {
            opened_at_ms,
            lanes: BTreeMap::new(),
            ready: VecDeque::new(),
            running: BTreeMap::new(),
            active_responses: BTreeMap::new(),
            queued: 0,
            queued_bytes: 0,
            next: 0,
        }
    }

    pub(crate) fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.opened_at_ms) >= MAX_CONNECTION_MS
    }

    pub(crate) fn enqueue(&mut self, create: Create) -> Result<(), ProtocolError> {
        let named = self
            .lanes
            .keys()
            .filter(|lane| matches!(lane, LaneId::Named(_)))
            .count();
        if matches!(create.lane, LaneId::Named(_))
            && !self.lanes.contains_key(&create.lane)
            && named >= MAX_NAMED_LANES
        {
            return Err(ProtocolError::new(
                "websocket_stream_limit_reached",
                Some("stream_id"),
                "This connection has reached its named stream limit",
            )
            .in_lane(&create.lane));
        }
        if self.queued >= MAX_QUEUED
            || create.bytes > MAX_QUEUE_BYTES.saturating_sub(self.queued_bytes)
        {
            return Err(ProtocolError::new(
                "websocket_queue_limit_reached",
                None,
                "This connection's pending request queue is full",
            )
            .in_lane(&create.lane));
        }
        self.queued += 1;
        self.queued_bytes += create.bytes;
        let lane = self.lanes.entry(create.lane.clone()).or_default();
        if !lane.active && lane.queue.is_empty() {
            self.ready.push_back(create.lane.clone());
        }
        lane.queue.push_back(create);
        Ok(())
    }

    pub(crate) fn dispatch(&mut self) -> Vec<Dispatch> {
        let mut output = vec![];
        while self.running.len() < MAX_ACTIVE {
            let Some(lane_id) = self.ready.pop_front() else {
                break;
            };
            let lane = self.lanes.get_mut(&lane_id).unwrap();
            let create = lane.queue.pop_front().unwrap();
            lane.active = true;
            self.queued -= 1;
            self.queued_bytes -= create.bytes;
            let parent = create.previous().and_then(|id| self.parent(id)).cloned();
            let id = self.next;
            self.next += 1;
            self.running.insert(
                id,
                Running {
                    lane: lane_id,
                    previous: create.previous().map(str::to_owned),
                    created: false,
                    terminal: false,
                },
            );
            output.push(Dispatch { id, create, parent });
        }
        output
    }

    fn parent(&self, id: &str) -> Option<&Parent> {
        self.lanes
            .values()
            .filter_map(|lane| lane.parent.as_ref())
            .find(|parent| parent.response_id == id)
    }

    pub(crate) fn response_started(&mut self, job: u64, response_id: String) -> bool {
        if !valid_response_id(&response_id)
            || !self.running.contains_key(&job)
            || self.active_responses.contains_key(&response_id)
        {
            return false;
        }
        self.active_responses.insert(response_id, job);
        let running = self.running.get_mut(&job).unwrap();
        running.created = true;
        running.terminal = false;
        true
    }

    fn response_finished(&mut self, job: u64, response_id: Option<&str>) -> bool {
        let Some(running) = self.running.get_mut(&job) else {
            return false;
        };
        if let Some(id) = response_id {
            if self.active_responses.get(id) != Some(&job) {
                return false;
            }
            self.active_responses.remove(id);
        } else {
            self.active_responses.retain(|_, active| *active != job);
        }
        running.terminal = true;
        true
    }

    fn complete(&self, job: u64) -> bool {
        self.running
            .get(&job)
            .is_some_and(|running| running.created && running.terminal)
            && !self.active_responses.values().any(|active| *active == job)
    }

    pub(crate) fn steering_lane(&self, response_id: &str) -> Result<LaneId, ProtocolError> {
        if let Some(running) = self
            .active_responses
            .get(response_id)
            .and_then(|id| self.running.get(id))
        {
            return Ok(running.lane.clone());
        }
        let code = if self.parent(response_id).is_some() {
            "response_already_completed"
        } else {
            "response_not_found"
        };
        Err(ProtocolError::new(
            code,
            Some("previous_response_id"),
            "The target response is not active on this connection",
        ))
    }

    pub(crate) fn cache(&mut self, job: u64, parent: Parent) -> bool {
        if parent.token.is_empty()
            || parent.token.len() > MAX_PARENT_TOKEN_BYTES
            || !valid_response_id(&parent.response_id)
        {
            return false;
        }
        let Some(running) = self.running.get(&job) else {
            return false;
        };
        self.lanes.get_mut(&running.lane).unwrap().parent = Some(parent);
        true
    }

    pub(crate) fn finish(&mut self, job: u64, status: Option<u16>) -> bool {
        let Some(running) = self.running.remove(&job) else {
            return false;
        };
        self.active_responses.retain(|_, active| *active != job);
        let lane = self.lanes.get_mut(&running.lane).unwrap();
        if status.is_some_and(|status| (400..600).contains(&status))
            && lane.parent.as_ref().is_some_and(|parent| {
                Some(parent.response_id.as_str()) == running.previous.as_deref()
            })
        {
            lane.parent = None;
        }
        lane.active = false;
        if !lane.queue.is_empty() {
            self.ready.push_back(running.lane);
        }
        true
    }
}

pub(crate) fn authorize_browser_upgrade(
    browser_session: bool,
    origin: Option<&str>,
    app_origin: &str,
) -> Result<(), ProtocolError> {
    if browser_session && origin != Some(app_origin) {
        let mut error = ProtocolError::new(
            "origin_not_allowed",
            None,
            "Browser WebSockets require the application origin",
        );
        error.status = 403;
        return Err(error);
    }
    Ok(())
}

enum JobResult {
    Opened(u64, Result<ServerEvents, RequestFailure>),
    Event(
        u64,
        ServerEvents,
        Option<Result<ServerEvent, RequestFailure>>,
    ),
    Steered(LaneId, Result<Vec<ServerEvent>, RequestFailure>),
}

type Job = LocalBoxFuture<'static, JobResult>;

enum Incoming {
    Socket(Option<worker::Result<WebsocketEvent>>),
    Job(Option<JobResult>),
    Expired,
}

fn next_event(id: u64, mut events: ServerEvents) -> Job {
    async move {
        let event = events.next().await;
        JobResult::Event(id, events, event)
    }
    .boxed_local()
}

fn send(socket: &WebSocket, value: &Value) -> worker::Result<()> {
    let native: &web_sys::WebSocket = socket.as_ref();
    if native.buffered_amount() > MAX_OUTBOUND_BUFFER_BYTES {
        let _ = socket.close(Some(1013), Some("Reader cannot keep up"));
        return Err(worker::Error::RustError("websocket_reader_slow".into()));
    }
    let text = serde_json::to_string(value)?;
    if text.len() > MAX_OUTBOUND_BUFFER_BYTES as usize {
        let _ = socket.close(
            Some(1009),
            Some("Response event exceeds the transport limit"),
        );
        return Err(worker::Error::RustError("websocket_event_too_large".into()));
    }
    socket.send_with_str(text)
}

fn send_server(
    socket: &WebSocket,
    state: &mut Connection,
    id: u64,
    mut event: ServerEvent,
) -> worker::Result<()> {
    if !event.value.is_object() || event.value.get("type").and_then(Value::as_str).is_none() {
        return Err(worker::Error::RustError("websocket_event_invalid".into()));
    }
    let Some(running) = state.running.get(&id) else {
        return Err(worker::Error::RustError("websocket_stale_dispatch".into()));
    };
    let lane = running.lane.clone();
    if event.value["type"] == "response.created" {
        let response_id = event
            .value
            .pointer("/response/id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                worker::Error::RustError("websocket_response_identity_missing".into())
            })?;
        if !state.response_started(id, response_id.to_owned()) {
            return Err(worker::Error::RustError(
                "websocket_response_identity_conflict".into(),
            ));
        }
    }
    match event.value.get("type").and_then(Value::as_str) {
        Some("response.completed" | "response.failed" | "response.incomplete") => {
            let response = event.value.pointer("/response/id").and_then(Value::as_str);
            if response.is_none() || !state.response_finished(id, response) {
                return Err(worker::Error::RustError(
                    "websocket_terminal_identity_conflict".into(),
                ));
            }
        }
        Some("error") => {
            state.response_finished(id, None);
        }
        _ => {}
    }
    if let Some(parent) = event.parent {
        if event.value.pointer("/response/id").and_then(Value::as_str)
            != Some(parent.response_id.as_str())
        {
            return Err(worker::Error::RustError(
                "websocket_parent_identity_conflict".into(),
            ));
        }
        if !state.cache(id, parent) {
            return Err(worker::Error::RustError(
                "websocket_parent_reference_invalid".into(),
            ));
        }
    }
    lane.tag(&mut event.value);
    send(socket, &event.value)
}

fn complete_job(
    socket: &WebSocket,
    state: &mut Connection,
    jobs: &mut FuturesUnordered<Job>,
    result: JobResult,
) -> worker::Result<()> {
    match result {
        JobResult::Opened(id, Ok(events)) => jobs.push(next_event(id, events)),
        JobResult::Event(id, events, Some(Ok(event))) => {
            send_server(socket, state, id, event)?;
            jobs.push(next_event(id, events));
        }
        JobResult::Event(id, _, None) => {
            if !state.complete(id) {
                if let Some(running) = state.running.get(&id) {
                    send(socket, &RequestFailure {
                        status:503,
                        error:json!({"type":"server_error", "code":"response_stream_interrupted", "message":"Observation ended before its original terminal outcome"}),
                    }.value(&running.lane))?;
                }
                state.finish(id, Some(503));
            } else {
                state.finish(id, None);
            }
        }
        JobResult::Opened(id, Err(error)) | JobResult::Event(id, _, Some(Err(error))) => {
            if let Some(running) = state.running.get(&id) {
                send(socket, &error.value(&running.lane))?;
            }
            state.finish(id, Some(error.status));
        }
        JobResult::Steered(lane, Ok(events)) => {
            for mut event in events {
                if event.parent.is_some() {
                    return Err(worker::Error::RustError(
                        "websocket_control_parent_invalid".into(),
                    ));
                }
                lane.tag(&mut event.value);
                send(socket, &event.value)?;
            }
        }
        JobResult::Steered(lane, Err(error)) => send(socket, &error.value(&lane))?,
    }
    Ok(())
}

async fn serve(
    socket: &WebSocket,
    incoming: &mut worker::EventStream<'_>,
    executor: Rc<dyn Executor>,
) -> worker::Result<()> {
    let mut state = Connection::new(crate::util::now_ms());
    let mut jobs = FuturesUnordered::<Job>::new();
    let lifetime = Delay::from(Duration::from_millis(MAX_CONNECTION_MS)).fuse();
    futures::pin_mut!(lifetime);
    loop {
        for dispatch in state.dispatch() {
            let id = dispatch.id;
            let future = executor.create(dispatch);
            jobs.push(async move { JobResult::Opened(id, future.await) }.boxed_local());
        }
        let ready = {
            let socket_event = incoming.next().fuse();
            let job_event = async {
                if jobs.is_empty() {
                    futures::future::pending().await
                } else {
                    jobs.next().await
                }
            }
            .fuse();
            futures::pin_mut!(socket_event, job_event);
            futures::select! {
                _ = lifetime => Incoming::Expired,
                event = socket_event => Incoming::Socket(event),
                event = job_event => Incoming::Job(event),
            }
        };
        match ready {
            Incoming::Expired => {
                send(
                    socket,
                    &ProtocolError::new(
                        "websocket_connection_limit_reached",
                        None,
                        "Create a new WebSocket connection after sixty minutes",
                    )
                    .value(),
                )?;
                return socket.close(Some(1000), Some("Connection lifetime reached"));
            }
            Incoming::Socket(None | Some(Ok(WebsocketEvent::Close(_)))) => return Ok(()),
            Incoming::Socket(Some(Err(error))) => return Err(error),
            Incoming::Job(Some(result)) => complete_job(socket, &mut state, &mut jobs, result)?,
            Incoming::Job(None) => {}
            Incoming::Socket(Some(Ok(WebsocketEvent::Message(message)))) => {
                receive(socket, &mut state, &mut jobs, &executor, message)?;
            }
        }
    }
}

fn receive(
    socket: &WebSocket,
    state: &mut Connection,
    jobs: &mut FuturesUnordered<Job>,
    executor: &Rc<dyn Executor>,
    message: worker::MessageEvent,
) -> worker::Result<()> {
    let event = message
        .text()
        .ok_or_else(|| ProtocolError::new("invalid_request_error", None, "Send JSON text events"))
        .and_then(|text| parse(&text));
    match event {
        Ok(ClientEvent::Create(create)) => {
            if let Err(error) = state.enqueue(create) {
                send(socket, &error.value())?;
            }
        }
        Ok(ClientEvent::Steer(steer)) => steer_command(socket, state, jobs, executor, steer)?,
        Err(error) => send(socket, &error.value())?,
    }
    Ok(())
}

fn steer_command(
    socket: &WebSocket,
    state: &Connection,
    jobs: &mut FuturesUnordered<Job>,
    executor: &Rc<dyn Executor>,
    request: Steer,
) -> worker::Result<()> {
    let lane = match state.steering_lane(&request.previous_response_id) {
        Ok(lane) => lane,
        Err(error) => return send(socket, &error.value()),
    };
    if jobs.len().saturating_sub(state.running.len()) >= MAX_PENDING_CONTROLS {
        return send(
            socket,
            &ProtocolError::new(
                "websocket_queue_limit_reached",
                None,
                "Too many pending connection control requests",
            )
            .in_lane(&lane)
            .value(),
        );
    }
    let future = executor.steer(request);
    jobs.push(async move { JobResult::Steered(lane, future.await) }.boxed_local());
    Ok(())
}

/// Authentication, cookie-origin validation and upgrade-header checks precede this call.
pub(crate) async fn upgrade(executor: Rc<dyn Executor>) -> worker::Result<Response> {
    let pair = WebSocketPair::new()?;
    let server = pair.server;
    let (ready, setup) = futures::channel::oneshot::channel();
    wasm_bindgen_futures::spawn_local(async move {
        let mut events = match server.events() {
            Ok(events) => events,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        if let Err(error) = server.accept() {
            let _ = ready.send(Err(error));
            return;
        }
        let _ = ready.send(Ok(()));
        if serve(&server, &mut events, executor).await.is_err() {
            let _ = server.close(Some(1011), Some("Observation connection ended"));
        }
    });
    setup
        .await
        .map_err(|_| worker::Error::RustError("websocket_setup_failed".into()))??;
    Response::from_websocket(pair.client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn request(lane: Option<&str>, previous: Option<&str>, input: &str) -> Create {
        let mut event = json!({"type":"response.create", "model":"agent", "input":input});
        if let Some(lane) = lane {
            event["stream_id"] = json!(lane);
        }
        if let Some(previous) = previous {
            event["previous_response_id"] = json!(previous);
        }
        match parse(&event.to_string()).unwrap() {
            ClientEvent::Create(create) => create,
            _ => unreachable!(),
        }
    }

    #[test]
    fn lanes_are_independent_of_lineage_and_transport_fields() {
        let create = request(Some("main.1"), Some("resp_parent"), "new input");
        assert_eq!(create.lane, LaneId::Named("main.1".into()));
        assert_eq!(create.previous(), Some("resp_parent"));
        assert!(create.generate);
        assert_eq!(create.body["stream"], true);
        assert!(create.body.get("stream_id").is_none());
        let mut event = json!({"type":"response.created", "stream_id":"forged"});
        LaneId::Default.tag(&mut event);
        assert!(event.get("stream_id").is_none());
        create.lane.tag(&mut event);
        assert_eq!(event["stream_id"], "main.1");
    }

    #[test]
    fn invalid_lane_never_consumes_a_distinct_lane_slot() {
        for lane in ["", "has space", "../", "雪"] {
            let error = parse(&json!({"type":"response.create","stream_id":lane}).to_string())
                .err()
                .unwrap();
            assert_eq!(error.code, "invalid_stream_id");
            assert!(error.value().get("stream_id").is_none());
        }
        assert_eq!(
            LaneId::parse(Some(&json!("a".repeat(256)))).unwrap(),
            LaneId::Named("a".repeat(256))
        );
        assert!(LaneId::parse(Some(&json!("a".repeat(257)))).is_err());
    }

    #[test]
    fn one_lane_is_fifo_while_sixteen_other_lanes_can_execute() {
        let mut connection = Connection::new(100);
        connection
            .enqueue(request(None, None, "default first"))
            .unwrap();
        connection
            .enqueue(request(None, None, "default second"))
            .unwrap();
        for n in 0..16 {
            connection
                .enqueue(request(Some(&format!("lane{n}")), None, "named"))
                .unwrap();
        }
        let running = connection.dispatch();
        assert_eq!(running.len(), 16);
        assert_eq!(running[0].create.body["input"], "default first");
        assert_eq!(connection.dispatch().len(), 0);
        connection.finish(running[0].id, None);
        let next = connection.dispatch();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].create.lane, LaneId::Named("lane15".into()));
        connection.finish(running[1].id, None);
        let next = connection.dispatch();
        assert_eq!(next[0].create.body["input"], "default second");
    }

    #[test]
    fn named_limit_excludes_default_and_counts_distinct_historical_names() {
        let mut connection = Connection::new(0);
        for n in 0..MAX_NAMED_LANES {
            connection
                .enqueue(request(Some(&format!("lane{n}")), None, "named"))
                .unwrap();
        }
        connection.enqueue(request(None, None, "default")).unwrap();
        assert_eq!(
            connection
                .enqueue(request(Some("extra"), None, "new"))
                .err()
                .unwrap()
                .code,
            "websocket_stream_limit_reached"
        );
        connection
            .enqueue(request(Some("lane0"), None, "reuse"))
            .unwrap();
    }

    #[test]
    fn parent_cache_is_resolved_at_dispatch_and_failures_only_evict_same_lane() {
        let mut connection = Connection::new(0);
        connection
            .enqueue(request(Some("main"), None, "initial"))
            .unwrap();
        let initial = connection.dispatch().pop().unwrap();
        connection.cache(
            initial.id,
            Parent {
                response_id: "resp_parent".into(),
                token: "ref-parent".into(),
            },
        );
        connection.finish(initial.id, None);
        connection
            .enqueue(request(Some("critic"), Some("resp_parent"), "fork"))
            .unwrap();
        let fork = connection.dispatch().pop().unwrap();
        assert_eq!(fork.parent.as_ref().unwrap().token, "ref-parent");
        connection.finish(fork.id, Some(400));
        assert!(connection.parent("resp_parent").is_some());
        connection
            .enqueue(request(Some("main"), Some("resp_parent"), "continue"))
            .unwrap();
        let same = connection.dispatch().pop().unwrap();
        connection.finish(same.id, Some(500));
        assert!(connection.parent("resp_parent").is_none());
    }

    #[test]
    fn steering_cannot_choose_a_different_lane_or_run() {
        assert!(parse(&json!({"type":"response.steer","previous_response_id":"resp_1","input":"update","stream_id":"other"}).to_string()).is_err());
        assert!(parse(&json!({"type":"response.steer","previous_response_id":"resp_1","input":[{"role":"assistant","content":"fake"}]}).to_string()).is_err());
        let mut connection = Connection::new(0);
        connection
            .enqueue(request(Some("main"), None, "initial"))
            .unwrap();
        let initial = connection.dispatch().pop().unwrap();
        assert!(connection.response_started(initial.id, "resp_1".into()));
        assert_eq!(
            connection.steering_lane("resp_1").unwrap(),
            LaneId::Named("main".into())
        );
        connection.cache(
            initial.id,
            Parent {
                response_id: "resp_1".into(),
                token: "ref".into(),
            },
        );
        connection.finish(initial.id, None);
        assert_eq!(
            connection.steering_lane("resp_1").err().unwrap().code,
            "response_already_completed"
        );
    }

    #[test]
    fn connection_deadline_and_browser_origin_do_not_grant_mutation_authority() {
        let connection = Connection::new(100);
        assert!(!connection.expired(100 + MAX_CONNECTION_MS - 1));
        assert!(connection.expired(100 + MAX_CONNECTION_MS));
        assert!(authorize_browser_upgrade(true, None, "https://adash.test").is_err());
        assert!(
            authorize_browser_upgrade(true, Some("https://evil.test"), "https://adash.test")
                .is_err()
        );
        assert!(
            authorize_browser_upgrade(true, Some("https://adash.test"), "https://adash.test")
                .is_ok()
        );
        assert!(authorize_browser_upgrade(false, None, "https://adash.test").is_ok());
    }

    #[test]
    fn queued_fork_does_not_capture_an_evicted_no_store_parent() {
        let mut connection = Connection::new(0);
        connection
            .enqueue(request(Some("main"), None, "initial"))
            .unwrap();
        let initial = connection.dispatch().pop().unwrap();
        connection.cache(
            initial.id,
            Parent {
                response_id: "resp_old".into(),
                token: "old".into(),
            },
        );
        connection.finish(initial.id, None);
        for n in 0..MAX_ACTIVE {
            connection
                .enqueue(request(Some(&format!("busy{n}")), None, "busy"))
                .unwrap();
        }
        let busy = connection.dispatch();
        connection
            .enqueue(request(Some("main"), Some("resp_old"), "advance"))
            .unwrap();
        connection
            .enqueue(request(Some("critic"), Some("resp_old"), "fork"))
            .unwrap();
        assert!(connection.dispatch().is_empty());
        connection.finish(busy[0].id, None);
        let advanced = connection.dispatch().pop().unwrap();
        assert_eq!(advanced.create.lane, LaneId::Named("main".into()));
        connection.cache(
            advanced.id,
            Parent {
                response_id: "resp_new".into(),
                token: "new".into(),
            },
        );
        connection.finish(advanced.id, None);
        let fork = connection.dispatch().pop().unwrap();
        assert!(fork.parent.is_none());
        assert_eq!(fork.create.previous(), Some("resp_old"));
    }

    #[test]
    fn queue_bytes_are_released_only_when_work_dispatches() {
        let mut connection = Connection::new(0);
        let mut full = request(None, None, "queued");
        full.bytes = MAX_QUEUE_BYTES;
        connection.enqueue(full).unwrap();
        assert_eq!(
            connection
                .enqueue(request(None, None, "excess"))
                .err()
                .unwrap()
                .code,
            "websocket_queue_limit_reached"
        );
        assert_eq!(connection.queued, 1);
        assert_eq!(connection.dispatch().len(), 1);
        assert_eq!(connection.queued_bytes, 0);
        connection.enqueue(request(None, None, "later")).unwrap();
        assert!(connection.dispatch().is_empty());
    }

    #[test]
    fn warmup_is_explicit_and_terminal_observation_is_not_invented_at_eof() {
        let ClientEvent::Create(create) =
            parse(r#"{"type":"response.create","generate":false,"input":[]}"#).unwrap()
        else {
            panic!("create")
        };
        assert!(!create.generate);
        assert!(create.body.get("generate").is_none());
        assert!(parse(r#"{"type":"response.create","stream":true}"#).is_err());
        let mut connection = Connection::new(0);
        connection.enqueue(request(None, None, "input")).unwrap();
        let job = connection.dispatch().pop().unwrap().id;
        assert!(!connection.complete(job));
        connection.response_started(job, "resp_one".into());
        assert!(!connection.complete(job));
        assert!(!connection.response_finished(job, Some("resp_other")));
        assert!(connection.response_finished(job, Some("resp_one")));
        assert!(connection.complete(job));
        connection.response_started(job, "resp_successor".into());
        assert!(!connection.complete(job));
        assert!(connection.response_finished(job, Some("resp_successor")));
        assert!(connection.complete(job));
    }

    proptest! {
        #[test]
        fn interleaved_lane_operations_preserve_capacity_and_single_owner(
            operations in prop::collection::vec((0u8..40, any::<bool>()), 0..250),
        ) {
            let mut connection = Connection::new(0);
            for (index, enqueue) in operations {
                if enqueue {
                    let name = format!("lane{index}");
                    let lane = (index != 39).then_some(name.as_str());
                    let _ = connection.enqueue(request(lane, None, "input"));
                } else if !connection.running.is_empty() {
                    let index = usize::from(index) % connection.running.len();
                    let id = *connection.running.keys().nth(index).unwrap();
                    connection.finish(id, None);
                }
                connection.dispatch();
                prop_assert!(connection.running.len() <= MAX_ACTIVE);
                prop_assert!(connection.queued <= MAX_QUEUED);
                prop_assert!(connection.queued_bytes <= MAX_QUEUE_BYTES);
                prop_assert!(connection.lanes.keys().filter(|lane| matches!(lane, LaneId::Named(_))).count() <= MAX_NAMED_LANES);
                prop_assert_eq!(connection.queued, connection.lanes.values().map(|lane| lane.queue.len()).sum::<usize>());
                for (id, lane) in &connection.lanes {
                    let owners = connection.running.values().filter(|run| &run.lane == id).count();
                    prop_assert_eq!(owners, usize::from(lane.active));
                }
            }
        }
    }
}
