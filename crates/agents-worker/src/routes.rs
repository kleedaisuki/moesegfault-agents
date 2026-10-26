use agents_core::{
    AgentConfig, CreateAgent, ModelConfig, PinnedContext, Provider, RegisteredTool, SessionRecord,
    SessionStatus, ToolExecution,
};
use futures::StreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use worker::{Context, Env, Method, Request, RequestInit, Response, Result};

use crate::{auth, provider::Message, store::Store, telemetry::TraceContext, tools, util};

const MAX_BODY_BYTES: usize = 1024 * 1024;

pub(crate) struct RouteContext<'a> {
    env: &'a Env,
    trace: &'a TraceContext,
    background: Option<&'a Context>,
}

impl<'a> RouteContext<'a> {
    pub(crate) fn new(env: &'a Env, trace: &'a TraceContext) -> Self {
        Self {
            env,
            trace,
            background: None,
        }
    }

    pub(crate) fn trace(&self) -> &TraceContext {
        self.trace
    }

    pub(crate) fn defer(&self, task: impl std::future::Future<Output = ()> + 'static) -> bool {
        if let Some(context) = self.background {
            context.wait_until(task);
            return true;
        }
        false
    }
}

impl std::ops::Deref for RouteContext<'_> {
    type Target = Env;
    fn deref(&self) -> &Env {
        self.env
    }
}

pub async fn handle(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let incoming_trace = req.headers().get("traceparent")?;
    let trace = TraceContext::from_traceparent(incoming_trace.as_deref());
    let route = RouteContext {
        env: &env,
        trace: &trace,
        background: Some(&_ctx),
    };
    let _span = crate::telemetry::NativeSpan::start("agents.http", Some(&trace));
    let started_at = util::now_ms();
    let origin = req.headers().get("Origin")?;
    let allowed = env.var("ALLOWED_ORIGIN").ok().map(|v| v.to_string());
    let cors = origin
        .as_ref()
        .filter(|origin| allowed.as_ref() == Some(origin));
    let response = if origin.is_some() && cors.is_none() {
        util::json_error(403, "origin_not_allowed", "Origin is not allowed")
    } else if req.method() == Method::Options {
        Response::empty().map(|r| r.with_status(204))
    } else {
        dispatch(req, &route).await
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => error_response(&error)?,
    };
    let mut response = own_headers(response);
    trace.inject(response.headers_mut())?;
    response.headers_mut().set("Vary", "Origin")?;
    let event_stream = response
        .headers()
        .get("Content-Type")?
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    response.headers_mut().set(
        "Cache-Control",
        if event_stream {
            "no-store, no-transform"
        } else {
            "no-store"
        },
    )?;
    if let Some(origin) = cors {
        response
            .headers_mut()
            .set("Access-Control-Allow-Origin", origin)?;
        response
            .headers_mut()
            .set("Access-Control-Allow-Credentials", "true")?;
        response.headers_mut().set(
            "Access-Control-Allow-Methods",
            "GET, POST, PUT, PATCH, DELETE, OPTIONS",
        )?;
        response.headers_mut().set(
            "Access-Control-Allow-Headers",
            "Authorization, Content-Type, X-CSRF-Token, X-Agents-Session, Idempotency-Key, X-Content-Sha256, If-None-Match, If-Range, Range, traceparent",
        )?;
        response.headers_mut().set(
            "Access-Control-Expose-Headers",
            "X-Agents-Session, X-Agents-Run, X-Agents-Response, X-Content-Sha256, Content-Length, Content-Disposition, Accept-Ranges, Content-Range, ETag, traceparent",
        )?;
        response
            .headers_mut()
            .set("Access-Control-Max-Age", "600")?;
    }
    crate::telemetry::emit(
        "request.completed",
        json!({"trace_id":trace.trace_id(),"status":response.status_code(),"duration_ms":util::now_ms().saturating_sub(started_at)}),
    );
    Ok(response)
}

fn own_headers(response: Response) -> Response {
    // Fetch/DO responses carry immutable guards. Headers::clone allocates a new
    // JS Headers; with_headers moves the original body without consuming or teeing it.
    let headers = response.headers().clone();
    response.with_headers(headers)
}

pub(crate) fn error_response(error: &worker::Error) -> Result<Response> {
    let text = match error {
        worker::Error::RustError(code) => code.clone(),
        _ => error.to_string(),
    };
    let (status, code, message) = classify_error(&text);
    util::json_error(status, code, message)
}

pub(crate) fn classify_error(text: &str) -> (u16, &'static str, &'static str) {
    if let Some(error) = attachment_input_error(text) {
        return error;
    }
    // D1 wraps trigger failures in a database error. Match a complete error token,
    // not arbitrary substrings, and never return the underlying error text.
    if text
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|token| token == "budget_exceeded")
    {
        return (
            402,
            "insufficient_credits",
            "Insufficient credits; add credit before starting another run",
        );
    }
    match text {
        "tool_runtime_unavailable" => (
            400,
            "unsupported_tool",
            "The requested hosted tool is not enabled on this deployment",
        ),
        "invalid_provider_model" => (
            400,
            "invalid_provider_model",
            "Model is not registered for the selected provider credential",
        ),
        "unknown_skill" | "invalid_skill" => (
            400,
            "invalid_skill",
            "A requested skill is unknown or invalid; check your skill registry",
        ),
        "unauthorized" | "authentication_failed" | "invalid_session" => (
            401,
            "authentication_error",
            "A valid access token or API key is required",
        ),
        "csrf_failed" => (
            403,
            "csrf_failed",
            "The request failed browser session validation; sign in again",
        ),
        "invalid_callback" | "invalid_state" | "invalid_id_token" | "invalid_access_token" => (
            400,
            "invalid_callback",
            "The sign-in callback is invalid or expired; start sign-in again",
        ),
        "invalid_key_name" | "invalid_key_id" | "invalid_provider" | "invalid_provider_key" => (
            400,
            "invalid_request_error",
            "The credential request is invalid",
        ),
        "credential_route_immutable" => (
            409,
            "credential_route_immutable",
            "Create a new credential to change its protocol or base URL",
        ),
        "provider_key_not_found" => (
            404,
            "provider_key_not_found",
            "The configured provider key no longer exists",
        ),
        "identity_unavailable" => (
            503,
            "identity_unavailable",
            "The identity service is temporarily unavailable",
        ),
        "insufficient_credits" | "resource_reservation_required" => (
            402,
            "insufficient_credits",
            "Insufficient credits; add credit before starting another run",
        ),
        "model_price_unconfigured" | "invalid_model_price" | "invalid_resource_tariff" => (
            402,
            "billing_unavailable",
            "Pricing for this resource is not configured; contact the service administrator",
        ),
        "workspace_startup_timeout" => (
            503,
            "workspace_startup_timeout",
            "The workspace did not become ready within its startup deadline; no command was replayed",
        ),
        "workspace_startup_unavailable" => (
            503,
            "workspace_startup_unavailable",
            "The workspace could not start or become ready; no command was replayed",
        ),
        "workspace_unavailable" | "workspace_state_encoding_failed" | "tool_outcome_unknown" => (
            503,
            "workspace_unavailable",
            "The workspace is unavailable; inspect session diagnostics before retrying",
        ),
        "resource_pricing_not_configured"
        | "resource_tariff_missing"
        | "invalid_resource_pricing" => (
            503,
            "billing_unavailable",
            "Resource pricing is unavailable; contact the service administrator",
        ),
        "run_limit_exceeded" => (
            429,
            "run_limit_exceeded",
            "Your plan's concurrent run limit has been reached",
        ),
        "provider_usage_missing"
        | "provider_http_error"
        | "provider_protocol_error"
        | "openrouter_cost_missing"
        | "invalid_provider_usage"
        | "invalid provider event stream"
        | "invalid provider stream chunk"
        | "provider stream ended before completion" => (
            502,
            "provider_error",
            "The model provider returned an incomplete or invalid response",
        ),
        "idempotency_conflict" => (
            409,
            "idempotency_conflict",
            "This request key was already used with different parameters",
        ),
        "session_not_found" => (404, "not_found", "Session not found"),
        "file_not_found" => (404, "file_not_found", "File not found in your Space"),
        "file_not_ready" | "file_upload_pending" => (
            409,
            "file_not_ready",
            "The file is not ready; wait for the upload to finish",
        ),
        "file_workspace_conflict" => (
            409,
            "file_workspace_conflict",
            "The file conflicts with an existing workspace path",
        ),
        "file_in_use" => (
            409,
            "file_in_use",
            "The file is in use by an active response; wait for it to finish or cancel it before deleting",
        ),
        "space_quota_exceeded" => (
            409,
            "space_quota_exceeded",
            "Your Space storage limit has been reached",
        ),
        "space_limit_exceeded" => (
            409,
            "space_limit_exceeded",
            "Your Space storage or file-count limit has been reached",
        ),
        "storage_debt" => (
            402,
            "storage_debt",
            "Resolve your account's storage balance before uploading more files",
        ),
        "invalid_workspace_version" | "invalid_workspace_restore" => (
            400,
            "invalid_workspace_version",
            "The workspace version request is invalid",
        ),
        "workspace_version_not_found" => (
            404,
            "workspace_version_not_found",
            "Workspace version not found in your Space",
        ),
        "workspace_version_cursor_not_found" => (
            400,
            "workspace_version_cursor_not_found",
            "The workspace version cursor is invalid",
        ),
        "workspace_version_unavailable" => (
            409,
            "workspace_version_unavailable",
            "The workspace version is not available for this operation",
        ),
        "response_route_unavailable" => (
            503,
            "response_unavailable",
            "The response route is temporarily unavailable; retry using the same request key",
        ),
        "context_budget_exceeded" => (
            400,
            "context_budget_exceeded",
            "The request exceeds the pinned model's context budget",
        ),
        _ if text
            .strip_prefix("provider returned HTTP ")
            .is_some_and(|status| {
                status.len() == 3 && status.bytes().all(|b| b.is_ascii_digit())
            }) =>
        {
            (
                502,
                "provider_error",
                "The model provider could not complete the request",
            )
        }
        _ => (500, "internal_error", "The request could not be completed"),
    }
}

fn attachment_input_error(text: &str) -> Option<(u16, &'static str, &'static str)> {
    const ERRORS: &[(&str, u16, &str)] = &[
        (
            "invalid_attachment",
            400,
            "The attachment contains invalid or unsupported fields",
        ),
        (
            "invalid_file_input",
            400,
            "Provide exactly one supported file source",
        ),
        ("invalid_file_id", 400, "The file ID is invalid"),
        (
            "invalid_file_name",
            400,
            "The attachment filename is invalid",
        ),
        ("invalid_file_data", 400, "The inline file data is invalid"),
        (
            "invalid_image_source",
            400,
            "Use an owned image file or a supported base64 image source",
        ),
        (
            "invalid_image_detail",
            400,
            "Image detail must be auto, low, or high",
        ),
        (
            "invalid_image_size",
            400,
            "The image is empty or exceeds the supported image size or count",
        ),
        (
            "unsupported_image_type",
            400,
            "Use a PNG, JPEG, GIF, or WebP image",
        ),
        (
            "file_requires_workspace",
            400,
            "Enable a workspace to use binary or large file attachments",
        ),
        (
            "too_many_input_items",
            400,
            "The request contains too many input items",
        ),
        (
            "too_many_content_parts",
            400,
            "The message contains too many content parts",
        ),
        (
            "attachments_too_large",
            413,
            "The attachments exceed the supported inline content limit",
        ),
        (
            "file_too_large",
            413,
            "The file exceeds the supported upload size limit",
        ),
        (
            "invalid_file_checksum",
            400,
            "Provide a valid SHA-256 file checksum",
        ),
        (
            "invalid_file_upload",
            400,
            "The file upload request is invalid",
        ),
        ("invalid_file_request", 400, "The file request is invalid"),
        ("invalid_space_request", 400, "The Space request is invalid"),
        (
            "file_length_required",
            411,
            "Provide the file content length",
        ),
        (
            "file_upload_conflict",
            409,
            "This file upload identity was used with different contents",
        ),
    ];
    ERRORS
        .iter()
        .find(|(code, _, _)| *code == text)
        .map(|&(code, status, message)| (status, code, message))
}

async fn dispatch(req: Request, env: &RouteContext<'_>) -> Result<Response> {
    let path = req.path();
    if path == "/healthz" && req.method() == Method::Get {
        return util::health(env);
    }
    if let Some(ip) = req.headers().get("CF-Connecting-IP")? {
        use sha2::{Digest, Sha256};
        let key = format!("ip:{:x}", Sha256::digest(ip.as_bytes()));
        if let Some(response) = rate_limited(env, "INGRESS_RATE_LIMITER", key).await? {
            return Ok(response);
        }
    }
    if path.starts_with("/auth/") {
        return auth::handle_auth(req, env).await;
    }
    if path == auth::BILLING_RETURN_PATH {
        return auth::handle_billing_return(req, env).await;
    }
    if !path.starts_with("/v1/") {
        return util::json_error(404, "not_found", "Route not found");
    }
    let principal = match auth::authenticate(&req, env).await {
        Ok(principal) => principal,
        Err(error) => return error_response(&error),
    };
    if let Some(response) =
        rate_limited(env, "ACCOUNT_RATE_LIMITER", principal.account_id.clone()).await?
    {
        return Ok(response);
    }
    if !matches!(req.method(), Method::Get | Method::Head | Method::Options) {
        if let Some(response) =
            rate_limited(env, "MUTATION_RATE_LIMITER", principal.account_id.clone()).await?
        {
            return Ok(response);
        }
    }
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    if matches!(
        segments.get(1),
        Some(&"account" | &"api-keys" | &"provider-keys" | &"diagnostics")
    ) {
        return auth::handle_account(req, env, &principal).await;
    }
    let owner = &principal.account_id;
    if segments.as_slice() == ["v1", "responses"] && req.method() == Method::Get {
        return crate::response_websocket_executor::connect(req, env, &principal).await;
    }
    match segments.as_slice() {
        ["v1", "space", "workspace-versions", tail @ ..] => {
            crate::workspace_versions::route(req, env, owner, tail).await
        }
        ["v1", "containers", cid] => crate::code_context_routes::handle(req, env, owner, cid).await,
        ["v1", "space" | "files", ..] => crate::space::handle(req, env, owner, &path).await,
        ["v1", "skills"] => crate::skills::handle(req, env, &principal, None).await,
        ["v1", "skills", name] => crate::skills::handle(req, env, &principal, Some(name)).await,
        ["v1", "usage"] if req.method() == Method::Get => {
            Response::from_json(&Store::new(env).usage_summary(owner).await?)
        }
        ["v1", "agents"] => agents(req, env, owner, None).await,
        ["v1", "agents", id] => agents(req, env, owner, Some(id)).await,
        ["v1", "agents", id, "sessions"] => agent_sessions(req, env, owner, id).await,
        ["v1", "sessions", id] => session_route(req, env, owner, id, None).await,
        ["v1", "sessions", id, "workspace", "archive"] => {
            session_route(req, env, owner, id, Some("workspace-archive")).await
        }
        ["v1", "sessions", id, "workspace", "commit"] => {
            session_route(req, env, owner, id, Some("workspace-commit")).await
        }
        ["v1", "sessions", id, "workspace", "restore"] => {
            session_route(req, env, owner, id, Some("workspace-restore")).await
        }
        ["v1", "sessions", id, action] => session_route(req, env, owner, id, Some(action)).await,
        ["v1", "sessions"] if req.method() == Method::Get => Response::from_json(
            &json!({"object":"list","data":Store::new(env).list_sessions(owner).await?}),
        ),
        ["v1", "vector_stores", ..] => crate::file_search_routes::handle(req, env, owner).await,
        ["v1", "models"] if req.method() == Method::Get => {
            let data = Store::new(env).list_agents(owner).await?.into_iter().map(|agent| {
                json!({"id":agent.id,"object":"model","created":agent.created_at_ms / 1000,"owned_by":owner})
            }).collect::<Vec<_>>();
            let available_models = model_registry(env)?;
            let available_platform_models =
                Store::new(env).available_platform_models(&available_models);
            Response::from_json(
                &json!({"object":"list","data":data,"available_models":available_models,"available_platform_models":available_platform_models}),
            )
        }
        ["v1", "chat", "completions"] if req.method() == Method::Post => {
            chat(req, env, owner).await
        }
        ["v1", "conversations", ..] => crate::conversation_routes::handle(req, env, owner).await,
        ["v1", "responses", ..] => crate::response_routes::handle(req, env, owner).await,
        _ => util::json_error(404, "not_found", "Route not found"),
    }
}

pub(crate) async fn rate_limited(
    env: &Env,
    binding: &str,
    key: String,
) -> Result<Option<Response>> {
    if env.rate_limiter(binding)?.limit(key).await?.success {
        return Ok(None);
    }
    let mut response = util::json_error(
        429,
        "request_rate_exceeded",
        "Too many requests; retry after the indicated delay",
    )?;
    response.headers_mut().set("Retry-After", "60")?;
    Ok(Some(response))
}

pub(crate) async fn body<T: DeserializeOwned>(
    req: &mut Request,
) -> std::result::Result<T, Response> {
    let mut stream = req
        .stream()
        .map_err(|_| bad_request("Could not read JSON body"))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| bad_request("Could not read JSON body"))?;
        if chunk.len() > MAX_BODY_BYTES.saturating_sub(bytes.len()) {
            return Err(
                util::json_error(413, "body_too_large", "Body exceeds 1 MiB").expect("JSON error"),
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| bad_request("Request body is not valid for this endpoint"))
}

fn bad_request(message: &str) -> Response {
    util::json_error(400, "invalid_request_error", message).expect("JSON error")
}

pub fn model_registry(env: &Env) -> Result<Vec<ModelConfig>> {
    let models = match env.var("MODEL_REGISTRY_JSON") {
        Ok(value) => serde_json::from_str::<Vec<ModelConfig>>(&value.to_string())
            .map_err(|_| worker::Error::RustError("Invalid MODEL_REGISTRY_JSON".into()))?,
        Err(_) => vec![ModelConfig::default()],
    };
    if models.is_empty()
        || models.iter().any(|m| {
            m.model.is_empty()
                || m.context_window == 0
                || m.max_output_tokens == 0
                || m.max_output_tokens >= m.context_window
        })
    {
        return Err(worker::Error::RustError(
            "Model registry contains invalid limits".into(),
        ));
    }
    Ok(models)
}

pub(crate) async fn normalize_model(
    env: &Env,
    owner: &str,
    model: &mut ModelConfig,
) -> Result<bool> {
    if model.provider == Provider::Byok {
        let Some(id) = model.credential_id.as_deref() else {
            return Ok(false);
        };
        let metadata = match auth::provider_metadata(env, owner, id).await {
            Ok(metadata) => metadata,
            Err(worker::Error::RustError(code))
                if code == "provider_key_not_found" || code == "invalid_provider" =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        if !valid_byok_model(model) {
            return Ok(false);
        }
        if model.protocol.is_some_and(|protocol| {
            protocol.credential_family() != metadata.protocol.credential_family()
        }) {
            return Ok(false);
        }
        // Legacy vendor credentials keep their established registered budgets.
        // New generic endpoints use the explicit (or conservative default) budget.
        let legacy_provider = match metadata.provider.as_str() {
            "openrouter" => Some(Provider::OpenRouter),
            "deepseek" => Some(Provider::DeepSeek),
            _ => None,
        };
        if let Some(provider) = legacy_provider {
            if let Some(entry) = model_registry(env)?
                .into_iter()
                .find(|entry| registered_model(entry, provider, &model.model))
            {
                model.context_window = entry.context_window;
                model.max_output_tokens = entry.max_output_tokens;
            }
        }
        return Ok(true);
    }
    let registry = model_registry(env)?;
    if model.protocol.is_some_and(|protocol| {
        protocol.credential_family() != agents_core::ProviderCredentialFamily::Openai
    }) {
        return Ok(false);
    }
    let Some(entry) = registry
        .iter()
        .find(|entry| registered_model(entry, model.provider, &model.model))
    else {
        return Ok(false);
    };
    model.context_window = entry.context_window;
    model.max_output_tokens = entry.max_output_tokens;
    if model.provider != Provider::Byok {
        model.credential_id = None;
    }
    Ok(true)
}

fn valid_byok_model(model: &ModelConfig) -> bool {
    !model.model.is_empty()
        && model.model.len() <= 1024
        && model.model.trim() == model.model
        && !model.model.chars().any(char::is_control)
        && model.context_window > 0
        && model.max_output_tokens > 0
        && model.max_output_tokens < model.context_window
}

fn registered_model(entry: &ModelConfig, provider: Provider, model: &str) -> bool {
    entry.provider == provider && entry.model == model
}

pub(crate) async fn pin(
    env: &RouteContext<'_>,
    agent: &AgentConfig,
) -> Result<std::result::Result<PinnedContext, &'static str>> {
    if let Err(error) =
        crate::tool_service_routes::validate_owned(env, &agent.owner_id, &agent.tool_services).await
    {
        return match error {
            worker::Error::RustError(code)
                if matches!(
                    code.as_str(),
                    "invalid_tool_services"
                        | "invalid_image_asset_origin"
                        | "provider_key_required"
                        | "provider_key_not_found"
                        | "tool_service_protocol_unsupported"
                ) =>
            {
                Ok(Err("Tool service route or owned credential is invalid"))
            }
            error => Err(error),
        };
    }
    if agent.model.provider == Provider::Byok {
        let mut model = agent.model.clone();
        if !normalize_model(env, &agent.owner_id, &mut model).await? {
            return Ok(Err("Model or provider credential is invalid"));
        }
    }
    let registry = tools::registered_tools(agent.workspace);
    let mut resolved = Vec::new();
    for name in &agent.tools {
        let Some(tool) = registry.iter().find(|tool| tool.function.name == *name) else {
            return Ok(Err("Unknown tool or bash requested without a workspace"));
        };
        if resolved
            .iter()
            .any(|tool: &RegisteredTool| tool.name == *name)
        {
            continue;
        }
        resolved.push(RegisteredTool {
            name: name.clone(),
            version: tools::REGISTRY_VERSION.into(),
            execution: if tools::tool_kind(name) == Some(tools::ToolKind::Workspace) {
                ToolExecution::Workspace
            } else {
                ToolExecution::Harness
            },
            description: tool.function.description.clone(),
            parameters: tool.function.parameters.clone(),
        });
    }
    let selected = crate::skills::resolve(env, &agent.owner_id, &agent.skills).await?;
    Ok(Ok(PinnedContext::resolve(agent, resolved, selected)))
}

async fn agents(
    mut req: Request,
    env: &RouteContext<'_>,
    owner: &str,
    id: Option<&str>,
) -> Result<Response> {
    let store = Store::new(env);
    match (req.method(), id) {
        (Method::Get, None) => {
            Response::from_json(&json!({"object":"list","data":store.list_agents(owner).await?}))
        }
        (Method::Get, Some(id)) => match store.get_agent(owner, id).await? {
            Some(agent) => Response::from_json(&agent),
            None => util::json_error(404, "not_found", "Agent not found"),
        },
        (Method::Post, None) | (Method::Put, Some(_)) | (Method::Patch, Some(_)) => {
            let value: Value = match body(&mut req).await {
                Ok(input) => input,
                Err(response) => return Ok(response),
            };
            let Some(fields) = value.as_object() else {
                return Ok(bad_request("Agent configuration must be an object"));
            };
            if fields.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "name"
                        | "model"
                        | "instructions"
                        | "tools"
                        | "skills"
                        | "compaction_percent"
                        | "workspace"
                        | "tool_services"
                )
            }) {
                return Ok(bad_request("Unknown agent configuration field"));
            }
            let value = if req.method() == Method::Patch {
                let Some(previous) = store.get_agent(owner, id.unwrap_or_default()).await? else {
                    return util::json_error(404, "not_found", "Agent not found");
                };
                let mut merged = serde_json::to_value(previous)?;
                for (key, value) in fields {
                    merged[key] = value.clone();
                }
                merged
            } else {
                value
            };
            let mut input: CreateAgent = match serde_json::from_value(value) {
                Ok(input) => input,
                Err(_) => return Ok(bad_request("Invalid agent configuration")),
            };
            if let Err(message) = input.validate() {
                return Ok(bad_request(message));
            }
            if !normalize_model(env, owner, &mut input.model).await? {
                return Ok(bad_request("Model or provider credential is invalid"));
            }
            let now = util::now_ms();
            let mut agent = input.into_config(
                id.map(str::to_owned)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                owner.into(),
                now,
            );
            if let Err(message) = pin(env, &agent).await? {
                return Ok(bad_request(message));
            }
            if let Some(id) = id {
                let Some(previous) = store.get_agent(owner, id).await? else {
                    return util::json_error(404, "not_found", "Agent not found");
                };
                agent.revision = previous
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| worker::Error::RustError("Agent revision exhausted".into()))?;
                agent.created_at_ms = previous.created_at_ms;
                if !store.update_agent(&agent).await? {
                    return util::json_error(409, "conflict", "Agent changed concurrently");
                }
            } else {
                let limits = store.account_plan(owner).await?.limits();
                if !store.create_agent(&agent, limits.max_agents).await? {
                    return util::json_error(
                        429,
                        "agent_limit",
                        "Your plan's agent limit has been reached",
                    );
                }
            }
            Response::from_json(&agent)
                .map(|response| response.with_status(if id.is_some() { 200 } else { 201 }))
        }
        (Method::Delete, Some(id)) => {
            if !store.delete_agent(owner, id, util::now_ms()).await? {
                return util::json_error(
                    409,
                    "agent_not_deletable",
                    "Agent does not exist or still has active sessions",
                );
            }
            Response::from_json(&json!({"id":id,"object":"agent.deleted","deleted":true}))
        }
        _ => util::json_error(405, "method_not_allowed", "Method not allowed"),
    }
}

async fn create_session(
    env: &RouteContext<'_>,
    owner: &str,
    agent_id: &str,
) -> std::result::Result<SessionRecord, Response> {
    match create_session_inner(env, owner, agent_id).await {
        Ok(result) => result,
        Err(error) => Err(error_response(&error).expect("JSON error")),
    }
}

async fn create_session_inner(
    env: &RouteContext<'_>,
    owner: &str,
    agent_id: &str,
) -> Result<std::result::Result<SessionRecord, Response>> {
    create_session_inner_at(env, owner, agent_id, &uuid::Uuid::new_v4().to_string()).await
}

/// Response retry identity reserves the session ID before admission. A lost edge
/// observation can reinitialize that exact DO without consuming a second slot.
pub(crate) async fn response_session(
    env: &RouteContext<'_>,
    owner: &str,
    agent_id: &str,
    session_id: &str,
) -> Result<std::result::Result<SessionRecord, Response>> {
    if let Some(session) = Store::new(env).get_session(owner, session_id).await? {
        return initialize_response_session(env, session).await;
    }
    match create_session_inner_at(env, owner, agent_id, session_id).await {
        Ok(result) => Ok(result),
        Err(error) => {
            if let Some(session) = Store::new(env).get_session(owner, session_id).await? {
                initialize_response_session(env, session).await
            } else {
                Err(error)
            }
        }
    }
}

async fn initialize_response_session(
    env: &RouteContext<'_>,
    session: SessionRecord,
) -> Result<std::result::Result<SessionRecord, Response>> {
    let response = forward(
        env,
        &session,
        Method::Post,
        "/init",
        Some(serde_json::to_value(&session)?),
    )
    .await?;
    if response.status_code() < 300 {
        Ok(Ok(session))
    } else {
        Ok(Err(response))
    }
}

/// Admit a deterministic fork through the normal concurrency and allowance gate.
pub(crate) async fn response_branch_session(
    env: &RouteContext<'_>,
    owner: &str,
    agent_id: &str,
    session_id: &str,
    pinned: Option<PinnedContext>,
) -> Result<std::result::Result<SessionRecord, Response>> {
    response_branch_session_profile(
        env,
        owner,
        agent_id,
        session_id,
        pinned,
        agents_core::RuntimeProfile::Basic1g,
    )
    .await
}

pub(crate) async fn response_session_profile(
    env: &RouteContext<'_>,
    owner: &str,
    agent_id: &str,
    session_id: &str,
    profile: agents_core::RuntimeProfile,
) -> Result<std::result::Result<SessionRecord, Response>> {
    response_branch_session_profile(env, owner, agent_id, session_id, None, profile).await
}

pub(crate) async fn response_branch_session_profile(
    env: &RouteContext<'_>,
    owner: &str,
    agent_id: &str,
    session_id: &str,
    pinned: Option<PinnedContext>,
    profile: agents_core::RuntimeProfile,
) -> Result<std::result::Result<SessionRecord, Response>> {
    if let Some(session) = Store::new(env).get_session(owner, session_id).await? {
        return initialize_response_session(env, session).await;
    }
    match create_session_inner_at_pinned(env, owner, agent_id, session_id, pinned, profile).await {
        Ok(Ok(session)) => Ok(Ok(session)),
        result => {
            if let Some(session) = Store::new(env).get_session(owner, session_id).await? {
                initialize_response_session(env, session).await
            } else {
                result
            }
        }
    }
}

async fn create_session_inner_at(
    env: &RouteContext<'_>,
    owner: &str,
    agent_id: &str,
    session_id: &str,
) -> Result<std::result::Result<SessionRecord, Response>> {
    create_session_inner_at_pinned(
        env,
        owner,
        agent_id,
        session_id,
        None,
        agents_core::RuntimeProfile::Basic1g,
    )
    .await
}

async fn create_session_inner_at_pinned(
    env: &RouteContext<'_>,
    owner: &str,
    agent_id: &str,
    session_id: &str,
    retained_pin: Option<PinnedContext>,
    runtime_profile: agents_core::RuntimeProfile,
) -> Result<std::result::Result<SessionRecord, Response>> {
    let store = Store::new(env);
    let Some(agent) = store.get_agent(owner, agent_id).await? else {
        return Ok(Err(util::json_error(404, "not_found", "Agent not found")?));
    };
    let pinned = match retained_pin {
        Some(pinned) => pinned,
        None => match pin(env, &agent).await? {
            Ok(pinned) => pinned,
            Err(message) => return Ok(Err(bad_request(message))),
        },
    };
    crate::runtime_profiles::validate_config(env, runtime_profile)?;
    let now = util::now_ms();
    let session = SessionRecord {
        id: session_id.into(),
        owner_id: owner.into(),
        agent_id: agent_id.into(),
        status: SessionStatus::Idle,
        created_at_ms: now,
        last_input_at_ms: now,
        input_revision: 0,
        pinned,
        workspace_snapshot: None,
        runtime_profile,
    };
    let namespace = crate::runtime_profiles::namespace(env, &session)?;
    let do_id = namespace
        .id_from_name(&format!("{}:{}", owner, session.id))?
        .to_string();
    let limits = store.account_plan(owner).await?.limits();
    if !store
        .create_session(&session, &do_id, limits.max_concurrent_sessions)
        .await?
    {
        return Ok(Err(util::json_error(
            429,
            "session_limit",
            "Your plan's session limit has been reached",
        )?));
    }
    let result = forward(
        env,
        &session,
        Method::Post,
        "/init",
        Some(serde_json::to_value(&session)?),
    )
    .await;
    match result {
        Ok(response) if response.status_code() < 300 => Ok(Ok(session)),
        Ok(response) => {
            store.close_session(owner, &session.id, now).await?;
            Ok(Err(response))
        }
        Err(error) => {
            store.close_session(owner, &session.id, now).await?;
            Err(error)
        }
    }
}

async fn agent_sessions(
    req: Request,
    env: &RouteContext<'_>,
    owner: &str,
    agent_id: &str,
) -> Result<Response> {
    if req.method() == Method::Post {
        return match create_session(env, owner, agent_id).await {
            Ok(session) => Response::from_json(&session).map(|response| response.with_status(201)),
            Err(response) => Ok(response),
        };
    }
    if req.method() == Method::Get {
        let store = Store::new(env);
        if store.get_agent(owner, agent_id).await?.is_none() {
            return util::json_error(404, "not_found", "Agent not found");
        }
        let data = store.list_agent_sessions(owner, agent_id).await?;
        return Response::from_json(&json!({"object":"list","data":data}));
    }
    util::json_error(405, "method_not_allowed", "Method not allowed")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Append {
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    stream: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Steer {
    content: String,
}

#[derive(Deserialize)]
struct Chat {
    model: String,
    messages: Vec<Message>,
    #[serde(default)]
    stream: bool,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, Value>,
}

fn validate_messages(messages: &[Message]) -> std::result::Result<(), &'static str> {
    if messages.is_empty() || messages.len() > 128 {
        return Err("Supply between 1 and 128 user messages");
    }
    for message in messages {
        if message.role != "user"
            || !message.tool_calls.is_empty()
            || message.tool_call_id.is_some()
            || !message.extra.is_empty()
        {
            return Err(
                "Only user messages may be appended; instructions and tool execution are managed by the server",
            );
        }
        if !matches!(&message.content, Value::String(text) if !text.trim().is_empty()) {
            return Err("Message content must be nonempty text");
        }
    }
    Ok(())
}

async fn session_route(
    mut req: Request,
    env: &RouteContext<'_>,
    owner: &str,
    id: &str,
    action: Option<&str>,
) -> Result<Response> {
    let Some(session) = Store::new(env).get_session(owner, id).await? else {
        return util::json_error(404, "not_found", "Session not found");
    };
    match (req.method(), action) {
        (Method::Get, None) => forward(env, &session, Method::Get, "/status", None).await,
        (Method::Get, Some("view")) => {
            if req.url()?.query().is_some() {
                return Ok(bad_request("Session views do not accept query parameters"));
            }
            forward_conditional(env, &session, "/view", req.headers().get("If-None-Match")?).await
        }
        (Method::Get, Some("messages")) => {
            let url = req.url()?;
            let path = match url.query() {
                Some(query) => format!("/messages?{query}"),
                None => "/messages".into(),
            };
            forward_conditional(env, &session, &path, req.headers().get("If-None-Match")?).await
        }
        (Method::Get, Some("export")) => {
            let Some(format) = crate::exports::requested_format(&req.url()?) else {
                return crate::exports::invalid_format();
            };
            forward(
                env,
                &session,
                Method::Get,
                &format!("/export?format={}", format.as_str()),
                None,
            )
            .await
        }
        (Method::Get, Some("workspace-archive")) => {
            if req.url()?.query().is_some() {
                return util::json_error(
                    400,
                    "invalid_request_error",
                    "Workspace downloads do not accept query fields",
                );
            }
            forward(env, &session, Method::Get, "/workspace/archive", None).await
        }
        (Method::Delete, None) => forward(env, &session, Method::Delete, "/close", None).await,
        (Method::Post, Some("workspace-commit" | "workspace-restore")) => {
            if req.url()?.query().is_some() {
                return Ok(bad_request(
                    "Workspace actions do not accept query parameters",
                ));
            }
            let input: Value = match body(&mut req).await {
                Ok(input) => input,
                Err(response) => return Ok(response),
            };
            let path = if action == Some("workspace-commit") {
                "/workspace/commit"
            } else {
                "/workspace/restore"
            };
            forward(env, &session, Method::Post, path, Some(input)).await
        }
        (Method::Post, Some("resume")) => resume_session(env, owner, session).await,
        (Method::Post, Some("messages")) => {
            let signal = req.inner().signal();
            let append: Append = match body(&mut req).await {
                Ok(input) => input,
                Err(response) => return Ok(response),
            };
            if let Some(content) = append.content {
                if !append.messages.is_empty()
                    || append.stream
                    || content.trim().is_empty()
                    || content.len() > 65536
                {
                    return Ok(bad_request(
                        "Supply content text or messages, not both; content accepts 1 to 65536 bytes",
                    ));
                }
                return forward(
                    env,
                    &session,
                    Method::Post,
                    "/steer",
                    Some(json!({"content":content})),
                )
                .await;
            }
            if let Err(message) = validate_messages(&append.messages) {
                return Ok(bad_request(message));
            }
            run(env, &session, append.messages, append.stream, signal).await
        }
        (Method::Post, Some("steer")) => {
            let steer: Steer = match body(&mut req).await {
                Ok(input) => input,
                Err(response) => return Ok(response),
            };
            if steer.content.trim().is_empty() || steer.content.len() > 65536 {
                return Ok(bad_request(
                    "Steering content must contain 1 to 65536 bytes",
                ));
            }
            forward(
                env,
                &session,
                Method::Post,
                "/steer",
                Some(json!({"content":steer.content})),
            )
            .await
        }
        _ => util::json_error(405, "method_not_allowed", "Method not allowed"),
    }
}

pub(crate) async fn resume_session(
    env: &RouteContext<'_>,
    owner: &str,
    session: SessionRecord,
) -> Result<Response> {
    let store = Store::new(env);
    let resumed = if session.status == SessionStatus::Closed {
        let limits = store.account_plan(owner).await?.limits();
        let admitted = store
            .resume_session(owner, &session.id, limits.max_concurrent_sessions)
            .await?;
        let Some(current) = store.get_session(owner, &session.id).await? else {
            return util::json_error(404, "not_found", "Session not found");
        };
        if !admitted && current.status != SessionStatus::Idle {
            return util::json_error(
                409,
                "session_not_resumable",
                "Session is busy or your plan's session limit has been reached",
            );
        }
        current
    } else {
        session
    };
    if resumed.status != SessionStatus::Idle {
        return util::json_error(409, "session_busy", "Session is busy");
    }
    // Admission survives an unknown forwarding outcome. A retry reconciles the
    // same pinned revision instead of releasing a possibly active slot.
    forward(
        env,
        &resumed,
        Method::Post,
        "/resume",
        Some(serde_json::to_value(&resumed)?),
    )
    .await
}

async fn chat(mut req: Request, env: &RouteContext<'_>, owner: &str) -> Result<Response> {
    let signal = req.inner().signal();
    let session_id = req.headers().get("X-Agents-Session")?;
    let input: Chat = match body(&mut req).await {
        Ok(input) => input,
        Err(response) => return Ok(response),
    };
    if let Err(message) = validate_messages(&input.messages) {
        return Ok(bad_request(message));
    }
    if input.extra.keys().any(|key| {
        !matches!(
            key.as_str(),
            "stream_options" | "user" | "metadata" | "store"
        )
    }) {
        return Ok(bad_request(
            "Generation parameters, tools, and instructions are pinned by the agent configuration",
        ));
    }
    let session = if let Some(id) = session_id {
        let Some(session) = Store::new(env).get_session(owner, &id).await? else {
            return util::json_error(404, "not_found", "Session not found");
        };
        if session.agent_id != input.model {
            return Ok(bad_request(
                "Session does not belong to the requested agent",
            ));
        }
        session
    } else {
        match create_session(env, owner, &input.model).await {
            Ok(session) => session,
            Err(response) => return Ok(response),
        }
    };
    run(env, &session, input.messages, input.stream, signal).await
}

async fn run(
    env: &RouteContext<'_>,
    session: &SessionRecord,
    messages: Vec<Message>,
    stream: bool,
    signal: web_sys::AbortSignal,
) -> Result<Response> {
    let response = match forward(
        env,
        session,
        Method::Post,
        "/run",
        Some(json!({"messages":messages,"stream":stream})),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            let failure = RunForwardFailure::new(&error, &session.id);
            crate::telemetry::emit(
                "request.failed",
                json!({
                    "phase":"session_forward", "trace_id":env.trace.trace_id(),
                    "session_id":session.id, "agent_id":session.agent_id,
                    "outcome":failure.error_family, "error_code":failure.code,
                    "status":failure.status,
                }),
            );
            failure.response()?
        }
    };
    let response = if stream {
        crate::chat_relay::wrap(
            env.env.clone(),
            session.clone(),
            env.trace.clone(),
            response,
            signal,
        )
        .await?
    } else {
        response
    };
    let mut response = own_headers(response);
    response
        .headers_mut()
        .set("X-Agents-Session", &session.id)?;
    Ok(response)
}

struct RunForwardFailure<'a> {
    session_id: &'a str,
    status: u16,
    code: &'static str,
    message: &'static str,
    error_family: &'static str,
}

impl<'a> RunForwardFailure<'a> {
    fn new(error: &worker::Error, session_id: &'a str) -> Self {
        let text = match error {
            worker::Error::RustError(code) => code.clone(),
            _ => error.to_string(),
        };
        let (status, code, message) = classify_error(&text);
        let error_family = match error {
            worker::Error::JsError(_)
            | worker::Error::UnknownJsError { .. }
            | worker::Error::Internal(_) => "javascript",
            worker::Error::BindingError(_) => "binding",
            worker::Error::D1(_) => "d1",
            worker::Error::SerdeJsonError(_) | worker::Error::SerdeWasmBindgenError(_) => {
                "serialization"
            }
            worker::Error::RustError(_) => "runtime",
            _ => "platform",
        };
        Self {
            session_id,
            status,
            code,
            message,
            error_family,
        }
    }

    fn session_header(&self) -> (&'static str, &str) {
        ("X-Agents-Session", self.session_id)
    }

    fn response(&self) -> Result<Response> {
        // A failed observer cannot prove whether the durable input was accepted.
        // Preserve its discoverable session without retrying or relabeling state.
        let mut response = util::json_error(self.status, self.code, self.message)?;
        let (name, value) = self.session_header();
        response.headers_mut().set(name, value)?;
        Ok(response)
    }
}

pub(crate) async fn forward(
    env: &RouteContext<'_>,
    session: &SessionRecord,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Response> {
    forward_inner(env, session, method, path, body, None).await
}

pub(crate) async fn read_chat_run(
    env: &Env,
    session: &SessionRecord,
    trace: &TraceContext,
    run: &str,
) -> Result<Response> {
    let context = RouteContext::new(env, trace);
    forward_raw(
        &context,
        session,
        Method::Get,
        &format!("/chat-runs/{run}?stream=true"),
        None,
        None,
    )
    .await
}

async fn forward_conditional(
    env: &RouteContext<'_>,
    session: &SessionRecord,
    path: &str,
    etag: Option<String>,
) -> Result<Response> {
    if etag.as_ref().is_some_and(|value| value.len() > 1024) {
        return Ok(bad_request("If-None-Match exceeds its limit"));
    }
    forward_inner(env, session, Method::Get, path, None, etag.as_deref()).await
}

async fn forward_inner(
    env: &RouteContext<'_>,
    session: &SessionRecord,
    method: Method,
    path: &str,
    body: Option<Value>,
    etag: Option<&str>,
) -> Result<Response> {
    let response = forward_raw(env, session, method, path, body, etag).await?;
    crate::observer_transport::resolve(response).await
}

async fn forward_raw(
    env: &RouteContext<'_>,
    session: &SessionRecord,
    method: Method,
    path: &str,
    body: Option<Value>,
    etag: Option<&str>,
) -> Result<Response> {
    let namespace = crate::runtime_profiles::namespace(env, session)?;
    let id = namespace.id_from_name(&format!("{}:{}", session.owner_id, session.id))?;
    let stub = id.get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(method);
    if let Some(body) = body {
        init.with_body(Some(body.to_string().into()));
    }
    let mut request = Request::new_with_init(&format!("https://session.internal{path}"), &init)?;
    request
        .headers_mut()?
        .set("Content-Type", "application/json")?;
    if let Some(etag) = etag {
        request.headers_mut()?.set("If-None-Match", etag)?;
    }
    env.trace.child().inject(request.headers_mut()?)?;
    stub.fetch_with_request(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_input_errors_are_bounded_client_errors_not_retryable_server_failures() {
        for code in [
            "invalid_attachment",
            "invalid_file_input",
            "invalid_file_id",
            "invalid_file_name",
            "invalid_file_data",
            "invalid_image_source",
            "invalid_image_detail",
            "invalid_image_size",
            "unsupported_image_type",
            "file_requires_workspace",
            "too_many_input_items",
            "too_many_content_parts",
            "invalid_file_checksum",
            "invalid_file_upload",
            "invalid_file_request",
            "invalid_space_request",
        ] {
            let (status, actual, message) = classify_error(code);
            assert_eq!(status, 400, "{code}");
            assert_eq!(actual, code);
            assert!(!message.contains("SQL"));
            assert!(!message.contains("spaces/"));
        }
        for code in ["attachments_too_large", "file_too_large"] {
            let (status, actual, _) = classify_error(code);
            assert_eq!(status, 413, "{code}");
            assert_eq!(actual, code);
        }
        assert_eq!(classify_error("file_not_found").0, 404);
        assert_eq!(classify_error("file_length_required").0, 411);
        assert_eq!(classify_error("file_upload_conflict").0, 409);
        assert_eq!(classify_error("file_in_use").0, 409);
        assert_eq!(classify_error("space_limit_exceeded").0, 409);
        assert_eq!(classify_error("storage_debt").0, 402);
        for code in [
            "file_integrity_error",
            "file_platform_unavailable",
            "file_upload_failed",
            "space_not_found",
            "invalid_image_source: private bytes",
            "private-file_requires_workspace",
            "file_not_found: owner-secret",
        ] {
            let (status, actual, message) = classify_error(code);
            assert_eq!((status, actual), (500, "internal_error"), "{code}");
            assert!(!message.contains(code));
        }
    }

    #[test]
    fn safe_errors_preserve_actionable_status_without_leaking_backend_details() {
        for (text, status, code) in [
            (
                "D1_ERROR: budget_exceeded: SQLITE_CONSTRAINT",
                402,
                "insufficient_credits",
            ),
            ("run_limit_exceeded", 429, "run_limit_exceeded"),
            ("file_not_found", 404, "file_not_found"),
            ("file_upload_pending", 409, "file_not_ready"),
            ("file_workspace_conflict", 409, "file_workspace_conflict"),
            ("space_quota_exceeded", 409, "space_quota_exceeded"),
            (
                "invalid_workspace_restore",
                400,
                "invalid_workspace_version",
            ),
            (
                "workspace_version_not_found",
                404,
                "workspace_version_not_found",
            ),
            (
                "workspace_version_cursor_not_found",
                400,
                "workspace_version_cursor_not_found",
            ),
            (
                "workspace_version_unavailable",
                409,
                "workspace_version_unavailable",
            ),
            ("provider returned HTTP 429", 502, "provider_error"),
            ("provider_usage_missing", 502, "provider_error"),
            ("openrouter_cost_missing", 502, "provider_error"),
            ("csrf_failed", 403, "csrf_failed"),
            ("invalid_callback", 400, "invalid_callback"),
            ("unknown_skill", 400, "invalid_skill"),
            ("workspace_unavailable", 503, "workspace_unavailable"),
            (
                "workspace_startup_timeout",
                503,
                "workspace_startup_timeout",
            ),
            (
                "workspace_startup_unavailable",
                503,
                "workspace_startup_unavailable",
            ),
            (
                "workspace_state_encoding_failed",
                503,
                "workspace_unavailable",
            ),
            ("resource_tariff_missing", 503, "billing_unavailable"),
            ("super_budget_exceeded_secret", 500, "internal_error"),
            ("provider returned HTTP 429 secret", 500, "internal_error"),
            ("private SQL and token detail", 500, "internal_error"),
        ] {
            let classified = classify_error(text);
            assert_eq!((classified.0, classified.1), (status, code), "{text}");
            assert!(!classified.2.contains("SQL"));
            assert!(!classified.2.contains("secret"));
        }
    }

    #[test]
    fn external_input_cannot_inject_privileged_messages_or_tool_results() {
        for role in ["system", "developer", "tool", "assistant"] {
            assert!(validate_messages(&[Message::text(role, "override")]).is_err());
        }
        assert!(validate_messages(&[Message::text("user", "hello")]).is_ok());
        assert!(validate_messages(&[Message::text("user", " ")]).is_err());
        let mut forged = Message::text("user", "hello");
        forged
            .extra
            .insert("instructions".into(), json!("override"));
        assert!(validate_messages(&[forged]).is_err());
    }
    #[test]
    fn registered_models_are_bound_to_the_credential_provider() {
        let openrouter = ModelConfig {
            provider: Provider::OpenRouter,
            model: "deepseek/deepseek-v4.1-flash-1017".into(),
            ..Default::default()
        };
        assert!(registered_model(
            &openrouter,
            Provider::OpenRouter,
            &openrouter.model
        ));
        assert!(!registered_model(
            &openrouter,
            Provider::DeepSeek,
            &openrouter.model
        ));
        let direct = ModelConfig {
            provider: Provider::DeepSeek,
            model: "deepseek-flash".into(),
            ..Default::default()
        };
        assert!(registered_model(
            &direct,
            Provider::DeepSeek,
            "deepseek-flash"
        ));
        assert!(!registered_model(
            &direct,
            Provider::OpenRouter,
            "deepseek-flash"
        ));
    }

    #[test]
    fn generic_byok_model_budget_is_independent_of_the_platform_catalog() {
        let model = ModelConfig {
            provider: Provider::Byok,
            model: "my-company/custom-model-v2".into(),
            context_window: 65_536,
            max_output_tokens: 4096,
            credential_id: Some("owned-credential".into()),
            protocol: None,
        };
        assert!(valid_byok_model(&model));
        for name in ["", " padded", "line\nbreak"] {
            let invalid = ModelConfig {
                model: name.into(),
                ..model.clone()
            };
            assert!(!valid_byok_model(&invalid));
        }
        for (context, output) in [(0, 1), (8192, 0), (8192, 8192), (8192, 8193)] {
            let invalid = ModelConfig {
                context_window: context,
                max_output_tokens: output,
                ..model.clone()
            };
            assert!(!valid_byok_model(&invalid));
        }
        assert_eq!(classify_error("credential_route_immutable").0, 409);
    }

    #[test]
    fn forwarding_failures_preserve_status_and_session_without_exposing_native_messages() {
        for (error, status, code, family) in [
            (
                worker::Error::JsError("synthetic-secret native exception".into()),
                500,
                "internal_error",
                "javascript",
            ),
            (
                worker::Error::BindingError("synthetic-secret binding detail".into()),
                500,
                "internal_error",
                "binding",
            ),
            (
                worker::Error::RustError("budget_exceeded".into()),
                402,
                "insufficient_credits",
                "runtime",
            ),
            (
                worker::Error::RustError("provider returned HTTP 429".into()),
                502,
                "provider_error",
                "runtime",
            ),
        ] {
            let failure = RunForwardFailure::new(&error, "known-owned-session");
            assert_eq!(
                (failure.status, failure.code, failure.error_family),
                (status, code, family)
            );
            assert_eq!(
                failure.session_header(),
                ("X-Agents-Session", "known-owned-session")
            );
            assert!(!failure.message.contains("synthetic-secret"));
            let safe = crate::telemetry::sanitize_fields(json!({
                "outcome":failure.error_family, "error_code":failure.code,
                "raw_error":error.to_string(),
            }));
            assert_eq!(safe["outcome"], family);
            assert_eq!(safe["error_code"], code);
            assert!(safe.get("raw_error").is_none());
        }
    }
}
