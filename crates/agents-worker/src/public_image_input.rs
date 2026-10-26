//! Freeze public image input through native Worker global Fetch, not a VPC or
//! service binding. Preparation retains verified bytes in its execution view
//! before model admission, without creating Files or starting a workspace.

use base64::{Engine, engine::general_purpose::STANDARD};
use worker::Result;

use crate::image_generation::{self, MAX_IMAGE_BYTES};

/// No caller-controlled headers, credentials, redirects or private-origin
/// bindings. Original GIF bytes are retained; count reads first-frame dimensions.
pub(crate) async fn freeze_public_input_image(
    url: &str,
    max_bytes: usize,
) -> Result<FrozenInputImage> {
    let host = validate_public_input(url, max_bytes)?;
    // Reuse the credential-free, redirect-denying, bounded image asset reader.
    // Its exact host pin is per-input; generated assets retain operator pins.
    let fetched = image_generation::fetch_public_image_bytes(
        url,
        &[host],
        max_bytes,
        "image/png,image/jpeg,image/webp,image/gif",
    )
    .await
    .map_err(|_| error())?;
    let header = crate::input_image_metadata::inspect(&fetched.bytes).map_err(|_| error())?;
    if fetched.declared_mime.as_deref().is_some_and(|mime| {
        mime != "application/octet-stream" && !mime.eq_ignore_ascii_case(header.mime)
    }) {
        return Err(error());
    }
    Ok(FrozenInputImage {
        bytes: fetched.bytes,
        mime: header.mime,
    })
}

pub(crate) struct FrozenInputImage {
    bytes: Vec<u8>,
    mime: &'static str,
}

impl FrozenInputImage {
    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }
    pub(crate) fn data_url(self) -> String {
        format!("data:{};base64,{}", self.mime, STANDARD.encode(self.bytes))
    }
}

pub(crate) fn validate_public_input(value: &str, max_bytes: usize) -> Result<String> {
    if max_bytes == 0
        || max_bytes > MAX_IMAGE_BYTES
        || value.len() > 8192
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(error());
    }
    let mut url = url::Url::parse(value).map_err(|_| error())?;
    if url.fragment().is_some() || url.port().is_some_and(|port| port != 443) {
        return Err(error());
    }
    // Signed public URLs can contain queries, unlike provider base URLs.
    // Reuse the common HTTPS/public-host/IP/userinfo validator without query.
    url.set_query(None);
    crate::auth::provider_keys::normalize_base_url(
        agents_core::ProviderProtocol::Openai,
        url.as_str(),
    )
    .map_err(|_| error())?;
    Ok(url.host_str().ok_or_else(error)?.to_owned())
}

fn error() -> worker::Error {
    worker::Error::RustError("invalid_image_source".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_inputs_accept_signed_https_but_never_private_or_credential_urls() {
        assert_eq!(
            validate_public_input("https://images.example.com/a.png?token=opaque", 1024).unwrap(),
            "images.example.com"
        );
        for url in [
            "http://images.example.com/a",
            "https://127.0.0.1/a",
            "https://2130706433/a",
            "https://[::1]/a",
            "https://169.254.169.254/a",
            "https://localhost/a",
            "https://host.internal/a",
            "https://host.local/a",
            "https://user:password@images.example.com/a",
            "https://images.example.com/a#fragment",
            "https://images.example.com:8080/a",
            "https://images.example.com/a\r\nheader",
        ] {
            assert!(validate_public_input(url, 1024).is_err(), "{url}");
        }
        assert!(validate_public_input("https://images.example.com/a", 0).is_err());
        assert!(
            validate_public_input("https://images.example.com/a", MAX_IMAGE_BYTES + 1).is_err()
        );
    }
}
