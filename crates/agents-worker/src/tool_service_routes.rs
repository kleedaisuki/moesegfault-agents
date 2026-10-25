//! Validate secret-free tool routes against the owner's immutable credential origins.

use agents_core::{
    ImageServiceProvider, ProviderCredentialFamily, SearchServiceProvider, ToolServices,
};
use worker::{Env, Error, Result};

use crate::auth::provider_keys;

pub async fn validate_owned(env: &Env, owner: &str, services: &ToolServices) -> Result<()> {
    services
        .validate()
        .map_err(|_| Error::RustError("invalid_tool_services".into()))?;
    if let Some(image) = &services.image_generation {
        for host in &image.asset_hosts {
            let normalized = match url::Host::parse(host) {
                Ok(url::Host::Domain(domain)) => domain,
                _ => return Err(Error::RustError("invalid_image_asset_origin".into())),
            };
            if !provider_keys::public_hostname(normalized.trim_end_matches('.')) {
                return Err(Error::RustError("invalid_image_asset_origin".into()));
            }
        }
        if image.provider != ImageServiceProvider::Platform {
            validate_credential(env, owner, image.credential_id.as_deref()).await?;
        }
    }
    if let Some(search) = &services.web_search {
        if search.provider != SearchServiceProvider::PlatformDeepseek {
            validate_credential(env, owner, search.credential_id.as_deref()).await?;
        }
    }
    Ok(())
}

async fn validate_credential(env: &Env, owner: &str, id: Option<&str>) -> Result<()> {
    let id = id.ok_or_else(|| Error::RustError("provider_key_required".into()))?;
    let metadata = provider_keys::metadata(env, owner, id).await?;
    if metadata.protocol.credential_family() != ProviderCredentialFamily::Openai {
        return Err(Error::RustError("tool_service_protocol_unsupported".into()));
    }
    Ok(())
}
