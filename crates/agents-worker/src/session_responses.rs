use super::*;
use crate::responses_wire::{self as wire, ResponseRequest};
use std::collections::BTreeMap;

const RESPONSE_PREFIX: &str = "response:";
const MAX_RESPONSE_EVENTS: usize = 65_536;
const MAX_RESPONSE_EVENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESPONSE_OBSERVERS: usize = 8;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ResponseRun {
    pub(super) id: String,
    pub(super) request: ResponseRequest,
    pub(super) created_at: u64,
    pub(super) input_items: Vec<Value>,
    pub(super) output: Vec<Value>,
    pub(super) events: Vec<Value>,
    pub(super) status: String,
    pub(super) error: Option<String>,
    pub(super) incomplete_reason: Option<String>,
    pub(super) cancel_requested: bool,
    pub(super) awaiting_calls: Vec<provider::ToolCall>,
    pub(super) usage: Usage,
    pub(super) next_sequence: u64,
    #[serde(default)]
    pub(super) reserved_sequence_end: u64,
    #[serde(default)]
    pub(super) event_bytes: usize,
    #[serde(default)]
    pub(super) completed_at: Option<u64>,
    #[serde(default)]
    pub(super) stream_error: Option<String>,
    pub(super) stream_item: Option<String>,
    #[serde(default)]
    pub(super) deleted: bool,
    #[serde(default)]
    pub(super) attachments: Vec<crate::space::OwnedFile>,
    #[serde(default)]
    pub(super) attachments_ready: usize,
}

impl ResponseRun {
    pub(super) fn recover_sequence(&mut self) {
        self.next_sequence = self.next_sequence.max(self.reserved_sequence_end);
        self.reserved_sequence_end = self
            .next_sequence
            .saturating_add(MAX_RESPONSE_EVENTS as u64 + 16);
    }

    fn value(&self) -> Value {
        let mut value = wire::response_value(
            &self.request,
            &self.id,
            self.created_at,
            &self.status,
            &self.output,
            Some(&self.usage),
        );
        value["completed_at"] = json!(self.completed_at);
        if self.cancel_requested {
            value["agents_cancel_requested"] = json!(true);
        }
        if let Some(code) = &self.error {
            value["error"] = json!({"code":"server_error","message":"Agent execution failed; inspect session diagnostics"});
            value["agents_error_code"] = json!(code);
        }
        if let Some(reason) = &self.incomplete_reason {
            value["incomplete_details"] = json!({"reason":reason});
        }
        value
    }

    fn event(&mut self, kind: &str, mut fields: Value) -> Result<Value> {
        if self.next_sequence >= self.reserved_sequence_end {
            self.stream_error = Some("response_event_limit".into());
            return Err(Error::RustError("response_event_limit".into()));
        }
        fields["type"] = json!(kind);
        fields["sequence_number"] = json!(self.next_sequence);
        let bytes = serde_json::to_vec(&fields)?.len();
        if bytes > MAX_RESPONSE_EVENT_BYTES {
            self.stream_error = Some("response_event_limit".into());
            return Err(Error::RustError("response_event_limit".into()));
        }
        // Retention is a replay window, never a reason to strand a terminal run.
        // Drop an amortized prefix; callers with an expired cursor use retrieval.
        while !self.events.is_empty()
            && (self.events.len() >= MAX_RESPONSE_EVENTS
                || self.event_bytes.saturating_add(bytes) > MAX_RESPONSE_EVENT_BYTES)
        {
            let count = self.events.len().div_ceil(4);
            let removed = self
                .events
                .drain(..count)
                .map(|event| serde_json::to_vec(&event).map(|b| b.len()).unwrap_or(0))
                .sum::<usize>();
            self.event_bytes = self.event_bytes.saturating_sub(removed);
        }
        self.next_sequence += 1;
        self.event_bytes += bytes;
        self.events.push(fields.clone());
        Ok(fields)
    }
}

enum ResponseTransport {
    Stream(mpsc::Sender<Result<Vec<u8>>>),
    Json(crate::observer_transport::Sender),
}

pub(super) struct ResponseObserver {
    id: String,
    transport: ResponseTransport,
    after: Option<u64>,
}

#[derive(Deserialize)]
struct CreateResponse {
    response_id: String,
    request: ResponseRequest,
}

impl SessionInner {
    pub(super) async fn workspace_action(&self, mut req: Request) -> Result<Response> {
        let value: Value = req.json().await?;
        let restore = req.path().ends_with("/restore");
        let Some(object) = value.as_object() else {
            return util::json_error(
                400,
                "invalid_request",
                "Workspace operation must be an object",
            );
        };
        let allowed = if restore {
            vec!["operation_id", "version_id"]
        } else {
            vec!["operation_id", "name"]
        };
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return util::json_error(400, "invalid_request", "Unknown workspace operation field");
        }
        let Some(operation) = value["operation_id"].as_str().filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        }) else {
            return util::json_error(
                400,
                "invalid_operation_id",
                "Provide a stable operation_id of 1 to 128 letters, digits, underscores or hyphens",
            );
        };
        let record = self.snapshot()?.record;
        if !record.pinned.workspace {
            return util::json_error(404, "workspace_disabled", "Agent has no workspace");
        }
        if self.running.get()
            || self.closing.get()
            || self.snapshot()?.run.is_some()
            || self.snapshot()?.archive_pending
            || self.snapshot()?.archive_preparing
        {
            return util::json_error(
                409,
                "session_busy",
                "Wait for the active task before modifying workspace state",
            );
        }
        if record.status == SessionStatus::Closed {
            return util::json_error(
                409,
                "session_closed",
                "Resume the session before modifying its workspace",
            );
        }
        let arguments = if restore {
            tools::parse_workspace_restore(
                &json!({"version_id":value.get("version_id").cloned().unwrap_or(Value::Null)})
                    .to_string(),
            )
            .map(|_| ())
        } else {
            tools::parse_workspace_commit(
                &json!({"name":value.get("name").cloned().unwrap_or(Value::Null)}).to_string(),
            )
            .map(|_| ())
        };
        if let Err(message) = arguments {
            return util::json_error(400, "invalid_tool_arguments", &message);
        }
        self.closing.set(true);
        let result = async {
            let workspace = self.workspace(&record);
            let mut output = if restore {
                workspace
                    .restore_version(operation, value["version_id"].as_str().unwrap())
                    .await?
            } else {
                workspace
                    .commit_version(operation, value.get("name").and_then(Value::as_str))
                    .await?
            };
            output["operation_id"] = json!(operation);
            let metadata = workspace.snapshot_metadata().await?;
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .record
                .workspace_snapshot = metadata;
            self.persist().await?;
            self.arm_alarm().await?;
            Response::from_json(&output)
        }
        .await;
        self.closing.set(false);
        result
    }

    pub(super) async fn response_route(&self, mut req: Request) -> Result<Response> {
        let path = req.path();
        let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
        if req.method() == Method::Post && parts.len() == 1 {
            let create: CreateResponse = req.json().await?;
            return self
                .create_response(create, req.headers().get("traceparent")?)
                .await;
        }
        let Some(id) = parts.get(1) else {
            return util::json_error(404, "not_found", "Response not found");
        };
        let Some(mut response) = self.read_response(id).await? else {
            return util::json_error(404, "not_found", "Response not found");
        };
        if response.deleted || (!response.request.store && terminal_status(&response.status)) {
            return util::json_error(404, "not_found", "Response not retained");
        }
        match (req.method(), parts.get(2).copied()) {
            (Method::Get, None) => Response::from_json(&response.value()),
            (Method::Get, Some("events")) => {
                let url = req.url()?;
                let after = url
                    .query_pairs()
                    .find(|(key, _)| key == "after")
                    .map(|(_, v)| v.into_owned());
                let after = match after.as_deref().map(str::parse::<u64>).transpose() {
                    Ok(value) => value,
                    Err(_) => {
                        return util::json_error(400, "invalid_cursor", "Invalid event cursor");
                    }
                };
                self.observe_response(&response, true, after).await
            }
            (Method::Get, Some("input_items")) => input_items(&response, &req.url()?),
            (Method::Post, Some("cancel")) => {
                if !response.request.background {
                    return util::json_error(
                        400,
                        "response_not_background",
                        "Only background responses can be cancelled",
                    );
                }
                if !terminal_status(&response.status) {
                    let mut borrowed = self.data.borrow_mut();
                    let data = borrowed.as_mut().unwrap();
                    if let Some(active) = data
                        .run
                        .as_mut()
                        .and_then(|r| r.response.as_mut())
                        .filter(|r| r.id == *id)
                    {
                        active.cancel_requested = true;
                        response.cancel_requested = true;
                    }
                    drop(borrowed);
                    self.persist().await?;
                    self.arm_alarm().await?;
                }
                Response::from_json(&response.value())
            }
            (Method::Delete, None) => {
                if !terminal_status(&response.status) {
                    return util::json_error(
                        409,
                        "response_in_progress",
                        "Cancel and wait for the response before deletion",
                    );
                }
                {
                    let borrowed = self.data.borrow();
                    let data = borrowed.as_ref().unwrap();
                    if data.response_head.as_deref() == Some(*id)
                        && !data.pending_client_tools.is_empty()
                    {
                        return util::json_error(
                            409,
                            "client_tool_outputs_required",
                            "Complete or explicitly decline outstanding function calls before deleting their response",
                        );
                    }
                }
                {
                    let mut borrowed = self.data.borrow_mut();
                    let data = borrowed.as_mut().unwrap();
                    if data.response_head.as_deref() == Some(*id) {
                        data.response_head = None;
                    }
                }
                self.persist().await?;
                response.deleted = true;
                response.request.input = Value::Null;
                response.request.instructions = None;
                response.request.tools.clear();
                response.request.metadata.clear();
                response.request.text = None;
                response.request.reasoning = None;
                response.request.stream_options = None;
                response.attachments.clear();
                response.input_items.clear();
                response.output.clear();
                response.events.clear();
                self.write_response(&response).await?;
                Response::from_json(&json!({"id":id,"object":"response.deleted","deleted":true}))
            }
            _ => util::json_error(405, "method_not_allowed", "Unsupported response operation"),
        }
    }

    async fn existing_response(
        &self,
        existing: ResponseRun,
        request: &ResponseRequest,
    ) -> Result<Response> {
        if existing.deleted || (!existing.request.store && terminal_status(&existing.status)) {
            return util::json_error(404, "not_found", "Response has been deleted");
        }
        if serde_json::to_value(&existing.request)? != serde_json::to_value(&request)? {
            return util::json_error(
                409,
                "idempotency_conflict",
                "Response ID already belongs to another request",
            );
        }
        if !terminal_status(&existing.status) {
            // Repair a warm retry's failed initial state/alarm publication,
            // without appending input or admitting a second execution.
            self.persist().await?;
            self.arm_alarm().await?;
        }
        self.observe_response(&existing, request.stream, None).await
    }

    async fn create_response(
        &self,
        create: CreateResponse,
        traceparent: Option<String>,
    ) -> Result<Response> {
        let request = create.request;
        if let Err(error) = request.validate() {
            return util::json_error(400, "invalid_request_error", &error.to_string());
        }
        if let Some(existing) = self.read_response(&create.response_id).await? {
            return self.existing_response(existing, &request).await;
        }
        let mut input = match crate::response_preflight::validate(&request) {
            Ok(input) => input,
            Err(error) => {
                return util::json_error(400, error.code, &error.message);
            }
        };
        let data = self.snapshot()?;
        if data.run.is_some()
            || self.closing.get()
            || data.archive_preparing
            || data.archive_pending
        {
            return util::json_error(
                409,
                "session_busy",
                "The session already has an active task",
            );
        }
        if request.model != data.record.agent_id {
            return util::json_error(
                400,
                "model_mismatch",
                "Response model must be the session agent",
            );
        }
        if request.previous_response_id != data.response_head {
            return util::json_error(
                409,
                "response_fork_not_supported",
                "Continue the current response head; history forks are not supported",
            );
        }
        if let Some(parent) = request.previous_response_id.as_deref() {
            let Some(parent) = self.read_response(parent).await? else {
                return util::json_error(404, "not_found", "Previous response not retained");
            };
            if parent.deleted || !parent.request.store {
                return util::json_error(404, "not_found", "Previous response not retained");
            }
        }
        if request.previous_response_id.is_some() {
            if let Err(code) =
                validate_tool_outputs(&data.pending_client_tools, &input.tool_outputs)
            {
                return util::json_error(
                    400,
                    code,
                    "Provide each outstanding function result exactly once before continuing",
                );
            }
        }
        let mut definitions = tool_definitions(&data.record);
        let local = request.tool_definitions().map_err(Error::RustError)?;
        definitions.extend(local);
        let mut config = self.provider_config(&data.record).await?;
        let options = request.generation_options();
        if let Err(error) = options.validate_for_config(&config, &definitions) {
            return util::json_error(400, "unsupported_generation_option", &error.to_string());
        }
        if let Some(limit) = request.max_output_tokens {
            if limit > data.record.pinned.model.max_output_tokens {
                return util::json_error(
                    400,
                    "output_budget_exceeded",
                    "Requested output exceeds the configured agent budget",
                );
            }
            config.max_output_tokens = Some(limit);
        }
        if data.record.status == SessionStatus::Closed {
            self.persist().await?;
            return util::json_error(
                409,
                "response_session_closed",
                "Readmit the archived session before continuing",
            );
        }
        // Credential lookup yields: recheck the head before accepting any input.
        let current = self.snapshot()?;
        if current.archive_preparing || current.archive_pending || self.closing.get() {
            return util::json_error(409, "session_busy", "Session archival is still in progress");
        }
        if current.record.status == SessionStatus::Closed {
            self.persist().await?;
            return util::json_error(
                409,
                "response_session_closed",
                "Readmit the archived session before continuing",
            );
        }
        if current.run.is_some()
            || current.response_head != data.response_head
            || current.record.input_revision != data.record.input_revision
        {
            return util::json_error(
                409,
                "session_busy",
                "Session changed while preparing this response",
            );
        }
        let original_items = wire::identified_input_items(&input.input_items, &create.response_id);
        let resolved = crate::responses_attachments::resolve(
            &self.env,
            &data.record.owner_id,
            &create.response_id,
            &request.input,
            data.record.pinned.workspace,
        )
        .await?;
        let mut execution_request = request.clone();
        execution_request.input = resolved.input;
        input = execution_request.normalize().map_err(Error::RustError)?;
        let current = self.snapshot()?;
        if current.archive_preparing || current.archive_pending || self.closing.get() {
            return util::json_error(409, "session_busy", "Session archival is still in progress");
        }
        if current.record.status == SessionStatus::Closed {
            self.persist().await?;
            return util::json_error(
                409,
                "response_session_closed",
                "Readmit the archived session before continuing",
            );
        }
        if current.run.is_some()
            || current.response_head != data.response_head
            || current.record.input_revision != data.record.input_revision
        {
            return util::json_error(409, "session_busy", "Session changed while resolving input");
        }
        let mut prefix = system_prefix(&data.record);
        for instruction in data
            .history_instructions
            .iter()
            .chain(input.history_instructions.iter())
        {
            prefix.push_str(&format!(
                "\n\nHistorical {} instruction:\n{}",
                instruction.role,
                instruction.content.as_str().unwrap_or("")
            ));
        }

        if !input.instructions.is_empty() {
            prefix.push_str("\n\nUser-provided instructions for this response:\n");
            prefix.push_str(&input.instructions.join("\n\n"));
        }
        if prefix.len() > 256 * 1024 {
            return util::json_error(
                400,
                "instructions_too_large",
                "Combined instruction prefix exceeds 256 KiB",
            );
        }
        let acceptance = self.persistence.lock().await;
        if let Some(existing) = self.read_response(&create.response_id).await? {
            drop(acceptance);
            return self.existing_response(existing, &request).await;
        }
        let before_pins = self.snapshot()?;
        if before_pins.run.is_some()
            || before_pins.archive_pending
            || before_pins.archive_preparing
            || self.closing.get()
            || before_pins.response_head != data.response_head
            || before_pins.record.input_revision != data.record.input_revision
            || before_pins.record.status != SessionStatus::Idle
        {
            drop(acceptance);
            if before_pins.record.status == SessionStatus::Closed {
                self.persist().await?;
                return util::json_error(
                    409,
                    "response_session_closed",
                    "Readmit the archived session before continuing",
                );
            }
            return util::json_error(
                409,
                "session_busy",
                "Session changed before attachment admission",
            );
        }
        crate::space::acquire_response_files(
            &self.env,
            &data.record.owner_id,
            &data.record.id,
            &create.response_id,
            &resolved.attachments,
        )
        .await?;
        let after_pins = self.snapshot()?;
        if after_pins.run.is_some()
            || after_pins.archive_pending
            || after_pins.archive_preparing
            || self.closing.get()
            || after_pins.response_head != data.response_head
            || after_pins.record.input_revision != data.record.input_revision
            || after_pins.record.status != SessionStatus::Idle
        {
            crate::space::release_response_files(
                &self.env,
                &data.record.owner_id,
                &data.record.id,
                &create.response_id,
            )
            .await?;
            drop(acceptance);
            if after_pins.record.status == SessionStatus::Closed {
                self.persist().await?;
                return util::json_error(
                    409,
                    "response_session_closed",
                    "Readmit the archived session before continuing",
                );
            }
            return util::json_error(
                409,
                "session_busy",
                "Session changed while pinning attachments",
            );
        }
        // No await separates this domain preflight from acceptance mutations.
        // Exhausted revisions and non-idle records cannot partially consume tools.
        let mut admission = current.record.clone();
        if admission.append_user_input(util::now_ms()).is_err() || admission.start_run().is_err() {
            return util::json_error(
                409,
                "session_busy",
                "Session cannot accept another response",
            );
        }
        {
            let mut borrowed = self.data.borrow_mut();
            let current = borrowed.as_mut().unwrap();
            current
                .history_instructions
                .extend(input.history_instructions.iter().cloned());
            current
                .journal
                .extend(input.history_instructions.iter().cloned());
            current.messages[0] = Message::text("system", prefix);
            current.pending_client_tools.clear();
        }
        self.append(input.messages)?;
        {
            let mut borrowed = self.data.borrow_mut();
            let current = borrowed.as_mut().unwrap();
            project_response_input(
                &mut current.messages,
                &mut current.pending,
                !data.pending_client_tools.is_empty(),
            );
        }
        self.start_run_with_id(create.response_id.clone())?;
        let mut response = ResponseRun {
            id: create.response_id.clone(),
            request: request.clone(),
            created_at: util::now_ms() / 1000,
            input_items: original_items,
            output: vec![],
            events: vec![],
            status: "queued".into(),
            error: None,
            incomplete_reason: None,
            cancel_requested: false,
            awaiting_calls: vec![],
            usage: Usage::default(),
            next_sequence: 0,
            reserved_sequence_end: MAX_RESPONSE_EVENTS as u64 + 16,
            event_bytes: 0,
            completed_at: None,
            stream_error: None,
            stream_item: None,
            deleted: false,
            attachments: resolved.attachments,
            attachments_ready: 0,
        };
        response.event("response.created", json!({"response":response.value()}))?;
        {
            let mut borrowed = self.data.borrow_mut();
            let current = borrowed.as_mut().unwrap();
            current.response_head = Some(create.response_id.clone());
            let run = current.run.as_mut().unwrap();
            run.worker_attached = !request.background;
            run.traceparent = traceparent;
            run.response = Some(response.clone());
        }
        {
            let mut accepted = self.snapshot()?;
            save_state(&self.state.storage(), &mut accepted).await?;
        }
        drop(acceptance);
        self.persist().await?;
        self.arm_alarm().await?;
        if request.background && !request.stream {
            return Response::from_json(&response.value());
        }
        self.observe_response(&response, request.stream, None).await
    }

    async fn observe_response(
        &self,
        response: &ResponseRun,
        stream: bool,
        after: Option<u64>,
    ) -> Result<Response> {
        // An alarm can complete while create waits for its storage acknowledgement.
        // Attach to the current run, never to a stale queued clone.
        let fresh = self.read_response(&response.id).await?;
        let response = fresh.as_ref().unwrap_or(response);
        if stream {
            if let Some(first) = response
                .events
                .first()
                .and_then(|event| event["sequence_number"].as_u64())
            {
                if first > after.map_or(0, |value| value.saturating_add(1)) {
                    return util::json_error(
                        410,
                        "event_cursor_expired",
                        "Event replay window expired; retrieve the complete response instead",
                    );
                }
            }
            if self.response_observers.borrow().len() >= MAX_RESPONSE_OBSERVERS {
                return util::json_error(
                    429,
                    "observer_limit",
                    "Too many active response observers",
                );
            }
            let (tx, rx) = mpsc::channel(32);
            let replay = response
                .events
                .iter()
                .filter(|event| {
                    after.is_none_or(|n| event["sequence_number"].as_u64().is_some_and(|s| s > n))
                })
                .map(|event| Ok(wire::event_frame(event)))
                .collect::<Vec<_>>();
            if !terminal_status(&response.status) {
                self.response_observers.borrow_mut().push(ResponseObserver {
                    id: response.id.clone(),
                    transport: ResponseTransport::Stream(tx),
                    after,
                });
            }
            use futures::StreamExt;
            let source = futures::stream::iter(replay).chain(rx);
            let mut output = Response::from_stream(sse::keep_alive(source))?;
            output
                .headers_mut()
                .set("Content-Type", "text/event-stream")?;
            output
                .headers_mut()
                .set("Cache-Control", "no-cache, no-transform")?;
            output.headers_mut().set("x-accel-buffering", "no")?;
            return Ok(output);
        }
        if terminal_status(&response.status) || response.request.background {
            return Response::from_json(&response.value());
        }
        if self.response_observers.borrow().len() >= MAX_RESPONSE_OBSERVERS {
            return util::json_error(429, "observer_limit", "Too many response observers");
        }
        let (tx, output) = crate::observer_transport::channel("responses")?;
        self.response_observers.borrow_mut().push(ResponseObserver {
            id: response.id.clone(),
            transport: ResponseTransport::Json(tx),
            after: None,
        });
        Ok(output)
    }

    pub(super) async fn read_response(&self, id: &str) -> Result<Option<ResponseRun>> {
        if let Some(response) = self
            .data
            .borrow()
            .as_ref()
            .and_then(|d| d.run.as_ref())
            .and_then(|r| r.response.as_ref())
            .filter(|r| r.id == id)
            .cloned()
        {
            return Ok(Some(response));
        }
        read_chunks(&self.state.storage(), &format!("{RESPONSE_PREFIX}{id}"))
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Error::from))
            .transpose()
    }

    async fn write_response(&self, response: &ResponseRun) -> Result<()> {
        write_chunks(
            &self.state.storage(),
            &format!("{RESPONSE_PREFIX}{}", response.id),
            &serde_json::to_vec(response)?,
        )
        .await
    }

    fn response_event(&self, kind: &str, fields: Value) -> Result<()> {
        let mut borrowed = self.data.borrow_mut();
        let Some(response) = borrowed
            .as_mut()
            .and_then(|d| d.run.as_mut())
            .and_then(|r| r.response.as_mut())
        else {
            return Ok(());
        };
        let mut fields = fields;
        if kind.ends_with(".delta")
            && response
                .request
                .stream_options
                .as_ref()
                .and_then(|options| options.get("include_obfuscation"))
                .and_then(Value::as_bool)
                .unwrap_or(true)
        {
            let random = uuid::Uuid::new_v4();
            let padding = random.simple().to_string();
            fields["obfuscation"] = json!(&padding[..16 + usize::from(random.as_bytes()[0] % 17)]);
        }
        let event = response.event(kind, fields)?;
        let id = response.id.clone();
        drop(borrowed);
        self.response_observers.borrow_mut().retain_mut(|observer| {
            if observer.id != id {
                return true;
            }
            if observer.after.is_some_and(|after| {
                event["sequence_number"]
                    .as_u64()
                    .is_some_and(|sequence| sequence <= after)
            }) {
                return true;
            }
            match &mut observer.transport {
                ResponseTransport::Stream(sender) => send_response_event(sender, &event),
                ResponseTransport::Json(_) => true,
            }
        });
        Ok(())
    }

    pub(super) fn reserve_response_events(&self) {
        let mut borrowed = self.data.borrow_mut();
        if let Some(response) = borrowed
            .as_mut()
            .and_then(|d| d.run.as_mut())
            .and_then(|r| r.response.as_mut())
        {
            response.reserved_sequence_end = response
                .next_sequence
                .saturating_add(MAX_RESPONSE_EVENTS as u64 + 16);
        }
    }

    pub(super) fn response_in_progress(&self) -> Result<()> {
        let value = {
            let mut borrowed = self.data.borrow_mut();
            let Some(response) = borrowed
                .as_mut()
                .and_then(|d| d.run.as_mut())
                .and_then(|r| r.response.as_mut())
            else {
                return Ok(());
            };
            response.status = "in_progress".into();
            response.value()
        };
        self.response_event("response.in_progress", json!({"response":value}))
    }

    pub(super) fn response_delta(&self, delta: &str) -> Result<()> {
        let (index, item_id, open) = {
            let borrowed = self.data.borrow();
            let Some(run) = borrowed.as_ref().and_then(|d| d.run.as_ref()) else {
                return Ok(());
            };
            let Some(response) = run.response.as_ref() else {
                return Ok(());
            };
            (
                response.output.len(),
                format!("msg_{}_{}", run.id, run.round),
                response.stream_item.is_some(),
            )
        };
        if !open {
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .stream_item = Some(item_id.clone());
            self.response_event("response.output_item.added", json!({"output_index":index,"item":{"id":item_id,"type":"message","role":"assistant","status":"in_progress","content":[]}}))?;
            self.response_event("response.content_part.added", json!({"item_id":item_id,"output_index":index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}))?;
        }
        self.response_event("response.output_text.delta", json!({"item_id":item_id,"output_index":index,"content_index":0,"delta":delta,"logprobs":[]}))
    }

    pub(super) async fn response_completion(&self, completion: &Completion) -> Result<()> {
        let data = self.snapshot()?;
        let Some(run) = data.run else { return Ok(()) };
        let Some(response) = run.response else {
            return Ok(());
        };
        if let Some(error) = &response.stream_error {
            return Err(Error::RustError(error.clone()));
        }
        let text = completion.message.content.as_str().unwrap_or("");
        let projected_bytes = serde_json::to_vec(&response.output)?
            .len()
            .saturating_add(serde_json::to_vec(&completion.message)?.len());
        if projected_bytes > MAX_RESPONSE_OUTPUT_BYTES {
            return Err(Error::RustError("response_output_limit".into()));
        }
        Self::validate_response_output(completion, &response.request, run.round == 1)?;

        let index = response.output.len();
        let item_id = format!("msg_{}_{}", run.id, run.round.saturating_sub(1));
        let structured = response
            .request
            .text
            .as_ref()
            .and_then(|text| text.get("format"))
            .and_then(|format| format.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "text");
        if (!structured || completion.message.tool_calls.is_empty())
            && (!text.is_empty()
                || completion.message.tool_calls.is_empty()
                || response.stream_item.is_some())
        {
            if response.stream_item.is_none() {
                self.response_delta(text)?;
            }
            let item_id = self
                .data
                .borrow()
                .as_ref()
                .unwrap()
                .run
                .as_ref()
                .unwrap()
                .response
                .as_ref()
                .unwrap()
                .stream_item
                .clone()
                .unwrap_or(item_id);
            let part = json!({"type":"output_text","text":text,"annotations":[],"logprobs":[]});
            let mut item = json!({"id":item_id,"type":"message","role":"assistant","status":"completed","content":[part]});
            self.response_event("response.output_text.done", json!({"item_id":item_id,"output_index":index,"content_index":0,"text":text,"logprobs":[]}))?;
            self.response_event(
                "response.content_part.done",
                json!({"item_id":item_id,"output_index":index,"content_index":0,"part":part}),
            )?;
            if let Some(refusal) = completion
                .message
                .extra
                .get("refusal")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let refusal_part = json!({"type":"refusal","refusal":refusal});
                self.response_event("response.content_part.added",json!({"item_id":item_id,"output_index":index,"content_index":1,"part":{"type":"refusal","refusal":""}}))?;
                self.response_event("response.refusal.delta",json!({"item_id":item_id,"output_index":index,"content_index":1,"delta":refusal}))?;
                self.response_event("response.refusal.done",json!({"item_id":item_id,"output_index":index,"content_index":1,"refusal":refusal}))?;
                self.response_event("response.content_part.done",json!({"item_id":item_id,"output_index":index,"content_index":1,"part":refusal_part}))?;
                item["content"].as_array_mut().unwrap().push(refusal_part);
            }
            self.response_event(
                "response.output_item.done",
                json!({"output_index":index,"item":item}),
            )?;
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .output
                .push(item);
        }
        self.data
            .borrow_mut()
            .as_mut()
            .unwrap()
            .run
            .as_mut()
            .unwrap()
            .response
            .as_mut()
            .unwrap()
            .stream_item = None;
        let mut published_calls = 0usize;
        for call in &completion.message.tool_calls {
            if !response
                .request
                .tools
                .iter()
                .any(|tool| tool.name == call.function.name)
            {
                continue;
            }
            let index = self
                .snapshot()?
                .run
                .as_ref()
                .unwrap()
                .response
                .as_ref()
                .unwrap()
                .output
                .len();
            let id = format!("fc_{}_{}", run.id, index);
            let item = json!({"type":"function_call","id":id,"call_id":call.id,"name":call.function.name,"arguments":call.function.arguments,"status":"completed"});
            let mut added = item.clone();
            added["arguments"] = json!("");
            added["status"] = json!("in_progress");
            self.response_event(
                "response.output_item.added",
                json!({"output_index":index,"item":added}),
            )?;
            self.response_event(
                "response.function_call_arguments.delta",
                json!({"item_id":id,"output_index":index,"delta":call.function.arguments}),
            )?;
            self.response_event("response.function_call_arguments.done", json!({"item_id":id,"output_index":index,"arguments":call.function.arguments,"name":call.function.name}))?;
            self.response_event(
                "response.output_item.done",
                json!({"output_index":index,"item":item}),
            )?;
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .output
                .push(item);
            published_calls += 1;
            if published_calls.is_multiple_of(2) {
                // A ready provider stream or a parallel handoff can otherwise
                // fill the observer queue before even a fast JS reader is polled.
                worker::Delay::from(std::time::Duration::ZERO).await;
            }
        }
        if completion.finish_reason == "content_filter" {
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .incomplete_reason = Some("content_filter".into());
        }
        if completion.finish_reason == "length" {
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .incomplete_reason = Some("max_output_tokens".into());
        }
        let output_bytes = {
            let borrowed = self.data.borrow();
            serde_json::to_vec(
                &borrowed
                    .as_ref()
                    .unwrap()
                    .run
                    .as_ref()
                    .unwrap()
                    .response
                    .as_ref()
                    .unwrap()
                    .output,
            )?
            .len()
        };
        if output_bytes > MAX_RESPONSE_OUTPUT_BYTES {
            return Err(Error::RustError("response_output_limit".into()));
        }
        Ok(())
    }

    fn validate_response_output(
        completion: &Completion,
        request: &ResponseRequest,
        first_round: bool,
    ) -> Result<()> {
        let calls = &completion.message.tool_calls;
        let refusal = completion
            .message
            .extra
            .get("refusal")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty());
        if request
            .tool_choice
            .as_ref()
            .is_some_and(|choice| choice == "none")
            && !calls.is_empty()
        {
            return Err(Error::RustError(
                "provider_tool_choice_policy_violation".into(),
            ));
        }
        if first_round
            && !refusal
            && !matches!(
                completion.finish_reason.as_str(),
                "length" | "content_filter"
            )
        {
            if let Some(choice) = request.tool_choice.as_ref() {
                let violation = if choice == "required" {
                    calls.is_empty()
                } else if choice["type"] == "function" {
                    calls.is_empty()
                        || calls.iter().any(|call| {
                            choice["name"].as_str() != Some(call.function.name.as_str())
                        })
                } else {
                    false
                };
                if violation {
                    return Err(Error::RustError(
                        "provider_tool_choice_policy_violation".into(),
                    ));
                }
            }
        }
        if request.parallel_tool_calls == Some(false) && completion.message.tool_calls.len() > 1 {
            return Err(Error::RustError(
                "provider_parallel_tool_policy_violation".into(),
            ));
        }
        for call in &completion.message.tool_calls {
            let Some(tool) = request
                .tools
                .iter()
                .find(|tool| tool.name == call.function.name)
            else {
                continue;
            };
            let arguments: Value = serde_json::from_str(&call.function.arguments)
                .map_err(|_| Error::RustError("invalid_function_arguments".into()))?;
            if tool.strict == Some(true) {
                crate::responses_schema::validate_instance(&tool.parameters, &arguments)
                    .map_err(|_| Error::RustError("invalid_function_arguments".into()))?;
            }
        }
        if !completion.message.tool_calls.is_empty()
            || completion.finish_reason == "length"
            || completion.finish_reason == "content_filter"
            || completion.message.extra.get("refusal").is_some()
        {
            return Ok(());
        }
        if let Some(format) = request.text.as_ref().and_then(|text| text.get("format")) {
            match format["type"].as_str() {
                Some("json_object" | "json_schema") => {
                    let value: Value =
                        serde_json::from_str(completion.message.content.as_str().unwrap_or(""))
                            .map_err(|_| Error::RustError("invalid_structured_output".into()))?;
                    if format["type"] == "json_object" && !value.is_object() {
                        return Err(Error::RustError("invalid_structured_output".into()));
                    }
                    if format["type"] == "json_schema" {
                        crate::responses_schema::validate_instance(&format["schema"], &value)
                            .map_err(|_| Error::RustError("invalid_structured_output".into()))?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn fit_response_prompt(&self, output_budget: u32) -> Result<()> {
        let mut borrowed = self.data.borrow_mut();
        let data = borrowed.as_mut().unwrap();
        let Some(response) = data.run.as_ref().and_then(|run| run.response.as_ref()) else {
            return Ok(());
        };
        let auto = response.request.truncation.as_deref() == Some("auto");
        let limit = u64::from(
            data.record
                .pinned
                .model
                .context_window
                .saturating_sub(output_budget),
        );
        loop {
            let new_bytes = budget::working_tokens(
                &data.messages[data.measured_messages.min(data.messages.len())..],
            )?;
            let estimate = if data.input_tokens == 0 {
                budget::working_tokens(&data.messages)?
            } else {
                data.input_tokens.saturating_add(new_bytes)
            };
            if estimate <= limit {
                return Ok(());
            }
            if !auto {
                return Err(Error::RustError("context_length_exceeded".into()));
            }
            // Never split a tool-call/result group or drop the pinned prefix/latest input.
            let Some(next_user) = data
                .messages
                .iter()
                .enumerate()
                .skip(2)
                .find(|(_, message)| message.role == "user")
                .map(|(n, _)| n)
            else {
                return Err(Error::RustError("context_length_exceeded".into()));
            };
            data.messages.drain(1..next_user);
            data.input_tokens = 0;
            data.measured_messages = 0;
        }
    }

    pub(super) fn local_tool_pending(&self, name: &str) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|d| d.run.as_ref())
            .and_then(|r| r.response.as_ref())
            .is_some_and(|r| r.request.tools.iter().any(|t| t.name == name))
    }

    pub(super) fn response_terminal_value(&self, run: &RunState) -> Option<Value> {
        let mut response = run.response.clone()?;
        response.usage = run.total.clone();
        response.status = if response.cancel_requested {
            "cancelled"
        } else if response.incomplete_reason.is_some() {
            "incomplete"
        } else {
            "completed"
        }
        .into();
        Some(response.value())
    }

    pub(super) async fn commit_response(
        &self,
        run: &RunState,
        result: &CompletionResult,
    ) -> Result<Option<ResponseRun>> {
        let Some(mut response) = run.response.clone() else {
            return Ok(None);
        };
        response.usage = run.total.clone();
        response.status = if response.cancel_requested {
            "cancelled"
        } else if result.is_err() {
            "failed"
        } else if response.incomplete_reason.is_some() {
            "incomplete"
        } else {
            "completed"
        }
        .into();
        response.error = result.as_ref().err().cloned();
        response.completed_at = run.ended_at_ms.map(|n| n / 1000);
        if response.status == "completed" {
            response.awaiting_calls =
                outstanding_local_calls(&self.snapshot()?.messages, &response.request);
        }
        let value = response.value();
        if response.status == "cancelled" {
            response.event("error",json!({"code":"response_cancelled","message":"Response cancelled; retrieve its final state using the same response ID","param":null}))?;
        } else {
            let kind = format!("response.{}", response.status);
            response.event(&kind, json!({"response":value}))?;
        }
        self.write_response(&response).await?;
        Ok(Some(response))
    }

    pub(super) fn publish_response(&self, response: ResponseRun) {
        let last = response.events.last().unwrap();
        let value = response.value();
        let observers = self.response_observers.take();
        let mut remaining = vec![];
        for observer in observers {
            if observer.id != response.id {
                remaining.push(observer);
                continue;
            }
            if observer.after.is_some_and(|after| {
                last["sequence_number"]
                    .as_u64()
                    .is_some_and(|sequence| sequence <= after)
            }) {
                continue;
            }
            match observer.transport {
                ResponseTransport::Stream(mut sender) => {
                    send_response_event(&mut sender, last);
                }
                ResponseTransport::Json(sender) => {
                    crate::observer_transport::publish(sender, 200, value.clone());
                }
            }
        }
        *self.response_observers.borrow_mut() = remaining;
    }
}

fn send_response_event(sender: &mut mpsc::Sender<Result<Vec<u8>>>, event: &Value) -> bool {
    sender.try_send(Ok(wire::event_frame(event))).is_ok()
}

fn terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "incomplete" | "cancelled")
}

fn outstanding_local_calls(
    messages: &[Message],
    request: &ResponseRequest,
) -> Vec<provider::ToolCall> {
    let Some(index) = messages.iter().rposition(|m| m.role == "assistant") else {
        return vec![];
    };
    messages[index]
        .tool_calls
        .iter()
        .filter(|call| {
            request
                .tools
                .iter()
                .any(|tool| tool.name == call.function.name)
                && !messages[index + 1..]
                    .iter()
                    .any(|m| m.tool_call_id.as_deref() == Some(&call.id))
        })
        .cloned()
        .collect()
}

fn project_response_input(
    messages: &mut Vec<Message>,
    pending: &mut Vec<Message>,
    client_results: bool,
) {
    let additions = std::mem::take(pending);
    if !client_results {
        messages.extend(additions);
        return;
    }
    // Client results close the previous assistant call group before queued user
    // steering. Source-journal ordering remains the actual acceptance ordering.
    let (results, input): (Vec<_>, Vec<_>) = additions
        .into_iter()
        .partition(|message| message.role == "tool");
    messages.extend(results);
    messages.extend(input);
}

fn validate_tool_outputs(
    expected: &[provider::ToolCall],
    provided: &[wire::ToolOutput],
) -> std::result::Result<(), &'static str> {
    let mut counts = BTreeMap::new();
    for output in provided {
        *counts.entry(output.call_id.as_str()).or_insert(0usize) += 1;
    }
    if counts.values().any(|count| *count != 1) {
        return Err("duplicate_tool_output");
    }
    if expected.len() != provided.len()
        || expected
            .iter()
            .any(|call| !counts.contains_key(call.id.as_str()))
    {
        return Err("invalid_tool_outputs");
    }
    Ok(())
}

fn input_items(response: &ResponseRun, url: &worker::Url) -> Result<Response> {
    let values = url.query_pairs().collect::<BTreeMap<_, _>>();
    let limit = match values.get("limit").map(|s| s.parse::<usize>()).transpose() {
        Ok(value) => value.unwrap_or(20),
        Err(_) => return util::json_error(400, "invalid_limit", "Invalid page limit"),
    };
    if !(1..=100).contains(&limit) {
        return util::json_error(400, "invalid_limit", "Limit must be between 1 and 100");
    }
    let desc = match values.get("order").map(|s| s.as_ref()).unwrap_or("desc") {
        "desc" => true,
        "asc" => false,
        _ => return util::json_error(400, "invalid_order", "Order must be asc or desc"),
    };
    let mut items = response.input_items.clone();
    if desc {
        items.reverse();
    }
    if let Some(after) = values.get("after") {
        let Some(index) = items
            .iter()
            .position(|item| item["id"].as_str() == Some(after.as_ref()))
        else {
            return util::json_error(
                400,
                "invalid_cursor",
                "Input item cursor does not belong to this response",
            );
        };
        items.drain(..=index);
    }
    if let Some(before) = values.get("before") {
        let Some(index) = items
            .iter()
            .position(|item| item["id"].as_str() == Some(before.as_ref()))
        else {
            return util::json_error(
                400,
                "invalid_cursor",
                "Input item cursor does not belong to this response",
            );
        };
        items.truncate(index);
    }
    let has_more = items.len() > limit;
    items.truncate(limit);
    Response::from_json(
        &json!({"object":"list","first_id":items.first().and_then(|i|i.get("id")),"last_id":items.last().and_then(|i|i.get("id")),"has_more":has_more,"data":items}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response_preflight::validate_imported_history;

    fn response() -> ResponseRun {
        ResponseRun {
            id: "resp_test".into(),
            request: serde_json::from_value(json!({"model":"agent_test","input":"hello"})).unwrap(),
            created_at: 10,
            input_items: vec![],
            output: vec![],
            events: vec![],
            status: "queued".into(),
            error: None,
            incomplete_reason: None,
            cancel_requested: false,
            awaiting_calls: vec![],
            usage: Usage::default(),
            next_sequence: 0,
            reserved_sequence_end: MAX_RESPONSE_EVENTS as u64 + 16,
            event_bytes: 0,
            completed_at: None,
            stream_error: None,
            stream_item: None,
            deleted: false,
            attachments: vec![],
            attachments_ready: 0,
        }
    }

    fn call(id: &str) -> provider::ToolCall {
        provider::ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: provider::FunctionCall {
                name: "local".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[test]
    fn imported_history_requires_ordered_complete_groups_but_not_global_call_ids() {
        let mut assistant = Message::text("assistant", "");
        assistant.tool_calls.push(call("reused"));
        let output = Message::tool_result("reused", "result");
        assert!(
            validate_imported_history(&[
                assistant.clone(),
                output.clone(),
                assistant.clone(),
                output.clone()
            ])
            .is_ok()
        );
        assert!(validate_imported_history(&[output.clone(), assistant.clone()]).is_err());
        assert!(
            validate_imported_history(&[
                assistant.clone(),
                Message::text("user", "next"),
                output.clone()
            ])
            .is_err()
        );
        assert!(validate_imported_history(&[assistant.clone()]).is_err());
        assert!(validate_imported_history(&[assistant, output.clone(), output]).is_err());
    }

    #[test]
    fn disclosed_sequences_are_not_reused_after_an_uncertain_provider_reset() {
        let mut persisted = response();
        persisted.event("response.created", json!({})).unwrap();
        persisted.reserved_sequence_end = persisted.next_sequence + 100;
        let mut live = persisted.clone();
        let delta = live
            .event("response.output_text.delta", json!({"delta":"partial"}))
            .unwrap();
        let mut restored: ResponseRun =
            serde_json::from_slice(&serde_json::to_vec(&persisted).unwrap()).unwrap();
        restored.recover_sequence();
        let failed = restored.event("response.failed", json!({})).unwrap();
        assert!(
            failed["sequence_number"].as_u64().unwrap()
                > delta["sequence_number"].as_u64().unwrap()
        );
    }

    #[test]
    fn repeated_resets_require_fresh_durable_ranges_before_any_new_event() {
        let mut first = response();
        first.event("response.created", json!({})).unwrap();
        let mut restored = first.clone();
        restored.recover_sequence();
        let durable = restored.clone();
        let disclosed = restored.event("response.in_progress", json!({})).unwrap();
        let mut restored_again = durable;
        restored_again.recover_sequence();
        let after = restored_again.event("response.failed", json!({})).unwrap();
        assert!(
            after["sequence_number"].as_u64().unwrap()
                > disclosed["sequence_number"].as_u64().unwrap()
        );
        restored_again.next_sequence = restored_again.reserved_sequence_end;
        assert!(
            restored_again
                .event(
                    "response.output_text.delta",
                    json!({"delta":"must not escape"})
                )
                .is_err()
        );
        restored_again.recover_sequence();
        assert!(restored_again.event("response.failed", json!({})).is_ok());
    }

    #[test]
    fn replay_window_eviction_cannot_strand_the_terminal_event() {
        let mut state = response();
        let text = "x".repeat(256 * 1024);
        for _ in 0..80 {
            state
                .event("response.output_text.delta", json!({"delta":text}))
                .unwrap();
        }
        state
            .event(
                "response.completed",
                json!({"response":{"status":"completed"}}),
            )
            .unwrap();
        assert!(
            state.events.first().unwrap()["sequence_number"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(state.events.last().unwrap()["type"], "response.completed");
        assert!(state.event_bytes <= MAX_RESPONSE_EVENT_BYTES);
        for pair in state.events.windows(2) {
            assert!(
                pair[0]["sequence_number"].as_u64().unwrap()
                    < pair[1]["sequence_number"].as_u64().unwrap()
            );
        }
    }

    #[test]
    fn immutable_public_failure_uses_standard_code_and_safe_platform_extension() {
        let mut state = response();
        state.status = "failed".into();
        state.completed_at = Some(20);
        state.error = Some("provider_outcome_unknown".into());
        let original = state.value();
        let restored: ResponseRun =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(restored.value(), original);
        assert_eq!(original["error"]["code"], "server_error");
        assert_eq!(original["agents_error_code"], "provider_outcome_unknown");
        assert_eq!(original["completed_at"], 20);
    }

    #[test]
    fn strict_arguments_and_structured_outputs_are_validated_before_handoff() {
        let mut request = response().request;
        request.tools = serde_json::from_value(json!([{"type":"function","name":"local","strict":true,"parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}])).unwrap();
        let mut completion = Completion {
            id: "p".into(),
            model: "m".into(),
            message: Message::text("assistant", ""),
            finish_reason: "tool_calls".into(),
            usage: Usage::default(),
        };
        completion.message.tool_calls.push(call("call_1"));
        assert!(SessionInner::validate_response_output(&completion, &request, true).is_err());
        completion.message.tool_calls[0].function.arguments = r#"{"path":"file.txt"}"#.into();
        assert!(SessionInner::validate_response_output(&completion, &request, true).is_ok());
        completion.message.tool_calls.clear();
        completion.finish_reason = "stop".into();
        request.text = Some(json!({"format":{"type":"json_object"}}));
        completion.message.content = json!("[]");
        assert!(SessionInner::validate_response_output(&completion, &request, true).is_err());
        completion.message.content = json!("{\"ok\":true}");
        assert!(SessionInner::validate_response_output(&completion, &request, true).is_ok());
        completion.message.content = json!("");
        completion
            .message
            .extra
            .insert("refusal".into(), json!("Cannot comply"));
        assert!(SessionInner::validate_response_output(&completion, &request, true).is_ok());
    }

    #[test]
    fn returned_tool_calls_cannot_override_the_callers_selection_policy() {
        let mut request = response().request;
        let mut completion = Completion {
            id: "p".into(),
            model: "m".into(),
            message: Message::text("assistant", "done"),
            finish_reason: "stop".into(),
            usage: Usage::default(),
        };
        request.tool_choice = Some(json!("required"));
        assert!(SessionInner::validate_response_output(&completion, &request, true).is_err());
        assert!(SessionInner::validate_response_output(&completion, &request, false).is_ok());
        completion.message.tool_calls.push(call("c"));
        request.tool_choice = Some(json!("none"));
        for first in [true, false] {
            assert!(SessionInner::validate_response_output(&completion, &request, first).is_err());
        }
        request.tool_choice = Some(json!({"type":"function","name":"other"}));
        assert!(SessionInner::validate_response_output(&completion, &request, true).is_err());
        assert!(SessionInner::validate_response_output(&completion, &request, false).is_ok());
        request.tool_choice = Some(json!({"type":"function","name":"local"}));
        assert!(SessionInner::validate_response_output(&completion, &request, true).is_ok());
        completion.message.tool_calls.clear();
        request.tool_choice = Some(json!("required"));
        completion
            .message
            .extra
            .insert("refusal".into(), json!("Cannot comply"));
        assert!(SessionInner::validate_response_output(&completion, &request, true).is_ok());
    }

    #[test]
    fn client_outputs_are_projected_before_unresolved_tool_detection_and_queued_steering() {
        let mut assistant = Message::text("assistant", "");
        assistant.tool_calls.push(call("local_call"));
        let mut messages = vec![Message::text("system", "fixed"), assistant];
        let mut pending = vec![
            Message::text("user", "late steering"),
            Message::tool_result("local_call", "actual local result"),
            Message::text("user", "new request"),
        ];
        project_response_input(&mut messages, &mut pending, true);
        assert!(pending.is_empty());
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("local_call"));
        assert_eq!(messages[3].content, "late steering");
        assert_eq!(messages[4].content, "new request");
        assert!(unresolved_tool(&messages).is_none());
    }

    #[test]
    fn fresh_manual_history_is_not_reordered_as_client_continuation() {
        let mut assistant = Message::text("assistant", "");
        assistant.tool_calls.push(call("manual"));
        let mut messages = vec![Message::text("system", "fixed")];
        let mut pending = vec![
            assistant,
            Message::tool_result("manual", "history"),
            Message::text("user", "continue"),
        ];
        project_response_input(&mut messages, &mut pending, false);
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].role, "tool");
        assert!(unresolved_tool(&messages).is_none());
    }

    #[test]
    fn function_results_require_exact_matching_set() {
        let expected = vec![provider::ToolCall {
            id: "a".into(),
            kind: "function".into(),
            function: provider::FunctionCall {
                name: "local".into(),
                arguments: "{}".into(),
            },
        }];
        let output = |id: &str| wire::ToolOutput { call_id: id.into() };
        assert!(validate_tool_outputs(&expected, &[output("a")]).is_ok());
        assert!(validate_tool_outputs(&expected, &[]).is_err());
        assert!(validate_tool_outputs(&expected, &[output("b")]).is_err());
        assert!(validate_tool_outputs(&expected, &[output("a"), output("a")]).is_err());
        assert!(validate_tool_outputs(&[], &[output("a")]).is_err());
    }
}
