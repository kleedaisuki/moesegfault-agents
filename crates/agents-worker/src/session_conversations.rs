//! Conversation snapshots select context; the Session remains the only harness.
use super::*;
use crate::response_execution::ConversationInput;
use crate::responses_wire::ResponseRequest;
use crate::{conversation_meter::ConversationOperation, conversation_routes};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ConversationBinding {
    pub(super) id: String,
    pub(super) accepted_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capacity_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication_error: Option<String>,
    #[serde(default)]
    pub(super) prepared: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ConversationRelease {
    id: String,
    response_id: String,
}

pub(super) struct ConversationSeed {
    pub(super) attachments: Vec<crate::space::OwnedFile>,
    pub(super) remote_frozen: bool,
    journal: Vec<Message>,
    messages: Vec<Message>,
    pending: Vec<Message>,
    instructions: Vec<Message>,
    calls: Vec<provider::ToolCall>,
    general_calls: Vec<native_output::GeneralPendingCall>,
    native: Option<native::NativeContext>,
}

pub(super) fn binding(
    input: Option<&ConversationInput>,
    capacity_generation: Option<String>,
) -> Option<ConversationBinding> {
    input.map(|input| ConversationBinding {
        id: input.admission.id.clone(),
        accepted_revision: input.admission.revision,
        capacity_generation,
        publication_error: None,
        prepared: false,
    })
}

pub(super) fn previous(
    request: &ResponseRequest,
    input: Option<&ConversationInput>,
) -> Option<String> {
    match input {
        Some(input) if input.reuse_head => input.admission.predecessor_response_id.clone(),
        Some(_) => None,
        None => request.previous_response_id.clone(),
    }
}

fn pending_calls(items: &[Value]) -> Vec<native_output::GeneralPendingCall> {
    let mut pending = BTreeMap::new();
    for item in items {
        if let Some(call) = native_output::GeneralPendingCall::from_item(item) {
            pending.insert(call.call_id.clone(), call);
        }
        if matches!(
            item["type"].as_str(),
            Some("function_call_output" | "custom_tool_call_output")
        ) {
            if let Some(id) = item["call_id"].as_str() {
                pending.remove(id);
            }
        }
    }
    pending.into_values().collect()
}

fn valid_generation(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn capacity_generation(binding: &ConversationBinding) -> Result<&str> {
    binding
        .capacity_generation
        .as_deref()
        .filter(|value| valid_generation(value))
        .ok_or_else(|| Error::RustError("conversation_capacity_generation_missing".into()))
}

fn reserved_generation(value: &Value) -> Result<String> {
    value["capacity_generation"]
        .as_str()
        .filter(|value| valid_generation(value))
        .map(str::to_owned)
        .ok_or_else(|| Error::RustError("conversation_capacity_generation_invalid".into()))
}

fn definite_publication_rejection(status: u16, capacity_pending: bool) -> bool {
    (400..500).contains(&status) && !matches!(status, 408 | 425 | 429) && !capacity_pending
}

pub(super) fn request_response_cancel(run: &mut RunState) -> bool {
    let Some(response) = run.response.as_ref() else {
        return true;
    };
    if let Some(binding) = &response.conversation
        && (run.terminal.is_some()
            || run.terminal_error.is_some()
            || run.ended_at_ms.is_some()
            || binding.prepared)
    {
        return false;
    }
    if let Some(response) = &mut run.response {
        response.cancel_requested = true;
    }
    true
}

fn same_conversation_input_epoch(data: &PersistedSession, id: &str, generation: &str) -> bool {
    let Some(run) = data
        .run
        .as_ref()
        .filter(|run| run.id == id && run.ended_at_ms.is_none())
    else {
        return false;
    };
    let Some(response) = run
        .response
        .as_ref()
        .filter(|response| !response.cancel_requested)
    else {
        return false;
    };
    let Some(binding) = response.conversation.as_ref() else {
        return false;
    };
    !binding.prepared && binding.capacity_generation.as_deref() == Some(generation)
}

impl ConversationSeed {
    pub(super) fn apply(&self, data: &mut PersistedSession) {
        data.journal = self.journal.clone();
        data.messages.truncate(1);
        data.messages.extend(self.messages.iter().cloned());
        data.pending = self.pending.clone();
        data.history_instructions = self.instructions.clone();
        data.pending_client_tools = self.calls.clone();
        data.pending_general_tools = self.general_calls.clone();
        data.native_context = self.native.clone();
        data.input_tokens = 0;
        data.measured_messages = 0;
    }
}

impl SessionInner {
    pub(super) async fn prepare_conversation_execution(
        &self,
        request: &ResponseRequest,
        response_id: &str,
        execution: &mut crate::response_execution::ExecutionContext,
    ) -> Result<()> {
        if execution.conversation.is_some() {
            return Ok(());
        }
        let Some(id) = request.conversation_id().map_err(Error::RustError)? else {
            return Ok(());
        };
        let record = self.data.borrow().as_ref().unwrap().record.clone();
        let mut response =
            conversation_routes::admit(&self.env, &record.owner_id, id, response_id).await?;
        if response.status_code() >= 300 {
            return Err(Error::RustError("conversation_admission_failed".into()));
        }
        let admission: conversation_routes::ConversationAdmission = response.json().await?;
        let reuse_head = admission.predecessor_session_id.as_deref() == Some(record.id.as_str())
            && admission.predecessor_agent_id.as_deref() == Some(record.agent_id.as_str())
            && admission.predecessor_pin_hash.as_deref()
                == Some(record.pinned.prefix_hash().as_str());
        execution.conversation = Some(ConversationInput {
            admission,
            reuse_head,
        });
        Ok(())
    }

    pub(super) async fn prepare_conversation_seed(
        &self,
        request: &ResponseRequest,
        input: Option<&ConversationInput>,
        response_id: &str,
    ) -> Result<Option<ConversationSeed>> {
        let Some(input) = input else {
            return Ok(None);
        };
        if request.conversation_id().map_err(Error::RustError)? != Some(input.admission.id.as_str())
        {
            return Err(Error::RustError("conversation_binding_mismatch".into()));
        }
        let data = self.snapshot()?;
        if input.reuse_head {
            if input.admission.predecessor_session_id.as_deref() != Some(data.record.id.as_str())
                || input.admission.predecessor_agent_id.as_deref()
                    != Some(data.record.agent_id.as_str())
                || input.admission.predecessor_pin_hash.as_deref()
                    != Some(data.record.pinned.prefix_hash().as_str())
                || input.admission.predecessor_response_id != data.response_head
            {
                return Err(Error::RustError("conversation_context_changed".into()));
            }
            return Ok(None);
        }
        if data.run.is_some() || !data.journal.is_empty() || data.response_head.is_some() {
            return Err(Error::RustError("conversation_context_changed".into()));
        }
        let resolved = crate::responses_attachments::resolve(
            &self.env,
            &data.record.owner_id,
            &format!("{response_id}:conversation"),
            &json!(input.admission.items),
            data.record.pinned.workspace,
            &self.state.storage(),
        )
        .await?;
        let source_items = resolved
            .input
            .as_array()
            .ok_or_else(|| Error::RustError("conversation_history_invalid".into()))?;
        let (items, native_history) = self
            .native_conversation_import(
                source_items,
                &input.admission.item_provenance,
                &format!("conversation:{response_id}"),
            )
            .await?;
        let mut history = request.clone();
        history.input = json!(items);
        history.instructions = None;
        history.previous_response_id = None;
        history.conversation = None;
        history.prompt = None;
        let normalized = history.normalize_structure().map_err(Error::RustError)?;
        let mut journal = normalized.history_instructions.clone();
        journal.extend(normalized.messages);
        let (messages, pending) = branch::project_branch_history(&journal)?;
        let general_calls = pending_calls(&input.admission.items);
        let calls = general_calls
            .iter()
            .filter(|call| call.kind == "function" && call.namespace.is_none())
            .map(|call| provider::ToolCall {
                id: call.call_id.clone(),
                kind: "function".into(),
                function: provider::FunctionCall {
                    name: call.name.clone(),
                    arguments: call.input.clone(),
                },
            })
            .collect();
        let native = native_history.map(|history| native::NativeContext {
            history,
            messages_end: messages.len() + 1,
        });
        Ok(Some(ConversationSeed {
            attachments: resolved.attachments,
            remote_frozen: resolved.remote_frozen,
            journal,
            messages,
            pending,
            instructions: normalized.history_instructions,
            calls,
            general_calls,
            native,
        }))
    }

    pub(super) async fn reserve_conversation_capacity(
        &self,
        input: Option<&ConversationInput>,
        response_id: &str,
        input_items: &[Value],
        warmup: bool,
    ) -> Result<std::result::Result<Option<String>, Response>> {
        let Some(input) = input else {
            return Ok(Ok(None));
        };
        let record = self.data.borrow().as_ref().unwrap().record.clone();
        let capacity = conversation_routes::ConversationCapacityRequest {
            input_items: input_items.to_vec(),
            input_provenance: BTreeMap::new(),
            agent_id: Some(record.agent_id.clone()),
            pin_hash: Some(record.pinned.prefix_hash()),
            profile: Some(record.runtime_profile),
            warmup,
        };
        let mut response = conversation_routes::reserve_capacity(
            &self.env,
            &record.owner_id,
            &input.admission.id,
            response_id,
            &record.id,
            input.admission.revision,
            &capacity,
        )
        .await?;
        if response.status_code() >= 300 {
            return Ok(Err(response));
        }
        let value: Value = response.json().await?;
        Ok(Ok(Some(reserved_generation(&value)?)))
    }

    pub(super) async fn accept_legacy_steer(
        &self,
        content: &str,
        traceparent: Option<String>,
    ) -> Result<Response> {
        let guard = self.persistence.lock().await;
        let data = self.snapshot()?;
        if data.has_pending_client_tools() {
            return util::json_error(
                409,
                "client_tool_outputs_required",
                "Submit outstanding function results through Responses before continuing",
            );
        }
        if self.closing.get() || data.archive_preparing || data.archive_pending {
            return util::json_error(409, "session_closing", "Session is being archived");
        }
        if data.record.status == SessionStatus::Closed {
            return util::json_error(
                409,
                "session_closed",
                "Resume the archived session before appending messages",
            );
        }
        let mut check = data.record.clone();
        check
            .append_user_input(util::now_ms())
            .map_err(|code| Error::RustError(code.into()))?;
        if data.run.is_none() {
            check
                .start_run()
                .map_err(|code| Error::RustError(code.into()))?;
        }
        let mut accepted_items = None;
        if let Some(run) = &data.run
            && let Some(response) = &run.response
            && let Some(binding) = &response.conversation
        {
            if run.terminal.is_some()
                || run.terminal_error.is_some()
                || run.ended_at_ms.is_some()
                || binding.prepared
                || response.cancel_requested
            {
                return util::json_error(
                    409,
                    "conversation_finalizing",
                    "Conversation response no longer accepts input",
                );
            }
            let Some(generation) = binding.capacity_generation.as_deref() else {
                return util::json_error(
                    409,
                    "conversation_capacity_generation_missing",
                    "Conversation input has no active capacity envelope",
                );
            };
            let submission = util::id("steer");
            let items = crate::responses_wire::canonical_input_items(&json!(content), &submission)
                .map_err(Error::RustError)?;
            let capacity = conversation_routes::ConversationCapacityRequest {
                input_items: items.clone(),
                input_provenance: BTreeMap::new(),
                agent_id: Some(data.record.agent_id.clone()),
                pin_hash: Some(data.record.pinned.prefix_hash()),
                profile: Some(data.record.runtime_profile),
                warmup: false,
            };
            let reserved = conversation_routes::reserve_additional_input(
                &self.env,
                &data.record.owner_id,
                &binding.id,
                &run.id,
                &data.record.id,
                generation,
                &submission,
                &capacity,
            )
            .await?;
            if reserved.status_code() >= 300 {
                return Ok(reserved);
            }
            let current = self.snapshot()?;
            let same_run = same_conversation_input_epoch(&current, &run.id, generation);
            // The same mutex orders accepted input before terminal conversion;
            // a model result may arrive during the RPC, but cannot convert it.
            if !same_run
                || current.record.input_revision != data.record.input_revision
                || self.closing.get()
                || current.archive_preparing
                || current.archive_pending
            {
                return util::json_error(
                    409,
                    "conversation_finalizing",
                    "Conversation execution changed while reserving input",
                );
            }
            accepted_items = Some(items);
        }
        let revision = self.append(vec![Message::text("user", content)])?;
        if let Some(items) = accepted_items {
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
                .input_items
                .extend(items);
        }
        if self.data.borrow().as_ref().unwrap().run.is_none() {
            self.start_run()?;
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .traceparent = traceparent;
        }
        let mut accepted = self.snapshot()?;
        save_state(&self.state.storage(), &mut accepted).await?;
        drop(guard);
        self.persist().await?;
        self.arm_alarm().await?;
        Response::from_json(&json!({"queued":true,"input_revision":revision}))
    }

    pub(super) async fn activate_conversation_run(&self, run: &RunState) -> Result<()> {
        let Some(binding) = run
            .response
            .as_ref()
            .and_then(|response| response.conversation.as_ref())
        else {
            return Ok(());
        };
        let record = self.data.borrow().as_ref().unwrap().record.clone();
        let generation = capacity_generation(binding)?;
        let response = conversation_routes::activate_generation(
            &self.env,
            &record.owner_id,
            &binding.id,
            &run.id,
            &record.id,
            binding.accepted_revision,
            generation,
        )
        .await?;
        if response.status_code() >= 300 {
            return Err(Error::RustError("conversation_admission_expired".into()));
        }
        Ok(())
    }

    pub(super) async fn prepare_conversation_commit(
        &self,
        terminal_failed: bool,
    ) -> Result<Option<String>> {
        let _guard = self.persistence.lock().await;
        let data = self.snapshot()?;
        let Some(run) = data.run.as_ref() else {
            return Ok(None);
        };
        let Some(response) = run.response.as_ref() else {
            return Ok(None);
        };
        let Some(binding) = response.conversation.as_ref() else {
            return Ok(None);
        };
        if binding.prepared {
            return Ok(binding.publication_error.clone());
        }
        let mut publication_error = None;
        let generation = capacity_generation(binding)?;
        let noappend = terminal_failed || response.cancel_requested;
        if noappend {
            let released = conversation_routes::release_noappend_actor(
                &self.env,
                &data.record.owner_id,
                &binding.id,
                &run.id,
                generation,
            )
            .await?;
            if released.status_code() >= 300 {
                return Err(Error::RustError(
                    "conversation_capacity_release_pending".into(),
                ));
            }
        } else if run.admitted || response.execution.warmup {
            if !run.admitted {
                self.activate_conversation_run(run).await?;
            }
            let operation = if run.admitted {
                ConversationOperation::session(&run.id, &data.record.id)?
            } else {
                ConversationOperation::response_actor(&self.env, &data.record.owner_id, &run.id)
                    .await?
            };
            let provider_binding = self
                .native_branch_ref()?
                .map(|history| history.provider_binding);
            let output_provenance = Some(conversation_routes::ConversationItemProvenance {
                source_session_id: Some(data.record.id.clone()),
                response_id: Some(run.id.clone()),
                provider_binding,
            });
            let context = conversation_routes::ConversationExecutionContext {
                image_assets: self.conversation_image_assets(response),
                capacity_generation: Some(generation.to_owned()),
                session_id: data.record.id.clone(),
                agent_id: data.record.agent_id.clone(),
                pin_hash: data.record.pinned.prefix_hash(),
                accepted_revision: binding.accepted_revision,
                defer_release: true,
                item_provenance: BTreeMap::new(),
                output_provenance,
            };
            let mut items = response.input_items.clone();
            items.extend(response.output.iter().cloned());
            let committed = conversation_routes::commit_response_with_meter(
                &self.env,
                &data.record.owner_id,
                &binding.id,
                &run.id,
                &items,
                Some(&context),
                &operation,
            )
            .await?;
            if committed.status_code() >= 300 {
                let status = committed.status_code();
                let pending = committed
                    .headers()
                    .get("X-Agents-Conversation-Capacity-Status")?
                    .is_some();
                if !definite_publication_rejection(status, pending) {
                    return Err(Error::RustError("conversation_commit_pending".into()));
                }
                let closed = conversation_routes::release_noappend_actor(
                    &self.env,
                    &data.record.owner_id,
                    &binding.id,
                    &run.id,
                    generation,
                )
                .await?;
                if closed.status_code() >= 300 {
                    return Err(Error::RustError(
                        "conversation_capacity_release_pending".into(),
                    ));
                }
                publication_error = Some("conversation_publication_rejected".to_owned());
                util::emit(
                    "conversation.publication_rejected",
                    json!({"session_id":data.record.id,
                    "response_id":run.id,"status":status}),
                );
            }
            operation.finish(&self.env, &data.record.owner_id).await?;
        }
        let mut current = self.data.borrow_mut();
        let current = current.as_mut().unwrap();
        let accepted = current
            .run
            .as_mut()
            .unwrap()
            .response
            .as_mut()
            .unwrap()
            .conversation
            .as_mut()
            .unwrap();
        accepted.prepared = true;
        accepted.publication_error = publication_error.clone();
        if !current
            .conversation_releases
            .iter()
            .any(|release| release.response_id == run.id)
        {
            current.conversation_releases.push(ConversationRelease {
                id: binding.id.clone(),
                response_id: run.id.clone(),
            });
        }
        Ok(publication_error)
    }

    pub(super) async fn reconcile_conversation_releases(&self) -> Result<()> {
        let ready = self.data.borrow().as_ref().is_some_and(|data| {
            !data.conversation_releases.is_empty() && data.conversation_retry_at <= util::now_ms()
        });
        if !ready {
            return Ok(());
        }
        let _guard = self.persistence.lock().await;
        self.release_conversations_locked().await
    }

    pub(super) async fn release_conversations_locked(&self) -> Result<()> {
        let pending = self.snapshot()?.conversation_releases;
        for release in pending.into_iter().take(16) {
            let data = self.snapshot()?;
            if data
                .run
                .as_ref()
                .is_some_and(|run| run.id == release.response_id)
            {
                continue;
            }
            let released = conversation_routes::release_actor(
                &self.env,
                &data.record.owner_id,
                &release.id,
                &release.response_id,
            )
            .await;
            if !released.is_ok_and(|response| response.status_code() < 300) {
                let mut data = self.snapshot()?;
                data.conversation_retry_at = util::now_ms().saturating_add(30_000);
                save_state(&self.state.storage(), &mut data).await?;
                self.data
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .conversation_retry_at = data.conversation_retry_at;
                util::emit(
                    "conversation.release_pending",
                    json!({"session_id":data.record.id,"response_id":release.response_id}),
                );
                return Ok(());
            }
            let mut data = self.snapshot()?;
            data.conversation_releases
                .retain(|entry| entry.response_id != release.response_id);
            data.conversation_retry_at = 0;
            save_state(&self.state.storage(), &mut data).await?;
            let mut current = self.data.borrow_mut();
            let current = current.as_mut().unwrap();
            current.conversation_releases = data.conversation_releases;
            current.conversation_retry_at = 0;
        }
        Ok(())
    }
}

pub(super) fn cleanup_deadline(data: &PersistedSession, now: u64) -> Option<u64> {
    (!data.conversation_releases.is_empty())
        .then(|| data.conversation_retry_at.max(now.saturating_add(1000)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_predecessor_is_private_and_fresh_lane_has_no_parent() {
        let request: ResponseRequest = serde_json::from_value(json!({
            "model":"agent", "conversation":"conv_a", "input":"new",
        }))
        .unwrap();
        let mut input = ConversationInput {
            admission: conversation_routes::ConversationAdmission {
                id: "conv_a".into(),
                revision: 3,
                items: vec![],
                predecessor_response_id: Some("resp_prior".into()),
                predecessor_session_id: Some("session".into()),
                predecessor_agent_id: Some("agent".into()),
                predecessor_pin_hash: Some("pin".into()),
                item_provenance: BTreeMap::new(),
            },
            reuse_head: true,
        };
        assert_eq!(
            previous(&request, Some(&input)).as_deref(),
            Some("resp_prior")
        );
        assert!(request.previous_response_id.is_none());
        assert_eq!(request.conversation_id().unwrap(), Some("conv_a"));
        input.reuse_head = false;
        assert!(previous(&request, Some(&input)).is_none());
    }

    #[test]
    fn conversation_cleanup_keeps_closed_session_recoverable() {
        let mut data = super::super::tests::state();
        data.record.status = SessionStatus::Closed;
        assert_eq!(cleanup_deadline(&data, 100), None);
        data.conversation_releases.push(ConversationRelease {
            id: "conv_a".into(),
            response_id: "resp_pending".into(),
        });
        data.conversation_retry_at = 30_000;
        assert_eq!(cleanup_deadline(&data, 100), Some(30_000));
    }

    #[test]
    fn pending_tool_projection_closes_each_group_without_replaying_old_calls() {
        let items = vec![
            json!({"type":"function_call","id":"fc_a","call_id":"a","name":"local","arguments":"{}"}),
            json!({"type":"function_call_output","id":"out_a","call_id":"a","output":"done"}),
            json!({"type":"custom_tool_call","id":"ct_b","call_id":"b","name":"raw","input":"new"}),
        ];
        let pending = pending_calls(&items);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].call_id, "b");
        assert_eq!(pending[0].kind, "custom");
    }
}
