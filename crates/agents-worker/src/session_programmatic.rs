//! One accepted program is a subroutine of the Session actor, not an agent loop.
//! The first enabled vertical executes only explicitly granted pinned time.

use super::*;
use crate::{
    programmatic_metering::{self as funding, TurnAdmission},
    programmatic_runtime::{self as runtime, NativeReceipt, OpenProgram, RuntimeSpec},
    programmatic_tools::{
        AuthorizedInvocation, ExecutionTarget, PROGRAM_ALIAS, ProgramPolicy, ProgramScope,
        ProgramState,
    },
    responses_wire::ResponseRequest,
    session_storage::{read_chunks, write_chunks},
};
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    code: String,
}

#[derive(Serialize, Deserialize)]
struct Operation {
    arguments_hash: String,
    runtime: RuntimeSpec,
    admission: TurnAdmission,
    native_receipt: Option<NativeReceipt>,
    native_valid: bool,
    capacity_receipt: Option<Value>,
    metering_gap: Option<Value>,
}

pub(super) fn prepare_programmatic_policy(
    env: &Env,
    request: &ResponseRequest,
    record: &SessionRecord,
) -> Result<Option<ProgramPolicy>> {
    let declarations = request
        .tools
        .iter()
        .map(|tool| tool.to_wire_value())
        .collect::<Vec<_>>();
    if !ProgramPolicy::requested(&declarations).map_err(fault)? {
        return Ok(None);
    }
    if !runtime::available(env) {
        return Err(fault("program_runtime_unavailable"));
    }
    funding::validate_configuration(env)?;
    let policy = ProgramPolicy::resolve(
        &declarations,
        record.pinned.tools.iter().any(|tool| tool.name == "time"),
    )
    .map_err(fault)?;
    Ok(Some(policy))
}

impl SessionInner {
    pub(super) async fn dispatch_programmatic(
        &self,
        record: &SessionRecord,
        execution: &str,
        call: &provider::ToolCall,
    ) -> Result<Option<super::hosted_dispatch::HostedDispatch>> {
        if call.function.name != PROGRAM_ALIAS {
            return Ok(None);
        }
        let arguments: Arguments = serde_json::from_str(&call.function.arguments)
            .map_err(|_| fault("invalid_programmatic_arguments"))?;
        let key = format!(
            "programmatic:{:x}",
            Sha256::digest(serde_json::to_vec(&(
                &record.owner_id,
                &record.id,
                execution
            ))?)
        );
        let state_key = format!("{key}:state");
        let storage = self.state.storage();
        let existing = read_chunks(&storage, &key).await?;
        let argument_hash = format!("{:x}", Sha256::digest(call.function.arguments.as_bytes()));
        let (mut operation, mut program) = if let Some(bytes) = existing {
            let operation: Operation =
                serde_json::from_slice(&bytes).map_err(|_| fault("program_operation_corrupt"))?;
            if operation.arguments_hash != argument_hash {
                return Err(fault("program_operation_conflict"));
            }
            let bytes = read_chunks(&storage, &state_key)
                .await?
                .ok_or_else(|| fault("program_operation_corrupt"))?;
            let program: ProgramState =
                serde_json::from_slice(&bytes).map_err(|_| fault("program_operation_corrupt"))?;
            program
                .verify_scope(&record.owner_id, &record.id, program.fingerprint())
                .map_err(fault)?;
            (operation, program)
        } else {
            let (request, response_id, mcp, registry) = {
                let borrowed = self.data.borrow();
                let response = borrowed
                    .as_ref()
                    .and_then(|data| data.run.as_ref())
                    .and_then(|run| run.response.as_ref())
                    .ok_or_else(|| fault("program_response_missing"))?;
                (
                    response.request.clone(),
                    response.id.clone(),
                    response.mcp_bindings.clone(),
                    response.discovery.clone(),
                )
            };
            prepare_programmatic_policy(&self.env, &request, record)?
                .ok_or_else(|| fault("program_not_admitted"))?;
            let policy = ProgramPolicy::resolve_bound(
                &request
                    .tools
                    .iter()
                    .map(|tool| tool.to_wire_value())
                    .collect::<Vec<_>>(),
                record.pinned.tools.iter().any(|tool| tool.name == "time"),
                &mcp,
                registry.as_ref(),
            )
            .map_err(fault)?;
            let identity = format!("{:x}", Sha256::digest(execution.as_bytes()));
            let program = ProgramState::new(
                ProgramScope {
                    owner: record.owner_id.clone(),
                    session: record.id.clone(),
                    response: response_id,
                },
                &identity,
                arguments.code,
                policy,
                util::now_ms(),
                300_000,
            )
            .map_err(fault)?;
            let runtime = RuntimeSpec::new(&program, 100)?;
            let admission = TurnAdmission {
                owner: record.owner_id.clone(),
                session: record.id.clone(),
                worker_id: runtime.worker_id().into(),
                code_revision: runtime.code_revision().into(),
                attempt_id: format!("origin_{identity}"),
                admitted_at_ms: util::now_ms(),
                cpu_limit_ms: 100,
            };
            let operation = Operation {
                arguments_hash: argument_hash,
                runtime,
                admission,
                native_receipt: None,
                native_valid: false,
                capacity_receipt: None,
                metering_gap: None,
            };
            write_chunks(&storage, &state_key, &serde_json::to_vec(&program)?).await?;
            save(&storage, &key, &operation).await?;
            (operation, program)
        };
        if program.output_item().is_some() {
            return self
                .finish_programmatic(&key, &operation, &program)
                .await
                .map(Some);
        }
        // No live channel survived this function's prior invocation. An ended
        // origin context cannot be reconstructed from persisted JavaScript data.
        if !program.ready() {
            program.runtime_lost();
            write_chunks(&storage, &state_key, &serde_json::to_vec(&program)?).await?;
            return self
                .finish_programmatic(&key, &operation, &program)
                .await
                .map(Some);
        }
        self.publish_hosted_item(&program.program_item(), true)
            .await?;
        if self.programmatic_cancelled() {
            program.cancel();
            write_chunks(&storage, &state_key, &serde_json::to_vec(&program)?).await?;
            return self
                .finish_programmatic(&key, &operation, &program)
                .await
                .map(Some);
        }
        let funded = funding::prepare_turn(&self.env, &operation.admission).await?;
        if self.programmatic_cancelled() {
            program.cancel();
            write_chunks(&storage, &state_key, &serde_json::to_vec(&program)?).await?;
            funding::release_partial_known_not_dispatched(&self.env, &operation.admission).await?;
            return self
                .finish_programmatic(&key, &operation, &program)
                .await
                .map(Some);
        }
        let start = program
            .commit_start(&storage, &state_key, util::now_ms())
            .await?;
        let deadline = util::now_ms().saturating_add(300_000);
        let opened =
            runtime::open_funded(&self.env, &operation.runtime, start, &funded, deadline).await;
        let mut channel = match opened {
            Ok(OpenProgram::Streaming(channel)) => channel,
            Ok(OpenProgram::Rejected { receipt, code }) => {
                operation.native_receipt = Some(receipt.clone());
                save(&storage, &key, &operation).await?;
                let gap = funding::record_known_request(
                    &self.env,
                    &funded,
                    receipt.started_at_ms,
                    receipt.response_at_ms,
                )
                .await?;
                operation.metering_gap = Some(serde_json::to_value(gap)?);
                program.fail(code);
                write_chunks(&storage, &state_key, &serde_json::to_vec(&program)?).await?;
                return self
                    .finish_programmatic(&key, &operation, &program)
                    .await
                    .map(Some);
            }
            Err(_) => {
                program.runtime_lost();
                operation.metering_gap =
                    Some(json!({"code":"programmatic_runtime_outcome_unknown"}));
                write_chunks(&storage, &state_key, &serde_json::to_vec(&program)?).await?;
                return self
                    .finish_programmatic(&key, &operation, &program)
                    .await
                    .map(Some);
            }
        };
        operation.native_receipt = Some(channel.receipt.clone());
        save(&storage, &key, &operation).await?;
        for _ in 0..514 {
            let reply = match self
                .next_programmatic_reply(&mut channel.output, deadline)
                .await
            {
                Ok(Some(reply)) => reply,
                Err(Error::RustError(code)) if code == "program_cancelled" => {
                    program.cancel();
                    break;
                }
                _ => {
                    program.runtime_lost();
                    break;
                }
            };
            // Retain trustworthy native execution evidence before settling fees.
            operation.native_valid = true;
            if operation.capacity_receipt.is_none() {
                operation.metering_gap =
                    Some(json!({"code":"programmatic_capacity_settlement_pending"}));
            }
            save(&storage, &key, &operation).await?;
            if operation.capacity_receipt.is_none() {
                let receipt = operation.native_receipt.as_ref().unwrap();
                let gap = funding::record_known_execution(
                    &self.env,
                    &funded,
                    receipt.started_at_ms,
                    receipt.response_at_ms,
                )
                .await?;
                let proof = channel
                    .output
                    .enforced_budget()
                    .ok_or_else(|| fault("program_budget_proof_missing"))?;
                let capacity =
                    funding::record_capacity_allocation(&self.env, &funded, &proof).await?;
                operation.capacity_receipt = Some(serde_json::to_value(capacity)?);
                operation.metering_gap = gap
                    .creation_day_pending
                    .then(|| json!({"code":"programmatic_creation_day_pending"}));
                save(&storage, &key, &operation).await?;
            }
            if reply.status == "failed" {
                program.fail(reply.safe_error());
                break;
            }
            if reply.status == "completed" {
                if !reply.media.is_empty() {
                    program.fail("program_image_publication_unavailable");
                    break;
                }
                program
                    .finish(reply.result.unwrap_or_default())
                    .map_err(fault)?;
                break;
            }
            if let Err(code) = program.admit_calls(&reply.calls, util::now_ms()) {
                program.fail(code);
                break;
            }
            write_chunks(&storage, &state_key, &serde_json::to_vec(&program)?).await?;
            for call in &reply.calls {
                if program.known_result(call.sequence).is_some() {
                    continue;
                }
                if self.programmatic_cancelled() {
                    program.cancel();
                    break;
                }
                let proof = program
                    .commit_dispatch(&storage, &state_key, call.sequence, util::now_ms())
                    .await?;
                proof
                    .verify_scope(&record.owner_id, &record.id, util::now_ms())
                    .map_err(fault)?;
                if self.programmatic_cancelled() {
                    program.cancel();
                    break;
                }
                let output = match self.dispatch_program_call(record, &proof).await {
                    Ok(output) => output,
                    Err(Error::RustError(code)) if code == "mcp_approval_required" => {
                        program.fail("program_tool_approval_required");
                        break;
                    }
                    Err(Error::RustError(code)) if code == "program_tool_allowance_exhausted" => {
                        program.fail("program_tool_allowance_exhausted");
                        break;
                    }
                    Err(Error::RustError(code)) if code == "program_cancelled" => {
                        program.cancel();
                        break;
                    }
                    Err(_) => {
                        program.fail("program_tool_dispatch_failed");
                        break;
                    }
                };
                program
                    .complete_call(call.sequence, output)
                    .map_err(fault)?;
                write_chunks(&storage, &state_key, &serde_json::to_vec(&program)?).await?;
            }
            if program.output_item().is_some() {
                break;
            }
            let frame = program.runtime_resume();
            if channel.input.send(&frame, deadline).await.is_err() {
                program.runtime_lost();
                break;
            }
        }
        channel.input.abort();
        if program.output_item().is_none() {
            program.fail("program_frame_limit");
        }
        write_chunks(&storage, &state_key, &serde_json::to_vec(&program)?).await?;
        self.finish_programmatic(&key, &operation, &program)
            .await
            .map(Some)
    }

    async fn dispatch_program_call(
        &self,
        record: &SessionRecord,
        proof: &AuthorizedInvocation,
    ) -> Result<Value> {
        proof
            .verify_scope(&record.owner_id, &record.id, util::now_ms())
            .map_err(fault)?;
        let item = proof.call_item().map_err(fault)?;
        {
            let data = self.data.borrow();
            let response = data
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .and_then(|run| run.response.as_ref())
                .ok_or_else(|| fault("program_response_missing"))?;
            if let Some(registry) = &response.discovery {
                let choice = response
                    .request
                    .tool_choice
                    .as_ref()
                    .filter(|choice| choice["type"] == "allowed_tools");
                registry
                    .validate_call_as(&item, choice, crate::hosted_bindings::Caller::Programmatic)
                    .map_err(Error::RustError)?;
            }
        }
        let name = match proof.target() {
            ExecutionTarget::HarnessTime
                if record.pinned.tools.iter().any(|tool| tool.name == "time") =>
            {
                "time"
            }
            ExecutionTarget::Adapter { capability } => capability,
            _ => return Err(fault("programmatic_tool_not_allowed")),
        };
        if !self
            .admit_tool_dispatch_as(
                record,
                proof.execution_key(),
                name,
                crate::hosted_bindings::Caller::Programmatic,
            )
            .await?
        {
            return Err(fault("program_tool_allowance_exhausted"));
        }
        self.publish_hosted_item(&item, true).await?;
        if self.programmatic_cancelled() {
            return Err(fault("program_cancelled"));
        }
        proof
            .verify_scope(&record.owner_id, &record.id, util::now_ms())
            .map_err(fault)?;
        let output = match proof.target() {
            ExecutionTarget::HarnessTime => {
                let text = crate::tools::dispatch_harness(
                    "time",
                    &serde_json::to_string(proof.arguments())?,
                )
                .map_err(Error::RustError)?
                .ok_or_else(|| fault("programmatic_tool_not_allowed"))?;
                serde_json::from_str(&text)?
            }
            ExecutionTarget::Adapter { capability } => {
                self.dispatch_programmatic_mcp(record, proof, capability)
                    .await?
            }
            _ => return Err(fault("programmatic_tool_not_allowed")),
        };
        self.publish_hosted_item(&proof.result_item(&output).map_err(fault)?, true)
            .await?;
        Ok(output)
    }

    fn programmatic_cancelled(&self) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .is_none_or(|response| {
                response.cancel_requested || response.incomplete_reason.is_some()
            })
    }

    async fn next_programmatic_reply(
        &self,
        output: &mut runtime::ProgramOutput,
        deadline: u64,
    ) -> Result<Option<runtime::RuntimeReply>> {
        let mut pending = Box::pin(output.next(deadline));
        loop {
            if self.programmatic_cancelled() {
                return Err(fault("program_cancelled"));
            }
            match futures::future::select(
                pending,
                Box::pin(worker::Delay::from(std::time::Duration::from_millis(200))),
            )
            .await
            {
                futures::future::Either::Left((reply, _)) => return reply,
                futures::future::Either::Right((_, waiting)) => pending = waiting,
            }
        }
    }

    async fn finish_programmatic(
        &self,
        key: &str,
        operation: &Operation,
        program: &ProgramState,
    ) -> Result<super::hosted_dispatch::HostedDispatch> {
        save(&self.state.storage(), key, operation).await?;
        self.publish_hosted_item(&program.program_item(), true)
            .await?;
        let output = program
            .output_item()
            .ok_or_else(|| fault("program_output_missing"))?;
        self.publish_hosted_item(&output, true).await?;
        Ok(super::hosted_dispatch::HostedDispatch::Ready(
            json!({"program_call_id":program.call_id(),"status":output["status"],"result":output["result"],"billing_pending":operation.metering_gap.is_some() || (operation.native_receipt.is_some() && operation.capacity_receipt.is_none())}).to_string().into(),
        ))
    }
}

async fn save(storage: &worker::Storage, key: &str, operation: &Operation) -> Result<()> {
    write_chunks(storage, key, &serde_json::to_vec(operation)?).await
}
fn fault(code: &str) -> Error {
    Error::RustError(code.into())
}
