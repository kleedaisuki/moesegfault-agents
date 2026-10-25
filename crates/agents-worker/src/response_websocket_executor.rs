//! Same canonical Responses admission and event observation over WebSocket lanes.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use eventsource_stream::Eventsource;
use futures::{FutureExt, Stream, StreamExt, future::LocalBoxFuture};
use serde_json::{Value, json};
use worker::{Env, Method, Response};

use crate::{
    response_execution::{ConnectionLease, ExecutionContext, OwnedParent},
    response_websocket::{Dispatch, Executor, RequestFailure, ServerEvent, ServerEvents, Steer},
    response_websocket_auth::Grant,
    routes::{self, RouteContext},
    telemetry::TraceContext,
};

const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;

pub(crate) async fn connect(
    request: worker::Request,
    env: &RouteContext<'_>,
    principal: &crate::auth::AuthContext,
) -> worker::Result<Response> {
    if request.url()?.query().is_some() {
        return crate::util::json_error(
            400,
            "invalid_request_error",
            "WebSocket upgrade does not accept query parameters",
        );
    }
    let upgrade = request.headers().get("Upgrade")?;
    let connection = request.headers().get("Connection")?;
    if !upgrade
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        || !connection.as_deref().is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
    {
        let mut response = crate::util::json_error(
            426,
            "upgrade_required",
            "Use a WebSocket upgrade for GET /v1/responses",
        )?;
        response.headers_mut().set("Upgrade", "websocket")?;
        return Ok(response);
    }
    let origin = request.headers().get("Origin")?;
    let app_origin = env.var("APP_ORIGIN")?.to_string();
    if let Err(error) = crate::response_websocket::authorize_browser_upgrade(
        principal.csrf_token.is_some(),
        origin.as_deref(),
        &app_origin,
    ) {
        return crate::util::json_error(
            error.status,
            error.code,
            "Browser WebSockets require the application origin",
        );
    }
    let grant = crate::response_websocket_auth::verified(&request, principal)?;
    let executor = Harness::new(
        std::ops::Deref::deref(env).clone(),
        env.trace().clone(),
        grant,
    );
    crate::response_websocket::upgrade(executor).await
}

pub(crate) struct Harness {
    env: Env,
    trace: TraceContext,
    grant: Grant,
    lease: ConnectionLease,
    sessions: Rc<RefCell<VecDeque<String>>>,
}

impl Harness {
    pub(crate) fn new(env: Env, trace: TraceContext, grant: Grant) -> Rc<Self> {
        Rc::new(Self {
            env,
            trace,
            grant,
            lease: ConnectionLease {
                connection_id: uuid::Uuid::new_v4().to_string(),
                expires_at_ms: crate::util::now_ms()
                    .saturating_add(crate::response_websocket::MAX_CONNECTION_MS),
            },
            sessions: Rc::new(RefCell::new(VecDeque::new())),
        })
    }

    async fn authorize(env: &Env, grant: &Grant) -> Result<(), RequestFailure> {
        if let Err(error) = grant.validate(env).await {
            let response = routes::error_response(&error).map_err(|_| detached())?;
            return Err(response_failure(response).await);
        }
        for binding in ["ACCOUNT_RATE_LIMITER", "MUTATION_RATE_LIMITER"] {
            match routes::rate_limited(env, binding, grant.owner().to_owned()).await {
                Ok(None) => {}
                Ok(Some(response)) => return Err(response_failure(response).await),
                Err(_) => return Err(detached()),
            }
        }
        Ok(())
    }
}

impl Executor for Harness {
    fn create(
        &self,
        dispatch: Dispatch,
    ) -> LocalBoxFuture<'static, Result<ServerEvents, RequestFailure>> {
        let env = self.env.clone();
        let trace = self.trace.child();
        let grant = self.grant.clone();
        let lease = self.lease.clone();
        let sessions = self.sessions.clone();
        async move {
            let _span = crate::telemetry::NativeSpan::start(
                "agents.responses.websocket.create",
                Some(&trace),
            );
            Self::authorize(&env, &grant).await?;
            let route = RouteContext::new(&env, &trace);
            let execution = ExecutionContext {
                connection_lease: Some(lease.clone()),
                parent_lease: dispatch.parent.map(|parent| OwnedParent {
                    response_id: parent.response_id,
                    token: parent.token,
                    connection_id: lease.connection_id.clone(),
                }),
                warmup: !dispatch.create.generate,
                ..Default::default()
            };
            let response = crate::response_routes::create_owned_with_context(
                &route,
                grant.owner(),
                dispatch.create.body,
                None,
                None,
                execution,
            )
            .await
            .map_err(|_| detached())?;
            if let Ok(Some(session)) = response.headers().get("X-Agents-Session") {
                remember(&sessions, session);
            }
            let events = response_events(response).await?;
            Ok(events
                .then(move |event| {
                    let env = env.clone();
                    let trace = trace.clone();
                    let grant = grant.clone();
                    let lease = lease.clone();
                    let sessions = sessions.clone();
                    async move {
                        let mut event = event?;
                        attach_parent(&env, &trace, &grant, &lease, &sessions, &mut event).await;
                        Ok(event)
                    }
                })
                .boxed_local())
        }
        .boxed_local()
    }

    fn steer(
        &self,
        request: Steer,
    ) -> LocalBoxFuture<'static, Result<Vec<ServerEvent>, RequestFailure>> {
        let env = self.env.clone();
        let trace = self.trace.child();
        let grant = self.grant.clone();
        async move {
            let _span = crate::telemetry::NativeSpan::start(
                "agents.responses.websocket.steer",
                Some(&trace),
            );
            Self::authorize(&env, &grant).await?;
            let route = RouteContext::new(&env, &trace);
            let indexed =
                crate::response_index::get(&env, grant.owner(), &request.previous_response_id)
                    .await
                    .map_err(|_| detached())?
                    .ok_or_else(|| failure(404, "response_not_found", "Response not found"))?;
            let session = crate::store::Store::new(&env)
                .get_session(grant.owner(), &indexed.session_id)
                .await
                .map_err(|_| detached())?
                .ok_or_else(|| failure(404, "response_not_found", "Response not found"))?;
            let mut response = routes::forward(
                &route,
                &session,
                Method::Post,
                &format!("/responses/{}/steer", request.previous_response_id),
                Some(json!({"input":request.input})),
            )
            .await
            .map_err(|_| detached())?;
            if !(200..300).contains(&response.status_code()) {
                return Err(response_failure(response).await);
            }
            let value = response.json::<Value>().await.map_err(|_| detached())?;
            steering_reply(value)
        }
        .boxed_local()
    }

    fn disconnected(&self) -> LocalBoxFuture<'static, ()> {
        let env = self.env.clone();
        let trace = self.trace.clone();
        let owner = self.grant.owner().to_owned();
        let connection = self.lease.connection_id.clone();
        let sessions: Vec<_> = self.sessions.borrow_mut().drain(..).collect();
        async move {
            let route = RouteContext::new(&env, &trace);
            for id in sessions {
                let Ok(Some(session)) = crate::store::Store::new(&env)
                    .get_session(&owner, &id)
                    .await
                else {
                    continue;
                };
                let _ = routes::forward(
                    &route,
                    &session,
                    Method::Delete,
                    &format!("/connection-leases/{connection}"),
                    Some(json!({"owner_id":owner})),
                )
                .await;
            }
        }
        .boxed_local()
    }

    fn retire_parent(
        &self,
        parent: crate::response_websocket::Parent,
    ) -> LocalBoxFuture<'static, ()> {
        let env = self.env.clone();
        let trace = self.trace.clone();
        let owner = self.grant.owner().to_owned();
        let connection = self.lease.connection_id.clone();
        async move {
            let Ok(Some(indexed)) =
                crate::response_index::get(&env, &owner, &parent.response_id).await
            else {
                return;
            };
            let Ok(Some(session)) = crate::store::Store::new(&env)
                .get_session(&owner, &indexed.session_id)
                .await
            else {
                return;
            };
            let route = RouteContext::new(&env, &trace);
            let _ = routes::forward(
                &route,
                &session,
                Method::Delete,
                &format!("/responses/{}/connection-parent", parent.response_id),
                Some(json!({"owner_id":owner,"connection_id":connection,"token":parent.token})),
            )
            .await;
        }
        .boxed_local()
    }
}

fn steering_reply(value: Value) -> Result<Vec<ServerEvent>, RequestFailure> {
    // Accepted acknowledgements travel on the original observer. Rejections are
    // sequenced by the actor but delivered once through the control reply.
    if value.get("queued") == Some(&Value::Bool(true)) {
        return Ok(Vec::new());
    }
    let events = value
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(detached)?;
    events
        .iter()
        .map(|event| {
            if event["type"] != "response.steer.failed"
                || event["sequence_number"].as_u64().is_none()
            {
                return Err(detached());
            }
            Ok(ServerEvent {
                value: event.clone(),
                parent: None,
            })
        })
        .collect()
}

fn remember(sessions: &Rc<RefCell<VecDeque<String>>>, session: String) {
    let mut remembered = sessions.borrow_mut();
    remembered.retain(|id| id != &session);
    remembered.push_back(session);
    // Older actors still expire grants by alarm if best-effort close misses them.
    if remembered.len() > 64 {
        remembered.pop_front();
    }
}

async fn attach_parent(
    env: &Env,
    trace: &TraceContext,
    grant: &Grant,
    lease: &ConnectionLease,
    sessions: &Rc<RefCell<VecDeque<String>>>,
    event: &mut ServerEvent,
) {
    if !matches!(
        event.value["type"].as_str(),
        Some("response.completed" | "response.incomplete" | "response.failed")
    ) {
        return;
    }
    let Some(id) = event.value.pointer("/response/id").and_then(Value::as_str) else {
        return;
    };
    let Ok(Some(indexed)) = crate::response_index::get(env, grant.owner(), id).await else {
        return;
    };
    let Ok(Some(session)) = crate::store::Store::new(env)
        .get_session(grant.owner(), &indexed.session_id)
        .await
    else {
        return;
    };
    remember(sessions, session.id.clone());
    let route = RouteContext::new(env, trace);
    let Ok(mut response) = routes::forward(&route, &session, Method::Post,
        &format!("/responses/{id}/connection-parent"),
        Some(json!({"owner_id":grant.owner(),"connection_id":lease.connection_id,"expires_at_ms":lease.expires_at_ms}))).await else { return; };
    if response.status_code() != 200 {
        return;
    }
    let Ok(value) = response.json::<Value>().await else {
        return;
    };
    if value["response_id"].as_str() != Some(id) {
        return;
    }
    let Some(token) = value["token"]
        .as_str()
        .filter(|token| !token.is_empty() && token.len() <= 4096)
    else {
        return;
    };
    event.parent = Some(crate::response_websocket::Parent {
        response_id: id.to_owned(),
        token: token.to_owned(),
    });
}

fn failure(status: u16, code: &str, message: &str) -> RequestFailure {
    RequestFailure {
        status,
        error: json!({"type":if status >= 500 {"server_error"} else {"invalid_request_error"},"code":code,"message":message}),
    }
}

fn detached() -> RequestFailure {
    failure(
        503,
        "response_stream_interrupted",
        "Observation detached; retrieve the original response, do not replay creation",
    )
}

async fn response_failure(mut response: Response) -> RequestFailure {
    let status = response.status_code();
    // Only the same trusted application protocol supplies this body; never proxy provider errors.
    let mut error = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|body| body.get("error").filter(|error| error.is_object()).cloned());
    if let Some(Value::Object(error)) = error.as_mut() {
        error.entry("type").or_insert_with(|| {
            json!(if status >= 500 {
                "server_error"
            } else {
                "invalid_request_error"
            })
        });
    }
    RequestFailure { status, error: error.unwrap_or_else(|| json!({"type":"server_error","code":"response_unavailable","message":"Response operation is unavailable"})) }
}

async fn response_events(mut response: Response) -> Result<ServerEvents, RequestFailure> {
    if !(200..300).contains(&response.status_code()) {
        return Err(response_failure(response).await);
    }
    let content_type = response
        .headers()
        .get("Content-Type")
        .map_err(|_| detached())?;
    if !content_type.as_deref().is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim() == "text/event-stream")
    }) {
        return Err(detached());
    }
    let source = response.stream().map_err(|_| detached())?;
    Ok(decode(source))
}

/// Bound a single frame, not an entire long-lived response or task duration.
#[derive(Default)]
struct FrameBudget {
    bytes: usize,
    line_bytes: usize,
}

impl FrameBudget {
    fn accept(&mut self, chunk: &[u8]) -> bool {
        for byte in chunk {
            self.bytes = self.bytes.saturating_add(1);
            if self.bytes > MAX_EVENT_BYTES {
                return false;
            }
            match byte {
                b'\n' => {
                    if self.line_bytes == 0 {
                        self.bytes = 0;
                    }
                    self.line_bytes = 0;
                }
                b'\r' => {}
                _ => self.line_bytes = self.line_bytes.saturating_add(1),
            }
        }
        true
    }
}

fn decode<S>(source: S) -> ServerEvents
where
    S: Stream<Item = worker::Result<Vec<u8>>> + Unpin + 'static,
{
    let mut budget = FrameBudget::default();
    source
        .map(move |chunk| {
            let chunk = chunk.map_err(|_| ())?;
            if !budget.accept(&chunk) {
                return Err(());
            }
            Ok(chunk)
        })
        .eventsource()
        .map(|event| {
            let event = event.map_err(|_| detached())?;
            let value: Value = serde_json::from_str(&event.data).map_err(|_| detached())?;
            if !value.is_object() || value.get("type").and_then(Value::as_str).is_none() {
                return Err(detached());
            }
            Ok(ServerEvent {
                value,
                parent: None,
            })
        })
        .boxed_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steering_replies_preserve_actor_sequence_without_duplicate_acceptance() {
        assert!(
            steering_reply(json!({"queued":true}))
                .ok()
                .unwrap()
                .is_empty()
        );
        let result = steering_reply(json!({"events":[{"type":"response.steer.failed","sequence_number":17,"error":{"code":"invalid_input"}}]})).ok().unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].value["sequence_number"], 17);
        assert!(result[0].parent.is_none());
        assert!(
            steering_reply(json!({"events":[{"type":"response.created","sequence_number":1}]}))
                .is_err()
        );
        assert!(steering_reply(json!({"events":[{"type":"response.steer.failed"}]})).is_err());
    }

    #[test]
    fn frame_budget_resets_at_blank_lines_not_chunks() {
        let mut budget = FrameBudget::default();
        assert!(budget.accept(b"data: {}\r\n"));
        assert_eq!(budget.bytes, 10);
        assert!(budget.accept(b"\r\n"));
        assert_eq!(budget.bytes, 0);
        budget.bytes = MAX_EVENT_BYTES;
        assert!(!budget.accept(b"x"));
    }

    #[test]
    fn native_sse_preserves_canonical_event_and_ignores_keepalive() {
        futures::executor::block_on(async {
            let source = futures::stream::iter(vec![Ok(
                b": ping\n\ndata: {\"type\":\"response.created\",\"sequence_number\":7}\n\n"
                    .to_vec(),
            )]);
            let mut events = decode(source);
            let event = events.next().await.unwrap().ok().unwrap();
            assert_eq!(event.value["sequence_number"], 7);
            assert!(event.parent.is_none());
            assert!(events.next().await.is_none());
        });
    }

    #[test]
    fn broken_observation_does_not_fabricate_terminal() {
        futures::executor::block_on(async {
            let source = futures::stream::iter(vec![Ok(b"data: invalid\n\n".to_vec())]);
            let mut events = decode(source);
            let error = events.next().await.unwrap().err().unwrap();
            assert_eq!(error.status, 503);
            assert_eq!(error.error["code"], "response_stream_interrupted");
        });
    }
}
