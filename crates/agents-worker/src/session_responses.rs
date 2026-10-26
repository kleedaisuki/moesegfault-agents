use super::*;
use crate::response_image_projection::{ResponseFrame, ResponseImages};
use crate::response_limits::{CanonicalOutput, OutputChange, OutputPlan};
use crate::responses_wire::{self as wire, ResponseRequest};
use std::collections::BTreeMap;

#[path = "response_obfuscation.rs"]
mod obfuscation;
use obfuscation::{Obfuscation, Seed};

const RESPONSE_PREFIX: &str = "response:";
const MAX_RESPONSE_EVENTS: usize = 65_536;
const MAX_RESPONSE_EVENT_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_RESPONSE_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESPONSE_OBSERVERS: usize = 8;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ResponseRun {
    #[serde(default)]
    pub(super) conversation: Option<super::conversations::ConversationBinding>,
    pub(super) id: String,
    pub(super) request: ResponseRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) resolved_prompt: Option<crate::prompt_templates::ResolvedPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) effective_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) compaction_source_end: Option<usize>,
    pub(super) created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) obfuscation_seed: Option<Seed>,
    pub(super) input_items: Vec<Value>,
    pub(super) output: CanonicalOutput,
    #[serde(default, skip_serializing_if = "ResponseImages::is_empty")]
    pub(super) images: ResponseImages,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) image_lifecycle: Option<super::image_lifecycle::ImageLifecycle>,
    pub(super) events: Vec<Value>,
    pub(super) status: String,
    pub(super) error: Option<String>,
    pub(super) incomplete_reason: Option<String>,
    pub(super) cancel_requested: bool,
    pub(super) awaiting_calls: Vec<provider::ToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) general_pending_calls: Vec<super::native_output::GeneralPendingCall>,
    #[serde(default)]
    pub(super) native_stream: super::native_output::NativeStream,
    #[serde(default)]
    pub(super) bound_tools: crate::hosted_bindings::BoundTools,
    #[serde(default)]
    pub(super) effective_service_tier: Option<String>,
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
    #[serde(default)]
    pub(super) branch_point: Option<super::branch::BranchPoint>,
    #[serde(default)]
    pub(super) execution: crate::response_execution::ExecutionContext,
}

impl ResponseRun {
    pub(super) fn plan_output(&mut self) -> Result<OutputPlan> {
        self.output
            .plan(&self.id)
            .map_err(|code| Error::RustError(code.into()))
    }

    pub(super) fn prepare_output(
        &mut self,
        index: Option<usize>,
        item: Value,
    ) -> Result<OutputChange> {
        self.output
            .prepare(&self.id, index, item)
            .map_err(|code| Error::RustError(code.into()))
    }

    pub(super) fn prepare_output_batch(&mut self, items: Vec<Value>) -> Result<Vec<OutputChange>> {
        let mut plan = self.plan_output()?;
        for item in items {
            plan.push(item)
                .map_err(|code| Error::RustError(code.into()))?;
        }
        Ok(plan.finish())
    }

    pub(super) fn apply_output(&mut self, change: OutputChange) -> Result<()> {
        self.output
            .apply(change)
            .map_err(|code| Error::RustError(code.into()))
    }

    pub(super) fn recover_sequence(&mut self) {
        self.next_sequence = self.next_sequence.max(self.reserved_sequence_end);
        self.reserved_sequence_end = self
            .next_sequence
            .saturating_add(MAX_RESPONSE_EVENTS as u64 + 16);
    }

    pub(super) fn value(&self) -> Value {
        self.value_with_include(&self.request.include)
    }

    fn value_with_include(&self, include: &[String]) -> Value {
        let mut value = wire::response_value_with_include(
            &self.request,
            &self.id,
            self.created_at,
            &self.status,
            &self.output,
            Some(&self.usage),
            include,
        );
        value["completed_at"] = json!(self.completed_at);
        if let Some(tier) = &self.effective_service_tier {
            value["service_tier"] = json!(tier);
        } else if let Some(fields) = value.as_object_mut() {
            fields.remove("service_tier");
        }
        if let (Some(prompt), Some(raw)) = (&self.resolved_prompt, &self.request.prompt) {
            value["prompt"] = prompt.public_reference(raw);
        }
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

    pub(super) fn event(&mut self, kind: &str, mut fields: Value) -> Result<Value> {
        if self.next_sequence >= self.reserved_sequence_end {
            self.stream_error = Some("response_event_limit".into());
            return Err(Error::RustError("response_event_limit".into()));
        }
        fields["type"] = json!(kind);
        fields["sequence_number"] = json!(self.next_sequence);
        if let Some(value) = fields.get_mut("response") {
            value["output"] = json!(self.output);
        }
        if let Some(object) = fields.as_object_mut() {
            object.remove("obfuscation");
        }
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
    Stream(mpsc::Sender<Result<ResponseFrame>>),
    Json(crate::observer_transport::Sender),
}

pub(super) struct ResponseObserver {
    id: String,
    transport: ResponseTransport,
    after: Option<u64>,
    follow_steering: bool,
    projection: wire::ResponseProjection,
    obfuscation: Obfuscation,
    original_image_observer: bool,
}

#[derive(Deserialize)]
pub(super) struct CreateResponse {
    pub(super) response_id: String,
    pub(super) request: ResponseRequest,
    #[serde(default)]
    pub(super) execution: crate::response_execution::ExecutionContext,
}

impl SessionInner {
    pub(super) async fn workspace_action(&self, mut req: Request) -> Result<Response> {
        let _resource = self.code_operation.lock().await;
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
        if req.method() == Method::Post && parts.get(2) == Some(&"steer") {
            return self.response_steer(id, req).await;
        }
        let Some(mut response) = self.read_response(id).await? else {
            return util::json_error(404, "not_found", "Response not found");
        };
        if response.deleted || (!response.request.store && terminal_status(&response.status)) {
            return util::json_error(404, "not_found", "Response not retained");
        }
        match (req.method(), parts.get(2).copied()) {
            (Method::Get, action @ (None | Some("events"))) => {
                let options = match wire::RetrieveOptions::parse(&req.url()?, action.is_some()) {
                    Ok(options) => options,
                    Err(error) => return util::json_error(400, "invalid_request", error),
                };
                if action.is_none() && !options.stream {
                    return Response::from_json(
                        &self
                            .public_response_value(&response, &options.projection.include)
                            .await?,
                    );
                }
                self.observe_response(
                    &response,
                    true,
                    options.after,
                    false,
                    Some(options.projection),
                    false,
                )
                .await
            }
            (Method::Get, Some("input_items")) => input_items(&response, &req.url()?),
            (Method::Post, Some("cancel")) => {
                let cancellation = self.persistence.lock().await;
                let Some(fresh) = self.read_response(id).await? else {
                    return util::json_error(404, "not_found", "Response not found");
                };
                response = fresh;
                if response.deleted
                    || (!response.request.store && terminal_status(&response.status))
                {
                    return util::json_error(404, "not_found", "Response not retained");
                }
                if !response.request.background {
                    return util::json_error(
                        400,
                        "response_not_background",
                        "Only background responses can be cancelled",
                    );
                }
                if !terminal_status(&response.status) {
                    let accepted = {
                        let mut borrowed = self.data.borrow_mut();
                        borrowed
                            .as_mut()
                            .and_then(|data| data.run.as_mut())
                            .filter(|run| run.id == *id)
                            .map(super::conversations::request_response_cancel)
                    };
                    if accepted == Some(false) {
                        return util::json_error(
                            409,
                            "response_finalizing",
                            "Response has already decided its terminal outcome",
                        );
                    }
                    response.cancel_requested |= accepted == Some(true);
                    let mut committed = self.snapshot()?;
                    save_state(&self.state.storage(), &mut committed).await?;
                    drop(cancellation);
                    self.persist().await?;
                    let record = self.snapshot()?.record;
                    let still_current = self
                        .data
                        .borrow()
                        .as_ref()
                        .and_then(|data| data.run.as_ref())
                        .is_some_and(|run| run.id == *id);
                    if still_current && self.cancel_current_hosted_code(id, &record).await.is_err()
                    {
                        util::emit(
                            "tool.cancel_pending",
                            json!({"session_id":record.id,"response_id":id,"tool_kind":"code_interpreter"}),
                        );
                    }
                    self.arm_alarm().await?;
                } else {
                    drop(cancellation);
                }
                Response::from_json(
                    &self
                        .public_response_value(&response, &response.request.include)
                        .await?,
                )
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
                    if data.response_head.as_deref() == Some(*id) && data.has_pending_client_tools()
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
                let deletion = self.persistence.lock().await;
                self.queue_response_archive_release(id).await?;
                response.deleted = true;
                response.request.input = Value::Null;
                response.request.instructions = None;
                response.request.prompt = None;
                response.resolved_prompt = None;
                response.request.tools.clear();
                response.request.metadata.clear();
                response.request.text = None;
                response.request.reasoning = None;
                response.request.stream_options = None;
                response.attachments.clear();
                response.input_items.clear();
                response.output.clear();
                response.events.clear();
                response.obfuscation_seed = None;
                self.write_response(&response).await?;
                self.state
                    .storage()
                    .delete(&format!("response-obfuscation:{id}"))
                    .await?;
                drop(deletion);
                self.reconcile_response_archives().await?;
                self.arm_alarm().await?;
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
        self.observe_response(&existing, request.stream, None, false, None, false)
            .await
    }

    pub(super) async fn create_response(
        &self,
        mut create: CreateResponse,
        traceparent: Option<String>,
    ) -> Result<Response> {
        let request = create.request;
        if let Err(error) = request.validate() {
            return util::json_error(400, "invalid_request_error", &error.to_string());
        }
        if let Some(existing) = self.read_response(&create.response_id).await? {
            return self.existing_response(existing, &request).await;
        }
        self.prepare_conversation_execution(&request, &create.response_id, &mut create.execution)
            .await?;
        if self.branch_dispatched(&create.response_id).await? {
            return util::json_error(
                409,
                "response_branch_required",
                "This response has an independent child session",
            );
        }
        let owner = self.data.borrow().as_ref().unwrap().record.owner_id.clone();
        let inherited_prompt = match (
            &create.execution.steering_parent,
            &create.execution.inherited_prompt,
        ) {
            (Some(parent), Some(prompt))
                if self.validate_steering_parent(
                    parent,
                    &create.response_id,
                    &create.execution,
                )? =>
            {
                Some(prompt.for_successor())
            }
            (_, Some(_)) => {
                return util::json_error(
                    400,
                    "invalid_steering_parent",
                    "Prompt inheritance requires an accepted steering queue",
                );
            }
            _ => None,
        };
        let resolved_prompt = if inherited_prompt.is_some() {
            inherited_prompt
        } else {
            match crate::prompt_templates::resolve(&self.env, &owner, &request) {
                Ok(prompt) => prompt,
                Err(error) => {
                    return util::json_error(
                        error.status,
                        error.code,
                        "Prompt could not be resolved",
                    );
                }
            }
        };
        let mut effective_request = match &resolved_prompt {
            Some(prompt) => match prompt.apply(&request) {
                Ok(effective) => effective,
                Err(error) => {
                    return util::json_error(
                        error.status,
                        error.code,
                        "Prompt variables are not valid",
                    );
                }
            },
            None => request.clone(),
        };
        effective_request.input =
            self.response_steering_input(&effective_request, &create.execution)?;
        let mut input = match crate::response_preflight::validate(&effective_request) {
            Ok(input) => input,
            Err(error) => {
                return util::json_error(400, error.code, &error.message);
            }
        };
        let mut data = self.snapshot()?;
        let conversation_seed = match self
            .prepare_conversation_seed(
                &request,
                create.execution.conversation.as_ref(),
                &create.response_id,
            )
            .await
        {
            Ok(seed) => seed,
            Err(Error::RustError(code)) if code == "conversation_context_changed" => {
                return util::json_error(
                    409,
                    "conversation_context_changed",
                    "Conversation execution context changed; use an independent execution lane",
                );
            }
            Err(error) => return Err(error),
        };
        if let Some(seed) = &conversation_seed {
            seed.apply(&mut data);
        }
        let previous_response_id =
            super::conversations::previous(&request, create.execution.conversation.as_ref());
        let requested_profile = match crate::runtime_profiles::requested_tools(&request.tools) {
            Ok(profile) => profile,
            Err(_) => {
                return util::json_error(
                    400,
                    "unsupported_runtime_profile",
                    "Requested Container memory is not supported",
                );
            }
        };
        if previous_response_id.is_some()
            && (requested_profile.is_some_and(|profile| profile != data.record.runtime_profile)
                || previous_response_id != data.response_head
                || request.model != data.record.agent_id
                || data.run.is_some()
                || self.closing.get()
                || data.archive_preparing
                || data.archive_pending)
        {
            if create.execution.conversation.is_some() {
                return util::json_error(
                    409,
                    "conversation_context_changed",
                    "Conversation execution lane is not available",
                );
            }
            return util::json_error(
                409,
                "response_branch_required",
                "Continue from an independent child session",
            );
        }
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
        if previous_response_id != data.response_head {
            if create.execution.conversation.is_some() {
                return util::json_error(
                    409,
                    "conversation_context_changed",
                    "Conversation execution context changed",
                );
            }
            return util::json_error(
                409,
                "response_branch_required",
                "Continue from an independent child session",
            );
        }
        if let Some(parent) = previous_response_id
            .as_deref()
            .filter(|_| create.execution.conversation.is_none())
        {
            let imported = data
                .imported_parent
                .as_ref()
                .is_some_and(|seed| seed.response_id == parent);
            if !imported {
                let Some(parent) = self.read_response(parent).await? else {
                    return util::json_error(404, "not_found", "Previous response not retained");
                };
                let queued = self.validate_steering_parent(
                    &parent.id,
                    &create.response_id,
                    &create.execution,
                )?;
                let leased = match &create.execution.parent_lease {
                    Some(lease) => self.validate_parent_grant(&parent.id, lease).await?,
                    None => false,
                };
                if parent.deleted || (!parent.request.store && !queued && !leased) {
                    return util::json_error(404, "not_found", "Previous response not retained");
                }
            }
        }
        if previous_response_id.is_some() || create.execution.conversation.is_some() {
            let result = if data.pending_general_tools.is_empty() {
                validate_tool_outputs(&data.pending_client_tools, &input.tool_outputs)
            } else {
                super::native_output::validate_returns(
                    &data.pending_general_tools,
                    &input.native_items,
                )
            };
            if let Err(code) = result {
                return util::json_error(
                    400,
                    code,
                    "Provide each outstanding function result exactly once before continuing",
                );
            }
        }
        let mut definitions = tool_definitions(&data.record);
        if request.tools.iter().any(|tool| {
            matches!(tool.kind.as_str(), "function" | "custom")
                && definitions
                    .iter()
                    .any(|definition| definition.function.name == tool.name)
        }) {
            return util::json_error(
                400,
                "tool_name_reserved",
                "Caller tool names must not shadow pinned harness tools",
            );
        }
        let bound_tools = match self.resolve_hosted_bindings(&request, &data.record).await {
            Ok(bindings) => bindings,
            Err(error) if matches!(&error,Error::RustError(code) if code=="code_container_not_found") =>
            {
                return crate::routes::error_response(&error);
            }
            Err(error) => return util::json_error(400, "invalid_hosted_tool", &error.to_string()),
        };
        let local = request.function_definitions().map_err(Error::RustError)?;
        definitions.extend(local);
        definitions.extend(bound_tools.definitions()?);
        if let Err(error) = request.validate_resolved_tool_choice(&definitions) {
            return util::json_error(400, "invalid_tool_choice", &error);
        }
        let mut config = self.provider_config(&data.record).await?;
        if config.protocol != agents_core::ProviderProtocol::Responses
            && request.tools.iter().any(|tool| {
                matches!(tool.kind.as_str(), "custom" | "namespace")
                    || crate::client_tool_items::declaration(&tool.to_wire_value())
            })
        {
            return util::json_error(
                400,
                "unsupported_native_request",
                "These caller-executed tools require a native Responses provider",
            );
        }
        let mut options = request.generation_options();
        if let Some(choice) = options.tool_choice.take() {
            options.tool_choice = Some(bound_tools.rewrite_choice(choice)?);
        }
        if let Err(error) = Self::validate_native_generation(
            &config,
            &request,
            &bound_tools,
            &definitions,
            &mut options,
        ) {
            return util::json_error(400, "unsupported_native_request", &error.to_string());
        }
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
        let owned_native = match self
            .resolve_owned_native_input(
                &config,
                &effective_request
                    .native_input_items()
                    .map_err(Error::RustError)?,
            )
            .await
        {
            Ok(items) => items,
            Err(error) if super::native_items::invalid_lookup(&error) => {
                return util::json_error(
                    400,
                    "unresolved_native_item",
                    "Referenced or opaque input must belong to this owner and pinned provider context",
                );
            }
            Err(error) => return Err(error),
        };
        let mut resolved = crate::responses_attachments::resolve(
            &self.env,
            &data.record.owner_id,
            &create.response_id,
            &Value::Array(owned_native),
            data.record.pinned.workspace,
        )
        .await?;
        if let Some(seed) = &conversation_seed {
            let mut seen = resolved
                .attachments
                .iter()
                .map(|file| file.id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            resolved.attachments.extend(
                seed.attachments
                    .iter()
                    .filter(|file| seen.insert(file.id.clone()))
                    .cloned(),
            );
        }
        self.retain_declared_tool_files(&bound_tools, &mut resolved.attachments)
            .await?;
        let mut execution_request = effective_request.clone();
        execution_request.input = resolved.input;
        let incoming = execution_request
            .native_input_items()
            .map_err(Error::RustError)?;
        let expanded = match crate::responses_auxiliary::expand_for_prefix(
            &self.env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
            &data.record.owner_id,
            &data.record.agent_id,
            &data.record.pinned.prefix_hash(),
            &incoming,
            &system_prefix(&data.record),
        ) {
            Ok(expanded) => expanded,
            Err(_) => {
                return util::json_error(
                    400,
                    "invalid_compaction",
                    "Compaction belongs to a different owner or pinned context",
                );
            }
        };
        execution_request.input =
            Value::Array(super::native_items::validation_input(&expanded.items)?);
        let effective_instructions = execution_request
            .instructions
            .clone()
            .or(expanded.instructions);
        if execution_request.instructions.is_none() {
            execution_request.instructions = effective_instructions.clone();
        }
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
        let native_input = if config.protocol == agents_core::ProviderProtocol::Responses {
            Some(expanded.items)
        } else {
            if input.native_only {
                return util::json_error(
                    400,
                    "unsupported_native_input",
                    "These items require a native Responses provider",
                );
            }
            None
        };
        let mut prefix = system_prefix(&data.record);
        if config.protocol != agents_core::ProviderProtocol::Responses {
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
        if self.branch_dispatched(&create.response_id).await? {
            return util::json_error(
                409,
                "response_branch_required",
                "This response has an independent child session",
            );
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
        if let Some(lease) = &create.execution.connection_lease {
            self.record_connection_grant(&create.response_id, lease)
                .await?;
            let unchanged = {
                let current = self.data.borrow();
                current.as_ref().is_some_and(|current| {
                    current.run.is_none()
                        && current.record.input_revision == data.record.input_revision
                        && current.response_head == data.response_head
                        && current.record.status == SessionStatus::Idle
                        && !current.archive_pending
                        && !current.archive_preparing
                        && !self.closing.get()
                })
            };
            if !unchanged {
                crate::space::release_response_files(
                    &self.env,
                    &data.record.owner_id,
                    &data.record.id,
                    &create.response_id,
                )
                .await?;
                return util::json_error(
                    409,
                    "session_busy",
                    "Session changed while preparing its connection cache",
                );
            }
        }
        let capacity_generation = match self
            .reserve_conversation_capacity(
                create.execution.conversation.as_ref(),
                &create.response_id,
                &original_items,
                create.execution.warmup,
            )
            .await?
        {
            Ok(generation) => generation,
            Err(response) => {
                if crate::space::release_response_files(
                    &self.env,
                    &data.record.owner_id,
                    &data.record.id,
                    &create.response_id,
                )
                .await
                .is_err()
                {
                    util::emit(
                        "response.file_pin_release_pending",
                        json!({"session_id":data.record.id,"response_id":create.response_id}),
                    );
                }
                return Ok(response);
            }
        };
        let ready = self.snapshot()?;
        if ready.run.is_some()
            || ready.record.input_revision != data.record.input_revision
            || ready.response_head != data.response_head
            || ready.record.status != SessionStatus::Idle
            || ready.archive_pending
            || ready.archive_preparing
            || self.closing.get()
        {
            return util::json_error(
                409,
                "session_busy",
                "Session changed while reserving Conversation capacity",
            );
        }
        self.import_conversation_workspace(conversation_seed.as_ref(), &create.response_id)
            .await?;
        // No await separates this domain preflight from acceptance mutations.
        // Exhausted revisions and non-idle records cannot partially consume tools.
        let _image_observation = self.begin_original_image_observation(
            &create.response_id,
            !create.execution.suppress_observer && (!request.background || request.stream),
        );
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
            if let Some(seed) = &conversation_seed {
                seed.apply(current);
            }
            current
                .history_instructions
                .extend(input.history_instructions.iter().cloned());
            current
                .journal
                .extend(input.history_instructions.iter().cloned());
            current.messages[0] = Message::text("system", prefix);
            current.pending_client_tools.clear();
            current.pending_general_tools.clear();
        }
        self.append_response_input(
            &create.response_id,
            &request,
            &create.execution,
            input.messages,
        )?;
        {
            let mut borrowed = self.data.borrow_mut();
            let current = borrowed.as_mut().unwrap();
            project_response_input(
                &mut current.messages,
                &mut current.pending,
                data.has_pending_client_tools(),
            );
        }
        self.start_run_with_id(create.response_id.clone())?;
        let mut response = ResponseRun {
            conversation: super::conversations::binding(
                create.execution.conversation.as_ref(),
                capacity_generation,
            ),
            id: create.response_id.clone(),
            request: request.clone(),
            resolved_prompt,
            effective_instructions,
            compaction_source_end: None,
            created_at: util::now_ms() / 1000,
            obfuscation_seed: Some(Seed::generate()),
            input_items: original_items,
            output: CanonicalOutput::default(),
            images: ResponseImages::default(),
            image_lifecycle: None,
            events: vec![],
            status: "queued".into(),
            error: None,
            incomplete_reason: None,
            cancel_requested: false,
            awaiting_calls: vec![],
            general_pending_calls: vec![],
            native_stream: super::native_output::NativeStream::default(),
            bound_tools: bound_tools.clone(),
            effective_service_tier: None,
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
            branch_point: None,
            execution: create.execution.persisted(),
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
            if let Some(items) = native_input {
                run.native_input = Some(native::NativeInputBatch {
                    items: native::ordered_input(items, data.has_pending_client_tools()),
                    message_begin: data.messages.len(),
                    message_end: current.messages.len(),
                    history_begin: data.history_instructions.len(),
                });
            }
        }
        {
            let mut accepted = self.snapshot()?;
            save_state(&self.state.storage(), &mut accepted).await?;
        }
        drop(acceptance);
        self.persist().await?;
        if let Some(parent) = create.execution.steering_parent.as_deref() {
            self.forward_steering_successor(parent, Some(&response), &[]);
        }
        if create.execution.warmup {
            self.complete_response_warmup().await?;
        } else {
            self.arm_alarm().await?;
        }
        if create.execution.suppress_observer {
            return Response::from_json(
                &self
                    .public_response_value(&response, &response.request.include)
                    .await?,
            );
        }
        if request.background && !request.stream {
            return Response::from_json(
                &self
                    .public_response_value(&response, &response.request.include)
                    .await?,
            );
        }
        self.observe_response(
            &response,
            request.stream,
            None,
            create.execution.connection_lease.is_some(),
            None,
            true,
        )
        .await
    }

    async fn observe_response(
        &self,
        response: &ResponseRun,
        stream: bool,
        after: Option<u64>,
        follow_steering: bool,
        projection: Option<wire::ResponseProjection>,
        original: bool,
    ) -> Result<Response> {
        // An alarm can complete while create waits for its storage acknowledgement.
        // Attach to the current run, never to a stale queued clone.
        let fresh = self.read_response(&response.id).await?;
        let response = fresh.as_ref().unwrap_or(response);
        let projection =
            projection.unwrap_or_else(|| wire::ResponseProjection::for_request(&response.request));
        let obfuscation = self.response_obfuscation(response).await?;
        let funding = original.then(|| self.original_image_funding(response));
        if funding.as_ref().is_some_and(|funding| !funding.is_empty()) {
            self.arm_original_image_observation(&response.id);
        }
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
            if original {
                self.attach_original_image_stream(&response.id, &tx);
            }
            let callback = if original {
                self.original_image_callback(&response.id)
            } else {
                None
            };
            let mut replay = response
                .events
                .iter()
                .filter(|event| {
                    after.is_none_or(|n| event["sequence_number"].as_u64().is_some_and(|s| s > n))
                })
                .map(|event| {
                    Ok(
                        response_frame(event, &projection, &response.images, &obfuscation)
                            .with_funding_snapshot(funding.as_ref()),
                    )
                })
                .collect::<Vec<_>>();
            if !terminal_status(&response.status) {
                self.response_observers.borrow_mut().push(ResponseObserver {
                    id: response.id.clone(),
                    transport: ResponseTransport::Stream(tx),
                    after,
                    follow_steering,
                    projection,
                    obfuscation,
                    original_image_observer: original,
                });
            } else if original {
                if response.image_lifecycle.is_none() {
                    self.mark_original_image_closed(&response.id);
                } else if let Some(callback) = self.drain_original_image_observation(&response.id) {
                    replay.push(Ok(ResponseFrame::Close(callback)));
                }
            }
            use futures::StreamExt;
            let env = self.env.clone();
            let owner = self.data.borrow().as_ref().unwrap().record.owner_id.clone();
            let source = futures::stream::iter(replay).chain(rx).then(move |frame| {
                let env = env.clone();
                let owner = owner.clone();
                async move { frame?.bytes(&env, &owner).await }
            });
            let body = super::image_observation::ImageObservationBody::new(
                Box::pin(source),
                self.env.clone(),
                self.state.as_raw().clone().into(),
                callback,
            );
            let mut output = Response::from_stream(sse::keep_alive(body))?;
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
            let result = self
                .public_response_value_with_funding(response, &projection.include, funding.as_ref())
                .await;
            if original {
                self.mark_original_image_closed(&response.id);
            }
            return Response::from_json(&result?);
        }
        if self.response_observers.borrow().len() >= MAX_RESPONSE_OBSERVERS {
            return util::json_error(429, "observer_limit", "Too many response observers");
        }
        let (tx, output) = crate::observer_transport::channel("responses")?;
        if original {
            self.attach_original_image_json(&response.id, &tx);
        }
        self.response_observers.borrow_mut().push(ResponseObserver {
            id: response.id.clone(),
            transport: ResponseTransport::Json(tx),
            after: None,
            follow_steering: false,
            projection,
            obfuscation,
            original_image_observer: original,
        });
        Ok(output)
    }

    async fn public_response_value(
        &self,
        response: &ResponseRun,
        include: &[String],
    ) -> Result<Value> {
        self.public_response_value_with_funding(response, include, None)
            .await
    }

    pub(super) async fn public_response_value_with_funding(
        &self,
        response: &ResponseRun,
        include: &[String],
        funding: Option<&crate::response_image_projection::ImageReadFundingMap>,
    ) -> Result<Value> {
        let mut value = response.value_with_include(include);
        let owner = self.data.borrow().as_ref().unwrap().record.owner_id.clone();
        response
            .images
            .hydrate_with_funding(&self.env, &owner, &mut value, funding)
            .await?;
        Ok(value)
    }

    async fn response_obfuscation(&self, response: &ResponseRun) -> Result<Obfuscation> {
        let seed = if let Some(seed) = &response.obfuscation_seed {
            seed.clone()
        } else {
            // Old responses lack admission entropy. Serialize lazy initialization
            // without changing their request, generation state or activity clock.
            let _guard = self.persistence.lock().await;
            let key = format!("response-obfuscation:{}", response.id);
            let storage = self.state.storage();
            match storage.get::<Seed>(&key).await? {
                Some(seed) => seed,
                None => {
                    let seed = Seed::generate();
                    storage.put(&key, &seed).await?;
                    seed
                }
            }
        };
        Ok(Obfuscation::new(seed, &response.id))
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
            .map(|bytes| crate::session_storage::decode_json(&bytes))
            .transpose()
    }

    pub(super) async fn write_response(&self, response: &ResponseRun) -> Result<()> {
        write_chunks(
            &self.state.storage(),
            &format!("{RESPONSE_PREFIX}{}", response.id),
            &serde_json::to_vec(response)?,
        )
        .await
    }

    pub(super) fn response_event(&self, kind: &str, fields: Value) -> Result<()> {
        let mut borrowed = self.data.borrow_mut();
        let Some(response) = borrowed
            .as_mut()
            .and_then(|d| d.run.as_mut())
            .and_then(|r| r.response.as_mut())
        else {
            return Ok(());
        };
        let event = response.event(kind, fields)?;
        let id = response.id.clone();
        drop(borrowed);
        self.publish_committed_response_event(&id, &event);
        Ok(())
    }

    pub(super) fn publish_committed_response_event(&self, id: &str, event: &Value) {
        let mut barrier = self.response_publication_barrier.borrow_mut();
        if barrier.hold(id, event) {
            if barrier.exceeded() {
                if let Some(response) = self
                    .data
                    .borrow_mut()
                    .as_mut()
                    .and_then(|data| data.run.as_mut())
                    .and_then(|run| run.response.as_mut())
                {
                    response.stream_error = Some("response_event_limit".into());
                }
                self.response_observers
                    .borrow_mut()
                    .retain(|observer| observer.id != id);
            }
            return;
        }
        drop(barrier);
        let funding = self
            .data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .filter(|response| response.id == id)
            .map(|response| self.original_image_funding(response))
            .unwrap_or_default();
        if !funding.is_empty() {
            self.arm_original_image_observation(id);
        }
        let images = self
            .data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .filter(|response| response.id == id)
            .map(|response| response.images.clone())
            .unwrap_or_default();
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
                ResponseTransport::Stream(sender) => {
                    let sent = send_response_event(
                        sender,
                        event,
                        &observer.projection,
                        &images,
                        &observer.obfuscation,
                        observer.original_image_observer.then_some(&funding),
                    );
                    if !sent && observer.original_image_observer {
                        let _ = self.drain_original_image_observation(id);
                    }
                    sent
                }
                ResponseTransport::Json(_) => true,
            }
        });
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
        let Some(ref response) = run.response else {
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
        self.register_native_output(completion, false).await?;
        if self.project_native_output(completion).await? {
            self.register_native_output(completion, true).await?;
            return Ok(());
        }

        let plan = {
            let mut data = self.data.borrow_mut();
            data.as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .plan_output()?
        };
        let (has_message, changes) = legacy_completion_plan(response, &run, completion, plan)?;
        let mut changes = changes.into_iter();
        if has_message {
            if response.stream_item.is_none() {
                self.response_delta(text)?;
            }
            let change = changes
                .next()
                .ok_or_else(|| Error::RustError("canonical_output_plan_invalid".into()))?;
            let item = change.item();
            let item_id = item["id"].as_str().unwrap();
            let index = change.index();
            let part = &item["content"][0];
            self.response_event("response.output_text.done", json!({"item_id":item_id,"output_index":index,"content_index":0,"text":text,"logprobs":[]}))?;
            self.response_event(
                "response.content_part.done",
                json!({"item_id":item_id,"output_index":index,"content_index":0,"part":part}),
            )?;
            if let Some(refusal) = item["content"].as_array().and_then(|parts| parts.get(1)) {
                self.response_event("response.content_part.added", json!({"item_id":item_id,"output_index":index,"content_index":1,"part":{"type":"refusal","refusal":""}}))?;
                self.response_event("response.refusal.delta", json!({"item_id":item_id,"output_index":index,"content_index":1,"delta":refusal["refusal"]}))?;
                self.response_event("response.refusal.done", json!({"item_id":item_id,"output_index":index,"content_index":1,"refusal":refusal["refusal"]}))?;
                self.response_event("response.content_part.done", json!({"item_id":item_id,"output_index":index,"content_index":1,"part":refusal}))?;
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
                .apply_output(change)?;
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
        for (published, change) in changes.enumerate() {
            let item = change.item();
            let index = change.index();
            let id = item["id"].as_str().unwrap();
            let mut added = item.clone();
            added["arguments"] = json!("");
            added["status"] = json!("in_progress");
            self.response_event(
                "response.output_item.added",
                json!({"output_index":index,"item":added}),
            )?;
            self.response_event(
                "response.function_call_arguments.delta",
                json!({"item_id":id,"output_index":index,"delta":item["arguments"]}),
            )?;
            self.response_event("response.function_call_arguments.done", json!({"item_id":id,"output_index":index,"arguments":item["arguments"],"name":item["name"]}))?;
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
                .apply_output(change)?;
            if (published + 1).is_multiple_of(2) {
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
        Ok(())
    }

    fn validate_response_output(
        completion: &Completion,
        request: &ResponseRequest,
        first_round: bool,
    ) -> Result<()> {
        if completion.native.is_some() {
            return super::native_output::validate_policy(completion, request, first_round);
        }
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
            .is_some_and(|r| {
                r.general_pending_calls.iter().any(|call| call.name == name)
                    || r.request.tools.iter().any(|t| t.name == name)
            })
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
        self.terminal_steering_events(&mut response)?;
        self.seal_branch_point(&mut response).await?;
        response.branch_point.as_mut().unwrap().pending_calls = response.awaiting_calls.clone();
        response
            .branch_point
            .as_mut()
            .unwrap()
            .general_pending_calls = response.general_pending_calls.clone();
        self.write_response(&response).await?;
        Ok(Some(response))
    }

    pub(super) async fn publish_response(&self, response: ResponseRun) {
        let begin = response
            .events
            .iter()
            .rposition(|event| {
                matches!(
                    event["type"].as_str(),
                    Some(
                        "response.completed" | "response.incomplete" | "response.failed" | "error"
                    )
                )
            })
            .unwrap_or(response.events.len());
        let follow = self.automatic_steering(&response);
        let funding = self.original_image_funding(&response);
        if !funding.is_empty() {
            self.arm_original_image_observation(&response.id);
        }
        let observers = self.response_observers.take();
        let mut remaining = vec![];
        for mut observer in observers {
            if observer.id != response.id {
                remaining.push(observer);
                continue;
            }
            match &mut observer.transport {
                ResponseTransport::Stream(sender) => {
                    let sent = response.events[begin..]
                        .iter()
                        .filter(|event| {
                            observer.after.is_none_or(|after| {
                                event["sequence_number"]
                                    .as_u64()
                                    .is_some_and(|sequence| sequence > after)
                            })
                        })
                        .all(|event| {
                            send_response_event(
                                sender,
                                event,
                                &observer.projection,
                                &response.images,
                                &observer.obfuscation,
                                observer.original_image_observer.then_some(&funding),
                            )
                        });
                    if observer.original_image_observer {
                        if response.image_lifecycle.is_none() {
                            self.mark_original_image_closed(&response.id);
                        } else if sent
                            && let Some(callback) =
                                self.drain_original_image_observation(&response.id)
                        {
                            let _ = sender.try_send(Ok(ResponseFrame::Close(callback)));
                        } else if !sent {
                            let _ = self.drain_original_image_observation(&response.id);
                        }
                    }
                    if sent && observer.follow_steering && follow {
                        remaining.push(observer);
                    }
                }
                ResponseTransport::Json(sender) => {
                    if sender.is_closed() {
                        if observer.original_image_observer {
                            self.mark_original_image_closed(&response.id);
                        }
                        continue;
                    }
                    let (status, value) = match self
                        .public_response_value_with_funding(
                            &response,
                            &observer.projection.include,
                            observer.original_image_observer.then_some(&funding),
                        )
                        .await
                    {
                        Ok(value) => (200, value),
                        Err(_) => (
                            500,
                            util::error_body(
                                "image_projection_failed",
                                "The retained image could not be read; retry retrieval",
                            ),
                        ),
                    };
                    crate::observer_transport::publish(sender.clone(), status, value);
                    if observer.original_image_observer {
                        self.mark_original_image_closed(&response.id);
                    }
                }
            }
        }
        // JSON image reads yield. Preserve observers admitted during projection
        // instead of replacing them with the pre-yield captured collection.
        self.response_observers.borrow_mut().extend(remaining);
        if response.image_lifecycle.is_none() {
            self.mark_original_image_closed(&response.id);
            self.forget_original_image_observation(&response.id);
        }
    }

    pub(super) fn forward_steering_successor(
        &self,
        parent: &str,
        successor: Option<&ResponseRun>,
        events: &[Value],
    ) {
        let images = self
            .data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .filter(|response| response.id == parent)
            .map(|response| response.images.clone())
            .unwrap_or_default();
        self.response_observers.borrow_mut().retain_mut(|observer| {
            if observer.id != parent || !observer.follow_steering {
                return true;
            }
            let ResponseTransport::Stream(sender) = &mut observer.transport else {
                return false;
            };
            if !events.iter().all(|event| {
                send_response_event(
                    sender,
                    event,
                    &observer.projection,
                    &images,
                    &observer.obfuscation,
                    None,
                )
            }) {
                return false;
            }
            let Some(successor) = successor else {
                return false;
            };
            let Some(seed) = &successor.obfuscation_seed else {
                // A legacy successor has no synchronous seed context. Closing
                // permits canonical retrieval; never emit a conflicting prefix.
                return false;
            };
            observer.obfuscation = Obfuscation::new(seed.clone(), &successor.id);
            if !successor.events.iter().all(|event| {
                send_response_event(
                    sender,
                    event,
                    &observer.projection,
                    &successor.images,
                    &observer.obfuscation,
                    None,
                )
            }) {
                return false;
            }
            observer.id = successor.id.clone();
            observer.original_image_observer = false;
            observer.after = None;
            true
        });
    }
}

fn projected_response_frame(
    event: &Value,
    projection: &wire::ResponseProjection,
    obfuscation: &Obfuscation,
) -> Vec<u8> {
    wire::event_frame(&projected_response_value(event, projection, obfuscation))
}

fn projected_response_value(
    event: &Value,
    projection: &wire::ResponseProjection,
    obfuscation: &Obfuscation,
) -> Value {
    let mut event = event.clone();
    wire::project_output_event(&mut event, &projection.include);
    obfuscation.project(&mut event, projection.include_obfuscation);
    event
}

fn send_response_event(
    sender: &mut mpsc::Sender<Result<ResponseFrame>>,
    event: &Value,
    projection: &wire::ResponseProjection,
    images: &ResponseImages,
    obfuscation: &Obfuscation,
    funding: Option<&crate::response_image_projection::ImageReadFundingMap>,
) -> bool {
    sender
        .try_send(Ok(
            response_frame(event, projection, images, obfuscation).with_funding_snapshot(funding)
        ))
        .is_ok()
}

fn response_frame(
    event: &Value,
    projection: &wire::ResponseProjection,
    images: &ResponseImages,
    obfuscation: &Obfuscation,
) -> ResponseFrame {
    let event = projected_response_value(event, projection, obfuscation);
    if images.needs_hydration(&event) {
        ResponseFrame::Image {
            event,
            images: images.clone(),
            funding: None,
        }
    } else {
        ResponseFrame::Encoded(wire::event_frame(&event))
    }
}

fn legacy_completion_plan(
    response: &ResponseRun,
    run: &RunState,
    completion: &Completion,
    mut plan: OutputPlan,
) -> Result<(bool, Vec<OutputChange>)> {
    let mut has_message = false;
    let text = completion.message.content.as_str().unwrap_or("");
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
        let id = response
            .stream_item
            .clone()
            .unwrap_or_else(|| format!("msg_{}_{}", run.id, run.round));
        let mut parts =
            vec![json!({"type":"output_text","text":text,"annotations":[],"logprobs":[]})];
        if let Some(refusal) = completion
            .message
            .extra
            .get("refusal")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            parts.push(json!({"type":"refusal","refusal":refusal}));
        }
        plan.push(json!({"id":id,"type":"message","role":"assistant","status":"completed","content":parts})).map_err(|code| Error::RustError(code.into()))?;
        has_message = true;
    }
    for call in &completion.message.tool_calls {
        if !response
            .request
            .tools
            .iter()
            .any(|tool| tool.name == call.function.name)
        {
            continue;
        }
        let index = plan.next_index();
        plan.push(
            json!({"type":"function_call","id":format!("fc_{}_{}",run.id,index),"call_id":call.id,
            "name":call.function.name,"arguments":call.function.arguments,"status":"completed"}),
        )
        .map_err(|code| Error::RustError(code.into()))?;
    }
    Ok((has_message, plan.finish()))
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

pub(super) fn validate_tool_outputs(
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

    proptest::proptest! {
        #[test]
        fn ordinary_image_aware_frames_preserve_legacy_projection(
            delta in ".{0,1024}", sequence in proptest::prelude::any::<u64>(), obfuscated in proptest::prelude::any::<bool>(),
        ) {
            let event = json!({"type":"response.output_text.delta","sequence_number":sequence,
                "item_id":"msg_known","output_index":0,"content_index":0,"delta":delta});
            let projection = wire::ResponseProjection { include: vec![], include_obfuscation: obfuscated };
            let obfuscation = test_obfuscation();
            let mut expected = event.clone();
            wire::project_output_event(&mut expected, &projection.include);
            obfuscation.project(&mut expected, obfuscated);
            let frame = response_frame(&event, &projection, &ResponseImages::default(), &obfuscation);
            let ResponseFrame::Encoded(bytes) = frame else { panic!("ordinary event requested image I/O") };
            proptest::prop_assert_eq!(bytes, wire::event_frame(&expected));
        }
    }
    use crate::response_preflight::validate_imported_history;

    fn response() -> ResponseRun {
        ResponseRun {
            conversation: None,
            id: "resp_test".into(),
            request: serde_json::from_value(json!({"model":"agent_test","input":"hello"})).unwrap(),
            resolved_prompt: None,
            effective_instructions: None,
            compaction_source_end: None,
            created_at: 10,
            obfuscation_seed: Some(Seed::generate()),
            input_items: vec![],
            output: CanonicalOutput::default(),
            images: ResponseImages::default(),
            image_lifecycle: None,
            events: vec![],
            status: "queued".into(),
            error: None,
            incomplete_reason: None,
            cancel_requested: false,
            awaiting_calls: vec![],
            general_pending_calls: vec![],
            native_stream: super::native_output::NativeStream::default(),
            bound_tools: crate::hosted_bindings::BoundTools::default(),
            effective_service_tier: None,
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
            branch_point: None,
            execution: crate::response_execution::ExecutionContext::default(),
        }
    }

    fn test_obfuscation() -> Obfuscation {
        Obfuscation::new(Seed::generate(), "resp_test")
    }

    proptest::proptest! {
        #[test]
        fn persisted_tagged_tools_restore_response_and_cold_session(score in 0.0f64..=1.0) {
            let mut response = response();
            response.bound_tools = serde_json::from_value(json!([{
                "tool":{"kind":"file_search","options":{
                    "vector_store_ids":["vs_owned"],
                    "ranking_options":{"score_threshold":score}
                }}, "allowed_callers":null
            }])).unwrap();
            let bytes = serde_json::to_vec(&response).unwrap();
            let restored: ResponseRun = crate::session_storage::decode_json(&bytes).unwrap();
            proptest::prop_assert_eq!(serde_json::to_value(&restored).unwrap(), serde_json::to_value(&response).unwrap());

            let mut state = super::super::tests::state();
            let mut value = serde_json::to_value(&state).unwrap();
            value["run"] = json!({"id":"resp_test","round":1,"admitted":true,
                "total":Usage::default(),"pending_completion":null,"started_at_ms":0,
                "response":response});
            state = serde_json::from_value(value).unwrap();
            let bytes = serde_json::to_vec(&state).unwrap();
            let restored: PersistedSession = crate::session_storage::decode_json(&bytes).unwrap();
            proptest::prop_assert_eq!(serde_json::to_value(restored.run.unwrap().response.unwrap().bound_tools).unwrap(),
                serde_json::to_value(state.run.unwrap().response.unwrap().bound_tools).unwrap());
            let exact: Value = crate::session_storage::decode_json(br#"{"integer":9007199254740993}"#).unwrap();
            proptest::prop_assert_eq!(exact["integer"].as_u64(), Some(9_007_199_254_740_993));
        }
    }

    #[test]
    fn persisted_entropy_keeps_same_sequence_frames_identical_without_public_seed() {
        let response = response();
        let serialized = serde_json::to_value(&response).unwrap();
        let restored: ResponseRun = serde_json::from_value(serialized.clone()).unwrap();
        let first = Obfuscation::new(response.obfuscation_seed.clone().unwrap(), &response.id);
        let replay = Obfuscation::new(restored.obfuscation_seed.clone().unwrap(), &restored.id);
        let projection = wire::ResponseProjection {
            include: vec![],
            include_obfuscation: true,
        };
        let event = json!({"type":"response.output_text.delta","sequence_number":4,"delta":"hello","logprobs":[]});
        let prefix = projected_response_frame(&event, &projection, &first);
        assert_eq!(
            prefix,
            projected_response_frame(&event, &projection, &replay)
        );
        assert_ne!(
            prefix,
            projected_response_frame(&event, &projection, &test_obfuscation())
        );
        assert!(response.value().get("obfuscation_seed").is_none());
        assert!(
            !response
                .value()
                .to_string()
                .contains(&serialized["obfuscation_seed"].to_string())
        );
        let mut legacy = serialized;
        legacy.as_object_mut().unwrap().remove("obfuscation_seed");
        let legacy: ResponseRun = serde_json::from_value(legacy).unwrap();
        assert!(legacy.obfuscation_seed.is_none());
    }

    #[test]
    fn output_turn_is_preflighted_before_any_item_or_event_is_published() {
        let mut run = response();
        let first = json!({"id":format!("fc_{}_0",run.id),"type":"function_call","call_id":"one","name":"local","arguments":"{}"});
        let second = json!({"id":format!("msg_{}_1",run.id),"type":"message","content":"x".repeat(MAX_RESPONSE_OUTPUT_BYTES)});
        assert!(run.prepare_output_batch(vec![first, second]).is_err());
        assert!(run.output.is_empty());
        assert!(run.events.is_empty());
    }

    #[test]
    fn output_replacement_replay_and_clear_keep_one_exact_capacity_charge() {
        let mut run = response();
        let id = format!("hosted_{}", "f".repeat(64));
        let initial =
            json!({"id":id,"type":"code_interpreter_call","status":"in_progress","outputs":[]});
        let change = run.prepare_output(None, initial).unwrap();
        run.apply_output(change).unwrap();
        let done = json!({"id":id,"type":"code_interpreter_call","status":"completed","outputs":[{"type":"logs","logs":"雪"}]});
        let change = run.prepare_output(Some(0), done.clone()).unwrap();
        run.apply_output(change).unwrap();
        let bytes = serde_json::to_vec(&run.output).unwrap().len();
        let change = run.prepare_output(Some(0), done).unwrap();
        run.apply_output(change).unwrap();
        assert_eq!(run.output.len(), 1);
        assert_eq!(serde_json::to_vec(&run.output).unwrap().len(), bytes);
        assert!(
            serde_json::to_value(&run)
                .unwrap()
                .get("output_budget")
                .is_none()
        );
        run.output.clear();
        let item = json!({"id":format!("msg_{}_0",run.id),"type":"message","content":[]});
        let change = run.prepare_output(None, item).unwrap();
        run.apply_output(change).unwrap();
        assert_eq!(run.output.len(), 1);
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
    fn observer_projection_is_per_request_and_keeps_private_replay_complete() {
        let mut run = response();
        run.output = vec![
            json!({"type":"code_interpreter_call","id":"ci_1","outputs":[{"type":"logs","logs":"private-result"}]}),
        ].into();
        let public = run.value();
        assert!(public["output"][0]["outputs"].is_null());
        let event = run
            .event("response.completed", json!({"response":public}))
            .unwrap();
        assert_eq!(
            event["response"]["output"][0]["outputs"][0]["logs"],
            "private-result"
        );
        let plain = wire::ResponseProjection {
            include: vec![],
            include_obfuscation: false,
        };
        let expanded = wire::ResponseProjection {
            include: vec!["code_interpreter_call.outputs".into()],
            include_obfuscation: false,
        };
        let plain = String::from_utf8(projected_response_frame(
            &event,
            &plain,
            &test_obfuscation(),
        ))
        .unwrap();
        let expanded = String::from_utf8(projected_response_frame(
            &event,
            &expanded,
            &test_obfuscation(),
        ))
        .unwrap();
        assert!(!plain.contains("private-result"));
        assert!(expanded.contains("private-result"));
        assert_eq!(run.events[0], event);
        assert_eq!(
            run.value_with_include(&["code_interpreter_call.outputs".into()])["output"][0]["outputs"]
                [0]["logs"],
            "private-result"
        );
    }

    #[test]
    fn observer_obfuscation_does_not_change_replay_identity_or_other_observers() {
        let event = json!({"type":"response.output_text.delta","sequence_number":7,"delta":"hello","logprobs":[{"token":"hello"}],"obfuscation":"legacy-random-padding"});
        let plain = wire::ResponseProjection {
            include: vec![],
            include_obfuscation: false,
        };
        let rich = wire::ResponseProjection {
            include: vec!["message.output_text.logprobs".into()],
            include_obfuscation: true,
        };
        let decode = |bytes: Vec<u8>| -> Value {
            let frame = String::from_utf8(bytes).unwrap();
            serde_json::from_str(
                frame
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap(),
            )
            .unwrap()
        };
        let projected = decode(projected_response_frame(
            &event,
            &plain,
            &test_obfuscation(),
        ));
        assert!(projected.get("obfuscation").is_none());
        assert_eq!(projected["logprobs"], json!([]));
        assert_eq!(projected["sequence_number"], 7);
        let projected = decode(projected_response_frame(&event, &rich, &test_obfuscation()));
        assert!(
            projected["obfuscation"]
                .as_str()
                .is_some_and(|padding| (16..=32).contains(&padding.len()))
        );
        assert_eq!(projected["logprobs"], event["logprobs"]);
        assert_eq!(event["obfuscation"], "legacy-random-padding");
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
            native: None,
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
            native: None,
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
