//! Pure admission and rendering shared by generation and input-token counting.
//! Bindings contain service identities, never decrypted credentials or network
//! secret values. Resource creation belongs to the executing Session harness.

use std::collections::BTreeSet;

use agents_core::{
    ImageService, ImageServiceProvider, SearchService, SearchServiceProvider, ToolServices,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wasm_bindgen::JsValue;
use worker::{Env, Error, Result};

use crate::{
    file_search::FileSearchOptions, hosted_image::HostedImageProfile, hosted_tools::Kind,
    image_generation::ImageOptions, search_byok::SearchOptions, tools::ToolDefinition,
};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MemoryTier {
    #[serde(rename = "1g")]
    OneGiB,
    #[serde(rename = "4g")]
    FourGiB,
}

impl MemoryTier {
    pub(crate) const fn from_core(profile: agents_core::RuntimeProfile) -> Self {
        match profile {
            agents_core::RuntimeProfile::Basic1g => Self::OneGiB,
            agents_core::RuntimeProfile::Standard4g => Self::FourGiB,
        }
    }
}

/// These facts must come from the operator's actual service/container profile.
/// A pinned Bash permission is not evidence of Python memory or network limits.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostCapabilities {
    pub platform_images: bool,
    pub platform_search: bool,
    pub file_search: bool,
    /// An existing Session's immutable allocation, not a preference for new work.
    pub container_memory: Option<MemoryTier>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_container_profiles: Vec<MemoryTier>,
    pub code_network_disabled: bool,
    pub code_network_allowlist: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Caller {
    Direct,
    Programmatic,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum NetworkPolicy {
    Disabled,
    Allowlist { allowed_domains: Vec<String> },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CodeContainer {
    Existing {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<crate::code_contexts::OwnedContext>,
    },
    Auto {
        file_ids: Vec<String>,
        network_policy: NetworkPolicy,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodeOptions {
    pub container: CodeContainer,
    pub allocated_memory: MemoryTier,
}

impl CodeOptions {
    pub(crate) fn owned_context(&self) -> Option<&crate::code_contexts::OwnedContext> {
        match &self.container {
            CodeContainer::Existing { source, .. } => source.as_ref(),
            CodeContainer::Auto { .. } => None,
        }
    }
    pub(crate) fn is_local_to(&self, session: &str) -> bool {
        self.owned_context()
            .is_none_or(|source| source.source_session == session)
    }
    pub(crate) fn file_ids(&self) -> &[String] {
        match &self.container {
            CodeContainer::Existing { .. } => &[],
            CodeContainer::Auto { file_ids, .. } => file_ids,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ResolvedTool {
    Code {
        options: CodeOptions,
    },
    Image {
        options: ImageOptions,
        service: ImageService,
        allowed_file_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<HostedImageProfile>,
    },
    Search {
        options: SearchOptions,
        service: SearchService,
        query_char_limit: usize,
    },
    FileSearch {
        options: FileSearchOptions,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolvedBinding {
    pub tool: ResolvedTool,
    pub allowed_callers: Option<Vec<Caller>>,
}

impl ResolvedBinding {
    pub(crate) fn kind(&self) -> Kind {
        match self.tool {
            ResolvedTool::Code { .. } => Kind::CodeInterpreter,
            ResolvedTool::Image { .. } => Kind::ImageGeneration,
            ResolvedTool::Search { .. } => Kind::WebSearch,
            ResolvedTool::FileSearch { .. } => Kind::FileSearch,
        }
    }

    pub(crate) fn allows(&self, caller: Caller) -> bool {
        self.allowed_callers
            .as_ref()
            .map_or(caller == Caller::Direct, |allowed| {
                allowed.contains(&caller)
            })
    }

    fn definition(&self) -> Result<ToolDefinition> {
        let limit = match &self.tool {
            ResolvedTool::Search {
                query_char_limit, ..
            } => *query_char_limit,
            _ => 128,
        };
        let mut definition = self.kind().definition(limit);
        if let ResolvedTool::Image {
            options, profile, ..
        } = &self.tool
        {
            let profile = profile
                .as_ref()
                .ok_or_else(|| fail("image_generation_profile_unresolved"))?;
            crate::hosted_image::validate_profile_options(profile, options)?;
            definition.function.parameters["properties"]["image_file_ids"]["maxItems"] =
                json!(profile.capabilities.max_references);
            if profile.capabilities.max_references == 0 {
                definition.function.description = "Generate one image using the declared image service. Editing and input reference images are not supported by this frozen service profile.".into();
            }
        }
        Ok(definition)
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct BoundTools(Vec<ResolvedBinding>);

impl BoundTools {
    /// Select a fresh Session's actual profile before admission. Existing
    /// logical-container references never authorize allocating a new namespace.
    pub(crate) fn requested_memory(declarations: &[Value]) -> Result<Option<MemoryTier>> {
        validate_caller_declarations(declarations, 0, &mut 0)?;
        let mut found = false;
        let mut requested = None;
        for declaration in declarations {
            if declaration["type"].as_str() != Some("code_interpreter") {
                continue;
            }
            if found {
                return Err(fail("duplicate_hosted_tool"));
            }
            found = true;
            let (options, _) = options(declaration)?;
            if let ParsedCode::Auto { memory, .. } = parse_code_declaration(options)? {
                requested = Some(memory);
            }
        }
        Ok(requested)
    }

    pub(crate) fn resolve(
        declarations: &[Value],
        services: &ToolServices,
        capabilities: &HostCapabilities,
        allowed_image_file_ids: &[String],
    ) -> Result<Self> {
        validate_caller_declarations(declarations, 0, &mut 0)?;
        services
            .validate()
            .map_err(|_| fail("invalid_tool_services"))?;
        let mut bindings = Self::default();
        for declaration in declarations {
            let Some(kind) = native_kind(declaration["type"].as_str().unwrap_or_default()) else {
                continue;
            };
            if bindings.lookup(kind.alias()).is_some() {
                return Err(fail("duplicate_hosted_tool"));
            }
            let (options, allowed_callers) = options(declaration)?;
            let tool = resolve_tool(
                kind,
                options,
                services,
                capabilities,
                allowed_image_file_ids,
            )?;
            bindings.0.push(ResolvedBinding {
                tool,
                allowed_callers,
            });
        }
        Ok(bindings)
    }

    pub(crate) fn lookup(&self, alias: &str) -> Option<&ResolvedBinding> {
        self.0
            .iter()
            .find(|binding| binding.kind().alias() == alias)
    }

    pub(crate) fn requires_container(&self) -> bool {
        self.lookup(Kind::CodeInterpreter.alias())
            .is_some_and(|binding| {
                matches!(&binding.tool,
            ResolvedTool::Code{options} if options.owned_context().is_none())
            })
    }

    pub(crate) fn retained_file_ids(&self) -> Vec<String> {
        let mut ids = BTreeSet::new();
        for binding in &self.0 {
            match &binding.tool {
                ResolvedTool::Code { options } => {
                    ids.extend(options.file_ids().iter().cloned());
                }
                ResolvedTool::Image {
                    options,
                    allowed_file_ids,
                    ..
                } => {
                    ids.extend(allowed_file_ids.iter().cloned());
                    if let Some(id) = options
                        .input_image_mask
                        .as_ref()
                        .and_then(|mask| mask.file_id.as_ref())
                    {
                        ids.insert(id.clone());
                    }
                }
                _ => {}
            }
        }
        ids.into_iter().collect()
    }

    pub(crate) fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        self.0
            .iter()
            .filter(|binding| binding.allows(Caller::Direct))
            .map(ResolvedBinding::definition)
            .collect()
    }

    /// Preserve caller custom/namespace tools byte-for-Value; only declared
    /// hosted implementations become server-dispatched private functions.
    pub(crate) fn render_native(&self, declarations: &[Value]) -> Result<Vec<Value>> {
        let mut tools = Vec::with_capacity(declarations.len());
        for declaration in declarations {
            let Some(kind) = native_kind(declaration["type"].as_str().unwrap_or_default()) else {
                tools.push(declaration.clone());
                continue;
            };
            let binding = self
                .lookup(kind.alias())
                .ok_or_else(|| fail("hosted_tool_not_bound"))?;
            if !binding.allows(Caller::Direct) {
                continue;
            }
            let definition = binding.definition()?.function;
            tools.push(json!({"type":"function","name":definition.name,"description":definition.description,
                "parameters":definition.parameters}));
        }
        Ok(tools)
    }

    pub(crate) fn rewrite_choice(&self, mut choice: Value) -> Result<Value> {
        let Some(fields) = choice.as_object_mut() else {
            return Ok(choice);
        };
        let kind = fields
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(kind) = native_kind(kind) {
            let binding = self
                .lookup(kind.alias())
                .ok_or_else(|| fail("hosted_tool_not_bound"))?;
            if !binding.allows(Caller::Direct) {
                return Err(fail("hosted_tool_caller_not_allowed"));
            }
            binding.definition()?;
            return Ok(json!({"type":"function","name":kind.alias()}));
        }
        if kind == "allowed_tools" {
            let tools = fields
                .get_mut("tools")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| fail("invalid_tool_choice"))?;
            for tool in tools {
                *tool = self.rewrite_choice(std::mem::take(tool))?;
            }
        }
        Ok(choice)
    }

    /// Read-only ownership checks, also usable by input-token counting. No
    /// Container, index, collection or inference reservation is created here.

    pub(crate) async fn resolve_owned_code(&mut self, env: &Env, owner: &str) -> Result<()> {
        for binding in &mut self.0 {
            let ResolvedTool::Code { options } = &mut binding.tool else {
                continue;
            };
            let CodeContainer::Existing { id, source } = &mut options.container else {
                continue;
            };
            let owned = crate::code_owned_bindings::resolve(env, owner, id).await?;
            if source.as_ref().is_some_and(|pinned| pinned != &owned) {
                return Err(fail("code_context_identity_conflict"));
            }
            options.allocated_memory = MemoryTier::from_core(owned.profile);
            *source = Some(owned);
        }
        Ok(())
    }

    pub(crate) async fn validate_owned(&self, env: &Env, owner: &str) -> Result<()> {
        let mut services = ToolServices::default();
        for binding in &self.0 {
            crate::hosted_tools::require_runtime(env, binding.kind().native_type())?;
            match &binding.tool {
                ResolvedTool::Image {
                    service,
                    options,
                    allowed_file_ids,
                    ..
                } => {
                    services.image_generation = Some(service.clone());
                    for id in allowed_file_ids.iter().chain(
                        options
                            .input_image_mask
                            .iter()
                            .filter_map(|mask| mask.file_id.as_ref()),
                    ) {
                        validate_file_owner(env, owner, id).await?;
                    }
                }
                ResolvedTool::Search { service, .. } => services.web_search = Some(service.clone()),
                ResolvedTool::Code { options } => {
                    for id in options.file_ids() {
                        validate_file_owner(env, owner, id).await?;
                    }
                }
                ResolvedTool::FileSearch { options } => {
                    for id in &options.vector_store_ids {
                        validate_store_owner(env, owner, id).await?;
                    }
                }
            }
        }
        crate::tool_service_routes::validate_owned(env, owner, &services).await
    }

    /// Freeze metadata-only service capabilities before rendering or reserving
    /// outer inference. A restored profile is revalidated, never replaced with
    /// today's registry entry. Ownership checks remain a separate read-only step.
    pub(crate) async fn resolve_owned_profiles(&mut self, env: &Env, owner: &str) -> Result<()> {
        for binding in &mut self.0 {
            let needs_profile = match &binding.tool {
                ResolvedTool::Image { profile, .. } => profile.is_none(),
                _ => continue,
            };
            if needs_profile {
                let resolved = crate::hosted_image::resolve_profile(env, owner, binding).await?;
                if let ResolvedTool::Image { profile, .. } = &mut binding.tool {
                    *profile = Some(resolved);
                }
            }
            if let ResolvedTool::Image {
                options,
                profile: Some(profile),
                ..
            } = &binding.tool
            {
                crate::hosted_image::validate_profile_options(profile, options)?;
            }
        }
        Ok(())
    }
}

fn resolve_tool(
    kind: Kind,
    options: Value,
    services: &ToolServices,
    capabilities: &HostCapabilities,
    allowed_files: &[String],
) -> Result<ResolvedTool> {
    match kind {
        Kind::CodeInterpreter => Ok(ResolvedTool::Code {
            options: code_options(options, capabilities)?,
        }),
        Kind::ImageGeneration => {
            let options: ImageOptions = parse(options)?;
            options.validate()?;
            if allowed_files.len() > 16
                || allowed_files.iter().any(|id| !identifier(id))
                || allowed_files.iter().collect::<BTreeSet<_>>().len() != allowed_files.len()
            {
                return Err(fail("invalid_image_file_authority"));
            }
            let service = services.image_generation.clone().unwrap_or_default();
            if service.provider == ImageServiceProvider::Platform && !capabilities.platform_images {
                return Err(fail("platform_image_service_unavailable"));
            }
            Ok(ResolvedTool::Image {
                options,
                service,
                allowed_file_ids: allowed_files.to_vec(),
                profile: None,
            })
        }
        Kind::WebSearch => {
            let options: SearchOptions = parse(options)?;
            options.validate()?;
            let service = services.web_search.clone().unwrap_or(SearchService {
                provider: SearchServiceProvider::PlatformDeepseek,
                credential_id: None,
                model: None,
                max_output_tokens: 1024,
                max_tool_calls: 4,
            });
            if service.provider == SearchServiceProvider::PlatformDeepseek
                && !capabilities.platform_search
            {
                return Err(fail("platform_search_service_unavailable"));
            }
            let query_char_limit = if service.provider == SearchServiceProvider::OpenaiResponses {
                8192
            } else {
                crate::search_deepseek::query_limit(&options)?
            };
            Ok(ResolvedTool::Search {
                options,
                service,
                query_char_limit,
            })
        }
        Kind::FileSearch => {
            if !capabilities.file_search {
                return Err(fail("file_search_service_unavailable"));
            }
            let options: FileSearchOptions = parse(options)?;
            options.validate()?;
            Ok(ResolvedTool::FileSearch { options })
        }
    }
}

fn options(declaration: &Value) -> Result<(Value, Option<Vec<Caller>>)> {
    let mut fields = declaration
        .as_object()
        .cloned()
        .ok_or_else(|| fail("invalid_hosted_tool"))?;
    let callers: Option<Vec<Caller>> = fields
        .remove("allowed_callers")
        .map(parse)
        .transpose()?
        .flatten();
    if callers.as_ref().is_some_and(|values| {
        values.is_empty() || values.len() > 2 || values.len() == 2 && values[0] == values[1]
    }) {
        return Err(fail("invalid_hosted_tool_callers"));
    }
    if native_kind(
        fields
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ) != Some(Kind::WebSearch)
    {
        fields.remove("type");
    }
    Ok((Value::Object(fields), callers))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeDeclaration {
    container: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoContainer {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    file_ids: Vec<String>,
    #[serde(default)]
    memory_limit: Option<String>,
    #[serde(default)]
    network_policy: Option<Value>,
}

enum ParsedCode {
    Existing(String),
    Auto {
        file_ids: Vec<String>,
        network_policy: NetworkPolicy,
        memory: MemoryTier,
    },
}

fn parse_code_declaration(value: Value) -> Result<ParsedCode> {
    let declaration: CodeDeclaration = parse(value)?;
    if let Some(id) = declaration.container.as_str() {
        if !identifier(id) || !id.starts_with("cntr_") {
            return Err(fail("invalid_code_container"));
        }
        return Ok(ParsedCode::Existing(id.into()));
    }
    let container: AutoContainer = parse(declaration.container)?;
    if container.kind != "auto"
        || container.file_ids.len() > 128
        || container.file_ids.iter().any(|id| !identifier(id))
        || container.file_ids.iter().collect::<BTreeSet<_>>().len() != container.file_ids.len()
    {
        return Err(fail("invalid_code_container"));
    }
    let requested = match container.memory_limit.as_deref().unwrap_or("1g") {
        "1g" => MemoryTier::OneGiB,
        "4g" => MemoryTier::FourGiB,
        "16g" | "64g" => return Err(fail("code_memory_profile_unavailable")),
        _ => return Err(fail("invalid_code_memory_limit")),
    };
    let network_policy = network_policy(container.network_policy)?;
    Ok(ParsedCode::Auto {
        file_ids: container.file_ids,
        network_policy,
        memory: requested,
    })
}

fn code_options(value: Value, capabilities: &HostCapabilities) -> Result<CodeOptions> {
    let parsed = parse_code_declaration(value)?;
    if let ParsedCode::Existing(id) = parsed {
        let allocated_memory = capabilities
            .container_memory
            .ok_or_else(|| fail("code_container_not_found"))?;
        if !capabilities.code_network_disabled && !capabilities.code_network_allowlist {
            return Err(fail("code_network_policy_unavailable"));
        }
        return Ok(CodeOptions {
            container: CodeContainer::Existing { id, source: None },
            allocated_memory,
        });
    }
    let ParsedCode::Auto {
        file_ids,
        network_policy,
        memory,
    } = parsed
    else {
        unreachable!("Existing container handled above")
    };
    let available = match capabilities.container_memory {
        Some(actual) => actual == memory,
        None => capabilities.available_container_profiles.contains(&memory),
    };
    if !available {
        return Err(fail("code_memory_profile_unavailable"));
    }
    let supported = match &network_policy {
        NetworkPolicy::Disabled => capabilities.code_network_disabled,
        NetworkPolicy::Allowlist { .. } => capabilities.code_network_allowlist,
    };
    if !supported {
        return Err(fail("code_network_policy_unavailable"));
    }
    Ok(CodeOptions {
        container: CodeContainer::Auto {
            file_ids,
            network_policy,
        },
        allocated_memory: memory,
    })
}

fn network_policy(value: Option<Value>) -> Result<NetworkPolicy> {
    let mut value = value.unwrap_or_else(|| json!({"type":"disabled"}));
    if let Some(fields) = value.as_object_mut()
        && let Some(secrets) = fields.remove("domain_secrets")
        && !secrets.is_null()
        && secrets.as_array().is_none_or(|values| !values.is_empty())
    {
        return Err(fail("code_network_secrets_unavailable"));
    }
    let policy: NetworkPolicy = parse(value)?;
    if let NetworkPolicy::Allowlist { allowed_domains } = &policy {
        if allowed_domains.is_empty() || allowed_domains.len() > 100 {
            return Err(fail("invalid_code_network_policy"));
        }
        for domain in allowed_domains {
            let Ok(url::Host::Domain(host)) = url::Host::parse(domain) else {
                return Err(fail("invalid_code_network_policy"));
            };
            if !crate::auth::provider_keys::public_hostname(host.trim_end_matches('.')) {
                return Err(fail("invalid_code_network_policy"));
            }
        }
    }
    Ok(policy)
}

async fn validate_store_owner(env: &Env, owner: &str, id: &str) -> Result<()> {
    let now = crate::util::now_ms().to_string();
    let found: Option<Value> = env.d1("DB")?.prepare("SELECT id FROM vector_stores WHERE account_id=? AND id=? AND status IN ('in_progress','completed') AND (expires_after_days IS NULL OR last_active_at+expires_after_days*86400000>CAST(? AS INTEGER))")
        .bind(&[JsValue::from_str(owner), JsValue::from_str(id), JsValue::from_str(&now)])?.first(None).await?;
    if found.is_none() {
        return Err(fail("vector_store_not_found"));
    }
    Ok(())
}

async fn validate_file_owner(env: &Env, owner: &str, id: &str) -> Result<()> {
    if crate::space::get_owned_file(env, owner, id)
        .await?
        .is_none()
    {
        return Err(fail("file_not_found"));
    }
    Ok(())
}

fn native_kind(kind: &str) -> Option<Kind> {
    match kind {
        "code_interpreter" => Some(Kind::CodeInterpreter),
        "image_generation" => Some(Kind::ImageGeneration),
        "web_search"
        | "web_search_2025_08_26"
        | "web_search_preview"
        | "web_search_preview_2025_03_11" => Some(Kind::WebSearch),
        "file_search" => Some(Kind::FileSearch),
        _ => None,
    }
}

fn validate_caller_declarations(tools: &[Value], depth: usize, count: &mut usize) -> Result<()> {
    *count = count.saturating_add(tools.len());
    if depth > 8 || *count > 128 {
        return Err(fail("tool_declarations_too_complex"));
    }
    for tool in tools {
        let kind = tool["type"].as_str().unwrap_or_default();
        if matches!(kind, "function" | "custom" | "namespace")
            && tool["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("__agents_"))
        {
            return Err(fail("tool_name_conflict"));
        }
        if kind == "namespace" {
            let nested = tool["tools"]
                .as_array()
                .ok_or_else(|| fail("invalid_tool_namespace"))?;
            validate_caller_declarations(nested, depth + 1, count)?;
        }
    }
    Ok(())
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| fail("invalid_hosted_tool_options"))
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_profile() -> HostedImageProfile {
        HostedImageProfile {
            api: crate::image_generation::ImageApi::Openai,
            base_url: "https://images.example.com/v1".into(),
            model: "owner-image-model".into(),
            public_model: None,
            capabilities: crate::image_generation::Capabilities {
                omit_default_output_format: true,
                ..Default::default()
            },
            router_endpoint: None,
            asset_hosts: vec![],
            price_revision: "owner-declared-image-v1".into(),
            reservation_microusd: 0,
            platform_funded: false,
            provenance: crate::hosted_image::ProfileProvenance::OwnerDeclaredMinimal,
            declared_contract: None,
        }
    }

    fn freeze_image(bound: &mut BoundTools, frozen: HostedImageProfile) {
        for binding in &mut bound.0 {
            if let ResolvedTool::Image { profile, .. } = &mut binding.tool {
                *profile = Some(frozen.clone());
            }
        }
    }

    fn capabilities() -> HostCapabilities {
        HostCapabilities {
            platform_images: true,
            platform_search: true,
            file_search: true,
            container_memory: Some(MemoryTier::OneGiB),
            available_container_profiles: vec![],
            code_network_disabled: true,
            code_network_allowlist: true,
        }
    }

    fn resolve(tools: &[Value]) -> Result<BoundTools> {
        BoundTools::resolve(tools, &ToolServices::default(), &capabilities(), &[])
    }

    #[test]
    fn generation_and_counting_share_aliases_and_choices_without_changing_client_tools() {
        let tools = vec![
            json!({"type":"web_search"}),
            json!({"type":"image_generation"}),
            json!({"type":"namespace","name":"client","tools":[{"type":"custom","name":"edit"}]}),
        ];
        let mut bound = resolve(&tools).unwrap();
        freeze_image(&mut bound, image_profile());
        let rendered = bound.render_native(&tools).unwrap();
        assert_eq!(rendered[0]["name"], Kind::WebSearch.alias());
        assert_eq!(rendered[2], tools[2]);
        assert_eq!(bound.definitions().unwrap().len(), 2);
        let choice = bound.rewrite_choice(json!({"type":"allowed_tools","mode":"required","tools":[{"type":"image_generation"},{"type":"custom","name":"edit"}]})).unwrap();
        assert_eq!(choice["tools"][0]["name"], Kind::ImageGeneration.alias());
        assert_eq!(choice["tools"][1], json!({"type":"custom","name":"edit"}));
        assert_eq!(bound.rewrite_choice(json!("auto")).unwrap(), "auto");
        assert!(
            bound
                .rewrite_choice(json!({"type":"code_interpreter"}))
                .is_err()
        );
    }

    #[test]
    fn declarations_do_not_grant_unavailable_platform_services_or_duplicate_authority() {
        assert!(
            BoundTools::resolve(
                &[json!({"type":"image_generation"})],
                &ToolServices::default(),
                &HostCapabilities::default(),
                &[]
            )
            .is_err()
        );
        assert!(
            resolve(&[
                json!({"type":"web_search"}),
                json!({"type":"web_search_preview"})
            ])
            .is_err()
        );
        assert!(resolve(&[json!({"type":"image_generation","unknown":true})]).is_err());
        let bound =
            resolve(&[json!({"type":"image_generation","allowed_callers":["programmatic"]})])
                .unwrap();
        assert!(bound.definitions().unwrap().is_empty());
        let binding = bound.lookup(Kind::ImageGeneration.alias()).unwrap();
        assert!(!binding.allows(Caller::Direct));
        assert!(binding.allows(Caller::Programmatic));
        assert!(
            bound
                .rewrite_choice(json!({"type":"image_generation"}))
                .is_err()
        );
    }

    #[test]
    fn callers_cannot_shadow_private_aliases_directly_or_inside_namespaces() {
        for kind in ["function", "custom", "namespace"] {
            let direct = json!({"type":kind,"name":"__agents_image_generation","tools":[]});
            assert!(resolve(&[json!({"type":"image_generation"}), direct.clone()]).is_err());
            let nested = json!({"type":"namespace","name":"safe","tools":[{"type":"namespace","name":"nested","tools":[direct]}]});
            assert!(resolve(&[nested.clone()]).is_err());
            assert!(BoundTools::requested_memory(&[nested]).is_err());
        }
        assert!(resolve(&[json!({"type":"function","name":"__agents_future_reserved"})]).is_err());
        let ordinary =
            json!({"type":"namespace","name":"safe","tools":[{"type":"custom","name":"edit"}]});
        let bound = resolve(&[ordinary.clone()]).unwrap();
        assert_eq!(
            bound.render_native(&[ordinary.clone()]).unwrap(),
            vec![ordinary]
        );
    }

    #[test]
    fn private_alias_validation_has_bounded_namespace_work() {
        let many = vec![json!({"type":"function","name":"local"}); 129];
        assert!(resolve(&many).is_err());
        let mut nested = json!({"type":"function","name":"local"});
        for _ in 0..10 {
            nested = json!({"type":"namespace","name":"layer","tools":[nested]});
        }
        assert!(resolve(&[nested]).is_err());
    }

    #[test]
    fn real_memory_and_network_capabilities_are_required_before_compute() {
        for memory in ["4g", "16g", "64g"] {
            assert!(resolve(&[json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":memory}})]).is_err());
        }
        let code = json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":"1g","file_ids":["file-user"]}});
        let bound = resolve(&[code.clone()]).unwrap();
        assert!(bound.requires_container());
        let mut unavailable = capabilities();
        unavailable.code_network_disabled = false;
        unavailable.code_network_allowlist = false;
        assert!(BoundTools::resolve(&[code], &ToolServices::default(), &unavailable, &[]).is_err());
        unavailable.code_network_disabled = true;
        let allowlist = json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":"1g","network_policy":{"type":"allowlist","allowed_domains":["example.com"]}}});
        assert!(
            BoundTools::resolve(&[allowlist], &ToolServices::default(), &unavailable, &[]).is_err()
        );
        let secret = json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":"1g","network_policy":{"type":"allowlist","allowed_domains":["example.com"],"domain_secrets":[{"domain":"example.com","name":"Authorization","value":"never-retain-this"}]}}});
        assert!(resolve(&[secret]).is_err());
    }

    #[test]
    fn omitted_public_memory_uses_one_gib_without_resizing_an_existing_profile() {
        let declaration = json!({"type":"code_interpreter","container":{"type":"auto"}});
        let bound = resolve(&[declaration.clone()]).unwrap();
        let ResolvedTool::Code { options } =
            &bound.lookup(Kind::CodeInterpreter.alias()).unwrap().tool
        else {
            panic!("Expected code binding");
        };
        assert!(options.allocated_memory == MemoryTier::OneGiB);
        let mut actual = capabilities();
        actual.container_memory = Some(MemoryTier::FourGiB);
        assert!(
            BoundTools::resolve(&[declaration], &ToolServices::default(), &actual, &[]).is_err()
        );
        assert!(
            resolve(&[
                json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":null}})
            ])
            .is_ok()
        );
        let explicit =
            json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":"4g"}});
        let old = BoundTools::resolve(&[explicit], &ToolServices::default(), &actual, &[]).unwrap();
        let stored = serde_json::to_vec(&old).unwrap();
        let restored: BoundTools = serde_json::from_slice(&stored).unwrap();
        let ResolvedTool::Code { options } =
            &restored.lookup(Kind::CodeInterpreter.alias()).unwrap().tool
        else {
            panic!("Expected stored code binding");
        };
        assert!(options.allocated_memory == MemoryTier::FourGiB);
        assert_eq!(serde_json::to_vec(&restored).unwrap(), stored);
    }

    #[test]
    fn memory_selection_uses_the_same_validated_declaration_without_creating_contexts() {
        assert!(MemoryTier::from_core(agents_core::RuntimeProfile::Basic1g) == MemoryTier::OneGiB);
        assert!(
            MemoryTier::from_core(agents_core::RuntimeProfile::Standard4g) == MemoryTier::FourGiB
        );
        let auto = json!({"type":"code_interpreter","container":{"type":"auto"}});
        assert!(BoundTools::requested_memory(&[auto.clone()]).unwrap() == Some(MemoryTier::OneGiB));
        assert!(
            BoundTools::requested_memory(&[
                json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":"1g"}})
            ])
            .unwrap()
                == Some(MemoryTier::OneGiB)
        );
        assert!(
            BoundTools::requested_memory(&[
                json!({"type":"code_interpreter","container":"cntr_existing"})
            ])
            .unwrap()
            .is_none()
        );
        assert!(
            BoundTools::requested_memory(&[json!({"type":"function","name":"local"})])
                .unwrap()
                .is_none()
        );
        assert!(BoundTools::requested_memory(&[auto.clone(), auto]).is_err());
        for container in [
            json!({"type":"auto","memory_limit":"16g"}),
            json!({"type":"auto","memory_limit":"64g"}),
            json!({"type":"manual"}),
            json!(123),
        ] {
            assert!(
                BoundTools::requested_memory(&[
                    json!({"type":"code_interpreter","container":container})
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn fresh_available_profiles_do_not_resize_existing_sessions_or_create_existing_ids() {
        let auto = json!({"type":"code_interpreter","container":{"type":"auto"}});
        let small =
            json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":"1g"}});
        let existing = json!({"type":"code_interpreter","container":"cntr_existing"});
        let mut fresh = capabilities();
        fresh.container_memory = None;
        fresh.available_container_profiles = vec![MemoryTier::OneGiB, MemoryTier::FourGiB];
        for (tool, expected) in [
            (auto.clone(), MemoryTier::OneGiB),
            (small, MemoryTier::OneGiB),
        ] {
            let bound =
                BoundTools::resolve(&[tool], &ToolServices::default(), &fresh, &[]).unwrap();
            let ResolvedTool::Code { options } =
                &bound.lookup(Kind::CodeInterpreter.alias()).unwrap().tool
            else {
                panic!("Expected code binding")
            };
            assert!(options.allocated_memory == expected);
        }
        assert!(
            BoundTools::resolve(&[existing.clone()], &ToolServices::default(), &fresh, &[])
                .is_err()
        );
        fresh.container_memory = Some(MemoryTier::OneGiB);
        assert!(BoundTools::resolve(&[auto], &ToolServices::default(), &fresh, &[]).is_ok());
        assert!(BoundTools::resolve(&[existing], &ToolServices::default(), &fresh, &[]).is_ok());
        fresh.container_memory = None;
        fresh.available_container_profiles = vec![MemoryTier::OneGiB];
        assert!(
            BoundTools::resolve(
                &[json!({"type":"code_interpreter","container":{"type":"auto"}})],
                &ToolServices::default(),
                &fresh,
                &[]
            )
            .is_ok()
        );
    }

    #[test]
    fn byok_does_not_require_platform_service_and_bindings_remain_secret_free() {
        let services = ToolServices {
            image_generation: Some(ImageService {
                provider: ImageServiceProvider::OpenaiImages,
                credential_id: Some("key-owned".into()),
                model: Some("image-model".into()),
                asset_hosts: BTreeSet::new(),
                capabilities: None,
            }),
            ..Default::default()
        };
        let bound = BoundTools::resolve(
            &[json!({"type":"image_generation"})],
            &services,
            &HostCapabilities::default(),
            &["file-original".into()],
        )
        .unwrap();
        let encoded = serde_json::to_string(&bound).unwrap();
        let restored: BoundTools = serde_json::from_str(&encoded).unwrap();
        assert!(restored.lookup(Kind::ImageGeneration.alias()).is_some());
        assert!(encoded.contains("file-original"));
        assert!(!encoded.contains("api_key"));
        assert!(!encoded.contains("domain_secrets"));
    }

    #[test]
    fn image_profile_freezes_the_same_menu_and_provenance_without_upstream_calls() {
        let tools = [json!({"type":"image_generation"})];
        let mut bound = resolve(&tools).unwrap();
        let legacy = serde_json::to_string(&bound).unwrap();
        assert!(!legacy.contains("\"profile\""));
        let old: BoundTools = serde_json::from_str(&legacy).unwrap();
        assert_eq!(legacy, serde_json::to_string(&old).unwrap());
        assert!(old.definitions().is_err());
        assert!(old.render_native(&tools).is_err());
        assert!(
            old.rewrite_choice(json!({"type":"image_generation"}))
                .is_err()
        );

        freeze_image(&mut bound, image_profile());
        let definition = bound.definitions().unwrap().remove(0).function;
        assert_eq!(
            definition.parameters["properties"]["image_file_ids"]["maxItems"],
            0
        );
        assert!(
            definition
                .description
                .contains("Editing and input reference images are not supported")
        );
        let native = bound.render_native(&tools).unwrap();
        assert_eq!(native[0]["parameters"], definition.parameters);
        assert_eq!(native[0]["description"], definition.description);
        let bytes = serde_json::to_vec(&native).unwrap();
        let restored: BoundTools =
            serde_json::from_slice(&serde_json::to_vec(&bound).unwrap()).unwrap();
        assert_eq!(
            bytes,
            serde_json::to_vec(&restored.render_native(&tools).unwrap()).unwrap()
        );
        let ResolvedTool::Image {
            profile: Some(profile),
            ..
        } = &restored.lookup(Kind::ImageGeneration.alias()).unwrap().tool
        else {
            panic!("Expected frozen image profile")
        };
        assert!(profile.provenance == crate::hosted_image::ProfileProvenance::OwnerDeclaredMinimal);
        assert!(!profile.platform_funded);
    }

    #[test]
    fn frozen_image_capabilities_reject_advanced_options_before_rendering() {
        for options in [
            json!({"action":"edit"}),
            json!({"quality":"high"}),
            json!({"partial_images":1}),
            json!({"background":"transparent"}),
            json!({"size":"1024x1024"}),
        ] {
            let mut declaration = options;
            declaration["type"] = json!("image_generation");
            let mut bound = resolve(&[declaration.clone()]).unwrap();
            freeze_image(&mut bound, image_profile());
            assert!(bound.definitions().is_err());
            assert!(bound.render_native(&[declaration]).is_err());
        }
        let mut profile = image_profile();
        profile.capabilities.max_references = 3;
        profile.provenance = crate::hosted_image::ProfileProvenance::OwnerDeclaredContract;
        profile.declared_contract = Some(
            serde_json::from_value(json!({"version":1,"edit":true,"max_references":3})).unwrap(),
        );
        let mut bound = resolve(&[json!({"type":"image_generation","action":"edit"})]).unwrap();
        freeze_image(&mut bound, profile);
        let definition = bound.definitions().unwrap().remove(0).function;
        assert_eq!(
            definition.parameters["properties"]["image_file_ids"]["maxItems"],
            3
        );
        assert!(definition.description.contains("Generate or edit"));
    }

    #[test]
    fn declared_code_inputs_and_image_masks_join_response_file_retention() {
        let bound = BoundTools::resolve(
            &[
                json!({"type":"code_interpreter","container":{"type":"auto","memory_limit":"1g","file_ids":["file-code","file-shared"]}}),
                json!({"type":"image_generation","input_image_mask":{"file_id":"file-mask"}}),
            ],
            &ToolServices::default(),
            &capabilities(),
            &["file-image".into(), "file-shared".into()],
        )
        .unwrap();
        assert_eq!(
            bound.retained_file_ids(),
            ["file-code", "file-image", "file-mask", "file-shared"]
        );
    }
    #[test]
    fn borrowed_code_memory_never_selects_or_bills_a_second_local_vm() {
        let id = format!("cntr_{}", "a".repeat(32));
        let source = crate::code_contexts::OwnedContext {
            id: id.clone(),
            source_session: "original".into(),
            profile: agents_core::RuntimeProfile::Standard4g,
            daemon_boot: "boot".into(),
            policy_revision: "policy".into(),
        };
        let options = CodeOptions {
            container: CodeContainer::Existing {
                id: id.clone(),
                source: Some(source),
            },
            allocated_memory: MemoryTier::FourGiB,
        };
        assert!(!options.is_local_to("fresh-guest"));
        assert!(options.is_local_to("original"));
        let tools = BoundTools(vec![ResolvedBinding {
            tool: ResolvedTool::Code { options },
            allowed_callers: None,
        }]);
        assert!(!tools.requires_container());
        assert!(
            BoundTools::requested_memory(&[json!({"type":"code_interpreter","container":id})])
                .unwrap()
                .is_none()
        );
        let restored: BoundTools =
            serde_json::from_slice(&serde_json::to_vec(&tools).unwrap()).unwrap();
        assert!(!restored.requires_container());
        let ResolvedTool::Code { options } = &restored.0[0].tool else {
            panic!("code binding");
        };
        assert!(matches!(options.allocated_memory, MemoryTier::FourGiB));
    }
}
