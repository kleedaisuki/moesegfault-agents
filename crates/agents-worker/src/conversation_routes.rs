//! Authenticated public Conversations boundary and internal Response integration.
use crate::{
    conversation_meter::ConversationOperation,
    routes::{self, RouteContext},
    util,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::*;

pub(crate) use crate::conversation_quota::escrow::Request as ConversationCapacityRequest;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ConversationSnapshot {
    pub id: String,
    pub revision: u64,
    pub items: Vec<Value>,
    #[serde(default)]
    pub predecessor_response_id: Option<String>,
    #[serde(default)]
    pub predecessor_session_id: Option<String>,
    #[serde(default)]
    pub predecessor_agent_id: Option<String>,
    #[serde(default)]
    pub predecessor_pin_hash: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub item_provenance: std::collections::BTreeMap<String, ConversationItemProvenance>,
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
    #[serde(default)]
    pub item_provenance: std::collections::BTreeMap<String, ConversationItemProvenance>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ConversationItemProvenance {
    pub source_session_id: Option<String>,
    pub response_id: Option<String>,
    pub provider_binding: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct ConversationExecutionContext {
    pub session_id: String,
    pub agent_id: String,
    pub pin_hash: String,
    pub accepted_revision: u64,
    pub defer_release: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_provenance: Option<ConversationItemProvenance>,
    pub item_provenance: std::collections::BTreeMap<String, ConversationItemProvenance>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub image_assets: std::collections::BTreeMap<String, crate::workspace_blobs::OwnedBlobRef>,
}

#[derive(Deserialize)]
struct Route {
    id: String,
    request_hash: String,
}

pub(crate) async fn handle(req: Request, env: &RouteContext<'_>, owner: &str) -> Result<Response> {
    let operation = ConversationOperation::account(env, owner)
        .await?
        .with_traceparent(Some(&env.trace().traceparent()));
    let result = handle_inner(req, env, owner, &operation).await;
    operation.finish_result(env, owner, result, false).await
}

async fn handle_inner(
    mut req: Request,
    env: &RouteContext<'_>,
    owner: &str,
    operation: &ConversationOperation,
) -> Result<Response> {
    let dispatch = Dispatch {
        env,
        owner,
        operation,
    };
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
        operation.query(env, owner, "create-index", env.d1("DB")?.prepare("INSERT INTO conversation_routes(id,account_id,request_hash,created_at) VALUES(?,?,?,?) ON CONFLICT(id) DO NOTHING")
            .bind(&[id.as_str().into(),owner.into(),request_hash.as_str().into(),wasm_bindgen::JsValue::from_f64(util::now_ms() as f64)])?).await?;
        let Some(route) = dispatch.lookup(&id).await? else {
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
        return dispatch
            .forward(&route.id, Method::Post, "/init", Some(init))
            .await;
    }
    let (id, suffix) = match segments.as_slice() {
        ["v1", "conversations", id] if valid_id(id) => (*id, "/metadata".to_owned()),
        ["v1", "conversations", id, "purge"] if valid_id(id) => (*id, "/purge".to_owned()),
        ["v1", "conversations", id, "items"] if valid_id(id) => (*id, "/items".to_owned()),
        ["v1", "conversations", id, "items", item] if valid_id(id) && valid_id(item) => {
            (*id, format!("/items/{item}"))
        }
        _ => return not_found(),
    };
    let allowed = if suffix == "/purge" {
        method == Method::Post
    } else if suffix == "/metadata" {
        matches!(method, Method::Get | Method::Post | Method::Delete)
    } else if suffix == "/items" {
        matches!(method, Method::Get | Method::Post)
    } else {
        matches!(method, Method::Get | Method::Delete)
    };
    if !allowed {
        return method_not_allowed();
    }
    if dispatch.lookup(id).await?.is_none() {
        return not_found();
    }
    let suffix = match query(&req.url()?, &suffix, method.clone()) {
        Ok(query) => format!("{suffix}{query}"),
        Err(message) => return invalid(message),
    };
    if suffix == "/purge" {
        return dispatch.forward(id, method, &suffix, None).await;
    }
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
    dispatch.forward(id, method, &suffix, body).await
}

pub(crate) async fn snapshot(
    env: &Env,
    owner: &str,
    id: &str,
) -> Result<Option<ConversationSnapshot>> {
    let operation = ConversationOperation::account(env, owner).await?;
    let result = Dispatch {
        env,
        owner,
        operation: &operation,
    }
    .existing(id, Method::Get, "/snapshot", None)
    .await;
    let mut response = operation.finish_result(env, owner, result, false).await?;
    if response.status_code() == 404 {
        return Ok(None);
    }
    if response.status_code() >= 300 {
        return Err(Error::RustError("conversation_snapshot_failed".into()));
    }
    response.json().await.map(Some)
}

pub(crate) async fn admit(env: &Env, owner: &str, id: &str, response_id: &str) -> Result<Response> {
    admit_traced(env, owner, id, response_id, None).await
}

pub(crate) async fn admit_traced(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    traceparent: Option<&str>,
) -> Result<Response> {
    // Admission is one edge I/O attempt, not the accepted execution lifetime.
    // Activation creates/reuses the persistent cleanup hold before model work.
    let operation = ConversationOperation::account(env, owner)
        .await?
        .with_traceparent(traceparent);
    let result = Dispatch {
        env,
        owner,
        operation: &operation,
    }
    .existing(
        id,
        Method::Post,
        "/responses/admit",
        Some(json!({"response_id":response_id})),
    )
    .await;
    operation.finish_result(env, owner, result, false).await
}

pub(crate) async fn reserve_capacity(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    session_id: &str,
    accepted_revision: u64,
    capacity: &ConversationCapacityRequest,
    traceparent: Option<&str>,
) -> Result<Response> {
    let operation = ConversationOperation::response_actor(env, owner, response_id)
        .await?
        .with_traceparent(traceparent);
    let result=Dispatch {env,owner,operation:&operation}.existing(id,Method::Post,"/responses/reserve",
        Some(json!({"response_id":response_id,"session_id":session_id,"accepted_revision":accepted_revision,"capacity":capacity}))).await;
    let rejected = result.as_ref().is_ok_and(|response| response.status_code() == 409
        && matches!(response.headers().get("X-Agents-Conversation-Writer-Released"), Ok(Some(value)) if value == "true"));
    operation.finish_result(env, owner, result, rejected).await
}

pub(crate) async fn activate_generation(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    session_id: &str,
    accepted_revision: u64,
    generation: &str,
) -> Result<Response> {
    let operation = ConversationOperation::response_actor(env, owner, response_id).await?;
    let result=Dispatch {env,owner,operation:&operation}.existing(id,Method::Post,"/responses/activate",
        Some(json!({"response_id":response_id,"session_id":session_id,"accepted_revision":accepted_revision,"capacity_generation":generation}))).await;
    operation.finish_result(env, owner, result, false).await
}

pub(crate) async fn reserve_additional_input(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    session_id: &str,
    generation: &str,
    submission_id: &str,
    capacity: &ConversationCapacityRequest,
) -> Result<Response> {
    let operation = ConversationOperation::response_actor(env, owner, response_id).await?;
    let result=Dispatch {env,owner,operation:&operation}.existing(id,Method::Post,"/responses/input",
        Some(json!({"response_id":response_id,"session_id":session_id,"capacity_generation":generation,"submission_id":submission_id,"capacity":capacity}))).await;
    operation.finish_result(env, owner, result, false).await
}

pub(crate) async fn release_noappend_actor(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    generation: &str,
) -> Result<Response> {
    let operation = ConversationOperation::response_actor(env, owner, response_id).await?;
    if operation.is_finished() {
        return Response::from_json(&json!({"id":id,"released":true}));
    }
    let result = Dispatch {
        env,
        owner,
        operation: &operation,
    }
    .existing(
        id,
        Method::Post,
        "/responses/noappend",
        Some(json!({"response_id":response_id,"capacity_generation":generation})),
    )
    .await;
    let released = result
        .as_ref()
        .is_ok_and(|response| response.status_code() < 300);
    operation.finish_result(env, owner, result, released).await
}

pub(crate) async fn activate(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    session_id: &str,
    accepted_revision: u64,
) -> Result<Response> {
    let operation = ConversationOperation::response_actor(env, owner, response_id).await?;
    let result = Dispatch { env, owner, operation: &operation }.existing(id, Method::Post, "/responses/activate", Some(json!({"response_id":response_id,"session_id":session_id,"accepted_revision":accepted_revision}))).await;
    operation.finish_result(env, owner, result, false).await
}

pub(crate) async fn release(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
) -> Result<Response> {
    let operation = ConversationOperation::response(env, owner, response_id).await?;
    release_with_operation(env, owner, id, response_id, &operation).await
}

pub(crate) async fn release_actor(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
) -> Result<Response> {
    let operation = ConversationOperation::response_actor(env, owner, response_id).await?;
    release_with_operation(env, owner, id, response_id, &operation).await
}

async fn release_with_operation(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    operation: &ConversationOperation,
) -> Result<Response> {
    if operation.is_finished() {
        return Response::from_json(&json!({"id":id,"released":true}));
    }
    let result = Dispatch {
        env,
        owner,
        operation,
    }
    .existing(
        id,
        Method::Post,
        "/responses/release",
        Some(json!({"response_id":response_id})),
    )
    .await;
    let release = result
        .as_ref()
        .is_ok_and(|response| response.status_code() < 300);
    operation.finish_result(env, owner, result, release).await
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

pub(crate) async fn commit_response_context(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    items: &[Value],
    context: Option<&ConversationExecutionContext>,
) -> Result<Response> {
    let operation = ConversationOperation::account(env, owner).await?;
    let result =
        commit_response_with_meter(env, owner, id, response_id, items, context, &operation).await;
    operation.finish_result(env, owner, result, false).await
}

pub(crate) async fn commit_response_with_meter(
    env: &Env,
    owner: &str,
    id: &str,
    response_id: &str,
    items: &[Value],
    context: Option<&ConversationExecutionContext>,
    operation: &ConversationOperation,
) -> Result<Response> {
    let mut body = json!({"response_id":response_id,"items":items});
    if let Some(context) = context {
        if let Some(fields) = serde_json::to_value(context)?.as_object() {
            body.as_object_mut()
                .expect("response commit object")
                .extend(fields.clone());
        }
    }
    Dispatch {
        env,
        owner,
        operation,
    }
    .existing(id, Method::Post, "/responses/commit", Some(body))
    .await
}

struct Dispatch<'a> {
    env: &'a Env,
    owner: &'a str,
    operation: &'a ConversationOperation,
}

impl Dispatch<'_> {
    async fn lookup(&self, id: &str) -> Result<Option<Route>> {
        let result = self.operation.query(self.env, self.owner, "lookup", self.env.d1("DB")?
            .prepare("SELECT id,request_hash FROM conversation_routes WHERE account_id=? AND id=?")
            .bind(&[self.owner.into(), id.into()])?).await?;
        Ok(result.results::<Route>()?.into_iter().next())
    }

    async fn existing(
        &self,
        id: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Response> {
        if !valid_id(id) || self.lookup(id).await?.is_none() {
            return not_found();
        }
        self.forward(id, method, path, body).await
    }

    async fn forward(
        &self,
        id: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Response> {
        let stub = self
            .env
            .durable_object("CONVERSATION_STATE")?
            .id_from_name(&format!("{}:{id}", self.owner))?
            .get_stub()?;
        let headers = Headers::new();
        headers.set("X-Agents-Owner", self.owner)?;
        headers.set("Content-Type", "application/json")?;
        self.operation
            .actor_headers(self.env, self.owner, "actor", &headers)
            .await?;
        let request_id = util::id("conv_rpc");
        headers.set("X-Agents-Conversation-ID", id)?;
        headers.set("X-Agents-Conversation-Request", &request_id)?;
        let grant: Value = serde_json::from_str(
            &headers
                .get(crate::conversation_meter::HEADER)?
                .unwrap_or_default(),
        )?;
        let response_id = body
            .as_ref()
            .and_then(|body| body["response_id"].as_str())
            .map(str::to_owned);
        self.operation.emit(
            "rpc.started",
            json!({"conversation_id":id,"request_id":request_id,
            "request_path":path,"grant_event":grant["event"],"response_id":response_id}),
        );
        let init = RequestInit {
            method,
            headers,
            body: body.map(|body| body.to_string().into()),
            ..Default::default()
        };
        let started = util::now_ms();
        let result = stub
            .fetch_with_request(Request::new_with_init(
                &format!("https://conversation.internal{path}"),
                &init,
            )?)
            .await;
        let mut fields = json!({"conversation_id":id,"request_id":request_id,"request_path":path,
            "grant_event":grant["event"],"response_id":response_id,"duration_ms":util::now_ms().saturating_sub(started),
            "status":result.as_ref().ok().map(|response| response.status_code())});
        if let Err(error) = &result {
            fields.as_object_mut().unwrap().extend(
                crate::conversation_meter::error_metadata(error, started)
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
        self.operation.emit(
            if result.is_ok() {
                "rpc.completed"
            } else {
                "rpc.failed"
            },
            fields,
        );
        result.map_err(|_| Error::RustError("conversation_actor_transport_failed".into()))
    }
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
