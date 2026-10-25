//! Durable user acceptance and safe-boundary application for Responses steering.
use super::*;
use crate::response_execution::{ExecutionContext, SourceRange};
use crate::responses_wire::ResponseRequest;

const MAX_STEERS: usize = 16;
const MAX_STEERING_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(super) struct PublicationBarrier {
    id: Option<String>,
    events: Vec<Value>,
    bytes: usize,
    overflow: bool,
}

impl PublicationBarrier {
    pub(super) fn hold(&mut self, id: &str, event: &Value) -> bool {
        if self.id.as_deref() != Some(id) {
            return false;
        }
        let bytes = serde_json::to_vec(event)
            .map(|value| value.len())
            .unwrap_or(usize::MAX);
        if self.overflow
            || self.events.len() >= 65_536
            || self.bytes.saturating_add(bytes) > 16 * 1024 * 1024
        {
            self.overflow = true;
            return true;
        }
        self.events.push(event.clone());
        self.bytes += bytes;
        true
    }
    fn begin(&mut self, id: &str, event: &Value) -> Result<()> {
        if self.id.as_deref().is_some_and(|active| active != id) {
            return Err(Error::RustError("response_publication_conflict".into()));
        }
        self.id = Some(id.into());
        self.hold(id, event);
        Ok(())
    }
    pub(super) fn exceeded(&self) -> bool {
        self.overflow
    }

    fn take(&mut self) -> Option<(String, Vec<Value>)> {
        let id = self.id.take()?;
        self.bytes = 0;
        self.overflow = false;
        Some((id, std::mem::take(&mut self.events)))
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SteeringSubmission {
    pub(super) id: String,
    pub(super) parent: String,
    pub(super) input: Value,
    pub(super) source: SourceRange,
    #[serde(default)]
    pub(super) applied_to: Option<String>,
    #[serde(default)]
    pub(super) pending_notified: bool,
}

fn steering_items(input: &Value) -> std::result::Result<Vec<Value>, &'static str> {
    let items = match input {
        Value::String(text) if !text.is_empty() => vec![json!({"role":"user","content":text})],
        Value::Array(items) if !items.is_empty() => items.clone(),
        _ => return Err("invalid_input"),
    };
    for item in &items {
        let object = item.as_object().ok_or("invalid_input")?;
        if item["role"] != "user"
            || item.get("type").is_some_and(|kind| kind != "message")
            || object
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "role" | "content"))
        {
            return Err("invalid_input");
        }
        match &item["content"] {
            Value::String(text) if !text.is_empty() => {}
            Value::Array(parts)
                if !parts.is_empty()
                    && parts.iter().all(|part| {
                        matches!(
                            part["type"].as_str(),
                            Some("input_text" | "input_image" | "input_file")
                        )
                    }) => {}
            _ => return Err("invalid_input"),
        }
    }
    if serde_json::to_vec(input)
        .map_err(|_| "invalid_input")?
        .len()
        > MAX_STEERING_BYTES
    {
        return Err("too_many_pending_steers");
    }
    Ok(items)
}

struct SteeringFailure {
    code: &'static str,
    message: &'static str,
}

fn rejection(code: &'static str, message: &'static str) -> SteeringFailure {
    SteeringFailure { code, message }
}

fn prepare_steering(
    data: &PersistedSession,
    id: &str,
    value: &Value,
) -> std::result::Result<Vec<Message>, SteeringFailure> {
    if !value
        .as_object()
        .is_some_and(|object| object.len() == 1 && object.contains_key("input"))
    {
        return Err(rejection(
            "invalid_input",
            "Steering requires only user input",
        ));
    }
    let items = steering_items(&value["input"])
        .map_err(|code| rejection(code, "Steering requires nonempty user messages"))?;
    let run = data
        .run
        .as_ref()
        .filter(|run| run.id == id)
        .ok_or_else(|| {
            rejection(
                "response_not_active",
                "The response no longer accepts steering",
            )
        })?;
    let response = run
        .response
        .as_ref()
        .ok_or_else(|| rejection("response_not_found", "Response not found"))?;
    if run.ended_at_ms.is_some() || response.cancel_requested {
        return Err(rejection(
            "response_already_completed",
            "The response is ending",
        ));
    }
    if response.request.conversation.is_some()
        || response
            .request
            .context_management
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|entries| !entries.is_empty())
    {
        return Err(rejection(
            "steering_not_supported",
            "Conversation-bound and automatic-compaction requests do not support steering",
        ));
    }
    let pending = data
        .steering_queue
        .iter()
        .filter(|steer| steer.applied_to.is_none())
        .collect::<Vec<_>>();
    let bytes = pending
        .iter()
        .try_fold(0usize, |total, steer| {
            serde_json::to_vec(&steer.input).map(|bytes| total.saturating_add(bytes.len()))
        })
        .map_err(|_| rejection("invalid_input", "Steering could not be encoded"))?;
    let incoming = serde_json::to_vec(&value["input"])
        .map_err(|_| rejection("invalid_input", "Steering could not be encoded"))?
        .len();
    if pending.len() >= MAX_STEERS || bytes.saturating_add(incoming) > MAX_STEERING_BYTES {
        return Err(rejection(
            "too_many_pending_steers",
            "Pending steering capacity exceeded",
        ));
    }
    let mut request = response.request.clone();
    request.input = Value::Array(items);
    request
        .normalize()
        .map(|input| input.messages)
        .map_err(|_| rejection("invalid_input", "Steering input is not valid"))
}

fn combined_input(queued: &[Value], input: &Value) -> Value {
    let mut items = queued.to_vec();
    match input {
        Value::String(text) if !text.is_empty() => {
            items.push(json!({"role":"user","content":text}))
        }
        Value::Array(input) => items.extend(input.iter().cloned()),
        Value::Null => {}
        other => items.push(other.clone()),
    }
    Value::Array(items)
}

fn apply_response_input(
    data: &mut PersistedSession,
    response_id: &str,
    request: &ResponseRequest,
    execution: &ExecutionContext,
    messages: Vec<Message>,
    now: u64,
) -> Result<u64> {
    let selected = data
        .steering_queue
        .iter()
        .filter(|steer| {
            Some(steer.parent.as_str())
                == execution
                    .steering_parent
                    .as_deref()
                    .or(request.previous_response_id.as_deref())
                && steer.applied_to.is_none()
        })
        .collect::<Vec<_>>();
    let consumed = selected
        .iter()
        .map(|steer| steer.source.clone())
        .collect::<Vec<_>>();
    let count = selected
        .iter()
        .map(|steer| steer.source.end - steer.source.begin)
        .sum::<usize>();
    if messages.len() < count {
        return Err(Error::RustError(
            "steering_source_projection_mismatch".into(),
        ));
    }
    let revision = if execution.steering_parent.is_some() && messages.len() == count {
        data.record.input_revision
    } else {
        data.record
            .append_user_input(now)
            .map_err(|code| Error::RustError(code.into()))?
    };
    data.inherited_source_exclusions.retain(|range| {
        !consumed
            .iter()
            .any(|used| used.begin == range.begin && used.end == range.end)
    });
    data.journal.extend(messages[count..].iter().cloned());
    data.pending.extend(messages);
    for steer in &mut data.steering_queue {
        if Some(steer.parent.as_str())
            == execution
                .steering_parent
                .as_deref()
                .or(request.previous_response_id.as_deref())
            && steer.applied_to.is_none()
        {
            steer.applied_to = Some(response_id.into());
        }
    }
    Ok(revision)
}

fn accept_steer(
    data: &mut PersistedSession,
    parent: &str,
    submission_id: &str,
    input: Value,
    messages: Vec<Message>,
    now: u64,
) -> Result<Value> {
    let mut record = data.record.clone();
    record
        .append_user_input(now)
        .map_err(|code| Error::RustError(code.into()))?;
    let begin = data.journal.len();
    let submission = SteeringSubmission {
        id: submission_id.into(),
        parent: parent.into(),
        input,
        source: SourceRange {
            begin,
            end: begin
                .checked_add(messages.len())
                .ok_or_else(|| Error::RustError("source_journal_limit".into()))?,
        },
        applied_to: None,
        pending_notified: false,
    };
    let response = data
        .run
        .as_mut()
        .and_then(|run| run.response.as_mut())
        .ok_or_else(|| Error::RustError("response_not_active".into()))?;
    let event = response.event(
        "response.steer.accepted",
        json!({"steer":{"id":submission.id,"previous_response_id":parent}}),
    )?;
    data.record = record;
    data.journal.extend(messages);
    data.steering_queue.push(submission);
    Ok(event)
}

fn successor_id(parent: &str, first_submission: &str) -> (String, String) {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"responses-steering-successor-v1\0");
    hash.update((parent.len() as u64).to_be_bytes());
    hash.update(parent.as_bytes());
    hash.update(first_submission.as_bytes());
    let digest = format!("{:x}", hash.finalize());
    (format!("resp_{digest}"), digest)
}

fn valid_steering_parent(
    data: &PersistedSession,
    parent: &str,
    child: &str,
    execution: &ExecutionContext,
) -> bool {
    if execution.steering_parent.as_deref() != Some(parent)
        || data.response_head.as_deref() != Some(parent)
        || data.run.is_some()
    {
        return false;
    }
    let queued = data
        .steering_queue
        .iter()
        .filter(|steer| steer.parent == parent && steer.applied_to.is_none())
        .collect::<Vec<_>>();
    let Some(first) = queued.first() else {
        return false;
    };
    if successor_id(parent, &first.id).0 != child {
        return false;
    }
    let mut end = 0;
    for steer in queued {
        if steer.source.begin < end
            || steer.source.begin >= steer.source.end
            || steer.source.end > data.journal.len()
            || data.journal[steer.source.begin..steer.source.end]
                .iter()
                .any(|message| message.role != "user")
        {
            return false;
        }
        end = steer.source.end;
    }
    true
}

impl SessionInner {
    pub(super) fn validate_steering_parent(
        &self,
        parent: &str,
        child: &str,
        execution: &ExecutionContext,
    ) -> Result<bool> {
        let borrowed = self.data.borrow();
        let data = borrowed
            .as_ref()
            .ok_or_else(|| Error::RustError("session_not_initialized".into()))?;
        Ok(valid_steering_parent(data, parent, child, execution))
    }

    pub(super) fn unapplied_steering_ranges(&self) -> Result<Vec<SourceRange>> {
        Ok(self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(|| Error::RustError("session_not_initialized".into()))?
            .steering_queue
            .iter()
            .filter(|steer| steer.applied_to.is_none())
            .map(|steer| steer.source.clone())
            .collect())
    }

    /// Preserve the public request for retry identity; only execution input is expanded.
    pub(super) fn response_steering_input(
        &self,
        request: &ResponseRequest,
        execution: &ExecutionContext,
    ) -> Result<Value> {
        let data = self.data.borrow();
        let data = data.as_ref().unwrap();
        let queued = data
            .steering_queue
            .iter()
            .filter(|steer| {
                Some(steer.parent.as_str())
                    == execution
                        .steering_parent
                        .as_deref()
                        .or(request.previous_response_id.as_deref())
                    && steer.applied_to.is_none()
            })
            .map(|steer| steering_items(&steer.input).map_err(|code| Error::RustError(code.into())))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        Ok(if queued.is_empty() {
            request.input.clone()
        } else {
            combined_input(&queued, &request.input)
        })
    }

    pub(super) fn append_response_input(
        &self,
        response_id: &str,
        request: &ResponseRequest,
        execution: &ExecutionContext,
        messages: Vec<Message>,
    ) -> Result<u64> {
        let mut borrowed = self.data.borrow_mut();
        let data = borrowed.as_mut().unwrap();
        apply_response_input(
            data,
            response_id,
            request,
            execution,
            messages,
            util::now_ms(),
        )
    }

    pub(super) async fn response_steer(&self, id: &str, mut request: Request) -> Result<Response> {
        let value: Value = request.json().await?;
        let _guard = self.persistence.lock().await;
        let messages = match prepare_steering(&self.snapshot()?, id, &value) {
            Ok(messages) => messages,
            Err(error) => {
                return self
                    .reject_response_steer(id, value["input"].clone(), error.code, error.message)
                    .await;
            }
        };
        let event = {
            let mut borrowed = self.data.borrow_mut();
            let current = borrowed.as_mut().unwrap();
            accept_steer(
                current,
                id,
                &util::id("steer"),
                value["input"].clone(),
                messages,
                util::now_ms(),
            )?
        };
        self.response_publication_barrier
            .borrow_mut()
            .begin(id, &event)?;
        let mut accepted = self.snapshot()?;
        save_state(&self.state.storage(), &mut accepted).await?;
        self.flush_response_publication_barrier();
        Response::from_json(&json!({"queued":true}))
    }

    // Caller holds the same acceptance fence as successful steering. Rejected
    // control events use the actor's sequence authority, not a gateway counter.
    async fn reject_response_steer(
        &self,
        id: &str,
        input: Value,
        code: &str,
        message: &str,
    ) -> Result<Response> {
        let Some(mut response) = self.read_response(id).await? else {
            return util::json_error(404, "response_not_found", "Response not found");
        };
        if response.deleted {
            return util::json_error(404, "response_not_found", "Response not found");
        }
        let fields = json!({"error":{"code":code,"type":"invalid_request_error","message":message},"steer":{"previous_response_id":id,"input":input}});
        let event = {
            let mut borrowed = self.data.borrow_mut();
            if let Some(active) = borrowed
                .as_mut()
                .and_then(|data| data.run.as_mut())
                .and_then(|run| run.response.as_mut())
                .filter(|response| response.id == id)
            {
                active.event("response.steer.failed", fields)?
            } else {
                response.event("response.steer.failed", fields)?
            }
        };
        if self
            .snapshot()?
            .run
            .as_ref()
            .is_some_and(|run| run.id == id)
        {
            self.response_publication_barrier
                .borrow_mut()
                .begin(id, &event)?;
            let mut snapshot = self.snapshot()?;
            save_state(&self.state.storage(), &mut snapshot).await?;
            self.flush_response_publication_barrier();
            return Response::from_json(&json!({"queued":true}));
        }
        self.write_response(&response).await?;
        // The control reply is the sole live delivery path for a rejection.
        Response::from_json(&json!({"events":[event]}))
    }

    pub(super) fn flush_response_publication_barrier(&self) {
        let batch = self.response_publication_barrier.borrow_mut().take();
        if let Some((id, events)) = batch {
            for event in events {
                self.publish_committed_response_event(&id, &event);
            }
        }
    }

    /// Called only after server-owned tool groups have closed, before another generation.
    pub(super) fn steering_boundary(&self) -> Result<Option<Value>> {
        let mut borrowed = self.data.borrow_mut();
        let data = borrowed.as_mut().unwrap();
        let Some(run) = data.run.as_mut() else {
            return Ok(None);
        };
        if run.round == 0
            || !data
                .steering_queue
                .iter()
                .any(|steer| steer.parent == run.id && steer.applied_to.is_none())
        {
            return Ok(None);
        }
        let Some(response) = run.response.as_mut() else {
            return Ok(None);
        };
        response.incomplete_reason = Some("steered".into());
        let value = self.response_terminal_value_from_parts(response, &run.total);
        run.terminal = Some(value.clone());
        Ok(Some(value))
    }

    fn response_terminal_value_from_parts(&self, response: &ResponseRun, usage: &Usage) -> Value {
        crate::responses_wire::response_value(
            &response.request,
            &response.id,
            response.created_at,
            "incomplete",
            &response.output,
            Some(usage),
        )
    }
}

impl SessionInner {
    pub(super) fn automatic_steering(&self, response: &ResponseRun) -> bool {
        matches!(response.status.as_str(), "completed" | "incomplete")
            && !response.cancel_requested
            && response.awaiting_calls.is_empty()
            && response.general_pending_calls.is_empty()
            && response
                .incomplete_reason
                .as_deref()
                .is_none_or(|reason| reason == "steered")
            && self.data.borrow().as_ref().is_some_and(|data| {
                data.steering_queue
                    .iter()
                    .any(|steer| steer.parent == response.id && steer.applied_to.is_none())
            })
    }

    pub(super) fn terminal_steering_events(&self, response: &mut ResponseRun) -> Result<()> {
        let mut required = response
            .awaiting_calls
            .iter()
            .map(|call| json!({"type":"function_call_output","call_id":call.id,"name":call.function.name}))
            .collect::<Vec<_>>();
        for call in &response.general_pending_calls {
            if !required.iter().any(|item| item["call_id"] == call.call_id) {
                required.push(if call.kind == "custom" {
                    json!({"type":"custom_tool_call_output","call_id":call.call_id})
                } else {
                    json!({"type":"function_call_output","call_id":call.call_id,"name":call.name})
                });
            }
        }
        let failed = response.status == "failed"
            || response.status == "cancelled"
            || response
                .incomplete_reason
                .as_deref()
                .is_some_and(|reason| reason != "steered");
        let response_id = response.id.clone();
        let mut borrowed = self.data.borrow_mut();
        let data = borrowed.as_mut().unwrap();
        for steer in data
            .steering_queue
            .iter_mut()
            .filter(|steer| steer.parent == response_id && steer.applied_to.is_none())
        {
            if failed {
                response.event("response.steer.failed", json!({"error":{"code":"successor_creation_failed","type":"invalid_request_error","message":"The response could not continue; the accepted input is returned"},"steer":{"id":steer.id,"previous_response_id":steer.parent,"input":steer.input}}))?;
                data.inherited_source_exclusions.push(steer.source.clone());
                steer.applied_to = Some("failed".into());
            } else if !required.is_empty() && !steer.pending_notified {
                response.event("response.steer.pending", json!({"reason":"waiting_for_required_input","required_input":required,"steer":{"id":steer.id,"previous_response_id":steer.parent}}))?;
                steer.pending_notified = true;
            }
        }
        data.steering_queue
            .retain(|steer| steer.applied_to.is_none());
        Ok(())
    }

    pub(super) async fn prepare_response_warmup(&self) -> Result<Value> {
        if self
            .snapshot()?
            .run
            .as_ref()
            .and_then(|run| run.response.as_ref())
            .is_some_and(|response| response.status == "queued")
        {
            self.response_in_progress()?;
        }
        self.drain_pending();
        self.warm_native_input().await?;
        let value = {
            let mut borrowed = self.data.borrow_mut();
            let run = borrowed.as_mut().unwrap().run.as_mut().unwrap();
            let value = run.response.as_ref().unwrap().value();
            run.terminal = Some(value.clone());
            value
        };
        Ok(value)
    }

    pub(super) async fn complete_response_warmup(&self) -> Result<()> {
        let value = match self.prepare_response_warmup().await {
            Ok(value) => value,
            Err(error) => {
                self.arm_alarm().await?;
                return Err(error);
            }
        };
        // The same preparation is used by alarms after a cold/storage retry;
        // a warmup can never fall through into model admission.
        Box::pin(self.finish(Ok(value))).await
    }

    /// Retry selects a deterministic route before any model admission. Accepted
    /// source messages are reused by position, never appended a second time.
    pub(super) async fn advance_response_steering(&self) -> Result<bool> {
        let data = self.snapshot()?;
        if data.run.is_some()
            || data.archive_pending
            || data.archive_preparing
            || self.closing.get()
        {
            return Ok(false);
        }
        let Some(parent) = data.response_head.as_deref() else {
            return Ok(false);
        };
        let Some(first) = data
            .steering_queue
            .iter()
            .find(|steer| steer.parent == parent && steer.applied_to.is_none())
        else {
            return Ok(false);
        };
        let Some(response) = self.read_response(parent).await? else {
            return Ok(false);
        };
        if !self.automatic_steering(&response) {
            return Ok(false);
        }
        let (id, digest) = successor_id(parent, &first.id);
        let mut request = response.request.clone();
        request.previous_response_id = request.conversation.is_none().then(|| parent.into());
        request.input = json!([]);
        request.instructions = response
            .effective_instructions
            .clone()
            .or(request.instructions);
        let mut execution = response.execution.clone();
        execution.inherited_prompt = response
            .resolved_prompt
            .as_ref()
            .map(crate::prompt_templates::ResolvedPrompt::for_successor);
        execution.warmup = false;
        execution.steering_parent = Some(parent.into());
        execution.suppress_observer = true;
        let route = crate::response_index::reserve(
            &self.env,
            &data.record.owner_id,
            crate::response_index::ResponseRoute {
                id: id.clone(),
                session_id: data.record.id.clone(),
                agent_id: data.record.agent_id.clone(),
                request_hash: digest.clone(),
            },
            Some(&format!("steering:{digest}")),
        )
        .await?;
        if route.id != id || route.session_id != data.record.id {
            return Err(Error::RustError("steering_successor_route_conflict".into()));
        }
        let outcome = Box::pin(self.create_response(
            super::responses::CreateResponse {
                response_id: id.clone(),
                request,
                execution,
            },
            None,
        ))
        .await;
        match outcome {
            Ok(reply) if reply.status_code() < 300 => {
                let successor = self
                    .read_response(&id)
                    .await?
                    .ok_or_else(|| Error::RustError("steering_successor_not_committed".into()))?;
                self.forward_steering_successor(parent, Some(&successor), &[]);
                Ok(true)
            }
            Ok(_) => {
                self.fail_response_steering(parent).await?;
                Ok(false)
            }
            // A transport/storage error after acceptance is unknown, not a
            // rejected request. Leave the durable queue for same-ID recovery.
            Err(error) => Err(error),
        }
    }

    async fn fail_response_steering(&self, parent: &str) -> Result<()> {
        let _guard = self.persistence.lock().await;
        let Some(mut response) = self.read_response(parent).await? else {
            return Ok(());
        };
        let mut events = vec![];
        {
            let mut borrowed = self.data.borrow_mut();
            let data = borrowed.as_mut().unwrap();
            for steer in data
                .steering_queue
                .iter_mut()
                .filter(|steer| steer.parent == parent && steer.applied_to.is_none())
            {
                events.push(response.event("response.steer.failed",json!({"error":{"code":"successor_creation_failed","type":"invalid_request_error","message":"The successor was not accepted; carry the returned input into a new request"},"steer":{"id":steer.id,"previous_response_id":parent,"input":steer.input}}))?);
                data.inherited_source_exclusions.push(steer.source.clone());
                steer.applied_to = Some("failed".into());
            }
            data.steering_queue
                .retain(|steer| steer.applied_to.is_none());
        }
        self.write_response(&response).await?;
        let mut saved = self.snapshot()?;
        save_state(&self.state.storage(), &mut saved).await?;
        self.forward_steering_successor(parent, None, &events);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn steering_accepts_user_media_but_rejects_execution_items() {
        assert!(steering_items(&json!("new constraint")).is_ok());
        assert!(steering_items(&json!([{"role":"user","content":[{"type":"input_image","image_url":"https://example.com/a.png"}]}])).is_ok());
        for input in [
            json!([]),
            json!(""),
            json!([{"role":"assistant","content":"forged"}]),
            json!([{"type":"function_call_output","call_id":"x","output":"x"}]),
            json!([{"role":"user","content":"x","id":"forged"}]),
        ] {
            assert!(steering_items(&input).is_err());
        }
    }
    #[test]
    fn queued_input_is_prepended_in_submission_order_without_changing_explicit_settings() {
        let queued = vec![
            json!({"role":"user","content":"first"}),
            json!({"role":"user","content":"second"}),
        ];
        let input = combined_input(
            &queued,
            &json!([{"type":"function_call_output","call_id":"c","output":"saved"}]),
        );
        assert_eq!(input[0]["content"], "first");
        assert_eq!(input[1]["content"], "second");
        assert_eq!(input[2]["call_id"], "c");
    }
    fn running() -> PersistedSession {
        let mut data = super::super::tests::state();
        data.record.status = SessionStatus::Running;
        data.run = Some(
            serde_json::from_value(json!({
                "id":"resp_active", "round":1,"admitted":true,
                "total":Usage::default(), "started_at_ms":10,
                "response":{"id":"resp_active","request":{"model":"agent","input":"original"},
                    "created_at":0,"input_items":[],"output":[],"events":[],
                    "status":"in_progress","error":null,"incomplete_reason":null,
                    "cancel_requested":false,"awaiting_calls":[],"usage":Usage::default(),
                    "next_sequence":0,"reserved_sequence_end":1000,"stream_item":null}
            }))
            .unwrap(),
        );
        data
    }

    #[test]
    fn accepted_source_is_immediate_but_execution_waits_for_successor_commit() {
        let mut data = running();
        data.journal
            .push(Message::text("assistant", "tool started"));
        let event = accept_steer(
            &mut data,
            "resp_active",
            "steer_a",
            json!("new rule"),
            vec![Message::text("user", "new rule")],
            42,
        )
        .unwrap();
        assert_eq!(event["type"], "response.steer.accepted");
        assert_eq!(data.record.last_input_at_ms, 42);
        assert_eq!(data.journal[1].content, "new rule");
        assert_eq!(data.steering_queue[0].source.begin, 1);
        assert_eq!(data.steering_queue[0].source.end, 2);
        assert!(data.pending.is_empty());
        assert_eq!(data.messages.len(), 1);
        data.journal
            .push(Message::tool_result("call", "saved result"));
        let request: ResponseRequest = serde_json::from_value(
            json!({"model":"agent","input":[],"previous_response_id":"resp_active"}),
        )
        .unwrap();
        let execution = ExecutionContext {
            steering_parent: Some("resp_active".into()),
            ..Default::default()
        };
        apply_response_input(
            &mut data,
            "resp_successor",
            &request,
            &execution,
            vec![Message::text("user", "new rule")],
            1000,
        )
        .unwrap();
        assert_eq!(data.journal.len(), 3);
        assert_eq!(data.journal[2].role, "tool");
        assert_eq!(data.pending[0].content, "new rule");
        assert_eq!(
            data.record.last_input_at_ms, 42,
            "automatic work is not new user activity"
        );
        assert_eq!(
            data.steering_queue[0].applied_to.as_deref(),
            Some("resp_successor")
        );
    }

    proptest::proptest! {
        #[test]
        fn queued_source_order_survives_interspersed_tool_results(inputs in proptest::collection::vec("[a-z]{1,24}",1..12)) {
            let mut data = running();
            for (index,input) in inputs.iter().enumerate() {
                accept_steer(&mut data,"resp_active",&format!("steer_{index}"),json!(input),vec![Message::text("user",input)],index as u64+1).unwrap();
                data.journal.push(Message::tool_result(&format!("call_{index}"),"done"));
            }
            let journal = serde_json::to_value(&data.journal).unwrap();
            let ranges = data.steering_queue.iter().map(|steer| steer.source.clone()).collect::<Vec<_>>();
            for (index,range) in ranges.iter().enumerate() {
                proptest::prop_assert_eq!(range.end-range.begin,1);
                proptest::prop_assert_eq!(data.journal[range.begin].content.as_str(),Some(inputs[index].as_str()));
            }
            let request: ResponseRequest = serde_json::from_value(json!({"model":"agent","input":[],"previous_response_id":"resp_active"})).unwrap();
            let execution = ExecutionContext {steering_parent:Some("resp_active".into()),..Default::default()};
            apply_response_input(&mut data,"resp_next",&request,&execution,inputs.iter().map(|input|Message::text("user",input)).collect(),10000).unwrap();
            proptest::prop_assert_eq!(serde_json::to_value(&data.journal).unwrap(),journal);
            proptest::prop_assert_eq!(data.pending.len(),inputs.len());
            proptest::prop_assert!(data.steering_queue.iter().all(|steer|steer.applied_to.as_deref()==Some("resp_next")));
            proptest::prop_assert_eq!(data.record.last_input_at_ms,inputs.len() as u64);
        }
    }
    #[test]
    fn publication_waits_for_durable_ack_and_flushes_once_in_sequence_order() {
        let mut barrier = PublicationBarrier::default();
        let accepted = json!({"type":"response.steer.accepted","sequence_number":7});
        barrier.begin("resp", &accepted).unwrap();
        assert!(barrier.hold(
            "resp",
            &json!({"type":"response.output_text.delta","sequence_number":8,"delta":"next"})
        ));
        assert!(!barrier.hold("another", &json!({"sequence_number":0})));
        let (id, events) = barrier.take().unwrap();
        assert_eq!(id, "resp");
        assert_eq!(
            events
                .iter()
                .map(|event| event["sequence_number"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![7, 8]
        );
        assert!(barrier.take().is_none());
        assert!(!barrier.hold("resp", &json!({"sequence_number":9})));
    }

    proptest::proptest! {
        #[test]
        fn publication_barrier_never_duplicates_or_reorders_ready_deltas(count in 0usize..128) {
            let mut barrier = PublicationBarrier::default();
            barrier.begin("resp",&json!({"sequence_number":0})).unwrap();
            for sequence in 1..=count {
                let held = barrier.hold("resp", &json!({"sequence_number":sequence}));
                proptest::prop_assert!(held);
            }
            let (_,events) = barrier.take().unwrap();
            proptest::prop_assert_eq!(events.len(),count+1);
            for (sequence,event) in events.iter().enumerate() {
                proptest::prop_assert_eq!(event["sequence_number"].as_u64(),Some(sequence as u64));
            }
        }
    }
    #[test]
    fn private_successor_authority_requires_committed_intent_not_a_transport_flag() {
        let mut data = running();
        accept_steer(
            &mut data,
            "resp_active",
            "steer_a",
            json!("rule"),
            vec![Message::text("user", "rule")],
            42,
        )
        .unwrap();
        data.run = None;
        data.response_head = Some("resp_active".into());
        let execution = ExecutionContext {
            steering_parent: Some("resp_active".into()),
            ..Default::default()
        };
        let child = successor_id("resp_active", "steer_a").0;
        assert!(valid_steering_parent(
            &data,
            "resp_active",
            &child,
            &execution
        ));
        assert!(!valid_steering_parent(
            &data,
            "resp_active",
            "resp_arbitrary",
            &execution
        ));
        assert!(!valid_steering_parent(
            &data,
            "resp_active",
            &child,
            &ExecutionContext::default()
        ));
        assert!(!valid_steering_parent(&data, "another", &child, &execution));
        data.steering_queue[0].applied_to = Some(child.clone());
        assert!(!valid_steering_parent(
            &data,
            "resp_active",
            &child,
            &execution
        ));
        data.steering_queue[0].applied_to = None;
        data.steering_queue[0].source.end = data.journal.len() + 1;
        assert!(!valid_steering_parent(
            &data,
            "resp_active",
            &child,
            &execution
        ));
    }
}
