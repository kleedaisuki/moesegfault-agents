//! The existing Session is the only MCP execution and publication authority.
use super::*;
use crate::{
    mcp_runtime::{ApprovalReference, ApprovedCall, CallIntent, McpBindings},
    mcp_transport::{RpcFailure, RpcOutcome},
};

#[derive(Clone, Serialize, Deserialize)]
struct DiscoverySnapshot {
    identity: String,
    bindings: McpBindings,
}

#[derive(Clone, Serialize, Deserialize)]
struct McpOperation {
    intent: CallIntent,
    dispatched: bool,
    outcome: Option<RpcOutcome>,
    result: Option<hosted_dispatch::ToolOutput>,
}

struct PreparationGuard<'a> {
    preparing: &'a RefCell<std::collections::BTreeMap<String, usize>>,
    id: String,
}
impl Drop for PreparationGuard<'_> {
    fn drop(&mut self) {
        let mut preparing = self.preparing.borrow_mut();
        if let Some(count) = preparing.get_mut(&self.id) {
            *count -= 1;
            if *count == 0 {
                preparing.remove(&self.id);
            }
        }
    }
}
impl SessionInner {
    async fn publish_mcp_item(&self, item: &Value, terminal: bool) -> Result<()> {
        let initial = crate::mcp_items::initial_item(item).unwrap_or_else(|| item.clone());
        self.publish_tool_item(item, terminal, initial, |index, is_new| {
            let mut events = if is_new {
                crate::mcp_items::argument_events(item, index)
            } else {
                vec![]
            };
            if let Some(kind) = crate::mcp_items::phase_event(item, terminal) {
                events.push((kind, json!({"item_id":item["id"],"output_index":index})));
            }
            events
        })
        .await
    }

    pub(super) async fn create_response(
        &self,
        create: responses::CreateResponse,
        traceparent: Option<String>,
    ) -> Result<Response> {
        let id = create.response_id.clone();
        *self
            .mcp_preparing
            .borrow_mut()
            .entry(id.clone())
            .or_default() += 1;
        let guard = PreparationGuard {
            preparing: &self.mcp_preparing,
            id: id.clone(),
        };
        let result = match self.create_response_inner(create, traceparent).await {
            Err(Error::RustError(code)) if code == "mcp_authorization_required" => {
                util::json_error(
                    400,
                    &code,
                    "Supply current MCP authorization on each new Response creation",
                )
            }
            Err(Error::RustError(code)) if code == "mcp_approval_not_found" => {
                util::json_error(404, &code, "MCP approval is not retained for this owner")
            }
            Err(Error::RustError(code))
                if matches!(
                    code.as_str(),
                    "mcp_approval_origin_mismatch"
                        | "mcp_approval_schema_changed"
                        | "mcp_tool_not_allowed"
                        | "invalid_mcp_approval_context"
                        | "mcp_approval_requires_execution"
                ) =>
            {
                util::json_error(400, &code, "The requested MCP approval cannot be applied")
            }
            Err(Error::RustError(code))
                if matches!(
                    code.as_str(),
                    "mcp_programmatic_binding_required" | "mcp_connector_not_configured"
                ) =>
            {
                util::json_error(
                    422,
                    &code,
                    "This MCP transport or caller capability is not enabled",
                )
            }
            result => result,
        };
        drop(guard);
        let rejected = result
            .as_ref()
            .map_or(true, |response| response.status_code() >= 400);
        if rejected && matches!(self.read_response(&id).await, Ok(None)) {
            if self.finalize_mcp_response(&id).await.is_err() {
                self.defer_mcp_cleanup().await?;
            }
        }
        result
    }

    pub(super) async fn restore_mcp_cleanup(&self) -> Result<()> {
        let entries = self
            .state
            .storage()
            .list_with_options(ListOptions::new().prefix("mcp-cache-index:").limit(1))
            .await?;
        if entries.size() != 0 {
            self.defer_mcp_cleanup().await?;
        }
        Ok(())
    }

    pub(super) async fn defer_mcp_cleanup(&self) -> Result<()> {
        if let Some(data) = self.data.borrow_mut().as_mut() {
            data.mcp_cleanup_at = Some(util::now_ms().saturating_add(1000));
        }
        self.persist().await?;
        self.arm_alarm().await
    }

    pub(super) async fn reconcile_mcp_cleanup(&self) -> Result<()> {
        if self
            .data
            .borrow()
            .as_ref()
            .and_then(|data| data.mcp_cleanup_at)
            .is_none_or(|deadline| deadline > util::now_ms())
        {
            return Ok(());
        }
        let entries = self
            .state
            .storage()
            .list_with_options(ListOptions::new().prefix("mcp-cache-index:").limit(16))
            .await?;
        let mut ids = Vec::new();
        entries.for_each(&mut |value, _| {
            if let Ok(id) = serde_wasm_bindgen::from_value::<String>(value) {
                ids.push(id);
            }
        });
        let mut pending = false;
        for id in ids {
            let busy = self.mcp_preparing.borrow().contains_key(&id)
                || self.data.borrow().as_ref().is_some_and(|data| {
                    data.run.as_ref().is_some_and(|run| run.id == id)
                        || data
                            .steering_queue
                            .iter()
                            .any(|steer| steer.parent == id && steer.applied_to.is_none())
                });
            if busy || self.finalize_mcp_response(&id).await.is_err() {
                pending = true;
            }
        }
        let more = self
            .state
            .storage()
            .list_with_options(ListOptions::new().prefix("mcp-cache-index:").limit(1))
            .await?
            .size()
            != 0;
        if let Some(data) = self.data.borrow_mut().as_mut() {
            data.mcp_cleanup_at = (pending || more).then(|| util::now_ms().saturating_add(30_000));
        }
        self.persist().await
    }

    /// Called after the terminal commit and after deciding no automatic successor
    /// needs this task's active credential cache. It uses the prepaid cleanup hold.
    pub(super) async fn finalize_mcp_response(&self, response_id: &str) -> Result<()> {
        let key = format!("mcp-bindings:{response_id}");
        let Some(bytes) = read_chunks(&self.state.storage(), &key).await? else {
            self.state
                .storage()
                .delete(&format!("mcp-cache-index:{response_id}"))
                .await?;
            return Ok(());
        };
        if bytes == br#"{"deleted":true}"# {
            self.state
                .storage()
                .delete(&format!("mcp-cache-index:{response_id}"))
                .await?;
            return Ok(());
        }
        let mut snapshot: DiscoverySnapshot =
            serde_json::from_slice(&bytes).map_err(|_| mcp_fault("mcp_bindings_corrupt"))?;
        let started = util::now_ms();
        snapshot.bindings.clear_credentials();
        if let Some(response) = self.read_response(response_id).await? {
            for operation in &response.mcp_operation_keys {
                let saved = match self.read_mcp_operation(operation).await {
                    Err(Error::RustError(code)) if code == "mcp_operation_deleted" => None,
                    saved => saved?,
                };
                if let Some(mut saved) = saved {
                    saved.intent.server.clear_credentials();
                    self.write_mcp_operation(operation, &saved).await?;
                }
            }
        }
        if let Some(funding) = snapshot.bindings.discovery_funding.as_mut() {
            funding.capture_cleanup(started, util::now_ms());
        }
        write_chunks(&self.state.storage(), &key, &serde_json::to_vec(&snapshot)?).await?;
        let owner = self.data.borrow().as_ref().unwrap().record.owner_id.clone();
        if let Some(funding) = snapshot.bindings.discovery_funding.as_mut() {
            funding.complete_cleanup(&self.env, &owner).await?;
        }
        write_chunks(&self.state.storage(), &key, &serde_json::to_vec(&snapshot)?).await?;
        self.state
            .storage()
            .delete(&format!("mcp-cache-index:{response_id}"))
            .await?;
        Ok(())
    }

    pub(super) async fn finish_mcp_handoff(&self, parent: &str, child: &str) -> Result<()> {
        let child_key = format!("mcp-bindings:{child}");
        if read_chunks(&self.state.storage(), &child_key)
            .await?
            .is_none()
        {
            return Ok(());
        }
        let parent_key = format!("mcp-bindings:{parent}");
        if let Some(bytes) = read_chunks(&self.state.storage(), &parent_key).await? {
            let mut snapshot: DiscoverySnapshot =
                serde_json::from_slice(&bytes).map_err(|_| mcp_fault("mcp_bindings_corrupt"))?;
            snapshot.bindings.clear_credentials();
            snapshot.bindings.discovery_funding = None;
            write_chunks(
                &self.state.storage(),
                &parent_key,
                &serde_json::to_vec(&snapshot)?,
            )
            .await?;
            self.state
                .storage()
                .delete(&format!("mcp-cache-index:{parent}"))
                .await?;
        }
        Ok(())
    }

    pub(super) async fn erase_mcp_response(&self, response: &ResponseRun) -> Result<()> {
        if !response.deleted
            || (response.mcp_request_proof.is_none() && response.mcp_operation_keys.is_empty())
        {
            return Ok(());
        }
        self.finalize_mcp_response(&response.id).await?;
        let owner = self.data.borrow().as_ref().unwrap().record.owner_id.clone();
        let mut funding = crate::mcp_funding::DiscoveryFunding::start_read(
            &self.env,
            &owner,
            agents_auth::billing::Resource::DurableObject,
        )
        .await?;
        for operation in &response.mcp_operation_keys {
            write_chunks(&self.state.storage(), operation, br#"{"deleted":true}"#).await?;
        }
        write_chunks(
            &self.state.storage(),
            &format!("mcp-bindings:{}", response.id),
            br#"{"deleted":true}"#,
        )
        .await?;
        funding.observe_end();
        funding.finish(&self.env, &owner).await
    }
    pub(super) async fn publish_mcp_lists(&self) -> Result<()> {
        let lists = {
            let data = self.data.borrow();
            let Some(response) = data
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .and_then(|run| run.response.as_ref())
            else {
                return Ok(());
            };
            if response.execution.warmup || response.mcp_lists_published {
                return Ok(());
            }
            response.mcp_bindings.list_items()
        };
        for item in lists {
            self.publish_mcp_item(&item, true).await?;
        }
        {
            let mut data = self.data.borrow_mut();
            let response = data
                .as_mut()
                .and_then(|data| data.run.as_mut())
                .and_then(|run| run.response.as_mut())
                .ok_or_else(|| mcp_fault("mcp_response_missing"))?;
            if response.mcp_bindings.is_empty() {
                return Ok(());
            }
            response.mcp_lists_published = true;
        }
        self.persist().await?;
        Ok(())
    }
    pub(super) async fn resolve_mcp_bindings(
        &self,
        request: &crate::responses_wire::ResponseRequest,
        record: &SessionRecord,
        response_id: &str,
        inherited: Option<&str>,
    ) -> Result<McpBindings> {
        if !request.tools.iter().any(|tool| tool.kind == "mcp") {
            return Ok(McpBindings::default());
        }
        let identity = McpBindings::declaration_proof(
            &self.env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
            &record.owner_id,
            response_id,
            request,
        )?;
        let key = format!("mcp-bindings:{response_id}");
        let saved = read_chunks(&self.state.storage(), &key)
            .await?
            .map(|bytes| {
                serde_json::from_slice::<DiscoverySnapshot>(&bytes)
                    .map_err(|_| mcp_fault("mcp_bindings_corrupt"))
            })
            .transpose()?;
        let mut saved = match saved {
            Some(saved) => {
                if !agents_auth::constant_eq(&saved.identity, &identity) {
                    return Err(mcp_fault("idempotency_conflict"));
                }
                saved
            }
            None => {
                let inherited = match inherited {
                    Some(parent) => {
                        read_chunks(&self.state.storage(), &format!("mcp-bindings:{parent}"))
                            .await?
                            .map(|bytes| {
                                serde_json::from_slice::<DiscoverySnapshot>(&bytes)
                                    .map_err(|_| mcp_fault("mcp_bindings_corrupt"))
                            })
                            .transpose()?
                            .map(|snapshot| snapshot.bindings)
                    }
                    None => None,
                };
                let bindings = match inherited {
                    Some(bindings) => {
                        if bindings
                            .servers
                            .iter()
                            .any(|server| !server.credentials_available())
                        {
                            return Err(mcp_fault("mcp_authorization_required"));
                        }
                        bindings
                    }
                    None => {
                        McpBindings::resolve(
                            &self.env,
                            &record.owner_id,
                            response_id,
                            &request.native_tool_items().map_err(Error::RustError)?,
                            agents_auth::billing::Resource::DurableObject,
                        )
                        .await?
                    }
                };
                DiscoverySnapshot { identity, bindings }
            }
        };
        self.state
            .storage()
            .put(&format!("mcp-cache-index:{response_id}"), response_id)
            .await?;
        write_chunks(&self.state.storage(), &key, &serde_json::to_vec(&saved)?).await?;
        if let Some(funding) = saved.bindings.discovery_funding.as_mut() {
            if funding.finish(&self.env, &record.owner_id).await.is_ok() {
                write_chunks(&self.state.storage(), &key, &serde_json::to_vec(&saved)?).await?;
            }
        }
        Ok(saved.bindings)
    }

    pub(super) fn resolved_mcp_call(
        &self,
        call: &provider::ToolCall,
    ) -> Result<provider::ToolCall> {
        let data = self.data.borrow();
        let registry = data
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .and_then(|response| response.discovery.as_ref());
        let raw = json!({"type":"function_call","name":call.function.name,"arguments":call.function.arguments,"call_id":call.id,"status":"completed"});
        let Some(resolved) = registry
            .map(|registry| registry.resolved_mcp_call(&raw))
            .transpose()
            .map_err(Error::RustError)?
            .flatten()
        else {
            return Ok(call.clone());
        };
        let mut call = call.clone();
        call.function.name = resolved["name"]
            .as_str()
            .ok_or_else(|| mcp_fault("invalid_mcp_arguments"))?
            .into();
        call.function.arguments = resolved["arguments"]
            .as_str()
            .ok_or_else(|| mcp_fault("invalid_mcp_arguments"))?
            .into();
        Ok(call)
    }

    pub(super) fn validate_mcp_calls(&self, completion: &Completion) -> Result<()> {
        let data = self.data.borrow();
        let Some(data) = data.as_ref() else {
            return Ok(());
        };
        let Some(run) = data.run.as_ref() else {
            return Ok(());
        };
        let Some(response) = run.response.as_ref() else {
            return Ok(());
        };
        for call in completion
            .message
            .tool_calls
            .iter()
            .filter(|call| mcp_call_complete(completion, call))
        {
            let call = self.resolved_mcp_call(call)?;
            response.mcp_bindings.prepare_call(
                &data.record.owner_id,
                &response.id,
                "validation",
                &call,
            )?;
        }
        Ok(())
    }
    /// Call after known model usage/native history, before server-tool dispatch.
    /// The pending item refers to durable source intent, never to a client list.
    pub(super) async fn classify_mcp_approvals(&self, completion: &Completion) -> Result<()> {
        for call in completion
            .message
            .tool_calls
            .iter()
            .filter(|call| mcp_call_complete(completion, call))
        {
            let original = call;
            let call = self.resolved_mcp_call(call)?;
            let (record, response_id, intent) = {
                let data = self.data.borrow();
                let data = data
                    .as_ref()
                    .ok_or_else(|| mcp_fault("session_not_initialized"))?;
                let run = data
                    .run
                    .as_ref()
                    .ok_or_else(|| mcp_fault("session_run_missing"))?;
                let Some(response) = run.response.as_ref() else {
                    return Ok(());
                };
                let execution = execution_key(&run.id, run.round, &call.id);
                let intent = response.mcp_bindings.prepare_call(
                    &data.record.owner_id,
                    &response.id,
                    &execution,
                    &call,
                )?;
                (data.record.clone(), response.id.clone(), intent)
            };
            let Some(mut intent) = intent.filter(|intent| intent.approval_id.is_some()) else {
                continue;
            };
            intent.server.clear_credentials();
            let key = mcp_key(&response_id, &intent.execution);
            self.track_mcp_operation(&key).await?;
            let saved = self.read_mcp_operation(&key).await?;
            if let Some(saved) = &saved {
                if !same_intent(&saved.intent, &intent)? {
                    return Err(mcp_fault("mcp_operation_conflict"));
                }
            } else {
                self.write_mcp_operation(
                    &key,
                    &McpOperation {
                        intent: intent.clone(),
                        dispatched: false,
                        outcome: None,
                        result: None,
                    },
                )
                .await?;
            }
            let item = intent
                .approval_item()
                .ok_or_else(|| mcp_fault("mcp_approval_missing"))?;
            let reference = ApprovalReference::new(&record.owner_id, &record.id, &key, &intent)?;
            let token = reference.seal(&self.env.secret("DATA_ENCRYPTION_KEY")?.to_string())?;
            let mut pending = native_output::GeneralPendingCall::from_item(&item)
                .ok_or_else(|| mcp_fault("mcp_approval_missing"))?;
            pending.name = original.function.name.clone();
            pending.details.as_mut().unwrap()["__agents_mcp"] =
                json!({"capability":token,"native_call_id":call.id});
            {
                let mut data = self.data.borrow_mut();
                let response = data
                    .as_mut()
                    .unwrap()
                    .run
                    .as_mut()
                    .unwrap()
                    .response
                    .as_mut()
                    .unwrap();
                if !response
                    .general_pending_calls
                    .iter()
                    .any(|known| known.item_id == pending.item_id)
                {
                    response.general_pending_calls.push(pending);
                }
            }
            self.publish_mcp_item(&item, true).await?;
        }
        Ok(())
    }

    pub(super) async fn prepare_mcp_approvals(
        &self,
        owner: &str,
        response_id: &str,
        pending: &[native_output::GeneralPendingCall],
        input: &[Value],
        fresh: &McpBindings,
    ) -> Result<Vec<ApprovedCall>> {
        if !input
            .iter()
            .any(|item| item["type"] == "mcp_approval_response")
        {
            return Ok(vec![]);
        }
        let mut funding = crate::mcp_funding::DiscoveryFunding::start_read(
            &self.env,
            owner,
            agents_auth::billing::Resource::DurableObject,
        )
        .await?;
        let result = self
            .prepare_mcp_approvals_inner(owner, response_id, pending, input, fresh)
            .await;
        funding.observe_end();
        let accounting = funding.finish(&self.env, owner).await;
        match result {
            Ok(result) => {
                accounting?;
                Ok(result)
            }
            Err(error) => Err(error),
        }
    }
    async fn prepare_mcp_approvals_inner(
        &self,
        owner: &str,
        response_id: &str,
        pending: &[native_output::GeneralPendingCall],
        input: &[Value],
        fresh: &McpBindings,
    ) -> Result<Vec<ApprovedCall>> {
        let mut result = Vec::new();
        for decision in input
            .iter()
            .filter(|item| item["type"] == "mcp_approval_response")
        {
            let id = decision["approval_request_id"]
                .as_str()
                .ok_or_else(|| mcp_fault("invalid_mcp_approval_response"))?;
            let expected = pending
                .iter()
                .find(|pending| pending.kind == "mcp_approval" && pending.call_id == id)
                .ok_or_else(|| mcp_fault("unexpected_mcp_approval"))?;
            let details = expected
                .details
                .as_ref()
                .ok_or_else(|| mcp_fault("invalid_mcp_approval_context"))?;
            crate::mcp_items::validate_result(details, decision).map_err(Error::RustError)?;
            let token = details
                .pointer("/__agents_mcp/capability")
                .and_then(Value::as_str)
                .ok_or_else(|| mcp_fault("invalid_mcp_approval_context"))?;
            let reference = ApprovalReference::unseal(
                &self.env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
                owner,
                id,
                token,
            )?;
            if reference.native_call_id != expected.native_call_id() {
                return Err(mcp_fault("invalid_mcp_approval_context"));
            }
            let intent = self.load_mcp_approval(&reference, token).await?;
            let execution = format!("approval:{response_id}:{id}");
            result.push(ApprovedCall {
                intent: if decision["approve"] == true {
                    fresh.rebind_approved(intent)?
                } else {
                    intent
                }
                .for_execution(&execution),
                approved: decision["approve"].as_bool().unwrap(),
                completed: false,
            });
        }
        Ok(result)
    }

    async fn load_mcp_approval(
        &self,
        reference: &ApprovalReference,
        token: &str,
    ) -> Result<CallIntent> {
        let record = self.data.borrow().as_ref().unwrap().record.clone();
        if record.owner_id != reference.owner {
            return Err(mcp_fault("mcp_approval_not_found"));
        }
        if record.id == reference.source_session {
            return self.local_mcp_approval(reference).await;
        }
        let source = Store::new(&self.env)
            .get_session(&record.owner_id, &reference.source_session)
            .await?
            .ok_or_else(|| mcp_fault("mcp_approval_not_found"))?;
        let trace = crate::telemetry::TraceContext::from_traceparent(None);
        let context = crate::routes::RouteContext::new(&self.env, &trace);
        let mut response = crate::routes::forward(
            &context,
            &source,
            Method::Post,
            "/mcp-approval",
            Some(json!({"approval_id":reference.approval_id,"capability":token})),
        )
        .await?;
        if response.status_code() != 200 {
            return Err(mcp_fault("mcp_approval_not_found"));
        }
        let intent: CallIntent = response.json().await?;
        if !reference.matches(&intent) {
            return Err(mcp_fault("mcp_approval_not_found"));
        }
        Ok(intent)
    }

    pub(super) async fn mcp_approval_route(&self, mut request: Request) -> Result<Response> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Lookup {
            approval_id: String,
            capability: String,
        }
        let input: Lookup = request.json().await?;
        let record = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(|| mcp_fault("session_not_initialized"))?
            .record
            .clone();
        let reference = ApprovalReference::unseal(
            &self.env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
            &record.owner_id,
            &input.approval_id,
            &input.capability,
        )?;
        Response::from_json(&self.local_mcp_approval(&reference).await?)
    }

    async fn local_mcp_approval(&self, reference: &ApprovalReference) -> Result<CallIntent> {
        let record = self.data.borrow().as_ref().unwrap().record.clone();
        if reference.owner != record.owner_id || reference.source_session != record.id {
            return Err(mcp_fault("mcp_approval_not_found"));
        }
        let response = self
            .read_response(&reference.source_response)
            .await?
            .filter(|response| {
                !response.deleted
                    && response
                        .output
                        .iter()
                        .any(|item| item["id"] == reference.approval_id)
            })
            .ok_or_else(|| mcp_fault("mcp_approval_not_found"))?;
        let operation = self
            .read_mcp_operation(&reference.operation)
            .await?
            .ok_or_else(|| mcp_fault("mcp_approval_not_found"))?;
        if !reference.matches(&operation.intent)
            || operation.dispatched
            || response.id != reference.source_response
        {
            return Err(mcp_fault("mcp_approval_not_found"));
        }
        Ok(operation.intent)
    }

    /// Return true when one approved/declined action was advanced. The driver
    /// calls this before any new inference and after its normal funding admission.
    pub(super) async fn advance_mcp_approvals(&self, record: &SessionRecord) -> Result<bool> {
        let next = {
            let data = self.data.borrow();
            let run = data
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .ok_or_else(|| mcp_fault("session_run_missing"))?;
            if !run.admitted {
                return Err(mcp_fault("mcp_run_not_funded"));
            }
            run.response.as_ref().and_then(|response| {
                response
                    .mcp_approvals
                    .iter()
                    .enumerate()
                    .find(|(_, action)| !action.completed)
                    .map(|(index, action)| (index, action.clone()))
            })
        };
        let Some((index, action)) = next else {
            return Ok(false);
        };
        let allowed = !action.approved
            || self
                .admit_tool_dispatch(
                    record,
                    &action.intent.execution,
                    &action.intent.server.alias(&action.intent.tool.name),
                )
                .await?;
        let output = if !allowed {
            self.publish_mcp_item(&action.intent.result_item(None, Some(json!({"type":"mcp_protocol_error","code":-32000,"message":"The response tool allowance is exhausted"}))), true).await?;
            tool_budget::exhausted_result().into()
        } else if action.approved {
            match self.execute_mcp(record, &action.intent).await? {
                hosted_dispatch::HostedDispatch::Ready(output) => output,
                hosted_dispatch::HostedDispatch::Pending => return Ok(true),
            }
        } else {
            self.publish_mcp_item(&action.intent.denied(), true).await?;
            json!({"error":{"code":"mcp_approval_denied","message":"The requested MCP call was declined."}}).to_string().into()
        };
        let native = action.intent.native_output(&output.clone().into())?;
        {
            let mut data = self.data.borrow_mut();
            let data = data.as_mut().unwrap();
            let message =
                Message::tool_result_content(&action.intent.native_call_id, output.into());
            let at = data
                .messages
                .iter()
                .rposition(|message| message.role == "assistant")
                .and_then(|at| {
                    data.messages
                        .iter()
                        .enumerate()
                        .skip(at + 1)
                        .find(|(_, message)| message.role != "tool")
                        .map(|(at, _)| at)
                })
                .unwrap_or(data.messages.len());
            data.messages.insert(at, message.clone());
            data.journal.push(message);
            let length = data.messages.len();
            let run = data.run.as_mut().unwrap();
            if let Some(batch) = run.native_input.as_mut() {
                for item in &mut batch.items {
                    if item["type"] == "mcp_approval_response"
                        && item["approval_request_id"]
                            == action.intent.approval_id.as_deref().unwrap()
                    {
                        *item = native.clone();
                    }
                }
                batch.message_end = length;
            }
            run.response.as_mut().unwrap().mcp_approvals[index].completed = true;
        }
        self.persist().await?;
        Ok(true)
    }

    pub(super) async fn dispatch_mcp_tool(
        &self,
        record: &SessionRecord,
        execution: &str,
        call: &provider::ToolCall,
    ) -> Result<Option<hosted_dispatch::HostedDispatch>> {
        let call = self.resolved_mcp_call(call)?;
        let intent = {
            let data = self.data.borrow();
            let run = data
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .ok_or_else(|| mcp_fault("session_run_missing"))?;
            let Some(response) = run.response.as_ref() else {
                return Ok(None);
            };
            response
                .mcp_bindings
                .prepare_call(&record.owner_id, &response.id, execution, &call)?
        };
        let Some(intent) = intent else {
            return Ok(None);
        };
        if intent.approval_id.is_some() {
            return Err(mcp_fault("mcp_approval_required"));
        }
        self.execute_mcp(record, &intent).await.map(Some)
    }

    async fn execute_mcp(
        &self,
        record: &SessionRecord,
        intent: &CallIntent,
    ) -> Result<hosted_dispatch::HostedDispatch> {
        let response_id = {
            let data = self.data.borrow();
            let run = data
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .filter(|run| run.admitted)
                .ok_or_else(|| mcp_fault("mcp_run_not_funded"))?;
            run.response
                .as_ref()
                .ok_or_else(|| mcp_fault("mcp_response_missing"))?
                .id
                .clone()
        };
        let key = mcp_key(&response_id, &intent.execution);
        self.track_mcp_operation(&key).await?;
        let mut operation = match self.read_mcp_operation(&key).await? {
            Some(operation) => {
                if !same_intent(&operation.intent, intent)? {
                    return Err(mcp_fault("mcp_operation_conflict"));
                }
                operation
            }
            None => McpOperation {
                intent: intent.clone(),
                dispatched: false,
                outcome: None,
                result: None,
            },
        };
        if let Some(result) = operation.result {
            return Ok(hosted_dispatch::HostedDispatch::Ready(result));
        }
        if operation.dispatched && operation.outcome.is_none() {
            operation.outcome = Some(RpcOutcome {
                result: None,
                failure: Some(RpcFailure {
                    code: "mcp_outcome_unknown".into(),
                    http_status: None,
                    protocol_code: None,
                    outcome_unknown: true,
                }),
                session_id: None,
                payload_bytes: 0,
                started_ms: 0,
                ended_ms: 0,
            });
            operation.intent.server.clear_credentials();
            self.write_mcp_operation(&key, &operation).await?;
        }
        if operation.outcome.is_none() {
            let request = intent.prepare(&self.env, &record.owner_id)?;
            operation.dispatched = true;
            self.write_mcp_operation(&key, &operation).await?;
            let started = util::now_ms();
            operation.outcome = Some(if self.mcp_cancelled() {
                RpcOutcome {
                    result: None,
                    failure: Some(RpcFailure {
                        code: "mcp_cancelled".into(),
                        http_status: None,
                        protocol_code: None,
                        outcome_unknown: false,
                    }),
                    session_id: None,
                    payload_bytes: 0,
                    started_ms: started,
                    ended_ms: util::now_ms(),
                }
            } else {
                match request
                    .dispatch_cancellable(self.wait_mcp_cancelled())
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => RpcOutcome {
                        result: None,
                        failure: Some(RpcFailure {
                            code: "mcp_outcome_unknown".into(),
                            http_status: None,
                            protocol_code: None,
                            outcome_unknown: true,
                        }),
                        session_id: None,
                        payload_bytes: 0,
                        started_ms: started,
                        ended_ms: util::now_ms(),
                    },
                }
            });
            operation.intent.server.clear_credentials();
            // This invocation keeps the known receipt alive across storage errors;
            // it never turns a failed publication into permission for another HTTP.
            while self.write_mcp_operation(&key, &operation).await.is_err() {
                worker::Delay::from(std::time::Duration::from_millis(250)).await;
            }
        }
        let outcome = operation.outcome.as_ref().unwrap();
        let (item, output) = completed(&operation.intent, outcome)?;
        self.publish_mcp_item(&item, true).await?;
        operation.result = Some(output.clone());
        operation.intent.server.clear_credentials();
        self.write_mcp_operation(&key, &operation).await?;
        Ok(hosted_dispatch::HostedDispatch::Ready(output))
    }

    fn mcp_cancelled(&self) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .is_none_or(|response| response.cancel_requested || response.deleted)
    }

    async fn wait_mcp_cancelled(&self) {
        while !self.mcp_cancelled() {
            worker::Delay::from(std::time::Duration::from_millis(100)).await;
        }
    }
    async fn track_mcp_operation(&self, key: &str) -> Result<()> {
        {
            let mut data = self.data.borrow_mut();
            let response = data
                .as_mut()
                .and_then(|data| data.run.as_mut())
                .and_then(|run| run.response.as_mut())
                .ok_or_else(|| mcp_fault("mcp_response_missing"))?;
            if !response.mcp_operation_keys.iter().any(|known| known == key) {
                response.mcp_operation_keys.push(key.into());
            }
        }
        self.persist().await
    }

    async fn read_mcp_operation(&self, key: &str) -> Result<Option<McpOperation>> {
        let Some(bytes) = read_chunks(&self.state.storage(), key).await? else {
            return Ok(None);
        };
        if bytes == br#"{"deleted":true}"# {
            return Err(mcp_fault("mcp_operation_deleted"));
        }
        let owner = self.data.borrow().as_ref().unwrap().record.owner_id.clone();
        let token: String =
            serde_json::from_slice(&bytes).map_err(|_| mcp_fault("mcp_operation_corrupt"))?;
        let plaintext = agents_auth::unseal(
            &self.env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
            &format!("mcp-operation:v1:{owner}:{key}"),
            &token,
        )
        .map_err(|_| mcp_fault("mcp_operation_corrupt"))?;
        serde_json::from_slice(&plaintext)
            .map(Some)
            .map_err(|_| mcp_fault("mcp_operation_corrupt"))
    }
    async fn write_mcp_operation(&self, key: &str, value: &McpOperation) -> Result<()> {
        let owner = self.data.borrow().as_ref().unwrap().record.owner_id.clone();
        let token = agents_auth::seal(
            &self.env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
            &format!("mcp-operation:v1:{owner}:{key}"),
            &serde_json::to_vec(value)?,
        )
        .map_err(|_| mcp_fault("mcp_operation_unavailable"))?;
        write_chunks(&self.state.storage(), key, &serde_json::to_vec(&token)?).await
    }
}

fn mcp_call_complete(completion: &Completion, call: &provider::ToolCall) -> bool {
    match &completion.native {
        Some(turn) => turn.output.iter().any(|item| {
            item["call_id"] == call.id && native_output::complete_call(item, turn.status)
        }),
        None => !matches!(
            completion.finish_reason.as_str(),
            "length" | "content_filter" | "error" | "cancelled"
        ),
    }
}
fn completed(
    intent: &CallIntent,
    outcome: &RpcOutcome,
) -> Result<(Value, hosted_dispatch::ToolOutput)> {
    if let Some(failure) = &outcome.failure {
        let error = failure.public();
        return Ok((
            intent.result_item(None, Some(error.clone())),
            json!({"error":error,"outcome_unknown":failure.outcome_unknown})
                .to_string()
                .into(),
        ));
    }
    let Some(result) = outcome.result.clone() else {
        return Err(mcp_fault("mcp_result_missing"));
    };
    match crate::mcp_transport::tool_result(result) {
        Ok((output, failed)) => {
            let error = failed.then(|| json!({"type":"mcp_tool_execution_error","content":serde_json::from_str::<Value>(&output).unwrap()["content"]}));
            let content = crate::mcp_transport::tool_content(&output).map(hosted_dispatch::ToolOutput::Mcp).unwrap_or_else(|_| {
                json!({"error":{"code":"mcp_result_modality_unavailable","message":"The MCP result remains available to the user but cannot be consumed by this model."}}).to_string().into()
            });
            Ok((intent.result_item(Some(&output), error), content))
        }
        Err(_) => {
            let error = json!({"type":"mcp_protocol_error","code":-32603,"message":"Invalid or oversized MCP tool result"});
            Ok((
                intent.result_item(None, Some(error.clone())),
                json!({"error":error}).to_string().into(),
            ))
        }
    }
}

fn mcp_key(response: &str, execution: &str) -> String {
    format!(
        "mcp:{response}:{}",
        crate::mcp_bindings::digest(execution.as_bytes())
    )
}
fn same_intent(left: &CallIntent, right: &CallIntent) -> Result<bool> {
    Ok(left.identity()? == right.identity()?)
}
fn mcp_fault(code: &str) -> Error {
    Error::RustError(code.into())
}
