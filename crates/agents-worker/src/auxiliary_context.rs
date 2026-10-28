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
    pub configuration: Option<agents_core::SessionConfiguration>,
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
    let configuration = selected.configuration;
    let configurations = selected.configurations;
    let registered = configuration
        .as_ref()
        .map_or(pinned.tools.as_slice(), |configuration| {
            configuration.tools.as_slice()
        });
    let config = provider::resolve_config(env, owner, &pinned.model).await?;
    let dynamic = config.protocol == agents_core::ProviderProtocol::Responses;
    let item_context =
        crate::session::native_items::ItemContext::new(owner, &request.model, &pinned, &config)?;
    let base = if dynamic {
        context_prefix::dynamic_prefix()
    } else {
        context_prefix::prefix(&pinned)
    };
    let mut prior_messages = vec![];
    let mut prior_native = vec![];
    let mut instructions = None;
    let mut inherited_discovery = None;
    let mut pending_search = Vec::new();
    let mut context_revision = None;
    let mut context_updates = vec![];
    let mut configuration_positions = vec![];
    let mut obsolete_updates = vec![];
    let mut configuration_state = None;
    let mut context_state = None;
    let requested_profile =
        crate::runtime_profiles::requested_tools_for(&request.tools, &pinned.tool_services)?;
    let mut profile = selected
        .runtime_profile
        .or(requested_profile)
        .unwrap_or_default();
    if let Some(context) = source {
        let cross_agent = context.record.agent_id != request.model;
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
        if requested_profile.is_none() {
            profile = context.record.runtime_profile;
        }
        context_revision = context.context_revision;
        context_updates = context.context_updates;
        configuration_positions = context.configuration_positions;
        obsolete_updates = context.obsolete_updates;
        configuration_state = context.configuration_state;
        context_state = context.context_state;
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
        if compact || cross_agent {
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
            configuration_positions.clear();
            obsolete_updates.clear();
            configuration_state = None;
            context_state = None;
            context_revision = None;
            if compact {
                instructions = context.response_instructions;
            }
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
        .filter(|_| !dynamic && !crate::prompt_context::enabled(&pinned, &config))
    {
        prefix.push_str("\n\nUser-provided instructions for this response:\n");
        prefix.push_str(instructions);
    }
    let protected_bytes = prefix.len().saturating_add(
        if dynamic || crate::prompt_context::enabled(&pinned, &config) {
            instructions.as_ref().map_or(0, String::len)
        } else {
            0
        },
    );
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
                context_prefix::registered_definitions(registered)
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
    let mut configuration_positions = configuration_positions
        .into_iter()
        .map(|index| index + 1)
        .collect::<Vec<_>>();
    let mut obsolete_positions = obsolete_updates
        .into_iter()
        .map(|index| index + 1)
        .collect::<Vec<_>>();
    let response_positions = context_updates
        .iter()
        .map(|index| index + 1)
        .filter(|index| configuration_positions.binary_search(index).is_err())
        .collect::<Vec<_>>();
    if let Some(history) = configuration_state.as_mut() {
        for position in &mut history.updates {
            *position += 1;
        }
        for position in history.active.values_mut() {
            *position += 1;
        }
    }
    if let Some(history) = context_state.as_mut() {
        for position in &mut history.updates {
            *position += 1;
        }
        for position in history.active.values_mut() {
            *position += 1;
        }
    }
    native.extend(native_input);
    if !compact {
        for configuration in &configurations {
            let prior = configuration_state.take();
            let items = if dynamic {
                crate::configuration::dynamic_items(&pinned, configuration, prior.as_ref())
            } else {
                vec![("legacy", crate::configuration::item(configuration))]
            };
            let mut updates = prior
                .as_ref()
                .map(|history| history.updates.clone())
                .unwrap_or_else(|| configuration_positions.clone());
            let mut active = prior
                .as_ref()
                .map(|history| history.active.clone())
                .unwrap_or_default();
            for (name, item) in items {
                let index = native.len();
                updates.push(index);
                active.insert(name.into(), index);
                native.push(item);
            }
            let state = crate::configuration::History {
                revision: configuration.agent_revision,
                updates,
                active,
                guidance_digest: Some(crate::configuration::guidance_digest(configuration)),
                catalog_digest: Some(crate::registered_tools::revision(configuration)),
            };
            let live = state.live_positions();
            obsolete_positions.extend(
                state
                    .updates
                    .iter()
                    .copied()
                    .filter(|index| !live.contains(index)),
            );
            configuration_positions = state.updates.clone();
            configuration_state = Some(state);
        }
    }
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
    let definitions = context_prefix::registered_definitions(registered);
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
        registered.iter().any(|tool| tool.name == "time"),
    )?);
    if dynamic {
        tools = crate::registered_tools::render(tools, registered);
    }
    let mut controls = request.native_controls();
    if let Some(choice) = controls.get_mut("tool_choice") {
        *choice = bindings.rewrite_choice(choice.clone(), true)?;
        if dynamic {
            *choice = crate::registered_tools::choice(choice.clone(), registered);
        }
    }
    if let Some(personality) = personality {
        controls["personality"] = personality;
    }
    if !compact && (dynamic || crate::prompt_context::enabled(&pinned, &config)) {
        let available = registered
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let snapshot = if dynamic {
            crate::prompt_context::dynamic_snapshot(
                &pinned,
                profile,
                instructions.as_deref(),
                request.tool_choice.as_ref(),
                &available,
            )
        } else {
            crate::prompt_context::snapshot(&pinned, profile, instructions.as_deref(), &tools)
        };
        if let Some((_, item)) =
            crate::prompt_context::update(context_revision.as_deref(), &snapshot)
        {
            let prior = context_state.take();
            let items = if dynamic {
                crate::prompt_context::dynamic_items(&snapshot, prior.as_ref())
            } else {
                vec![("legacy", item)]
            };
            let mut updates = prior
                .as_ref()
                .map(|history| history.updates.clone())
                .unwrap_or(response_positions);
            let mut active = prior
                .as_ref()
                .map(|history| history.active.clone())
                .unwrap_or_default();
            for (name, item) in items {
                let index = native.len();
                updates.push(index);
                active.insert(name.into(), index);
                native.push(item);
            }
            let state = crate::prompt_context::Revision {
                digest: crate::prompt_context::digest(&snapshot),
                updates,
                active,
                guidance_digest: Some(crate::prompt_context::digest(&snapshot["instructions"])),
            };
            let live = state.live_positions();
            obsolete_positions.extend(
                state
                    .updates
                    .iter()
                    .copied()
                    .filter(|index| !live.contains(index)),
            );
            context_state = Some(state);
        }
    }
    obsolete_positions.sort_unstable();
    obsolete_positions.dedup();
    let mut required = configuration_state
        .as_ref()
        .map(|state| state.live_positions())
        .unwrap_or_default();
    required.extend(
        context_state
            .as_ref()
            .map(|state| state.live_positions())
            .unwrap_or_default(),
    );
    let (prepared, measured_input_tokens) = if controls["truncation"] == "auto" {
        let fitted = crate::responses_auxiliary::resource_count(
            env,
            owner,
            crate::native_window::fit_with_transitions(
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
                &obsolete_positions,
                &required,
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
        configuration,
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
