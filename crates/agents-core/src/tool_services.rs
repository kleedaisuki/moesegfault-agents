//! Secret-free routes for hosted tools, independent of the outer model route.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolServices {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_generation: Option<ImageService>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<SearchService>,
}

impl ToolServices {
    pub fn is_empty(&self) -> bool {
        self.image_generation.is_none() && self.web_search.is_none()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(image) = &self.image_generation {
            image.validate()?;
        }
        if let Some(search) = &self.web_search {
            search.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageServiceProvider {
    #[default]
    Platform,
    OpenaiImages,
    OpenrouterImages,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageService {
    #[serde(default)]
    pub provider: ImageServiceProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub asset_hosts: BTreeSet<String>,
}

impl ImageService {
    pub fn validate(&self) -> Result<(), &'static str> {
        match (self.provider, self.credential_id.as_deref()) {
            (ImageServiceProvider::Platform, None) => {}
            (ImageServiceProvider::Platform, Some(_)) => {
                return Err("Platform image service cannot select a user credential");
            }
            (_, Some(id)) if identifier(id) => {}
            _ => return Err("BYOK image service requires credential_id"),
        }
        if self
            .model
            .as_deref()
            .is_some_and(|model| !model_name(model))
            || self.asset_hosts.len() > 16
            || self.asset_hosts.iter().any(|host| !host_name(host))
        {
            return Err("Invalid image service route");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchServiceProvider {
    #[default]
    OpenaiResponses,
    DeepseekSearch,
    PlatformDeepseek,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchService {
    #[serde(default)]
    pub provider: SearchServiceProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "search_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "search_calls")]
    pub max_tool_calls: u8,
}

fn search_output_tokens() -> u32 {
    1024
}

fn search_calls() -> u8 {
    4
}

impl SearchService {
    pub fn validate(&self) -> Result<(), &'static str> {
        let route_valid = match self.provider {
            SearchServiceProvider::PlatformDeepseek => {
                self.credential_id.is_none() && self.model.is_none()
            }
            SearchServiceProvider::DeepseekSearch => {
                self.credential_id.as_deref().is_some_and(identifier) && self.model.is_none()
            }
            SearchServiceProvider::OpenaiResponses => {
                self.credential_id.as_deref().is_some_and(identifier)
                    && self.model.as_deref().is_some_and(model_name)
            }
        };
        if !route_valid
            || !(64..=8192).contains(&self.max_output_tokens)
            || !(1..=8).contains(&self.max_tool_calls)
        {
            return Err("Invalid search service route");
        }
        Ok(())
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn model_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn host_name(value: &str) -> bool {
    // The Worker applies the shared public-host/IDNA policy before HTTP. Core
    // rejects URLs and secret-bearing strings without implementing DNS policy.
    !value.is_empty()
        && value.len() <= 253
        && value.contains('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omitted_routes_keep_old_configuration_serialization_empty() {
        let routes: ToolServices = serde_json::from_value(json!({})).unwrap();
        assert!(routes.is_empty());
        assert_eq!(serde_json::to_value(routes).unwrap(), json!({}));
    }

    #[test]
    fn service_route_references_are_independent_from_outer_model_secrets() {
        let routes: ToolServices = serde_json::from_value(json!({
            "image_generation":{"provider":"openai_images","credential_id":"cred_image"},
            "web_search":{"credential_id":"cred_search","model":"search/model"},
        }))
        .unwrap();
        routes.validate().unwrap();
        assert_eq!(routes.web_search.unwrap().max_tool_calls, 4);
        assert!(serde_json::from_value::<ToolServices>(json!({
            "web_search":{"credential_id":"cred_search","model":"search/model","api_key":"secret"},
        })).is_err());
    }

    #[test]
    fn invalid_or_ambiguous_funding_routes_fail_before_resolution() {
        for value in [
            json!({"image_generation":{"provider":"platform","credential_id":"cred_x"}}),
            json!({"image_generation":{"provider":"openai_images"}}),
            json!({"web_search":{"credential_id":"","model":"search/model"}}),
            json!({"web_search":{"credential_id":"cred_x","model":"search/model","max_tool_calls":0}}),
            json!({"image_generation":{"asset_hosts":["https://private.example/key"]}}),
            json!({"web_search":{"provider":"platform_deepseek","credential_id":"cred_x"}}),
            json!({"web_search":{"provider":"deepseek_search","credential_id":"cred_x","model":"not-a-search-parameter"}}),
        ] {
            assert!(
                serde_json::from_value::<ToolServices>(value)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
    }

    #[test]
    fn platform_and_dedicated_byok_search_have_no_fake_model_route() {
        for value in [
            json!({"web_search":{"provider":"platform_deepseek"}}),
            json!({"web_search":{"provider":"deepseek_search","credential_id":"cred_search"}}),
        ] {
            let routes: ToolServices = serde_json::from_value(value).unwrap();
            routes.validate().unwrap();
            assert!(routes.web_search.unwrap().model.is_none());
        }
    }
}
