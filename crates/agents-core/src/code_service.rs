//! Secret-free selection of a real code executor, independent of the outer LLM.
use serde::{Deserialize, Serialize};

/// Code memory is an executor capability, not a Cloudflare application class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodeMemory {
    #[serde(rename = "1g")]
    OneGiB,
    #[serde(rename = "4g")]
    FourGiB,
    #[serde(rename = "16g")]
    SixteenGiB,
    #[serde(rename = "64g")]
    SixtyFourGiB,
}

impl CodeMemory {
    pub const fn from_core(profile: crate::RuntimeProfile) -> Self {
        match profile {
            crate::RuntimeProfile::Basic1g => Self::OneGiB,
            crate::RuntimeProfile::Standard4g => Self::FourGiB,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeServiceProvider {
    #[default]
    Platform,
    WorkspaceDaemon,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeService {
    #[serde(default)]
    pub provider: CodeServiceProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
}

impl CodeService {
    pub fn is_platform(&self) -> bool {
        self.provider == CodeServiceProvider::Platform && self.credential_id.is_none()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        match (self.provider, self.credential_id.as_deref()) {
            (CodeServiceProvider::Platform, None) => Ok(()),
            (CodeServiceProvider::WorkspaceDaemon, Some(id))
                if !id.is_empty()
                    && id.len() <= 256
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)) =>
            {
                Ok(())
            }
            _ => Err("Code service requires its own compatible executor credential"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_executor_credential_is_not_implicit_outer_model_authority() {
        let platform = CodeService::default();
        assert!(platform.validate().is_ok() && platform.is_platform());
        let owned = CodeService {
            provider: CodeServiceProvider::WorkspaceDaemon,
            credential_id: Some("cred_owned_executor".into()),
        };
        assert!(owned.validate().is_ok() && !owned.is_platform());
        assert!(
            CodeService {
                credential_id: Some("cred_model".into()),
                ..CodeService::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            CodeService {
                provider: CodeServiceProvider::WorkspaceDaemon,
                credential_id: None
            }
            .validate()
            .is_err()
        );
        assert_eq!(
            serde_json::from_slice::<CodeService>(&serde_json::to_vec(&owned).unwrap()).unwrap(),
            owned
        );
    }
}
