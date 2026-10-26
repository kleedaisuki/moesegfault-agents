//! Program authority and a durable nested-call journal. JavaScript may request
//! work, but cannot supply its own caller identity, owner, tool grants or clock.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const PROGRAM_ALIAS: &str = "__agents_program";
pub(crate) const MAX_CALLS: usize = 256;
const MAX_CODE: usize = 64 * 1024;
const MAX_VALUE: usize = 1024 * 1024;
const MAX_JOURNAL: usize = 4 * 1024 * 1024;
const MAX_POLICY: usize = 256 * 1024;
const MAX_TOOLS: usize = 128;
const MAX_NAMESPACE_DEPTH: usize = 8;
const MAX_LEASE_MS: u64 = 3_600_000;

type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub(crate) struct ToolPath(Vec<String>);

impl ToolPath {
    pub(crate) fn new(path: Vec<String>) -> Result<Self> {
        if path.is_empty()
            || path.len() > MAX_NAMESPACE_DEPTH + 1
            || path.iter().any(|part| !identifier(part))
        {
            return Err("invalid_programmatic_tool_path");
        }
        Ok(Self(path))
    }

    pub(crate) fn name(&self) -> &str {
        self.0.last().expect("validated nonempty tool path")
    }

    pub(crate) fn namespace(&self) -> Option<String> {
        (self.0.len() > 1).then(|| self.0[..self.0.len() - 1].join("."))
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExecutionTarget {
    HarnessTime,
    ClientFunction,
    ClientCustom,
    /// The owning adapter resolves this opaque capability. It is not a URL,
    /// credential or a model-supplied choice of server authority.
    Adapter {
        capability: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolGrant {
    path: ToolPath,
    target: ExecutionTarget,
    declaration: Value,
}

impl ToolGrant {
    pub(crate) fn adapter(path: ToolPath, capability: String, declaration: Value) -> Result<Self> {
        if !identifier(&capability) || !programmatic_allowed(&declaration)? {
            return Err("programmatic_tool_not_allowed");
        }
        Ok(Self {
            path,
            target: ExecutionTarget::Adapter { capability },
            // MCP authorization belongs to the owning encrypted adapter, not
            // another plaintext copy in a program journal or model interface.
            declaration: declaration_view(&declaration),
        })
    }

    fn validate_input(&self, input: &Value) -> Result<()> {
        bounded(input)?;
        match self.target {
            ExecutionTarget::ClientCustom => {
                let input = input.as_str().ok_or("invalid_programmatic_custom_input")?;
                crate::custom_tool_formats::validate_completed_input(&self.declaration, input)
                    .map_err(|_| "programmatic_custom_format_violation")
            }
            _ => {
                if !input.is_object() {
                    return Err("invalid_programmatic_function_arguments");
                }
                if self.declaration["strict"] == true
                    || matches!(self.target, ExecutionTarget::HarnessTime)
                {
                    crate::responses_schema::validate_instance(
                        &self.declaration["parameters"],
                        input,
                    )
                    .map_err(|_| "programmatic_argument_schema_violation")?;
                }
                Ok(())
            }
        }
    }

    fn validate_result(&self, value: &Value) -> Result<()> {
        bounded(value)?;
        if let Some(schema) = self
            .declaration
            .get("output_schema")
            .filter(|schema| !schema.is_null())
        {
            crate::responses_schema::validate_instance(schema, value)
                .map_err(|_| "programmatic_result_schema_violation")?;
        }
        Ok(())
    }
}

fn declaration_view(declaration: &Value) -> Value {
    let mut fields = serde_json::Map::new();
    for name in [
        "type",
        "name",
        "description",
        "parameters",
        "strict",
        "format",
        "output_schema",
        "allowed_callers",
        "defer_loading",
    ] {
        if let Some(value) = declaration.get(name) {
            fields.insert(name.into(), value.clone());
        }
    }
    Value::Object(fields)
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ProgramPolicy(Vec<ToolGrant>);

impl ProgramPolicy {
    pub(crate) fn server_only(&self) -> bool {
        self.0.iter().all(|grant| {
            matches!(
                grant.target,
                ExecutionTarget::HarnessTime | ExecutionTarget::Adapter { .. }
            )
        })
    }

    pub(crate) fn resolve_bound(
        declarations: &[Value],
        server_time: bool,
        mcp: &crate::mcp_runtime::McpBindings,
        registry: Option<&crate::tool_discovery::ToolRegistry>,
    ) -> Result<Self> {
        let declarations = registry.map_or_else(
            || declarations.to_vec(),
            |registry| registry.programmatic_declarations(),
        );
        let mut policy = Self::resolve(&declarations, server_time)?;
        for server in mcp.servers.iter().filter(|server| server.programmatic) {
            for tool in &server.tools {
                let path = ToolPath::new(vec![server.label.clone(), tool.name.clone()])?;
                let declaration = json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.input_schema,"strict":true,"allowed_callers":["programmatic"]});
                if server.deferred && !registry.is_some_and(|registry| {
                    registry.declares_public_call(
                        &json!({"type":"function_call","namespace":server.label,"name":tool.name}),
                    )
                }) {
                    continue;
                }
                policy.0.retain(|grant| grant.path != path);
                policy.insert(ToolGrant::adapter(
                    path,
                    server.alias(&tool.name),
                    declaration,
                )?)?;
            }
        }
        if !policy.server_only() {
            return Err("programmatic_client_continuation_unavailable");
        }
        Ok(policy)
    }

    pub(crate) fn requested_alias(declarations: &[Value], name: &str) -> bool {
        name == PROGRAM_ALIAS && Self::requested(declarations) == Ok(true)
    }
    pub(crate) fn requested(declarations: &[Value]) -> Result<bool> {
        let count = declarations
            .iter()
            .filter(|tool| tool["type"] == "programmatic_tool_calling")
            .count();
        if count > 1 {
            return Err("duplicate_programmatic_tool");
        }
        Ok(count == 1)
    }

    /// The actor passes its authenticated loaded declaration snapshot. Deferred
    /// definitions that have not actually been loaded remain unavailable.
    pub(crate) fn resolve(declarations: &[Value], server_time: bool) -> Result<Self> {
        let mut policy = Self::default();
        let mut budget = MAX_TOOLS;
        collect_grants(declarations, &[], &mut budget, &mut policy)?;
        if server_time {
            policy.insert(ToolGrant {
                path: ToolPath::new(vec!["time".into()])?,
                target: ExecutionTarget::HarnessTime,
                declaration: json!({"type":"function","name":"time","strict":true,"parameters":{"type":"object","properties":{},"additionalProperties":false},"allowed_callers":["programmatic"]}),
            })?;
        }
        Ok(policy)
    }

    pub(crate) fn insert(&mut self, grant: ToolGrant) -> Result<()> {
        if self.0.len() >= MAX_TOOLS || self.0.iter().any(|old| old.path == grant.path) {
            return Err("programmatic_tool_conflict");
        }
        self.0.push(grant);
        if encoded_len(&self.0)? > MAX_POLICY {
            self.0.pop();
            return Err("programmatic_policy_limit");
        }
        self.0.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(())
    }

    fn lookup(&self, path: &ToolPath) -> Result<&ToolGrant> {
        self.0
            .iter()
            .find(|grant| &grant.path == path)
            .ok_or("programmatic_tool_not_allowed")
    }

    pub(crate) fn runtime_tools(&self) -> Value {
        Value::Array(self.0.iter().map(|grant| {
            json!({"path":grant.path,"kind":if matches!(grant.target,ExecutionTarget::ClientCustom) {"custom"} else {"function"}})
        }).collect())
    }

    pub(crate) fn definition(&self) -> crate::tools::ToolDefinition {
        let interfaces:Vec<_>=self.0.iter().map(|grant|{
            let mut value=json!({"path":grant.path,"type":if matches!(grant.target,ExecutionTarget::ClientCustom){"custom"}else{"function"}});
            for key in ["description","parameters","format","output_schema"] {
                if let Some(field)=grant.declaration.get(key) {value[key]=field.clone();}
            }
            value
        }).collect();
        crate::tools::ToolDefinition {
            kind: "function".into(),
            function: crate::tools::FunctionDefinition {
                name: PROGRAM_ALIAS.into(),
                description: format!(
                    "Run one JavaScript program with top-level await. Use tools.<name>(arguments) only from the admitted interface below. Emit a final result with text(value). There is no Node, filesystem, subprocess, console or direct network. Available interfaces: {}",
                    json!(interfaces)
                ),
                parameters: json!({"type":"object","properties":{"code":{"type":"string","minLength":1,"maxLength":MAX_CODE}},"required":["code"],"additionalProperties":false}),
                strict: Some(true),
            },
        }
    }

    pub(crate) fn render_native(&self, declarations: &[Value]) -> Result<Vec<Value>> {
        if !Self::requested(declarations)? {
            return Ok(declarations.to_vec());
        }
        let definition = self.definition().function;
        Ok(declarations.iter().map(|tool|if tool["type"]=="programmatic_tool_calling" {
            json!({"type":"function","name":definition.name,"description":definition.description,"parameters":definition.parameters,"strict":definition.strict})
        }else{tool.clone()}).collect())
    }

    pub(crate) fn rewrite_choice(&self, mut choice: Value) -> Result<Value> {
        if choice["type"] == "programmatic_tool_calling" {
            return Ok(json!({"type":"function","name":PROGRAM_ALIAS}));
        }
        if choice["type"] == "allowed_tools" {
            let tools = choice["tools"]
                .as_array_mut()
                .ok_or("invalid_programmatic_tool_choice")?;
            for tool in tools {
                *tool = self.rewrite_choice(std::mem::take(tool))?;
            }
        }
        Ok(choice)
    }
}

fn collect_grants(
    declarations: &[Value],
    namespace: &[String],
    budget: &mut usize,
    policy: &mut ProgramPolicy,
) -> Result<()> {
    if namespace.len() > MAX_NAMESPACE_DEPTH || declarations.len() > *budget {
        return Err("programmatic_tool_limit");
    }
    for declaration in declarations {
        *budget = budget.checked_sub(1).ok_or("programmatic_tool_limit")?;
        let kind = declaration["type"]
            .as_str()
            .ok_or("invalid_programmatic_tool")?;
        if kind == "namespace" {
            let mut path = namespace.to_vec();
            path.push(
                declaration["name"]
                    .as_str()
                    .filter(|name| identifier(name))
                    .ok_or("invalid_programmatic_tool_path")?
                    .into(),
            );
            let tools = declaration["tools"]
                .as_array()
                .ok_or("invalid_programmatic_tool")?;
            collect_grants(tools, &path, budget, policy)?;
            continue;
        }
        if !matches!(kind, "function" | "custom")
            || declaration["defer_loading"] == true
            || !programmatic_allowed(declaration)?
        {
            continue;
        }
        let mut path = namespace.to_vec();
        path.push(
            declaration["name"]
                .as_str()
                .ok_or("invalid_programmatic_tool_path")?
                .into(),
        );
        let path = ToolPath::new(path)?;
        if path.0.iter().any(|part| part.starts_with("__agents_")) {
            return Err("reserved_programmatic_tool_name");
        }
        if kind == "function" {
            crate::responses_schema::validate_schema(&declaration["parameters"])
                .map_err(|_| "invalid_programmatic_argument_schema")?;
        } else {
            crate::custom_tool_formats::validate_tool(declaration)
                .map_err(|_| "invalid_programmatic_custom_format")?;
        }
        if let Some(schema) = declaration
            .get("output_schema")
            .filter(|schema| !schema.is_null())
        {
            crate::responses_schema::validate_schema(schema)
                .map_err(|_| "invalid_programmatic_output_schema")?;
        }
        policy.insert(ToolGrant {
            path,
            target: if kind == "custom" {
                ExecutionTarget::ClientCustom
            } else {
                ExecutionTarget::ClientFunction
            },
            declaration: declaration.clone(),
        })?;
    }
    Ok(())
}

fn programmatic_allowed(declaration: &Value) -> Result<bool> {
    let Some(callers) = declaration
        .get("allowed_callers")
        .filter(|value| !value.is_null())
    else {
        return Ok(false);
    };
    let callers = callers
        .as_array()
        .filter(|callers| callers.len() <= 2)
        .ok_or("invalid_programmatic_callers")?;
    let mut seen = BTreeSet::new();
    for caller in callers {
        let caller = caller
            .as_str()
            .filter(|caller| matches!(*caller, "direct" | "programmatic"))
            .ok_or("invalid_programmatic_callers")?;
        if !seen.insert(caller) {
            return Err("invalid_programmatic_callers");
        }
    }
    Ok(seen.contains("programmatic"))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProgramScope {
    pub owner: String,
    pub session: String,
    pub response: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum CallState {
    Prepared,
    Dispatched,
    ClientPending,
    Completed {
        result: Value,
        resolution_order: u32,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallRecord {
    sequence: u32,
    path: ToolPath,
    arguments: Value,
    call_id: String,
    item_id: String,
    state: CallState,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ProgramPhase {
    Ready,
    Running,
    Waiting,
    Completed { result: String },
    Incomplete { code: String },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProgramState {
    scope: ProgramScope,
    item_id: String,
    call_id: String,
    fingerprint: String,
    code: String,
    code_hash: String,
    policy: ProgramPolicy,
    expires_at_ms: u64,
    phase: ProgramPhase,
    calls: Vec<CallRecord>,
    resolution_count: u32,
    #[serde(default)]
    runtime_acknowledged: u32,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    revoked: BTreeSet<ToolPath>,
}

/// This type has no Deserialize implementation. Only a validated, owner-bound
/// program journal can mint authority for the existing actor dispatcher.
pub(crate) struct AuthorizedInvocation {
    sequence: u32,
    parent_call_id: String,
    call_id: String,
    execution_key: String,
    item_id: String,
    target: ExecutionTarget,
    path: ToolPath,
    arguments: Value,
    owner: String,
    session: String,
    expires_at_ms: u64,
}

impl AuthorizedInvocation {
    pub(crate) fn call_item(&self) -> Result<Value> {
        let mut item = json!({"id":self.item_id,"type":"function_call","call_id":self.call_id,"name":self.path.name(),"arguments":serde_json::to_string(&self.arguments).map_err(|_|"invalid_programmatic_value")?,"status":"completed","caller":{"type":"program","caller_id":self.parent_call_id}});
        if let Some(namespace) = self.path.namespace() {
            item["namespace"] = json!(namespace);
        }
        Ok(item)
    }

    pub(crate) fn result_item(&self, output: &Value) -> Result<Value> {
        let mut item = json!({"id":format!("presult_{}",digest(&self.call_id)?),"type":"function_call_output","call_id":self.call_id,"output":serde_json::to_string(output).map_err(|_|"invalid_programmatic_value")?,"status":"completed","caller":{"type":"program","caller_id":self.parent_call_id}});
        if let Some(namespace) = self.path.namespace() {
            item["namespace"] = json!(namespace);
        }
        Ok(item)
    }
    pub(crate) fn sequence(&self) -> u32 {
        self.sequence
    }
    pub(crate) fn parent_call_id(&self) -> &str {
        &self.parent_call_id
    }
    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }
    pub(crate) fn execution_key(&self) -> &str {
        &self.execution_key
    }
    pub(crate) fn target(&self) -> &ExecutionTarget {
        &self.target
    }
    pub(crate) fn path(&self) -> &ToolPath {
        &self.path
    }
    pub(crate) fn arguments(&self) -> &Value {
        &self.arguments
    }
    pub(crate) fn matches_scope(&self, owner: &str, session: &str) -> bool {
        self.owner == owner && self.session == session
    }
    pub(crate) fn verify_scope(&self, owner: &str, session: &str, now_ms: u64) -> Result<()> {
        if !self.matches_scope(owner, session) || now_ms >= self.expires_at_ms {
            return Err("program_authority_expired");
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeCall {
    pub sequence: u32,
    pub target: Vec<String>,
    pub arguments: Value,
}

/// Consumed by one native origin request. No deserializer or public constructor
/// can turn an incoming program item into permission to execute its source.
pub(crate) struct ProgramStart {
    message: Value,
    worker_identity: String,
    expires_at_ms: u64,
}

impl ProgramStart {
    pub(crate) fn message(&self) -> &Value {
        &self.message
    }
    pub(crate) fn worker_identity(&self) -> &str {
        &self.worker_identity
    }
    pub(crate) fn verify_time(&self, now_ms: u64) -> Result<()> {
        if now_ms >= self.expires_at_ms {
            return Err("program_not_active");
        }
        Ok(())
    }
}

impl ProgramState {
    pub(crate) fn new(
        scope: ProgramScope,
        execution: &str,
        code: String,
        policy: ProgramPolicy,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Self> {
        if !identifier(execution)
            || scope.owner.is_empty()
            || scope.session.is_empty()
            || scope.response.is_empty()
            || code.is_empty()
            || code.len() > MAX_CODE
            || code.contains('\0')
            || lease_ms == 0
            || lease_ms > MAX_LEASE_MS
        {
            return Err("invalid_program_admission");
        }
        let identity = digest(&(
            scope.owner.as_str(),
            scope.session.as_str(),
            scope.response.as_str(),
            execution,
        ))?;
        Ok(Self {
            scope,
            item_id: format!("prog_{identity}"),
            call_id: format!("call_prog_{identity}"),
            fingerprint: agents_auth::random_token()
                .map_err(|_| "program_randomness_unavailable")?,
            code_hash: digest(&code)?,
            code,
            policy,
            expires_at_ms: now_ms
                .checked_add(lease_ms)
                .ok_or("invalid_program_admission")?,
            phase: ProgramPhase::Ready,
            calls: vec![],
            resolution_count: 0,
            runtime_acknowledged: 0,
            revoked: BTreeSet::new(),
        })
    }

    pub(crate) fn verify_scope(&self, owner: &str, session: &str, fingerprint: &str) -> Result<()> {
        if self.scope.owner != owner
            || self.scope.session != session
            || !agents_auth::constant_eq(&self.fingerprint, fingerprint)
            || digest(&self.code)? != self.code_hash
        {
            return Err("program_authority_mismatch");
        }
        Ok(())
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }
    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn ready(&self) -> bool {
        matches!(self.phase, ProgramPhase::Ready)
    }
    pub(crate) fn known_result(&self, sequence: u32) -> Option<&Value> {
        match &self.calls.get(sequence as usize)?.state {
            CallState::Completed { result, .. } => Some(result),
            _ => None,
        }
    }
    pub(crate) fn fail(&mut self, code: &'static str) {
        if !matches!(self.phase, ProgramPhase::Completed { .. }) {
            self.phase = ProgramPhase::Incomplete { code: code.into() };
        }
    }

    /// Continuation cannot gain permissions from a new declaration or retain a
    /// removed declaration. Old receipts still use the original result schema.
    pub(crate) fn restrict_new_calls(&mut self, current: &ProgramPolicy) {
        for grant in &self.policy.0 {
            if !current.0.iter().any(|candidate| {
                candidate.path == grant.path
                    && candidate.target == grant.target
                    && candidate.declaration == grant.declaration
            }) {
                self.revoked.insert(grant.path.clone());
            }
        }
    }

    pub(crate) fn worker_id(&self) -> Result<String> {
        // Per-program identity, never a cache shared across principals or runs.
        Ok(format!(
            "program-{}",
            digest(&(
                &self.scope.owner,
                &self.scope.session,
                &self.call_id,
                &self.fingerprint,
                &self.code_hash
            ))?
        ))
    }

    pub(crate) fn runtime_start(&self) -> Value {
        json!({"op":"start","program_id":self.call_id,"tools":self.policy.runtime_tools(),"max_calls":MAX_CALLS,"max_output_bytes":MAX_VALUE})
    }

    pub(crate) async fn commit_start(
        &mut self,
        storage: &worker::Storage,
        key: &str,
        now_ms: u64,
    ) -> worker::Result<ProgramStart> {
        self.require_active(now_ms)
            .map_err(|code| worker::Error::RustError(code.into()))?;
        if !matches!(self.phase, ProgramPhase::Ready) {
            return Err(worker::Error::RustError("program_already_started".into()));
        }
        let start = ProgramStart {
            message: self.runtime_start(),
            worker_identity: self
                .worker_id()
                .map_err(|code| worker::Error::RustError(code.into()))?,
            expires_at_ms: self.expires_at_ms,
        };
        self.phase = ProgramPhase::Running;
        let result = async {
            let bytes = serde_json::to_vec(self)?;
            crate::session_storage::write_chunks(storage, key, &bytes).await
        }
        .await;
        if let Err(error) = result {
            self.phase = ProgramPhase::Ready;
            return Err(error);
        }
        Ok(start)
    }

    pub(crate) fn runtime_resume(&self) -> Value {
        let results: Vec<_> = self.calls.iter().filter_map(|call| {
            let CallState::Completed{result,resolution_order}=&call.state else {return None;};
            if *resolution_order < self.runtime_acknowledged {return None;}
            Some(json!({"sequence":call.sequence,"result":result,"resolution_order":resolution_order}))
        }).collect();
        json!({"op":"resume","program_id":self.call_id,"results":results})
    }

    pub(crate) fn resolution_count(&self) -> u32 {
        self.resolution_count
    }

    /// The actor retains this pre-dispatch count; JavaScript cannot acknowledge
    /// its own result journal or remove its authority to recover a known result.
    pub(crate) fn acknowledge_runtime(&mut self, through: u32) -> Result<()> {
        if through > self.resolution_count {
            return Err("program_runtime_ack_conflict");
        }
        self.runtime_acknowledged = self.runtime_acknowledged.max(through);
        Ok(())
    }

    /// Persist the returned state before sending the message. Once a start was
    /// attempted, even a crash before its acknowledgement permits only resume,
    /// never another execution of the source. A fresh isolate then reports loss.
    pub(crate) fn begin_runtime_turn(&mut self, now_ms: u64) -> Result<Value> {
        self.require_active(now_ms)?;
        if self.calls.iter().any(|call| {
            matches!(
                call.state,
                CallState::Prepared | CallState::Dispatched | CallState::ClientPending
            )
        }) {
            return Err("program_waiting_for_tool");
        }
        let message = if matches!(self.phase, ProgramPhase::Ready) {
            self.runtime_start()
        } else {
            self.runtime_resume()
        };
        self.phase = ProgramPhase::Running;
        Ok(message)
    }

    /// Validate the entire batch before adding any call. Failure cannot leave a
    /// valid prefix ready to execute while a later forbidden call was rejected.
    pub(crate) fn admit_calls(&mut self, calls: &[RuntimeCall], now_ms: u64) -> Result<()> {
        self.require_active(now_ms)?;
        if calls.len() > 64 {
            return Err("program_call_limit");
        }
        let mut prepared = Vec::new();
        for call in calls {
            let path = ToolPath::new(call.target.clone())?;
            if let Some(old) = self.calls.get(call.sequence as usize) {
                if old.path != path || old.arguments != call.arguments {
                    return Err("program_replay_conflict");
                }
                continue;
            }
            if self.revoked.contains(&path) {
                return Err("programmatic_tool_revoked");
            }
            let grant = self.policy.lookup(&path)?;
            grant.validate_input(&call.arguments)?;
            if call.sequence as usize != self.calls.len() + prepared.len() {
                return Err("program_sequence_conflict");
            }
            let id = digest(&(&self.call_id, call.sequence, &path, &call.arguments))?;
            prepared.push(CallRecord {
                sequence: call.sequence,
                path,
                arguments: call.arguments.clone(),
                call_id: format!("call_{id}"),
                item_id: format!("pcall_{id}"),
                state: CallState::Prepared,
            });
        }
        if self.calls.len().saturating_add(prepared.len()) > MAX_CALLS {
            return Err("program_call_limit");
        }
        if encoded_len(&self.calls)?.saturating_add(encoded_len(&prepared)?) > MAX_JOURNAL {
            return Err("program_journal_limit");
        }
        self.calls.extend(prepared);
        self.phase = ProgramPhase::Waiting;
        Ok(())
    }

    /// Persist this mutation BEFORE invoking a server tool. Dispatched records
    /// never mint a second execution authority after a crash.
    fn begin_dispatch(&mut self, sequence: u32, now_ms: u64) -> Result<AuthorizedInvocation> {
        self.require_active(now_ms)?;
        let call = self
            .calls
            .get_mut(sequence as usize)
            .ok_or("program_call_not_found")?;
        if self.revoked.contains(&call.path) {
            return Err("programmatic_tool_revoked");
        }
        if !matches!(call.state, CallState::Prepared) {
            return Err("program_call_already_dispatched");
        }
        let grant = self.policy.lookup(&call.path)?;
        if matches!(
            grant.target,
            ExecutionTarget::ClientFunction | ExecutionTarget::ClientCustom
        ) {
            return Err("program_call_client_owned");
        }
        call.state = CallState::Dispatched;
        Ok(AuthorizedInvocation {
            sequence,
            parent_call_id: self.call_id.clone(),
            call_id: call.call_id.clone(),
            execution_key: format!("program:{}:{}", self.call_id, sequence),
            item_id: call.item_id.clone(),
            target: grant.target.clone(),
            path: call.path.clone(),
            arguments: call.arguments.clone(),
            owner: self.scope.owner.clone(),
            session: self.scope.session.clone(),
            expires_at_ms: self.expires_at_ms,
        })
    }

    /// The only public minting path returns authority after its write-ahead
    /// state is durable. A failed write returns no proof and performs no effect.
    pub(crate) async fn commit_dispatch(
        &mut self,
        storage: &worker::Storage,
        key: &str,
        sequence: u32,
        now_ms: u64,
    ) -> worker::Result<AuthorizedInvocation> {
        let proof = self
            .begin_dispatch(sequence, now_ms)
            .map_err(|code| worker::Error::RustError(code.into()))?;
        let result = async {
            let bytes = serde_json::to_vec(self)?;
            crate::session_storage::write_chunks(storage, key, &bytes).await
        }
        .await;
        if let Err(error) = result {
            self.calls[sequence as usize].state = CallState::Prepared;
            return Err(error);
        }
        proof
            .verify_scope(
                &self.scope.owner,
                &self.scope.session,
                crate::util::now_ms(),
            )
            .map_err(|code| worker::Error::RustError(code.into()))?;
        Ok(proof)
    }

    pub(crate) fn expose_client_call(&mut self, sequence: u32, now_ms: u64) -> Result<Value> {
        self.require_active(now_ms)?;
        let call = self
            .calls
            .get_mut(sequence as usize)
            .ok_or("program_call_not_found")?;
        if self.revoked.contains(&call.path) && matches!(call.state, CallState::Prepared) {
            return Err("programmatic_tool_revoked");
        }
        let grant = self.policy.lookup(&call.path)?;
        let custom = matches!(grant.target, ExecutionTarget::ClientCustom);
        if !custom && !matches!(grant.target, ExecutionTarget::ClientFunction) {
            return Err("program_call_not_client_owned");
        }
        if !matches!(call.state, CallState::Prepared | CallState::ClientPending) {
            return Err("program_call_already_dispatched");
        }
        call.state = CallState::ClientPending;
        let mut value = json!({"id":call.item_id,"type":if custom {"custom_tool_call"} else {"function_call"},"call_id":call.call_id,"name":call.path.name(),"status":"completed","caller":{"type":"program","caller_id":self.call_id}});
        if let Some(namespace) = call.path.namespace() {
            value["namespace"] = json!(namespace);
        }
        value[if custom { "input" } else { "arguments" }] = if custom {
            call.arguments.clone()
        } else {
            json!(serde_json::to_string(&call.arguments).map_err(|_| "invalid_programmatic_value")?)
        };
        Ok(value)
    }

    pub(crate) fn complete_call(&mut self, sequence: u32, result: Value) -> Result<()> {
        let journal_bytes = encoded_len(&self.calls)?;
        let call = self
            .calls
            .get_mut(sequence as usize)
            .ok_or("program_call_not_found")?;
        self.policy.lookup(&call.path)?.validate_result(&result)?;
        if let CallState::Completed { result: old, .. } = &call.state {
            return if old == &result {
                Ok(())
            } else {
                Err("program_result_conflict")
            };
        }
        if !matches!(call.state, CallState::Dispatched | CallState::ClientPending) {
            return Err("program_result_without_dispatch");
        }
        if journal_bytes.saturating_add(encoded_len(&result)?) > MAX_JOURNAL {
            return Err("program_journal_limit");
        }
        call.state = CallState::Completed {
            result,
            resolution_order: self.resolution_count,
        };
        self.resolution_count = self
            .resolution_count
            .checked_add(1)
            .ok_or("program_call_limit")?;
        Ok(())
    }

    pub(crate) fn accept_client_output(&mut self, item: &Value) -> Result<()> {
        let id = item["call_id"]
            .as_str()
            .ok_or("invalid_programmatic_output")?;
        let sequence = self
            .calls
            .iter()
            .find(|call| call.call_id == id)
            .ok_or("program_call_not_found")?
            .sequence;
        let call = &self.calls[sequence as usize];
        let grant = self.policy.lookup(&call.path)?;
        let expected = if matches!(grant.target, ExecutionTarget::ClientCustom) {
            "custom_tool_call_output"
        } else {
            "function_call_output"
        };
        if item["type"] != expected
            || item["caller"]["type"] != "program"
            || item["caller"]["caller_id"] != self.call_id
            || item
                .get("status")
                .is_some_and(|status| status != "completed")
        {
            return Err("program_output_caller_mismatch");
        }
        if !matches!(
            call.state,
            CallState::ClientPending | CallState::Completed { .. }
        ) {
            return Err("program_call_not_client_pending");
        }
        let output = item["output"]
            .as_str()
            .ok_or("invalid_programmatic_output")?;
        let result = serde_json::from_str(output).unwrap_or_else(|_| Value::String(output.into()));
        self.complete_call(sequence, result)
    }

    pub(crate) fn accept_client_outputs(&mut self, items: &[Value]) -> Result<()> {
        if items.len() > 64 {
            return Err("program_call_limit");
        }
        let mut next = self.clone();
        for item in items {
            next.accept_client_output(item)?;
        }
        *self = next;
        Ok(())
    }

    pub(crate) fn finish(&mut self, result: String) -> Result<()> {
        if let ProgramPhase::Completed { result: old } = &self.phase {
            return if old == &result {
                Ok(())
            } else {
                Err("program_result_conflict")
            };
        }
        if matches!(self.phase, ProgramPhase::Incomplete { .. }) {
            return Err("program_not_active");
        }
        if result.len() > MAX_VALUE
            || self
                .calls
                .iter()
                .any(|call| !matches!(call.state, CallState::Completed { .. }))
        {
            return Err("program_completed_with_pending_calls");
        }
        self.phase = ProgramPhase::Completed { result };
        Ok(())
    }

    pub(crate) fn runtime_lost(&mut self) {
        // A lost V8 stack is not permission to rerun an ambiguous program.
        if !matches!(self.phase, ProgramPhase::Completed { .. }) {
            self.phase = ProgramPhase::Incomplete {
                code: "program_runtime_expired".into(),
            };
        }
    }

    pub(crate) fn cancel(&mut self) {
        if !matches!(self.phase, ProgramPhase::Completed { .. }) {
            self.phase = ProgramPhase::Incomplete {
                code: "program_cancelled".into(),
            };
        }
    }

    fn require_active(&self, now_ms: u64) -> Result<()> {
        if now_ms >= self.expires_at_ms
            || matches!(
                self.phase,
                ProgramPhase::Completed { .. } | ProgramPhase::Incomplete { .. }
            )
        {
            return Err("program_not_active");
        }
        Ok(())
    }

    pub(crate) fn program_item(&self) -> Value {
        json!({"type":"program","id":self.item_id,"call_id":self.call_id,"code":self.code,"fingerprint":self.fingerprint})
    }

    pub(crate) fn output_item(&self) -> Option<Value> {
        let (status, result) = match &self.phase {
            ProgramPhase::Completed { result } => ("completed", result.clone()),
            ProgramPhase::Incomplete { code } => {
                ("incomplete", json!({"error":{"code":code}}).to_string())
            }
            _ => return None,
        };
        Some(
            json!({"type":"program_output","id":format!("progout_{}",self.item_id.strip_prefix("prog_").expect("owned program id")),"call_id":self.call_id,"result":result,"status":status}),
        )
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn bounded(value: &Value) -> Result<()> {
    if encoded_len(value)? > MAX_VALUE {
        return Err("programmatic_value_limit");
    }
    Ok(())
}

fn encoded_len<T: Serialize>(value: &T) -> Result<usize> {
    Ok(serde_json::to_vec(value)
        .map_err(|_| "invalid_programmatic_value")?
        .len())
}

fn digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(|_| "invalid_programmatic_value")?)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ProgramState {
        let policy=ProgramPolicy::resolve(&[json!({"type":"function","name":"inventory","allowed_callers":["programmatic"],"strict":true,"parameters":{"type":"object","properties":{"sku":{"type":"string"}},"required":["sku"],"additionalProperties":false},"output_schema":{"type":"object","properties":{"stock":{"type":"integer"}},"required":["stock"],"additionalProperties":false}})],true).unwrap();
        ProgramState::new(
            ProgramScope {
                owner: "owner".into(),
                session: "session".into(),
                response: "response".into(),
            },
            "operation",
            "text(await tools.time({}))".into(),
            policy,
            100,
            10000,
        )
        .unwrap()
    }

    #[test]
    fn program_authority_is_owner_and_fingerprint_bound_not_a_claimed_caller_id() {
        let state = state();
        assert!(
            state
                .verify_scope("owner", "session", state.fingerprint())
                .is_ok()
        );
        assert!(
            state
                .verify_scope("other", "session", state.fingerprint())
                .is_err()
        );
        assert!(
            state
                .verify_scope("owner", "session", "nonempty-but-forged")
                .is_err()
        );
        let encoded = serde_json::to_string(&state).unwrap();
        let restored: ProgramState = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored.worker_id().unwrap(), state.worker_id().unwrap());
    }

    #[test]
    fn unallowed_or_invalid_batch_admits_zero_effects() {
        let mut state = state();
        let batch = [
            RuntimeCall {
                sequence: 0,
                target: vec!["time".into()],
                arguments: json!({}),
            },
            RuntimeCall {
                sequence: 1,
                target: vec!["bash".into()],
                arguments: json!({"command":"secret"}),
            },
        ];
        assert!(state.admit_calls(&batch, 101).is_err());
        assert!(state.calls.is_empty());
        assert!(
            state
                .admit_calls(
                    &[RuntimeCall {
                        sequence: 0,
                        target: vec!["inventory".into()],
                        arguments: json!({"sku":7})
                    }],
                    101
                )
                .is_err()
        );
        assert!(state.calls.is_empty());
    }

    #[test]
    fn server_time_dispatch_is_once_and_unknown_never_repeats() {
        let mut state = state();
        state
            .admit_calls(
                &[RuntimeCall {
                    sequence: 0,
                    target: vec!["time".into()],
                    arguments: json!({}),
                }],
                101,
            )
            .unwrap();
        let authorized = state.begin_dispatch(0, 102).unwrap();
        assert_eq!(authorized.parent_call_id, state.call_id());
        assert!(state.begin_dispatch(0, 103).is_err());
        let mut restored: ProgramState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert!(restored.begin_dispatch(0, 104).is_err());
        restored.runtime_lost();
        assert_eq!(restored.output_item().unwrap()["status"], "incomplete");
    }

    #[test]
    fn nested_client_result_requires_exact_causal_caller_and_schema() {
        let mut state = state();
        state
            .admit_calls(
                &[RuntimeCall {
                    sequence: 0,
                    target: vec!["inventory".into()],
                    arguments: json!({"sku":"a"}),
                }],
                101,
            )
            .unwrap();
        let call = state.expose_client_call(0, 102).unwrap();
        assert_eq!(call["caller"]["caller_id"], state.call_id());
        let mut output = json!({"type":"function_call_output","call_id":call["call_id"],"caller":call["caller"],"output":"{\"stock\":3}"});
        output["caller"]["caller_id"] = json!("forged");
        assert!(state.accept_client_output(&output).is_err());
        output["caller"] = call["caller"].clone();
        state.accept_client_output(&output).unwrap();
        state.accept_client_output(&output).unwrap();
        output["output"] = json!("{\"stock\":4}");
        assert!(state.accept_client_output(&output).is_err());
        state.finish("3".into()).unwrap();
        assert_eq!(state.output_item().unwrap()["result"], "3");
    }

    #[test]
    fn direct_and_unloaded_functions_are_unavailable_to_programs() {
        let policy=ProgramPolicy::resolve(&[json!({"type":"function","name":"direct","parameters":{}}),json!({"type":"function","name":"deferred","allowed_callers":["programmatic"],"defer_loading":true,"parameters":{}})],false).unwrap();
        assert_eq!(policy.runtime_tools(), json!([]));
        assert!(ToolPath::new(vec!["__proto__".into()]).is_ok());
    }

    #[test]
    fn expiry_blocks_new_effects_but_does_not_discard_known_receipts() {
        let mut state = state();
        state
            .admit_calls(
                &[RuntimeCall {
                    sequence: 0,
                    target: vec!["time".into()],
                    arguments: json!({}),
                }],
                101,
            )
            .unwrap();
        state.begin_dispatch(0, 102).unwrap();
        assert!(state.begin_dispatch(0, 10100).is_err());
        state
            .complete_call(0, json!({"utc":"known","unix_ms":102}))
            .unwrap();
        assert_eq!(
            state.runtime_resume()["results"][0]["result"]["utc"],
            "known"
        );
    }

    #[test]
    fn an_unacknowledged_start_can_only_query_resume_never_restart_source() {
        let mut state = state();
        assert_eq!(state.begin_runtime_turn(101).unwrap()["op"], "start");
        let mut restored: ProgramState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored.begin_runtime_turn(102).unwrap()["op"], "resume");
        restored.runtime_lost();
        assert!(restored.begin_runtime_turn(103).is_err());
        assert!(restored.calls.is_empty());
    }

    #[test]
    fn removing_a_tool_revokes_new_effects_without_erasing_a_known_receipt() {
        let mut state = state();
        state
            .admit_calls(
                &[RuntimeCall {
                    sequence: 0,
                    target: vec!["time".into()],
                    arguments: json!({}),
                }],
                101,
            )
            .unwrap();
        state.begin_dispatch(0, 102).unwrap();
        state.restrict_new_calls(&ProgramPolicy::default());
        state
            .complete_call(0, json!({"utc":"known","unix_ms":102}))
            .unwrap();
        assert!(
            state
                .admit_calls(
                    &[RuntimeCall {
                        sequence: 1,
                        target: vec!["time".into()],
                        arguments: json!({})
                    }],
                    103
                )
                .is_err()
        );
        assert_eq!(state.calls.len(), 1);
    }

    #[test]
    fn adapter_grants_do_not_duplicate_authorization_into_program_state() {
        let grant=ToolGrant::adapter(ToolPath::new(vec!["remote".into()]).unwrap(),"cap_owned".into(),json!({"type":"function","name":"remote","allowed_callers":["programmatic"],"parameters":{"type":"object"},"authorization":"private-sentinel","headers":{"X-Key":"private-sentinel"}})).unwrap();
        assert!(
            !serde_json::to_string(&grant)
                .unwrap()
                .contains("private-sentinel")
        );
    }
}
