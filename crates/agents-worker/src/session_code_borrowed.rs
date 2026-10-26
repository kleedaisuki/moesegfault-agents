//! Resource-only RPCs never initialize or resume the source model harness.
use super::*;
use crate::{
    code_borrowing::{Cell as BorrowedCell, Ticket},
    code_contexts::OwnedContext,
    resources::ResourceMeter,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Funding {
    run_id: String,
    session_id: Option<String>,
    #[serde(rename = "event")]
    _event: String,
}

impl Funding {
    fn require(&self, ticket: &Ticket) -> Result<()> {
        if !ticket.matches_funding(self.session_id.as_deref(), &self.run_id) {
            return Err(fail("code_resource_funding_mismatch"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Stage {
    #[default]
    Prepared,
    ResultKnown,
    Published,
}

#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    owner: String,
    context: OwnedContext,
    ticket: Ticket,
    fingerprint: String,
    stage: Stage,
    #[serde(default)]
    artifacts: Vec<crate::workspace::PythonArtifact>,
    #[serde(default)]
    acknowledged: std::collections::BTreeSet<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    context: OwnedContext,
    ticket: Ticket,
    #[serde(default)]
    artifact_id: Option<String>,
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
fn key(id: &str) -> String {
    format!("code-borrowed:{id}")
}

impl SessionInner {
    pub(super) async fn borrowed_code_route(&self, request: Request) -> Result<Response> {
        let record = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(|| fail("session_not_initialized"))?
            .record
            .clone();
        if request.headers().get("X-Agents-Code-Owner")?.as_deref()
            != Some(record.owner_id.as_str())
        {
            return util::json_error(404, "not_found", "Code container not found");
        }
        let headers = request.headers().clone();
        let traceparent = headers.get("traceparent")?;
        let trace = traceparent
            .as_deref()
            .and_then(crate::telemetry::TraceContext::parse);
        let _span = crate::telemetry::NativeSpan::start("code.resource.rpc", trace.as_ref());
        // Reuse the established internal actor grant. The duplicate decode only
        // extracts its guest identity; actor() independently verifies the ledger.
        crate::conversation_meter::ConversationOperation::actor_scope(&headers)?;
        let funding: Funding = serde_json::from_str(
            &headers
                .get(crate::conversation_meter::HEADER)?
                .ok_or_else(|| fail("conversation_meter_grant_required"))?,
        )?;
        let result = crate::conversation_meter::actor(
            &self.env,
            &record.owner_id,
            &headers,
            self.borrowed_code_dispatch(&record, request, &funding, traceparent),
        )
        .await;
        match result {
            Err(Error::RustError(code))
                if matches!(
                    code.as_str(),
                    "code_cancelled_before_dispatch"
                        | "code_execution_lost"
                        | "code_artifacts_lost"
                        | "code_resource_execution_already_published"
                ) =>
            {
                util::json_error(
                    409,
                    &code,
                    "The original execution cannot continue and will not be replayed",
                )
            }
            result => result,
        }
    }

    async fn borrowed_code_dispatch(
        &self,
        record: &SessionRecord,
        mut request: Request,
        funding: &Funding,
        traceparent: Option<String>,
    ) -> Result<Response> {
        match request.path().as_str() {
            "/code-executions/execute" => {
                let cell: BorrowedCell = request.json().await?;
                funding.require(&cell.ticket)?;
                self.borrowed_code_execute(record, cell, traceparent).await
            }
            "/code-executions/inspect" | "/code-executions/poll" | "/code-executions/cancel" => {
                let reference: Reference = request.json().await?;
                funding.require(&reference.ticket)?;
                if request.path().ends_with("/inspect") {
                    return self
                        .borrowed_code_inspect(record, &reference, traceparent)
                        .await;
                }
                self.borrowed_code_observe(
                    record,
                    reference,
                    request.path().ends_with("/cancel"),
                    traceparent,
                )
                .await
            }
            "/code-executions/artifact" | "/code-executions/ack" | "/code-executions/complete" => {
                let reference: Reference = request.json().await?;
                funding.require(&reference.ticket)?;
                self.borrowed_code_publication(record, reference, &request.path(), traceparent)
                    .await
            }
            _ => util::json_error(404, "not_found", "Unknown code execution operation"),
        }
    }

    async fn borrowed_code_inspect(
        &self,
        record: &SessionRecord,
        reference: &Reference,
        traceparent: Option<String>,
    ) -> Result<Response> {
        let allocation = self
            .local_code_allocation(&reference.context.id)
            .await?
            .filter(|allocation| {
                allocation.account_id == record.owner_id
                    && allocation.expires_at_ms > util::now_ms()
            })
            .ok_or_else(|| fail("code_container_not_found"))?;
        if allocation.owned()? != reference.context
            || !self
                .workspace(record)
                .with_traceparent(traceparent)
                .python_context_live(&reference.context.id, &reference.context.daemon_boot)
                .await?
        {
            return Err(fail("code_container_not_found"));
        }
        Response::from_json(&json!({"container_id":reference.context.id}))
    }

    async fn borrowed_code_execute(
        &self,
        record: &SessionRecord,
        cell: BorrowedCell,
        traceparent: Option<String>,
    ) -> Result<Response> {
        use sha2::{Digest, Sha256};
        let id = cell.ticket.supervisor_id(&record.owner_id, &cell.context)?;
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&cell)?));
        let storage = self.state.storage();
        let receipt_key = key(&id);
        let mut receipt = read_chunks(&storage, &receipt_key)
            .await?
            .map(|bytes| {
                serde_json::from_slice::<Receipt>(&bytes)
                    .map_err(|_| fail("code_resource_receipt_corrupt"))
            })
            .transpose()?;
        if let Some(receipt) = &receipt {
            if receipt.fingerprint != fingerprint {
                return Err(fail("code_resource_ticket_conflict"));
            }
            if receipt.stage == Stage::Published {
                return Err(fail("code_resource_execution_already_published"));
            }
            if receipt.stage == Stage::ResultKnown {
                return self.borrowed_code_known(&receipt_key).await;
            }
        }
        let _operation = self.code_operation.lock().await;
        // Another request may have completed this exact ticket while we waited.
        receipt = read_chunks(&storage, &receipt_key)
            .await?
            .map(|bytes| {
                serde_json::from_slice::<Receipt>(&bytes)
                    .map_err(|_| fail("code_resource_receipt_corrupt"))
            })
            .transpose()?;
        if let Some(receipt) = &receipt {
            if receipt.fingerprint != fingerprint {
                return Err(fail("code_resource_ticket_conflict"));
            }
            if receipt.stage == Stage::Published {
                return Err(fail("code_resource_execution_already_published"));
            }
            if receipt.stage == Stage::ResultKnown {
                return self.borrowed_code_known(&receipt_key).await;
            }
        }
        let workspace = self
            .workspace(record)
            .existing_code_only()
            .with_traceparent(traceparent);
        let draining = receipt.is_some() && workspace.python_dispatch_exists(&id).await?;
        let allocation = if draining {
            None
        } else {
            let allocation = self
                .local_code_allocation(&cell.context.id)
                .await?
                .ok_or_else(|| fail("code_container_not_found"))?;
            cell.validate_source(&record.owner_id, &allocation, util::now_ms())?;
            if !workspace
                .python_context_live(&cell.context.id, &cell.context.daemon_boot)
                .await?
            {
                return Err(fail("code_container_not_found"));
            }
            Some(allocation)
        };
        if receipt.is_none() {
            let fresh = Receipt {
                owner: record.owner_id.clone(),
                context: cell.context.clone(),
                ticket: cell.ticket.clone(),
                fingerprint,
                stage: Stage::Prepared,
                artifacts: Vec::new(),
                acknowledged: std::collections::BTreeSet::new(),
            };
            write_chunks(&storage, &receipt_key, &serde_json::to_vec(&fresh)?).await?;
            receipt = Some(fresh);
        }
        if let Some(allocation) = allocation {
            let meter = ResourceMeter::new(
                &self.env,
                &record.owner_id,
                &cell.ticket.guest_session,
                &cell.ticket.guest_run,
            )?;
            self.touch_borrowed_code(&allocation.allocation_id, &id, &meter)
                .await?;
        }
        // Workspace's original dispatch fence survives a lost source RPC reply.
        // Re-entering with this same ticket may probe but cannot re-POST a cell.
        let outcome = workspace.execute_python(&id, &cell.request).await?;
        let _metadata = self.code_metadata.lock().await;
        let latest: Receipt = serde_json::from_slice(
            &read_chunks(&storage, &receipt_key)
                .await?
                .ok_or_else(|| fail("code_resource_receipt_corrupt"))?,
        )
        .map_err(|_| fail("code_resource_receipt_corrupt"))?;
        if latest.stage == Stage::Published {
            return Err(fail("code_resource_execution_already_published"));
        }
        if latest.stage == Stage::ResultKnown {
            return Response::from_json(&outcome);
        }
        let receipt = receipt.as_mut().unwrap();
        write_chunks(
            &storage,
            &format!("{receipt_key}:outcome"),
            &serde_json::to_vec(&outcome)?,
        )
        .await?;
        receipt.artifacts = outcome.artifacts.clone();
        receipt.stage = Stage::ResultKnown;
        write_chunks(&storage, &receipt_key, &serde_json::to_vec(receipt)?).await?;
        Response::from_json(&outcome)
    }

    async fn borrowed_code_observe(
        &self,
        record: &SessionRecord,
        reference: Reference,
        cancel: bool,
        traceparent: Option<String>,
    ) -> Result<Response> {
        let id = reference
            .ticket
            .supervisor_id(&record.owner_id, &reference.context)?;
        let workspace = self
            .workspace(record)
            .existing_code_only()
            .with_traceparent(traceparent);
        let receipt = read_chunks(&self.state.storage(), &key(&id))
            .await?
            .map(|bytes| {
                serde_json::from_slice::<Receipt>(&bytes)
                    .map_err(|_| fail("code_resource_receipt_corrupt"))
            })
            .transpose()?;
        if let Some(receipt) = &receipt {
            if receipt.owner != record.owner_id || receipt.context != reference.context {
                return Err(fail("code_resource_ticket_conflict"));
            }
            if receipt.stage == Stage::Published {
                return Err(fail("code_resource_execution_already_published"));
            }
            if receipt.stage == Stage::ResultKnown {
                return self.borrowed_code_known(&key(&id)).await;
            }
        } else {
            // A pre-dispatch cancellation can only name a still-owned context;
            // its stable execution tombstone cannot target another guest ticket.
            let allocation = self
                .local_code_allocation(&reference.context.id)
                .await?
                .ok_or_else(|| fail("code_container_not_found"))?;
            if allocation.owned()? != reference.context {
                return Err(fail("code_container_not_found"));
            }
        }
        if cancel {
            workspace.cancel_python(&id).await?;
        }
        match workspace.poll_python(&id).await? {
            Some(outcome) => {
                let _metadata = self.code_metadata.lock().await;
                let storage = self.state.storage();
                if let Some(bytes) = read_chunks(&storage, &key(&id)).await? {
                    let mut latest: Receipt = serde_json::from_slice(&bytes)
                        .map_err(|_| fail("code_resource_receipt_corrupt"))?;
                    if latest.stage == Stage::Prepared {
                        write_chunks(
                            &storage,
                            &format!("{}:outcome", key(&id)),
                            &serde_json::to_vec(&outcome)?,
                        )
                        .await?;
                        latest.artifacts = outcome.artifacts.clone();
                        latest.stage = Stage::ResultKnown;
                        write_chunks(&storage, &key(&id), &serde_json::to_vec(&latest)?).await?;
                    }
                }
                Response::from_json(&outcome)
            }
            None => Ok(Response::empty()?.with_status(202)),
        }
    }

    async fn borrowed_code_known(&self, receipt_key: &str) -> Result<Response> {
        let _metadata = self.code_metadata.lock().await;
        let receipt: Receipt = serde_json::from_slice(
            &read_chunks(&self.state.storage(), receipt_key)
                .await?
                .ok_or_else(|| fail("code_resource_receipt_corrupt"))?,
        )
        .map_err(|_| fail("code_resource_receipt_corrupt"))?;
        if receipt.stage == Stage::Published {
            return Err(fail("code_resource_execution_already_published"));
        }
        let bytes = read_chunks(&self.state.storage(), &format!("{receipt_key}:outcome"))
            .await?
            .ok_or_else(|| fail("code_resource_receipt_corrupt"))?;
        let headers = Headers::new();
        headers.set("Content-Type", "application/json")?;
        Ok(Response::from_bytes(bytes)?.with_headers(headers))
    }

    async fn borrowed_code_publication(
        &self,
        record: &SessionRecord,
        reference: Reference,
        path: &str,
        traceparent: Option<String>,
    ) -> Result<Response> {
        let id = reference
            .ticket
            .supervisor_id(&record.owner_id, &reference.context)?;
        let _operation = self.code_operation.lock().await;
        let storage = self.state.storage();
        let receipt_key = key(&id);
        let mut receipt: Receipt = serde_json::from_slice(
            &read_chunks(&storage, &receipt_key)
                .await?
                .ok_or_else(|| fail("code_resource_receipt_missing"))?,
        )
        .map_err(|_| fail("code_resource_receipt_corrupt"))?;
        if receipt.owner != record.owner_id || receipt.context != reference.context {
            return Err(fail("code_resource_ticket_conflict"));
        }
        if receipt.stage == Stage::Prepared {
            return Err(fail("code_resource_outcome_pending"));
        }
        let workspace = self
            .workspace(record)
            .existing_code_only()
            .with_traceparent(traceparent);
        if path.ends_with("/complete") {
            if receipt
                .artifacts
                .iter()
                .any(|artifact| !receipt.acknowledged.contains(&artifact.id))
            {
                return Err(fail("code_resource_publication_pending"));
            }
            let _metadata = self.code_metadata.lock().await;
            receipt.stage = Stage::Published;
            write_chunks(&storage, &receipt_key, &serde_json::to_vec(&receipt)?).await?;
            workspace.complete_python_publication(&id).await?;
            // The guest committed exact outcome bytes before requesting this
            // barrier. Keep the small replay fence, not a second log archive.
            write_chunks(&storage, &format!("{receipt_key}:outcome"), b"").await?;
            drop(_metadata);
            self.finish_borrowed_code(&reference.context.id, &id)
                .await?;
            return Ok(Response::empty()?.with_status(204));
        }
        let artifact = reference
            .artifact_id
            .as_deref()
            .and_then(|id| receipt.artifacts.iter().find(|artifact| artifact.id == id))
            .ok_or_else(|| fail("code_resource_artifact_not_found"))?;
        if path.ends_with("/artifact") {
            let bytes = workspace.python_artifact(artifact).await?;
            let headers = Headers::new();
            headers.set("Content-Type", &artifact.mime_type)?;
            headers.set("X-Content-Sha256", &artifact.sha256)?;
            return Ok(Response::from_bytes(bytes)?.with_headers(headers));
        }
        if !receipt.acknowledged.contains(&artifact.id) {
            workspace.ack_python_artifact(&artifact.id).await?;
            let _metadata = self.code_metadata.lock().await;
            receipt.acknowledged.insert(artifact.id.clone());
            write_chunks(&storage, &receipt_key, &serde_json::to_vec(&receipt)?).await?;
        }
        Ok(Response::empty()?.with_status(204))
    }
}
