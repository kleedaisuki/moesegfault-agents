//! Prepare one owner-scoped image operation before Session's durable dispatch
//! fence. This module never retries generation or owns funding/artifact state.

use std::future::Future;

use agents_core::{
    ImageCapabilityContract, ImageOutputFormat, ImageService, ImageServiceProvider,
    ProviderCredentialFamily,
};
use serde::{Deserialize, Serialize};
use worker::{Env, Result};

use crate::{
    hosted_bindings::{ResolvedBinding, ResolvedTool},
    image_generation::{
        self, Capabilities, ImageApi, ImageConfig, ImageInput, ImageOptions, ImageResult,
        MAX_IMAGE_BYTES, PartialImage, Raster, RouterEndpoint,
    },
    telemetry::TraceContext,
};

const SEEDREAM: &str = "bytedance-seed/seedream-5-0-flash";
const OPENROUTER: &str = "https://openrouter.ai/api/v1";

/// Immutable operator facts retained with the dispatch intent. No secret or
/// credential identifier is needed to retry an anonymous known-result download.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostedImageProfile {
    pub api: ImageApi,
    pub base_url: String,
    pub model: String,
    pub public_model: Option<String>,
    pub capabilities: Capabilities,
    pub router_endpoint: Option<RouterEndpoint>,
    pub asset_hosts: Vec<String>,
    pub price_revision: String,
    pub reservation_microusd: u64,
    pub platform_funded: bool,
    #[serde(default)]
    pub provenance: ProfileProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_contract: Option<ImageCapabilityContract>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProfileProvenance {
    #[default]
    VerifiedOperator,
    OwnerDeclaredMinimal,
    OwnerDeclaredContract,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profiles {
    version: u32,
    profiles: Vec<HostedImageProfile>,
}

/// Intentionally neither Serialize nor Debug: this owns a decrypted service key
/// and user input until it is consumed by the single external dispatch.
pub(crate) struct PreparedHostedImage {
    profile: HostedImageProfile,
    config: ImageConfig,
    input: ImageInput,
    execution_id: String,
}

pub(crate) async fn prepare(
    env: &Env,
    owner: &str,
    binding: &ResolvedBinding,
    arguments: &str,
    execution_id: &str,
    trace: Option<&TraceContext>,
) -> Result<PreparedHostedImage> {
    let ResolvedTool::Image {
        options,
        service,
        allowed_file_ids,
        ..
    } = &binding.tool
    else {
        return Err(error("hosted_image_binding_mismatch"));
    };
    service
        .validate()
        .map_err(|_| error("invalid_image_binding"))?;
    validate_execution_id(execution_id)?;
    options.validate()?;
    let arguments = crate::hosted_tools::parse_image(arguments, allowed_file_ids).map_err(error)?;
    let profile = resolve_profile(env, owner, binding).await?;
    let (api, base_url, key, funded) = resolve_credential(env, owner, service).await?;
    if profile.api != api || profile.base_url != base_url || profile.platform_funded != funded {
        return Err(error("image_generation_credential_route_changed"));
    }
    let config = profile.config(key, trace.map(TraceContext::traceparent));
    if arguments.image_file_ids.len() > config.capabilities.max_references {
        return Err(error("image_generation_reference_limit"));
    }
    let mut images = Vec::with_capacity(arguments.image_file_ids.len());
    let mut input_bytes = 0usize;
    for id in &arguments.image_file_ids {
        let image = read_image(env, owner, id).await?;
        input_bytes = input_bytes.saturating_add(image.len());
        if input_bytes > MAX_IMAGE_BYTES {
            return Err(error("image_generation_input_too_large"));
        }
        images.push(image);
    }
    let mask = load_mask(env, owner, options, allowed_file_ids).await?;
    if input_bytes.saturating_add(mask.as_ref().map_or(0, Raster::len)) > MAX_IMAGE_BYTES {
        return Err(error("image_generation_input_too_large"));
    }
    let input = ImageInput {
        prompt: arguments.prompt,
        images,
        mask,
        options: options.clone(),
    };
    // Capability/options/body construction failures happen before paid intent.
    // The transport revalidates immediately before HTTP as defense in depth.
    drop(image_generation::prepare(&config, &input)?);
    Ok(PreparedHostedImage {
        profile,
        config,
        input,
        execution_id: execution_id.into(),
    })
}

/// Shared count/generation preflight. Metadata lookup authorizes the exact owner
/// origin without decrypting credentials, reading images or contacting providers.
pub(crate) async fn resolve_profile(
    env: &Env,
    owner: &str,
    binding: &ResolvedBinding,
) -> Result<HostedImageProfile> {
    let ResolvedTool::Image {
        options,
        service,
        profile: pinned,
        ..
    } = &binding.tool
    else {
        return Err(error("hosted_image_binding_mismatch"));
    };
    service
        .validate()
        .map_err(|_| error("invalid_image_binding"))?;
    let (api, base, funded) = if service.provider == ImageServiceProvider::Platform {
        (ImageApi::Openrouter, OPENROUTER.into(), true)
    } else {
        let metadata = crate::auth::provider_metadata(
            env,
            owner,
            service
                .credential_id
                .as_deref()
                .ok_or_else(|| error("image_byok_required"))?,
        )
        .await?;
        if metadata.protocol.credential_family() != ProviderCredentialFamily::Openai {
            return Err(error("image_generation_credential_protocol_mismatch"));
        }
        let api = if service.provider == ImageServiceProvider::OpenaiImages {
            ImageApi::Openai
        } else {
            ImageApi::Openrouter
        };
        let base =
            crate::auth::provider_keys::normalize_base_url(metadata.protocol, &metadata.base_url)?;
        (api, base, false)
    };
    if let Some(profile) = pinned {
        if profile.api != api
            || profile.base_url != base
            || profile.platform_funded != funded
            || service
                .model
                .as_ref()
                .is_some_and(|model| model != &profile.model)
            || options.model.as_ref().is_some_and(|model| {
                model != &profile.model && Some(model) != profile.public_model.as_ref()
            })
        {
            return Err(error("image_generation_credential_route_changed"));
        }
        validate_profile_options(profile, options)?;
        return Ok(profile.clone());
    }
    let profiles = match env.var("IMAGE_GENERATION_PROFILE_JSON") {
        Ok(source) => parse_profiles(&source.to_string())?,
        Err(_) if !funded => Vec::new(),
        Err(_) => return Err(error("image_generation_profile_not_configured")),
    };
    let mut profile = select_or_declare(
        &profiles,
        api,
        &base,
        funded,
        service,
        options.model.as_deref(),
    )?;
    // Narrow once before pinning, so counting and paid execution retain exactly
    // the same anonymous asset-origin authority.
    if !service.asset_hosts.is_empty() {
        if service
            .asset_hosts
            .iter()
            .any(|host| !profile.asset_hosts.contains(host))
        {
            return Err(error("image_generation_asset_origin_not_approved"));
        }
        profile
            .asset_hosts
            .retain(|host| service.asset_hosts.contains(host));
    }
    validate_profile_options(&profile, options)?;
    Ok(profile)
}

impl PreparedHostedImage {
    pub(crate) fn plan(&self) -> &HostedImageProfile {
        &self.profile
    }

    /// Consume only after persisting the intent and resource/credit reservation.
    /// Any error after invocation is potentially uncertain, never retryable here.
    pub(crate) async fn dispatch<F, Fut, C, Captured>(
        self,
        partial: F,
        capture: C,
    ) -> std::result::Result<image_generation::ImageExchange, image_generation::ImageFailure>
    where
        F: FnMut(PartialImage) -> Fut,
        Fut: Future<Output = Result<()>>,
        C: FnMut(image_generation::ProviderReply) -> Captured,
        Captured: Future<Output = Result<image_generation::ProviderReply>>,
    {
        image_generation::generate(
            &self.config,
            &self.input,
            &self.execution_id,
            partial,
            capture,
        )
        .await
    }
}

impl HostedImageProfile {
    fn config(&self, api_key: String, traceparent: Option<String>) -> ImageConfig {
        ImageConfig {
            api: self.api,
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            public_model: self.public_model.clone(),
            api_key,
            capabilities: self.capabilities.clone(),
            asset_hosts: self.asset_hosts.clone(),
            traceparent,
            router_endpoint: self.router_endpoint.clone(),
        }
    }

    /// Restore the fenced profile, not today's operator registry or credential.
    /// This only materializes an already-known outcome; it cannot issue a POST.
    pub(crate) async fn materialize(&self, result: &ImageResult) -> Result<Raster> {
        validate_profile(self)?;
        image_generation::materialize(&self.config(String::new(), None), result).await
    }
}

async fn resolve_credential(
    env: &Env,
    owner: &str,
    service: &ImageService,
) -> Result<(ImageApi, String, String, bool)> {
    if service.provider == ImageServiceProvider::Platform {
        return Ok((
            ImageApi::Openrouter,
            OPENROUTER.into(),
            env.secret("OPENROUTER_API_KEY")?.to_string(),
            true,
        ));
    }
    let credential = crate::auth::provider_credential(
        env,
        owner,
        service
            .credential_id
            .as_deref()
            .ok_or_else(|| error("image_byok_required"))?,
    )
    .await?;
    if credential.protocol.credential_family() != ProviderCredentialFamily::Openai {
        return Err(error("image_generation_credential_protocol_mismatch"));
    }
    let api = match service.provider {
        ImageServiceProvider::OpenaiImages => ImageApi::Openai,
        ImageServiceProvider::OpenrouterImages => ImageApi::Openrouter,
        ImageServiceProvider::Platform => unreachable!(),
    };
    Ok((api, credential.base_url, credential.api_key, false))
}

fn parse_profiles(source: &str) -> Result<Vec<HostedImageProfile>> {
    if source.len() > 256 * 1024 {
        return Err(error("invalid_image_generation_profiles"));
    }
    let registry: Profiles =
        serde_json::from_str(source).map_err(|_| error("invalid_image_generation_profiles"))?;
    if registry.version != 1 || registry.profiles.is_empty() || registry.profiles.len() > 64 {
        return Err(error("invalid_image_generation_profiles"));
    }
    for profile in &registry.profiles {
        if profile.provenance != ProfileProvenance::VerifiedOperator {
            return Err(error("invalid_operator_image_profile_provenance"));
        }
        validate_profile(profile)?;
    }
    Ok(registry.profiles)
}

fn validate_profile(profile: &HostedImageProfile) -> Result<()> {
    let base = crate::auth::provider_keys::normalize_base_url(
        agents_core::ProviderProtocol::Openai,
        &profile.base_url,
    )?;
    if base != profile.base_url
        || profile.model.trim().is_empty()
        || profile.model.len() > 256
        || profile.model.chars().any(char::is_control)
        || profile.price_revision.is_empty()
        || profile.price_revision.len() > 256
        || profile.asset_hosts.len() > 16
        || profile.capabilities.max_references > 16
    {
        return Err(error("invalid_image_generation_profile"));
    }
    for host in &profile.asset_hosts {
        let parsed = worker::Url::parse(&format!("https://{host}"))
            .map_err(|_| error("invalid_image_generation_asset_origin"))?;
        if parsed.host_str() != Some(host.as_str())
            || parsed.port().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(error("invalid_image_generation_asset_origin"));
        }
        crate::auth::provider_keys::normalize_base_url(
            agents_core::ProviderProtocol::Openai,
            &format!("https://{host}"),
        )?;
    }
    if profile.platform_funded {
        let pin = profile.router_endpoint.as_ref();
        if profile.provenance != ProfileProvenance::VerifiedOperator
            || profile.api != ImageApi::Openrouter
            || profile.base_url != OPENROUTER
            || profile.model != SEEDREAM
            || profile.reservation_microusd != 18_000
            || !pin.is_some_and(|pin| {
                pin.provider_slug == "seed"
                    && pin.provider_tag == "seed"
                    && pin.passthrough_parameters.is_empty()
            })
        {
            return Err(error("invalid_platform_image_generation_profile"));
        }
    } else if profile.reservation_microusd != 0 {
        return Err(error("image_byok_cannot_reserve_model_credit"));
    }
    if (profile.api == ImageApi::Openrouter) != profile.router_endpoint.is_some()
        && profile.provenance == ProfileProvenance::VerifiedOperator
    {
        return Err(error("invalid_image_generation_endpoint_pin"));
    }
    if let Some(contract) = &profile.declared_contract {
        contract
            .validate()
            .map_err(|_| error("invalid_image_binding"))?;
    }
    Ok(())
}

fn select_or_declare(
    profiles: &[HostedImageProfile],
    api: ImageApi,
    base: &str,
    funded: bool,
    service: &ImageService,
    requested: Option<&str>,
) -> Result<HostedImageProfile> {
    match select_profile(
        profiles,
        api,
        base,
        funded,
        service.model.as_deref(),
        requested,
    ) {
        Ok(profile) => return Ok(profile.clone()),
        Err(worker::Error::RustError(code))
            if !funded && code == "image_generation_profile_not_available" => {}
        Err(error) => return Err(error),
    }
    let model = service
        .model
        .as_deref()
        .or(requested)
        .ok_or_else(|| error("image_byok_model_required"))?;
    if requested.is_some_and(|requested| requested != model) {
        return Err(error("image_generation_model_route_mismatch"));
    }
    let capabilities = declared_capabilities(service.capabilities.as_ref())?;
    if api == ImageApi::Openrouter && capabilities.mask {
        return Err(error("image_generation_mask_unavailable"));
    }
    let profile = HostedImageProfile {
        api,
        base_url: base.into(),
        model: model.into(),
        public_model: None,
        capabilities,
        router_endpoint: None,
        asset_hosts: service.asset_hosts.iter().cloned().collect(),
        price_revision: "owner-declared-image-v1".into(),
        reservation_microusd: 0,
        platform_funded: false,
        provenance: if service.capabilities.is_some() {
            ProfileProvenance::OwnerDeclaredContract
        } else {
            ProfileProvenance::OwnerDeclaredMinimal
        },
        declared_contract: service.capabilities.clone(),
    };
    validate_profile(&profile)?;
    Ok(profile)
}

fn declared_capabilities(contract: Option<&ImageCapabilityContract>) -> Result<Capabilities> {
    use crate::image_generation::Parameter;
    let mut caps = Capabilities {
        omit_default_output_format: true,
        ..Default::default()
    };
    let Some(contract) = contract else {
        return Ok(caps);
    };
    contract
        .validate()
        .map_err(|_| error("invalid_image_binding"))?;
    caps.streaming = contract.partial_images > 0;
    caps.max_references = usize::from(contract.max_references);
    caps.mask = contract.mask;
    caps.parameters.insert(
        "output_format".into(),
        Parameter::Enum {
            values: contract
                .output_formats
                .iter()
                .map(|format| serde_json::to_value(format).unwrap())
                .collect(),
        },
    );
    for (name, values) in [
        ("size", &contract.sizes),
        ("quality", &contract.qualities),
        ("input_fidelity", &contract.input_fidelity),
        ("moderation", &contract.moderation),
    ] {
        if !values.is_empty() {
            caps.parameters.insert(
                name.into(),
                Parameter::Enum {
                    values: values
                        .iter()
                        .map(|value| serde_json::json!(value))
                        .collect(),
                },
            );
        }
    }
    if contract.transparent {
        caps.parameters.insert(
            "background".into(),
            Parameter::Enum {
                values: vec![
                    serde_json::json!("opaque"),
                    serde_json::json!("transparent"),
                ],
            },
        );
    }
    if contract.output_compression {
        caps.parameters.insert(
            "output_compression".into(),
            Parameter::Range { min: 0, max: 100 },
        );
    }
    Ok(caps)
}

pub(crate) fn validate_profile_options(
    profile: &HostedImageProfile,
    options: &ImageOptions,
) -> Result<()> {
    validate_profile(profile)?;
    if let Some(contract) = &profile.declared_contract {
        if options.partial_images.unwrap_or(0) > contract.partial_images {
            return Err(error("image_generation_streaming_unavailable"));
        }
        if options.output_format.is_none()
            && !contract.output_formats.contains(&ImageOutputFormat::Png)
        {
            return Err(error("image_generation_option_unavailable"));
        }
    }
    image_generation::validate_options(&profile.capabilities, options)?;
    validate_size(profile, options)
}

fn select_profile<'a>(
    profiles: &'a [HostedImageProfile],
    api: ImageApi,
    base: &str,
    funded: bool,
    service_model: Option<&str>,
    requested_model: Option<&str>,
) -> Result<&'a HostedImageProfile> {
    let mut matching = profiles.iter().filter(|profile| {
        profile.api == api
            && profile.base_url == base
            && profile.platform_funded == funded
            && service_model.map_or(true, |model| profile.model == model)
            && requested_model.map_or(true, |model| {
                profile.model == model || profile.public_model.as_deref() == Some(model)
            })
    });
    let profile = matching
        .next()
        .ok_or_else(|| error("image_generation_profile_not_available"))?;
    if matching.next().is_some() {
        return Err(error("ambiguous_image_generation_profile"));
    }
    Ok(profile)
}

async fn read_image(env: &Env, owner: &str, id: &str) -> Result<Raster> {
    let bytes = crate::space::read_bytes(env, owner, id, MAX_IMAGE_BYTES as u64)
        .await?
        .ok_or_else(|| error("image_generation_input_too_large"))?;
    Raster::from_bytes(&bytes)
}

async fn load_mask(
    env: &Env,
    owner: &str,
    options: &ImageOptions,
    allowed_ids: &[String],
) -> Result<Option<Raster>> {
    let Some(mask) = &options.input_image_mask else {
        return Ok(None);
    };
    if let Some(id) = &mask.file_id {
        if !allowed_ids.contains(id) {
            return Err(error("image_generation_input_not_declared"));
        }
        return read_image(env, owner, id).await.map(Some);
    }
    let encoded = mask
        .image_url
        .as_deref()
        .and_then(|url| url.strip_prefix("data:image/png;base64,"))
        .ok_or_else(|| error("invalid_image_generation_mask"))?;
    Raster::from_base64(encoded).map(Some)
}

fn validate_execution_id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(error("invalid_image_generation_operation"));
    }
    Ok(())
}

fn validate_size(profile: &HostedImageProfile, options: &ImageOptions) -> Result<()> {
    // OpenRouter's explicit-pixel size shorthand is documented independently of
    // endpoint discovery. Boolean presence is not evidence of unlimited tiers:
    // https://openrouter.ai/docs/guides/overview/multimodal/image-generation#resolution-and-aspect-ratio
    if !profile.platform_funded || profile.model != SEEDREAM || profile.api != ImageApi::Openrouter
    {
        return Ok(());
    }
    let Some(size) = options.size.as_deref().filter(|size| *size != "auto") else {
        return Ok(());
    };
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .ok_or_else(|| error("invalid_image_generation_size"))?;
    if width.max(height) > 2048 {
        return Err(error("image_generation_size_not_supported"));
    }
    Ok(())
}

fn error(code: &str) -> worker::Error {
    worker::Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seedream() -> HostedImageProfile {
        let endpoint = json!({"provider_slug":"seed","provider_tag":"seed",
            "allowed_passthrough_parameters":[],"supports_streaming":false,
            "supported_parameters":{"resolution":{"type":"enum","values":["1K","2K"]},
                "aspect_ratio":{"type":"enum","values":["1:1","16:9"]},
                "input_references":{"type":"range","min":0,"max":14}}});
        HostedImageProfile {
            api: ImageApi::Openrouter,
            base_url: OPENROUTER.into(),
            model: SEEDREAM.into(),
            public_model: None,
            capabilities: Capabilities::from_openrouter_endpoint(&endpoint).unwrap(),
            router_endpoint: Some(RouterEndpoint::from_catalog(&endpoint).unwrap()),
            asset_hosts: vec![],
            price_revision: "catalog-2026-10-25".into(),
            reservation_microusd: 18_000,
            platform_funded: true,
            provenance: ProfileProvenance::VerifiedOperator,
            declared_contract: None,
        }
    }

    #[test]
    fn platform_route_and_price_are_explicit_not_model_prefix_guesses() {
        let mut profile = seedream();
        validate_profile(&profile).unwrap();
        profile.model = "openai/gpt-image-2.5-flare".into();
        assert!(validate_profile(&profile).is_err());
        profile = seedream();
        profile.reservation_microusd = 18;
        assert!(validate_profile(&profile).is_err());
        profile = seedream();
        profile.router_endpoint.as_mut().unwrap().provider_tag = "other".into();
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn byok_profiles_bind_exact_origin_model_and_funding() {
        let mut byok = seedream();
        byok.platform_funded = false;
        byok.reservation_microusd = 0;
        validate_profile(&byok).unwrap();
        let profiles = vec![seedream(), byok];
        assert!(
            select_profile(
                &profiles,
                ImageApi::Openrouter,
                OPENROUTER,
                false,
                Some(SEEDREAM),
                None
            )
            .is_ok()
        );
        assert!(
            select_profile(
                &profiles,
                ImageApi::Openrouter,
                "https://unreviewed.example/v1",
                false,
                Some(SEEDREAM),
                None
            )
            .is_err()
        );
        assert!(
            select_profile(
                &profiles,
                ImageApi::Openrouter,
                OPENROUTER,
                false,
                Some("unknown"),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn profile_registry_version_duplicates_and_secret_free_roundtrip() {
        let profile = seedream();
        let source = json!({"version":1,"profiles":[profile]}).to_string();
        let parsed = parse_profiles(&source).unwrap();
        assert!(!source.contains("api_key"));
        assert!(!source.contains("credential_id"));
        assert!(parse_profiles(&source.replace("\"version\":1", "\"version\":2")).is_err());
        let duplicates = vec![parsed[0].clone(), parsed[0].clone()];
        assert!(
            select_profile(
                &duplicates,
                ImageApi::Openrouter,
                OPENROUTER,
                true,
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn owned_ids_and_operation_ids_are_not_provider_supplied_capabilities() {
        let allowed = vec!["file_owned".to_owned()];
        assert!(
            crate::hosted_tools::parse_image(
                r#"{"prompt":"edit","image_file_ids":["file_other"]}"#,
                &allowed
            )
            .is_err()
        );
        assert!(validate_execution_id("tool:canonical:1").is_ok());
        assert!(validate_execution_id("unsafe\r\nheader").is_err());
    }

    #[test]
    fn seedream_size_shorthand_does_not_grant_unadvertised_4k() {
        let profile = seedream();
        for size in ["1024x1024", "2048x2048"] {
            let options = ImageOptions {
                size: Some(size.into()),
                ..Default::default()
            };
            validate_size(&profile, &options).unwrap();
        }
        let options = ImageOptions {
            size: Some("4096x4096".into()),
            ..Default::default()
        };
        assert!(validate_size(&profile, &options).is_err());
    }

    #[test]
    fn unknown_byok_model_without_operator_registry_has_minimal_declared_contract() {
        let service: ImageService = serde_json::from_value(json!({"provider":"openai_images",
            "credential_id":"owner-key","model":"unknown-selfhost-model"}))
        .unwrap();
        let profile = select_or_declare(
            &[],
            ImageApi::Openai,
            "https://images.owner.example/v1",
            false,
            &service,
            None,
        )
        .unwrap();
        assert!(profile.provenance == ProfileProvenance::OwnerDeclaredMinimal);
        assert_eq!(profile.reservation_microusd, 0);
        assert_eq!(profile.capabilities.max_references, 0);
        assert!(!profile.capabilities.streaming);
        validate_profile_options(&profile, &ImageOptions::default()).unwrap();
        let mut options = ImageOptions::default();
        options.partial_images = Some(1);
        assert!(validate_profile_options(&profile, &options).is_err());
        options.partial_images = None;
        options.size = Some("1024x1024".into());
        assert!(validate_profile_options(&profile, &options).is_err());
        options.size = None;
        options.background = Some("transparent".into());
        assert!(validate_profile_options(&profile, &options).is_err());
    }

    #[test]
    fn owner_declared_advanced_profile_and_verified_precedence_are_not_model_guesses() {
        let service: ImageService = serde_json::from_value(json!({"provider":"openai_images",
            "credential_id":"owner-key","model":"unknown-selfhost-model","capabilities":{
                "version":1,"edit":true,"max_references":2,"mask":true,"partial_images":1,
                "output_formats":["png","webp"],"transparent":true,"sizes":["1024x1024"]}}))
        .unwrap();
        let profile = select_or_declare(
            &[],
            ImageApi::Openai,
            "https://images.owner.example/v1",
            false,
            &service,
            None,
        )
        .unwrap();
        assert!(profile.provenance == ProfileProvenance::OwnerDeclaredContract);
        let mut options = ImageOptions {
            partial_images: Some(1),
            background: Some("transparent".into()),
            size: Some("1024x1024".into()),
            ..Default::default()
        };
        validate_profile_options(&profile, &options).unwrap();
        options.partial_images = Some(2);
        assert!(validate_profile_options(&profile, &options).is_err());
        let mut verified = profile.clone();
        verified.provenance = ProfileProvenance::VerifiedOperator;
        verified.declared_contract = None;
        verified.capabilities.streaming = false;
        let selected = select_or_declare(
            &[verified],
            ImageApi::Openai,
            "https://images.owner.example/v1",
            false,
            &service,
            None,
        )
        .unwrap();
        assert!(selected.provenance == ProfileProvenance::VerifiedOperator);
        assert!(!selected.capabilities.streaming);
    }
}
