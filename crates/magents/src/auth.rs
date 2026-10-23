use anyhow::{Context, Result, bail};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, CsrfToken, IssuerUrl, Nonce, OAuth2TokenResponse,
    PkceCodeChallenge, RedirectUrl, RefreshToken, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreJwsSigningAlgorithm, CoreProviderMetadata},
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

#[derive(Serialize, Deserialize)]
struct Credential {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_at: u64,
    issuer: String,
    client_id: String,
}

fn credential_lock() -> Result<std::fs::File> {
    let dirs = directories::ProjectDirs::from("dev", "moeSegFault", "magents")
        .context("Cannot locate platform application data directory")?;
    std::fs::create_dir_all(dirs.data_local_dir())?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dirs.data_local_dir().join("credential.lock"))?;
    fs2::FileExt::lock_exclusive(&file).context("Cannot lock credential rotation")?;
    Ok(file)
}

#[derive(Deserialize)]
struct NativeConfig {
    issuer: String,
    native_client_id: String,
    native_redirect_uri: String,
}

async fn native_config(base: &str, http: &reqwest::Client) -> Result<NativeConfig> {
    let settings = [
        "MAGENTS_OIDC_ISSUER",
        "MAGENTS_OIDC_CLIENT_ID",
        "MAGENTS_OIDC_REDIRECT_URI",
    ];
    if settings.iter().any(|name| std::env::var_os(name).is_some()) {
        return Ok(NativeConfig {
            issuer: setting(settings[0])?,
            native_client_id: setting(settings[1])?,
            native_redirect_uri: setting(settings[2])?,
        });
    }
    http.get(Url::parse(base)?.join("/auth/config")?)
        .send()
        .await
        .context("Cannot load native registration from API")?
        .error_for_status()
        .context("Native registration is not available on this deployment")?
        .json()
        .await
        .context("Invalid native registration configuration")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn entry(base: &str) -> Result<keyring::v1::Entry> {
    keyring::v1::Entry::new("dev.moesegfault.agents", base)
        .context("OS credential store unavailable; no plaintext fallback is permitted")
}
fn setting(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| {
        format!("{name} must identify your deployment's reviewed native OIDC registration")
    })
}

pub async fn access_token(base: &str) -> Result<String> {
    if let Ok(token) = std::env::var("MAGENTS_API_KEY") {
        if token.is_empty() {
            bail!("MAGENTS_API_KEY is empty");
        }
        return Ok(token);
    }
    let _lock = credential_lock()?;
    let raw = entry(base)?
        .get_password()
        .context("Not logged in; run magents auth login")?;
    let credential: Credential =
        serde_json::from_str(&raw).context("Stored login is invalid; log in again")?;
    if std::env::var("MAGENTS_OIDC_ISSUER").is_ok_and(|issuer| credential.issuer != issuer)
        || std::env::var("MAGENTS_OIDC_CLIENT_ID")
            .is_ok_and(|client| credential.client_id != client)
    {
        bail!("Stored login belongs to a different OIDC registration; log in again");
    }
    if credential.expires_at > now() + 30 {
        return Ok(credential.access_token);
    }
    let refresh = credential
        .refresh_token
        .as_ref()
        .context("Login expired; run magents auth login")?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let metadata =
        CoreProviderMetadata::discover_async(IssuerUrl::new(credential.issuer.clone())?, &http)
            .await
            .context("Cannot discover issuer for refresh")?;
    if metadata.issuer().as_str() != credential.issuer {
        bail!("Refresh discovery issuer mismatch");
    }
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(credential.client_id.clone()),
        None,
    );
    // Remove the predecessor before dispatch: an ambiguous network result must never cause reuse.
    entry(base)?
        .delete_credential()
        .context("Cannot retire predecessor credential")?;
    let token = client
        .exchange_refresh_token(&RefreshToken::new(refresh.clone()))?
        .request_async(&http)
        .await
        .map_err(|_| {
            anyhow::anyhow!("Refresh failed or had an unknown outcome; run magents auth login")
        })?;
    let refresh_token = token
        .refresh_token()
        .context("Issuer did not rotate refresh token; log in again")?
        .secret()
        .clone();
    if bool::from(refresh_token.as_bytes().ct_eq(refresh.as_bytes())) {
        bail!("Issuer reused the predecessor refresh token; log in again");
    }
    let lifetime = token
        .expires_in()
        .context("Refresh response omitted access token lifetime")?
        .as_secs();
    let next = Credential {
        access_token: token.access_token().secret().clone(),
        refresh_token: Some(refresh_token),
        expires_at: now().saturating_add(lifetime),
        issuer: credential.issuer,
        client_id: credential.client_id,
    };
    entry(base)?
        .set_password(&serde_json::to_string(&next)?)
        .context("Cannot persist refreshed credential; log in again")?;
    Ok(next.access_token)
}

pub fn status(base: &str) -> Result<()> {
    if std::env::var_os("MAGENTS_API_KEY").is_some() {
        println!("API key supplied by environment");
        return Ok(());
    }
    let _lock = credential_lock()?;
    let raw = entry(base)?.get_password().context("Not logged in")?;
    let credential: Credential = serde_json::from_str(&raw)?;
    println!(
        "{}",
        if credential.expires_at > now() + 30 {
            "Logged in"
        } else {
            "Login expired"
        }
    );
    Ok(())
}

pub fn logout(base: &str) -> Result<()> {
    let _lock = credential_lock()?;
    match entry(base)?.delete_credential() {
        Ok(()) | Err(keyring::v1::Error::NoEntry) => (),
        Err(error) => return Err(error.into()),
    }
    println!("Local login removed. Identity SSO and environment API keys are unchanged.");
    Ok(())
}

fn redirect_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!(
            "Native redirect must be a registered HTTP loopback URL without credentials, query or fragment"
        );
    }
    Ok(url)
}

fn callback(target: &str, redirect: &Url, state: &str, issuer: &str) -> Result<String> {
    let url = redirect.join(target)?;
    if url.origin() != redirect.origin() || url.path() != redirect.path() {
        bail!("Unexpected callback path");
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let field = |name: &str| -> Result<String> {
        let values: Vec<_> = pairs.iter().filter(|(key, _)| key == name).collect();
        if values.len() != 1 {
            bail!("Missing or duplicate callback field");
        }
        Ok(values[0].1.to_string())
    };
    let actual = field("state")?;
    if !bool::from(actual.as_bytes().ct_eq(state.as_bytes())) {
        bail!("Login state mismatch");
    }
    if field("iss")? != issuer {
        bail!("Authorization response issuer mismatch");
    }
    if pairs.iter().any(|(key, _)| key == "error") {
        bail!("Authorization was denied");
    }
    let code = field("code")?;
    if code.is_empty() {
        bail!("Authorization code is empty");
    }
    Ok(code)
}

pub async fn login(base: &str) -> Result<()> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let config = native_config(base, &http).await?;
    let issuer = config.issuer;
    if Url::parse(&issuer)?.scheme() != "https" {
        bail!("OIDC issuer must use HTTPS");
    }
    let client_id = config.native_client_id;
    if client_id.is_empty() {
        bail!("Native client registration has not been provisioned");
    }
    let mut redirect = redirect_url(&config.native_redirect_uri)?;
    let host = redirect
        .host_str()
        .context("Redirect has no host")?
        .trim_matches(['[', ']']);
    let address = std::net::SocketAddr::new(host.parse()?, redirect.port().unwrap_or(0));
    let listener = TcpListener::bind(address)
        .await
        .context("Cannot bind the registered loopback redirect port")?;
    // A portless registration uses Identity's deployment-reviewed loopback-any-port mode.
    redirect
        .set_port(Some(listener.local_addr()?.port()))
        .map_err(|_| anyhow::anyhow!("Cannot set loopback port"))?;
    let metadata = CoreProviderMetadata::discover_async(IssuerUrl::new(issuer.clone())?, &http)
        .await
        .context("OIDC discovery failed")?;
    if metadata.issuer().as_str() != issuer {
        bail!("Discovery issuer mismatch");
    }
    let client =
        CoreClient::from_provider_metadata(metadata, ClientId::new(client_id.clone()), None)
            .set_redirect_uri(RedirectUrl::new(redirect.to_string())?);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("profile".into()))
        .add_scope(Scope::new("offline_access".into()))
        .set_pkce_challenge(challenge)
        .url();
    eprintln!("Opening your system browser. Login expires in 3 minutes.");
    webbrowser::open(url.as_str()).context("Cannot open browser")?;
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        let (mut stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() { bail!("Non-loopback callback rejected"); }
        let mut request = Vec::new();
        while !request.windows(4).any(|value| value == b"\r\n\r\n") {
            let mut bytes = [0; 1024];
            let size = stream.read(&mut bytes).await?;
            if size == 0 || request.len() + size > 8192 { bail!("Invalid callback request"); }
            request.extend_from_slice(&bytes[..size]);
        }
        let raw = String::from_utf8(request)?;
        let mut line = raw.lines().next().context("Empty callback")?.split_whitespace();
        if line.next() != Some("GET") { bail!("Callback must use GET"); }
        let target = line.next().context("Missing callback target")?;
        let outcome: Result<()> = async {
            let code = callback(target, &redirect, state.secret(), &issuer)?;
            let token = client.exchange_code(AuthorizationCode::new(code))?.set_pkce_verifier(verifier).request_async(&http).await.context("Code exchange failed")?;
            let id = token.id_token().context("Issuer did not return an ID token")?;
            if *id.signing_alg()? != CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256 { bail!("Only RS256 ID tokens are accepted"); }
            let verifier = client.id_token_verifier();
            let claims = id.claims(&verifier, &nonce).context("ID token validation failed")?;
            if let Some(expected) = claims.access_token_hash() {
                let actual = AccessTokenHash::from_token(token.access_token(), id.signing_alg()?, id.signing_key(&verifier)?)?;
                if actual != *expected { bail!("Access token hash mismatch"); }
            }
            let expires = token.expires_in().context("Issuer did not specify access token lifetime")?.as_secs();
            let credential = Credential { access_token: token.access_token().secret().clone(), refresh_token: token.refresh_token().map(|value| value.secret().clone()), expires_at: now().saturating_add(expires), issuer, client_id };
            let _lock = credential_lock()?;
            entry(base)?.set_password(&serde_json::to_string(&credential)?).context("Cannot protect login in OS credential store")?;
            Ok(())
        }.await;
        let body = if outcome.is_ok() { "<!doctype html><title>magents</title><h1>Signed in</h1><p>You can return to your terminal.</p>" } else { "<!doctype html><title>magents</title><h1>Login not completed</h1><p>Return to your terminal for details.</p>" };
        let status = if outcome.is_ok() { "200 OK" } else { "400 Bad Request" };
        stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await?;
        outcome
    }).await.context("Login timed out")?;
    result?;
    println!("Logged in. Explore with magents agents --help.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_credentials_remain_readable() {
        let stored = r#"{"access_token":"old","expires_at":123,"issuer":"https://identity.test","client_id":"native"}"#;
        let credential: Credential = serde_json::from_str(stored).unwrap();
        assert!(credential.refresh_token.is_none());
    }
    #[test]
    fn exact_loopback_redirect_only() {
        for url in [
            "http://localhost:9000/cb",
            "https://127.0.0.1:9000/cb",
            "http://127.0.0.1:9000/cb?x=y",
        ] {
            assert!(redirect_url(url).is_err());
        }
        assert!(redirect_url("http://127.0.0.1:9000/cb").is_ok());
        assert!(redirect_url("http://127.0.0.1/callback").is_ok());
    }
    #[test]
    fn rejects_callback_confusion() {
        let redirect = redirect_url("http://127.0.0.1:9000/cb").unwrap();
        let good = "/cb?state=s&iss=https%3A%2F%2Fidentity.test&code=c";
        assert_eq!(
            callback(good, &redirect, "s", "https://identity.test").unwrap(),
            "c"
        );
        assert!(callback(good, &redirect, "other", "https://identity.test").is_err());
        assert!(
            callback(
                &format!("{good}&code=evil"),
                &redirect,
                "s",
                "https://identity.test"
            )
            .is_err()
        );
        assert!(
            callback(
                "/wrong?state=s&iss=https%3A%2F%2Fidentity.test&code=c",
                &redirect,
                "s",
                "https://identity.test"
            )
            .is_err()
        );
    }
}
