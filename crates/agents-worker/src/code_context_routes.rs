//! Public owned-container management never revives the creator's model harness.
use worker::*;

use crate::{
    code_contexts::{self as catalog, ContextState},
    conversation_meter::ConversationOperation as ResourceOperation,
    resources::ResourceMeter,
    routes::RouteContext,
    util,
};

pub(crate) async fn handle(
    request: Request,
    env: &RouteContext<'_>,
    owner: &str,
    cid: &str,
) -> Result<Response> {
    if !catalog::valid_cid(cid) {
        return missing();
    }
    if !matches!(request.method(), Method::Get | Method::Delete) {
        return util::json_error(
            405,
            "method_not_allowed",
            "Use GET or DELETE for a code container",
        );
    }
    let operation = ResourceOperation::account(env, owner).await?;
    let meter = ResourceMeter::account(env, owner, operation.id())?;
    let result = handle_inner(request, env, owner, cid, &operation, &meter).await;
    operation.finish_result(env, owner, result, false).await
}

async fn handle_inner(
    request: Request,
    env: &RouteContext<'_>,
    owner: &str,
    cid: &str,
    operation: &ResourceOperation,
    meter: &ResourceMeter<'_>,
) -> Result<Response> {
    let Some(allocation) = catalog::find_any(env, owner, cid, meter).await? else {
        return missing();
    };
    if request.method() == Method::Get && allocation.state == ContextState::Deleted {
        return missing();
    }
    if allocation.allocation_released_at_ms.is_some() {
        if request.method() == Method::Delete {
            catalog::end(
                env,
                owner,
                &allocation.allocation_id,
                true,
                util::now_ms(),
                meter,
            )
            .await?;
            return Ok(Response::empty()?.with_status(204));
        }
        return Response::from_json(&catalog::public(&allocation, false));
    }
    if request.method() == Method::Get && allocation.state != ContextState::Ready {
        return Response::from_json(&catalog::public(&allocation, false));
    }
    let action = if request.method() == Method::Delete {
        "delete"
    } else {
        "inspect"
    };
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    headers.set("X-Agents-Code-Owner", owner)?;
    env.trace().child().inject(&headers)?;
    operation
        .actor_headers(env, owner, "code-resource", &headers)
        .await?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(
            serde_json::to_string(
                &serde_json::json!({"allocation_id":allocation.allocation_id,"container_id":cid}),
            )?
            .into(),
        ));
    let namespace =
        env.durable_object(crate::runtime_profiles::binding(allocation.source_profile))?;
    namespace
        .id_from_name(&format!("{owner}:{}", allocation.source_session_id))?
        .get_stub()?
        .fetch_with_request(Request::new_with_init(
            &format!("https://code.internal/code-resources/{action}"),
            &init,
        )?)
        .await
}

fn missing() -> Result<Response> {
    util::json_error(404, "not_found", "Code container not found")
}
