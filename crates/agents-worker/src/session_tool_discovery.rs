//! Native public views and deterministic discovery stay under one Session owner.
use super::*;
use crate::tool_discovery::SEARCH_ALIAS;
use sha2::{Digest, Sha256};

impl SessionInner {
    pub(super) async fn initialize_discovery(
        &self,
        data: &PersistedSession,
        request: &crate::responses_wire::ResponseRequest,
        items: &[Value],
    ) -> Result<Option<crate::tool_discovery::ToolRegistry>> {
        let mut registry = crate::tool_discovery::ToolRegistry::compile_with_input(
            &request.native_tool_items().map_err(Error::RustError)?,
            items,
        )
        .map_err(Error::RustError)?;
        if let Some(registry) = registry.as_mut() {
            let definitions = tool_definitions(&data.record);
            registry
                .reserve_server_names(
                    definitions
                        .iter()
                        .map(|definition| definition.function.name.as_str()),
                )
                .map_err(Error::RustError)?;
            registry
                .authorize_pending_search(
                    data.pending_general_tools
                        .iter()
                        .filter(|call| call.kind == "tool_search")
                        .map(|call| call.call_id.as_str()),
                )
                .map_err(Error::RustError)?;
            registry.select_choice(request.tool_choice.as_ref());
        }
        Ok(registry)
    }

    pub(super) async fn join_discovery(
        &self,
        data: &PersistedSession,
        registry: &mut crate::tool_discovery::ToolRegistry,
        mcp: &crate::mcp_runtime::McpBindings,
        choice: Option<&Value>,
    ) -> Result<()> {
        registry.register_mcp(mcp).map_err(Error::RustError)?;
        if let Some(previous) = &data.inherited_discovery {
            let origins = match &data.native_context {
                Some(context) => {
                    native_items::authorized_origins(
                        &self.state.storage(),
                        &context.history,
                        &previous.origin_digests(),
                    )
                    .await?
                }
                None => Default::default(),
            };
            registry.inherit_loaded_from(previous, &origins);
        }

        registry.select_choice(choice);
        Ok(())
    }

    pub(super) fn discovery_validation_declarations(&self) -> Result<Vec<Value>> {
        let data = self.data.borrow();
        let response = data
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .ok_or_else(|| Error::RustError("response_missing".into()))?;
        let mut declarations = response
            .request
            .native_tool_items()
            .map_err(Error::RustError)?;
        if let Some(registry) = &response.discovery {
            declarations.extend(registry.validation_declarations());
        }
        Ok(declarations)
    }

    pub(super) fn discovery_declared(&self, item: &Value) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .and_then(|response| response.discovery.as_ref())
            .is_some_and(|registry| registry.declares_public_call(item))
    }

    pub(super) fn mcp_native_call(&self, raw: &Value) -> Result<bool> {
        let data = self.data.borrow();
        let Some(response) = data
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
        else {
            return Ok(false);
        };
        if response
            .mcp_bindings
            .lookup(raw["name"].as_str().unwrap_or_default())
            .is_some()
        {
            return Ok(true);
        }
        response.discovery.as_ref().map_or(Ok(false), |registry| {
            registry
                .resolved_mcp_call(raw)
                .map(|item| item.is_some())
                .map_err(Error::RustError)
        })
    }

    pub(super) fn discovery_complete_identity(&self, raw: &Value) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .and_then(|response| response.discovery.as_ref())
            .is_some_and(|registry| registry.requires_complete_identity(raw))
    }

    pub(super) fn native_server_choices(
        &self,
        completion: &Completion,
    ) -> Result<std::collections::BTreeMap<String, crate::tool_rendering::ServerChoice>> {
        let turn = completion
            .native
            .as_ref()
            .ok_or_else(|| Error::RustError("native_turn_missing".into()))?;
        let data = self.data.borrow();
        let response = data
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .ok_or_else(|| Error::RustError("response_missing".into()))?;
        let bindings = crate::tool_rendering::ToolBindings {
            hosted: &response.bound_tools,
            mcp: &response.mcp_bindings,
            discovery: response.discovery.as_ref(),
            programmatic: response
                .request
                .tools
                .iter()
                .any(|tool| tool.kind == "programmatic_tool_calling"),
        };
        let mut choices = std::collections::BTreeMap::new();
        for raw in &turn.output {
            if !native_output::complete_call(raw, turn.status) {
                continue;
            }
            if let Some(policy) = bindings.policy_call(raw) {
                let id = raw["call_id"]
                    .as_str()
                    .ok_or_else(|| Error::RustError("invalid_native_tool_call".into()))?;
                choices.insert(
                    id.into(),
                    crate::tool_rendering::ServerChoice {
                        kind: "function_call",
                        alias: raw["name"]
                            .as_str()
                            .ok_or_else(|| Error::RustError("invalid_native_tool_call".into()))?
                            .into(),
                        namespace: None,
                        choice: policy,
                    },
                );
            } else if raw["type"] == "tool_search_call"
                && raw["execution"] == "server"
                && response
                    .discovery
                    .as_ref()
                    .is_some_and(|registry| registry.server_search_enabled())
            {
                let id = raw["id"]
                    .as_str()
                    .ok_or_else(|| Error::RustError("invalid_tool_search_call_id".into()))?;
                choices.insert(
                    format!("native-search:{id}"),
                    crate::tool_rendering::ServerChoice {
                        kind: "function_call",
                        alias: SEARCH_ALIAS.into(),
                        namespace: None,
                        choice: raw.clone(),
                    },
                );
            }
        }
        Ok(choices)
    }

    pub(super) fn discovery_policy_calls(
        &self,
        completion: &Completion,
        first_round: bool,
    ) -> Result<Vec<Value>> {
        let turn = completion
            .native
            .as_ref()
            .ok_or_else(|| Error::RustError("native_turn_missing".into()))?;
        let mut data = self.data.borrow_mut();
        let response = data
            .as_mut()
            .and_then(|data| data.run.as_mut())
            .and_then(|run| run.response.as_mut())
            .ok_or_else(|| Error::RustError("response_missing".into()))?;
        let choice = effective_choice(response.request.tool_choice.as_ref(), first_round);
        let mut calls = Vec::new();
        for raw in &turn.output {
            if let Some(registry) = response.discovery.as_mut() {
                registry.observe_output(raw).map_err(Error::RustError)?;
            }
            if !matches!(
                raw["type"].as_str(),
                Some("function_call" | "custom_tool_call" | "tool_search_call")
            ) && !crate::client_tool_items::is_call(raw)
            {
                continue;
            }
            let completed = native_output::complete_call(raw, turn.status);
            if !completed
                && response
                    .discovery
                    .as_ref()
                    .is_some_and(|registry| registry.requires_complete_identity(raw))
            {
                continue;
            }
            let bindings = crate::tool_rendering::ToolBindings {
                hosted: &response.bound_tools,
                mcp: &response.mcp_bindings,
                discovery: response.discovery.as_ref(),
                programmatic: response
                    .request
                    .tools
                    .iter()
                    .any(|tool| tool.kind == "programmatic_tool_calling"),
            };
            if let Some(policy) = bindings.policy_call(raw) {
                if let Some(registry) = response.discovery.as_ref() {
                    if registry
                        .resolved_mcp_call(raw)
                        .map_err(Error::RustError)?
                        .is_some()
                    {
                        let mut complete = raw.clone();
                        if completed {
                            complete["status"] = json!("completed");
                        }
                        registry
                            .validate_call(&complete, None)
                            .map_err(Error::RustError)?;
                    }
                }
                if !crate::tool_discovery::allowed(choice, &policy) {
                    return Err(Error::RustError(
                        "provider_tool_choice_policy_violation".into(),
                    ));
                }
                calls.push(raw.clone());
                continue;
            }
            let item = if let Some(registry) = response.discovery.as_ref() {
                let mut checked = raw.clone();
                if completed {
                    checked["status"] = json!("completed");
                }
                registry
                    .validate_call(&checked, choice)
                    .map_err(Error::RustError)?;
                if raw["type"] == "tool_search_call" && raw["execution"] == "server" {
                    let id = raw["id"]
                        .as_str()
                        .ok_or_else(|| Error::RustError("invalid_tool_search_call_id".into()))?;
                    json!({"type":"function_call","name":SEARCH_ALIAS,"call_id":format!("native-search:{id}"),"arguments":serde_json::to_string(&raw["arguments"])? ,"status":raw["status"]})
                } else {
                    registry.restore_item(raw).map_err(Error::RustError)?
                }
            } else {
                raw.clone()
            };
            calls.push(item);
        }
        Ok(calls)
    }
    pub(super) fn restore_native_item(&self, raw: &Value) -> Result<Value> {
        let data = self.data.borrow();
        let registry = data
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .and_then(|response| response.discovery.as_ref());
        registry.map_or_else(
            || Ok(raw.clone()),
            |registry| registry.restore_item(raw).map_err(Error::RustError),
        )
    }

    pub(super) fn restore_native_event(
        &self,
        kind: &str,
        payload: &Value,
    ) -> Result<Option<Value>> {
        let mut data = self.data.borrow_mut();
        let Some(run) = data.as_mut().and_then(|data| data.run.as_mut()) else {
            return Ok(Some(payload.clone()));
        };
        let round = run.round;
        let Some(response) = run.response.as_mut() else {
            return Ok(Some(payload.clone()));
        };
        let Some(registry) = response.discovery.as_mut() else {
            return Ok(Some(payload.clone()));
        };
        registry.begin_round(round);
        registry
            .restore_event(kind, payload)
            .map_err(Error::RustError)
    }

    pub(super) fn discovery_client_alias(&self, name: &str) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .and_then(|response| response.discovery.as_ref())
            .is_some_and(|registry| registry.is_client_alias(name))
    }

    pub(super) fn discovery_server_alias(&self, name: &str) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .and_then(|response| response.discovery.as_ref())
            .is_some_and(|registry| registry.is_server_search(name))
    }

    pub(super) async fn dispatch_tool_discovery(
        &self,
        execution: &str,
        call: &provider::ToolCall,
    ) -> Result<Option<hosted_dispatch::HostedDispatch>> {
        if !self.discovery_server_alias(&call.function.name) {
            return Ok(None);
        }
        let arguments: Value = serde_json::from_str(&call.function.arguments)
            .map_err(|_| Error::RustError("invalid_tool_search_arguments".into()))?;
        let (mut registry, choice) = {
            let data = self.data.borrow();
            let data = data
                .as_ref()
                .ok_or_else(|| Error::RustError("session_missing".into()))?;
            let response = data
                .run
                .as_ref()
                .and_then(|run| run.response.as_ref())
                .ok_or_else(|| Error::RustError("response_missing".into()))?;
            (
                response
                    .discovery
                    .clone()
                    .ok_or_else(|| Error::RustError("tool_registry_missing".into()))?,
                response.request.tool_choice.clone(),
            )
        };
        let bridge = registry.is_bridge();
        let result = registry
            .discover_execution(execution, &call.id, &arguments, choice.as_ref(), bridge)
            .map_err(Error::RustError)?;
        let output = result.private_output["output"]
            .as_str()
            .ok_or_else(|| Error::RustError("tool_search_bridge_required".into()))?
            .to_owned();
        {
            let mut data = self.data.borrow_mut();
            data.as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .discovery = Some(registry);
        }
        // Loading is the effect: publish neither a loaded schema nor a completed
        // call until its immutable receipt and loaded set are durable.
        self.persist().await?;
        let mut item = result.call;
        item["id"] = json!(format!(
            "discovery_{:x}",
            Sha256::digest(format!("{execution}:call").as_bytes())
        ));
        self.publish_hosted_item(&item, true).await?;
        let mut item = result.output;
        item["id"] = json!(format!(
            "discovery_{:x}",
            Sha256::digest(format!("{execution}:output").as_bytes())
        ));
        self.publish_hosted_item(&item, true).await?;
        Ok(Some(hosted_dispatch::HostedDispatch::Ready(output.into())))
    }
}

fn effective_choice(choice: Option<&Value>, first_round: bool) -> Option<&Value> {
    choice.filter(|choice| first_round || *choice == "none" || choice["type"] == "allowed_tools")
}
