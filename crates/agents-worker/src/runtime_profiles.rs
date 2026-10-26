//! Physical application selection is trusted persisted state, never a header.

use agents_auth::billing::Resource;
use agents_core::{RuntimeProfile, SessionRecord};
use serde::Deserialize;
use serde_json::Value;
use worker::{Env, Error, ObjectNamespace, Result};

pub(crate) fn namespace(env: &Env, session: &SessionRecord) -> Result<ObjectNamespace> {
    env.durable_object(binding(session.runtime_profile))
}

pub(crate) const fn binding(profile: RuntimeProfile) -> &'static str {
    match profile {
        RuntimeProfile::Basic1g => "SESSIONS",
        RuntimeProfile::Standard4g => "SESSION_4G",
    }
}

pub(crate) async fn namespace_for(
    env: &Env,
    owner: &str,
    session_id: &str,
) -> Result<ObjectNamespace> {
    env.durable_object(binding(profile_for(env, owner, session_id).await?))
}

pub(crate) async fn profile_for(
    env: &Env,
    owner: &str,
    session_id: &str,
) -> Result<RuntimeProfile> {
    #[derive(Deserialize)]
    struct Row {
        runtime_profile: RuntimeProfile,
    }
    let row = env
        .d1("DB")?
        .prepare("SELECT runtime_profile FROM sessions WHERE account_id=? AND id=?")
        .bind(&[owner.into(), session_id.into()])?
        .first::<Row>(None)
        .await?
        .ok_or_else(|| Error::RustError("session_not_found".into()))?;
    Ok(row.runtime_profile)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComputePrice {
    version: String,
    microusd_numerator: u64,
    quantity_denominator: u64,
}

pub(crate) fn validate_config(env: &Env, profile: RuntimeProfile) -> Result<()> {
    env.durable_object(binding(profile))?;
    if profile == RuntimeProfile::Standard4g {
        pricing(env, profile)?;
    }
    Ok(())
}

pub(crate) fn pricing(
    env: &Env,
    profile: RuntimeProfile,
) -> Result<crate::resources::ResourcePricing> {
    let base =
        crate::resources::ResourcePricing::parse(&env.var("RESOURCE_PRICING_JSON")?.to_string())?;
    if profile == RuntimeProfile::Basic1g {
        return Ok(base);
    }
    let configured: std::collections::BTreeMap<String, ComputePrice> = serde_json::from_str(
        &env.var("CONTAINER_PROFILE_PRICING_JSON")
            .map_err(|_| Error::RustError("runtime_profile_pricing_not_configured".into()))?
            .to_string(),
    )
    .map_err(|_| Error::RustError("invalid_runtime_profile_pricing".into()))?;
    let price = configured
        .get(profile.name())
        .ok_or_else(|| Error::RustError("runtime_profile_pricing_not_configured".into()))?;
    profile_pricing(base, price)
}

fn profile_pricing(
    mut base: crate::resources::ResourcePricing,
    price: &ComputePrice,
) -> Result<crate::resources::ResourcePricing> {
    if price.version.is_empty() || price.microusd_numerator == 0 || price.quantity_denominator == 0
    {
        return Err(Error::RustError("invalid_runtime_profile_pricing".into()));
    }
    let mut replaced = false;
    for tariff in &mut base.tariffs {
        tariff.price_version.clone_from(&price.version);
        if tariff.resource == Resource::Container && tariff.unit == "wall_ms" {
            tariff.microusd_numerator = price.microusd_numerator;
            tariff.quantity_denominator = price.quantity_denominator;
            replaced = true;
        }
    }
    if !replaced {
        return Err(Error::RustError("resource_tariff_missing".into()));
    }
    base.version.clone_from(&price.version);
    crate::resources::ResourcePricing::parse(&serde_json::to_string(&base)?)
}

pub(crate) fn requested_for(
    request: &Value,
    services: &agents_core::ToolServices,
) -> Result<Option<RuntimeProfile>> {
    if services.code_interpreter.as_ref().is_some_and(|service| {
        service.provider == agents_core::CodeServiceProvider::WorkspaceDaemon
    }) {
        if let Some(tools) = request.get("tools").and_then(Value::as_array) {
            crate::hosted_bindings::BoundTools::declared_code_memory(tools)?;
        }
        return Ok(None);
    }
    requested(request)
}

pub(crate) fn requested_tools_for(
    tools: &[crate::responses_wire::ResponseTool],
    services: &agents_core::ToolServices,
) -> Result<Option<RuntimeProfile>> {
    let declarations = tools
        .iter()
        .map(crate::responses_wire::ResponseTool::to_wire_value)
        .collect::<Vec<_>>();
    requested_for(&serde_json::json!({"tools":declarations}), services)
}

pub(crate) fn requested(request: &Value) -> Result<Option<RuntimeProfile>> {
    let declarations = request.get("tools").and_then(Value::as_array);
    let Some(declarations) = declarations else {
        return Ok(None);
    };
    crate::hosted_bindings::BoundTools::requested_memory(declarations)
        .and_then(|memory| memory.map(from_memory).transpose())
}

fn from_memory(memory: crate::hosted_bindings::MemoryTier) -> Result<RuntimeProfile> {
    match memory {
        crate::hosted_bindings::MemoryTier::OneGiB => Ok(RuntimeProfile::Basic1g),
        crate::hosted_bindings::MemoryTier::FourGiB => Ok(RuntimeProfile::Standard4g),
        _ => Err(Error::RustError("code_memory_profile_unavailable".into())),
    }
}

pub(crate) fn requested_tools(
    tools: &[crate::responses_wire::ResponseTool],
) -> Result<Option<RuntimeProfile>> {
    let declarations = tools
        .iter()
        .map(crate::responses_wire::ResponseTool::to_wire_value)
        .collect::<Vec<_>>();
    crate::hosted_bindings::BoundTools::requested_memory(&declarations)
        .and_then(|memory| memory.map(from_memory).transpose())
}

pub(crate) fn check_archive(profile: RuntimeProfile, logical_bytes: u64) -> Result<()> {
    if logical_bytes > profile.disk_bytes() {
        return Err(Error::RustError(
            "workspace_exceeds_runtime_disk_capacity".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn physical_profile_is_finite_and_legacy_defaults_basic() {
        assert_eq!(binding(RuntimeProfile::Basic1g), "SESSIONS");
        assert_eq!(binding(RuntimeProfile::Standard4g), "SESSION_4G");
        assert!(serde_json::from_value::<RuntimeProfile>(json!("16g")).is_err());
        assert_eq!(RuntimeProfile::default(), RuntimeProfile::Basic1g);
    }

    #[test]
    fn code_memory_is_selected_before_allocation() {
        assert_eq!(
            requested(&json!({"tools":[{"type":"code_interpreter","container":{"type":"auto"}}]}))
                .unwrap(),
            Some(RuntimeProfile::Standard4g)
        );
        assert_eq!(requested(&json!({"tools":[{"type":"code_interpreter","container":{"type":"auto","memory_limit":"1g"}}]})).unwrap(), Some(RuntimeProfile::Basic1g));
        assert!(requested(&json!({"tools":[{"type":"code_interpreter","container":{"type":"auto","memory_limit":"16g"}}]})).is_err());
        assert_eq!(requested(&json!({"tools":[]})).unwrap(), None);
    }

    #[test]
    fn branch_capacity_uses_destination_physical_disk() {
        assert!(check_archive(RuntimeProfile::Basic1g, 4_000_000_000).is_ok());
        assert!(check_archive(RuntimeProfile::Basic1g, 4_000_000_001).is_err());
        assert!(check_archive(RuntimeProfile::Standard4g, 4_000_000_001).is_ok());
    }

    #[test]
    fn allocation_tariff_is_snapshotted_without_repricing_basic() {
        let base = crate::resources::ResourcePricing::parse(r#"{"version":"basic","tariffs":[{"resource":"container","unit":"wall_ms","microusd_numerator":1,"quantity_denominator":100,"price_version":"basic"}]}"#).unwrap();
        let saved = serde_json::to_value(&base).unwrap();
        let standard = profile_pricing(
            base.clone(),
            &ComputePrice {
                version: "2026-10-25-platform-standard4g-v1".into(),
                microusd_numerator: 21,
                quantity_denominator: 1000,
            },
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&base).unwrap(), saved);
        assert_eq!(
            base.tariff(&Resource::Container, "wall_ms")
                .unwrap()
                .charge(1_800_000)
                .unwrap()
                .amount_microusd
                .get(),
            18_000
        );
        assert_eq!(
            standard
                .tariff(&Resource::Container, "wall_ms")
                .unwrap()
                .charge(1_800_000)
                .unwrap()
                .amount_microusd
                .get(),
            37_800
        );
    }
    #[test]
    fn owned_executor_is_selected_before_cloudflare_memory_policy() {
        let request = serde_json::json!({"tools":[{"type":"code_interpreter","container":{"type":"auto","memory_limit":"16g"}}]});
        assert!(requested(&request).is_err());
        let services = agents_core::ToolServices {
            code_interpreter: Some(agents_core::CodeService {
                provider: agents_core::CodeServiceProvider::WorkspaceDaemon,
                credential_id: Some("executor".into()),
            }),
            ..Default::default()
        };
        assert!(requested_for(&request, &services).unwrap().is_none());
    }
}
