//! Request policy adapts the existing compaction state machine and native log.

use super::*;
use crate::context_management::{InputSize, Policy};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ActiveTurn {
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<Vec<Value>>,
}

impl ActiveTurn {
    // Native input must already exclude storage-owned configuration/runtime
    // positions. Their user role is transport, not a new user exchange.
    pub(super) fn capture(messages: &[Message], native: Option<&[Value]>) -> Self {
        // This boundary has drained issued calls. Keep the current request
        // exact, but let the summary carry resolved work so long tasks shrink.
        let messages = messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .cloned()
            .into_iter()
            .collect();
        let native = native.map(|items| {
            items
                .iter()
                .rev()
                .find(|item| item["role"] == "user")
                .cloned()
                .into_iter()
                .collect()
        });
        Self { messages, native }
    }
}

pub(super) fn policy(data: &PersistedSession) -> Result<Policy> {
    let model = &data.record.pinned.model;
    let threshold = data
        .record
        .pinned
        .compaction_percent
        .threshold_tokens(model.context_window)
        .min(model.context_window.saturating_sub(model.max_output_tokens));
    let requested = data
        .run
        .as_ref()
        .and_then(|run| run.response.as_ref())
        .and_then(|response| response.request.context_management.as_ref());
    Policy::resolve(requested, u64::from(threshold))
        .map_err(|_| Error::RustError("invalid_context_management".into()))
}

pub(super) fn triggered(data: &PersistedSession, measured: Option<u64>) -> Result<bool> {
    let policy = policy(data)?;
    // Retaining the live turn can leave the input above the threshold. Only
    // real journal progress makes another paid summary useful, in every mode.
    if data
        .run
        .as_ref()
        .is_some_and(|run| run.compaction_source_end == Some(data.journal.len()))
        || policy.emits_item()
            && data
                .run
                .as_ref()
                .and_then(|run| run.response.as_ref())
                .is_some_and(|response| response.compaction_source_end == Some(data.journal.len()))
    {
        return Ok(false);
    }
    let size = match measured {
        Some(tokens) => InputSize::Exact(tokens),
        None => {
            let added = budget::working_tokens(
                &data.messages[data.measured_messages.min(data.messages.len())..],
            )?;
            let configuration_messages = data
                .pending_configurations
                .iter()
                .map(crate::configuration::message)
                .collect::<Vec<_>>();
            let configuration_tokens = budget::working_tokens(&configuration_messages)?;
            InputSize::WorkingEstimate(
                (if data.input_tokens == 0 {
                    budget::working_tokens(&data.messages)?
                } else {
                    data.input_tokens.saturating_add(added)
                })
                .saturating_add(configuration_tokens),
            )
        }
    };
    policy
        .should_compact(size)
        .map_err(|_| Error::RustError("context_management_exact_count_unavailable".into()))
}

impl SessionInner {
    pub(super) async fn active_compaction_turn(
        &self,
        data: &PersistedSession,
    ) -> Result<ActiveTurn> {
        let native = if let Some(context) = &data.native_context {
            let items = self.native_export(&context.history).await?;
            let updates = super::native::context_positions(&context.history)?;
            Some(
                items
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, item)| {
                        updates.binary_search(&(index + 1)).is_err().then_some(item)
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        Ok(ActiveTurn::capture(&data.messages[1..], native.as_deref()))
    }
    pub(super) async fn exact_compaction_tokens(
        &self,
        config: &ProviderConfig,
        options: &provider::GenerationOptions,
    ) -> Result<u64> {
        let prepared = options
            .native
            .as_ref()
            .ok_or_else(|| Error::RustError("context_management_exact_count_unavailable".into()))?;
        crate::responses_auxiliary::count_rendered(config, prepared)
            .await
            .map(|count| count.input_tokens)
            .map_err(|_| Error::RustError("context_management_exact_count_unavailable".into()))
    }

    pub(super) fn compaction_item(
        &self,
        data: &PersistedSession,
        summary: &Completion,
        active: &ActiveTurn,
    ) -> Result<Option<Value>> {
        if !policy(data)?.emits_item() {
            return Ok(None);
        }
        let run = data
            .run
            .as_ref()
            .ok_or_else(|| Error::RustError("run_not_found".into()))?;
        let response = run
            .response
            .as_ref()
            .ok_or_else(|| Error::RustError("run_not_found".into()))?;
        let text = summary
            .message
            .content
            .as_str()
            .filter(|text| !text.is_empty())
            .ok_or_else(|| Error::RustError("empty_compaction".into()))?;
        let mut protected = vec![Message::text("system", system_prefix(&data.record))];
        protected.extend(data.history_instructions.iter().cloned());
        protected.push(Message::text(
            "user",
            format!("Earlier session context (summary):\n{text}"),
        ));
        let native = active
            .native
            .as_ref()
            .map(|tail| {
                let mut items =
                    provider::responses::message_items(&protected).map_err(Error::RustError)?;
                items.extend_from_slice(tail);
                Ok::<_, Error>(items)
            })
            .transpose()?;
        protected.extend(active.messages.iter().cloned());
        crate::responses_auxiliary::CompactionState::new(
            data.record.agent_id.clone(),
            data.record.pinned.prefix_hash(),
            response
                .effective_instructions
                .clone()
                .or_else(|| response.request.instructions.clone()),
            protected,
        )?
        .with_configuration(data.record.configuration.clone())
        .with_native(native)
        .seal(
            &self.env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
            &data.record.owner_id,
            &format!("item_{}_{}_0", run.id, u64::MAX - data.compact_count),
        )
        .map(Some)
    }

    pub(super) fn append_compaction_item(&self, item: Value) -> Result<()> {
        let (index, change) = {
            let mut borrowed = self.data.borrow_mut();
            let response = borrowed
                .as_mut()
                .and_then(|data| data.run.as_mut())
                .and_then(|run| run.response.as_mut())
                .ok_or_else(|| Error::RustError("run_not_found".into()))?;
            (
                response.output.len(),
                response.prepare_output(None, item.clone())?,
            )
        };
        self.response_event(
            "response.output_item.added",
            json!({"output_index":index,"item":item}),
        )?;
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_compaction_retains_current_turn_and_exact_native_facts() {
        let messages = vec![
            Message::text("user", "Earlier task"),
            Message::text("assistant", "Earlier answer"),
            Message::text("user", "Current request: retain nonce-28"),
            Message::text("assistant", "Working on current request"),
            Message::tool_result("call_current", "verified current result"),
        ];
        let current = json!({"type":"message","role":"user","content":[
            {"type":"input_text","text":"Current request: retain nonce-28"},
            {"type":"input_image","image_url":"https://example.org/current.png"}
        ]});
        let native = vec![
            json!({"type":"message","role":"user","content":"Earlier task"}),
            current.clone(),
            json!({"type":"reasoning","encrypted_content":"opaque-current-fact"}),
            json!({"type":"function_call","call_id":"call_current","name":"__agents_call_tool","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"call_current","output":"verified current result"}),
        ];
        let active = ActiveTurn::capture(&messages, Some(&native));
        assert_eq!(active.messages.len(), 1);
        assert_eq!(active.messages[0].content, messages[2].content);
        assert_eq!(
            active.native.as_deref(),
            Some(std::slice::from_ref(&current))
        );
        let mut long_task = native.clone();
        for index in 0..512 {
            long_task.push(
                json!({"type":"function_call","call_id":format!("call_{index}"),
                "name":"__agents_call_tool","arguments":"{}"}),
            );
            long_task.push(
                json!({"type":"function_call_output","call_id":format!("call_{index}"),
                "output":"previous resolved evidence"}),
            );
        }
        assert_eq!(
            ActiveTurn::capture(&messages, Some(&long_task)).native,
            active.native
        );
        let recovered: ActiveTurn =
            serde_json::from_slice(&serde_json::to_vec(&active).unwrap()).unwrap();
        assert_eq!(recovered.native, active.native);

        let protected = vec![
            Message::text("system", "platform"),
            Message::text("user", "Earlier session context (summary): old facts only"),
        ];
        let mut sealed_native = provider::responses::message_items(&protected).unwrap();
        sealed_native.extend_from_slice(recovered.native.as_deref().unwrap());
        let mut projected = protected;
        projected.extend(recovered.messages);
        let hash = "a".repeat(64);
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let item = crate::responses_auxiliary::CompactionState::new(
            "agent".into(),
            hash.clone(),
            None,
            projected,
        )
        .unwrap()
        .with_native(Some(sealed_native.clone()))
        .seal(key, "owner", "cmp_current")
        .unwrap();
        let expanded = crate::responses_auxiliary::expand_for_prefix(
            key,
            "owner",
            "agent",
            &hash,
            &[item],
            "current platform",
        )
        .unwrap();
        assert_eq!(expanded.items, sealed_native[1..]);
        assert_eq!(expanded.items[1], current);
        assert_eq!(expanded.items.len(), 2);
        assert!(
            !expanded
                .items
                .iter()
                .any(|item| item.get("call_id").is_some())
        );
        assert!(ActiveTurn::capture(&[], None).messages.is_empty());
        assert!(
            ActiveTurn::capture(&[Message::text("assistant", "no user")], Some(&[]))
                .native
                .unwrap()
                .is_empty()
        );

        for explicit in [false, true] {
            let mut data = explicit_state();
            if !explicit {
                data.run.as_mut().unwrap().response = None;
            }
            assert!(triggered(&data, Some(u64::MAX)).unwrap());
            data.run.as_mut().unwrap().compaction_source_end = Some(data.journal.len());
            let recovered: PersistedSession =
                serde_json::from_slice(&serde_json::to_vec(&data).unwrap()).unwrap();
            assert!(!triggered(&recovered, Some(u64::MAX)).unwrap());
            assert!(!triggered(&recovered, None).unwrap());
            data.journal
                .push(Message::tool_result("call_new", "New evidence"));
            assert!(triggered(&data, Some(u64::MAX)).unwrap());
        }
    }

    fn explicit_state() -> PersistedSession {
        let mut data = super::super::tests::state();
        data.messages.push(Message::text("user", "Original source"));
        data.journal.push(Message::text("user", "Original source"));
        data.run = Some(
            serde_json::from_value(json!({"id":"resp_policy","round":0,"admitted":true,
            "total":Usage::default(),"pending_completion":null,"started_at_ms":0,
            "response":{"id":"resp_policy","request":{"model":"agent","input":"Original source",
                "context_management":[{"type":"compaction","compact_threshold":42}]},
                "created_at":1,"input_items":[],"output":[],"events":[],"status":"in_progress",
                "error":null,"incomplete_reason":null,"cancel_requested":false,"awaiting_calls":[],
                "usage":Usage::default(),"next_sequence":0}}))
            .unwrap(),
        );
        data
    }

    #[test]
    fn exact_threshold_and_source_boundary_prevent_repeated_paid_compaction() {
        let mut data = explicit_state();
        assert!(triggered(&data, None).is_err());
        assert!(!triggered(&data, Some(41)).unwrap());
        assert!(triggered(&data, Some(42)).unwrap());
        data.run
            .as_mut()
            .unwrap()
            .response
            .as_mut()
            .unwrap()
            .compaction_source_end = Some(data.journal.len());
        assert!(!triggered(&data, Some(1000)).unwrap());
        data.journal.push(Message::text("tool", "New evidence"));
        assert!(triggered(&data, Some(1000)).unwrap());
    }
}
