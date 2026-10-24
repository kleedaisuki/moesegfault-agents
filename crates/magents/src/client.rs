use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, Response, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Write},
    path::Path,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const MAX_CONFIG_JSON_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_JSON_BYTES: usize = 16 * 1024 * 1024;

pub struct Api {
    base: Url,
    http: Client,
}

#[derive(Clone, Copy)]
enum Receipt {
    Workspace,
    File,
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
        self.request_with_key(method, path, body, session, None)
            .await
    }

    async fn request_with_key(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        session: Option<String>,
        idempotency_key: Option<&str>,
    ) -> Result<Response> {
        if let Some(key) = idempotency_key
            && (key.is_empty()
                || key.len() > 255
                || !key.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            bail!("Idempotency key must contain 1 to 255 printable non-space ASCII bytes");
        }
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
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        let response = request.send().await.context("API connection failed")?;
        check_response(response)
    }

    pub async fn print(&self, method: &str, path: &str, body: Option<Value>) -> Result<()> {
        let response = self.request(method, path, body, None).await?;
        print_response(response).await
    }

    pub async fn json(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let response = self.request(method, path, body, None).await?;
        response_json(response, MAX_CONFIG_JSON_BYTES).await
    }

    pub async fn upload_file(&self, path: &str, file: &Path, content_type: &str) -> Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(300),
            self.upload_file_inner(path, file, content_type),
        )
        .await
        .context("Upload timed out; inspect Space files before retrying")?
    }

    async fn upload_file_inner(
        &self,
        path: &str,
        file: &Path,
        content_type: &str,
    ) -> Result<Value> {
        let mut file = tokio::fs::File::open(file)
            .await
            .context("Cannot open upload file")?;
        let metadata = file.metadata().await?;
        let length = metadata.len();
        if !metadata.is_file() || length > crate::space::MAX_UPLOAD_BYTES {
            bail!("Upload must be a regular file no larger than 64 MiB");
        }
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut hashed = 0u64;
        loop {
            let count = file.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            hashed += count as u64;
            if hashed > length {
                bail!("Upload file changed while its checksum was being prepared");
            }
            hasher.update(&buffer[..count]);
        }
        if hashed != length {
            bail!("Upload file changed while its checksum was being prepared");
        }
        let digest = format!("{:x}", hasher.finalize());
        file.rewind().await?;
        let stream =
            futures_util::stream::try_unfold((file, length), |(mut file, left)| async move {
                if left == 0 {
                    return Ok::<_, io::Error>(None);
                }
                let mut chunk = vec![0u8; left.min(64 * 1024) as usize];
                let count = file.read(&mut chunk).await?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Upload file changed",
                    ));
                }
                chunk.truncate(count);
                Ok(Some((chunk, (file, left - count as u64))))
            });
        let token = crate::auth::access_token(self.base()).await?;
        let response = self
            .http
            .post(self.base.join(path)?)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::CONTENT_LENGTH, length)
            .header("x-content-sha256", &digest)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .context("Upload interrupted; inspect Space files before retrying")?;
        let record = response_json(check_response(response)?, MAX_CONFIG_JSON_BYTES).await?;
        if record.get("sha256").and_then(Value::as_str) != Some(digest.as_str())
            || record.get("bytes").and_then(Value::as_u64) != Some(length)
        {
            bail!(
                "Upload receipt does not match the local file; inspect Space files before retrying"
            );
        }
        Ok(record)
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
            self.download_to_inner(
                path,
                output,
                limit,
                checkpoint.then_some(Receipt::Workspace),
            ),
        )
        .await
        .context("Download timed out; no output file was published")?
    }

    pub async fn download_file_to(
        &self,
        path: &str,
        output: &mut impl Write,
        limit: u64,
    ) -> Result<()> {
        tokio::time::timeout(
            Duration::from_secs(300),
            self.download_to_inner(path, output, limit, Some(Receipt::File)),
        )
        .await
        .context("Download timed out; no output file was published")?
    }

    async fn download_to_inner(
        &self,
        path: &str,
        output: &mut impl Write,
        limit: u64,
        receipt: Option<Receipt>,
    ) -> Result<()> {
        let response = self.request("GET", path, None, None).await?;
        let expected_length = response.content_length();
        if expected_length.is_some_and(|length| length > limit) {
            bail!("Download exceeds the supported size limit");
        }
        let expected_hash = if let Some(receipt) = receipt {
            if matches!(receipt, Receipt::Workspace)
                && response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    != Some("application/zstd")
            {
                bail!("API returned an unexpected workspace archive format");
            }
            let header = match receipt {
                Receipt::Workspace => "x-snapshot-sha256",
                Receipt::File => "x-content-sha256",
            };
            let hash = response
                .headers()
                .get(header)
                .and_then(|value| value.to_str().ok())
                .filter(|value| {
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .context("Download is missing its integrity receipt")?;
            if expected_length.is_none() {
                bail!("Download is missing its length receipt");
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
                bail!("Download checksum mismatch; no output file was published");
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

    pub async fn create_response(
        &self,
        body: Value,
        raw_events: bool,
        key: Option<&str>,
    ) -> Result<()> {
        let stream = body.get("stream").and_then(Value::as_bool) == Some(true);
        let response = self
            .request_with_key("POST", "/v1/responses", Some(body), None, key)
            .await?;
        if stream {
            response_stream(response, raw_events).await
        } else {
            print_response_object(response).await
        }
    }

    pub async fn response_object(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<()> {
        let response = self.request(method, path, body, None).await?;
        print_response_object(response).await
    }

    pub async fn retrieve_response_stream(&self, path: &str, raw_events: bool) -> Result<()> {
        let response = self.request("GET", path, None, None).await?;
        response_stream(response, raw_events).await
    }
}

async fn print_response_object(response: Response) -> Result<()> {
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(());
    }
    let value = response_json(response, MAX_RESPONSE_JSON_BYTES).await?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn print_response(response: Response) -> Result<()> {
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

async fn response_stream(response: Response, raw_events: bool) -> Result<()> {
    if let Some(id) = response
        .headers()
        .get("x-agents-session")
        .and_then(|id| id.to_str().ok())
    {
        eprintln!("Session: {id}");
    }
    let mut reported_id = None;
    let mut events = response.bytes_stream().eventsource();
    while let Some(event) = events.next().await {
        let event = event.context(
            "Response stream interrupted; inspect the reported response before retrying",
        )?;
        if event.data == "[DONE]" {
            break;
        }
        let event: Value =
            serde_json::from_str(&event.data).context("Invalid response stream event")?;
        if let Some(id) = event.pointer("/response/id").and_then(Value::as_str)
            && reported_id.as_deref() != Some(id)
        {
            eprintln!("Response: {id}");
            reported_id = Some(id.to_owned());
        }
        if raw_events {
            println!("{}", serde_json::to_string(&event)?);
        }
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") if !raw_events => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    print!("{delta}");
                    io::stdout().flush()?;
                }
            }
            Some("response.completed") => {
                if event.pointer("/response/status").and_then(Value::as_str) != Some("completed") {
                    bail!(
                        "Response completion event has an inconsistent status; inspect the response"
                    );
                }
                if !raw_events {
                    println!();
                }
                if event
                    .pointer("/response/output")
                    .and_then(Value::as_array)
                    .is_some_and(|output| {
                        output.iter().any(|item| {
                            item.get("type").and_then(Value::as_str) == Some("function_call")
                        })
                    })
                {
                    eprintln!(
                        "Local function calls returned; inspect the response and submit only authorized results using responses create"
                    );
                }
                return Ok(());
            }
            Some("response.failed" | "response.incomplete" | "response.cancelled" | "error") => {
                bail!(
                    "Response did not complete successfully; inspect the reported response for diagnostics"
                );
            }
            _ => {}
        }
    }
    bail!("Response stream ended without a terminal response; inspect the response before retrying")
}

fn check_response(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let correlation = response
        .headers()
        .get("x-moesegfault-correlation-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unavailable");
    // Response bodies can contain upstream credentials or user content; never dump them.
    bail!("API returned {status}; correlation ID: {correlation}");
}

async fn response_json(response: Response, limit: usize) -> Result<Value> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        bail!("API response exceeds {} MiB", limit / (1024 * 1024));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("API response interrupted")?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            bail!("API response exceeds {} MiB", limit / (1024 * 1024));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("API returned invalid JSON")
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
