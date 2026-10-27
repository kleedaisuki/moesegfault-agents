//! Resolve a borrowed namespace before model admission without selecting a VM.
use futures::StreamExt;
use serde_json::json;
use worker::{Env, Error, Headers, Method, Request, RequestInit, Result};

use crate::{
    code_contexts::{self, OwnedContext},
    conversation_meter::ConversationOperation,
    resources::ResourceMeter,
    telemetry::TraceContext,
    util,
};

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

pub(crate) async fn resolve(env: &Env, owner: &str, cid: &str) -> Result<OwnedContext> {
    if !code_contexts::valid_cid(cid) {
        return Err(fail("code_container_not_found"));
    }
    let operation = ConversationOperation::account(env, owner).await?;
    let result = resolve_inner(env, owner, cid, &operation).await;
    let settled = operation.finish(env, owner).await;
    match result {
        Ok(context) => {
            settled?;
            Ok(context)
        }
        Err(error) => Err(error),
    }
}

async fn resolve_inner(
    env: &Env,
    owner: &str,
    cid: &str,
    operation: &ConversationOperation,
) -> Result<OwnedContext> {
    let meter = ResourceMeter::account(env, owner, operation.id())?;
    let allocation = code_contexts::find(env, owner, cid, &meter)
        .await?
        .filter(|allocation| {
            allocation.expires_at_ms > util::now_ms()
                && allocation.allocation_released_at_ms.is_none()
        })
        .ok_or_else(|| fail("code_container_not_found"))?;
    let context = allocation.owned()?;
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    headers.set("X-Agents-Code-Owner", owner)?;
    TraceContext::from_traceparent(None).inject(&headers)?;
    operation
        .actor_headers(env, owner, "code-resource", &headers)
        .await?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(
            json!({
                "allocation_id":allocation.allocation_id,"container_id":cid
            })
            .to_string()
            .into(),
        ));
    let mut response = env
        .durable_object(crate::runtime_profiles::binding(context.profile))?
        .id_from_name(&format!("{owner}:{}", context.source_session))?
        .get_stub()?
        .fetch_with_request(Request::new_with_init(
            "https://code.internal/code-resources/inspect",
            &init,
        )?)
        .await?;
    if response.status_code() != 200 {
        return Err(fail("code_container_not_found"));
    }
    let mut stream = response.stream()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > 4096_usize.saturating_sub(bytes.len()) {
            return Err(fail("code_resource_protocol_error"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| fail("code_resource_protocol_error"))?;
    if value["id"].as_str() != Some(cid) || value["status"] != "running" {
        return Err(fail("code_container_not_found"));
    }
    Ok(context)
}
