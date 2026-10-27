//! One immutable MCP discovery snapshot per accepted Response. The Session remains
//! the execution authority; remote RPC completion is never an inference retry.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Env, Result};

use crate::{
    mcp_bindings::{self, DiscoveredTool, Protocol, ServerBinding, fail},
    mcp_transport,
};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct McpBindings {
    #[serde(default)]
    pub servers: Vec<ServerBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_funding: Option<crate::mcp_funding::DiscoveryFunding>,
}

impl std::fmt::Debug for McpBindings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpBindings")
            .field("servers", &self.servers.len())
            .finish_non_exhaustive()
    }
}

impl McpBindings {
    pub(crate) fn clear_credentials(&mut self) {
        for server in &mut self.servers {
            server.clear_credentials();
        }
    }

    pub(crate) fn rebind_approved(&self, mut intent: CallIntent) -> Result<CallIntent> {
        if let Some(server) = self
            .servers
            .iter()
            .find(|server| server.label == intent.server.label)
        {
            if server.owner != intent.owner || server.url != intent.server.url {
                return Err(fail("mcp_approval_origin_mismatch"));
            }
            let current = server
                .tools
                .iter()
                .find(|tool| tool.name == intent.tool.name)
                .ok_or_else(|| fail("mcp_tool_not_allowed"))?;
            if !server.allows(intent.caller) {
                return Err(fail("mcp_tool_not_allowed"));
            }
            if current.input_schema != intent.tool.input_schema {
                return Err(fail("mcp_approval_schema_changed"));
            }
            if intent.server.credentials_required && !server.credentials_required {
                return Err(fail("mcp_authorization_required"));
            }
            intent.server = server.for_call();
        } else if intent.server.credentials_required {
            return Err(fail("mcp_authorization_required"));
        }
        Ok(intent)
    }
    pub(crate) fn require_ready(&self) -> Result<()> {
        if self
            .servers
            .iter()
            .any(|server| server.discovery_error.is_some())
        {
            return Err(fail("mcp_discovery_failed"));
        }
        Ok(())
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub(crate) async fn resolve(
        env: &Env,
        owner: &str,
        response: &str,
        declarations: &[Value],
        resource: agents_auth::billing::Resource,
    ) -> Result<Self> {
        let bindings = Self::prepare(env, owner, response, declarations)?;
        if bindings.is_empty() {
            return Ok(Self::default());
        }
        let mut funding = crate::mcp_funding::DiscoveryFunding::start(env, owner, resource).await?;
        let mut resolved = Self::discover(env, owner, bindings).await;
        funding.observe_end();
        let _ = funding.finish(env, owner).await;
        resolved.discovery_funding = Some(funding);
        Ok(resolved)
    }

    /// The caller already owns a funded Worker/DO preparation envelope. This
    /// variant neither creates a second hold nor double-counts its wall interval.
    pub(crate) async fn resolve_in_scope(
        env: &Env,
        owner: &str,
        response: &str,
        declarations: &[Value],
    ) -> Result<Self> {
        let bindings = Self::prepare(env, owner, response, declarations)?;
        Ok(Self::discover(env, owner, bindings).await)
    }

    fn prepare(
        env: &Env,
        owner: &str,
        response: &str,
        declarations: &[Value],
    ) -> Result<Vec<ServerBinding>> {
        if declarations.iter().any(|tool| tool["type"] == "mcp") {
            crate::hosted_tools::require_runtime(env, "mcp")?;
        }
        let declarations = mcp_bindings::declarations(declarations)?;
        if declarations.is_empty() {
            return Ok(vec![]);
        }
        let key = env.secret("DATA_ENCRYPTION_KEY")?.to_string();
        declarations
            .iter()
            .map(|declaration| declaration.bind(owner, response, &key))
            .collect()
    }

    async fn discover(env: &Env, owner: &str, mut bindings: Vec<ServerBinding>) -> Self {
        let mut schema_bytes = 0usize;
        let mut tools = 0usize;
        for binding in &mut bindings {
            if let Err(error) = discover(env, owner, binding).await {
                mcp_transport::discovery_failed(binding, &error);
                binding.tools.clear();
                binding.discovery_error = Some("MCP server discovery failed".into());
            }
            let added = serde_json::to_vec(&binding.tools).map_or(usize::MAX, |bytes| bytes.len());
            if tools.saturating_add(binding.tools.len()) > mcp_bindings::MAX_TOOLS
                || schema_bytes.saturating_add(added) > 512 * 1024
            {
                binding.tools.clear();
                binding.discovery_error = Some("MCP discovery exceeds the tool limits".into());
                continue;
            }
            tools = tools.saturating_add(binding.tools.len());
            schema_bytes = schema_bytes.saturating_add(added);
        }
        Self {
            servers: bindings,
            discovery_funding: None,
        }
    }
    pub(crate) fn request_proof(
        key: &str,
        owner: &str,
        response: &str,
        request: &crate::responses_wire::ResponseRequest,
    ) -> Result<String> {
        crate::mcp_bindings::proof(
            key,
            &format!("request:{owner}:{response}"),
            &serde_json::to_vec(request)?,
        )
    }

    pub(crate) fn declaration_proof(
        key: &str,
        owner: &str,
        response: &str,
        request: &crate::responses_wire::ResponseRequest,
    ) -> Result<String> {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .filter(|tool| tool.kind == "mcp")
            .map(crate::responses_wire::ResponseTool::to_wire_value)
            .collect();
        crate::mcp_bindings::proof(
            key,
            &format!("declarations:{owner}:{response}"),
            &serde_json::to_vec(&tools)?,
        )
    }
    pub(crate) fn sanitize_request(request: &mut crate::responses_wire::ResponseRequest) {
        for tool in request.tools.iter_mut().filter(|tool| tool.kind == "mcp") {
            tool.options.remove("authorization");
            tool.options.remove("headers");
        }
    }
    pub(crate) fn lookup(&self, alias: &str) -> Option<(&ServerBinding, &DiscoveredTool)> {
        self.servers
            .iter()
            .find_map(|server| server.tool(alias).map(|tool| (server, tool)))
    }

    pub(crate) fn definitions(&self, include_deferred: bool) -> Vec<crate::tools::ToolDefinition> {
        self.servers
            .iter()
            .filter(|server| server.direct && (include_deferred || !server.deferred))
            .flat_map(|server| {
                server
                    .tools
                    .iter()
                    .map(|tool| tool.definition(&server.label, &server.alias(&tool.name)))
            })
            .collect()
    }

    pub(crate) fn render_native(&self, declarations: &[Value]) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        for declaration in declarations {
            if declaration["type"] != "mcp" {
                result.push(declaration.clone());
                continue;
            }
            let server = self
                .servers
                .iter()
                .find(|server| declaration["server_label"] == server.label)
                .ok_or_else(|| fail("mcp_server_not_bound"))?;
            if !server.direct || server.deferred {
                continue;
            }
            for tool in &server.tools {
                let definition = tool
                    .definition(&server.label, &server.alias(&tool.name))
                    .function;
                result.push(json!({"type":"function","name":definition.name,"description":definition.description,"parameters":definition.parameters}));
            }
        }
        Ok(result)
    }

    pub(crate) fn rewrite_choice(&self, choice: Value) -> Result<Value> {
        if choice["type"] == "allowed_tools" {
            let mut result = choice;
            let items = result["tools"]
                .as_array_mut()
                .ok_or_else(|| fail("invalid_tool_choice"))?;
            let mut choices = Vec::new();
            for item in std::mem::take(items) {
                let rewritten = self.rewrite_choice(item)?;
                if rewritten["type"] == "allowed_tools" {
                    choices.extend(rewritten["tools"].as_array().cloned().unwrap_or_default());
                } else {
                    choices.push(rewritten);
                }
            }
            *items = choices;
            return Ok(result);
        }
        if choice["type"] != "mcp" {
            return Ok(choice);
        }
        let server = self
            .servers
            .iter()
            .find(|server| choice["server_label"] == server.label)
            .filter(|server| server.direct)
            .ok_or_else(|| fail("mcp_tool_choice_unavailable"))?;
        let tools: Vec<Value> = server
            .tools
            .iter()
            .filter(|tool| choice.get("name").is_none_or(|name| *name == tool.name))
            .map(|tool| json!({"type":"function","name":server.alias(&tool.name)}))
            .collect();
        match tools.len() {
            0 => Err(fail("mcp_tool_choice_unavailable")),
            1 => Ok(tools[0].clone()),
            _ => Ok(json!({"type":"allowed_tools","mode":"required","tools":tools})),
        }
    }

    pub(crate) fn list_items(&self) -> Vec<Value> {
        self.servers
            .iter()
            .map(ServerBinding::public_list)
            .collect()
    }

    pub(crate) fn prepare_call(
        &self,
        owner: &str,
        response: &str,
        execution: &str,
        call: &crate::provider::ToolCall,
    ) -> Result<Option<CallIntent>> {
        self.prepare_call_as(
            owner,
            response,
            execution,
            call,
            crate::hosted_bindings::Caller::Direct,
        )
    }

    pub(crate) fn prepare_call_as(
        &self,
        owner: &str,
        response: &str,
        execution: &str,
        call: &crate::provider::ToolCall,
        caller: crate::hosted_bindings::Caller,
    ) -> Result<Option<CallIntent>> {
        let Some((server, tool)) = self.lookup(&call.function.name) else {
            return Ok(None);
        };
        if server.owner != owner || !server.allows(caller) {
            return Err(fail("mcp_tool_not_bound"));
        }
        if call.function.arguments.len() > crate::mcp_items::MAX_ARGUMENT_BYTES {
            return Err(fail("mcp_arguments_limit"));
        }
        let arguments: Value = serde_json::from_str(&call.function.arguments)
            .map_err(|_| fail("invalid_mcp_arguments"))?;
        if !arguments.is_object() {
            return Err(fail("invalid_mcp_arguments"));
        }
        crate::responses_schema::validate_instance(&tool.input_schema, &arguments)
            .map_err(|_| fail("invalid_mcp_arguments"))?;
        let digest = mcp_bindings::digest(&serde_json::to_vec(&(
            owner, response, execution, &call.id, &tool.name, &arguments,
        ))?);
        Ok(Some(CallIntent {
            owner: owner.into(),
            source_response: response.into(),
            execution: execution.into(),
            native_call_id: call.id.clone(),
            caller,
            server: server.for_call(),
            tool: tool.clone(),
            arguments,
            approval_id: server
                .approval
                .requires(tool)
                .then(|| crate::mcp_items::item_id("mcpr", &digest)),
            item_id: crate::mcp_items::item_id("mcp", &digest),
        }))
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CallIntent {
    #[serde(default = "direct_caller")]
    pub caller: crate::hosted_bindings::Caller,
    pub owner: String,
    pub source_response: String,
    pub execution: String,
    pub native_call_id: String,
    pub server: ServerBinding,
    pub tool: DiscoveredTool,
    pub arguments: Value,
    pub approval_id: Option<String>,
    pub item_id: String,
}

fn direct_caller() -> crate::hosted_bindings::Caller {
    crate::hosted_bindings::Caller::Direct
}

impl std::fmt::Debug for CallIntent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CallIntent")
            .field("item_id", &self.item_id)
            .finish_non_exhaustive()
    }
}

impl CallIntent {
    pub(crate) fn approval_item(&self) -> Option<Value> {
        Some(crate::mcp_items::approval(
            self.approval_id.as_deref()?,
            &self.server.label,
            &self.tool.name,
            &self.arguments.to_string(),
        ))
    }

    pub(crate) fn identity(&self) -> Result<String> {
        Ok(mcp_bindings::digest(&serde_json::to_vec(&(
            &self.owner,
            &self.source_response,
            &self.execution,
            &self.native_call_id,
            self.caller,
            &self.server.label,
            &self.server.url,
            &self.tool.name,
            &self.tool.input_schema,
            &self.arguments,
            &self.approval_id,
            &self.item_id,
        ))?))
    }
    pub(crate) fn for_execution(&self, execution: &str) -> Self {
        let mut intent = self.clone();
        intent.execution = execution.into();
        intent.item_id = crate::mcp_items::item_id(
            "mcp",
            &format!("{}:{}:{}", self.owner, self.item_id, execution),
        );
        intent
    }
    pub(crate) fn result_item(&self, output: Option<&str>, error: Option<Value>) -> Value {
        crate::mcp_items::call(
            &self.item_id,
            &self.server.label,
            &self.tool.name,
            &self.arguments.to_string(),
            self.approval_id.as_deref(),
            output,
            error,
        )
    }

    pub(crate) fn denied(&self) -> Value {
        self.result_item(None, Some(json!({"type":"mcp_protocol_error","code":-32000,"message":"MCP tool call was not approved"})))
    }

    pub(crate) fn native_output(&self, output: &Value) -> Result<Value> {
        let message =
            crate::provider::Message::tool_result_content(&self.native_call_id, output.clone());
        crate::provider::responses::message_items(&[message])
            .map_err(worker::Error::RustError)?
            .pop()
            .ok_or_else(|| fail("mcp_native_result_missing"))
    }
    pub(crate) fn prepare(&self, env: &Env, owner: &str) -> Result<mcp_transport::PreparedRpc> {
        if self.owner != owner || !self.server.allows(self.caller) {
            return Err(fail("mcp_approval_not_found"));
        }
        mcp_transport::prepare(
            env,
            owner,
            &self.server,
            "tools/call",
            json!({"name":self.tool.name,"arguments":self.arguments}),
            &format!("call-{}", mcp_bindings::digest(self.execution.as_bytes())),
            Some(&self.tool.input_schema),
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ApprovalReference {
    pub owner: String,
    pub source_session: String,
    pub source_response: String,
    pub operation: String,
    pub approval_id: String,
    pub native_call_id: String,
}

impl ApprovalReference {
    pub(crate) fn new(
        owner: &str,
        source_session: &str,
        operation: &str,
        intent: &CallIntent,
    ) -> Result<Self> {
        Ok(Self {
            owner: owner.into(),
            source_session: source_session.into(),
            source_response: intent.source_response.clone(),
            operation: operation.into(),
            approval_id: intent
                .approval_id
                .clone()
                .ok_or_else(|| fail("mcp_approval_not_required"))?,
            native_call_id: intent.native_call_id.clone(),
        })
    }

    pub(crate) fn seal(&self, key: &str) -> Result<String> {
        agents_auth::seal(
            key,
            &format!("mcp-approval-ref:v1:{}:{}", self.owner, self.approval_id),
            &serde_json::to_vec(self)?,
        )
        .map_err(|_| fail("mcp_approval_unavailable"))
    }

    pub(crate) fn unseal(key: &str, owner: &str, approval: &str, token: &str) -> Result<Self> {
        if token.len() > 8192 {
            return Err(fail("mcp_approval_not_found"));
        }
        let bytes = agents_auth::unseal(
            key,
            &format!("mcp-approval-ref:v1:{owner}:{approval}"),
            token,
        )
        .map_err(|_| fail("mcp_approval_not_found"))?;
        let reference: Self =
            serde_json::from_slice(&bytes).map_err(|_| fail("mcp_approval_not_found"))?;
        if reference.owner != owner
            || reference.approval_id != approval
            || !reference
                .operation
                .starts_with(&format!("mcp:{}:", reference.source_response))
        {
            return Err(fail("mcp_approval_not_found"));
        }
        Ok(reference)
    }

    pub(crate) fn matches(&self, intent: &CallIntent) -> bool {
        intent.owner == self.owner
            && intent.source_response == self.source_response
            && intent.approval_id.as_deref() == Some(&self.approval_id)
            && intent.native_call_id == self.native_call_id
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ApprovedCall {
    pub reference: ApprovalReference,
    pub capability: String,
    pub intent: CallIntent,
    pub approved: bool,
    pub completed: bool,
}
async fn discover(env: &Env, owner: &str, server: &mut ServerBinding) -> Result<()> {
    let request_id = format!(
        "discover-{}",
        mcp_bindings::digest(format!("{}:{}", server.response, server.label).as_bytes())
    );
    let outcome = mcp_transport::prepare(
        env,
        owner,
        server,
        "server/discover",
        json!({}),
        &request_id,
        None,
    )?
    .dispatch()
    .await?;
    if let Some(result) = outcome.result {
        let discovery: rmcp::model::DiscoverResult =
            serde_json::from_value(result).map_err(|_| fail("mcp_invalid_discovery"))?;
        if !discovery
            .supported_versions
            .iter()
            .any(|version| version.to_string() == Protocol::Current.version())
        {
            return Err(fail("mcp_protocol_version_unavailable"));
        }
    } else if outcome
        .failure
        .as_ref()
        .is_some_and(|failure| failure.permits_legacy_initialize("server/discover"))
    {
        server.protocol = Protocol::Legacy;
        let initialized = mcp_transport::prepare(
            env,
            owner,
            server,
            "initialize",
            json!({
                "protocolVersion":server.protocol.version(),"capabilities":{},
                "clientInfo":{"name":"magents","version":env!("CARGO_PKG_VERSION")}
            }),
            &format!("initialize-{request_id}"),
            None,
        )?
        .dispatch()
        .await?;
        let result = initialized
            .result
            .ok_or_else(|| fail("mcp_initialize_failed"))?;
        let result: rmcp::model::InitializeResult =
            serde_json::from_value(result).map_err(|_| fail("mcp_invalid_initialize"))?;
        server.protocol = Protocol::negotiated(&result.protocol_version.to_string())
            .filter(|version| *version != Protocol::Current)
            .ok_or_else(|| fail("mcp_protocol_version_unavailable"))?;
        server.session_id = initialized.session_id;
        if mcp_transport::notification(env, owner, server)?
            .dispatch()
            .await?
            .failure
            .is_some()
        {
            return Err(fail("mcp_initialize_failed"));
        }
    } else {
        return Err(fail("mcp_discovery_failed"));
    }
    let mut cursor: Option<String> = None;
    for page in 0..16 {
        let params = cursor
            .as_ref()
            .map(|cursor| json!({"cursor":cursor}))
            .unwrap_or_else(|| json!({}));
        let outcome = mcp_transport::prepare(
            env,
            owner,
            server,
            "tools/list",
            params,
            &format!("list-{request_id}-{page}"),
            None,
        )?
        .dispatch()
        .await?;
        let result = outcome.result.ok_or_else(|| fail("mcp_discovery_failed"))?;
        let (tools, next) = mcp_transport::tools(result)?;
        server.insert_tools(tools)?;
        if next.is_none() {
            return Ok(());
        }
        if cursor == next {
            return Err(fail("mcp_pagination_cycle"));
        }
        cursor = next;
    }
    Err(fail("mcp_pagination_limit"))
}
