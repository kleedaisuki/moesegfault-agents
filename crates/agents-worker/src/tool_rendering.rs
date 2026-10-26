//! Borrowed resolved tool capabilities shared by counting and generation.
use serde_json::{Value, json};
use worker::{Error, Result};

use crate::{
    hosted_bindings::{BoundTools, Caller},
    mcp_runtime::McpBindings,
    tool_discovery::ToolRegistry,
};

pub(crate) struct ToolBindings<'a> {
    pub hosted: &'a BoundTools,
    pub mcp: &'a McpBindings,
    pub discovery: Option<&'a ToolRegistry>,
    pub programmatic: bool,
}

pub(crate) struct ServerChoice {
    pub kind: &'static str,
    pub alias: String,
    pub namespace: Option<String>,
    pub choice: Value,
}

impl ServerChoice {
    pub(crate) fn matches(&self, call: &Value) -> bool {
        call["type"] == self.kind
            && call["name"] == self.alias
            && match &self.namespace {
                Some(namespace) => call["namespace"].as_str() == Some(namespace.as_str()),
                None => call.get("namespace").is_none_or(Value::is_null),
            }
    }
}

impl ToolBindings<'_> {
    pub(crate) fn render_native(
        &self,
        declarations: &[Value],
        _bridge: bool,
        server_time: bool,
    ) -> Result<Vec<Value>> {
        let declarations = if self.programmatic {
            self.programmatic_policy(declarations, server_time)?
                .render_native(declarations)
                .map_err(Error::RustError)?
        } else {
            declarations.to_vec()
        };
        let tools = self.hosted.render_native(&declarations)?;
        let tools = self.mcp.render_native(&tools)?;
        match self.discovery {
            None => Ok(tools),
            Some(registry) => registry
                .render_resolved(&tools, true)
                .map_err(Error::RustError),
        }
    }

    pub(crate) fn rewrite_choice(&self, choice: Value, _bridge: bool) -> Result<Value> {
        let choice = crate::programmatic_tools::ProgramPolicy::default()
            .rewrite_choice(choice)
            .map_err(Error::RustError)?;
        let choice = self.hosted.rewrite_choice(choice)?;
        let choice = self.mcp.rewrite_choice(choice)?;
        match self.discovery {
            None => Ok(choice),
            Some(registry) => registry
                .rewrite_choice(&choice, true)
                .map_err(Error::RustError),
        }
    }

    pub(crate) fn is_server_alias(&self, name: &str) -> bool {
        (self.programmatic && name == crate::programmatic_tools::PROGRAM_ALIAS)
            || self
                .hosted
                .lookup(name)
                .is_some_and(|binding| binding.allows(Caller::Direct))
            || self
                .mcp
                .lookup(name)
                .is_some_and(|(server, _)| server.direct)
            || self
                .discovery
                .is_some_and(|registry| registry.is_server_search(name))
    }

    /// A private local policy view, never an annotation accepted from the model.
    /// Only resolved capabilities can manufacture this server-choice identity.
    pub(crate) fn policy_call(&self, raw: &Value) -> Option<Value> {
        if raw["type"] != "function_call" {
            return None;
        }
        if self.programmatic
            && raw["name"] == crate::programmatic_tools::PROGRAM_ALIAS
            && raw.get("namespace").is_none_or(Value::is_null)
        {
            return Some(json!({"type":"programmatic_tool_calling_call","call_id":raw["call_id"]}));
        }
        let resolved = self
            .discovery
            .and_then(|registry| registry.resolved_mcp_call(raw).ok().flatten());
        let raw = resolved.as_ref().unwrap_or(raw);
        let name = raw["name"].as_str()?;
        if let Some(binding) = self
            .hosted
            .lookup(name)
            .filter(|binding| binding.allows(Caller::Direct))
        {
            return Some(
                json!({"type":format!("{}_call",binding.kind().native_type()),"call_id":raw["call_id"]}),
            );
        }
        if let Some((server, tool)) = self.mcp.lookup(name).filter(|(server, _)| server.direct) {
            return Some(
                json!({"type":"mcp_call","server_label":server.label,"name":tool.name,"call_id":raw["call_id"]}),
            );
        }
        if self
            .discovery
            .is_some_and(|registry| registry.is_server_search(name))
        {
            return Some(json!({"type":"tool_search_call","execution":"server","call_id":null}));
        }
        None
    }

    pub(crate) fn programmatic_policy(
        &self,
        declarations: &[Value],
        server_time: bool,
    ) -> Result<crate::programmatic_tools::ProgramPolicy> {
        crate::programmatic_tools::ProgramPolicy::resolve_bound(
            declarations,
            server_time,
            self.mcp,
            self.discovery,
        )
        .map_err(Error::RustError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_choice_binds_alias_and_namespace_not_only_reused_call_id() {
        let route = ServerChoice {
            kind: "function_call",
            alias: "__agents_tool_search".into(),
            namespace: None,
            choice: json!({"type":"tool_search_call"}),
        };
        assert!(route.matches(
            &json!({"type":"function_call","call_id":"same","name":"__agents_tool_search"})
        ));
        assert!(
            !route.matches(
                &json!({"type":"function_call","call_id":"same","name":"forbidden_client"})
            )
        );
        assert!(!route.matches(&json!({"type":"function_call","call_id":"same","name":"__agents_tool_search","namespace":"forged"})));
        assert!(!route.matches(&json!({"type":"function_call","call_id":"same","name":"__agents_tool_search","namespace":42})));
    }
}
