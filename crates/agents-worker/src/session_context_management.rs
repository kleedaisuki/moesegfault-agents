//! Request policy adapts the existing compaction state machine and native log.

use super::*;
use crate::context_management::{InputSize, Policy};

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
    if policy.emits_item()
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
        let mut protected = vec![Message::text(
            "system",
            crate::context_prefix::prefix(&data.record.pinned),
        )];
        protected.extend(data.history_instructions.iter().cloned());
        protected.push(Message::text(
            "user",
            format!("Earlier session context (summary):\n{text}"),
        ));
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
