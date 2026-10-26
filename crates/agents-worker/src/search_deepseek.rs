//! Dedicated DeepSeek Search API: no language-model round trip or key fallback.

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit, RequestRedirect, Result};

use crate::{
    provider::Usage,
    search_byok::{
        NativeSearchCall, SearchAction, SearchContextSize, SearchEvidence, SearchExchange,
        SearchFailure, SearchOptions, SearchSource, SearchStatus,
    },
    telemetry::TraceContext,
};

pub const PRICE_REVISION: &str = "deepseek-direct-search-fetch-2026-10-26";
pub const PAID_UNIT_MICRODOLLARS: u64 = 750;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeepseekAction {
    Search { query: String },
    OpenPage { url: String },
    FindInPage { url: String, pattern: String },
}

impl DeepseekAction {
    fn operation(&self) -> &'static str {
        match self {
            Self::Search { .. } => "search",
            Self::OpenPage { .. } | Self::FindInPage { .. } => "fetch",
        }
    }
}

/// This receipt is independent of language-model token usage. A tariff version
/// and its effective date belong to the owning billing ledger, not this parser.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchRequestReceipt {
    pub provider: String,
    pub operation: String,
    pub successful_nonempty_requests: u32,
    pub price_revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeepseekSearchExchange {
    pub exchange: SearchExchange,
    /// None means dispatch may have happened but no trustworthy receipt exists.
    pub receipt: Option<SearchRequestReceipt>,
}

/// Deliberately not Debug/Serialize; preparation resolves exactly one secret.
pub struct PreparedDeepseekSearch {
    api_key: String,
    body: Value,
    execution_id: String,
    options: SearchOptions,
    trace: Option<TraceContext>,
    action: DeepseekAction,
}

/// Only the caller's explicit PlatformDeepseek service selection may call this.
/// BYOK preparation never enters this route after credential lookup fails.
pub fn prepare_platform(
    env: &Env,
    options: &SearchOptions,
    query: &str,
    execution_id: &str,
    trace: Option<&TraceContext>,
) -> Result<PreparedDeepseekSearch> {
    prepare_platform_action(
        env,
        options,
        &DeepseekAction::Search {
            query: query.into(),
        },
        execution_id,
        trace,
    )
}

pub fn prepare_platform_action(
    env: &Env,
    options: &SearchOptions,
    action: &DeepseekAction,
    execution_id: &str,
    trace: Option<&TraceContext>,
) -> Result<PreparedDeepseekSearch> {
    let body = action_body(options, action)?;
    if execution_id.is_empty() || execution_id.len() > 512 || execution_id.contains('\0') {
        return Err(error("invalid_search_execution"));
    }
    let api_key = env
        .secret("DEEPSEEK_API_KEY")
        .map_err(|_| error("platform_deepseek_search_unavailable"))?
        .to_string();
    if api_key.is_empty() {
        return Err(error("platform_deepseek_search_unavailable"));
    }
    validate_key(&api_key)?;
    Ok(PreparedDeepseekSearch {
        api_key,
        body,
        execution_id: execution_id.to_owned(),
        options: options.clone(),
        trace: trace.cloned(),
        action: action.clone(),
    })
}

/// Dedicated DeepSeek API with an explicitly selected owner credential. This is
/// not inferred from a main-model key and never falls back to the platform key.
pub async fn prepare_byok(
    env: &Env,
    account: &str,
    credential_id: &str,
    options: &SearchOptions,
    query: &str,
    execution_id: &str,
    trace: Option<&TraceContext>,
) -> Result<PreparedDeepseekSearch> {
    prepare_byok_action(
        env,
        account,
        credential_id,
        options,
        &DeepseekAction::Search {
            query: query.into(),
        },
        execution_id,
        trace,
    )
    .await
}

pub async fn prepare_byok_action(
    env: &Env,
    account: &str,
    credential_id: &str,
    options: &SearchOptions,
    action: &DeepseekAction,
    execution_id: &str,
    trace: Option<&TraceContext>,
) -> Result<PreparedDeepseekSearch> {
    let body = action_body(options, action)?;
    if execution_id.is_empty() || execution_id.len() > 512 || execution_id.contains('\0') {
        return Err(error("invalid_search_execution"));
    }
    let credential = crate::auth::provider_keys::credential(env, account, credential_id).await?;
    validate_key(&credential.api_key)?;
    if credential.protocol.credential_family() != agents_core::ProviderCredentialFamily::Openai {
        return Err(error("search_provider_protocol_unsupported"));
    }
    let endpoint = url::Url::parse(&credential.base_url)
        .map_err(|_| error("deepseek_search_endpoint_unsupported"))?;
    if endpoint.host_str() != Some("api.deepseek.com")
        || endpoint.scheme() != "https"
        || !matches!(endpoint.path().trim_end_matches('/'), "" | "/v1")
        || endpoint.port().is_some_and(|port| port != 443)
    {
        return Err(error("deepseek_search_endpoint_unsupported"));
    }
    Ok(PreparedDeepseekSearch {
        api_key: credential.api_key,
        body,
        execution_id: execution_id.to_owned(),
        options: options.clone(),
        trace: trace.cloned(),
        action: action.clone(),
    })
}

impl PreparedDeepseekSearch {
    /// Persist the existing external-dispatch fence before consuming this value.
    pub async fn dispatch(self) -> Result<DeepseekSearchExchange> {
        let headers = Headers::new();
        headers.set("content-type", "application/json")?;
        headers.set("authorization", &format!("Bearer {}", self.api_key))?;
        headers.set(
            "idempotency-key",
            &format!("search-{:x}", Sha256::digest(self.execution_id.as_bytes())),
        )?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_redirect(RequestRedirect::Manual)
            .with_headers(headers)
            .with_body(Some(self.body.to_string().into()));
        let endpoint = format!(
            "https://api.deepseek.com/v1/tools/{}",
            self.action.operation()
        );
        let mut request = Request::new_with_init(&endpoint, &init)?;
        if let Some(trace) = &self.trace {
            trace.child().inject(request.headers_mut()?)?;
        }
        let mut response = Fetch::Request(request)
            .send()
            .await
            .map_err(|_| error("search_provider_outcome_unknown"))?;
        let request_id = response
            .headers()
            .get("x-request-id")?
            .filter(|id| id.len() <= 256 && !id.chars().any(char::is_control));
        if response.status_code() != 200 {
            return Ok(failed(
                "search_provider_http_error",
                Some(response.status_code()),
                response.status_code() >= 500,
                request_id,
                Some(receipt(self.action.operation(), 0)),
            ));
        }
        let mut bytes = Vec::new();
        let mut stream = response.stream()?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| error("search_provider_outcome_unknown"))?;
            if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                return Err(error("search_provider_response_limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value =
            serde_json::from_slice(&bytes).map_err(|_| error("search_provider_invalid_json"))?;
        Ok(match self.action {
            DeepseekAction::Search { .. } => {
                parse_exchange(value, request_id, &self.execution_id, &self.options)
            }
            action => parse_fetch_exchange(value, request_id, &self.execution_id, &action),
        })
    }
}

fn action_body(options: &SearchOptions, action: &DeepseekAction) -> Result<Value> {
    if let DeepseekAction::Search { query } = action {
        return request_body(options, query);
    }
    options.validate()?;
    if options.external_web_access == Some(false)
        || options.user_location.is_some()
        || options.search_content_types.is_some()
    {
        return Err(error("deepseek_search_options_unsupported"));
    }
    let url = match action {
        DeepseekAction::OpenPage { url } => url,
        DeepseekAction::FindInPage { url, pattern } => {
            if pattern.trim().is_empty() || pattern.len() > 1024 || pattern.contains('\0') {
                return Err(error("invalid_search_pattern"));
            }
            url
        }
        DeepseekAction::Search { .. } => unreachable!(),
    };
    if url.chars().count() > 8192 {
        return Err(error("invalid_search_url"));
    }
    options.allows(url)?;
    Ok(json!({"url":url}))
}

fn request_body(options: &SearchOptions, query: &str) -> Result<Value> {
    options.validate()?;
    if options.external_web_access == Some(false)
        || options.user_location.is_some()
        || options.search_content_types.as_ref().is_some_and(|types| {
            types
                .iter()
                .any(|kind| matches!(kind, crate::search_byok::SearchContentType::Image))
        })
    {
        return Err(error("deepseek_search_options_unsupported"));
    }
    if query.trim().is_empty() || query.contains('\0') {
        return Err(error("invalid_search_query"));
    }
    let mut query = query.to_owned();
    if let Some(domains) = options
        .filters
        .as_ref()
        .and_then(|filter| filter.allowed_domains.as_ref())
    {
        if !domains.is_empty() {
            query.push(' ');
            query.push_str(
                &domains
                    .iter()
                    .map(|domain| format!("site:{domain}"))
                    .collect::<Vec<_>>()
                    .join(" OR "),
            );
        }
    }
    if query.chars().count() > 128 {
        return Err(error("deepseek_search_query_too_long"));
    }
    let count = match options.search_context_size {
        Some(SearchContextSize::Low) => 5,
        Some(SearchContextSize::High) => 20,
        _ => 10,
    };
    Ok(json!({"query": query, "count": count, "need_content": false}))
}

/// The model-visible query schema must subtract server-injected site filters.
/// This is a Unicode scalar-value bound, matching DeepSeek's documented limit.
pub fn query_limit(options: &SearchOptions) -> Result<usize> {
    let body = request_body(options, "x")?;
    let injected = body["query"].as_str().unwrap_or_default().chars().count() - 1;
    Ok(128 - injected)
}

fn validate_key(api_key: &str) -> Result<()> {
    if api_key.is_empty() || api_key.len() > 16384 || api_key.chars().any(char::is_control) {
        return Err(error("invalid_search_credential"));
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct SearchResult {
    rerank_score: f64,
    title: String,
    url: String,
    snippet: String,
    summary: String,
    content: String,
    site_name: Option<String>,
    site_icon: Option<String>,
    date: Option<String>,
    authority: i32,
}

/// Public for native payload fixtures; no secret, clock or worker runtime needed.
pub fn parse_exchange(
    value: Value,
    request_id: Option<String>,
    execution_id: &str,
    options: &SearchOptions,
) -> DeepseekSearchExchange {
    let Some(raw_results) = value.get("results").and_then(Value::as_array) else {
        return failed(
            "deepseek_search_invalid_results",
            None,
            true,
            request_id,
            None,
        );
    };
    // Provider charging is based on the HTTP-200 nonempty array, not whether the
    // untrusted payload subsequently passes our publication policy.
    let charge = Some(receipt("search", u32::from(!raw_results.is_empty())));
    match evidence(raw_results, execution_id, options) {
        Ok(evidence) => DeepseekSearchExchange {
            exchange: SearchExchange {
                response_id: None,
                request_id,
                status: SearchStatus::Completed,
                evidence: Some(evidence),
                usage: Usage::default(),
                raw_usage: None,
                incomplete_details: None,
                failure: None,
            },
            receipt: charge,
        },
        Err(_) => failed(
            "deepseek_search_invalid_results",
            None,
            false,
            request_id,
            charge,
        ),
    }
}

fn evidence(raw: &[Value], execution_id: &str, options: &SearchOptions) -> Result<SearchEvidence> {
    if raw.len() > 50 {
        return Err(error("search_provider_result_limit"));
    }
    let mut sources = Vec::new();
    let mut results = Vec::new();
    for value in raw {
        let result: SearchResult = serde_json::from_value(value.clone())
            .map_err(|_| error("deepseek_search_invalid_results"))?;
        if !(0.0..=3.0).contains(&result.rerank_score) || !(-1..=4).contains(&result.authority) {
            return Err(error("deepseek_search_invalid_results"));
        }
        options.allows(&result.url)?;
        if let Some(icon) = &result.site_icon {
            super::search_byok::public_url(icon)?;
        }
        if !sources
            .iter()
            .any(|source: &SearchSource| source.url == result.url)
        {
            sources.push(SearchSource {
                kind: "url".into(),
                url: result.url.clone(),
            });
        }
        results.push(result);
    }
    let results = serde_json::to_value(results)?;
    let answer = results.to_string();
    if answer.len() > 1024 * 1024 {
        return Err(error("search_provider_answer_limit"));
    }
    Ok(SearchEvidence {
        // This is the real platform-hosted search operation, not a fabricated
        // upstream Responses item. The execution ID provides its provenance.
        native_calls: vec![NativeSearchCall {
            id: format!("ws_{:x}", Sha256::digest(execution_id.as_bytes())),
            kind: "web_search_call".into(),
            status: "completed".into(),
            action: Some(SearchAction::Search {
                query: None,
                queries: None,
                sources: Some(sources.clone()),
            }),
            results: Some(results),
        }],
        answer,
        annotations: Vec::new(),
        sources,
    })
}

pub fn parse_fetch_exchange(
    value: Value,
    request_id: Option<String>,
    execution_id: &str,
    action: &DeepseekAction,
) -> DeepseekSearchExchange {
    let Some(content) = value.get("content").and_then(Value::as_str) else {
        return failed(
            "deepseek_fetch_invalid_content",
            None,
            true,
            request_id,
            None,
        );
    };
    let charge = Some(receipt("fetch", u32::from(!content.is_empty())));
    let Some(title) = value.get("title").and_then(Value::as_str) else {
        return failed(
            "deepseek_fetch_invalid_content",
            None,
            false,
            request_id,
            charge,
        );
    };
    if content.len() > 1024 * 1024 || title.len() > 16 * 1024 {
        return failed(
            "deepseek_fetch_content_limit",
            None,
            false,
            request_id,
            charge,
        );
    }
    let (url, native_action, results) = match action {
        DeepseekAction::OpenPage { url } => (
            url,
            SearchAction::OpenPage {
                url: Some(url.clone()),
            },
            json!({"url":url,"title":title,"content":content}),
        ),
        DeepseekAction::FindInPage { url, pattern }
            if !pattern.trim().is_empty() && pattern.len() <= 1024 =>
        {
            // Literal matching only: no user-supplied regular expressions and no
            // further network operations hidden behind a find action.
            let matches: Vec<Value> = content
                .lines()
                .enumerate()
                .filter(|(_, line)| line.contains(pattern.as_str()))
                .take(100)
                .map(|(index, line)| json!({"line_number":index + 1,"text":line}))
                .collect();
            (
                url,
                SearchAction::FindInPage {
                    url: Some(url.clone()),
                    pattern: pattern.clone(),
                },
                json!({"url":url,"title":title,"pattern":pattern,"matches":matches}),
            )
        }
        _ => {
            return failed(
                "deepseek_fetch_invalid_action",
                None,
                false,
                request_id,
                charge,
            );
        }
    };
    if crate::search_byok::public_url(url).is_err() {
        return failed(
            "deepseek_fetch_invalid_url",
            None,
            false,
            request_id,
            charge,
        );
    }
    let sources = vec![SearchSource {
        kind: "url".into(),
        url: url.clone(),
    }];
    let answer = results.to_string();
    // Escaping control characters in JSON can be larger than native Markdown.
    if answer.len() > 1024 * 1024 {
        return failed(
            "deepseek_fetch_content_limit",
            None,
            false,
            request_id,
            charge,
        );
    }
    DeepseekSearchExchange {
        exchange: SearchExchange {
            response_id: None,
            request_id,
            status: SearchStatus::Completed,
            evidence: Some(SearchEvidence {
                native_calls: vec![NativeSearchCall {
                    id: format!("ws_{:x}", Sha256::digest(execution_id.as_bytes())),
                    kind: "web_search_call".into(),
                    status: "completed".into(),
                    action: Some(native_action),
                    results: Some(results),
                }],
                answer,
                annotations: Vec::new(),
                sources,
            }),
            usage: Usage::default(),
            raw_usage: None,
            incomplete_details: None,
            failure: None,
        },
        receipt: charge,
    }
}

fn receipt(operation: &str, units: u32) -> SearchRequestReceipt {
    SearchRequestReceipt {
        provider: "deepseek".into(),
        operation: operation.into(),
        successful_nonempty_requests: units,
        price_revision: PRICE_REVISION.into(),
    }
}

fn failed(
    code: &str,
    status: Option<u16>,
    unknown: bool,
    request_id: Option<String>,
    receipt: Option<SearchRequestReceipt>,
) -> DeepseekSearchExchange {
    DeepseekSearchExchange {
        exchange: SearchExchange {
            response_id: None,
            request_id,
            status: SearchStatus::Failed,
            evidence: None,
            usage: Usage::default(),
            raw_usage: None,
            incomplete_details: None,
            failure: Some(SearchFailure {
                code: code.into(),
                http_status: status,
                outcome_unknown: unknown,
            }),
        },
        receipt,
    }
}

fn error(code: &str) -> worker::Error {
    worker::Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> SearchOptions {
        serde_json::from_value(json!({"type":"web_search"})).unwrap()
    }
    fn item() -> Value {
        json!({"rerank_score":2.4,"title":"Rust","url":"https://rust-lang.org/",
        "snippet":"Rust","summary":"A language","content":"","site_name":"Rust",
        "site_icon":null,"date":null,"authority":3})
    }

    #[test]
    fn dedicated_request_never_declares_model_or_fake_flags() {
        assert_eq!(
            request_body(&options(), "Rust").unwrap(),
            json!({"query":"Rust","count":10,"need_content":false})
        );
        assert!(request_body(&options(), &"火".repeat(128)).is_ok());
        assert!(request_body(&options(), &"火".repeat(129)).is_err());
        let cached =
            serde_json::from_value(json!({"type":"web_search","external_web_access":false}))
                .unwrap();
        assert!(request_body(&cached, "Rust").is_err());
    }

    #[test]
    fn receipt_counts_nonempty_operations_not_tokens_or_results() {
        let exchange = parse_exchange(
            json!({"results":[item(),item()]}),
            None,
            "tool_one",
            &options(),
        );
        assert_eq!(exchange.receipt.unwrap().successful_nonempty_requests, 1);
        assert_eq!(exchange.exchange.status, SearchStatus::Completed);
        let evidence = exchange.exchange.evidence.unwrap();
        assert_eq!(evidence.sources.len(), 1);
        assert!(evidence.annotations.is_empty());
        let empty = parse_exchange(json!({"results":[]}), None, "tool_two", &options());
        assert_eq!(empty.receipt.unwrap().successful_nonempty_requests, 0);
        assert_eq!(empty.exchange.status, SearchStatus::Completed);
    }

    #[test]
    fn rejected_publication_still_preserves_successful_provider_unit() {
        let mut unsafe_result = item();
        unsafe_result["url"] = json!("http://127.0.0.1/private");
        let exchange = parse_exchange(json!({"results":[unsafe_result]}), None, "tool", &options());
        assert_eq!(exchange.receipt.unwrap().successful_nonempty_requests, 1);
        assert_eq!(exchange.exchange.status, SearchStatus::Failed);
        assert!(exchange.exchange.evidence.is_none());
        let malformed = parse_exchange(json!({}), None, "tool", &options());
        assert!(malformed.receipt.is_none());
        assert!(malformed.exchange.failure.unwrap().outcome_unknown);
    }

    #[test]
    fn fetch_and_find_are_one_operation_with_separate_fetch_receipt() {
        let action = DeepseekAction::OpenPage {
            url: "https://rust-lang.org/".into(),
        };
        assert_eq!(
            action_body(&options(), &action).unwrap(),
            json!({"url":"https://rust-lang.org/"})
        );
        let fetched = parse_fetch_exchange(
            json!({"title":"Rust","content":"# Rust\nA language"}),
            None,
            "open1",
            &action,
        );
        assert_eq!(fetched.receipt.unwrap().operation, "fetch");
        assert_eq!(fetched.exchange.status, SearchStatus::Completed);
        let find = DeepseekAction::FindInPage {
            url: "https://rust-lang.org/".into(),
            pattern: "language".into(),
        };
        let found = parse_fetch_exchange(
            json!({"title":"Rust","content":"# Rust\nA language"}),
            None,
            "find1",
            &find,
        );
        let call = &found.exchange.evidence.unwrap().native_calls[0];
        assert_eq!(
            call.results.as_ref().unwrap()["matches"][0]["line_number"],
            2
        );
        assert_eq!(
            call.results.as_ref().unwrap()["matches"][0]["text"],
            "A language"
        );
        let empty =
            parse_fetch_exchange(json!({"title":"Rust","content":""}), None, "open2", &action);
        assert_eq!(empty.receipt.unwrap().successful_nonempty_requests, 0);
    }

    #[test]
    fn unsafe_fetch_and_unsupported_options_fail_before_secret_resolution() {
        let action = DeepseekAction::OpenPage {
            url: "http://169.254.169.254/latest".into(),
        };
        assert!(action_body(&options(), &action).is_err());
        let geographical = serde_json::from_value(
            json!({"type":"web_search","user_location":{"type":"approximate","country":"US"}}),
        )
        .unwrap();
        assert!(request_body(&geographical, "Rust").is_err());
        let restricted = serde_json::from_value(
            json!({"type":"web_search","filters":{"allowed_domains":["rust-lang.org"]}}),
        )
        .unwrap();
        assert_eq!(
            request_body(&restricted, "Rust").unwrap()["query"],
            "Rust site:rust-lang.org"
        );
        let wrong_domain = DeepseekAction::OpenPage {
            url: "https://example.com/".into(),
        };
        assert!(action_body(&restricted, &wrong_domain).is_err());
    }
}
