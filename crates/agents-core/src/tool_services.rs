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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ImageCapabilityContract>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageOutputFormat {
    Png,
    Jpeg,
    Webp,
}

/// Owner-declared endpoint contract, not a platform verification or a grant of
/// additional compute, storage, byte, pixel or credential authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageCapabilityContract {
    pub version: u8,
    #[serde(default)]
    pub edit: bool,
    #[serde(default)]
    pub max_references: u8,
    #[serde(default)]
    pub mask: bool,
    #[serde(default)]
    pub partial_images: u8,
    #[serde(default = "png_output")]
    pub output_formats: BTreeSet<ImageOutputFormat>,
    #[serde(default)]
    pub transparent: bool,
    #[serde(default)]
    pub sizes: BTreeSet<String>,
    #[serde(default)]
    pub qualities: BTreeSet<String>,
    #[serde(default)]
    pub input_fidelity: BTreeSet<String>,
    #[serde(default)]
    pub moderation: BTreeSet<String>,
    #[serde(default)]
    pub output_compression: bool,
}

fn png_output() -> BTreeSet<ImageOutputFormat> {
    BTreeSet::from([ImageOutputFormat::Png])
}

impl ImageCapabilityContract {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1
            || self.max_references > 16
            || self.partial_images > 3
            || (!self.edit && (self.max_references > 0 || self.mask))
            || (self.edit && self.max_references == 0)
            || self.output_formats.is_empty()
            || self.sizes.len() > 32
            || self.qualities.len() > 6
            || !self.qualities.iter().all(|value| {
                ["auto", "low", "medium", "high", "xhigh", "max"].contains(&value.as_str())
            })
            || !self
                .input_fidelity
                .iter()
                .all(|value| ["low", "high"].contains(&value.as_str()))
            || !self
                .moderation
                .iter()
                .all(|value| ["auto", "low"].contains(&value.as_str()))
            || (self.transparent
                && self
                    .output_formats
                    .iter()
                    .all(|format| *format == ImageOutputFormat::Jpeg))
            || self.sizes.iter().any(|size| !image_dimensions(size))
        {
            return Err("Invalid image capability contract");
        }
        Ok(())
    }
}

fn image_dimensions(size: &str) -> bool {
    if size.len() > 16 {
        return false;
    }
    let Some((width, height)) = size.split_once('x') else {
        return false;
    };
    if !width.bytes().all(|b| b.is_ascii_digit()) || !height.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let (Ok(width), Ok(height)) = (width.parse::<u32>(), height.parse::<u32>()) else {
        return false;
    };
    width > 0
        && height > 0
        && width <= 16_384
        && height <= 16_384
        && u64::from(width) * u64::from(height) <= 64 * 1024 * 1024
}

impl ImageService {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(contract) = &self.capabilities {
            if self.provider == ImageServiceProvider::Platform {
                return Err("Platform image capabilities are operator managed");
            }
            contract.validate()?;
        }
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
    fn omitted_image_contract_keeps_legacy_route_bytes_unchanged() {
        let source = json!({"provider":"openai_images","credential_id":"owned","model":"custom"});
        let service: ImageService = serde_json::from_value(source.clone()).unwrap();
        service.validate().unwrap();
        assert_eq!(serde_json::to_value(service).unwrap(), source);
    }

    #[test]
    fn owner_image_contract_is_versioned_bounded_and_never_platform_authority() {
        let mut service: ImageService = serde_json::from_value(json!({"provider":"openai_images",
            "credential_id":"owned","model":"custom","capabilities":{"version":1,
                "edit":true,"max_references":2,"partial_images":1,"sizes":["1024x1024"]}}))
        .unwrap();
        service.validate().unwrap();
        service.capabilities.as_mut().unwrap().max_references = 17;
        assert!(service.validate().is_err());
        service.capabilities.as_mut().unwrap().max_references = 2;
        service
            .capabilities
            .as_mut()
            .unwrap()
            .sizes
            .insert("16384x16384".into());
        assert!(service.validate().is_err());
        service.capabilities.as_mut().unwrap().sizes.clear();
        service.provider = ImageServiceProvider::Platform;
        service.credential_id = None;
        assert!(service.validate().is_err());
    }

    #[test]
    fn image_contract_has_no_secret_or_unversioned_escape_hatch() {
        for value in [
            json!({"version":2}),
            json!({"version":1,"api_key":"secret"}),
            json!({"version":1,"edit":false,"mask":true}),
            json!({"version":1,"partial_images":4}),
        ] {
            match serde_json::from_value::<ImageCapabilityContract>(value) {
                Ok(contract) => assert!(contract.validate().is_err()),
                Err(_) => {}
            }
        }
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
