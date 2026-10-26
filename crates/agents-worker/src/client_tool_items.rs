//! Caller-executed native tools share the same pending-call authority as functions.
use serde_json::Value;

const MAX_TEXT: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Computer,
    LocalShell,
    Shell,
    ApplyPatch,
}

impl Family {
    pub(crate) fn choice(kind: &str) -> Option<Self> {
        match kind {
            "computer" | "computer_use" | "computer_use_preview" => Some(Self::Computer),
            "local_shell" => Some(Self::LocalShell),
            "shell" => Some(Self::Shell),
            "apply_patch" => Some(Self::ApplyPatch),
            _ => None,
        }
    }
    pub(crate) fn call(kind: &str) -> Option<Self> {
        match kind {
            "computer_call" => Some(Self::Computer),
            "local_shell_call" => Some(Self::LocalShell),
            "shell_call" => Some(Self::Shell),
            "apply_patch_call" => Some(Self::ApplyPatch),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Computer => "computer",
            Self::LocalShell => "local_shell",
            Self::Shell => "shell",
            Self::ApplyPatch => "apply_patch",
        }
    }

    pub(crate) fn output(self) -> &'static str {
        match self {
            Self::Computer => "computer_call_output",
            Self::LocalShell => "local_shell_call_output",
            Self::Shell => "shell_call_output",
            Self::ApplyPatch => "apply_patch_call_output",
        }
    }
}

pub(crate) fn is_call(item: &Value) -> bool {
    item["type"].as_str().and_then(Family::call).is_some()
}

pub(crate) fn is_output(item: &Value) -> bool {
    matches!(
        item["type"].as_str(),
        Some(
            "computer_call_output"
                | "local_shell_call_output"
                | "shell_call_output"
                | "apply_patch_call_output"
        )
    )
}

pub(crate) fn result_id(item: &Value) -> Option<&str> {
    item[if item["type"] == "local_shell_call_output" {
        "id"
    } else {
        "call_id"
    }]
    .as_str()
}

/// The legacy local-shell result uses `id=call_id`, not the output-item ID.
pub(crate) fn validate_result(call: &Value, output: &Value) -> Result<(), &'static str> {
    let family = call["type"]
        .as_str()
        .and_then(Family::call)
        .ok_or("invalid_client_tool_call")?;
    if output["type"] != family.output() {
        return Err("tool_output_type_mismatch");
    }
    if result_id(output) != call["call_id"].as_str() {
        return Err("unexpected_tool_output");
    }
    validate_output(output)?;
    if family == Family::Computer {
        let pending = call["pending_safety_checks"]
            .as_array()
            .ok_or("invalid_computer_safety_checks")?;
        let acknowledged = output["acknowledged_safety_checks"].as_array();
        for check in pending {
            if !acknowledged.is_some_and(|checks| {
                checks.iter().any(|value| {
                    value["id"] == check["id"]
                        && ["code", "message"].iter().all(|field| {
                            value
                                .get(field)
                                .filter(|value| !value.is_null())
                                .is_none_or(|value| Some(value) == check.get(field))
                        })
                })
            }) {
                return Err("computer_safety_check_not_acknowledged");
            }
        }
        if acknowledged.is_some_and(|checks| {
            checks
                .iter()
                .any(|check| !pending.iter().any(|value| value["id"] == check["id"]))
        }) {
            return Err("unexpected_computer_safety_check");
        }
    }
    if let Some(caller) = output.get("caller").filter(|value| !value.is_null()) {
        let expected = call.get("caller").filter(|value| !value.is_null());
        if !expected.map_or_else(
            || {
                caller["type"] == "direct"
                    && caller.as_object().is_some_and(|fields| fields.len() == 1)
            },
            |expected| expected == caller,
        ) {
            return Err("tool_output_caller_mismatch");
        }
    }
    Ok(())
}

pub(crate) fn validate_output(item: &Value) -> Result<(), &'static str> {
    if result_id(item).is_none_or(|id| id.is_empty() || id.len() > 128) {
        return Err("invalid_tool_output");
    }
    if let Some(status) = item.get("status").filter(|status| !status.is_null()) {
        let accepted = matches!(
            status.as_str(),
            Some("in_progress" | "completed" | "incomplete")
        ) || (item["type"] == "computer_call_output" && status == "failed")
            || (item["type"] == "apply_patch_call_output" && status == "failed");
        if !accepted {
            return Err("invalid_client_tool_status");
        }
    }
    match item["type"].as_str() {
        Some("local_shell_call_output") => {
            keys(item, &["type", "id", "output", "status"])?;
            text(&item["output"], MAX_TEXT)
        }
        Some("apply_patch_call_output") => {
            keys(
                item,
                &[
                    "type",
                    "id",
                    "call_id",
                    "status",
                    "output",
                    "caller",
                    "created_by",
                ],
            )?;
            if !matches!(item["status"].as_str(), Some("completed" | "failed")) {
                return Err("invalid_patch_outcome");
            }
            optional_text(item, "output", MAX_TEXT)
        }
        Some("shell_call_output") => {
            keys(
                item,
                &[
                    "type",
                    "id",
                    "call_id",
                    "output",
                    "status",
                    "caller",
                    "created_by",
                    "max_output_length",
                ],
            )?;
            optional_number(item, "max_output_length")?;
            let outputs = array(&item["output"], 128)?;
            for output in outputs {
                keys(output, &["stdout", "stderr", "outcome"])?;
                text(&output["stdout"], MAX_TEXT)?;
                text(&output["stderr"], MAX_TEXT)?;
                match output["outcome"]["type"].as_str() {
                    Some("timeout") => keys(&output["outcome"], &["type"])?,
                    Some("exit") if output["outcome"]["exit_code"].as_i64().is_some() => {
                        keys(&output["outcome"], &["type", "exit_code"])?
                    }
                    _ => return Err("invalid_shell_outcome"),
                }
            }
            Ok(())
        }
        Some("computer_call_output") => {
            keys(
                item,
                &[
                    "type",
                    "id",
                    "call_id",
                    "output",
                    "status",
                    "created_by",
                    "acknowledged_safety_checks",
                ],
            )?;
            let screenshot = &item["output"];
            keys(screenshot, &["type", "file_id", "image_url"])?;
            if screenshot["type"] != "computer_screenshot"
                || screenshot["file_id"].is_string() == screenshot["image_url"].is_string()
            {
                return Err("invalid_computer_screenshot");
            }
            if let Some(checks) = item
                .get("acknowledged_safety_checks")
                .filter(|checks| !checks.is_null())
            {
                safety_checks(checks)?;
            }
            Ok(())
        }
        _ => Err("invalid_tool_output"),
    }
}

pub(crate) fn declaration(tool: &Value) -> bool {
    match tool["type"].as_str() {
        Some(
            "computer" | "computer_use" | "computer_use_preview" | "local_shell" | "apply_patch",
        ) => true,
        Some("shell") => tool["environment"]["type"] == "local",
        _ => false,
    }
}

pub(crate) fn validate_declaration(tool: &Value) -> Result<(), String> {
    if tool["allowed_callers"]
        .as_array()
        .is_some_and(|values| values.iter().any(|value| value == "programmatic"))
    {
        return Err(
            "Client programmatic calls require a configured causal program registry".into(),
        );
    }
    Ok(())
}

pub(crate) fn matches_declaration(tool: &Value, item: &Value) -> bool {
    let Some(family) = item["type"].as_str().and_then(Family::call) else {
        return false;
    };
    let matches = match family {
        Family::Computer => matches!(
            tool["type"].as_str(),
            Some("computer" | "computer_use" | "computer_use_preview")
        ),
        Family::Shell => {
            tool["type"] == "shell"
                && tool["environment"]["type"] == "local"
                && item
                    .get("environment")
                    .is_none_or(|value| value.is_null() || value["type"] == "local")
        }
        _ => tool["type"] == family.name(),
    };
    if !matches {
        return false;
    }
    let caller = item.get("caller").filter(|caller| !caller.is_null());
    let caller_kind = caller.map_or(Some("direct"), |caller| caller["type"].as_str());
    let allowed = tool.get("allowed_callers").filter(|value| !value.is_null());
    match caller_kind {
        Some("direct") => allowed.is_none_or(|values| {
            values
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value == "direct"))
        }),
        // A model-supplied caller_id is not an execution capability. The
        // program registry must supply a causal proof before this can dispatch.
        Some("program") => false,
        _ => false,
    }
}

/// Unknown vendor fields are never promoted to canonical public output.
pub(crate) fn public_item(item: &Value) -> Result<Option<Value>, String> {
    let Some(family) = item["type"].as_str().and_then(Family::call) else {
        return Ok(None);
    };
    text(&item["id"], 128).map_err(str::to_owned)?;
    text(&item["call_id"], 128).map_err(str::to_owned)?;
    if item["id"] == "" || item["call_id"] == "" {
        return Err("invalid_client_tool_identity".into());
    }
    if let Some(status) = item.get("status") {
        if !matches!(
            status.as_str(),
            Some("in_progress" | "completed" | "incomplete")
        ) || (family == Family::ApplyPatch && status == "incomplete")
        {
            return Err("invalid_client_tool_status".into());
        }
    } else {
        return Err("invalid_client_tool_status".into());
    }
    let mut result = select(item, &["type", "id", "call_id", "status"]);
    if let Some(caller) = item.get("caller").filter(|value| !value.is_null()) {
        match caller["type"].as_str() {
            Some("direct") => {}
            Some("program")
                if caller["caller_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty() && id.len() <= 64) => {}
            _ => return Err("invalid_client_tool_caller".into()),
        }
        result["caller"] = select(caller, &["type", "caller_id"]);
    }
    let partial = item["status"] == "in_progress";
    match family {
        Family::Computer => {
            if !partial || item.get("pending_safety_checks").is_some() {
                safety_checks(&item["pending_safety_checks"]).map_err(str::to_owned)?;
            }
            result["pending_safety_checks"] = Value::Array(
                item["pending_safety_checks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|value| select(value, &["id", "code", "message"]))
                    .collect(),
            );
            if let Some(actions) = item.get("actions") {
                result["actions"] = Value::Array(
                    array(actions, 128)
                        .map_err(str::to_owned)?
                        .iter()
                        .map(computer_action)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(str::to_owned)?,
                );
            } else if !partial || item.get("action").is_some() {
                result["action"] = computer_action(&item["action"]).map_err(str::to_owned)?;
            }
        }
        Family::LocalShell => {
            if partial && item.get("action").is_none() {
                return Ok(Some(result));
            }
            let action = &item["action"];
            if action["type"] != "exec" {
                return Err("invalid_local_shell_action".into());
            }
            strings(&action["command"], 128).map_err(str::to_owned)?;
            optional_number(action, "timeout_ms").map_err(str::to_owned)?;
            optional_text(action, "user", 256).map_err(str::to_owned)?;
            optional_text(action, "working_directory", 8192).map_err(str::to_owned)?;
            let env = action["env"]
                .as_object()
                .filter(|env| env.len() <= 128)
                .ok_or("invalid_local_shell_environment")?;
            for (key, value) in env {
                if key.len() > 128 {
                    return Err("invalid_local_shell_environment".into());
                }
                text(value, MAX_TEXT).map_err(str::to_owned)?;
            }
            result["action"] = select(
                action,
                &[
                    "type",
                    "command",
                    "env",
                    "timeout_ms",
                    "user",
                    "working_directory",
                ],
            );
        }
        Family::Shell => {
            if partial && item.get("action").is_none() {
                return Ok(Some(result));
            }
            strings(&item["action"]["commands"], 128).map_err(str::to_owned)?;
            optional_number(&item["action"], "timeout_ms").map_err(str::to_owned)?;
            optional_number(&item["action"], "max_output_length").map_err(str::to_owned)?;
            result["action"] = select(
                &item["action"],
                &["commands", "timeout_ms", "max_output_length"],
            );
            for field in ["timeout_ms", "max_output_length"] {
                if result["action"].get(field).is_none() {
                    result["action"][field] = Value::Null;
                }
            }
            result["environment"] = item
                .get("environment")
                .filter(|value| !value.is_null())
                .map_or(Value::Null, |value| {
                    select(value, &["type", "container_id"])
                });
        }
        Family::ApplyPatch => {
            if partial && item.get("operation").is_none() {
                return Ok(Some(result));
            }
            let operation = &item["operation"];
            text(&operation["path"], 8192).map_err(str::to_owned)?;
            match operation["type"].as_str() {
                Some("create_file" | "update_file") => {
                    text(&operation["diff"], MAX_TEXT).map_err(str::to_owned)?
                }
                Some("delete_file") => {}
                _ => return Err("invalid_patch_operation".into()),
            }
            result["operation"] = select(operation, &["type", "path", "diff"]);
        }
    }
    Ok(Some(result))
}

fn computer_action(action: &Value) -> Result<Value, &'static str> {
    let fields: &[&str] = match action["type"].as_str() {
        Some("click") => {
            if !matches!(
                action["button"].as_str(),
                Some("left" | "right" | "wheel" | "back" | "forward")
            ) {
                return Err("invalid_computer_button");
            }
            coordinates(action)?;
            &["type", "x", "y", "button", "keys"]
        }
        Some("double_click" | "move") => {
            coordinates(action)?;
            &["type", "x", "y", "keys"]
        }
        Some("drag") => {
            for point in array(&action["path"], 4096)? {
                coordinates(point)?;
            }
            &["type", "path", "keys"]
        }
        Some("keypress") => {
            strings(&action["keys"], 128)?;
            &["type", "keys"]
        }
        Some("scroll") => {
            coordinates(action)?;
            number(&action["scroll_x"])?;
            number(&action["scroll_y"])?;
            &["type", "x", "y", "scroll_x", "scroll_y", "keys"]
        }
        Some("type") => {
            text(&action["text"], MAX_TEXT)?;
            &["type", "text"]
        }
        Some("wait" | "screenshot") => &["type"],
        _ => return Err("invalid_computer_action"),
    };
    if let Some(keys) = action.get("keys").filter(|value| !value.is_null()) {
        strings(keys, 128)?;
    }
    let mut projected = select(action, fields);
    if action["type"] == "double_click" && projected.get("keys").is_none() {
        projected["keys"] = Value::Null;
    }
    if let Some(path) = projected.get_mut("path").and_then(Value::as_array_mut) {
        for point in path {
            *point = select(point, &["x", "y"]);
        }
    }
    Ok(projected)
}

fn safety_checks(value: &Value) -> Result<(), &'static str> {
    let mut ids = std::collections::BTreeSet::new();
    for check in array(value, 128)? {
        text(&check["id"], 128)?;
        if !ids.insert(check["id"].as_str().unwrap()) {
            return Err("duplicate_computer_safety_check");
        }
        optional_text(check, "code", 1024)?;
        optional_text(check, "message", 16384)?;
    }
    Ok(())
}

fn select(value: &Value, fields: &[&str]) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|field| {
                value
                    .get(*field)
                    .map(|value| ((*field).into(), value.clone()))
            })
            .collect(),
    )
}
fn keys(value: &Value, allowed: &[&str]) -> Result<(), &'static str> {
    let fields = value.as_object().ok_or("invalid_client_tool_object")?;
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("unknown_client_tool_field");
    }
    Ok(())
}
fn array(value: &Value, max: usize) -> Result<&[Value], &'static str> {
    value
        .as_array()
        .filter(|values| values.len() <= max)
        .map(Vec::as_slice)
        .ok_or("invalid_client_tool_array")
}
fn strings(value: &Value, max: usize) -> Result<(), &'static str> {
    for value in array(value, max)? {
        text(value, MAX_TEXT)?;
    }
    Ok(())
}
fn text(value: &Value, max: usize) -> Result<(), &'static str> {
    value
        .as_str()
        .filter(|value| value.len() <= max)
        .map(|_| ())
        .ok_or("invalid_client_tool_text")
}
fn optional_text(value: &Value, name: &str, max: usize) -> Result<(), &'static str> {
    if let Some(value) = value.get(name).filter(|value| !value.is_null()) {
        text(value, max)?;
    }
    Ok(())
}
fn number(value: &Value) -> Result<(), &'static str> {
    value
        .as_f64()
        .filter(|value| value.is_finite())
        .map(|_| ())
        .ok_or("invalid_computer_coordinate")
}
fn optional_number(value: &Value, field: &str) -> Result<(), &'static str> {
    if let Some(value) = value.get(field).filter(|value| !value.is_null()) {
        if value
            .as_f64()
            .is_none_or(|value| !value.is_finite() || value < 0.0)
        {
            return Err("invalid_client_tool_limit");
        }
    }
    Ok(())
}
fn coordinates(value: &Value) -> Result<(), &'static str> {
    number(&value["x"])?;
    number(&value["y"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn local_shell_uses_the_sdk_id_result_not_a_fabricated_call_id() {
        let call = json!({"type":"local_shell_call","call_id":"call_1","id":"item_1"});
        assert!(validate_result(&call, &json!({"type":"local_shell_call_output","id":"call_1","output":"{\"stdout\":\"ok\"}"})).is_ok());
        assert_eq!(
            validate_result(
                &call,
                &json!({"type":"shell_call_output","call_id":"call_1","output":[]})
            ),
            Err("tool_output_type_mismatch")
        );
    }

    #[test]
    fn computer_safety_acknowledgements_bind_exact_pending_checks() {
        let call = json!({"type":"computer_call","call_id":"c","pending_safety_checks":[{"id":"s","code":"sensitive","message":"Confirm"}]});
        let mut output = json!({"type":"computer_call_output","call_id":"c","output":{"type":"computer_screenshot","image_url":"data:image/png;base64,AA=="}});
        assert_eq!(
            validate_result(&call, &output),
            Err("computer_safety_check_not_acknowledged")
        );
        output["acknowledged_safety_checks"] = call["pending_safety_checks"].clone();
        assert!(validate_result(&call, &output).is_ok());
        output["acknowledged_safety_checks"][0]["message"] = json!("other");
        assert_eq!(
            validate_result(&call, &output),
            Err("computer_safety_check_not_acknowledged")
        );
    }

    #[test]
    fn client_shell_never_claims_a_hosted_environment_or_program_right() {
        let tool = json!({"type":"shell","environment":{"type":"local"}});
        let mut call = json!({"type":"shell_call","environment":{"type":"local"}});
        assert!(matches_declaration(&tool, &call));
        call["environment"] = json!({"type":"container_reference","container_id":"foreign"});
        assert!(!matches_declaration(&tool, &call));
        call["environment"] = json!({"type":"local"});
        call["caller"] = json!({"type":"program","caller_id":"program_1"});
        assert!(!matches_declaration(&tool, &call));
    }

    #[test]
    fn typed_actions_strip_nested_vendor_data_and_keep_patch_operations() {
        let call = json!({"type":"computer_call","id":"i","call_id":"c","status":"completed","pending_safety_checks":[],"action":{"type":"drag","path":[{"x":1,"y":2,"secret":"never"}],"secret":"never"},"secret":"never"});
        let projected = public_item(&call).unwrap().unwrap();
        assert!(!projected.to_string().contains("never"));
        let patch = json!({"type":"apply_patch_call","id":"p","call_id":"c","status":"completed","operation":{"type":"update_file","path":"a.rs","diff":"@@\n-old\n+new","secret":"never"}});
        assert_eq!(
            public_item(&patch).unwrap().unwrap()["operation"],
            json!({"type":"update_file","path":"a.rs","diff":"@@\n-old\n+new"})
        );
    }

    #[test]
    fn in_progress_items_keep_caller_identity_before_actions_arrive() {
        for kind in ["local_shell_call", "shell_call", "apply_patch_call"] {
            let raw = json!({"type":kind,"id":"i","call_id":"c","status":"in_progress","caller":{"type":"program","caller_id":"program_1","secret":"hidden"}});
            assert_eq!(
                public_item(&raw).unwrap().unwrap()["caller"],
                json!({"type":"program","caller_id":"program_1"})
            );
        }
    }

    #[test]
    fn shell_outcomes_and_patch_status_are_not_arbitrary_json() {
        assert_eq!(
            validate_output(
                &json!({"type":"shell_call_output","call_id":"c","output":[{"stdout":"ok","stderr":"","outcome":{"type":"exit"}}]})
            ),
            Err("invalid_shell_outcome")
        );
        assert_eq!(
            validate_output(
                &json!({"type":"apply_patch_call_output","call_id":"c","status":"made_up"})
            ),
            Err("invalid_client_tool_status")
        );
    }

    #[test]
    fn nullable_safety_acknowledgements_do_not_bypass_pending_checks() {
        let mut call = json!({"type":"computer_call","call_id":"c","pending_safety_checks":[]});
        let mut output = json!({"type":"computer_call_output","call_id":"c","output":{"type":"computer_screenshot","file_id":"file-owned"},"acknowledged_safety_checks":null});
        assert!(validate_result(&call, &output).is_ok());
        call["pending_safety_checks"] = json!([{"id":"check"}]);
        assert_eq!(
            validate_result(&call, &output),
            Err("computer_safety_check_not_acknowledged")
        );
        call["pending_safety_checks"][0] =
            json!({"id":"check","code":"sensitive","message":"Confirm"});
        output["acknowledged_safety_checks"] = json!([{"id":"check","code":null,"message":null}]);
        assert!(validate_result(&call, &output).is_ok());
    }

    #[test]
    fn malformed_provider_limits_are_not_public_sdk_values() {
        let mut call = json!({"type":"shell_call","id":"i","call_id":"c","status":"completed","action":{"commands":["echo ok"],"timeout_ms":"private"}});
        assert!(public_item(&call).is_err());
        call["action"]["timeout_ms"] = Value::Null;
        let public = public_item(&call).unwrap().unwrap();
        assert_eq!(public["action"]["max_output_length"], Value::Null);
        assert_eq!(public["environment"], Value::Null);
        call["status"] = json!({"secret":"private"});
        assert!(public_item(&call).is_err());
    }

    #[test]
    fn programmatic_rights_require_preflight_proof_and_direct_defaults_match() {
        assert!(validate_declaration(&json!({"type":"shell","environment":{"type":"local"},"allowed_callers":["direct","programmatic"]})).is_err());
        let call = json!({"type":"shell_call","call_id":"c"});
        let output = json!({"type":"shell_call_output","call_id":"c","output":[],"caller":{"type":"direct"}});
        assert!(validate_result(&call, &output).is_ok());
    }
}
