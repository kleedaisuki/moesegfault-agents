use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, Response, Url};
use serde_json::Value;
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
