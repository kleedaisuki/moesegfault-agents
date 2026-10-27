//! Read-only context resolution for standalone Responses operations.

use agents_core::PinnedContext;
use serde_json::Value;
use worker::{Error, Result};

use crate::{
    context_prefix,
    provider::{self, Message, ProviderConfig, responses::PreparedRequest},
    responses_auxiliary::expand_for_prefix,
    responses_wire::ResponseRequest,
    routes::RouteContext,
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
    pub measured_input_tokens: Option<u64>,
}

pub(crate) async fn prepare(
    env: &RouteContext<'_>,
    owner: &str,
    request: &mut ResponseRequest,
    compact: bool,
    personality: Option<Value>,
    session_header: Option<&str>,
) -> Result<Context> {
    let selected = crate::auxiliary_source::select(env, owner, request, session_header).await?;
    let source = selected.previous;
    let pinned = selected.pinned;
    let config = provider::resolve_config(env, owner, &pinned.model).await?;
    let item_context =
        crate::session::native_items::ItemContext::new(owner, &request.model, &pinned, &config)?;
    let base = context_prefix::prefix(&pinned);
    let mut prior_messages = vec![];
    let mut prior_native = vec![];
    let mut instructions = None;
    let mut inherited_discovery = None;
    let mut pending_search = Vec::new();
    let mut context_revision = None;
    let mut context_updates = vec![];
    let mut profile = selected
        .runtime_profile
        .or(crate::runtime_profiles::requested_tools(&request.tools)?)
        .unwrap_or_default();
    if let Some(context) = source {
        if context
            .native_provider_binding
            .as_ref()
            .is_some_and(|binding| *binding != item_context.provider_binding)
        {
            return Err(fail("response_context_mismatch"));
        }
        if context.record.owner_id != owner {
            return Err(fail("response_not_found"));
        }
        if crate::runtime_profiles::requested_tools(&request.tools)?.is_none() {
            profile = context.record.runtime_profile;
        }
        context_revision = context.context_revision;
        context_updates = context.context_updates;
        inherited_discovery = context.discovery;
        pending_search = context
            .general_pending_calls
            .iter()
            .filter(|call| call.kind == "tool_search")
            .map(|call| call.call_id.clone())
            .collect();
        prior_messages.extend(context.history_instructions);
        prior_messages.extend(context.messages.into_iter().skip(1));
        prior_native = match context.native_items {
            Some(items) => items,
            None => provider::responses::message_items(&prior_messages)
                .map_err(|_| fail("response_context_unavailable"))?,
        };
        if compact {
            prior_native = prior_native
                .into_iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    context_updates
                        .binary_search(&index)
                        .is_err()
                        .then_some(item)
                })
                .collect();
            context_updates.clear();
            instructions = context.response_instructions;
        }
    }
    if let Some(conversation) = selected.conversation {
        prior_native.extend(
            crate::session::native_items::resolve_conversation(
                env,
                &item_context,
                &conversation.items,
                &conversation.item_provenance,
                &base,
            )
            .await?,
        );
    }
    let initial = request
        .normalize_structure()
        .map_err(|_| fail("invalid_auxiliary_request"))?;
    let owned_native =
        crate::session::native_items::resolve_input(env, &item_context, &initial.native_items)
            .await?;
    PreparedRequest::validate_input_capabilities(&config, &owned_native).map_err(fail)?;
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
        &owned_native,
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
    let mut native_input = resolved
        .input
        .as_array()
        .ok_or_else(|| fail("invalid_resolved_native_input"))?
        .clone();
    request.input = Value::Array(crate::session::native_items::validation_input(
        &native_input,
    )?);
    let normalized = request
        .normalize()
        .map_err(|_| fail("invalid_auxiliary_request"))?;
    if !compact && request.instructions.is_some() {
        instructions = request.instructions.clone();
    }
    let mut prefix = base.clone();
    if let Some(instructions) = instructions
        .as_ref()
        .filter(|_| !crate::prompt_context::enabled(&pinned, &config))
    {
        prefix.push_str("\n\nUser-provided instructions for this response:\n");
        prefix.push_str(instructions);
    }
    let protected_bytes =
        prefix
            .len()
            .saturating_add(if crate::prompt_context::enabled(&pinned, &config) {
                instructions.as_ref().map_or(0, String::len)
            } else {
                0
            });
    if protected_bytes > 256 * 1024 {
        return Err(fail("invalid_auxiliary_request"));
    }
    let declarations = request.native_tool_items().map_err(fail)?;
    let mut mcp = if declarations.iter().any(|tool| tool["type"] == "mcp") {
        let mut funding = crate::mcp_funding::DiscoveryFunding::start_read(
            env,
            owner,
            agents_auth::billing::Resource::Worker,
        )
        .await?;
        let resolved = crate::mcp_runtime::McpBindings::resolve_in_scope(
            env,
            owner,
            &crate::util::id("count_mcp"),
            &declarations,
        )
        .await;
        funding.observe_end();
        funding.finish(env, owner).await?;
        resolved?
    } else {
        crate::mcp_runtime::McpBindings::default()
    };
    mcp.require_ready()?;
    let mut discovery =
        crate::tool_discovery::ToolRegistry::compile_with_input(&declarations, &native_input)
            .map_err(fail)?;
    if let Some(registry) = discovery.as_mut() {
        registry.register_mcp(&mcp).map_err(fail)?;
        registry
            .reserve_server_names(
                context_prefix::definitions(&pinned)
                    .iter()
                    .map(|definition| definition.function.name.as_str()),
            )
            .map_err(fail)?;
        if let Some(previous) = &inherited_discovery {
            let origins = prior_native
                .iter()
                .map(crate::tool_discovery::output_digest)
                .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()
                .map_err(fail)?;
            registry.inherit_loaded_from(previous, &origins);
        }
        registry
            .authorize_pending_search(pending_search.iter().map(String::as_str))
            .map_err(fail)?;
        native_input = registry
            .encode_input(&native_input, true, request.tool_choice.as_ref())
            .map_err(fail)?;
        registry.select_choice(request.tool_choice.as_ref());
    }
    let mut native = provider::responses::message_items(&[Message::text("system", prefix.clone())])
        .map_err(fail)?;
    native.append(&mut prior_native);
    native.extend(native_input);
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
        let mut bound = crate::hosted_bindings::BoundTools::resolve(
            &declarations,
            &pinned.tool_services,
            &capabilities,
            &image_file_ids,
        )?;
        bound.validate_owned(env, owner).await?;
        bound.resolve_owned_profiles(env, owner).await?;
        bound
    } else {
        crate::hosted_bindings::BoundTools::default()
    };
    let mut tools = provider::responses::tool_items(&definitions).map_err(fail)?;
    let bindings = crate::tool_rendering::ToolBindings {
        hosted: &bound,
        mcp: &mcp,
        discovery: discovery.as_ref(),
        programmatic: declarations
            .iter()
            .any(|tool| tool["type"] == "programmatic_tool_calling"),
    };
    tools.extend(bindings.render_native(
        &declarations,
        true,
        pinned.tools.iter().any(|tool| tool.name == "time"),
    )?);
    let mut controls = request.native_controls();
    if let Some(choice) = controls.get_mut("tool_choice") {
        *choice = bindings.rewrite_choice(choice.clone(), true)?;
    }
    if let Some(personality) = personality {
        controls["personality"] = personality;
    }
    let mut context_positions = context_updates
        .into_iter()
        .map(|index| index + 1)
        .collect::<Vec<_>>();
    if !compact && crate::prompt_context::enabled(&pinned, &config) {
        let snapshot =
            crate::prompt_context::snapshot(&pinned, profile, instructions.as_deref(), &tools);
        if let Some((_, item)) =
            crate::prompt_context::update(context_revision.as_deref(), &snapshot)
        {
            context_positions.push(native.len());
            native.push(item);
        }
    }
    let (prepared, measured_input_tokens) = if controls["truncation"] == "auto" {
        let fitted = crate::responses_auxiliary::resource_count(
            env,
            owner,
            crate::native_window::fit_with_context(
                &config,
                &native,
                &tools,
                &controls,
                pinned.model.context_window,
                config
                    .max_output_tokens
                    .unwrap_or(pinned.model.max_output_tokens),
                crate::provider::controls::HarnessControlEffects {
                    max_tool_calls: true,
                    ..Default::default()
                },
                &context_positions,
            ),
        )
        .await?;
        (fitted.prepared, fitted.input_tokens)
    } else {
        (
            PreparedRequest::new(&config, &native, &tools, &controls).map_err(fail)?,
            None,
        )
    };
    mcp.clear_credentials();
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
        measured_input_tokens,
    })
}

fn fail(code: impl Into<String>) -> Error {
    Error::RustError(code.into())
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
