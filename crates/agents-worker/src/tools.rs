use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const REGISTRY_VERSION: &str = "2026-10-23.1";

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Harness,
    Workspace,
}

pub fn tool_kind(name: &str) -> Option<ToolKind> {
    match name {
        "time" => Some(ToolKind::Harness),
        "bash" => Some(ToolKind::Workspace),
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

/// `None` delegates to the workspace executor; unknown names are never delegated.
pub fn dispatch_harness(name: &str, arguments: &str) -> Result<Option<String>, String> {
    match tool_kind(name) {
        Some(ToolKind::Workspace) => Ok(None),
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
        assert_eq!(tool_kind("unknown"), None);
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
}
