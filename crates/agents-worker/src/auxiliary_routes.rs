//! Authenticated edge dispatch after the shared Responses context resolver.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use worker::{Env, Method, Request, RequestInit, Response, Result};

use crate::{
    model_operation::{CompactLookup, PreparedCompact, operation_id},
    observer_transport, util,
};

pub(crate) async fn handle(
    mut request: Request,
    env: &crate::routes::RouteContext<'_>,
    owner: &str,
) -> Result<Response> {
    if request.method() != Method::Post {
        return util::json_error(405, "method_not_allowed", "Use POST for this operation");
    }
    if request.url()?.query().is_some() {
        return util::json_error(
            400,
            "invalid_request_error",
            "This operation does not accept query parameters",
        );
    }
    let value: Value = match crate::routes::body(&mut request).await {
        Ok(value) => value,
        Err(response) => return Ok(response),
    };
    match execute(&request, env, owner, value).await {
        Ok(response) => Ok(response),
        Err(error) => error_response(&error),
    }
}

async fn execute(
    request: &Request,
    env: &crate::routes::RouteContext<'_>,
    owner: &str,
    value: Value,
) -> Result<Response> {
    let is_compact = request.path().ends_with("/compact");
    let (mut generation, guidance, personality) = parse(&value, is_compact)?;
    let request_hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?));
    if is_compact {
        if let Some(key) = compact_key(request)? {
            if let Some(response) = observe_compact(env, owner, &key, &request_hash).await? {
                return Ok(response);
            }
        }
    }
    let session_header = if is_compact {
        None
    } else {
        request.headers().get("X-Agents-Session")?
    };
    let mut context = crate::auxiliary_context::prepare(
        env,
        owner,
        &mut generation,
        is_compact,
        personality,
        session_header.as_deref(),
    )
    .await?;
    context.config.traceparent = request.headers().get("traceparent")?;
    if !is_compact {
        if context.config.protocol != agents_core::ProviderProtocol::Responses {
            return util::json_error(
                422,
                "token_count_unavailable",
                "Exact counting requires a supported native model renderer",
            );
        }
        return match context.measured_input_tokens {
            Some(input_tokens) => {
                Response::from_json(&crate::responses_auxiliary::InputTokenCount {
                    object: "response.input_tokens".into(),
                    input_tokens,
                })
            }
            None => input_tokens(env, owner, &context.config, &context.prepared).await,
        };
    }
    let native_summary = context.config.protocol == agents_core::ProviderProtocol::Responses
        || crate::provider::is_deepseek(&context.config);
    let mut prepared = PreparedCompact {
        owner: owner.into(),
        operation: String::new(),
        request_hash,
        agent: context.agent,
        model: context.pinned.model.clone(),
        prefix_hash: context.pinned.prefix_hash(),
        instructions: context.instructions,
        protected: context.protected,
        messages: context.messages,
        native_input: Some(context.native),
        retained: context.retained,
        guidance,
        controls: generation.native_controls(),
        traceparent: context.config.traceparent,
    };
    // The native source already contains every retained user. Neither its Chat
    // projection nor the counter's independent request is durable model input.
    prepared.normalize_source(native_summary);
    drop(context.prepared);
    compact(request, env, prepared).await
}

fn parse(
    value: &Value,
    compact: bool,
) -> Result<(
    crate::responses_wire::ResponseRequest,
    Option<String>,
    Option<Value>,
)> {
    let mut generation = value.clone();
    let (guidance, personality) = if compact {
        let request: crate::responses_auxiliary::CompactRequest =
            serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        request.validate()?;
        generation
            .as_object_mut()
            .ok_or_else(invalid)?
            .remove("instructions");
        (request.instructions, None)
    } else {
        let request: crate::responses_auxiliary::CountRequest =
            serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        request.validate()?;
        if request.options.keys().any(|key| {
            !matches!(
                key.as_str(),
                "parallel_tool_calls"
                    | "personality"
                    | "reasoning"
                    | "text"
                    | "tool_choice"
                    | "tools"
                    | "truncation"
            )
        }) {
            return Err(invalid());
        }
        let personality = generation
            .as_object_mut()
            .ok_or_else(invalid)?
            .remove("personality");
        if personality.as_ref().is_some_and(|value| {
            !value
                .as_str()
                .is_some_and(|text| !text.is_empty() && text.len() <= 64)
        }) {
            return Err(invalid());
        }
        (None, personality)
    };
    if generation.get("model").is_some_and(Value::is_null) {
        generation
            .as_object_mut()
            .ok_or_else(invalid)?
            .remove("model");
    }
    let request = serde_json::from_value(generation).map_err(|_| invalid())?;
    Ok((request, guidance, personality))
}

fn invalid() -> worker::Error {
    worker::Error::RustError("invalid_auxiliary_request".into())
}

fn error_response(error: &worker::Error) -> Result<Response> {
    if crate::session::native_items::invalid_lookup(error) {
        return util::json_error(
            400,
            "unresolved_native_item",
            "Referenced or opaque input must belong to this owner and pinned provider context",
        );
    }
    let code = match error {
        worker::Error::RustError(code) => code.as_str(),
        _ => return crate::routes::error_response(error),
    };
    match code {
        "context_length_exceeded" => util::json_error(
            400,
            code,
            "The latest input group cannot fit the selected model context",
        ),
        "compaction_input_too_large" => util::json_error(
            413,
            code,
            "The resolved compaction context exceeds the bounded input size",
        ),
        "mcp_discovery_failed" => util::json_error(
            503,
            code,
            "MCP discovery failed; no zero-schema count was returned",
        ),
        code if code.starts_with("invalid_mcp_") || code == "mcp_authorization_required" => {
            util::json_error(
                400,
                code,
                "Invalid MCP declaration or missing current authorization",
            )
        }
        "invalid_compaction_key" => util::json_error(
            400,
            "invalid_request",
            "Idempotency-Key must contain 1–256 visible ASCII characters",
        ),
        "compaction_observer_unavailable" => util::json_error(
            503,
            code,
            "Observe the original compact key after the operation service is available",
        ),
        "invalid_auxiliary_request" | "invalid_agent_configuration" | "unresolved_native_item" => {
            util::json_error(
                400,
                "invalid_request_error",
                "Invalid auxiliary request or agent configuration",
            )
        }
        "response_not_found" | "agent_not_found" | "conversation_not_found" => {
            util::json_error(404, "not_found", "Owned context was not found")
        }
        "response_context_unavailable" => util::json_error(
            409,
            "response_context_unavailable",
            "The source response is not available for context export",
        ),
        code if code.starts_with("token_count_") => util::json_error(
            422,
            code,
            "This model input does not have a supported exact-count capability",
        ),
        "response_context_mismatch" | "compaction_context_mismatch" => util::json_error(
            400,
            code,
            "Compaction belongs to a different owner, agent, or pinned context",
        ),
        "auxiliary_binary_attachment_unavailable" => util::json_error(
            422,
            code,
            "Inline binary workspace attachments require a stable owned file reference for auxiliary operations",
        ),
        _ => crate::routes::error_response(error),
    }
}

pub(crate) async fn input_tokens(
    env: &Env,
    owner: &str,
    config: &crate::provider::ProviderConfig,
    prepared: &crate::provider::responses::PreparedRequest,
) -> Result<Response> {
    Response::from_json(
        &crate::responses_auxiliary::count_prepared(env, owner, config, prepared).await?,
    )
}

/// `prepared` comes from the same owner/pin/source resolution as generation.
/// Its request hash excludes transport tracing and the retry key itself.
pub(crate) async fn compact(
    request: &Request,
    env: &Env,
    mut prepared: PreparedCompact,
) -> Result<Response> {
    let key = compact_key(request)?.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    prepared.operation = operation_id(&prepared.owner, &key);
    prepared.validate()?;
    let response = dispatch_operation(env, &prepared.operation, "/compact", &prepared).await?;
    observation_response(response, &key, &prepared.operation).await
}

fn compact_key(request: &Request) -> Result<Option<String>> {
    let key = request.headers().get("Idempotency-Key")?;
    if key.as_deref().is_some_and(|key| !valid_key(key)) {
        return Err(worker::Error::RustError("invalid_compaction_key".into()));
    }
    Ok(key)
}

async fn observe_compact(
    env: &Env,
    owner: &str,
    key: &str,
    request_hash: &str,
) -> Result<Option<Response>> {
    let identity = CompactLookup {
        owner: owner.into(),
        operation: operation_id(owner, key),
        request_hash: request_hash.into(),
    };
    let mut response = dispatch_operation(env, &identity.operation, "/observe", &identity).await?;
    if response.status_code() == 404 {
        let body: Value = response.json().await?;
        if body.pointer("/error/code").and_then(Value::as_str) == Some("compaction_not_found") {
            return Ok(None);
        }
        return Err(worker::Error::RustError(
            "compaction_observer_unavailable".into(),
        ));
    }
    observation_response(response, key, &identity.operation)
        .await
        .map(Some)
}

async fn dispatch_operation<T: Serialize>(
    env: &Env,
    operation: &str,
    path: &str,
    body: &T,
) -> Result<Response> {
    let stub = env
        .durable_object("MODEL_OPERATIONS")?
        .id_from_name(operation)?
        .get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(serde_json::to_string(body)?.into()));
    init.headers.set("Content-Type", "application/json")?;
    stub.fetch_with_request(Request::new_with_init(
        &format!("https://model-operation{path}"),
        &init,
    )?)
    .await
}

async fn observation_response(response: Response, key: &str, operation: &str) -> Result<Response> {
    let response = observer_transport::resolve(response).await?;
    let headers = response.headers().clone();
    let mut response = response.with_headers(headers);
    response.headers_mut().set("Idempotency-Key", key)?;
    response
        .headers_mut()
        .set("X-Agents-Operation", operation)?;
    Ok(response)
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 256 && key.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_are_bounded_and_header_safe() {
        assert!(valid_key("compact-retry_01"));
        assert!(!valid_key(""));
        assert!(!valid_key("line\nbreak"));
        assert!(!valid_key(&"x".repeat(257)));
    }

    #[test]
    fn compact_guidance_is_not_ongoing_response_instructions() {
        let (request, guidance, _) = parse(&serde_json::json!({"model":"agent_a","input":"hello","instructions":"Summarize in bullets"}), true).unwrap();
        assert_eq!(guidance.as_deref(), Some("Summarize in bullets"));
        assert!(request.instructions.is_none());
        assert_eq!(request.input, "hello");
    }

    #[test]
    fn count_schema_never_silently_ignores_generation_fields() {
        for field in [
            "stream",
            "background",
            "max_output_tokens",
            "metadata",
            "unknown",
        ] {
            let mut value = serde_json::json!({"model":"agent_a","input":"hello"});
            value[field] = serde_json::json!(true);
            assert!(parse(&value, false).is_err());
        }
        let (request, _, personality) = parse(
            &serde_json::json!({"model":"agent_a","input":"hello",
            "instructions":"Exact scope", "personality":"friendly", "reasoning":{"effort":"low"}}),
            false,
        )
        .unwrap();
        assert_eq!(request.instructions.as_deref(), Some("Exact scope"));
        assert_eq!(personality.unwrap(), "friendly");
    }
}
