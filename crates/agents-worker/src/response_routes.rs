//! Responses protocol boundary. Routing/retry identity precedes durable dispatch.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::{Method, Request, Response, Result};

use crate::{
    response_index::{self, ResponseRoute},
    responses_wire::ResponseRequest,
    routes::{self, RouteContext},
    store::Store,
    util,
};

pub(crate) async fn handle(req: Request, env: &RouteContext<'_>, owner: &str) -> Result<Response> {
    let path = req.path();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        ["v1", "responses"] if req.method() == Method::Post => create(req, env, owner).await,
        ["v1", "responses"] => method_not_allowed(),
        ["v1", "responses", id] if valid_id(id) => {
            let method = req.method();
            if !matches!(method, Method::Get | Method::Delete) {
                return method_not_allowed();
            }
            let query = match retrieve_query(&req.url()?, method == Method::Get) {
                Ok(query) => query,
                Err(message) => return invalid(message),
            };
            indexed(req, env, owner, id, &query).await
        }
        ["v1", "responses", id, action] if valid_id(id) => {
            let query = match (*action, req.method()) {
                ("cancel", Method::Post) if req.url()?.query().is_none() => String::new(),
                ("input_items", Method::Get) => match input_query(&req.url()?) {
                    Ok(query) => query,
                    Err(message) => return invalid(message),
                },
                ("events", Method::Get) => match event_query(&req.url()?) {
                    Ok(query) => query,
                    Err(message) => return invalid(message),
                },
                ("cancel" | "input_items" | "events", _) => return method_not_allowed(),
                _ => return not_found(),
            };
            indexed(req, env, owner, id, &format!("/{action}{query}")).await
        }
        _ => not_found(),
    }
}

async fn create(mut req: Request, env: &RouteContext<'_>, owner: &str) -> Result<Response> {
    if req.url()?.query().is_some() {
        return invalid("Responses creation does not accept query parameters");
    }
    let session_header = req.headers().get("X-Agents-Session")?;
    let key = match idempotency_key(req.headers().get("Idempotency-Key")?.as_deref()) {
        Ok(key) => key,
        Err(message) => return invalid(message),
    };
    let mut value: Value = match routes::body(&mut req).await {
        Ok(value) => value,
        Err(response) => return Ok(response),
    };
    let previous = match value.get("previous_response_id") {
        Some(Value::String(id)) if valid_id(id) => response_index::get(env, owner, id).await?,
        Some(Value::String(_)) => return not_found(),
        Some(Value::Null) | None => None,
        _ => return invalid("previous_response_id must be a response ID or null"),
    };
    if value
        .get("previous_response_id")
        .is_some_and(Value::is_string)
        && previous.is_none()
    {
        return not_found();
    }
    if let Some(parent) = &previous {
        if value.get("model").is_none_or(Value::is_null) {
            value["model"] = json!(parent.agent_id);
        }
    }
    let input: ResponseRequest = match serde_json::from_value(value.clone()) {
        Ok(input) => input,
        Err(_) => return invalid("Request body is not valid for Responses creation"),
    };
    if let Err(error) = crate::response_preflight::validate(&input) {
        return util::json_error(400, error.code, &error.message);
    }
    if let Some(parent) = &previous {
        if session_header
            .as_ref()
            .is_some_and(|id| id != &parent.session_id)
        {
            return invalid("The session does not contain the previous response");
        }
    }
    let request_hash = request_hash(&value, session_header.as_deref());
    let existing_session = previous.is_some() || session_header.is_some();
    if let Some(key) = &key {
        if let Some(route) = response_index::by_key(env, owner, key).await? {
            return dispatch_create(env, owner, route, &request_hash, value, existing_session)
                .await;
        }
    }
    let session_id = match (&previous, session_header) {
        (Some(parent), _) => parent.session_id.clone(),
        (None, Some(id)) => {
            let Some(session) = Store::new(env).get_session(owner, &id).await? else {
                return not_found();
            };
            if session.agent_id != input.model {
                return invalid("The session belongs to a different agent");
            }
            id
        }
        (None, None) => {
            if Store::new(env)
                .get_agent(owner, &input.model)
                .await?
                .is_none()
            {
                return util::json_error(404, "not_found", "Agent not found");
            }
            uuid::Uuid::new_v4().to_string()
        }
    };
    let route = response_index::reserve(
        env,
        owner,
        ResponseRoute {
            id: util::id("resp"),
            session_id,
            agent_id: input.model,
            request_hash: request_hash.clone(),
        },
        key.as_deref(),
    )
    .await?;
    dispatch_create(env, owner, route, &request_hash, value, existing_session).await
}

async fn dispatch_create(
    env: &RouteContext<'_>,
    owner: &str,
    route: ResponseRoute,
    request_hash: &str,
    request: Value,
    existing_session: bool,
) -> Result<Response> {
    if route.request_hash != request_hash {
        return util::json_error(
            409,
            "idempotency_conflict",
            "This Idempotency-Key was already used for a different request",
        );
    }
    let session = if existing_session {
        let Some(session) = Store::new(env)
            .get_session(owner, &route.session_id)
            .await?
        else {
            return not_found();
        };
        session
    } else {
        match routes::response_session(env, owner, &route.agent_id, &route.session_id).await {
            Ok(Ok(session)) => session,
            Ok(Err(response)) => return with_identity(response, &route),
            Err(error) => return with_identity(routes::error_response(&error)?, &route),
        }
    };
    let payload = json!({"response_id":route.id,"request":request});
    let mut response = match routes::forward(
        env,
        &session,
        Method::Post,
        "/responses",
        Some(payload.clone()),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => return with_identity(routes::error_response(&error)?, &route),
    };
    if response.status_code() != 409 {
        retire_rejected_session(
            env,
            &session,
            &route,
            existing_session,
            response.status_code(),
        )
        .await;
        return with_identity(response, &route);
    }
    let headers = response.headers().clone();
    let body: Value = response.json().await?;
    let code = body.pointer("/error/code").and_then(Value::as_str);
    if code == Some("response_branch_required")
        || (code == Some("session_busy") && request["previous_response_id"].is_string())
    {
        return dispatch_branch(env, owner, &route, &session, request).await;
    }
    if body.pointer("/error/code").and_then(Value::as_str) != Some("response_session_closed") {
        retire_rejected_session(env, &session, &route, existing_session, 409).await;
        return with_identity(
            Response::from_json(&body)?
                .with_status(409)
                .with_headers(headers),
            &route,
        );
    }
    let Some(current) = Store::new(env)
        .get_session(owner, &route.session_id)
        .await?
    else {
        return not_found();
    };
    let resumed = routes::resume_session(env, owner, current).await?;
    if resumed.status_code() >= 300 {
        return with_identity(resumed, &route);
    }
    // Only the canonical DO's pre-admission readmit signal permits this retry.
    // An ambiguous forwarding failure never authorizes a second dispatch.
    let response =
        match routes::forward(env, &session, Method::Post, "/responses", Some(payload)).await {
            Ok(response) => response,
            Err(error) => return with_identity(routes::error_response(&error)?, &route),
        };
    retire_rejected_session(
        env,
        &session,
        &route,
        existing_session,
        response.status_code(),
    )
    .await;
    with_identity(response, &route)
}

async fn dispatch_branch(
    env: &RouteContext<'_>,
    owner: &str,
    route: &ResponseRoute,
    source: &agents_core::SessionRecord,
    request: Value,
) -> Result<Response> {
    let Some(parent) = request["previous_response_id"].as_str() else {
        return invalid("A branch requires a previous response");
    };
    let mut exported = routes::forward(
        env,
        source,
        Method::Post,
        "/branches/export",
        Some(json!({
            "owner_id": owner, "parent_response_id": parent, "child_response_id": route.id, "request": request,
        })),
    )
    .await?;
    if exported.status_code() >= 300 {
        let status = exported.status_code();
        let body: Value = exported.json().await?;
        if body.pointer("/error/code").and_then(Value::as_str) == Some("response_already_admitted")
        {
            // Canonical positive evidence of this exact run permits observation,
            // not a retry after ambiguous transport failure.
            let response = routes::forward(
                env,
                source,
                Method::Post,
                "/responses",
                Some(json!({
                    "response_id": route.id, "request": request,
                })),
            )
            .await?;
            return with_identity(response, route);
        }
        return with_identity(Response::from_json(&body)?.with_status(status), route);
    }
    let seed: crate::session::branch::BranchExport = exported.json().await?;
    let child_id = crate::session::branch::child_session_id(owner, &route.id);
    let retained_pin = (seed.agent_id == route.agent_id).then(|| seed.pinned.clone());
    let child =
        match routes::response_branch_session(env, owner, &route.agent_id, &child_id, retained_pin)
            .await?
        {
            Ok(child) => child,
            Err(response) => return with_identity(response, route),
        };
    let imported = routes::forward(
        env,
        &child,
        Method::Post,
        "/branches/import",
        Some(serde_json::to_value(seed)?),
    )
    .await?;
    if imported.status_code() >= 300 {
        return with_identity(imported, route);
    }
    let Some(moved) =
        response_index::move_to_child(env, owner, &route.id, &route.session_id, &child_id).await?
    else {
        return not_found();
    };
    if moved.session_id != child_id {
        return util::json_error(
            409,
            "response_route_conflict",
            "Response was bound to another execution session",
        );
    }
    let response = routes::forward(
        env,
        &child,
        Method::Post,
        "/responses",
        Some(json!({
            "response_id": route.id, "request": request,
        })),
    )
    .await?;
    with_identity(response, &moved)
}

async fn retire_rejected_session(
    env: &RouteContext<'_>,
    session: &agents_core::SessionRecord,
    route: &ResponseRoute,
    existing_session: bool,
    status: u16,
) {
    if existing_session || !(400..500).contains(&status) {
        return;
    }
    // A response status alone cannot prove the run was never admitted. The DO
    // checks canonical state and in-flight admission fences before retirement.
    let retired = routes::forward(
        env,
        session,
        Method::Post,
        "/retire-empty",
        Some(json!({"response_id":route.id})),
    )
    .await;
    if !retired.is_ok_and(|response| response.status_code() < 300 || response.status_code() == 409)
    {
        util::emit(
            "session.empty_retirement_deferred",
            json!({"session_id":session.id,"agent_id":session.agent_id}),
        );
    }
}

async fn indexed(
    req: Request,
    env: &RouteContext<'_>,
    owner: &str,
    id: &str,
    suffix: &str,
) -> Result<Response> {
    let Some(route) = response_index::get(env, owner, id).await? else {
        return not_found();
    };
    let Some(session) = Store::new(env)
        .get_session(owner, &route.session_id)
        .await?
    else {
        return not_found();
    };
    let response = routes::forward(
        env,
        &session,
        req.method(),
        &format!("/responses/{id}{suffix}"),
        None,
    )
    .await;
    with_identity(
        match response {
            Ok(response) => response,
            Err(error) => routes::error_response(&error)?,
        },
        &route,
    )
}

fn with_identity(response: Response, route: &ResponseRoute) -> Result<Response> {
    let headers = response.headers().clone();
    let mut response = response.with_headers(headers);
    response
        .headers_mut()
        .set("X-Agents-Session", &route.session_id)?;
    response.headers_mut().set("X-Agents-Response", &route.id)?;
    Ok(response)
}

fn retrieve_query(url: &url::Url, allow_query: bool) -> std::result::Result<String, &'static str> {
    let mut stream = false;
    let mut after = None;
    let mut seen = std::collections::BTreeSet::new();
    for (key, value) in url.query_pairs() {
        if !allow_query || !seen.insert(key.to_string()) {
            return Err("Unsupported or repeated Responses query parameter");
        }
        match key.as_ref() {
            "stream" if value == "true" => stream = true,
            "stream" if value == "false" => {}
            "starting_after" => after = Some(sequence(&value)?),
            _ => return Err("Unsupported Responses query parameter"),
        }
    }
    match (stream, after) {
        (true, Some(after)) => Ok(format!("/events?after={after}")),
        (true, None) => Ok("/events".into()),
        (false, Some(_)) => Err("starting_after requires stream=true"),
        (false, None) => Ok(String::new()),
    }
}

fn input_query(url: &url::Url) -> std::result::Result<String, &'static str> {
    let mut seen = std::collections::BTreeSet::new();
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in url.query_pairs() {
        if !seen.insert(key.to_string()) {
            return Err("Repeated input-items query parameter");
        }
        let valid = match key.as_ref() {
            "after" | "before" => valid_item_id(&value),
            "order" => matches!(value.as_ref(), "asc" | "desc"),
            "limit" => value
                .parse::<u32>()
                .is_ok_and(|limit| (1..=100).contains(&limit)),
            _ => false,
        };
        if !valid {
            return Err("Invalid input-items pagination parameter");
        }
        query.append_pair(&key, &value);
    }
    if seen.contains("after") && seen.contains("before") {
        return Err("Use one input-items cursor at a time");
    }
    let query = query.finish();
    Ok(if query.is_empty() {
        query
    } else {
        format!("?{query}")
    })
}

fn event_query(url: &url::Url) -> std::result::Result<String, &'static str> {
    let pairs: Vec<_> = url.query_pairs().collect();
    match pairs.as_slice() {
        [] => Ok(String::new()),
        [(key, value)] if key == "after" => Ok(format!("?after={}", sequence(value)?)),
        _ => Err("Only the after sequence cursor is supported for response events"),
    }
}

fn sequence(value: &str) -> std::result::Result<u64, &'static str> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("The event cursor must be a nonnegative integer");
    }
    value.parse().map_err(|_| "The event cursor is too large")
}

fn valid_id(id: &str) -> bool {
    id.starts_with("resp_") && valid_item_id(id)
}

fn valid_item_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn idempotency_key(value: Option<&str>) -> std::result::Result<Option<String>, &'static str> {
    value
        .map(|value| {
            if value.is_empty()
                || value.len() > 256
                || !value.bytes().all(|b| (33..=126).contains(&b))
            {
                return Err("Idempotency-Key must contain 1 to 256 visible ASCII characters");
            }
            Ok(format!("{:x}", Sha256::digest(value.as_bytes())))
        })
        .transpose()
}

fn request_hash(value: &Value, session: Option<&str>) -> String {
    let mut hash = Sha256::new();
    hash.update(value.to_string().as_bytes());
    hash.update(b"\0");
    hash.update(session.unwrap_or_default().as_bytes());
    format!("{:x}", hash.finalize())
}

fn invalid(message: &str) -> Result<Response> {
    util::json_error(400, "invalid_request_error", message)
}

fn not_found() -> Result<Response> {
    util::json_error(404, "not_found", "Response not found")
}

fn method_not_allowed() -> Result<Response> {
    util::json_error(405, "method_not_allowed", "Method not allowed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(query: &str) -> url::Url {
        url::Url::parse(&format!(
            "https://agents.invalid/v1/responses/resp_test{query}"
        ))
        .unwrap()
    }

    #[test]
    fn sdk_stream_retrieval_uses_durable_sequence_cursor() {
        assert_eq!(
            retrieve_query(&url("?stream=true&starting_after=12"), true).unwrap(),
            "/events?after=12"
        );
        assert_eq!(
            retrieve_query(&url("?stream=true"), true).unwrap(),
            "/events"
        );
        assert_eq!(retrieve_query(&url("?stream=false"), true).unwrap(), "");
        for query in [
            "?starting_after=1",
            "?stream=yes",
            "?stream=true&starting_after=-1",
            "?stream=true&stream=false",
            "?stream=true&starting_after=18446744073709551616",
        ] {
            assert!(retrieve_query(&url(query), true).is_err(), "{query}");
        }
        assert!(retrieve_query(&url("?stream=true"), false).is_err());
    }

    #[test]
    fn input_item_pagination_is_bounded_and_unambiguous() {
        assert_eq!(
            input_query(&url("?limit=100&order=asc&after=msg_1")).unwrap(),
            "?limit=100&order=asc&after=msg_1"
        );
        for query in [
            "?limit=0",
            "?limit=101",
            "?limit=-1",
            "?order=other",
            "?after=msg_1&before=msg_2",
            "?after=a/b",
            "?after=one&after=two",
        ] {
            assert!(input_query(&url(query)).is_err(), "{query}");
        }
    }

    #[test]
    fn retry_identity_is_scoped_to_body_and_explicit_session() {
        let first = json!({"model":"agent","input":"hello","metadata":{"a":"1","b":"2"}});
        let reordered = json!({"metadata":{"b":"2","a":"1"},"input":"hello","model":"agent"});
        assert_eq!(request_hash(&first, None), request_hash(&reordered, None));
        assert_ne!(
            request_hash(&first, None),
            request_hash(&first, Some("session"))
        );
        assert_ne!(
            request_hash(&first, None),
            request_hash(&json!({"model":"agent","input":"different"}), None)
        );
        assert_eq!(
            idempotency_key(Some("same-key")).unwrap(),
            idempotency_key(Some("same-key")).unwrap()
        );
        for key in ["", "has space", "has\nnewline", "秘密"] {
            assert!(idempotency_key(Some(key)).is_err());
        }
    }

    #[test]
    fn identifiers_and_replay_cursors_cannot_inject_internal_paths() {
        for id in ["resp_a/b", "resp_a?stream=true", "resp_../x", "other"] {
            assert!(!valid_id(id));
        }
        assert!(valid_id("resp_012abc"));
        assert_eq!(event_query(&url("?after=5")).unwrap(), "?after=5");
        assert!(event_query(&url("?after=1&after=2")).is_err());
    }
}
