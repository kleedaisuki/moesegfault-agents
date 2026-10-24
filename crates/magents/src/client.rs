use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, Response, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Write},
    time::Duration,
};

pub struct Api {
    base: Url,
    http: Client,
}

impl Api {
    pub fn new(base: &str) -> Result<Self> {
        let mut base = Url::parse(base).context("Invalid API URL")?;
        let local = matches!(base.host_str(), Some("127.0.0.1" | "[::1]"));
        if base.scheme() != "https" && !(base.scheme() == "http" && local) {
            bail!("API URL requires HTTPS (HTTP is allowed only for loopback development)");
        }
        if !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path() != "/"
        {
            bail!("API URL must be an origin without credentials, path, query or fragment");
        }
        base.set_path("/");
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("magents/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { base, http })
    }

    pub fn base(&self) -> &str {
        self.base.as_str()
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        session: Option<String>,
    ) -> Result<Response> {
        let token = crate::auth::access_token(self.base()).await?;
        let mut request = self
            .http
            .request(method.parse()?, self.base.join(path)?)
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(session) = session {
            request = request.header("x-agents-session", session);
        }
        let response = request.send().await.context("API connection failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let correlation = response
                .headers()
                .get("x-moesegfault-correlation-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unavailable")
                .to_owned();
            // Response bodies can contain upstream credentials or user content; never dump them.
            bail!("API returned {status}; correlation ID: {correlation}");
        }
        Ok(response)
    }

    pub async fn print(&self, method: &str, path: &str, body: Option<Value>) -> Result<()> {
        let response = self.request(method, path, body, None).await?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(());
        }
        let text = response.text().await?;
        if text.is_empty() {
            return Ok(());
        }
        let value: Value = serde_json::from_str(&text).context("API returned invalid JSON")?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok(())
    }

    pub async fn json(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let response = self.request(method, path, body, None).await?;
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Configuration download interrupted")?;
            if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                bail!("Configuration response exceeds 1 MiB");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("API returned invalid configuration JSON")
    }

    pub async fn download_to(
        &self,
        path: &str,
        output: &mut impl Write,
        limit: u64,
        checkpoint: bool,
    ) -> Result<()> {
        tokio::time::timeout(
            Duration::from_secs(300),
            self.download_to_inner(path, output, limit, checkpoint),
        )
        .await
        .context("Download timed out; no output file was published")?
    }

    async fn download_to_inner(
        &self,
        path: &str,
        output: &mut impl Write,
        limit: u64,
        checkpoint: bool,
    ) -> Result<()> {
        let response = self.request("GET", path, None, None).await?;
        let expected_length = response.content_length();
        if expected_length.is_some_and(|length| length > limit) {
            bail!("Download exceeds the supported size limit");
        }
        let expected_hash = if checkpoint {
            if response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                != Some("application/zstd")
            {
                bail!("API returned an unexpected workspace archive format");
            }
            let hash = response
                .headers()
                .get("x-snapshot-sha256")
                .and_then(|value| value.to_str().ok())
                .filter(|value| {
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .context("Workspace archive is missing its integrity receipt")?;
            if expected_length.is_none() {
                bail!("Workspace archive is missing its length receipt");
            }
            Some(hash.to_lowercase())
        } else {
            None
        };
        let mut hasher = Sha256::new();
        let mut length = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Download interrupted; no output file was published")?;
            length = length
                .checked_add(chunk.len() as u64)
                .context("Download size overflow")?;
            if length > limit {
                bail!("Download exceeds the supported size limit");
            }
            hasher.update(&chunk);
            output.write_all(&chunk)?;
        }
        if expected_length.is_some_and(|expected| expected != length) {
            bail!("Download length does not match its receipt");
        }
        if let Some(expected) = expected_hash {
            let actual = format!("{:x}", hasher.finalize());
            if actual != expected {
                bail!("Workspace archive checksum mismatch; no output file was published");
            }
        }
        Ok(())
    }

    pub async fn stream(&self, body: Value, session: Option<String>) -> Result<()> {
        let response = self
            .request("POST", "/v1/chat/completions", Some(body), session)
            .await?;
        if let Some(id) = response
            .headers()
            .get("x-agents-session")
            .and_then(|v| v.to_str().ok())
        {
            eprintln!("Session: {id}");
        }
        let mut events = response.bytes_stream().eventsource();
        while let Some(event) = events.next().await {
            let event = event
                .context("Agent stream interrupted; use the reported session to inspect state")?;
            if event.data == "[DONE]" {
                println!();
                return Ok(());
            }
            let chunk: Value = serde_json::from_str(&event.data).context("Invalid stream event")?;
            if chunk.get("error").is_some() {
                bail!("Agent execution failed; inspect the session for diagnostics");
            }
            if let Some(content) = chunk
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
            {
                print!("{content}");
                io::stdout().flush()?;
            }
        }
        bail!("Agent stream ended without a completion marker; inspect the session before retrying")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origin_safety() {
        for url in [
            "http://evil.test",
            "https://x.test/path",
            "https://user:pass@x.test",
            "https://x.test?token=x",
        ] {
            assert!(Api::new(url).is_err());
        }
        assert!(Api::new("http://127.0.0.1:8787").is_ok());
        assert!(Api::new("https://agents.moesegfault.dev").is_ok());
    }
}
