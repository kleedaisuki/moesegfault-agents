//! Stable provider-prefix rendering shared by execution and count-only requests.

use crate::tools::{FunctionDefinition, ToolDefinition};
use agents_core::PinnedContext;

pub(crate) fn definitions(pinned: &PinnedContext) -> Vec<ToolDefinition> {
    registered_definitions(&pinned.tools)
}

pub(crate) fn registered_definitions(tools: &[agents_core::RegisteredTool]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|tool| ToolDefinition {
            kind: "function".into(),
            function: FunctionDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
                strict: None,
            },
        })
        .collect()
}

pub(crate) fn prefix(pinned: &PinnedContext) -> String {
    let mut prefix = "You are an agent running in moeSegFault Agents. Tools and skills below are pinned for this session.\n".to_owned();
    if pinned.prompt_layout == Some(agents_core::PromptLayout::AppendOnlyV1) {
        prefix.push_str(crate::prompt_context::POLICY);
    }
    prefix.push_str(&pinned.instructions);
    for skill in &pinned.skills {
        prefix.push_str(&format!(
            "\n\n<skill name={:?} version={:?}>\n{}\n</skill>",
            skill.name, skill.version, skill.instructions
        ));
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use agents_core::{CreateAgent, RegisteredSkill};

    #[test]
    fn execution_prefix_bytes_remain_identical_after_extraction() {
        let agent = serde_json::from_value::<CreateAgent>(
            serde_json::json!({"name":"fixture","instructions":"Keep exact 雪."}),
        )
        .unwrap()
        .into_config("agent_fixture".into(), "owner".into(), 0);
        let pinned = PinnedContext::resolve(
            &agent,
            vec![],
            vec![RegisteredSkill {
                name: "review".into(),
                version: "v1".into(),
                instructions: "Read before edits.".into(),
            }],
        );
        assert_eq!(prefix(&pinned).as_bytes(),
            b"You are an agent running in moeSegFault Agents. Tools and skills below are pinned for this session.\nKeep exact \xe9\x9b\xaa.\n\n<skill name=\"review\" version=\"v1\">\nRead before edits.\n</skill>");
        assert!(definitions(&pinned).is_empty());
    }
}
