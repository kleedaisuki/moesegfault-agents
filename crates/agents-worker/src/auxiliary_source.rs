//! Read-only source selection uses published immutable Response boundaries.

use agents_core::PinnedContext;
use worker::{Error, Result};

use crate::{
    conversation_routes::{self, ConversationSnapshot},
    responses_wire::ResponseRequest,
    routes::{self, RouteContext},
    session::context::ResponseContext,
    store::Store,
};

pub(crate) struct Selection {
    pub pinned: PinnedContext,
    pub previous: Option<ResponseContext>,
    pub conversation: Option<ConversationSnapshot>,
}

pub(crate) async fn select(
    env: &RouteContext<'_>,
    owner: &str,
    request: &mut ResponseRequest,
) -> Result<Selection> {
    let conversation = match request
        .conversation_id()
        .map_err(|_| fail("invalid_auxiliary_request"))?
    {
        Some(id) => Some(
            conversation_routes::snapshot(env, owner, id)
                .await?
                .ok_or_else(|| fail("conversation_not_found"))?,
        ),
        None => None,
    };
    let independent = conversation.as_ref().is_some_and(|view| {
        !request.model.is_empty()
            && view
                .predecessor_agent_id
                .as_ref()
                .is_some_and(|agent| *agent != request.model)
    });
    let source_id = request.previous_response_id.clone().or_else(|| {
        conversation
            .as_ref()
            .filter(|_| !independent)
            .and_then(|view| view.predecessor_response_id.clone())
    });
    let previous = match source_id.as_deref() {
        Some(id) => {
            let mut response = crate::response_routes::context(env, owner, id).await?;
            if response.status_code() != 200 {
                return Err(fail(if response.status_code() == 404 {
                    "response_not_found"
                } else {
                    "response_context_unavailable"
                }));
            }
            let context: ResponseContext = response.json().await?;
            if context.record.owner_id != owner || context.response_id != id {
                return Err(fail("response_not_found"));
            }
            if let Some(view) = &conversation {
                let profile = crate::runtime_profiles::requested_tools(&request.tools)?;
                if !matches_anchor(view, &context)
                    || profile.is_some_and(|profile| profile != context.record.runtime_profile)
                {
                    return Err(fail("response_context_mismatch"));
                }
            }
            Some(context)
        }
        None => None,
    };
    if request.model.is_empty() {
        request.model = previous
            .as_ref()
            .map(|context| context.record.agent_id.clone())
            .unwrap_or_default();
    }
    if request.model.is_empty() {
        return Err(fail("invalid_auxiliary_request"));
    }
    let pinned = match &previous {
        Some(context) if context.record.agent_id == request.model => context.record.pinned.clone(),
        _ => {
            let agent = Store::new(env)
                .get_agent(owner, &request.model)
                .await?
                .ok_or_else(|| fail("agent_not_found"))?;
            routes::pin(env, &agent)
                .await?
                .map_err(|_| fail("invalid_agent_configuration"))?
        }
    };
    let use_anchor = conversation.is_some() && previous.is_some();
    Ok(Selection {
        pinned,
        previous,
        conversation: if use_anchor { None } else { conversation },
    })
}

fn matches_anchor(view: &ConversationSnapshot, source: &ResponseContext) -> bool {
    view.predecessor_response_id.as_deref() == Some(source.response_id.as_str())
        && view.predecessor_session_id.as_deref() == Some(source.record.id.as_str())
        && view.predecessor_agent_id.as_deref() == Some(source.record.agent_id.as_str())
        && view.predecessor_pin_hash.as_deref() == Some(source.record.pinned.prefix_hash().as_str())
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
