use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{AgentConfig, ModelConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct CompactionPercent(u8);

impl CompactionPercent {
    pub fn new(value: u8) -> Result<Self, &'static str> {
        if (1..=99).contains(&value) {
            Ok(Self(value))
        } else {
            Err("compaction_percent must be between 1 and 99")
        }
    }

    pub const fn get(self) -> u8 {
        self.0
    }

    pub fn threshold_tokens(self, context_window: u32) -> u32 {
        (u64::from(context_window) * u64::from(self.0) / 100) as u32
    }

    pub fn should_compact(self, input_tokens: u32, model: &ModelConfig) -> bool {
        let safe_input = model.context_window.saturating_sub(model.max_output_tokens);
        input_tokens >= self.threshold_tokens(model.context_window).min(safe_input)
    }
}

impl Default for CompactionPercent {
    fn default() -> Self {
        Self(80)
    }
}

impl TryFrom<u8> for CompactionPercent {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CompactionPercent> for u8 {
    fn from(value: CompactionPercent) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredTool {
    pub name: String,
    pub version: String,
    pub execution: ToolExecution,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecution {
    Harness,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredSkill {
    pub name: String,
    pub version: String,
    pub instructions: String,
}

/// Store the complete resolved registry, never live references to mutable entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedContext {
    pub agent_revision: u64,
    pub model: ModelConfig,
    pub instructions: String,
    pub tools: Vec<RegisteredTool>,
    pub skills: Vec<RegisteredSkill>,
    pub compaction_percent: CompactionPercent,
    pub workspace: bool,
}

impl PinnedContext {
    pub fn resolve(
        agent: &AgentConfig,
        mut tools: Vec<RegisteredTool>,
        mut skills: Vec<RegisteredSkill>,
    ) -> Self {
        tools.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
        skills.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
        Self {
            agent_revision: agent.revision,
            model: agent.model.clone(),
            instructions: agent.instructions.clone(),
            tools,
            skills,
            compaction_percent: agent.compaction_percent,
            workspace: agent.workspace,
        }
    }

    /// Stable bytes are persisted with a session and reused verbatim on every turn.
    pub fn prefix_bytes(&self) -> Vec<u8> {
        let value =
            serde_json::to_value(self).expect("pinned context contains only JSON-safe data");
        let mut bytes = Vec::new();
        canonical_json(&value, &mut bytes);
        bytes
    }

    pub fn prefix_hash(&self) -> String {
        format!("{:x}", Sha256::digest(self.prefix_bytes()))
    }
}

fn canonical_json(value: &serde_json::Value, out: &mut Vec<u8>) {
    match value {
        serde_json::Value::Object(map) => {
            out.push(b'{');
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).expect("JSON object key");
                out.push(b':');
                canonical_json(value, out);
            }
            out.push(b'}');
        }
        serde_json::Value::Array(array) => {
            out.push(b'[');
            for (index, value) in array.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                canonical_json(value, out);
            }
            out.push(b']');
        }
        _ => serde_json::to_writer(out, value).expect("JSON value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn threshold_is_proportional_and_never_overflows(p in 1u8..=99, window in 1u32..=u32::MAX) {
            let threshold = CompactionPercent::new(p).unwrap().threshold_tokens(window);
            prop_assert!(threshold <= window);
            prop_assert_eq!(u64::from(threshold), u64::from(window) * u64::from(p) / 100);
        }

        #[test]
        fn invalid_percent_cannot_cross_json_boundary(p in any::<u8>()) {
            let value = serde_json::from_str::<CompactionPercent>(&p.to_string());
            prop_assert_eq!(value.is_ok(), (1..=99).contains(&p));
        }

        #[test]
        fn personal_skill_pins_survive_registry_edits_and_removal(
            original in ".{1,128}", replacement in ".{1,128}",
        ) {
            prop_assume!(original != replacement);
            let mut agent = serde_json::from_value::<crate::CreateAgent>(
                serde_json::json!({"name":"a","skills":["review"]})
            ).unwrap().into_config("a".into(), "owner".into(), 0);
            let mut registry = vec![RegisteredSkill {
                name: "review".into(), version: "first".into(), instructions: original.clone(),
            }];
            let pinned = PinnedContext::resolve(&agent, vec![], registry.clone());
            let bytes = pinned.prefix_bytes();
            let hash = pinned.prefix_hash();
            registry[0].instructions = replacement;
            registry[0].version = "second".into();
            let next = PinnedContext::resolve(&agent, vec![], registry.clone());
            prop_assert_ne!(next.prefix_bytes(), bytes.clone());
            registry.clear();
            agent.skills.clear();
            agent.instructions = "future sessions only".into();
            let restored: PinnedContext = serde_json::from_slice(&bytes).unwrap();
            prop_assert_eq!(restored.skills[0].instructions.as_str(), original.as_str());
            prop_assert_eq!(restored.prefix_bytes(), bytes.clone());
            prop_assert_eq!(pinned.prefix_bytes(), bytes);
            prop_assert_eq!(restored.prefix_hash(), hash);
        }
    }

    #[test]
    fn output_reservation_can_trigger_early_compaction() {
        let model = ModelConfig {
            context_window: 100,
            max_output_tokens: 30,
            ..Default::default()
        };
        assert!(CompactionPercent::default().should_compact(70, &model));
        assert!(!CompactionPercent::default().should_compact(69, &model));
    }

    #[test]
    fn registry_order_and_later_agent_changes_do_not_modify_pinned_prefix() {
        let mut agent =
            serde_json::from_value::<crate::CreateAgent>(serde_json::json!({"name":"a"}))
                .unwrap()
                .into_config("a".into(), "u".into(), 0);
        let time = RegisteredTool {
            name: "time".into(),
            version: "1".into(),
            execution: ToolExecution::Harness,
            description: "Get time".into(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
        };
        let bash = RegisteredTool {
            name: "bash".into(),
            execution: ToolExecution::Workspace,
            ..time.clone()
        };
        let pinned = PinnedContext::resolve(&agent, vec![time.clone(), bash.clone()], vec![]);
        let reversed = PinnedContext::resolve(&agent, vec![bash, time], vec![]);
        let bytes = pinned.prefix_bytes();
        assert_eq!(bytes, reversed.prefix_bytes());
        agent.instructions = "changed after session creation".into();
        agent.revision += 1;
        assert_eq!(bytes, pinned.prefix_bytes());
        let roundtrip: PinnedContext = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(roundtrip.prefix_hash(), pinned.prefix_hash());
    }
}
