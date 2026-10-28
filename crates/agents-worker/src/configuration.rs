//! Versioned Agent instruction/tool edits, independent of immutable Session pins.

use agents_core::{PinnedContext, SessionConfiguration, ToolExecution};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct History {
    pub revision: u64,
    pub updates: Vec<usize>,
}

impl History {
    pub(crate) fn valid(&self, end: usize) -> bool {
        self.revision > 0
            && !self.updates.is_empty()
            && self.updates.iter().all(|index| *index < end)
            && self.updates.windows(2).all(|pair| pair[0] < pair[1])
    }
    pub(crate) fn rebased(&self, indices: &[usize]) -> Self {
        Self {
            revision: self.revision,
            updates: indices
                .iter()
                .enumerate()
                .filter_map(|(new, old)| self.updates.binary_search(old).is_ok().then_some(new))
                .collect(),
        }
    }
}

pub(crate) fn for_session(
    pinned: &PinnedContext,
    mut configuration: SessionConfiguration,
) -> SessionConfiguration {
    configuration.unavailable_tools = configuration
        .tools
        .iter()
        .filter(|tool| requires_workspace(tool) && !pinned.workspace)
        .map(|tool| tool.name.clone())
        .collect();
    configuration
        .tools
        .retain(|tool| !requires_workspace(tool) || pinned.workspace);
    configuration
}

fn requires_workspace(tool: &agents_core::RegisteredTool) -> bool {
    tool.execution == ToolExecution::Workspace
        || matches!(
            crate::tools::tool_kind(&tool.name),
            Some(crate::tools::ToolKind::Workspace | crate::tools::ToolKind::Persistence)
        )
}

pub(crate) fn item(configuration: &SessionConfiguration) -> Value {
    let mut value = serde_json::to_value(configuration).expect("configuration is JSON-safe");
    value["scope"] = json!("agent_configuration");
    value["policy"] = json!(
        "This trusted platform configuration revision replaces ONLY the Agent instructions and registered tool set from the initial pin or an earlier Agent configuration revision. The instructions field is durable Agent guidance, not response-specific instructions. Apply it to this and later responses until a newer Agent configuration revision. Response-specific instructions retain their normal precedence and scope; resetting them does not reset this Agent configuration. Ordinary historical system/developer guidance, model, workspace authority, skills and runtime profile remain unchanged. Old Agent configuration revisions are historical, not active guidance. The actual current tool bindings alone authorize execution; unavailable_tools are not callable."
    );
    json!({"type":"message","role":"system","content":[{"type":"input_text",
        "text":format!("<agents_configuration_update>\n{value}\n</agents_configuration_update>")}]})
}

pub(crate) fn message(configuration: &SessionConfiguration) -> crate::provider::Message {
    let item = item(configuration);
    crate::provider::Message::text("system", item["content"][0]["text"].as_str().unwrap())
}

pub(crate) fn initial(pinned: &PinnedContext) -> SessionConfiguration {
    SessionConfiguration {
        agent_revision: pinned.agent_revision,
        instructions: pinned.instructions.clone(),
        tools: pinned.tools.clone(),
        unavailable_tools: vec![],
    }
}

pub(crate) async fn pending(
    env: &worker::Env,
    owner: &str,
    agent: &str,
    pinned: &PinnedContext,
    after: u64,
) -> worker::Result<Vec<SessionConfiguration>> {
    let mut cursor = after;
    let mut pending = vec![];
    loop {
        let changes = crate::store::Store::new(env)
            .configuration_revisions(owner, agent, cursor, None)
            .await?;
        let count = changes.len();
        for change in changes {
            if change.agent_revision <= cursor {
                return Err(worker::Error::RustError(
                    "configuration_revision_invalid".into(),
                ));
            }
            cursor = change.agent_revision;
            pending.push(for_session(pinned, change));
        }
        if count < 128 {
            return Ok(pending);
        }
    }
}

pub(crate) fn legacy_prefix(
    record: &agents_core::SessionRecord,
    history: &[crate::provider::Message],
    instructions: Option<&str>,
) -> String {
    let mut pinned = record.pinned.clone();
    if let Some(configuration) = &record.configuration {
        pinned.instructions = configuration.instructions.clone();
    }
    let mut prefix = crate::context_prefix::prefix(&pinned);
    for instruction in history {
        prefix.push_str(&format!(
            "\n\nHistorical {} instruction:\n{}",
            instruction.role,
            instruction.content.as_str().unwrap_or("")
        ));
    }
    if let Some(instructions) = instructions {
        prefix.push_str("\n\nUser-provided instructions for this response:\n");
        prefix.push_str(instructions);
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use agents_core::{CreateAgent, RegisteredTool, ToolExecution};

    fn pin() -> PinnedContext {
        let agent = serde_json::from_value::<CreateAgent>(
            json!({"name":"configuration","instructions":"ORIGINAL"}),
        )
        .unwrap()
        .into_config("agent".into(), "owner".into(), 0);
        PinnedContext::resolve(&agent, vec![], vec![])
    }

    #[test]
    fn session_configuration_filters_all_workspace_authority_without_dropping_instructions() {
        let tools = [
            "time",
            "bash",
            "workspace_commit",
            "workspace_restore",
            "workspace_versions",
        ]
        .iter()
        .map(|name| RegisteredTool {
            name: (*name).into(),
            version: "1".into(),
            execution: if *name == "bash" {
                ToolExecution::Workspace
            } else {
                ToolExecution::Harness
            },
            description: "registered".into(),
            parameters: json!({}),
        })
        .collect();
        let change = SessionConfiguration {
            agent_revision: 2,
            instructions: "LATEST".into(),
            tools,
            unavailable_tools: vec![],
        };
        let resolved = for_session(&pin(), change.clone());
        assert_eq!(resolved.instructions, "LATEST");
        assert_eq!(
            resolved
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["time"]
        );
        assert_eq!(
            resolved.unavailable_tools,
            vec![
                "bash",
                "workspace_commit",
                "workspace_restore",
                "workspace_versions"
            ]
        );
        let mut available = pin();
        available.workspace = true;
        assert_eq!(for_session(&available, change.clone()), change);
    }

    #[test]
    fn session_configuration_scope_and_legacy_fallback_do_not_mutate_the_original_pin() {
        let pin = pin();
        let hash = pin.prefix_hash();
        let configuration = SessionConfiguration {
            agent_revision: 2,
            instructions: "LATEST".into(),
            tools: vec![],
            unavailable_tools: vec![],
        };
        let event = item(&configuration);
        assert_eq!(event["role"], "system");
        assert!(
            event["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("<agents_configuration_update>\n")
        );
        assert!(
            event["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("agent_configuration")
        );
        let record = agents_core::SessionRecord {
            id: "session".into(),
            owner_id: "owner".into(),
            agent_id: "agent".into(),
            status: agents_core::SessionStatus::Idle,
            created_at_ms: 0,
            last_input_at_ms: 0,
            input_revision: 0,
            pinned: pin.clone(),
            configuration: Some(configuration),
            workspace_snapshot: None,
            runtime_profile: Default::default(),
        };
        let prefix = legacy_prefix(&record, &[], Some("RESPONSE_ONLY"));
        assert!(prefix.contains("LATEST"));
        assert!(prefix.contains("RESPONSE_ONLY"));
        assert!(!prefix.contains("ORIGINAL"));
        assert_eq!(record.pinned.prefix_hash(), hash);
        assert_eq!(
            History {
                revision: 2,
                updates: vec![1, 4, 7]
            }
            .rebased(&[0, 1, 3, 7])
            .updates,
            vec![1, 3]
        );
    }
}
