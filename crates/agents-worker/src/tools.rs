use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const REGISTRY_VERSION: &str = "2026-10-25.1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDefinition,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Harness,
    Workspace,
    Persistence,
}

pub fn tool_kind(name: &str) -> Option<ToolKind> {
    match name {
        "time" => Some(ToolKind::Harness),
        "bash" => Some(ToolKind::Workspace),
        "workspace_commit" | "workspace_versions" | "workspace_restore" => {
            Some(ToolKind::Persistence)
        }
        _ => None,
    }
}

/// Order is part of the pinned session prefix, not a hash-map iteration detail.
pub fn registered_tools(workspace_enabled: bool) -> Vec<ToolDefinition> {
    let mut tools = vec![definition(
        "time",
        "Get the current UTC time and Unix timestamp in milliseconds.",
        json!({"type":"object","properties":{},"additionalProperties":false}),
    )];
    if workspace_enabled {
        tools.push(definition(
            "bash",
            "Run bash in the session's persistent workspace. Commands are non-interactive.",
            json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string","minLength":1,"maxLength":65536},
                    "timeout_ms":{"type":"integer","minimum":100,"maximum":300000},
                    "cwd":{"type":"string","description":"Path relative to the workspace root."}
                },
                "required":["command"],
                "additionalProperties":false
            }),
        ));
        tools.push(definition(
            "workspace_commit",
            "Persist the current workspace as an immutable version in your Space. Returns a version ID you can use to download or restore it later.",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string","minLength":1,"maxLength":100}},
                "additionalProperties":false
            }),
        ));
        tools.push(definition(
            "workspace_versions",
            "List saved workspace versions across your Space, newest first. Versions are immutable and may be downloaded or restored.",
            json!({
                "type":"object",
                "properties":{
                    "after":{"type":"string","description":"Continue after this saved version ID."},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                },
                "additionalProperties":false
            }),
        ));
        tools.push(definition(
            "workspace_restore",
            "Restore a saved workspace version from your Space into this workspace. The current workspace is saved first so your work is not lost.",
            json!({
                "type":"object",
                "properties":{"version_id":{"type":"string","minLength":1,"maxLength":100}},
                "required":["version_id"],
                "additionalProperties":false
            }),
        ));
    }
    tools
}

fn definition(name: &str, description: &str, parameters: Value) -> ToolDefinition {
    ToolDefinition {
        kind: "function".into(),
        function: FunctionDefinition {
            name: name.into(),
            description: description.into(),
            parameters,
            strict: None,
        },
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BashArguments {
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u32,
    #[serde(default)]
    pub cwd: Option<String>,
}

fn default_timeout() -> u32 {
    30_000
}

pub fn parse_bash_arguments(arguments: &str) -> Result<BashArguments, String> {
    let args: BashArguments = serde_json::from_str(arguments)
        .map_err(|_| "bash arguments must match the registered schema".to_owned())?;
    if args.command.trim().is_empty() || args.command.len() > 65_536 {
        return Err("command must contain 1 to 65536 bytes".into());
    }
    if !(100..=300_000).contains(&args.timeout_ms) {
        return Err("timeout_ms must be between 100 and 300000".into());
    }
    if let Some(cwd) = &args.cwd {
        if cwd.starts_with('/')
            || cwd.contains('\\')
            || cwd.contains('\0')
            || cwd
                .split('/')
                .any(|part| part == ".." || part.contains(':'))
        {
            return Err("cwd must be relative to the workspace root".into());
        }
    }
    Ok(args)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCommitArguments {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceVersionsArguments {
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default = "default_version_limit")]
    pub limit: usize,
}

fn default_version_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRestoreArguments {
    pub version_id: String,
}

pub fn parse_workspace_commit(arguments: &str) -> Result<WorkspaceCommitArguments, String> {
    let args: WorkspaceCommitArguments = serde_json::from_str(arguments)
        .map_err(|_| "workspace_commit arguments must match the registered schema".to_owned())?;
    if args
        .name
        .as_ref()
        .is_some_and(|name| name.trim().is_empty() || name.len() > 100)
    {
        return Err("name must contain 1 to 100 bytes".into());
    }
    Ok(args)
}

pub fn parse_workspace_versions(arguments: &str) -> Result<WorkspaceVersionsArguments, String> {
    let args: WorkspaceVersionsArguments = serde_json::from_str(arguments)
        .map_err(|_| "workspace_versions arguments must match the registered schema".to_owned())?;
    if !(1..=100).contains(&args.limit)
        || args
            .after
            .as_ref()
            .is_some_and(|after| after.is_empty() || after.len() > 100)
    {
        return Err("limit must be between 1 and 100 and after must be a saved version ID".into());
    }
    Ok(args)
}

pub fn parse_workspace_restore(arguments: &str) -> Result<WorkspaceRestoreArguments, String> {
    let args: WorkspaceRestoreArguments = serde_json::from_str(arguments)
        .map_err(|_| "workspace_restore arguments must match the registered schema".to_owned())?;
    if args.version_id.is_empty() || args.version_id.len() > 100 {
        return Err("version_id must be a saved version ID".into());
    }
    Ok(args)
}

pub fn persistence_output(name: &str, value: &Value) -> Value {
    match name {
        "workspace_commit" => version_output(value),
        "workspace_versions" => json!({
            "versions":value["data"].as_array().map(|items|items.iter().map(version_output).collect::<Vec<_>>()).unwrap_or_default(),
            "has_more":value["has_more"],
            "next_cursor":value["next_cursor"]
        }),
        "workspace_restore" => json!({
            "restored":version_output(&value["restored"]),
            "safety_version_id":value["safety_version_id"]
        }),
        _ => value.clone(),
    }
}

fn version_output(version: &Value) -> Value {
    json!({
        "id":version["id"],
        "name":version["name"],
        "created_at_ms":version["created_at_ms"],
        "size_bytes":version["archive_bytes"],
        "sha256":version["sha256"]
    })
}

/// `None` delegates to another registered executor; unknown names are never delegated.
pub fn dispatch_harness(name: &str, arguments: &str) -> Result<Option<String>, String> {
    match tool_kind(name) {
        Some(ToolKind::Workspace | ToolKind::Persistence) => Ok(None),
        Some(ToolKind::Harness) => {
            let args: Value = serde_json::from_str(arguments)
                .map_err(|_| "time arguments must be an empty object".to_owned())?;
            if args.as_object().is_none_or(|args| !args.is_empty()) {
                return Err("time arguments must be an empty object".into());
            }
            let now = worker::Date::now();
            Ok(Some(
                json!({"utc":now.to_string(),"unix_ms":now.as_millis()}).to_string(),
            ))
        }
        None => Err(format!("unregistered tool: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_stable_and_workspace_is_opt_in() {
        assert_eq!(registered_tools(false).len(), 1);
        let full = registered_tools(true);
        assert_eq!(full[0].function.name, "time");
        assert_eq!(full[1].function.name, "bash");
        assert_eq!(full.len(), 5);
        assert_eq!(full[2].function.name, "workspace_commit");
        assert_eq!(full[3].function.name, "workspace_versions");
        assert_eq!(full[4].function.name, "workspace_restore");
        assert_eq!(tool_kind("workspace_restore"), Some(ToolKind::Persistence));
        assert_eq!(tool_kind("unknown"), None);
    }

    #[test]
    fn persisted_definitions_remain_compatible_and_local_strictness_survives() {
        let legacy =
            json!({"name":"local","description":"Local extension","parameters":{"type":"object"}});
        let definition: FunctionDefinition = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(definition.strict, None);
        assert_eq!(serde_json::to_value(definition).unwrap(), legacy);
        let strict = json!({"strict":true,"name":"local","description":"Local extension","parameters":{"type":"object"}});
        let definition: FunctionDefinition = serde_json::from_value(strict.clone()).unwrap();
        assert_eq!(definition.strict, Some(true));
        assert_eq!(serde_json::to_value(definition).unwrap(), strict);
    }

    #[test]
    fn bash_enforces_execution_bounds_and_relative_paths() {
        assert_eq!(
            parse_bash_arguments(r#"{"command":"pwd"}"#)
                .unwrap()
                .timeout_ms,
            30000
        );
        for cwd in ["/etc", "../etc", "a/../../etc", "C:/etc", "a\\b"] {
            let input = json!({"command":"pwd","cwd":cwd}).to_string();
            assert!(parse_bash_arguments(&input).is_err(), "{cwd}");
        }
        assert!(parse_bash_arguments(r#"{"command":" ","timeout_ms":100}"#).is_err());
        assert!(parse_bash_arguments(r#"{"command":"pwd","timeout_ms":300001}"#).is_err());
        assert!(parse_bash_arguments(r#"{"command":"pwd","extra":true}"#).is_err());
    }

    #[test]
    fn persistence_arguments_are_bounded_and_never_accept_storage_details() {
        assert!(parse_workspace_commit("{}").unwrap().name.is_none());
        assert_eq!(parse_workspace_versions("{}").unwrap().limit, 50);
        assert!(parse_workspace_commit(r#"{"name":" "}"#).is_err());
        assert!(parse_workspace_commit(r#"{"object_key":"private"}"#).is_err());
        assert!(parse_workspace_versions(r#"{"limit":0}"#).is_err());
        assert!(parse_workspace_versions(r#"{"limit":101}"#).is_err());
        assert!(parse_workspace_restore(r#"{"version_id":""}"#).is_err());
        assert!(parse_workspace_restore(r#"{"version_id":"v1","owner_id":"other"}"#).is_err());
        assert_eq!(
            parse_workspace_restore(r#"{"version_id":"v1"}"#)
                .unwrap()
                .version_id,
            "v1"
        );
    }

    #[test]
    fn persistence_tools_expose_operations_not_storage_protocol() {
        let version = json!({
            "id":"saved","name":"Before refactor","created_at_ms":123,
            "archive_bytes":456,"sha256":"digest","object_key":"private/key",
            "generation":7,"format_version":1,"logical_bytes":900,"entries":3
        });
        let expected = json!({"id":"saved","name":"Before refactor","created_at_ms":123,"size_bytes":456,"sha256":"digest"});
        assert_eq!(persistence_output("workspace_commit", &version), expected);
        let list = json!({"data":[version.clone()],"has_more":true,"next_cursor":"saved"});
        assert_eq!(
            persistence_output("workspace_versions", &list),
            json!({"versions":[expected.clone()],"has_more":true,"next_cursor":"saved"})
        );
        let restored = json!({"restored":version,"safety_version_id":"backup"});
        assert_eq!(
            persistence_output("workspace_restore", &restored),
            json!({"restored":expected,"safety_version_id":"backup"})
        );
    }
}
