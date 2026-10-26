//! Response-scoped tool discovery. Provider aliases never become public authority.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const SEARCH_ALIAS: &str = "__agents_tool_search";
pub(crate) const INVOKE_ALIAS: &str = "__agents_invoke_discovered";
const MAX_TOOLS: usize = 1024;
const MAX_RESULTS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct ToolKey {
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

impl ToolKey {
    fn declaration(value: &Value, namespace: Option<&str>) -> Result<Self, String> {
        let kind = value["type"].as_str().ok_or("invalid_discovery_tool")?;
        if !matches!(kind, "function" | "custom") {
            return Err("discovery_requires_callable_tool".into());
        }
        let name = value["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or("invalid_discovery_tool")?;
        if name.starts_with("__agents_") {
            return Err("reserved_tool_name".into());
        }
        Ok(Self {
            kind: kind.into(),
            name: name.into(),
            namespace: namespace.map(str::to_owned),
        })
    }

    pub(crate) fn alias(&self) -> String {
        if self.namespace.is_none() && self.kind != "custom" {
            return self.name.clone();
        }
        let namespace = self.namespace.as_deref().unwrap_or_default();
        let mut digest = Sha256::new();
        for value in [self.kind.as_str(), namespace, self.name.as_str()] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        let digest = format!("{:x}", digest.finalize());
        format!(
            "__agents_{}_{}",
            if self.kind == "custom" {
                "custom"
            } else {
                "ns"
            },
            &digest[..40]
        )
    }

    fn path(&self) -> String {
        self.namespace.as_ref().map_or_else(
            || self.name.clone(),
            |namespace| format!("{namespace}.{}", self.name),
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    key: ToolKey,
    tool: Value,
    namespace_description: Option<String>,
    deferred: bool,
    #[serde(default = "initial_entry")]
    initial: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    loaded_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_alias: Option<String>,
}

impl Entry {
    fn alias(&self) -> Cow<'_, str> {
        self.native_alias
            .as_deref()
            .map_or_else(|| Cow::Owned(self.key.alias()), Cow::Borrowed)
    }
}

fn initial_entry() -> bool {
    true
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Execution {
    Server,
    Client,
}

#[derive(Clone, Serialize, Deserialize)]
struct Search {
    execution: Execution,
    parameters: Value,
    description: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    digest: String,
    keys: Vec<ToolKey>,
}

/// Durable loaded-set and lookup receipts belong to one accepted Response.
/// No provider credentials, MCP authorization or whole native history is stored.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct ToolRegistry {
    entries: Vec<Entry>,
    search: Option<Search>,
    loaded: BTreeSet<ToolKey>,
    client_receipts: BTreeMap<String, Receipt>,
    #[serde(default)]
    server_receipts: BTreeMap<String, Receipt>,
    #[serde(default)]
    pending_search: BTreeSet<String>,
    #[serde(default)]
    reserved_names: BTreeSet<String>,
    #[serde(default)]
    bridge: bool,
    #[serde(skip)]
    search_stream_items: BTreeSet<String>,
    #[serde(skip)]
    invoke_stream_items: BTreeSet<String>,
    #[serde(skip)]
    stream_round: Option<u64>,
    #[serde(skip)]
    defer_turn: bool,
    #[serde(skip)]
    input_origins: BTreeSet<String>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field("entries", &self.entries.len())
            .field("loaded", &self.loaded.len())
            .finish_non_exhaustive()
    }
}

pub(crate) struct SearchResult {
    pub call: Value,
    pub output: Value,
    pub private_output: Value,
}

pub(crate) fn public_item(item: &Value) -> Result<Option<Value>, String> {
    let Some(kind @ ("tool_search_call" | "tool_search_output")) = item["type"].as_str() else {
        return Ok(None);
    };
    if !matches!(item["execution"].as_str(), Some("server" | "client"))
        || !item["call_id"].is_null() && !item["call_id"].is_string()
        || !matches!(
            item["status"].as_str(),
            None | Some("in_progress" | "completed" | "incomplete")
        )
    {
        return Err("invalid_tool_search_item".into());
    }
    let mut value = json!({"type":kind,"execution":item["execution"],"call_id":item["call_id"]});
    for key in ["id", "status"] {
        if let Some(field) = item.get(key) {
            value[key] = field.clone();
        }
    }
    if kind == "tool_search_call" {
        let arguments = item.get("arguments").ok_or("invalid_tool_search_item")?;
        if serde_json::to_vec(arguments)
            .map_err(|_| "invalid_tool_search_item")?
            .len()
            > 65536
        {
            return Err("tool_search_arguments_limit".into());
        }
        value["arguments"] = arguments.clone();
    } else {
        let tools = item["tools"]
            .as_array()
            .filter(|tools| tools.len() <= 128)
            .ok_or("invalid_tool_search_item")?;
        value["tools"] = Value::Array(
            tools
                .iter()
                .map(public_declaration)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(Some(value))
}

fn public_declaration(raw: &Value) -> Result<Value, String> {
    let tool: crate::responses_wire::ResponseTool =
        serde_json::from_value(raw.clone()).map_err(|_| "invalid_discovered_tool")?;
    let mut value = tool.public_value();
    if value["type"] == "namespace" {
        let children = value["tools"]
            .as_array()
            .filter(|tools| tools.len() <= 128)
            .ok_or("invalid_discovered_namespace")?;
        value["tools"] = Value::Array(
            children
                .iter()
                .map(public_declaration)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(value)
}

impl ToolRegistry {
    pub(crate) fn requires_native_transport(&self) -> bool {
        self.search.is_some()
            || self.entries.iter().any(|entry| {
                entry.key.namespace.is_some()
                    || entry.key.kind == "custom"
                    || entry.deferred
                    || !entry.initial
            })
    }
    pub(crate) fn input_origins(&self) -> BTreeSet<String> {
        self.input_origins.clone()
    }
    /// Resolve credentials separately; this catalog stores only immutable callable schemas.
    pub(crate) fn register_mcp(
        &mut self,
        bindings: &crate::mcp_runtime::McpBindings,
    ) -> Result<(), String> {
        for server in bindings.servers.iter().filter(|server| server.deferred) {
            for tool in &server.tools {
                let key = ToolKey {
                    kind: "function".into(),
                    name: tool.name.clone(),
                    namespace: Some(server.label.clone()),
                };
                if let Some(existing) = self.entries.iter().find(|entry| entry.key == key) {
                    if existing.native_alias.as_deref() != Some(server.alias(&tool.name).as_str()) {
                        return Err("duplicate_discovery_tool".into());
                    }
                    continue;
                }
                let callers = [
                    server.direct.then_some("direct"),
                    server.programmatic.then_some("programmatic"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                let declaration = json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.input_schema,"strict":true,"defer_loading":true,"allowed_callers":callers});
                self.add(
                    &declaration,
                    Some(&server.label),
                    server.description.as_deref(),
                )?;
                self.entries.last_mut().unwrap().native_alias = Some(server.alias(&tool.name));
            }
        }
        Ok(())
    }

    pub(crate) fn resolved_mcp_call(&self, raw: &Value) -> Result<Option<Value>, String> {
        let item = self.restore_item(raw)?;
        let Some(entry) = self.entries.iter().find(|entry| {
            entry.key.kind == "function"
                && item["name"] == entry.key.name
                && item["namespace"].as_str() == entry.key.namespace.as_deref()
                && entry.alias().starts_with(crate::mcp_bindings::ALIAS_PREFIX)
        }) else {
            return Ok(None);
        };
        if !self.loaded.contains(&entry.key) {
            return Err("deferred_tool_not_loaded".into());
        }
        let mut item = item;
        item["name"] = json!(entry.alias());
        item.as_object_mut().unwrap().remove("namespace");
        Ok(Some(item))
    }

    pub(crate) fn is_bridge(&self) -> bool {
        self.bridge
    }
    pub(crate) fn origin_digests(&self) -> BTreeSet<String> {
        self.entries
            .iter()
            .filter_map(|entry| entry.loaded_from.clone())
            .collect()
    }

    pub(crate) fn reserve_server_names<'a>(
        &mut self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), String> {
        self.reserved_names
            .extend(names.into_iter().map(str::to_owned));
        if self.entries.iter().any(|entry| {
            entry.key.namespace.is_none() && self.reserved_names.contains(&entry.key.name)
        }) {
            return Err("discovered_tool_shadows_harness".into());
        }
        Ok(())
    }

    /// Explicit SDK replay pairs introduce only caller-executed schemas. Raw
    /// private function aliases never establish a discovery witness.
    pub(crate) fn authorize_history(&mut self, items: &[Value]) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        let mut open = self.pending_search.clone();
        for item in items {
            if item["type"] == "tool_search_call" && item["execution"] == "client" {
                if !matches!(item["status"].as_str(), None | Some("completed")) {
                    return Err("incomplete_tool_search_history".into());
                }
                public_item(item)?;
                let id = item["call_id"]
                    .as_str()
                    .ok_or("invalid_tool_search_call_id")?;
                if !seen.insert(id.to_owned()) || !open.insert(id.to_owned()) {
                    return Err("duplicate_tool_search_call_id".into());
                }
                self.authorize_pending_search([id])?;
            }
            if item["type"] == "tool_search_output" && item["execution"] == "client" {
                let id = item["call_id"]
                    .as_str()
                    .ok_or("invalid_tool_search_call_id")?;
                if !open.remove(id) && !self.client_receipts.contains_key(id) {
                    return Err("unexpected_tool_search_output".into());
                }
            }
        }
        Ok(())
    }

    pub(crate) fn compile_with_input(
        declarations: &[Value],
        input: &[Value],
    ) -> Result<Option<Self>, String> {
        let registry = Self::compile(declarations)?;
        if registry.is_none()
            && input.iter().any(|item| {
                item["type"] == "additional_tools"
                    || (matches!(
                        item["type"].as_str(),
                        Some("tool_search_call" | "tool_search_output")
                    ) && item["execution"] == "client")
            })
        {
            return Ok(Some(Self::default()));
        }
        Ok(registry)
    }
    pub(crate) fn requires_complete_identity(&self, raw: &Value) -> bool {
        raw["type"] == "function_call"
            && (raw["name"] == INVOKE_ALIAS
                || raw["name"] == SEARCH_ALIAS
                || raw["name"].as_str().is_some_and(|name| {
                    self.entries
                        .iter()
                        .any(|entry| entry.key.kind == "custom" && entry.alias() == name)
                }))
    }
    fn has_invoker(&self) -> bool {
        self.search.is_some() || self.entries.iter().any(|entry| !entry.initial)
    }
    pub(crate) fn authorize_pending_search<'a>(
        &mut self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), String> {
        for id in ids {
            if id.is_empty()
                || id.len() > 256
                || (self.pending_search.len() >= MAX_TOOLS && !self.pending_search.contains(id))
            {
                return Err("invalid_tool_search_call_id".into());
            }
            self.pending_search.insert(id.into());
        }
        Ok(())
    }

    pub(crate) fn remember_call(&mut self, item: &Value) -> Result<(), String> {
        if item["type"] == "tool_search_call" && item["execution"] == "client" {
            let id = item["call_id"]
                .as_str()
                .ok_or("invalid_tool_search_call_id")?;
            self.authorize_pending_search(std::iter::once(id))?;
        }
        Ok(())
    }
    pub(crate) fn begin_round(&mut self, round: u64) {
        if self.stream_round != Some(round) {
            self.stream_round = Some(round);
            self.defer_turn = false;
            self.search_stream_items.clear();
            self.invoke_stream_items.clear();
        }
    }
    pub(crate) fn validation_declarations(&self) -> Vec<Value> {
        self.group(&self.loaded.iter().cloned().collect::<Vec<_>>())
    }

    pub(crate) fn declares_public_call(&self, item: &Value) -> bool {
        if item["type"] == "tool_search_call" {
            return self.search.is_some();
        }
        let Some(kind) = call_kind(item) else {
            return false;
        };
        self.loaded.iter().any(|key| {
            item["name"] == key.name
                && key.kind == kind
                && item["namespace"].as_str() == key.namespace.as_deref()
        })
    }

    pub(crate) fn encode_input(
        &mut self,
        items: &[Value],
        bridge: bool,
        choice: Option<&Value>,
    ) -> Result<Vec<Value>, String> {
        self.bridge = bridge;
        self.input_origins.clear();
        self.authorize_history(items)?;
        let mut encoded = Vec::with_capacity(items.len());
        for raw in items {
            if raw["type"] == "additional_tools" {
                encoded.push(self.load_additional(raw, choice, bridge)?);
                continue;
            }
            if raw["type"] == "tool_search_output" && raw["execution"] == "client" {
                encoded.push(self.accept_client_output(raw, choice, bridge)?);
                continue;
            }
            let mut value = raw.clone();
            if bridge && raw["type"] == "tool_search_call" && raw["execution"] == "client" {
                value = json!({"type":"function_call","name":SEARCH_ALIAS,"call_id":raw["call_id"],"arguments":serde_json::to_string(&raw["arguments"]).map_err(|_| "invalid_tool_search_arguments")?,"status":"completed"});
                if let Some(id) = raw.get("id") {
                    value["id"] = id.clone();
                }
            }
            if bridge && raw["type"] == "custom_tool_call_output" {
                value["type"] = json!("function_call_output");
            }
            if bridge
                && matches!(
                    raw["type"].as_str(),
                    Some("function_call" | "custom_tool_call")
                )
            {
                let kind = if raw["type"] == "custom_tool_call" {
                    "custom"
                } else {
                    "function"
                };
                if let Some(entry) = self.entries.iter().find(|entry| {
                    entry.key.kind == kind
                        && raw["name"] == entry.key.name
                        && raw["namespace"].as_str() == entry.key.namespace.as_deref()
                }) {
                    value["name"] = json!(entry.alias());
                    value.as_object_mut().unwrap().remove("namespace");
                    if kind == "custom" {
                        value["type"] = json!("function_call");
                        value["arguments"] = json!(serde_json::to_string(&json!({"input":raw["input"].as_str().ok_or("invalid_custom_input")?})).map_err(|_| "invalid_custom_input")?);
                        value.as_object_mut().unwrap().remove("input");
                    }
                }
            }
            encoded.push(value);
        }
        Ok(encoded)
    }

    fn load_additional(
        &mut self,
        item: &Value,
        choice: Option<&Value>,
        bridge: bool,
    ) -> Result<Value, String> {
        if item["role"] != "developer" {
            return Err("additional_tools_requires_developer_role".into());
        }
        let id = format!("additional_{}", output_digest(item)?);
        self.authorize_pending_search([id.as_str()])?;
        let output = json!({"type":"tool_search_output","execution":"client","call_id":id,"tools":item["tools"]});
        let encoded = self.accept_client_output(&output, choice, bridge)?;
        let private = if bridge {
            json!({"type":"message","role":"developer","content":encoded["output"]})
        } else {
            item.clone()
        };
        let digest = output_digest(&private)?;
        self.input_origins.insert(digest.clone());
        let keys = &self
            .client_receipts
            .get(&id)
            .ok_or("invalid_additional_tools")?
            .keys;
        for entry in self
            .entries
            .iter_mut()
            .filter(|entry| keys.contains(&entry.key))
        {
            entry.loaded_from = Some(digest.clone());
        }
        Ok(private)
    }

    pub(crate) fn observe_output(&mut self, item: &Value) -> Result<(), String> {
        if item["type"] != "tool_search_output" || item["execution"] != "server" {
            return Ok(());
        }
        if self
            .search
            .as_ref()
            .is_none_or(|search| search.execution != Execution::Server)
        {
            return Err("unexpected_tool_search_output".into());
        }
        let tools = item["tools"]
            .as_array()
            .ok_or("invalid_tool_search_output")?;
        let mut keys = Vec::new();
        for tool in tools {
            let namespace = (tool["type"] == "namespace")
                .then(|| tool["name"].as_str())
                .flatten();
            let children = if namespace.is_some() {
                tool["tools"]
                    .as_array()
                    .ok_or("invalid_tool_search_output")?
                    .as_slice()
            } else {
                std::slice::from_ref(tool)
            };
            for child in children {
                let key = ToolKey::declaration(child, namespace)?;
                if !self
                    .entries
                    .iter()
                    .any(|entry| entry.key == key && entry.tool == *child)
                {
                    return Err("undeclared_discovered_tool".into());
                }
                keys.push(key);
            }
        }
        self.loaded.extend(keys.iter().cloned());
        let digest = output_digest(item)?;
        for entry in self
            .entries
            .iter_mut()
            .filter(|entry| keys.contains(&entry.key))
        {
            if entry.deferred {
                entry.loaded_from = Some(digest.clone());
            }
        }
        Ok(())
    }

    pub(crate) fn select_choice(&mut self, choice: Option<&Value>) {
        let Some(choice) =
            choice.filter(|choice| matches!(choice["type"].as_str(), Some("function" | "custom")))
        else {
            return;
        };
        for entry in &mut self.entries {
            if choice["type"] == entry.key.kind
                && choice["name"] == entry.key.name
                && choice["namespace"].as_str() == entry.key.namespace.as_deref()
            {
                // An explicit caller selection already discovers exactly this
                // schema. It does not expose other deferred definitions.
                entry.deferred = false;
                self.loaded.insert(entry.key.clone());
            }
        }
    }

    pub(crate) fn inherit_loaded(&mut self, previous: &Self) {
        self.inherit_loaded_from(previous, &BTreeSet::new());
    }

    /// `authorized_outputs` is derived from the owner's effective native
    /// history, never from the old registry or caller-supplied digest strings.
    pub(crate) fn inherit_loaded_from(
        &mut self,
        previous: &Self,
        authorized_outputs: &BTreeSet<String>,
    ) {
        for entry in &mut self.entries {
            if previous.loaded.contains(&entry.key)
                && let Some(prior) = previous.entries.iter().find(|prior| {
                    prior.key == entry.key
                        && prior.tool == entry.tool
                        && (!entry.deferred
                            || prior
                                .loaded_from
                                .as_ref()
                                .is_some_and(|digest| authorized_outputs.contains(digest)))
                })
            {
                self.loaded.insert(entry.key.clone());
                entry.loaded_from = prior.loaded_from.clone();
            }
        }
        for prior in previous.entries.iter().filter(|entry| {
            !entry.initial
                && entry
                    .loaded_from
                    .as_ref()
                    .is_some_and(|digest| authorized_outputs.contains(digest))
        }) {
            if self.entries.iter().any(|entry| entry.key == prior.key) {
                continue;
            }
            self.entries.push(prior.clone());
            self.loaded.insert(prior.key.clone());
        }
    }
    pub(crate) fn is_client_alias(&self, alias: &str) -> bool {
        if !self.bridge {
            return false;
        }
        (alias == INVOKE_ALIAS && self.has_invoker())
            || (alias == SEARCH_ALIAS
                && self
                    .search
                    .as_ref()
                    .is_some_and(|search| search.execution == Execution::Client))
            || self.entries.iter().any(|entry| {
                (entry.key.namespace.is_some() || entry.key.kind == "custom")
                    && entry.alias() == alias
            })
    }

    /// Tool-search arguments are an object, not function-argument text deltas.
    /// Publish its ordinary item added/done frames; never leak bridge events.
    pub(crate) fn restore_event(
        &mut self,
        kind: &str,
        payload: &Value,
    ) -> Result<Option<Value>, String> {
        if self.defer_turn {
            return Ok(None);
        }
        let item = payload.get("item");
        let search = item
            .is_some_and(|item| item["type"] == "function_call" && item["name"] == SEARCH_ALIAS);
        if item.is_some_and(|item| {
            item["type"] == "function_call"
                && (item["name"] == INVOKE_ALIAS
                    || item["name"].as_str().is_some_and(|name| {
                        self.entries
                            .iter()
                            .any(|entry| entry.key.kind == "custom" && entry.alias() == name)
                    }))
        }) {
            if let Some(id) = item.and_then(|item| item["id"].as_str()) {
                if self.invoke_stream_items.len() >= MAX_TOOLS
                    && !self.invoke_stream_items.contains(id)
                {
                    return Err("tool_registry_limit".into());
                }
                self.invoke_stream_items.insert(id.into());
            }
            // The invoker's public identity lives inside its JSON envelope.
            // Defer the remainder of this turn rather than emit out-of-order
            // output slots or retain another copy of following text deltas.
            self.defer_turn = true;
            return Ok(None);
        }
        if payload["item_id"]
            .as_str()
            .is_some_and(|id| self.invoke_stream_items.contains(id))
        {
            return Ok(None);
        }
        if kind == "response.output_item.done"
            && let Some(item) = item
        {
            self.observe_output(item)?;
        }
        if search {
            if let Some(id) = item.and_then(|item| item["id"].as_str()) {
                if self.search_stream_items.len() >= MAX_TOOLS
                    && !self.search_stream_items.contains(id)
                {
                    return Err("tool_registry_limit".into());
                }
                self.search_stream_items.insert(id.into());
            }
            if self
                .search
                .as_ref()
                .is_some_and(|search| search.execution == Execution::Server)
            {
                return Ok(None);
            }
        }
        if kind.starts_with("response.function_call_arguments.")
            && (payload["name"] == SEARCH_ALIAS
                || payload["item_id"]
                    .as_str()
                    .is_some_and(|id| self.search_stream_items.contains(id)))
        {
            return Ok(None);
        }
        let mut restored = payload.clone();
        if let Some(item) = item {
            restored["item"] = self.restore_item(item)?;
        }
        if let Some(name) = payload["name"].as_str()
            && let Some(entry) = self
                .entries
                .iter()
                .find(|entry| entry.key.namespace.is_some() && entry.alias() == name)
        {
            restored["name"] = json!(entry.key.name);
            restored["namespace"] = json!(entry.key.namespace);
        }
        Ok(Some(restored))
    }

    pub(crate) fn render_resolved(
        &self,
        declarations: &[Value],
        bridge: bool,
    ) -> Result<Vec<Value>, String> {
        let mut retained = declarations
            .iter()
            .filter(|tool| {
                if matches!(tool["type"].as_str(), Some("namespace" | "tool_search")) {
                    return false;
                }
                !self.entries.iter().any(|entry| {
                    entry.key.namespace.is_none()
                        && tool["type"] == entry.key.kind
                        && tool["name"] == entry.key.name
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        retained.extend(self.render(bridge)?);
        Ok(retained)
    }

    pub(crate) fn is_server_search(&self, alias: &str) -> bool {
        self.bridge && alias == SEARCH_ALIAS && self.server_search_enabled()
    }

    pub(crate) fn server_search_enabled(&self) -> bool {
        self.search
            .as_ref()
            .is_some_and(|search| search.execution == Execution::Server)
    }

    /// Client discovery may add user-defined client tools, never credential-bearing
    /// hosted declarations. Validate with the same public wire/schema budgets.
    pub(crate) fn accept_client_output(
        &mut self,
        item: &Value,
        choice: Option<&Value>,
        bridge: bool,
    ) -> Result<Value, String> {
        if item["type"] != "tool_search_output" || item["execution"] != "client" {
            return Err("unexpected_tool_search_output".into());
        }
        let call_id = item["call_id"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .ok_or("invalid_tool_search_call_id")?;
        let tools = item["tools"]
            .as_array()
            .filter(|tools| tools.len() <= 128)
            .ok_or("invalid_tool_search_output")?;
        if tools.iter().any(|tool| {
            !matches!(
                tool["type"].as_str(),
                Some("function" | "custom" | "namespace")
            )
        }) {
            return Err("discovery_hosted_binding_required".into());
        }
        let request: crate::responses_wire::ResponseRequest = serde_json::from_value(
            json!({"model":"discovery-validation","input":"","tools":tools}),
        )
        .map_err(|_| "invalid_tool_search_output")?;
        request.validate()?;
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(tools).map_err(|_| "invalid_tool_search_output")?)
        );
        if let Some(receipt) = self.client_receipts.get(call_id) {
            if receipt.digest != digest {
                return Err("tool_search_idempotency_conflict".into());
            }
            self.pending_search.remove(call_id);
            let output = self.private_output(call_id, &receipt.keys, bridge)?;
            self.input_origins.insert(output_digest(&output)?);
            return Ok(output);
        }
        if !self.pending_search.contains(call_id) {
            return Err("unexpected_tool_search_output".into());
        }
        let mut candidate = self.clone();
        let mut keys = Vec::new();
        for tool in tools {
            let namespace = (tool["type"] == "namespace")
                .then(|| tool["name"].as_str())
                .flatten();
            let children = if namespace.is_some() {
                tool["tools"].as_array().unwrap().as_slice()
            } else {
                std::slice::from_ref(tool)
            };
            for child in children {
                let key = ToolKey::declaration(child, namespace)?;
                let call = json!({"type":if key.kind == "custom" {"custom_tool_call"} else {"function_call"},"name":key.name,"namespace":key.namespace});
                if !allowed_discovery(choice, &call) {
                    return Err("tool_search_output_not_allowed".into());
                }
                if let Some(existing) = candidate.entries.iter().find(|entry| entry.key == key) {
                    if existing.tool != *child {
                        return Err("discovered_tool_definition_conflict".into());
                    }
                } else {
                    candidate.add(child, namespace, tool["description"].as_str())?;
                    candidate.entries.last_mut().unwrap().initial = false;
                }
                candidate.loaded.insert(key.clone());
                keys.push(key);
            }
        }
        let output = candidate.private_output(call_id, &keys, bridge)?;
        let source = output_digest(&output)?;
        candidate.input_origins.insert(source.clone());
        for entry in candidate
            .entries
            .iter_mut()
            .filter(|entry| keys.contains(&entry.key))
        {
            entry.loaded_from = Some(source.clone());
        }
        candidate
            .client_receipts
            .insert(call_id.into(), Receipt { digest, keys });
        candidate.pending_search.remove(call_id);
        *self = candidate;
        Ok(output)
    }

    fn private_output(
        &self,
        call_id: &str,
        keys: &[ToolKey],
        bridge: bool,
    ) -> Result<Value, String> {
        let tools = self.group(keys);
        if bridge {
            return Ok(
                json!({"type":"function_call_output","call_id":call_id,"output":serde_json::to_string(&json!({"tools":tools,"invoke":INVOKE_ALIAS})).map_err(|_| "invalid_tool_search_output")?}),
            );
        }
        Ok(
            json!({"type":"tool_search_output","execution":"client","call_id":call_id,"status":"completed","tools":tools}),
        )
    }

    pub(crate) fn compile(declarations: &[Value]) -> Result<Option<Self>, String> {
        let needed = declarations.iter().any(|tool| {
            tool["type"] == "namespace"
                || tool["type"] == "custom"
                || tool
                    .get("allowed_callers")
                    .is_some_and(|value| !value.is_null())
                || tool["type"] == "tool_search"
                || tool["defer_loading"] == true
        });
        if !needed {
            return Ok(None);
        }
        let mut registry = Self::default();
        for tool in declarations {
            if tool["type"] == "mcp" && tool["defer_loading"] == true {
                return Err("mcp_discovery_binding_required".into());
            }
            match tool["type"].as_str() {
                Some("function" | "custom") => registry.add(tool, None, None)?,
                Some("namespace") => {
                    let name = tool["name"].as_str().ok_or("invalid_discovery_namespace")?;
                    for child in tool["tools"]
                        .as_array()
                        .ok_or("invalid_discovery_namespace")?
                    {
                        registry.add(child, Some(name), tool["description"].as_str())?;
                    }
                }
                Some("tool_search") => {
                    if registry.search.is_some() {
                        return Err("duplicate_tool_search".into());
                    }
                    let execution = match tool["execution"].as_str() {
                        None | Some("server") => Execution::Server,
                        Some("client") => Execution::Client,
                        _ => return Err("invalid_tool_search_execution".into()),
                    };
                    let parameters = tool
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(search_schema);
                    if execution == Execution::Server
                        && parameters["properties"]
                            .as_object()
                            .is_some_and(|properties| {
                                properties
                                    .keys()
                                    .any(|key| !matches!(key.as_str(), "query" | "goal" | "paths"))
                            })
                    {
                        return Err("unsupported_server_discovery_arguments".into());
                    }
                    registry.search = Some(Search {
                        execution,
                        parameters,
                        description: tool["description"].as_str().map(str::to_owned),
                    });
                }
                _ => {}
            }
        }
        if registry.search.is_none() && registry.entries.iter().any(|entry| entry.deferred) {
            return Err("deferred_tools_require_tool_search".into());
        }
        Ok(Some(registry))
    }

    fn add(
        &mut self,
        tool: &Value,
        namespace: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), String> {
        if self.entries.len() == MAX_TOOLS {
            return Err("tool_registry_limit".into());
        }
        let key = ToolKey::declaration(tool, namespace)?;
        let native_alias = key.alias();
        if key.namespace.is_none() && self.reserved_names.contains(&key.name) {
            return Err("discovered_tool_shadows_harness".into());
        }
        if key.kind == "custom" {
            crate::custom_tool_formats::validate_tool(tool)?;
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.key == key || entry.alias() == native_alias)
        {
            return Err("duplicate_discovery_tool".into());
        }
        let deferred = tool["defer_loading"] == true;
        if !deferred {
            self.loaded.insert(key.clone());
        }
        self.entries.push(Entry {
            key,
            tool: tool.clone(),
            namespace_description: description.map(str::to_owned),
            deferred,
            initial: true,
            loaded_from: None,
            native_alias: Some(native_alias),
        });
        Ok(())
    }

    /// Native-capable routes retain namespace/defer syntax. The function bridge
    /// keeps initial tool bytes stable; discovered schemas enter native history.
    pub(crate) fn render(&self, bridge: bool) -> Result<Vec<Value>, String> {
        if self.entries.iter().any(|entry| {
            entry.key.namespace.is_none() && self.reserved_names.contains(&entry.key.name)
        }) {
            return Err("discovered_tool_shadows_harness".into());
        }
        let mut tools = Vec::new();
        if bridge {
            for entry in self
                .entries
                .iter()
                .filter(|entry| entry.initial && !entry.deferred)
            {
                tools.push(bridge_tool(entry)?);
            }
        } else {
            tools.extend(
                self.group(
                    &self
                        .entries
                        .iter()
                        .filter(|entry| entry.initial)
                        .map(|entry| entry.key.clone())
                        .collect::<Vec<_>>(),
                ),
            );
        }
        if let Some(search) = &self.search {
            if bridge {
                let mut overview = Vec::new();
                let mut namespaces = BTreeSet::new();
                for entry in self
                    .entries
                    .iter()
                    .filter(|entry| entry.initial && entry.deferred)
                {
                    if let Some(namespace) = &entry.key.namespace {
                        if namespaces.insert(namespace) {
                            overview.push(json!({"namespace":namespace,"description":entry.namespace_description.as_deref().unwrap_or_default()}));
                        }
                    } else {
                        overview.push(
                            json!({"name":entry.key.name,"description":entry.tool["description"]}),
                        );
                    }
                }
                tools.push(json!({"type":"function","name":SEARCH_ALIAS,"description":format!("Discover and load deferred tools. Search by exact paths or a short query. Invoke loaded tools only through __agents_invoke_discovered; their qualified names are not standalone function declarations. Inventory: {}", json!(overview)),"parameters":search.parameters}));
            } else {
                let mut declaration = json!({"type":"tool_search","execution":if search.execution == Execution::Server {"server"} else {"client"},"parameters":search.parameters});
                if let Some(description) = &search.description {
                    declaration["description"] = json!(description);
                }
                tools.push(declaration);
            }
        }
        if bridge && self.has_invoker() {
            tools.push(json!({"type":"function","name":INVOKE_ALIAS,"description":"Call one tool already loaded by tool search. Use its qualified name and follow the loaded JSON parameter schema. For a custom tool use kind=custom and arguments={input: raw_text}. Never invoke an unloaded tool.","parameters":{"type":"object","properties":{"tool":{"type":"string","maxLength":256},"kind":{"type":"string","enum":["function","custom"]},"arguments":{"type":"object"}},"required":["tool","arguments"],"additionalProperties":false}}));
        }
        Ok(tools)
    }

    pub(crate) fn restore_item(&self, raw: &Value) -> Result<Value, String> {
        if raw["type"] != "function_call" {
            return Ok(raw.clone());
        }
        let name = raw["name"].as_str().ok_or("invalid_discovery_call")?;
        if !self.bridge
            && (matches!(name, SEARCH_ALIAS | INVOKE_ALIAS)
                || name.starts_with("__agents_ns_")
                || name.starts_with("__agents_custom_"))
        {
            return Err("inactive_tool_bridge_alias".into());
        }
        if name == INVOKE_ALIAS {
            if !self.has_invoker() {
                return Err("undeclared_discovery_invoker".into());
            }
            let envelope: Value =
                serde_json::from_str(raw["arguments"].as_str().ok_or("invalid_discovery_call")?)
                    .map_err(|_| "invalid_discovery_call")?;
            let entry = self
                .entries
                .iter()
                .find(|entry| {
                    envelope["tool"]
                        .as_str()
                        .is_some_and(|path| entry.key.path() == path)
                        && entry.key.kind == envelope["kind"].as_str().unwrap_or("function")
                })
                .ok_or("undeclared_discovered_tool")?;
            if !self.loaded.contains(&entry.key) {
                return Err("deferred_tool_not_loaded".into());
            }
            if !envelope["arguments"].is_object()
                || envelope.as_object().is_none_or(|object| {
                    object
                        .keys()
                        .any(|key| !matches!(key.as_str(), "tool" | "kind" | "arguments"))
                })
            {
                return Err("invalid_discovery_call".into());
            }
            let mut value = raw.clone();
            value["name"] = json!(entry.key.name);
            if let Some(namespace) = &entry.key.namespace {
                value["namespace"] = json!(namespace);
            }
            let arguments = serde_json::to_string(&envelope["arguments"])
                .map_err(|_| "invalid_discovery_call")?;
            if entry.key.kind == "custom" {
                value["type"] = json!("custom_tool_call");
                value["input"] = json!(crate::custom_tool_formats::restore_input(&arguments)?);
                value.as_object_mut().unwrap().remove("arguments");
            } else {
                value["arguments"] = json!(arguments);
            }
            return Ok(value);
        }
        if name == SEARCH_ALIAS {
            let search = self.search.as_ref().ok_or("undeclared_tool_search")?;
            let text = raw["arguments"]
                .as_str()
                .ok_or("invalid_tool_search_arguments")?;
            let arguments: Value = if text.is_empty() && raw["status"] == "in_progress" {
                json!({})
            } else {
                serde_json::from_str(text).map_err(|_| "invalid_tool_search_arguments")?
            };
            if !arguments.is_object() {
                return Err("invalid_tool_search_arguments".into());
            }
            let mut item = json!({"type":"tool_search_call","arguments":arguments,"execution":if search.execution == Execution::Server {"server"} else {"client"},"call_id":if search.execution == Execution::Client {raw["call_id"].clone()} else {Value::Null}});
            for key in ["id", "status"] {
                if let Some(value) = raw.get(key) {
                    item[key] = value.clone();
                }
            }
            return Ok(item);
        }
        let Some(entry) = self.entries.iter().find(|entry| {
            (entry.key.namespace.is_some() || entry.key.kind == "custom") && entry.alias() == name
        }) else {
            return Ok(raw.clone());
        };
        let mut item = raw.clone();
        item["name"] = json!(entry.key.name);
        if let Some(namespace) = &entry.key.namespace {
            item["namespace"] = json!(namespace);
        }
        if entry.key.kind == "custom" {
            item["type"] = json!("custom_tool_call");
            item["input"] = json!(crate::custom_tool_formats::restore_input(
                raw["arguments"].as_str().ok_or("invalid_custom_input")?
            )?);
            item.as_object_mut().unwrap().remove("arguments");
        }
        Ok(item)
    }

    pub(crate) fn rewrite_choice(&self, choice: &Value, bridge: bool) -> Result<Value, String> {
        if !bridge {
            return Ok(choice.clone());
        }
        if choice["type"] == "tool_search" {
            return Ok(json!({"type":"function","name":SEARCH_ALIAS}));
        }
        if choice["type"] == "allowed_tools" {
            let mut value = choice.clone();
            let mut tools = Vec::new();
            for tool in choice["tools"].as_array().ok_or("invalid_tool_choice")? {
                let deferred = self.entries.iter().any(|entry| {
                    entry.deferred
                        && tool["type"] == entry.key.kind
                        && tool["name"] == entry.key.name
                        && tool["namespace"].as_str() == entry.key.namespace.as_deref()
                });
                let resolved = if deferred {
                    json!({"type":"function","name":INVOKE_ALIAS})
                } else {
                    self.rewrite_choice(tool, bridge)?
                };
                if !tools.contains(&resolved) {
                    tools.push(resolved);
                }
            }
            value["tools"] = Value::Array(tools);
            return Ok(value);
        }
        let namespace = choice["namespace"].as_str();
        if namespace.is_none() && choice["type"] != "custom" {
            return Ok(choice.clone());
        }
        let entry = self
            .entries
            .iter()
            .find(|entry| {
                entry.key.namespace.as_deref() == namespace
                    && entry.key.name == choice["name"]
                    && entry.key.kind == choice["type"]
            })
            .ok_or("tool_choice_unavailable")?;
        Ok(json!({"type":"function","name":entry.alias()}))
    }

    pub(crate) fn validate_call(&self, item: &Value, choice: Option<&Value>) -> Result<(), String> {
        self.validate_call_as(item, choice, crate::hosted_bindings::Caller::Direct)
    }

    /// The runtime chooses `caller` after authenticating its execution scope;
    /// a provider's JSON `caller` field cannot grant programmatic permission.
    pub(crate) fn validate_call_as(
        &self,
        item: &Value,
        choice: Option<&Value>,
        caller: crate::hosted_bindings::Caller,
    ) -> Result<(), String> {
        let item = self.restore_item(item)?;
        if !allowed(choice, &item) {
            return Err("provider_allowed_tools_policy_violation".into());
        }
        if item["type"] == "tool_search_call" {
            let search = self.search.as_ref().ok_or("undeclared_tool_search")?;
            let execution = search.execution;
            if item["execution"]
                != if execution == Execution::Server {
                    "server"
                } else {
                    "client"
                }
            {
                return Err("tool_search_execution_mismatch".into());
            }
            if item["status"] == "completed" {
                crate::responses_schema::validate_instance(&search.parameters, &item["arguments"])
                    .map_err(|_| "invalid_tool_search_arguments")?;
            }
            return Ok(());
        }
        let Some(kind) = call_kind(&item) else {
            return Ok(());
        };
        let key = ToolKey {
            kind: kind.into(),
            name: item["name"]
                .as_str()
                .ok_or("invalid_discovery_call")?
                .into(),
            namespace: item["namespace"].as_str().map(str::to_owned),
        };
        let declared = self.entries.iter().any(|entry| entry.key == key);
        if let Some(entry) = self.entries.iter().find(|entry| entry.key == key) {
            let mode = match caller {
                crate::hosted_bindings::Caller::Direct => "direct",
                crate::hosted_bindings::Caller::Programmatic => "programmatic",
            };
            if caller == crate::hosted_bindings::Caller::Direct
                && item["caller"]["type"]
                    .as_str()
                    .is_some_and(|mode| mode != "direct")
            {
                return Err("untrusted_programmatic_caller".into());
            }
            if entry
                .tool
                .get("allowed_callers")
                .filter(|value| !value.is_null())
                .is_some_and(|callers| {
                    !callers
                        .as_array()
                        .is_some_and(|callers| callers.iter().any(|caller| caller == mode))
                })
            {
                return Err("tool_caller_not_allowed".into());
            }
        }
        if !declared
            && (key.namespace.is_some()
                || key.name.starts_with("__agents_ns_")
                || key.name.starts_with("__agents_custom_"))
        {
            return Err("undeclared_tool_namespace".into());
        }
        if declared && !self.loaded.contains(&key) {
            return Err("deferred_tool_not_loaded".into());
        }
        if item["status"] == "completed"
            && let Some(entry) = self.entries.iter().find(|entry| {
                entry.key == key && entry.key.kind == "function" && entry.tool["strict"] == true
            })
        {
            let arguments: Value = serde_json::from_str(
                item["arguments"]
                    .as_str()
                    .ok_or("invalid_function_arguments")?,
            )
            .map_err(|_| "invalid_function_arguments")?;
            crate::responses_schema::validate_instance(&entry.tool["parameters"], &arguments)
                .map_err(|_| "invalid_function_arguments")?;
        }
        if item["status"] == "completed"
            && key.kind == "custom"
            && let Some(entry) = self.entries.iter().find(|entry| entry.key == key)
        {
            crate::custom_tool_formats::validate_completed_input(
                &entry.tool,
                item["input"].as_str().ok_or("invalid_custom_input")?,
            )?;
        }
        Ok(())
    }

    pub(crate) fn discover(
        &mut self,
        call_id: &str,
        arguments: &Value,
        choice: Option<&Value>,
        bridge: bool,
    ) -> Result<SearchResult, String> {
        self.discover_execution(call_id, call_id, arguments, choice, bridge)
    }

    pub(crate) fn discover_execution(
        &mut self,
        execution: &str,
        call_id: &str,
        arguments: &Value,
        choice: Option<&Value>,
        bridge: bool,
    ) -> Result<SearchResult, String> {
        if execution.is_empty() || execution.len() > 512 {
            return Err("invalid_tool_search_execution".into());
        }
        if call_id.is_empty() || call_id.len() > 256 {
            return Err("invalid_tool_search_call_id".into());
        }
        let mode = self
            .search
            .as_ref()
            .ok_or("undeclared_tool_search")?
            .execution;
        if mode != Execution::Server {
            return Err("tool_search_requires_client_output".into());
        }
        crate::responses_schema::validate_instance(
            &self.search.as_ref().unwrap().parameters,
            arguments,
        )
        .map_err(|_| "invalid_tool_search_arguments")?;
        let digest = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(arguments).map_err(|_| "invalid_tool_search_arguments")?
            )
        );
        let keys = if let Some(receipt) = self.server_receipts.get(execution) {
            if receipt.digest != digest {
                return Err("tool_search_idempotency_conflict".into());
            }
            receipt.keys.clone()
        } else {
            if self.server_receipts.len() >= MAX_TOOLS {
                return Err("tool_registry_limit".into());
            }
            let keys = self.search_keys(arguments, choice)?;
            self.server_receipts.insert(
                execution.into(),
                Receipt {
                    digest,
                    keys: keys.clone(),
                },
            );
            self.loaded.extend(keys.iter().cloned());
            keys
        };
        let tools = self.group(&keys);
        let private_output = self.private_output(call_id, &keys, bridge)?;
        let source = output_digest(&private_output)?;
        for entry in self
            .entries
            .iter_mut()
            .filter(|entry| keys.contains(&entry.key))
        {
            entry.loaded_from = Some(source.clone());
        }
        Ok(SearchResult {
            call: json!({"type":"tool_search_call","execution":"server","call_id":null,"status":"completed","arguments":arguments}),
            output: json!({"type":"tool_search_output","execution":"server","call_id":null,"status":"completed","tools":tools}),
            private_output,
        })
    }

    fn search_keys(
        &self,
        arguments: &Value,
        choice: Option<&Value>,
    ) -> Result<Vec<ToolKey>, String> {
        let object = arguments
            .as_object()
            .ok_or("invalid_tool_search_arguments")?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "query" | "paths" | "goal"))
        {
            return Err("invalid_tool_search_arguments".into());
        }
        let paths = arguments
            .get("paths")
            .map(|paths| {
                paths
                    .as_array()
                    .filter(|paths| !paths.is_empty() && paths.len() <= MAX_RESULTS)
                    .ok_or("invalid_tool_search_paths")
            })
            .transpose()?;
        let query = arguments
            .get("query")
            .or_else(|| arguments.get("goal"))
            .map(|query| {
                query
                    .as_str()
                    .filter(|query| !query.trim().is_empty() && query.len() <= 4096)
                    .ok_or("invalid_tool_search_query")
            })
            .transpose()?
            .map(str::to_lowercase);
        if paths.is_none() && query.is_none() {
            return Err("tool_search_arguments_empty".into());
        }
        if paths.is_some_and(|paths| {
            paths
                .iter()
                .any(|path| path.as_str().is_none_or(|path| path.len() > 256))
        }) {
            return Err("invalid_tool_search_paths".into());
        }
        Ok(self.entries.iter().filter(|entry| {
            let item = if entry.alias().starts_with(crate::mcp_bindings::ALIAS_PREFIX) { json!({"type":"mcp_call","server_label":entry.key.namespace,"name":entry.key.name}) } else { json!({"type":if entry.key.kind == "custom" {"custom_tool_call"} else {"function_call"},"name":entry.key.name,"namespace":entry.key.namespace}) };
            if !allowed_discovery(choice, &item) { return false }
            let path = entry.key.path();
            paths.is_some_and(|paths| paths.iter().any(|candidate| candidate.as_str().is_some_and(|candidate| candidate == path || entry.key.namespace.as_deref() == Some(candidate)))) || query.as_ref().is_some_and(|query| {
                let haystack = format!("{} {} {}", path, entry.tool["description"].as_str().unwrap_or_default(), entry.namespace_description.as_deref().unwrap_or_default()).to_lowercase();
                query.split_whitespace().all(|term| haystack.contains(term))
            })
        }).take(MAX_RESULTS).map(|entry| entry.key.clone()).collect())
    }

    fn group(&self, keys: &[ToolKey]) -> Vec<Value> {
        let mut result = Vec::<Value>::new();
        for entry in self
            .entries
            .iter()
            .filter(|entry| keys.contains(&entry.key))
        {
            let Some(namespace) = &entry.key.namespace else {
                result.push(entry.tool.clone());
                continue;
            };
            if let Some(group) = result
                .iter_mut()
                .find(|group| group["type"] == "namespace" && group["name"] == *namespace)
            {
                group["tools"]
                    .as_array_mut()
                    .unwrap()
                    .push(entry.tool.clone());
            } else {
                result.push(json!({"type":"namespace","name":namespace,"description":entry.namespace_description.as_deref().unwrap_or_default(),"tools":[entry.tool]}));
            }
        }
        result
    }
}

fn bridge_tool(entry: &Entry) -> Result<Value, String> {
    if entry.key.kind != "function" {
        return crate::custom_tool_formats::bridge_tool(&entry.tool, &entry.alias());
    }
    let mut tool = entry.tool.clone();
    tool["name"] = json!(entry.alias());
    tool.as_object_mut().unwrap().remove("defer_loading");
    Ok(tool)
}

fn search_schema() -> Value {
    json!({"type":"object","properties":{"query":{"type":"string","maxLength":4096},"paths":{"type":"array","maxItems":32,"items":{"type":"string","maxLength":256}}},"additionalProperties":false})
}

fn call_kind(item: &Value) -> Option<&'static str> {
    match item["type"].as_str()? {
        "function_call" => Some("function"),
        "custom_tool_call" => Some("custom"),
        _ => None,
    }
}

fn choice_matches(choice: &Value, item: &Value) -> bool {
    if let Some(family) = item["type"]
        .as_str()
        .and_then(crate::client_tool_items::Family::call)
    {
        return choice["type"]
            .as_str()
            .and_then(crate::client_tool_items::Family::choice)
            == Some(family);
    }
    if item["type"] == "function_call" && item["name"] == SEARCH_ALIAS {
        return choice["type"] == "tool_search";
    }
    if item["type"] == "function_call"
        && let Some(kind) = item["name"]
            .as_str()
            .and_then(crate::hosted_tools::Kind::from_alias)
    {
        return choice["type"] == kind.native_type()
            || (kind == crate::hosted_tools::Kind::WebSearch
                && matches!(
                    choice["type"].as_str(),
                    Some("web_search_preview" | "web_search_preview_2025_03_11")
                ));
    }
    if let Some(kind) = call_kind(item) {
        return choice["type"] == kind
            && choice["name"] == item["name"]
            && choice.get("namespace").filter(|value| !value.is_null())
                == item.get("namespace").filter(|value| !value.is_null());
    }
    match item["type"].as_str() {
        Some("tool_search_call") => choice["type"] == "tool_search",
        Some("mcp_call") => {
            choice["type"] == "mcp"
                && choice["server_label"] == item["server_label"]
                && (choice.get("name").is_none() || choice["name"] == item["name"])
        }
        Some(kind) => choice["type"]
            .as_str()
            .is_some_and(|value| kind.strip_suffix("_call") == Some(value)),
        None => false,
    }
}

/// The subset is an execution capability, not merely a provider hint. Required
/// mode's existence check is handled at the turn boundary by the same harness.
pub(crate) fn allowed(choice: Option<&Value>, item: &Value) -> bool {
    let Some(choice) = choice else { return true };
    if choice == "none" {
        return false;
    }
    if choice.is_string() {
        return true;
    }
    if choice["type"] == "allowed_tools" {
        return choice["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| choice_matches(tool, item)));
    }
    choice_matches(choice, item)
}

pub(crate) fn requires_call(choice: Option<&Value>) -> bool {
    choice.is_some_and(|choice| {
        choice == "required" || (choice["type"] == "allowed_tools" && choice["mode"] == "required")
    })
}

pub(crate) fn output_digest(item: &Value) -> Result<String, String> {
    let mut normalized = Cow::Borrowed(item);
    if item["type"] == "function_call_output" {
        let text = item["output"]
            .as_array()
            .filter(|parts| parts.len() == 1)
            .and_then(|parts| {
                let part = &parts[0];
                (part["type"] == "input_text"
                    && part.as_object().is_some_and(|part| part.len() == 2))
                .then(|| part["text"].as_str())
                .flatten()
            });
        if let Some(text) = text {
            normalized.to_mut()["output"] = json!(text);
        }
    }
    Ok(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(normalized.as_ref()).map_err(|_| "invalid_tool_search_output")?
        )
    ))
}

fn allowed_discovery(choice: Option<&Value>, item: &Value) -> bool {
    // Selecting the discovery operation alone must still allow it to discover
    // its registry. A subset, however, limits both discovery and invocation.
    match choice {
        Some(choice) if choice["type"] == "allowed_tools" || choice == "none" => {
            allowed(Some(choice), item)
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::compile(&[
            json!({"type":"namespace","name":"crm","description":"Orders and customers","tools":[{"type":"function","name":"orders","description":"Find open orders","defer_loading":true,"parameters":{"type":"object","properties":{"id":{"type":"string"}}}}]}),
            json!({"type":"function","name":"time","parameters":{"type":"object"}}),
            json!({"type":"tool_search"}),
        ]).unwrap().unwrap();
        registry.encode_input(&[], true, None).unwrap();
        registry
    }

    #[test]
    fn namespace_aliases_are_stable_and_restored_without_changing_identity() {
        let registry = registry();
        let key = &registry.entries[0].key;
        let original = json!({"type":"function_call","id":"upstream_item","call_id":"call1","name":key.alias(),"arguments":"{}","status":"completed"});
        let restored = registry.restore_item(&original).unwrap();
        assert_eq!(restored["id"], "upstream_item");
        assert_eq!(restored["call_id"], "call1");
        assert_eq!(restored["namespace"], "crm");
        assert_eq!(restored["name"], "orders");
        let replay: ToolRegistry =
            serde_json::from_value(serde_json::to_value(&registry).unwrap()).unwrap();
        assert_eq!(key.alias(), replay.entries[0].key.alias());
        assert_eq!(original["name"], key.alias());
    }

    #[test]
    fn loading_preserves_initial_tools_and_is_idempotent() {
        let mut registry = registry();
        let initial = registry.render(true).unwrap();
        assert!(
            !initial
                .iter()
                .any(|tool| tool["parameters"]["properties"]["id"].is_object())
        );
        let args = json!({"paths":["crm"]});
        let result = registry.discover("call1", &args, None, true).unwrap();
        assert_eq!(result.output["call_id"], Value::Null);
        assert_eq!(result.private_output["call_id"], "call1");
        assert_eq!(result.output["tools"][0]["tools"][0]["name"], "orders");
        assert_eq!(registry.render(true).unwrap(), initial);
        assert_eq!(
            registry
                .discover("call1", &args, None, true)
                .unwrap()
                .output,
            result.output
        );
        assert!(
            registry
                .discover("call1", &json!({"query":"orders"}), None, true)
                .is_err()
        );
    }

    #[test]
    fn unloaded_and_unallowed_calls_cannot_execute() {
        let mut registry = registry();
        let call =
            json!({"type":"function_call","name":"orders","namespace":"crm","arguments":"{}"});
        assert!(registry.validate_call(&call, None).is_err());
        registry
            .discover("load", &json!({"query":"open orders"}), None, true)
            .unwrap();
        assert!(registry.validate_call(&call, None).is_ok());
        let choice = json!({"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"time"}]});
        assert!(registry.validate_call(&call, Some(&choice)).is_err());
        assert!(
            registry
                .discover("empty", &json!({"paths":["crm"]}), Some(&choice), true)
                .unwrap()
                .output["tools"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn client_search_and_reserved_names_are_not_server_capabilities() {
        let mut registry =
            ToolRegistry::compile(&[json!({"type":"tool_search","execution":"client"})])
                .unwrap()
                .unwrap();
        assert!(
            registry
                .discover("x", &json!({"query":"anything"}), None, true)
                .is_err()
        );
        assert!(
            ToolRegistry::compile(&[
                json!({"type":"function","name":SEARCH_ALIAS}),
                json!({"type":"tool_search"})
            ])
            .is_err()
        );
    }

    #[test]
    fn advanced_client_tools_need_pending_authority_and_retained_output_witness() {
        let mut registry =
            ToolRegistry::compile(&[json!({"type":"tool_search","execution":"client"})])
                .unwrap()
                .unwrap();
        let initial = registry.render(true).unwrap();
        let output = json!({"type":"tool_search_output","execution":"client","call_id":"client-search","tools":[{"type":"function","name":"shipping_eta","parameters":{"type":"object"},"defer_loading":true}]});
        assert!(registry.accept_client_output(&output, None, true).is_err());
        registry
            .authorize_pending_search(["client-search"])
            .unwrap();
        let native = registry.accept_client_output(&output, None, true).unwrap();
        assert_eq!(native["type"], "function_call_output");
        assert_eq!(registry.render(true).unwrap(), initial);
        let call = json!({"type":"function_call","name":"shipping_eta","arguments":"{}"});
        assert!(registry.declares_public_call(&call));
        let mut child =
            ToolRegistry::compile(&[json!({"type":"tool_search","execution":"client"})])
                .unwrap()
                .unwrap();
        child.inherit_loaded(&registry);
        assert!(!child.declares_public_call(&call));
        child.inherit_loaded_from(
            &registry,
            &BTreeSet::from([output_digest(&native).unwrap()]),
        );
        assert!(child.declares_public_call(&call));
        assert_eq!(child.render(true).unwrap(), initial);
    }

    #[test]
    fn invoker_restores_inner_arguments_without_exposing_private_envelope() {
        let mut registry = registry();
        let raw = json!({"type":"function_call","id":"raw","call_id":"invoke","name":INVOKE_ALIAS,"arguments":"{\"tool\":\"crm.orders\",\"arguments\":{\"id\":\"C1\"}}","status":"completed"});
        assert!(registry.restore_item(&raw).is_err());
        registry
            .discover("load", &json!({"paths":["crm"]}), None, true)
            .unwrap();
        let restored = registry.restore_item(&raw).unwrap();
        assert_eq!(restored["name"], "orders");
        assert_eq!(restored["namespace"], "crm");
        assert_eq!(restored["call_id"], "invoke");
        assert_eq!(
            serde_json::from_str::<Value>(restored["arguments"].as_str().unwrap()).unwrap(),
            json!({"id":"C1"})
        );
        assert!(
            !serde_json::to_string(&restored)
                .unwrap()
                .contains(INVOKE_ALIAS)
        );
    }

    #[test]
    fn wrapped_identity_does_not_create_out_of_order_public_slots() {
        let mut registry = registry();
        registry.begin_round(0);
        let item = json!({"output_index":0,"item":{"type":"function_call","id":"call","name":INVOKE_ALIAS,"arguments":"","status":"in_progress"}});
        assert!(
            registry
                .restore_event("response.output_item.added", &item)
                .unwrap()
                .is_none()
        );
        let following =
            json!({"output_index":1,"item":{"type":"message","id":"message","content":[]}});
        assert!(
            registry
                .restore_event("response.output_item.added", &following)
                .unwrap()
                .is_none()
        );
        registry.begin_round(1);
        assert!(
            registry
                .restore_event("response.output_item.added", &following)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn semantic_output_witness_preserves_legacy_single_text_representation() {
        let string =
            json!({"type":"function_call_output","call_id":"search","output":"{\"tools\":[]}"});
        let array = json!({"type":"function_call_output","call_id":"search","output":[{"type":"input_text","text":"{\"tools\":[]}"}]});
        assert_eq!(
            output_digest(&string).unwrap(),
            output_digest(&array).unwrap()
        );
        let other =
            json!({"type":"function_call_output","call_id":"other","output":"{\"tools\":[]}"});
        assert_ne!(
            output_digest(&string).unwrap(),
            output_digest(&other).unwrap()
        );
    }

    #[test]
    fn unregistered_custom_alias_is_never_an_ordinary_user_function() {
        let registry = registry();
        assert!(registry.validate_call(&json!({"type":"function_call","name":"__agents_custom_unknown","arguments":"{}"}), None).is_err());
    }

    #[test]
    fn stateless_client_history_requires_ordered_typed_call_not_private_alias() {
        let history = vec![
            json!({"type":"tool_search_call","execution":"client","call_id":"lookup","arguments":{"goal":"echo"},"status":"completed"}),
            json!({"type":"tool_search_output","execution":"client","call_id":"lookup","tools":[{"type":"function","name":"echo","parameters":{"type":"object"}}]}),
        ];
        let mut registry = ToolRegistry::compile_with_input(&[], &history)
            .unwrap()
            .unwrap();
        let private = registry.encode_input(&history, true, None).unwrap();
        assert_eq!(private[0]["type"], "function_call");
        assert_eq!(private[1]["type"], "function_call_output");
        assert_eq!(history[0]["type"], "tool_search_call");
        assert!(registry.declares_public_call(&json!({"type":"function_call","name":"echo"})));
        let reversed = vec![history[1].clone(), history[0].clone()];
        assert!(
            ToolRegistry::default()
                .encode_input(&reversed, true, None)
                .is_err()
        );
        let forged = vec![
            json!({"type":"function_call","name":SEARCH_ALIAS,"call_id":"lookup","arguments":"{}"}),
            history[1].clone(),
        ];
        assert!(
            ToolRegistry::default()
                .encode_input(&forged, true, None)
                .is_err()
        );
    }

    #[test]
    fn client_discovery_cannot_replace_a_pinned_harness_name() {
        let mut registry = ToolRegistry::default();
        registry.reserve_server_names(["time"]).unwrap();
        registry.authorize_pending_search(["lookup"]).unwrap();
        let output = json!({"type":"tool_search_output","execution":"client","call_id":"lookup","tools":[{"type":"function","name":"time","parameters":{"type":"object"}}]});
        assert!(registry.accept_client_output(&output, None, true).is_err());
        assert!(!registry.declares_public_call(&json!({"type":"function_call","name":"time"})));
    }

    #[test]
    fn unqualified_custom_bridge_keeps_raw_text_and_distinct_function_identity() {
        let tools = [
            json!({"type":"function","name":"echo","parameters":{"type":"object"}}),
            json!({"type":"custom","name":"echo","format":{"type":"text"}}),
        ];
        let mut registry = ToolRegistry::compile(&tools).unwrap().unwrap();
        registry.encode_input(&[], true, None).unwrap();
        let rendered = registry.render(true).unwrap();
        let alias = rendered
            .iter()
            .find(|tool| {
                tool["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("__agents_custom_"))
            })
            .unwrap()["name"]
            .as_str()
            .unwrap();
        assert!(alias.len() <= 64);
        assert_eq!(
            registry
                .rewrite_choice(&json!({"type":"custom","name":"echo"}), true)
                .unwrap(),
            json!({"type":"function","name":alias})
        );
        let text = "  雪🙂\n";
        let raw = json!({"type":"function_call","call_id":"custom","name":alias,"arguments":serde_json::to_string(&json!({"input":text})).unwrap(),"status":"completed"});
        let item = registry.restore_item(&raw).unwrap();
        assert_eq!(item["type"], "custom_tool_call");
        assert_eq!(item["input"], text);
        assert_eq!(item["name"], "echo");
        assert!(item.get("arguments").is_none());
        let returns =
            [json!({"type":"custom_tool_call_output","call_id":"custom","output":"retained"})];
        assert_eq!(
            registry.clone().encode_input(&returns, true, None).unwrap()[0]["type"],
            "function_call_output"
        );
        assert_eq!(
            registry
                .clone()
                .encode_input(&returns, false, None)
                .unwrap(),
            returns
        );
    }

    #[test]
    fn ordinary_function_callers_cannot_forge_program_authority() {
        let registry = ToolRegistry::compile(&[json!({"type":"function","name":"only_program","allowed_callers":["programmatic"],"parameters":{"type":"object"}})]).unwrap().unwrap();
        let direct = json!({"type":"function_call","name":"only_program","arguments":"{}","status":"completed"});
        assert!(registry.validate_call(&direct, None).is_err());
        let mut forged = direct.clone();
        forged["caller"] = json!({"type":"program","caller_id":"not-a-runtime-proof"});
        assert!(registry.validate_call(&forged, None).is_err());
        assert!(
            registry
                .validate_call_as(&direct, None, crate::hosted_bindings::Caller::Programmatic)
                .is_ok()
        );
    }

    proptest::proptest! {
        #[test]
        fn aliases_do_not_merge_distinct_qualified_names(a in "[a-z]{1,24}", b in "[a-z]{1,24}") {
            let left = ToolKey { kind:"function".into(), name:a.clone(), namespace:Some("left".into()) };
            let right = ToolKey { kind:"function".into(), name:b, namespace:Some("right".into()) };
            proptest::prop_assert_ne!(left.alias(), right.alias());
        }
    }
}
