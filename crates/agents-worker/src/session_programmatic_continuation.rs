use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProgramPause {
    pub(in crate::session) operation: String,
    pub(in crate::session) run: String,
    pub(in crate::session) response: String,
    pub(in crate::session) expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(in crate::session) inputs: Vec<Value>,
}

impl SessionInner {
    pub(in crate::session) async fn programmatic_continuation(
        &self,
        data: &PersistedSession,
        parent: Option<&str>,
    ) -> Result<Option<ProgramPause>> {
        let Some(parent) = parent else {
            return Ok(None);
        };
        let Some(response) = self.read_response(parent).await? else {
            return Ok(None);
        };
        let Some(pause) = response.programmatic else {
            return Ok(None);
        };
        if response.status != "completed"
            || response.deleted
            || response.cancel_requested
            || data.record.status == SessionStatus::Closed
            || self.closing.get()
        {
            return Err(fault("program_not_active"));
        }
        if !accepts_run(data, &Some(pause.clone())) {
            return Err(fault("program_live_continuation_required"));
        }
        Ok(Some(pause))
    }

    pub(in crate::session) async fn programmatic_inputs(
        &self,
        pause: &ProgramPause,
        record: &SessionRecord,
        input: &[Value],
    ) -> Result<Vec<Value>> {
        let bytes = read_chunks(&self.state.storage(), &format!("{}:state", pause.operation))
            .await?
            .ok_or_else(|| fault("program_operation_corrupt"))?;
        let mut program: ProgramState = serde_json::from_slice(&bytes)?;
        program
            .verify_scope(&record.owner_id, &record.id, program.fingerprint())
            .map_err(fault)?;
        let outputs: Vec<_> = input
            .iter()
            .filter(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("function_call_output" | "custom_tool_call_output")
                )
            })
            .cloned()
            .collect();
        program.accept_client_outputs(&outputs).map_err(fault)?;
        Ok(outputs)
    }

    pub(in crate::session) async fn pause_programmatic_response(
        &self,
        key: &str,
        program: &ProgramState,
        calls: &[Value],
    ) -> Result<()> {
        {
            let mut data = self.data.borrow_mut();
            let data = data.as_mut().unwrap();
            let run = data.run.as_mut().unwrap();
            run.close_worker_segment(util::now_ms());
            let response = run.response.as_mut().unwrap();
            response.programmatic = Some(ProgramPause {
                operation: key.into(),
                run: run.id.clone(),
                response: response.id.clone(),
                expires_at_ms: program.expires_at_ms(),
                inputs: vec![],
            });
            response.general_pending_calls.extend(
                calls
                    .iter()
                    .filter_map(native_output::GeneralPendingCall::from_item),
            );
            let pending = response.general_pending_calls.clone();
            data.pending_general_tools = pending;
        }
        self.prepare_response_branch().await?;
        if let Some(code) = self.prepare_conversation_commit(false).await? {
            return Err(fault(&code));
        }
        let run = self.snapshot()?.run.unwrap();
        let mut response = run.response.as_ref().unwrap().clone();
        response.status = "completed".into();
        response.usage = response.usage_since_start(&run.total);
        response.completed_at = Some(util::now_ms() / 1000);
        response.event("response.completed", json!({"response":response.value()}))?;
        self.terminal_steering_events(&mut response)?;
        self.seal_branch_point(&mut response).await?;
        {
            let _guard = self.persistence.lock().await;
            self.write_response(&response).await?;
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response = Some(response.clone());
            let mut saved = self.snapshot()?;
            save_state(&self.state.storage(), &mut saved).await?;
            self.release_conversations_locked().await?;
        }
        self.finalize_mcp_response(&response.id).await?;
        self.publish_response(response).await;
        Ok(())
    }

    pub(in crate::session) async fn await_programmatic_results(
        &self,
        key: &str,
        program: &mut ProgramState,
        deadline: u64,
    ) -> Result<()> {
        loop {
            if self.programmatic_cancelled() {
                return Err(fault("program_cancelled"));
            }
            if util::now_ms() >= deadline {
                return Err(fault("program_runtime_expired"));
            }
            let data = self.snapshot()?;
            let response = data
                .run
                .as_ref()
                .and_then(|run| run.response.as_ref())
                .ok_or_else(|| fault("program_cancelled"))?;
            if let Some(pause) = response.programmatic.as_ref().filter(|pause| {
                pause.operation == key && !pause.inputs.is_empty() && response.status != "completed"
            }) {
                self.activate_conversation_run(data.run.as_ref().unwrap())
                    .await?;
                if self.programmatic_cancelled() {
                    return Err(fault("program_cancelled"));
                }
                program
                    .accept_client_outputs(&pause.inputs)
                    .map_err(fault)?;
                write_chunks(
                    &self.state.storage(),
                    &format!("{key}:state"),
                    &serde_json::to_vec(program)?,
                )
                .await?;
                for item in &pause.inputs {
                    self.publish_hosted_item(
                        &program.client_result_item(item).map_err(fault)?,
                        true,
                    )
                    .await?;
                }
                let policy = ProgramPolicy::resolve_bound(
                    &response
                        .request
                        .tools
                        .iter()
                        .map(|tool| tool.to_wire_value())
                        .collect::<Vec<_>>(),
                    data.record
                        .pinned
                        .tools
                        .iter()
                        .any(|tool| tool.name == "time"),
                    &response.mcp_bindings,
                    response.discovery.as_ref(),
                )
                .map_err(fault)?;
                program.restrict_new_calls(&policy);
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
                    .programmatic
                    .as_mut()
                    .unwrap()
                    .inputs
                    .clear();
                self.persist().await?;
                return Ok(());
            }
            worker::Delay::from(std::time::Duration::from_millis(200)).await;
        }
    }
}

pub(in crate::session) fn accepts_run(
    data: &PersistedSession,
    expected: &Option<ProgramPause>,
) -> bool {
    match expected {
        None => data.run.is_none(),
        Some(expected) => data.run.as_ref().is_some_and(|run| {
            run.id == expected.run
                && run.ended_at_ms.is_none()
                && run.response.as_ref().is_some_and(|response| {
                    response.id == expected.response
                        && response.status == "completed"
                        && !response.cancel_requested
                        && !response.deleted
                        && response
                            .programmatic
                            .as_ref()
                            .is_some_and(|pause| pause.operation == expected.operation)
                })
        }),
    }
}

pub(in crate::session) fn accepts_status(
    data: &PersistedSession,
    expected: &Option<ProgramPause>,
) -> bool {
    data.record.status
        == if expected.is_some() {
            SessionStatus::Running
        } else {
            SessionStatus::Idle
        }
}
