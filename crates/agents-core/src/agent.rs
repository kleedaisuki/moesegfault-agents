use serde::{Deserialize, Serialize};

use crate::{CompactionPercent, ToolServices};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    #[serde(rename = "openrouter", alias = "open_router")]
    OpenRouter,
    #[serde(rename = "deepseek", alias = "deep_seek")]
    DeepSeek,
    Byok,
}

/// Wire protocol, independent of the company hosting a user-owned endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    #[default]
    Openai,
    Responses,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCredentialFamily {
    Openai,
    Anthropic,
}

impl ProviderProtocol {
    pub fn credential_family(self) -> ProviderCredentialFamily {
        match self {
            Self::Openai | Self::Responses => ProviderCredentialFamily::Openai,
            Self::Anthropic => ProviderCredentialFamily::Anthropic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ModelInput")]
pub struct ModelConfig {
    #[serde(default)]
    pub provider: Provider,
    pub model: String,
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    #[serde(default = "default_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProviderProtocol>,
}

#[derive(Deserialize)]
struct ModelInput {
    #[serde(default)]
    provider: Provider,
    model: String,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    credential_id: Option<String>,
    #[serde(default)]
    protocol: Option<ProviderProtocol>,
}

impl From<ModelInput> for ModelConfig {
    fn from(input: ModelInput) -> Self {
        Self {
            provider: input.provider,
            model: input.model,
            context_window: input.context_window.unwrap_or_else(|| {
                if input.provider == Provider::Byok {
                    131_072
                } else {
                    default_context_window()
                }
            }),
            max_output_tokens: input
                .max_output_tokens
                .unwrap_or_else(default_output_tokens),
            credential_id: input.credential_id,
            protocol: input.protocol,
        }
    }
}

fn default_context_window() -> u32 {
    1_048_576
}

fn default_output_tokens() -> u32 {
    8_192
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: Provider::OpenRouter,
            model: "deepseek/deepseek-v4.1-flash-1017".into(),
            context_window: default_context_window(),
            max_output_tokens: default_output_tokens(),
            credential_id: None,
            protocol: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub owner_id: String,
    pub name: String,
    pub revision: u64,
    pub model: ModelConfig,
    pub instructions: String,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
    pub compaction_percent: CompactionPercent,
    pub workspace: bool,
    #[serde(default, skip_serializing_if = "ToolServices::is_empty")]
    pub tool_services: ToolServices,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAgent {
    pub name: String,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub instructions: String,
    #[serde(default = "default_tools")]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub compaction_percent: CompactionPercent,
    #[serde(default)]
    pub workspace: bool,
    #[serde(default, skip_serializing_if = "ToolServices::is_empty")]
    pub tool_services: ToolServices,
}

fn default_tools() -> Vec<String> {
    vec!["time".into()]
}

impl CreateAgent {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.tool_services.validate()?;
        if self.name.trim().is_empty() || self.name.len() > 100 {
            return Err("name must contain 1 to 100 bytes");
        }
        if self.instructions.len() > 64 * 1024 {
            return Err("instructions exceed 64 KiB");
        }
        if self.model.model.is_empty()
            || self.model.context_window == 0
            || self.model.max_output_tokens >= self.model.context_window
        {
            return Err("invalid model context or output budget");
        }
        if self.model.provider == Provider::Byok && self.model.credential_id.is_none() {
            return Err("BYOK requires credential_id");
        }
        Ok(())
    }

    pub fn into_config(self, id: String, owner_id: String, now_ms: u64) -> AgentConfig {
        AgentConfig {
            id,
            owner_id,
            name: self.name,
            revision: 1,
            model: self.model,
            instructions: self.instructions,
            tools: self.tools,
            skills: self.skills,
            compaction_percent: self.compaction_percent,
            workspace: self.workspace,
            tool_services: self.tool_services,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    #[default]
    Free,
    Go,
    Plus,
    Pro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanLimits {
    pub max_agents: u32,
    pub max_concurrent_sessions: u32,
    pub max_concurrent_runs: u32,
}

impl Plan {
    /// The account service must enforce these with atomic admission, not list counts.
    pub const fn limits(self) -> PlanLimits {
        match self {
            Self::Free => PlanLimits {
                max_agents: 2,
                max_concurrent_sessions: 2,
                max_concurrent_runs: 1,
            },
            Self::Go => PlanLimits {
                max_agents: 10,
                max_concurrent_sessions: 5,
                max_concurrent_runs: 2,
            },
            Self::Plus => PlanLimits {
                max_agents: 30,
                max_concurrent_sessions: 15,
                max_concurrent_runs: 5,
            },
            Self::Pro => PlanLimits {
                max_agents: 200,
                max_concurrent_sessions: 100,
                max_concurrent_runs: 20,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_limits_match_approved_subscription_tiers() {
        for (plan, expected) in [
            (Plan::Free, (2, 2, 1)),
            (Plan::Go, (10, 5, 2)),
            (Plan::Plus, (30, 15, 5)),
            (Plan::Pro, (200, 100, 20)),
        ] {
            let limits = plan.limits();
            assert_eq!(
                (
                    limits.max_agents,
                    limits.max_concurrent_sessions,
                    limits.max_concurrent_runs
                ),
                expected
            );
        }
    }

    #[test]
    fn clients_select_model_without_manually_entering_context_limits() {
        let input: CreateAgent = serde_json::from_value(serde_json::json!({
            "name":"test", "model":{"provider":"openrouter","model":"deepseek/deepseek-v4.1-flash-1017"}
        }))
        .unwrap();
        assert!(input.validate().is_ok());
        assert_eq!(input.model.context_window, 1_048_576);
    }

    #[test]
    fn omitted_model_uses_the_deployed_openrouter_route() {
        let input: CreateAgent =
            serde_json::from_value(serde_json::json!({"name":"test"})).unwrap();
        assert_eq!(input.model.provider, Provider::OpenRouter);
        assert_eq!(input.model.model, "deepseek/deepseek-v4.1-flash-1017");
        assert_eq!(input.model.context_window, 1_048_576);
        assert_eq!(input.model.max_output_tokens, 8192);
    }

    #[test]
    fn provider_wire_names_are_conventional_and_old_names_remain_readable() {
        assert_eq!(
            serde_json::to_string(&Provider::OpenRouter).unwrap(),
            "\"openrouter\""
        );
        assert_eq!(
            serde_json::from_str::<Provider>("\"deep_seek\"").unwrap(),
            Provider::DeepSeek
        );
    }

    #[test]
    fn generic_byok_defaults_to_a_budget_not_an_invented_provider_limit() {
        let input: CreateAgent = serde_json::from_value(serde_json::json!({
            "name":"custom", "model":{"provider":"byok", "model":"private-model", "credential_id":"cred_example"}
        }))
        .unwrap();
        assert_eq!(input.model.context_window, 131_072);
        assert_eq!(input.model.max_output_tokens, 8192);
        assert!(input.validate().is_ok());
    }

    #[test]
    fn explicit_model_budgets_and_old_pins_survive_roundtrip() {
        let mut model = ModelConfig {
            provider: Provider::Byok,
            credential_id: Some("existing".into()),
            ..Default::default()
        };
        let encoded = serde_json::to_value(&model).unwrap();
        assert_eq!(
            serde_json::from_value::<ModelConfig>(encoded).unwrap(),
            model
        );
        model.context_window = 64_000;
        model.max_output_tokens = 4096;
        assert_eq!(
            serde_json::from_value::<ModelConfig>(serde_json::to_value(&model).unwrap()).unwrap(),
            model
        );
    }

    #[test]
    fn byok_protocols_describe_protocols_not_vendors() {
        for (name, protocol) in [
            ("openai", ProviderProtocol::Openai),
            ("responses", ProviderProtocol::Responses),
            ("anthropic", ProviderProtocol::Anthropic),
        ] {
            assert_eq!(
                serde_json::from_value::<ProviderProtocol>(name.into()).unwrap(),
                protocol
            );
            assert_eq!(serde_json::to_value(protocol).unwrap(), name);
        }
    }

    #[test]
    fn native_transport_is_explicit_and_uses_the_existing_credential_family() {
        let old = ModelConfig::default();
        assert!(old.protocol.is_none());
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("protocol")
                .is_none()
        );
        let mut native = old.clone();
        native.protocol = Some(ProviderProtocol::Responses);
        assert_eq!(
            serde_json::to_value(&native).unwrap()["protocol"],
            "responses"
        );
        assert_eq!(
            ProviderProtocol::Responses.credential_family(),
            ProviderProtocol::Openai.credential_family()
        );
        assert_ne!(
            ProviderProtocol::Responses.credential_family(),
            ProviderProtocol::Anthropic.credential_family()
        );
        assert_eq!(
            serde_json::from_value::<ModelConfig>(serde_json::to_value(&native).unwrap()).unwrap(),
            native
        );
        assert_eq!(
            serde_json::from_value::<ModelConfig>(serde_json::to_value(&old).unwrap()).unwrap(),
            old
        );
    }
}
