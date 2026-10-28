use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetireEmpty {
    response_id: String,
}

#[derive(Default)]
struct RetirementReceipts {
    response: bool,
    response_storage: bool,
    workspace_storage: bool,
    source_journal: bool,
    attachment_pins: bool,
}

impl RetirementReceipts {
    fn empty(&self) -> bool {
        !self.response
            && !self.response_storage
            && !self.workspace_storage
            && !self.source_journal
            && !self.attachment_pins
    }
}

fn valid_response_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn empty_source(data: &PersistedSession) -> bool {
    let prefix = data.messages.first();
    matches!(
        data.record.status,
        SessionStatus::Idle | SessionStatus::Closed
    ) && data.record.input_revision == 0
        && data.record.last_input_at_ms == data.record.created_at_ms
        && data.record.workspace_snapshot.is_none()
        && data.response_head.is_none()
        && data.run.is_none()
        && data.journal.is_empty()
        && data.journal_count == 0
        && data.pending.is_empty()
        && data.pending_client_tools.is_empty()
        && data.history_instructions.is_empty()
        && data.input_tokens == 0
        && data.compact_count == 0
        && data.measured_messages <= 1
        && data.last_error.is_none()
        && data.checkpoint_error.is_none()
        && !data.archive_pending
        && !data.archive_preparing
        && data.archive_funding.is_none()
        && data.archive_backstop_generation.is_none()
        && data.archive_key.is_none()
        && data.archive_started_at_ms.is_none()
        && data.messages.len() == 1
        && prefix.is_some_and(|message| {
            message.role == "system"
                && message.content == Value::String(system_prefix(&data.record))
                && message.tool_calls.is_empty()
                && message.tool_call_id.is_none()
                && message.extra.is_empty()
        })
}

fn same_empty_source(before: &PersistedSession, after: &PersistedSession) -> bool {
    empty_source(before)
        && empty_source(after)
        && initialization_matches(&before.record, &after.record)
}

fn retirement_conflict() -> Result<Response> {
    util::json_error(
        409,
        "session_not_empty",
        "Session admission or durable activity prevents empty-session retirement",
    )
}

impl SessionInner {
    /// Used only by the edge after a definite rejected creation on a fresh chain.
    /// Absence of an HTTP reply, an index row, or an old timestamp is not proof
    /// that execution was never accepted.
    pub(super) async fn retire_empty(&self, mut req: Request) -> Result<Response> {
        let input: RetireEmpty = match req.json().await {
            Ok(input) => input,
            Err(_) => {
                return util::json_error(400, "invalid_request", "A response_id is required");
            }
        };
        if !valid_response_id(&input.response_id) {
            return util::json_error(400, "invalid_response_id", "Invalid response identifier");
        }
        if self.running.get()
            || self.closing.get()
            || self.observer.borrow().is_some()
            || !self.response_observers.borrow().is_empty()
            || !self.data.borrow().as_ref().is_some_and(empty_source)
        {
            return retirement_conflict();
        }
        // Publish the gate before yielding. Every accepting path rechecks it
        // after I/O; the persistence mutex also fences file-pin admission.
        self.closing.set(true);
        let result = self.retire_empty_locked(&input.response_id).await;
        self.closing.set(false);
        result
    }

    async fn retire_empty_locked(&self, response_id: &str) -> Result<Response> {
        {
            let _guard = self.persistence.lock().await;
            let data = self.snapshot()?;
            if self.running.get() || !empty_source(&data) {
                return retirement_conflict();
            }
            let storage = self.state.storage();
            let response = self.read_response(response_id).await?.is_some();
            let response_storage = storage
                .list_with_options(ListOptions::new().prefix("response:").limit(1))
                .await?
                .size()
                != 0;
            let workspace_storage = storage
                .list_with_options(ListOptions::new().prefix("workspace:").limit(1))
                .await?
                .size()
                != 0;
            let journal = storage
                .list_with_options(ListOptions::new().prefix("journal:").limit(2))
                .await?;
            let empty_journal = journal.size() == 0
                || (journal.size() == 1
                    && journal.has(&wasm_bindgen::JsValue::from_str("journal:count"))
                    && storage.get::<usize>("journal:count").await? == Some(0));
            let attachment_pins = !crate::space::list_response_pins(
                &self.env,
                &data.record.owner_id,
                &data.record.id,
            )
            .await?
            .is_empty();
            let receipts = RetirementReceipts {
                response,
                response_storage,
                workspace_storage,
                source_journal: !empty_journal,
                attachment_pins,
            };
            // Storage and database reads yield. Compare the entire protected
            // source again before changing its lifecycle, not a captured row.
            if !receipts.empty()
                || self.running.get()
                || self.observer.borrow().is_some()
                || !self.response_observers.borrow().is_empty()
                || !self
                    .data
                    .borrow()
                    .as_ref()
                    .is_some_and(|current| same_empty_source(&data, current))
            {
                return retirement_conflict();
            }
            let mut borrowed = self.data.borrow_mut();
            let current = borrowed.as_mut().unwrap();
            current.record.status = SessionStatus::Closed;
            current.mark_projection_dirty();
        }
        // persist owns the same mutex, so do not call it under the proof guard.
        // Keep the closing gate until canonical state and its D1 acknowledgement
        // are durable. A partial failure still arms ordinary projection repair.
        let persisted = self.persist().await;
        let alarm = self.arm_alarm().await;
        persisted?;
        alarm?;
        let current = self.data.borrow();
        if !current.as_ref().is_some_and(|data| {
            data.record.status == SessionStatus::Closed
                && !data.projection_pending
                && empty_source(data)
        }) {
            return retirement_conflict();
        }
        Response::from_json(&json!({"closed":true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PersistedSession {
        let agent = serde_json::from_value::<agents_core::CreateAgent>(json!({"name":"test"}))
            .unwrap()
            .into_config("agent".into(), "owner".into(), 42);
        let record = SessionRecord {
            id: "session".into(),
            owner_id: "owner".into(),
            agent_id: "agent".into(),
            status: SessionStatus::Idle,
            created_at_ms: 42,
            last_input_at_ms: 42,
            input_revision: 0,
            pinned: agents_core::PinnedContext::resolve(&agent, vec![], vec![]),
            configuration: None,
            context_layout: None,
            workspace_snapshot: None,
            runtime_profile: agents_core::RuntimeProfile::Basic1g,
        };
        let prefix = system_prefix(&record);
        serde_json::from_value(json!({
            "record":record,"messages":[Message::text("system",prefix)],
            "measured_messages":1,
        }))
        .unwrap()
    }

    #[test]
    fn only_the_original_unused_pin_can_be_retired_without_activity() {
        let mut data = state();
        assert!(empty_source(&data));
        let before = data.record.clone();
        data.record.status = SessionStatus::Closed;
        assert!(empty_source(&data));
        assert_eq!(data.record.input_revision, before.input_revision);
        assert_eq!(data.record.last_input_at_ms, before.last_input_at_ms);
        data.record.status = SessionStatus::Running;
        assert!(!empty_source(&data));
    }

    #[test]
    fn external_absence_proofs_are_bound_to_the_same_owner_session_and_pin() {
        let before = state();
        assert!(same_empty_source(&before, &before));
        for field in ["owner", "session", "agent", "created", "pin"] {
            let mut after = before.clone();
            match field {
                "owner" => after.record.owner_id = "other-owner".into(),
                "session" => after.record.id = "other-session".into(),
                "agent" => after.record.agent_id = "other-agent".into(),
                "created" => {
                    after.record.created_at_ms += 1;
                    after.record.last_input_at_ms += 1;
                }
                "pin" => after.record.pinned.instructions = "another prefix".into(),
                _ => unreachable!(),
            }
            after.messages[0].content = json!(system_prefix(&after.record));
            assert!(empty_source(&after), "{field}");
            assert!(!same_empty_source(&before, &after), "{field}");
        }
    }

    #[test]
    fn accepted_input_or_response_identity_is_never_empty() {
        let initial = state();
        for field in [
            "revision",
            "clock",
            "head",
            "journal",
            "journal_count",
            "pending",
            "history",
        ] {
            let mut data = initial.clone();
            match field {
                "revision" => data.record.input_revision = 1,
                "clock" => data.record.last_input_at_ms += 1,
                "head" => data.response_head = Some("resp_one".into()),
                "journal" => data.journal.push(Message::text("user", "accepted")),
                "journal_count" => data.journal_count = 1,
                "pending" => data.pending.push(Message::text("user", "queued")),
                "history" => data
                    .history_instructions
                    .push(Message::text("developer", "retained")),
                _ => unreachable!(),
            }
            assert!(!empty_source(&data), "{field}");
        }
    }

    #[test]
    fn run_client_tools_and_workspace_receipts_prevent_retirement() {
        let initial = state();
        let mut active = initial.clone();
        active.run = Some(
            serde_json::from_value(json!({
                "id":"run_one","round":0,"admitted":false,"total":Usage::default(),
                "pending_completion":null,"started_at_ms":42,
            }))
            .unwrap(),
        );
        assert!(!empty_source(&active));
        let mut waiting = initial.clone();
        waiting.pending_client_tools.push(provider::ToolCall {
            id: "call_one".into(),
            kind: "function".into(),
            function: provider::FunctionCall {
                name: "local".into(),
                arguments: "{}".into(),
            },
        });
        assert!(!empty_source(&waiting));
        let mut snapshot = initial;
        snapshot.record.workspace_snapshot = Some(agents_core::WorkspaceSnapshot {
            generation: 1,
            manifest_key: "private-manifest".into(),
            sha256: "hash".into(),
            format_version: 1,
            created_at_ms: 42,
            logical_bytes: 0,
            archive_bytes: 0,
            entries: 0,
        });
        assert!(!empty_source(&snapshot));
    }

    #[test]
    fn compaction_errors_archive_work_and_modified_prefix_are_not_unused() {
        let initial = state();
        for field in [
            "tokens",
            "compact",
            "error",
            "checkpoint_error",
            "archive_pending",
            "archive_preparing",
            "archive_key",
            "archive_started",
            "archive_generation",
            "measured",
            "prefix",
            "extra_message",
        ] {
            let mut data = initial.clone();
            match field {
                "tokens" => data.input_tokens = 1,
                "compact" => data.compact_count = 1,
                "error" => data.last_error = Some("uncertain".into()),
                "checkpoint_error" => data.checkpoint_error = Some("pending".into()),
                "archive_pending" => data.archive_pending = true,
                "archive_preparing" => data.archive_preparing = true,
                "archive_key" => data.archive_key = Some("stored".into()),
                "archive_started" => data.archive_started_at_ms = Some(42),
                "archive_generation" => data.archive_backstop_generation = Some(1),
                "measured" => data.measured_messages = 2,
                "prefix" => data.messages[0].content = json!("different"),
                "extra_message" => data.messages.push(Message::text("assistant", "answer")),
                _ => unreachable!(),
            }
            assert!(!empty_source(&data), "{field}");
        }
    }

    #[test]
    fn every_uncertain_external_receipt_vetoes_retirement() {
        assert!(RetirementReceipts::default().empty());
        for field in [
            "response",
            "response_storage",
            "workspace_storage",
            "journal",
            "pins",
        ] {
            let mut receipts = RetirementReceipts::default();
            match field {
                "response" => receipts.response = true,
                "response_storage" => receipts.response_storage = true,
                "workspace_storage" => receipts.workspace_storage = true,
                "journal" => receipts.source_journal = true,
                "pins" => receipts.attachment_pins = true,
                _ => unreachable!(),
            }
            assert!(!receipts.empty(), "{field}");
        }
    }

    #[test]
    fn response_identity_is_bounded_before_durable_lookups() {
        assert!(valid_response_id("resp_one-123"));
        for id in ["", "../other", "a/b", "a\0b", "name:part"] {
            assert!(!valid_response_id(id));
        }
        assert!(!valid_response_id(&"x".repeat(129)));
        assert!(
            serde_json::from_value::<RetireEmpty>(
                json!({"response_id":"resp_one","owner":"other"})
            )
            .is_err()
        );
    }
}
