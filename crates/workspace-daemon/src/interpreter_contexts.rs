//! Explicit logical-context inspection/release never starts another namespace.
use super::*;

pub(crate) async fn current_context(State(state): State<Arc<App>>) -> Response {
    let slot = state.interpreter.context.lock().await;
    let Some(context) = slot.as_ref().filter(|context| !context.expired) else {
        return error(StatusCode::NOT_FOUND, "code container not found");
    };
    Json(serde_json::json!({"container_id":context.id})).into_response()
}

pub(crate) async fn inspect_context(
    State(state): State<Arc<App>>,
    Path(id): Path<String>,
) -> Response {
    if !context_id(&id) {
        return error(StatusCode::BAD_REQUEST, "invalid code container id");
    }
    let Ok(mut slot) = state.interpreter.context.try_lock() else {
        let current = state.interpreter.context_identity.lock().unwrap();
        return if current.as_deref() == Some(id.as_str()) {
            Json(serde_json::json!({"container_id":id,"status":"running"})).into_response()
        } else {
            error(StatusCode::NOT_FOUND, "code container not found")
        };
    };
    let Some(context) = slot
        .as_mut()
        .filter(|context| context.id == id && !context.expired)
    else {
        return error(StatusCode::NOT_FOUND, "code container not found");
    };
    let live = context
        .bridge
        .as_mut()
        .is_some_and(|bridge| matches!(bridge.child.try_wait(), Ok(None)));
    if !live {
        return error(StatusCode::NOT_FOUND, "code container not found");
    }
    Json(serde_json::json!({"container_id":context.id,"status":"running"})).into_response()
}

pub(crate) async fn delete_context(
    State(state): State<Arc<App>>,
    Path(id): Path<String>,
) -> Response {
    if !context_id(&id) {
        return error(StatusCode::BAD_REQUEST, "invalid code container id");
    }
    let Ok(_guard) = state.gate.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    let mut slot = state.interpreter.context.lock().await;
    if slot.as_ref().is_some_and(|context| context.id != id) {
        return error(StatusCode::NOT_FOUND, "code container not found");
    }
    // retire() keeps failed cleanup fenced, kills every interpreter-UID writer
    // and removes only context scratch. Immutable publication spools survive.
    if state.interpreter.retire(&mut slot).await.is_err() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "code_context_cleanup_pending",
        );
    }
    StatusCode::NO_CONTENT.into_response()
}
