//! A root-only HTTP authority boundary for kernels restricted to Unix sockets.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

const LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum NetworkPolicy {
    #[default]
    Disabled,
    Allowlist {
        allowed_domains: Vec<String>,
        #[serde(default)]
        domain_secrets: Vec<DomainSecret>,
    },
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct DomainSecret {
    domain: String,
    name: String,
    value: String,
}

impl std::fmt::Debug for NetworkPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Disabled => "NetworkPolicy::Disabled",
            Self::Allowlist { .. } => "NetworkPolicy::Allowlist(<private>)",
        })
    }
}

impl NetworkPolicy {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        let Self::Allowlist {
            allowed_domains,
            domain_secrets,
        } = self
        else {
            return Ok(());
        };
        ensure!(
            !allowed_domains.is_empty() && allowed_domains.len() <= 100,
            "invalid network domains"
        );
        ensure!(domain_secrets.len() <= 100, "too many domain secrets");
        ensure!(
            allowed_domains.iter().collect::<BTreeSet<_>>().len() == allowed_domains.len(),
            "duplicate network domain"
        );
        let mut injected = BTreeSet::new();
        for domain in allowed_domains {
            ensure!(
                hostname(domain).as_deref() == Some(domain.as_str()),
                "invalid network domain"
            );
        }
        for secret in domain_secrets {
            ensure!(
                injected.insert((&secret.domain, secret.name.to_ascii_lowercase())),
                "duplicate secret header"
            );
            ensure!(
                allowed_domains.contains(&secret.domain),
                "secret origin not allowed"
            );
            reqwest::header::HeaderName::from_bytes(secret.name.as_bytes())?;
            reqwest::header::HeaderValue::from_str(&secret.value)?;
            ensure!(
                !secret.value.is_empty() && secret.value.len() <= 16_384,
                "invalid domain secret"
            );
            ensure!(
                !reserved_header(&secret.name, true),
                "reserved secret header"
            );
        }
        Ok(())
    }
}

fn hostname(value: &str) -> Option<String> {
    match url::Host::parse(value).ok()? {
        url::Host::Domain(name)
            if name.contains('.')
                && !name.ends_with('.')
                && !name.ends_with(".localhost")
                && !name.ends_with(".local") =>
        {
            Some(name)
        }
        _ => None,
    }
}

fn reserved_header(name: &str, secret: bool) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "host" | "connection" | "content-length" | "transfer-encoding" | "upgrade" | "forwarded"
    ) || name.starts_with("proxy-")
        || name.starts_with("x-forwarded-")
        || !secret && name == "authorization"
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchRequest {
    url: String,
    #[serde(default = "get")]
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: String,
}
fn get() -> String {
    "GET".into()
}

#[derive(Serialize)]
struct FetchResponse {
    status: u16,
    content_type: Option<String>,
    body: String,
}

struct GatewayState {
    policy: NetworkPolicy,
    slots: Arc<Semaphore>,
    cancel: tokio_util::sync::CancellationToken,
}

fn target(policy: &NetworkPolicy, request: &FetchRequest) -> anyhow::Result<url::Url> {
    let url = url::Url::parse(&request.url)?;
    ensure!(
        request.url.len() <= 8192
            && url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "invalid egress URL"
    );
    let host = url
        .host_str()
        .and_then(hostname)
        .ok_or_else(|| anyhow::anyhow!("invalid egress host"))?;
    let NetworkPolicy::Allowlist {
        allowed_domains, ..
    } = policy
    else {
        bail!("network disabled")
    };
    ensure!(allowed_domains.contains(&host), "network host denied");
    if let NetworkPolicy::Allowlist { domain_secrets, .. } = policy {
        ensure!(
            !domain_secrets
                .iter()
                .filter(|secret| secret.domain == host)
                .any(|secret| request
                    .headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case(&secret.name))),
            "secret header override denied"
        );
    }
    ensure!(
        request.headers.len() <= 64
            && request
                .headers
                .iter()
                .all(|(key, value)| !reserved_header(key, false)
                    && key.len() <= 256
                    && value.len() <= 8192),
        "invalid egress headers"
    );
    ensure!(
        matches!(
            request.method.as_str(),
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
        ),
        "invalid egress method"
    );
    Ok(url)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_unspecified()
                || ip.is_multicast()
                || a == 0
                || a >= 224
                || a == 100 && (64..128).contains(&b)
                || a == 192 && b == 0 && (c == 0 || c == 2)
                || a == 198 && (b == 18 || b == 19 || b == 51 && c == 100)
                || a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            let bytes = ip.octets();
            bytes[0] & 0xe0 == 0x20
                && !(bytes[0] == 0x20 && bytes[1] == 1 && bytes[2] <= 1)
                && !(bytes[0] == 0x20 && bytes[1] == 2)
                && !(bytes[0] == 0x20 && bytes[1] == 1 && bytes[2] == 0x0d && bytes[3] == 0xb8)
        }
    }
}

async fn dispatch(state: &GatewayState, request: FetchRequest) -> anyhow::Result<FetchResponse> {
    let url = target(&state.policy, &request)?;
    let host = url.host_str().unwrap().to_owned();
    let addresses: Vec<SocketAddr> = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host.as_str(), 443)),
    )
    .await??
    .take(32)
    .collect();
    ensure!(
        !addresses.is_empty() && addresses.iter().all(|address| public_ip(address.ip())),
        "nonpublic egress address"
    );
    let body = STANDARD.decode(&request.body)?;
    ensure!(body.len() <= LIMIT, "egress body too large");
    let client = reqwest::Client::builder()
        .no_proxy()
        .https_only(true)
        .tls_sni(true)
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(&host, &addresses)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut builder = client
        .request(reqwest::Method::from_bytes(request.method.as_bytes())?, url)
        .body(body);
    for (name, value) in request.headers {
        builder = builder.header(name, value);
    }
    if let NetworkPolicy::Allowlist { domain_secrets, .. } = &state.policy {
        for secret in domain_secrets.iter().filter(|secret| secret.domain == host) {
            builder = builder.header(&secret.name, &secret.value);
        }
    }
    let response = builder.send().await?;
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= LIMIT as u64),
        "egress reply too large"
    );
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(reply_type);
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= LIMIT,
            "egress reply too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(FetchResponse {
        status,
        content_type,
        body: STANDARD.encode(bytes),
    })
}

fn reply_type(value: &str) -> Option<String> {
    if value.len() > 256 {
        return None;
    }
    let essence = value.split(';').next()?.trim().to_ascii_lowercase();
    [
        "application/json",
        "application/octet-stream",
        "application/pdf",
        "application/zip",
        "text/plain",
        "text/html",
        "text/csv",
        "text/xml",
        "application/xml",
        "image/png",
        "image/jpeg",
        "image/webp",
    ]
    .into_iter()
    .find(|allowed| *allowed == essence)
    .map(str::to_owned)
}

async fn fetch(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<Peer>,
    State(state): State<Arc<GatewayState>>,
    Json(request): Json<FetchRequest>,
) -> Response {
    if peer.0 != 1001 {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(_slot) = state.slots.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let outcome = tokio::select! {
        result=dispatch(&state,request)=>result,
        _=state.cancel.cancelled()=>return StatusCode::GONE.into_response(),
    };
    match outcome {
        Ok(value) => Json(value).into_response(),
        Err(_) => (StatusCode::BAD_REQUEST, "egress request rejected").into_response(),
    }
}

pub(super) struct Gateway {
    task: tokio::task::JoinHandle<()>,
    cancel: tokio_util::sync::CancellationToken,
}

#[derive(Clone)]
struct Peer(u32);

#[cfg(target_os = "linux")]
impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, LimitedListener>>
    for Peer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, LimitedListener>) -> Self {
        Self(
            stream
                .io()
                .inner
                .peer_cred()
                .map(|cred| cred.uid())
                .unwrap_or(u32::MAX),
        )
    }
}

#[cfg(target_os = "linux")]
struct LimitedListener {
    inner: tokio::net::UnixListener,
    slots: Arc<Semaphore>,
}

#[cfg(target_os = "linux")]
struct LimitedStream {
    inner: tokio::net::UnixStream,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[cfg(target_os = "linux")]
impl axum::serve::Listener for LimitedListener {
    type Io = LimitedStream;
    type Addr = tokio::net::unix::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let permit = self
                .slots
                .clone()
                .acquire_owned()
                .await
                .expect("private connection limiter remains open");
            match self.inner.accept().await {
                Ok((inner, address)) => {
                    return (
                        LimitedStream {
                            inner,
                            _permit: permit,
                        },
                        address,
                    );
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

#[cfg(target_os = "linux")]
impl tokio::io::AsyncRead for LimitedStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        tokio::io::AsyncRead::poll_read(std::pin::Pin::new(&mut self.inner), cx, buffer)
    }
}

#[cfg(target_os = "linux")]
impl tokio::io::AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        tokio::io::AsyncWrite::poll_write(std::pin::Pin::new(&mut self.inner), cx, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_flush(std::pin::Pin::new(&mut self.inner), cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_shutdown(std::pin::Pin::new(&mut self.inner), cx)
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

#[cfg(target_os = "linux")]
pub(super) fn start(path: &std::path::Path, policy: NetworkPolicy) -> anyhow::Result<Gateway> {
    use std::os::unix::fs::PermissionsExt;
    policy.validate()?;
    let listener = LimitedListener {
        inner: tokio::net::UnixListener::bind(path)?,
        slots: Arc::new(Semaphore::new(8)),
    };
    let encoded = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()))?;
    ensure!(
        unsafe { libc::chown(encoded.as_ptr(), 0, 1001) } == 0,
        "assign egress socket"
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let state = Arc::new(GatewayState {
        policy,
        slots: Arc::new(Semaphore::new(4)),
        cancel: cancel.clone(),
    });
    let router = Router::new()
        .route("/fetch", post(fetch))
        .layer(DefaultBodyLimit::max(24 * 1024 * 1024))
        .with_state(state);
    Ok(Gateway {
        cancel,
        task: tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<Peer>(),
            )
            .await;
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_cannot_grant_ip_redirect_host_or_connection_authority() {
        let policy = NetworkPolicy::Allowlist {
            allowed_domains: vec!["example.com".into()],
            domain_secrets: vec![],
        };
        for value in [
            "https://127.0.0.1/",
            "http://example.com/",
            "https://evil.example.com/",
            "https://user:pass@example.com/",
            "https://example.com:444/",
            "https://example.com/#fragment",
        ] {
            assert!(
                target(
                    &policy,
                    &FetchRequest {
                        url: value.into(),
                        method: get(),
                        headers: BTreeMap::new(),
                        body: String::new()
                    }
                )
                .is_err()
            );
        }
        assert!(reserved_header("Authorization", false));
        assert!(!reserved_header("Authorization", true));
        assert!(reserved_header("Host", true));
        for value in [
            "127.0.0.1",
            "10.1.2.3",
            "100.64.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!public_ip(value.parse().unwrap()), "{value}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
        assert_eq!(
            reply_type("text/plain; secret=private-sentinel"),
            Some("text/plain".into())
        );
        assert!(reply_type("application/x-private-sentinel").is_none());
    }
}
