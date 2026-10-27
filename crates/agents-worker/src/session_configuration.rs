//! Agent edits are pulled at safe boundaries, never while a provider outcome is pending.

use super::*;

fn boundary(run: &RunState) -> bool {
    !run.provider_in_flight
        && !run.compaction_in_flight
        && run.pending_completion.is_none()
        && run.pending_compaction.is_none()
        && run.terminal.is_none()
        && run.ended_at_ms.is_none()
}

impl SessionInner {
    async fn issued_configuration_effect(&self, data: &PersistedSession) -> Result<bool> {
        let Some(run) = &data.run else {
            return Ok(false);
        };
        let Some(call) = next_tool(&data.messages, run.response.as_ref()) else {
            return Ok(false);
        };
        if !matches!(
            tools::tool_kind(&call.function.name),
            Some(tools::ToolKind::Workspace | tools::ToolKind::Persistence)
        ) {
            return Ok(false);
        }
        self.workspace(&data.record)
            .operation_issued(
                &call.function.name,
                &execution_key(&run.id, run.round, &call.id),
            )
            .await
            .map_err(|error| {
                let normalized = tool_infrastructure_error(&error);
                util::emit(
                    "tool.failed",
                    json!({
                        "session_id":data.record.id,"run_id":run.id,"tool_call_id":call.id,
                        "tool_name":call.function.name,"stage":"issued_recovery",
                        "error_code":safe_error_code(&normalized),"outcome":error_family(&error),
                    }),
                );
                normalized
            })
    }

    pub(super) async fn refresh_configuration(&self) -> Result<()> {
        loop {
            let before = self.snapshot()?;
            if before.run.as_ref().is_some_and(|run| !boundary(run)) {
                return Ok(());
            }
            if self.issued_configuration_effect(&before).await? {
                return Ok(());
            }
            let cursor = before.record.configuration_revision();
            let meter = before
                .run
                .as_ref()
                .filter(|run| run.admitted)
                .map(|run| {
                    crate::resources::ResourceMeter::new(
                        &self.env,
                        &before.record.owner_id,
                        &before.record.id,
                        &run.id,
                    )
                })
                .transpose()?;
            let event = util::id("configuration_read");
            let changes = Store::new(&self.env)
                .configuration_revisions(
                    &before.record.owner_id,
                    &before.record.agent_id,
                    cursor,
                    meter.as_ref().map(|meter| (meter, event.as_str())),
                )
                .await?;
            if changes.is_empty() {
                return Ok(());
            }
            let count = changes.len();
            let guard = self.persistence.lock().await;
            let mut data = self.snapshot()?;
            if data.run.as_ref().is_some_and(|run| !boundary(run)) {
                return Ok(());
            }
            if self.issued_configuration_effect(&data).await? {
                return Ok(());
            }
            if data.record.configuration_revision() != cursor {
                drop(guard);
                continue;
            }
            for change in changes {
                if change.agent_revision <= data.record.configuration_revision() {
                    return Err(Error::RustError("configuration_revision_invalid".into()));
                }
                let change = crate::configuration::for_session(&data.record.pinned, change);
                if data.record.pinned.model.protocol
                    == Some(agents_core::ProviderProtocol::Responses)
                {
                    data.pending_configurations.push(change.clone());
                }
                data.record.configuration = Some(change);
            }
            if data.record.pinned.model.protocol != Some(agents_core::ProviderProtocol::Responses) {
                let instructions = data
                    .run
                    .as_ref()
                    .and_then(|run| run.response.as_ref())
                    .and_then(|response| {
                        response
                            .effective_instructions
                            .as_deref()
                            .or(response.request.instructions.as_deref())
                    });
                data.messages[0] = Message::text(
                    "system",
                    crate::configuration::legacy_prefix(
                        &data.record,
                        &data.history_instructions,
                        instructions,
                    ),
                );
            }
            save_state(&self.state.storage(), &mut data).await?;
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .record
                .configuration = data.record.configuration;
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .pending_configurations = data.pending_configurations;
            if before.record.pinned.model.protocol != Some(agents_core::ProviderProtocol::Responses)
            {
                self.data.borrow_mut().as_mut().unwrap().messages = data.messages;
            }
            drop(guard);
            util::emit(
                "session.configuration_applied",
                json!({"session_id":before.record.id,
                "agent_id":before.record.agent_id,"config_revision":self.snapshot()?.record.configuration_revision(),
                "count":count}),
            );
            if count < 128 {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_configuration_issued_preflight_preserves_unknown_error_identity() {
        let error = Error::RustError("tool_outcome_unknown: an issued Bash operation has no matching live workspace; no command or restore was replayed".into());
        let normalized = tool_infrastructure_error(&error);
        assert_eq!(safe_error_code(&normalized), "tool_outcome_unknown");
        assert!(correctable_tool_error(&normalized).is_none());
        assert_eq!(error_family(&error), "runtime_error");
    }

    #[test]
    fn session_configuration_boundary_never_applies_over_unresolved_or_terminal_work() {
        let mut run: RunState =
            serde_json::from_value(json!({"id":"run","round":0,"admitted":true,
            "total":Usage::default(),"pending_completion":null,"started_at_ms":0}))
            .unwrap();
        assert!(boundary(&run));
        run.provider_in_flight = true;
        assert!(!boundary(&run));
        run.provider_in_flight = false;
        run.compaction_in_flight = true;
        assert!(!boundary(&run));
        run.compaction_in_flight = false;
        run.terminal = Some(json!({"done":true}));
        assert!(!boundary(&run));
        run.terminal = None;
        run.ended_at_ms = Some(1);
        assert!(!boundary(&run));
    }
}
