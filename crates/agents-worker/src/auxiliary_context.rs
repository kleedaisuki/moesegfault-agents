//! Read-only context resolution for standalone Responses operations.

use agents_core::PinnedContext;
use serde_json::Value;
use worker::{Error, Result};

use crate::{
    context_prefix,
    provider::{self, Message, ProviderConfig, responses::PreparedRequest},
    responses_auxiliary::expand_for_prefix,
    responses_wire::ResponseRequest,
    routes::{self, RouteContext},
    store::Store,
};

pub(crate) struct Context {
    pub agent: String,
    pub pinned: PinnedContext,
    pub messages: Vec<Message>,
    pub native: Vec<Value>,
    pub protected: Vec<Message>,
    pub instructions: Option<String>,
    pub retained: Vec<Value>,
    pub config: ProviderConfig,
    pub prepared: PreparedRequest,
}

pub(crate) async fn prepare(
    env: &RouteContext<'_>,
    owner: &str,
    request: &mut ResponseRequest,
    compact: bool,
    personality: Option<Value>,
) -> Result<Context> {
    let source = match request.previous_response_id.as_deref() {
        Some(id) => {
            let mut response = crate::response_routes::context(env, owner, id).await?;
            if response.status_code() >= 300 {
                return Err(fail(if response.status_code() == 404 {
                    "response_not_found"
                } else {
                    "response_context_unavailable"
                }));
            }
            Some(
                response
                    .json::<crate::session::context::ResponseContext>()
                    .await?,
            )
        }
        None => None,
    };
    if request.model.is_empty() {
        request.model = source
            .as_ref()
            .map(|context| context.record.agent_id.clone())
            .unwrap_or_default();
    }
    if request.model.is_empty() {
        return Err(fail("invalid_auxiliary_request"));
    }
    let pinned = match &source {
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
    let config = provider::resolve_config(env, owner, &pinned.model).await?;
    let base = context_prefix::prefix(&pinned);
    let mut prior_messages = vec![];
    let mut prior_native = vec![];
    let mut instructions = None;
    if let Some(context) = source {
        if context.record.owner_id != owner {
            return Err(fail("response_not_found"));
        }
        prior_messages.extend(context.history_instructions);
        prior_messages.extend(context.messages.into_iter().skip(1));
        prior_native = match context.native_items {
            Some(items) => items,
            None => provider::responses::message_items(&prior_messages)
                .map_err(|_| fail("response_context_unavailable"))?,
        };
        if compact {
            instructions = context.response_instructions;
        }
    }
    if let Some(id) = request
        .conversation_id()
        .map_err(|_| fail("invalid_auxiliary_request"))?
    {
        let conversation = crate::conversation_routes::snapshot(env, owner, id)
            .await?
            .ok_or_else(|| fail("conversation_not_found"))?;
        if conversation.items.iter().any(unbound_item) {
            return Err(fail("unresolved_native_item"));
        }
        prior_native.extend(conversation.items);
    }
    let initial = request
        .normalize_structure()
        .map_err(|_| fail("invalid_auxiliary_request"))?;
    if initial
        .native_items
        .iter()
        .filter(|item| item["type"] != "compaction")
        .any(unbound_item)
    {
        return Err(fail("unresolved_native_item"));
    }
    let image_file_ids = initial
        .native_items
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part["type"] == "input_image")
        .filter_map(|part| {
            part.get("file_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    let expanded = expand_for_prefix(
        &env.secret("DATA_ENCRYPTION_KEY")?.to_string(),
        owner,
        &request.model,
        &pinned.prefix_hash(),
        &initial.native_items,
        &base,
    )?;
    if let Some(protected) = expanded.instructions {
        instructions = Some(protected);
    }
    request.input = Value::Array(expanded.items);
    // Resolve attachments with the established owner-scoped, bounded resolver.
    // This creates no workspace or inference session.
    let resolved = crate::responses_attachments::resolve_read_only(
        env,
        owner,
        &request.input,
        pinned.workspace,
    )
    .await?;
    request.input = resolved.input;
    let normalized = request
        .normalize()
        .map_err(|_| fail("invalid_auxiliary_request"))?;
    if !compact && request.instructions.is_some() {
        instructions = request.instructions.clone();
    }
    let mut prefix = base.clone();
    if let Some(instructions) = &instructions {
        prefix.push_str("\n\nUser-provided instructions for this response:\n");
        prefix.push_str(instructions);
    }
    if prefix.len() > 256 * 1024 {
        return Err(fail("invalid_auxiliary_request"));
    }
    let mut native = provider::responses::message_items(&[Message::text("system", prefix.clone())])
        .map_err(fail)?;
    native.append(&mut prior_native);
    native.extend(normalized.native_items.iter().cloned());
    let mut messages = vec![Message::text("system", prefix)];
    messages.append(&mut prior_messages);
    messages.extend(normalized.messages.iter().cloned());
    let mut protected = vec![Message::text("system", base)];
    protected.extend(
        messages
            .iter()
            .skip(1)
            .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
            .cloned(),
    );
    protected.extend(normalized.history_instructions.iter().cloned());
    let retained = native
        .iter()
        .filter(|item| item.get("role").and_then(Value::as_str) == Some("user"))
        .cloned()
        .collect();
    let definitions = context_prefix::definitions(&pinned);
    if reserved_tool_conflict(request, &definitions) {
        return Err(fail("invalid_auxiliary_request"));
    }
    let declarations = request
        .native_tool_items()
        .map_err(|_| fail("invalid_auxiliary_request"))?;
    let bound = if declarations.iter().any(|tool| {
        matches!(
            tool["type"].as_str(),
            Some(
                "code_interpreter"
                    | "image_generation"
                    | "web_search"
                    | "web_search_preview"
                    | "file_search"
            )
        )
    }) {
        let capabilities = crate::hosted_tools::capabilities(env)?;
        let bound = crate::hosted_bindings::BoundTools::resolve(
            &declarations,
            &pinned.tool_services,
            &capabilities,
            &image_file_ids,
        )?;
        bound.validate_owned(env, owner).await?;
        bound
    } else {
        crate::hosted_bindings::BoundTools::default()
    };
    let mut tools = provider::responses::tool_items(&definitions).map_err(fail)?;
    tools.extend(bound.render_native(&declarations)?);
    let mut controls = request.native_controls();
    if let Some(choice) = controls.get_mut("tool_choice") {
        *choice = bound.rewrite_choice(choice.clone())?;
    }
    if let Some(personality) = personality {
        controls["personality"] = personality;
    }
    let prepared = PreparedRequest::new(&config, &native, &tools, &controls).map_err(fail)?;
    Ok(Context {
        agent: request.model.clone(),
        pinned,
        messages,
        native,
        protected,
        instructions,
        retained,
        config,
        prepared,
    })
}

fn fail(code: impl Into<String>) -> Error {
    Error::RustError(code.into())
}

fn unbound_item(item: &Value) -> bool {
    matches!(
        item["type"].as_str(),
        Some("item_reference" | "reasoning" | "compaction")
    )
}

fn reserved_tool_conflict(
    request: &ResponseRequest,
    definitions: &[crate::tools::ToolDefinition],
) -> bool {
    request
        .tools
        .iter()
        .filter(|tool| matches!(tool.kind.as_str(), "function" | "custom"))
        .any(|tool| {
            tool.name.starts_with("__agents_")
                || crate::tools::tool_kind(&tool.name).is_some()
                || definitions
                    .iter()
                    .any(|definition| definition.function.name == tool.name)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_identity_does_not_collide_with_unqualified_harness_functions() {
        let request: ResponseRequest = serde_json::from_value(serde_json::json!({"model":"agent",
            "tools":[{"type":"namespace","name":"time","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]}]})).unwrap();
        assert!(!reserved_tool_conflict(&request, &[]));
        for kind in ["function", "custom"] {
            let request: ResponseRequest =
                serde_json::from_value(serde_json::json!({"model":"agent",
                "tools":[{"type":kind,"name":"time","parameters":{"type":"object"}}]}))
                .unwrap();
            assert!(reserved_tool_conflict(&request, &[]));
        }
    }
}
