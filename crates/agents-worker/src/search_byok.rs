//! Owner-selected Responses search and the explicit platform DeepSeek search service.

use agents_core::ProviderCredentialFamily;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit, RequestRedirect, Result};

use crate::{auth::provider_keys, provider::Usage, telemetry::TraceContext};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_ANSWER_BYTES: usize = 1024 * 1024;
const MAX_SOURCES: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchBinding {
    pub credential_id: String,
    pub model: String,
    #[serde(default = "output_limit")]
    pub max_output_tokens: u32,
    #[serde(default = "call_limit")]
    pub max_tool_calls: u8,
}

fn output_limit() -> u32 {
    1024
}
fn call_limit() -> u8 {
    4
}

impl SearchBinding {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.credential_id, 256)?;
        text(&self.model, 256)?;
        if self.model.is_empty()
            || !(64..=8192).contains(&self.max_output_tokens)
            || !(1..=8).contains(&self.max_tool_calls)
        {
            return Err(failure("invalid_search_binding"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchKind {
    #[serde(rename = "web_search")]
    Current,
    #[serde(rename = "web_search_2025_08_26")]
    CurrentSnapshot,
    #[serde(rename = "web_search_preview")]
    Preview,
    #[serde(rename = "web_search_preview_2025_03_11")]
    PreviewSnapshot,
}

impl SearchKind {
    fn preview(self) -> bool {
        matches!(self, Self::Preview | Self::PreviewSnapshot)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchContextSize {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchContentType {
    Text,
    Image,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserLocation {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchOptions {
    #[serde(rename = "type")]
    pub kind: SearchKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_web_access: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<DomainFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_context_size: Option<SearchContextSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<UserLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_content_types: Option<Vec<SearchContentType>>,
}

impl SearchOptions {
    pub fn validate(&self) -> Result<()> {
        if self.kind.preview() && (self.external_web_access.is_some() || self.filters.is_some())
            || !self.kind.preview() && self.search_content_types.is_some()
        {
            return Err(failure("invalid_search_options"));
        }
        if let Some(domains) = self
            .filters
            .as_ref()
            .and_then(|filter| filter.allowed_domains.as_ref())
        {
            if domains.len() > 100 {
                return Err(failure("invalid_search_domains"));
            }
            for domain in domains {
                public_domain(domain)?;
            }
        }
        if self
            .search_content_types
            .as_ref()
            .is_some_and(|types| types.is_empty() || types.len() > 2)
        {
            return Err(failure("invalid_search_options"));
        }
        if let Some(location) = &self.user_location {
            validate_location(location)?;
        }
        Ok(())
    }

    pub(crate) fn allows(&self, address: &str) -> Result<()> {
        let host = public_url(address)?;
        let Some(domains) = self
            .filters
            .as_ref()
            .and_then(|filter| filter.allowed_domains.as_ref())
        else {
            return Ok(());
        };
        if domains.is_empty() {
            return Ok(());
        }
        for domain in domains {
            let domain = normalized_domain(domain)?;
            if host == domain || host.ends_with(&format!(".{domain}")) {
                return Ok(());
            }
        }
        Err(failure("search_provider_filter_violation"))
    }
}

fn validate_location(location: &UserLocation) -> Result<()> {
    if location
        .kind
        .as_deref()
        .is_some_and(|kind| kind != "approximate")
    {
        return Err(failure("invalid_search_location"));
    }
    if location.country.as_ref().is_some_and(|country| {
        country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic())
    }) {
        return Err(failure("invalid_search_location"));
    }
    for value in [&location.city, &location.region, &location.timezone]
        .into_iter()
        .flatten()
    {
        text(value, 256)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchSource {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SearchAction {
    #[serde(rename = "search")]
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queries: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sources: Option<Vec<SearchSource>>,
    },
    #[serde(rename = "open_page")]
    OpenPage {
        #[serde(default)]
        url: Option<String>,
    },
    #[serde(rename = "find_in_page")]
    FindInPage {
        pattern: String,
        #[serde(default)]
        url: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeSearchCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<SearchAction>,
    /// Provider result objects are untrusted user data, never URLs to fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Value>,
}

/// Offsets refer to the identified native output part, not a later model answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeCitation {
    pub message_id: String,
    pub content_index: usize,
    pub start_index: u64,
    pub end_index: u64,
    pub url: String,
    pub title: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchEvidence {
    pub native_calls: Vec<NativeSearchCall>,
    pub answer: String,
    pub annotations: Vec<NativeCitation>,
    pub sources: Vec<SearchSource>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchStatus {
    Completed,
    Incomplete,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchFailure {
    pub code: String,
    pub http_status: Option<u16>,
    pub outcome_unknown: bool,
}

/// Known usage is retained even when a provider's terminal payload is invalid.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchExchange {
    pub response_id: Option<String>,
    pub request_id: Option<String>,
    pub status: SearchStatus,
    pub evidence: Option<SearchEvidence>,
    pub usage: Usage,
    pub raw_usage: Option<Value>,
    pub incomplete_details: Option<Value>,
    pub failure: Option<SearchFailure>,
}

/// Deliberately not Debug/Serialize: a prepared dispatch owns one resolved key.
pub struct PreparedSearch {
    endpoint: String,
    api_key: String,
    body: Value,
    idempotency_key: String,
    max_calls: u8,
    options: SearchOptions,
    trace: Option<TraceContext>,
}

pub async fn prepare(
    env: &Env,
    account: &str,
    binding: Option<&SearchBinding>,
    options: &SearchOptions,
    query: &str,
    execution_id: &str,
    trace: Option<&TraceContext>,
) -> Result<PreparedSearch> {
    let binding = binding.ok_or_else(|| failure("search_byok_required"))?;
    binding.validate()?;
    options.validate()?;
    let body = request_body(binding, options, query)?;
    text(execution_id, 512)?;
    if execution_id.is_empty() {
        return Err(failure("invalid_search_execution"));
    }
    let credential = provider_keys::credential(env, account, &binding.credential_id).await?;
    if credential.protocol.credential_family() != ProviderCredentialFamily::Openai {
        return Err(failure("search_provider_protocol_unsupported"));
    }
    Ok(PreparedSearch {
        endpoint: format!("{}/responses", credential.base_url.trim_end_matches('/')),
        api_key: credential.api_key,
        body,
        idempotency_key: format!("search-{:x}", Sha256::digest(execution_id.as_bytes())),
        max_calls: binding.max_tool_calls,
        options: options.clone(),
        trace: trace.cloned(),
    })
}

impl PreparedSearch {
    /// The caller commits its dispatch fence before consuming this value once.
    pub async fn dispatch(self) -> Result<SearchExchange> {
        let headers = Headers::new();
        headers.set("content-type", "application/json")?;
        headers.set("authorization", &format!("Bearer {}", self.api_key))?;
        headers.set("idempotency-key", &self.idempotency_key)?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_redirect(RequestRedirect::Manual)
            .with_headers(headers)
            .with_body(Some(self.body.to_string().into()));
        let mut request = Request::new_with_init(&self.endpoint, &init)?;
        if let Some(trace) = &self.trace {
            trace.child().inject(request.headers_mut()?)?;
        }
        let mut response = Fetch::Request(request)
            .send()
            .await
            .map_err(|_| failure("search_provider_outcome_unknown"))?;
        let request_id = response
            .headers()
            .get("x-request-id")?
            .filter(|value| safe_identifier(value, 256));
        if !(200..300).contains(&response.status_code()) {
            return Ok(http_failure(response.status_code(), request_id));
        }
        let value = bounded_json(&mut response).await?;
        Ok(parse_exchange(
            value,
            request_id,
            self.max_calls,
            &self.options,
        ))
    }
}

fn request_body(binding: &SearchBinding, options: &SearchOptions, query: &str) -> Result<Value> {
    text(query, MAX_QUERY_BYTES)?;
    if query.trim().is_empty() {
        return Err(failure("invalid_search_query"));
    }
    Ok(json!({
        "model":binding.model,"input":query,"tools":[options],"tool_choice":"required",
        "max_tool_calls":binding.max_tool_calls,"max_output_tokens":binding.max_output_tokens,
        "store":false,"background":false,"stream":false,
        "include":["web_search_call.action.sources","web_search_call.results"],
    }))
}

async fn bounded_json(response: &mut worker::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    let mut stream = response
        .stream()
        .map_err(|_| failure("search_provider_outcome_unknown"))?;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| failure("search_provider_outcome_unknown"))?;
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err(failure("search_provider_response_limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| failure("search_provider_invalid_json"))
}

fn http_failure(status: u16, request_id: Option<String>) -> SearchExchange {
    SearchExchange {
        response_id: None,
        request_id,
        status: SearchStatus::Failed,
        evidence: None,
        usage: Usage::default(),
        raw_usage: None,
        incomplete_details: None,
        failure: Some(SearchFailure {
            code: "search_provider_http_error".into(),
            http_status: Some(status),
            outcome_unknown: status >= 500,
        }),
    }
}

pub fn parse_exchange(
    value: Value,
    request_id: Option<String>,
    max_calls: u8,
    options: &SearchOptions,
) -> SearchExchange {
    let raw_usage = value
        .get("usage")
        .filter(|usage| usage.is_object())
        .cloned();
    let mut usage = parse_usage(raw_usage.as_ref());
    let response_id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| safe_identifier(id, 256))
        .map(str::to_owned);
    usage.provider_request_id = response_id.clone().or_else(|| request_id.clone());
    let mut exchange = SearchExchange {
        response_id,
        request_id,
        status: SearchStatus::Failed,
        evidence: None,
        usage,
        raw_usage,
        incomplete_details: value
            .get("incomplete_details")
            .filter(|details| !details.is_null())
            .cloned(),
        failure: None,
    };
    match evidence(&value, max_calls, options) {
        Ok(data) => {
            exchange.status = if value["status"] == "completed" {
                SearchStatus::Completed
            } else {
                SearchStatus::Incomplete
            };
            exchange.evidence = Some(data);
        }
        Err(error) => {
            let code = match error {
                worker::Error::RustError(code) => code,
                _ => "search_provider_protocol_error".into(),
            };
            exchange.failure = Some(SearchFailure {
                code,
                http_status: None,
                outcome_unknown: !matches!(
                    value["status"].as_str(),
                    Some("completed" | "incomplete" | "failed" | "cancelled")
                ),
            });
        }
    }
    exchange
}

fn evidence(value: &Value, max_calls: u8, options: &SearchOptions) -> Result<SearchEvidence> {
    if value["object"] != "response"
        || !matches!(value["status"].as_str(), Some("completed" | "incomplete"))
    {
        return Err(failure("search_provider_failed"));
    }
    identifier(value["id"].as_str().unwrap_or(""), 256)?;
    let output = value["output"]
        .as_array()
        .filter(|items| items.len() <= 256)
        .ok_or_else(|| failure("search_provider_protocol_error"))?;
    let mut data = SearchEvidence {
        native_calls: vec![],
        answer: String::new(),
        annotations: vec![],
        sources: vec![],
    };
    for item in output {
        match item["type"].as_str() {
            Some("web_search_call") => add_call(&mut data, item, options)?,
            Some("message") => add_message(&mut data, item, options)?,
            Some("reasoning") => {}
            _ => return Err(failure("search_provider_unexpected_item")),
        }
        if data.native_calls.len() > usize::from(max_calls) || data.sources.len() > MAX_SOURCES {
            return Err(failure("search_provider_result_limit"));
        }
    }
    if data.native_calls.is_empty()
        || data.native_calls.len() > usize::from(max_calls)
        || value["status"] == "completed"
            && data
                .native_calls
                .iter()
                .any(|call| call.status != "completed")
    {
        return Err(failure("search_provider_protocol_error"));
    }
    Ok(data)
}

fn add_call(data: &mut SearchEvidence, value: &Value, options: &SearchOptions) -> Result<()> {
    let call: NativeSearchCall = serde_json::from_value(value.clone())
        .map_err(|_| failure("search_provider_protocol_error"))?;
    identifier(&call.id, 256)?;
    if !matches!(
        call.status.as_str(),
        "in_progress" | "searching" | "completed" | "failed" | "incomplete"
    ) {
        return Err(failure("search_provider_protocol_error"));
    }
    if let Some(action) = &call.action {
        validate_action(action, options, &mut data.sources)?;
    }
    if call.status == "completed" && call.action.is_none() {
        return Err(failure("search_provider_protocol_error"));
    }
    if data
        .native_calls
        .iter()
        .any(|previous| previous.id == call.id)
    {
        return Err(failure("search_provider_protocol_error"));
    }
    data.native_calls.push(call);
    Ok(())
}

fn validate_action(
    action: &SearchAction,
    options: &SearchOptions,
    sources: &mut Vec<SearchSource>,
) -> Result<()> {
    match action {
        SearchAction::Search {
            query,
            queries,
            sources: found,
        } => {
            if let Some(query) = query {
                text(query, MAX_QUERY_BYTES)?;
            }
            if let Some(queries) = queries {
                if queries.len() > 16 {
                    return Err(failure("search_provider_result_limit"));
                }
                for query in queries {
                    text(query, MAX_QUERY_BYTES)?;
                }
            }
            if let Some(found) = found {
                if found.len() > MAX_SOURCES {
                    return Err(failure("search_provider_result_limit"));
                }
                for source in found {
                    if source.kind != "url" {
                        return Err(failure("search_provider_protocol_error"));
                    }
                    options.allows(&source.url)?;
                    insert_source(sources, source.clone())?;
                }
            }
        }
        SearchAction::OpenPage { url } => {
            if let Some(url) = url {
                options.allows(url)?;
                insert_source(
                    sources,
                    SearchSource {
                        kind: "url".into(),
                        url: url.clone(),
                    },
                )?;
            }
        }
        SearchAction::FindInPage { pattern, url } => {
            text(pattern, MAX_QUERY_BYTES)?;
            if let Some(url) = url {
                options.allows(url)?;
                insert_source(
                    sources,
                    SearchSource {
                        kind: "url".into(),
                        url: url.clone(),
                    },
                )?;
            }
        }
    }
    Ok(())
}

fn add_message(data: &mut SearchEvidence, value: &Value, options: &SearchOptions) -> Result<()> {
    if value["role"] != "assistant" {
        return Err(failure("search_provider_protocol_error"));
    }
    let message_id = value["id"]
        .as_str()
        .ok_or_else(|| failure("search_provider_protocol_error"))?;
    identifier(message_id, 256)?;
    let content = value["content"]
        .as_array()
        .filter(|parts| parts.len() <= 100)
        .ok_or_else(|| failure("search_provider_protocol_error"))?;
    for (content_index, part) in content.iter().enumerate() {
        if part["type"] != "output_text" {
            return Err(failure("search_provider_unexpected_content"));
        }
        let answer = part["text"]
            .as_str()
            .ok_or_else(|| failure("search_provider_protocol_error"))?;
        text(answer, MAX_ANSWER_BYTES.saturating_sub(data.answer.len()))?;
        data.answer.push_str(answer);
        let Some(annotations) = part.get("annotations").and_then(Value::as_array) else {
            continue;
        };
        if annotations.len() > MAX_SOURCES {
            return Err(failure("search_provider_result_limit"));
        }
        for annotation in annotations {
            let annotation = citation(annotation, message_id, content_index, answer, options)?;
            insert_source(
                &mut data.sources,
                SearchSource {
                    kind: "url".into(),
                    url: annotation.url.clone(),
                },
            )?;
            data.annotations.push(annotation);
        }
    }
    if data.annotations.len() > MAX_SOURCES {
        return Err(failure("search_provider_result_limit"));
    }
    Ok(())
}

fn insert_source(sources: &mut Vec<SearchSource>, source: SearchSource) -> Result<()> {
    if sources.iter().any(|existing| existing.url == source.url) {
        return Ok(());
    }
    if sources.len() >= MAX_SOURCES {
        return Err(failure("search_provider_result_limit"));
    }
    sources.push(source);
    Ok(())
}

fn citation(
    value: &Value,
    message_id: &str,
    content_index: usize,
    answer: &str,
    options: &SearchOptions,
) -> Result<NativeCitation> {
    if value["type"] != "url_citation" {
        return Err(failure("search_provider_unexpected_annotation"));
    }
    let start = value["start_index"]
        .as_u64()
        .ok_or_else(|| failure("search_provider_protocol_error"))?;
    let end = value["end_index"]
        .as_u64()
        .ok_or_else(|| failure("search_provider_protocol_error"))?;
    let url = value["url"]
        .as_str()
        .ok_or_else(|| failure("search_provider_protocol_error"))?;
    let title = value["title"]
        .as_str()
        .ok_or_else(|| failure("search_provider_protocol_error"))?;
    if start > end || end > answer.len() as u64 {
        return Err(failure("search_provider_protocol_error"));
    }
    options.allows(url)?;
    text(title, 4096)?;
    Ok(NativeCitation {
        message_id: message_id.into(),
        content_index,
        start_index: start,
        end_index: end,
        url: url.into(),
        title: title.into(),
    })
}

fn parse_usage(raw: Option<&Value>) -> Usage {
    let Some(raw) = raw else {
        return Usage::default();
    };
    let input = raw["input_tokens"].as_u64();
    let output = raw["output_tokens"].as_u64();
    let total = raw["total_tokens"].as_u64();
    let cached = raw["input_tokens_details"]["cached_tokens"].as_u64();
    let reasoning = raw["output_tokens_details"]["reasoning_tokens"].as_u64();
    let reported = match (input, output, total, cached) {
        (Some(input), Some(output), Some(total), Some(cached)) => {
            input.checked_add(output) == Some(total)
                && cached <= input
                && reasoning.is_none_or(|tokens| tokens <= output)
        }
        _ => false,
    };
    Usage {
        prompt_tokens: input.unwrap_or(0),
        completion_tokens: output.unwrap_or(0),
        total_tokens: total.unwrap_or(0),
        cached_tokens: cached.unwrap_or(0),
        reasoning_tokens: reasoning,
        reported,
        cost: raw.get("cost").filter(|cost| !cost.is_null()).cloned(),
        ..Usage::default()
    }
}

pub(crate) fn public_url(value: &str) -> Result<String> {
    text(value, 8192)?;
    let url = url::Url::parse(value).map_err(|_| failure("invalid_search_url"))?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(failure("invalid_search_url"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| failure("invalid_search_url"))?;
    public_domain(host)?;
    Ok(host.to_ascii_lowercase())
}

fn public_domain(value: &str) -> Result<()> {
    normalized_domain(value).map(|_| ())
}

fn normalized_domain(value: &str) -> Result<String> {
    let host = url::Host::parse(value).map_err(|_| failure("invalid_search_domain"))?;
    let url::Host::Domain(host) = host else {
        return Err(failure("invalid_search_domain"));
    };
    if !provider_keys::public_hostname(&host)
        || host.len() > 253
        || host.ends_with('.')
        || host.split('.').any(|part| {
            part.is_empty()
                || part.starts_with('-')
                || part.ends_with('-')
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(failure("invalid_search_domain"));
    }
    Ok(host)
}

fn safe_identifier(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:/".contains(&byte))
}

fn identifier(value: &str, limit: usize) -> Result<()> {
    if !safe_identifier(value, limit) {
        return Err(failure("invalid_search_identifier"));
    }
    Ok(())
}

fn text(value: &str, limit: usize) -> Result<()> {
    if value.len() > limit || value.contains('\0') {
        return Err(failure("search_text_limit"));
    }
    Ok(())
}

fn failure(code: &str) -> worker::Error {
    worker::Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> SearchOptions {
        serde_json::from_value(json!({"type":"web_search"})).unwrap()
    }
    fn binding() -> SearchBinding {
        serde_json::from_value(json!({"credential_id":"cred-owned","model":"search-model"}))
            .unwrap()
    }
    fn response() -> Value {
        json!({
            "id":"resp_native","object":"response","status":"completed",
            "usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":null},"cost":0.001234},
            "output":[{"id":"ws_native","type":"web_search_call","status":"completed","action":{"type":"search","queries":["query"],"sources":[{"type":"url","url":"https://www.rust-lang.org/"}]}},
            {"id":"msg_native","type":"message","role":"assistant","content":[{"type":"output_text","text":"Rust source.","annotations":[{"type":"url_citation","start_index":0,"end_index":4,"url":"https://www.rust-lang.org/","title":"Rust"}]}]}],
        })
    }

    #[test]
    fn standard_options_roundtrip_without_silently_losing_filters_or_location() {
        let wire = json!({"type":"web_search_2025_08_26","external_web_access":false,"filters":{"allowed_domains":["rust-lang.org"]},"search_context_size":"high","user_location":{"type":"approximate","country":"US","city":"San Francisco","region":"California","timezone":"America/Los_Angeles"}});
        let options: SearchOptions = serde_json::from_value(wire.clone()).unwrap();
        options.validate().unwrap();
        let body = request_body(&binding(), &options, "Rust releases").unwrap();
        assert_eq!(body["tools"][0], wire);
        assert_eq!(body["store"], false);
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["max_tool_calls"], 4);
        assert!(body.get("api_key").is_none());
    }

    #[test]
    fn preview_is_distinct_from_modern_search_and_invalid_options_fail_before_dispatch() {
        let preview: SearchOptions = serde_json::from_value(
            json!({"type":"web_search_preview_2025_03_11","search_content_types":["text","image"]}),
        )
        .unwrap();
        preview.validate().unwrap();
        let bad: SearchOptions = serde_json::from_value(
            json!({"type":"web_search_preview","filters":{"allowed_domains":["rust-lang.org"]}}),
        )
        .unwrap();
        assert!(bad.validate().is_err());
        let bad: SearchOptions =
            serde_json::from_value(json!({"type":"web_search","search_content_types":["image"]}))
                .unwrap();
        assert!(bad.validate().is_err());
        assert!(request_body(&binding(), &options(), " ").is_err());
    }

    #[test]
    fn native_actions_citations_and_known_usage_remain_tied_to_their_original_text() {
        let mut wire = response();
        wire["output"].as_array_mut().unwrap().push(json!({"id":"ws_open","type":"web_search_call","status":"completed","action":{"type":"open_page","url":"https://www.rust-lang.org/"}}));
        wire["output"].as_array_mut().unwrap().push(json!({"id":"ws_find","type":"web_search_call","status":"completed","action":{"type":"find_in_page","url":"https://www.rust-lang.org/","pattern":"Rust"}}));
        let exchange = parse_exchange(wire, Some("req_owned".into()), 4, &options());
        assert_eq!(exchange.status, SearchStatus::Completed);
        assert!(exchange.usage.reported);
        assert_eq!(exchange.usage.reasoning_tokens, None);
        assert_eq!(exchange.usage.cost, Some(json!(0.001234)));
        let evidence = exchange.evidence.unwrap();
        assert_eq!(evidence.native_calls.len(), 3);
        assert_eq!(evidence.answer, "Rust source.");
        assert_eq!(evidence.annotations[0].message_id, "msg_native");
        assert_eq!(evidence.annotations[0].content_index, 0);
        assert_eq!(evidence.sources.len(), 1);
    }

    #[test]
    fn invalid_or_failed_native_payload_keeps_reported_usage_but_never_claims_success() {
        let mut wire = response();
        wire["output"][0]["status"] = json!("failed");
        let exchange = parse_exchange(wire, None, 4, &options());
        assert_eq!(exchange.status, SearchStatus::Failed);
        assert!(exchange.usage.reported);
        assert!(exchange.evidence.is_none());
        let mut wire = response();
        wire["status"] = json!("failed");
        let exchange = parse_exchange(wire, None, 4, &options());
        assert_eq!(exchange.status, SearchStatus::Failed);
        assert!(!exchange.failure.unwrap().outcome_unknown);
        let mut wire = response();
        wire["usage"]["total_tokens"] = json!(999);
        let exchange = parse_exchange(wire, None, 4, &options());
        assert!(!exchange.usage.reported);
        assert_eq!(exchange.raw_usage.unwrap()["input_tokens"], 10);
    }

    #[test]
    fn source_urls_and_domain_filters_fail_closed_without_fetching_any_source() {
        for url in [
            "http://127.0.0.1/x",
            "https://localhost/x",
            "https://metadata.internal/x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:secret@rust-lang.org/",
        ] {
            assert!(public_url(url).is_err(), "{url}");
        }
        let filtered: SearchOptions = serde_json::from_value(
            json!({"type":"web_search","filters":{"allowed_domains":["docs.rs"]}}),
        )
        .unwrap();
        let exchange = parse_exchange(response(), None, 4, &filtered);
        assert_eq!(exchange.status, SearchStatus::Failed);
        assert_eq!(
            exchange.failure.unwrap().code,
            "search_provider_filter_violation"
        );
        assert!(filtered.allows("https://std.docs.rs/path").is_ok());
        assert!(filtered.allows("https://docs.rs.attacker.net/").is_err());
    }

    #[test]
    fn incomplete_and_unknown_outcomes_are_not_completed_and_http_errors_do_not_leak_bodies() {
        let mut wire = response();
        wire["status"] = json!("incomplete");
        wire["incomplete_details"] = json!({"reason":"max_output_tokens"});
        let exchange = parse_exchange(wire, None, 4, &options());
        assert_eq!(exchange.status, SearchStatus::Incomplete);
        assert_eq!(
            exchange.incomplete_details,
            Some(json!({"reason":"max_output_tokens"}))
        );
        let mut wire = response();
        wire["status"] = json!("in_progress");
        assert!(
            parse_exchange(wire, None, 4, &options())
                .failure
                .unwrap()
                .outcome_unknown
        );
        assert!(http_failure(503, None).failure.unwrap().outcome_unknown);
        assert!(!http_failure(429, None).failure.unwrap().outcome_unknown);
        assert!(
            serde_json::to_string(&http_failure(302, None))
                .unwrap()
                .contains("search_provider_http_error")
        );
    }

    #[test]
    fn repeated_sources_are_bounded_by_unique_url_and_offsets_remain_per_part() {
        let mut wire = response();
        let original = wire["output"][1].clone();
        for number in 1..24 {
            let mut message = original.clone();
            message["id"] = json!(format!("msg_{number}"));
            wire["output"].as_array_mut().unwrap().push(message);
        }
        let exchange = parse_exchange(wire, None, 4, &options());
        assert_eq!(exchange.status, SearchStatus::Completed);
        let evidence = exchange.evidence.unwrap();
        assert_eq!(evidence.annotations.len(), 24);
        assert_eq!(evidence.sources.len(), 1);
        assert!(
            evidence
                .annotations
                .iter()
                .all(|annotation| annotation.start_index == 0)
        );
        assert_ne!(
            evidence.annotations[0].message_id,
            evidence.annotations[1].message_id
        );
    }

    #[test]
    fn public_domain_policy_is_shared_and_idna_filters_match_normalized_urls() {
        for domain in ["service.home", "router.lan", "metadata.google"] {
            assert!(public_domain(domain).is_err());
        }
        let filtered: SearchOptions = serde_json::from_value(json!({
            "type":"web_search", "filters":{"allowed_domains":["例子.中国"]},
        }))
        .unwrap();
        filtered.validate().unwrap();
        assert!(filtered.allows("https://例子.中国/path").is_ok());
    }
}
