//! Authenticated public Conversations boundary and internal Response integration.
use crate::{
    routes::{self, RouteContext},
    util,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ConversationSnapshot {
    pub id: String,
    pub revision: u64,
    pub items: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ConversationAdmission {
    pub id: String,
    pub revision: u64,
    pub items: Vec<Value>,
    pub predecessor_response_id: Option<String>,
    pub predecessor_session_id: Option<String>,
    pub predecessor_agent_id: Option<String>,
    pub predecessor_pin_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConversationExecutionContext {
    pub session_id: String,
    pub agent_id: String,
    pub pin_hash: String,
    pub accepted_revision: u64,
}

#[derive(Deserialize)]
struct Route {
    id: String,
    request_hash: String,
}

pub(crate) async fn handle(
    mut req: Request,
    env: &RouteContext<'_>,
    owner: &str,
) -> Result<Response> {
    let path = req.path();
    let segments = path.trim_matches('/').split('/').collect::<Vec<_>>();
    let method = req.method();
    if segments.as_slice() == ["v1", "conversations"] {
        if method != Method::Post {
            return method_not_allowed();
        }
        if req.url()?.query().is_some() {
            return invalid("Conversation create does not accept query parameters");
        }
        let body: Value = match routes::body(&mut req).await {
            Ok(body) => body,
            Err(response) => return Ok(response),
        };
        if !body.is_object()
            || body.as_object().is_some_and(|body| {
                body.keys()
                    .any(|key| !matches!(key.as_str(), "metadata" | "items"))
            })
        {
            return invalid("Conversation create accepts metadata and items only");
        }
        let request_hash = hash(&serde_json::to_string(&body)?);
        let key = req.headers().get("Idempotency-Key")?;
        if key.as_ref().is_some_and(|key| {
            key.is_empty() || key.len() > 255 || key.chars().any(char::is_control)
        }) {
            return invalid("Invalid Idempotency-Key");
        }
        let id = key
            .as_ref()
            .map(|key| format!("conv_{}", hash(&format!("{owner}\0{key}"))))
            .unwrap_or_else(|| util::id("conv"));
        env.d1("DB")?.prepare("INSERT INTO conversation_routes(id,account_id,request_hash,created_at) VALUES(?,?,?,?) ON CONFLICT(id) DO NOTHING")
            .bind(&[id.as_str().into(),owner.into(),request_hash.as_str().into(),wasm_bindgen::JsValue::from_f64(util::now_ms() as f64)])?
            .run().await?;
        let Some(route) = lookup(env, owner, &id).await? else {
            return not_found();
        };
        if route.request_hash != request_hash {
            return util::json_error(
                409,
                "idempotency_conflict",
                "Idempotency key was used with different parameters",
            );
        }
        let mut init = body;
        init["id"] = json!(route.id);
        return forward(env, owner, &route.id, Method::Post, "/init", Some(init)).await;
    }
    let (id, suffix) = match segments.as_slice() {
        ["v1", "conversations", id] if valid_id(id) => (*id, "/metadata".to_owned()),
        ["v1", "conversations", id, "items"] if valid_id(id) => (*id, "/items".to_owned()),
        ["v1", "conversations", id, "items", item] if valid_id(id) && valid_id(item) => {
            (*id, format!("/items/{item}"))
        }
        _ => return not_found(),
    };
    let allowed = if suffix == "/metadata" {
        matches!(method, Method::Get | Method::Post | Method::Delete)
    } else if suffix == "/items" {
        matches!(method, Method::Get | Method::Post)
    } else {
        matches!(method, Method::Get | Method::Delete)
    };
    if !allowed {
        return method_not_allowed();
    }
    if lookup(env, owner, id).await?.is_none() {
        return not_found();
    }
    let suffix = match query(&req.url()?, &suffix, method.clone()) {
        Ok(query) => format!("{suffix}{query}"),
        Err(message) => return invalid(message),
    };
    let body = if method == Method::Post {
        let body: Value = match routes::body(&mut req).await {
            Ok(body) => body,
            Err(response) => return Ok(response),
        };
        let key = if suffix.starts_with("/metadata") {
            "metadata"
        } else {
            "items"
        };
        if !body.is_object()
            || body
                .as_object()
                .is_some_and(|map| map.len() != 1 || !map.contains_key(key))
        {
            return invalid("Unexpected conversation operation fields");
        }
        Some(body)
    } else {
        None
    };
    forward(env, owner, id, method, &suffix, body).await
}

pub(crate) async fn snapshot(
    env: &Env,
    owner: &str,
    id: &str,
) -> Result<Option<ConversationSnapshot>> {
    if !valid_id(id) || lookup(env, owner, id).await?.is_none() {
        return Ok(None);
    }
    let mut response = forward(env, owner, id, Method::Get, "/snapshot", None).await?;
    if response.status_code() == 404 {
        return Ok(None);
    }
    if response.status_code() >= 300 {
        return Err(Error::RustError("conversation_snapshot_failed".into()));
    }
    response.json().await.map(Some)
}

pub(crate) async fn commit_response(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    items: &[Value],
) -> Result<Response> {
    commit_response_context(env, owner, id, response_id, items, None).await
}

pub(crate) async fn admit(env: &Env, owner: &str, id: &str, response_id: &str) -> Result<Response> {
    if !valid_id(id) || lookup(env, owner, id).await?.is_none() {
        return not_found();
    }
    forward(
        env,
        owner,
        id,
        Method::Post,
        "/responses/admit",
        Some(json!({"response_id":response_id})),
    )
    .await
}

pub(crate) async fn release(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
) -> Result<Response> {
    if !valid_id(id) || lookup(env, owner, id).await?.is_none() {
        return not_found();
    }
    forward(
        env,
        owner,
        id,
        Method::Post,
        "/responses/release",
        Some(json!({"response_id":response_id})),
    )
    .await
}

pub(crate) async fn commit_response_context(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    items: &[Value],
    context: Option<&ConversationExecutionContext>,
) -> Result<Response> {
    if !valid_id(id) || lookup(env, owner, id).await?.is_none() {
        return not_found();
    }
    let mut body = json!({"response_id":response_id,"items":items});
    if let Some(context) = context {
        if let Some(fields) = serde_json::to_value(context)?.as_object() {
            body.as_object_mut()
                .expect("response commit object")
                .extend(fields.clone());
        }
    }
    forward(
        env,
        owner,
        id,
        Method::Post,
        "/responses/commit",
        Some(body),
    )
    .await
}

async fn lookup(env: &Env, owner: &str, id: &str) -> Result<Option<Route>> {
    env.d1("DB")?
        .prepare("SELECT id,request_hash FROM conversation_routes WHERE account_id=? AND id=?")
        .bind(&[owner.into(), id.into()])?
        .first(None)
        .await
}

async fn forward(
    env: &Env,
    owner: &str,
    id: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Response> {
    let stub = env
        .durable_object("CONVERSATION_STATE")?
        .id_from_name(&format!("{owner}:{id}"))?
        .get_stub()?;
    let headers = Headers::new();
    headers.set("X-Agents-Owner", owner)?;
    headers.set("Content-Type", "application/json")?;
    let init = RequestInit {
        method,
        headers,
        body: body.map(|body| body.to_string().into()),
        ..Default::default()
    };
    stub.fetch_with_request(Request::new_with_init(
        &format!("https://conversation.internal{path}"),
        &init,
    )?)
    .await
}

fn query(url: &url::Url, path: &str, method: Method) -> std::result::Result<String, &'static str> {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    let mut seen = std::collections::BTreeSet::new();
    for (key, value) in url.query_pairs() {
        let include = matches!(key.as_ref(), "include" | "include[]");
        if include && path.starts_with("/items") && method != Method::Delete {
            if !matches!(
                value.as_ref(),
                "web_search_call.action.sources"
                    | "code_interpreter_call.outputs"
                    | "computer_call_output.output.image_url"
                    | "file_search_call.results"
                    | "message.input_image.image_url"
                    | "message.output_text.logprobs"
                    | "reasoning.encrypted_content"
            ) {
                return Err("Invalid include value");
            }
            query.append_pair("include[]", &value);
            continue;
        }
        if path != "/items"
            || method != Method::Get
            || !matches!(key.as_ref(), "after" | "limit" | "order")
            || !seen.insert(key.to_string())
        {
            return Err("Unsupported or repeated conversation query parameter");
        }
        query.append_pair(&key, &value);
    }
    let query = query.finish();
    Ok(if query.is_empty() {
        query
    } else {
        format!("?{query}")
    })
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}
fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn invalid(message: &str) -> Result<Response> {
    util::json_error(400, "invalid_request", message)
}
fn not_found() -> Result<Response> {
    util::json_error(404, "not_found", "Conversation not found")
}
fn method_not_allowed() -> Result<Response> {
    util::json_error(405, "method_not_allowed", "Method not allowed")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queries_allow_repeated_includes_but_not_repeated_cursors() {
        let url = url::Url::parse("https://example.test/?include[]=reasoning.encrypted_content&include[]=file_search_call.results").unwrap();
        assert!(query(&url, "/items", Method::Get).is_ok());
        let url = url::Url::parse("https://example.test/?after=a&after=b").unwrap();
        assert!(query(&url, "/items", Method::Get).is_err());
    }
    #[test]
    fn metadata_does_not_accept_item_queries() {
        let url = url::Url::parse("https://example.test/?limit=10").unwrap();
        assert!(query(&url, "/metadata", Method::Get).is_err());
    }
}
