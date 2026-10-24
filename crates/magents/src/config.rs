use agents_core::{CreateAgent, ModelConfig};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io::Read, path::Path};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFile {
    schema_version: u32,
    name: String,
    #[serde(default)]
    instructions: String,
    #[serde(default = "default_tools")]
    tools: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default = "default_compaction")]
    compaction_percent: u8,
    #[serde(default)]
    workspace: bool,
    model: FileModel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileModel {
    provider: agents_core::Provider,
    model: String,
    context_window: u32,
    max_output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_id: Option<String>,
}

fn default_tools() -> Vec<String> {
    vec!["time".into()]
}

fn default_compaction() -> u8 {
    80
}

pub fn read_source(path: &Path) -> Result<String> {
    let source: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(path).context("Cannot read configuration")?)
    };
    let mut bytes = Vec::new();
    source.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        bail!("Configuration exceeds 1 MiB");
    }
    String::from_utf8(bytes).context("Configuration must be UTF-8")
}

impl AgentFile {
    pub fn parse(source: &str) -> Result<Self> {
        // Parser diagnostics can echo source lines, including accidentally pasted keys.
        let config: Self = toml::from_str(source).map_err(|_| {
            anyhow::anyhow!("Invalid agent TOML; check schema_version, field names and value types")
        })?;
        config.api_body()?;
        Ok(config)
    }

    pub fn read(path: &Path) -> Result<Self> {
        Self::parse(&read_source(path)?)
    }

    pub fn template() -> Result<Self> {
        Self::from_api(serde_json::json!({"name":"My agent", "model":ModelConfig::default()}))
    }

    pub fn from_api(value: Value) -> Result<Self> {
        let agent: CreateAgent =
            serde_json::from_value(value).context("API returned an invalid agent configuration")?;
        agent.validate().map_err(anyhow::Error::msg)?;
        Ok(Self {
            schema_version: 1,
            name: agent.name,
            instructions: agent.instructions,
            tools: agent.tools,
            skills: agent.skills,
            compaction_percent: agent.compaction_percent.get(),
            workspace: agent.workspace,
            model: FileModel {
                provider: agent.model.provider,
                model: agent.model.model,
                context_window: agent.model.context_window,
                max_output_tokens: agent.model.max_output_tokens,
                credential_id: agent.model.credential_id,
            },
        })
    }

    pub fn api_body(&self) -> Result<Value> {
        if self.schema_version != 1 {
            bail!("Unsupported agent schema_version; this CLI supports version 1");
        }
        let mut value = serde_json::to_value(self)?;
        value.as_object_mut().unwrap().remove("schema_version");
        let agent: CreateAgent = serde_json::from_value(value.clone())
            .map_err(|_| anyhow::anyhow!("Invalid agent model or compaction percentage"))?;
        agent.validate().map_err(anyhow::Error::msg)?;
        Ok(value)
    }

    pub fn toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("Cannot serialize agent TOML")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn template_is_complete_and_valid() {
        let template = AgentFile::template().unwrap();
        assert_eq!(
            AgentFile::parse(&template.toml().unwrap()).unwrap(),
            template
        );
    }

    #[test]
    fn editable_configuration_roundtrips_without_account_or_secrets() {
        for provider in ["openrouter", "deepseek", "byok"] {
            for context in [1024, 16384, 1048576] {
                for percent in [20, 50, 80, 95] {
                    let source = json!({
                        "id":"a_private", "owner_id":"owner_private", "revision":42,
                        "created_at_ms":123, "updated_at_ms":456, "api_key":"never_export",
                        "name":"Café 可莉", "instructions":"Inspect \"quotes\".\nThen use `bash`.\n",
                        "tools":["time","bash"], "skills":["rust-review"],
                        "compaction_percent":percent, "workspace":true,
                        "model":{"provider":provider,"model":"custom/model","context_window":context,
                          "max_output_tokens":512,"credential_id":"key_reference"}
                    });
                    let file = AgentFile::from_api(source).unwrap();
                    let encoded = file.toml().unwrap();
                    let restored = AgentFile::parse(&encoded).unwrap();
                    assert_eq!(restored, file);
                    for forbidden in [
                        "a_private",
                        "owner_private",
                        "never_export",
                        "revision",
                        "created_at_ms",
                    ] {
                        assert!(!encoded.contains(forbidden));
                    }
                    assert_eq!(
                        restored.api_body().unwrap()["model"]["context_window"],
                        context
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_fields_secrets_and_versions_are_not_silently_accepted() {
        let template = AgentFile::template().unwrap().toml().unwrap();
        for bad in [
            template.replace("schema_version = 1", "schema_version = 2"),
            format!("api_key = 'super-secret'\n{template}"),
            format!("{template}\napi_key = 'super-secret'"),
            template.replace("compaction_percent = 80", "compaction_percent = 0"),
            template.replace("max_output_tokens = 8192", "max_output_tokens = 1048576"),
        ] {
            let error = AgentFile::parse(&bad).unwrap_err().to_string();
            assert!(!error.contains("super-secret"));
        }
    }
}
