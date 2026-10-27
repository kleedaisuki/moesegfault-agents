//! Resolve credentials at the operation boundary, never into persisted state.

use agents_core::{ModelConfig, Provider, ProviderProtocol};
use worker::{Env, Error, Result};

use super::ProviderConfig;

pub async fn resolve_config(env: &Env, owner: &str, model: &ModelConfig) -> Result<ProviderConfig> {
    let (credential_protocol, base_url, api_key) = match model.provider {
        Provider::OpenRouter => (
            ProviderProtocol::Openai,
            "https://openrouter.ai/api/v1".into(),
            env.secret("OPENROUTER_API_KEY")?.to_string(),
        ),
        Provider::DeepSeek => (
            ProviderProtocol::Openai,
            "https://api.deepseek.com".into(),
            env.secret("DEEPSEEK_API_KEY")?.to_string(),
        ),
        Provider::Byok => {
            let id = model
                .credential_id
                .as_deref()
                .ok_or_else(|| Error::RustError("missing_credential".into()))?;
            let credential = crate::auth::provider_credential(env, owner, id).await?;
            (credential.protocol, credential.base_url, credential.api_key)
        }
    };
    let protocol = effective_protocol(credential_protocol, model.protocol)?;
    Ok(ProviderConfig {
        protocol,
        base_url,
        api_key,
        model: model.model.clone(),
        max_output_tokens: Some(model.max_output_tokens),
        session_id: None,
        traceparent: None,
    })
}

fn effective_protocol(
    credential: ProviderProtocol,
    selected: Option<ProviderProtocol>,
) -> Result<ProviderProtocol> {
    let protocol = selected.unwrap_or(credential);
    if protocol.credential_family() != credential.credential_family() {
        return Err(Error::RustError(
            "provider_credential_family_mismatch".into(),
        ));
    }
    Ok(protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_selection_preserves_legacy_defaults_and_credential_families() {
        for protocol in [ProviderProtocol::Openai, ProviderProtocol::Anthropic] {
            assert_eq!(effective_protocol(protocol, None).unwrap(), protocol);
        }
        assert_eq!(
            effective_protocol(ProviderProtocol::Openai, Some(ProviderProtocol::Responses))
                .unwrap(),
            ProviderProtocol::Responses
        );
        assert!(
            effective_protocol(
                ProviderProtocol::Anthropic,
                Some(ProviderProtocol::Responses)
            )
            .is_err()
        );
        assert!(
            effective_protocol(ProviderProtocol::Openai, Some(ProviderProtocol::Anthropic))
                .is_err()
        );
    }
}
