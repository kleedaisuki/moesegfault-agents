//! Provider-native continuation state is private and independent of public events.

use super::*;
use agents_core::ProviderProtocol;
use sha2::{Digest, Sha256};

const MAX_BATCH_ITEMS: usize = 4096;
const MAX_ITEM_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct NativeContext {
    pub(super) history: branch::NativeHistoryRef,
    pub(super) messages_end: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct NativeInputBatch {
    pub(super) items: Vec<Value>,
    pub(super) message_begin: usize,
    pub(super) message_end: usize,
    pub(super) history_begin: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct Generation {
    binding: String,
    end: usize,
}

#[derive(Serialize, Deserialize)]
struct AppendReceipt {
    digest: String,
    history: branch::NativeHistoryRef,
}

#[derive(Clone, Copy)]
enum Mutation {
    Append,
    Replace,
}

fn fault(code: &'static str) -> Error {
    Error::RustError(code.into())
}

fn key(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub(super) fn provider_binding(record: &SessionRecord, config: &ProviderConfig) -> Result<String> {
    let value = json!({"owner":record.owner_id,"provider":record.pinned.model.provider,
        "protocol":config.protocol,"base_url":config.base_url,"model":config.model,
        "credential_id":record.pinned.model.credential_id});
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
}

fn binding(record: &SessionRecord, config: &ProviderConfig) -> Result<String> {
    provider_binding(record, config)
}

fn validate_items(items: &[Value]) -> Result<()> {
    if items.len() > MAX_BATCH_ITEMS {
        return Err(fault("native_context_item_limit"));
    }
    for item in items {
        let easy_message = item.get("type").is_none()
            && matches!(
                item["role"].as_str(),
                Some("system" | "developer" | "user" | "assistant")
            )
            && (item["content"].is_string() || item["content"].is_array());
        if !item.is_object()
            || (item.get("type").and_then(Value::as_str).is_none() && !easy_message)
        {
            return Err(fault("invalid_native_context_item"));
        }
        if serde_json::to_vec(item)?.len() > MAX_ITEM_BYTES {
            return Err(fault("native_context_item_limit"));
        }
    }
    Ok(())
}

#[cfg(test)]
fn unresolved_input(items: &[Value]) -> bool {
    // These identifiers/ciphertexts need our owner-bound resolver, not the
    // platform provider account's broader upstream item namespace.
    items.iter().any(|item| {
        matches!(
            item["type"].as_str(),
            Some("item_reference" | "reasoning" | "compaction")
        )
    })
}

pub(super) fn ordered_input(items: Vec<Value>, closes_calls: bool) -> Vec<Value> {
    if !closes_calls {
        return items;
    }
    let (mut results, input): (Vec<_>, Vec<_>) = items.into_iter().partition(|item| {
        matches!(
            item["type"].as_str(),
            Some("function_call_output" | "custom_tool_call_output")
        )
    });
    results.extend(input);
    results
}

async fn read_history(
    storage: &Storage,
    reference: &branch::NativeHistoryRef,
    session: &str,
) -> Result<Vec<Value>> {
    if reference.source_session_id != session || reference.generation.is_empty() {
        return Err(fault("native_context_unavailable"));
    }
    let generation_key = format!("native:{}:meta", reference.generation);
    let generation = storage
        .get::<Generation>(&generation_key)
        .await?
        .ok_or_else(|| fault("native_context_unavailable"))?;
    if generation.binding != reference.provider_binding || generation.end < reference.end {
        return Err(fault("native_context_unavailable"));
    }
    let keys = (0..reference.end)
        .map(|index| format!("native:{}:entry:{index}", reference.generation))
        .collect::<Vec<_>>();
    let mut items = Vec::with_capacity(reference.end);
    for bytes in crate::session_storage::read_many(storage, &keys).await? {
        let bytes = bytes.ok_or_else(|| fault("native_context_entry_missing"))?;
        items.push(serde_json::from_slice(&bytes)?);
    }
    Ok(items)
}

async fn mutate_history(
    storage: &Storage,
    session: &str,
    prior: Option<&branch::NativeHistoryRef>,
    provider_binding: &str,
    operation: &str,
    mutation: Mutation,
    items: &[Value],
) -> Result<branch::NativeHistoryRef> {
    validate_items(items)?;
    let mode = match mutation {
        Mutation::Append => "append",
        Mutation::Replace => "replace",
    };
    let digest = key(&[mode, provider_binding, &serde_json::to_string(items)?]);
    let operation_key = format!("native:operation:{}", key(&[session, operation]));
    if let Some(receipt) = storage.get::<AppendReceipt>(&operation_key).await? {
        if receipt.digest != digest || receipt.history.provider_binding != provider_binding {
            return Err(fault("native_context_operation_conflict"));
        }
        publish_generation(storage, &receipt.history).await?;
        return Ok(receipt.history);
    }
    let mut history = match (mutation, prior) {
        (Mutation::Append, Some(prior)) => prior.clone(),
        _ => branch::NativeHistoryRef {
            source_session_id: session.into(),
            generation: key(&[session, operation, "generation"]),
            end: 0,
            provider_binding: provider_binding.into(),
        },
    };
    if history.source_session_id != session || history.provider_binding != provider_binding {
        return Err(fault("native_context_provider_mismatch"));
    }
    let meta_key = format!("native:{}:meta", history.generation);
    if let Some(meta) = storage.get::<Generation>(&meta_key).await? {
        if meta.binding != provider_binding || meta.end != history.end {
            return Err(fault("native_context_append_conflict"));
        }
    } else if history.end != 0 {
        return Err(fault("native_context_unavailable"));
    }
    for item in items {
        write_chunks(
            storage,
            &format!("native:{}:entry:{}", history.generation, history.end),
            &serde_json::to_vec(item)?,
        )
        .await?;
        storage
            .put(
                &format!(
                    "native:origin:{}:{}",
                    history.generation,
                    crate::tool_discovery::output_digest(item).map_err(Error::RustError)?
                ),
                history.end,
            )
            .await?;
        history.end = history
            .end
            .checked_add(1)
            .ok_or_else(|| fault("native_context_item_limit"))?;
    }
    storage
        .put(
            &operation_key,
            AppendReceipt {
                digest,
                history: history.clone(),
            },
        )
        .await?;
    publish_generation(storage, &history).await?;
    Ok(history)
}

async fn publish_generation(storage: &Storage, history: &branch::NativeHistoryRef) -> Result<()> {
    let meta_key = format!("native:{}:meta", history.generation);
    if let Some(meta) = storage.get::<Generation>(&meta_key).await? {
        if meta.binding != history.provider_binding {
            return Err(fault("native_context_provider_mismatch"));
        }
        if meta.end >= history.end {
            return Ok(());
        }
    }
    storage
        .put(
            &meta_key,
            Generation {
                binding: history.provider_binding.clone(),
                end: history.end,
            },
        )
        .await
}

fn portable_projection(items: &[Value]) -> bool {
    items.iter().all(|item| {
        matches!(
            item["type"].as_str(),
            Some("message" | "function_call" | "function_call_output" | "reasoning" | "compaction")
        )
    })
}

pub(super) fn conversation_projection(
    items: &[Value],
    provenance: &std::collections::BTreeMap<
        String,
        crate::conversation_routes::ConversationItemProvenance,
    >,
    provider_binding: &str,
) -> Result<Vec<Value>> {
    let mut adapted = Vec::with_capacity(items.len());
    for source in items {
        let origin = source["id"].as_str().and_then(|id| provenance.get(id));
        let trusted_binding = origin.and_then(|origin| origin.provider_binding.as_deref());
        let kind = source["type"].as_str().unwrap_or_default();
        if matches!(kind, "item_reference" | "reasoning" | "compaction") {
            if trusted_binding.is_none() {
                return Err(fault("unresolved_native_item"));
            }
            if trusted_binding == Some(provider_binding)
                && source.get("encrypted_content").is_some()
            {
                return Err(fault("native_context_resolution_required"));
            }
            let summary = source["summary"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !summary.is_empty() {
                adapted.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":summary,"annotations":[]}]}));
            }
            continue;
        }
        if trusted_binding.is_some_and(|binding| binding != provider_binding)
            && !portable_projection(std::slice::from_ref(source))
        {
            return Err(fault("unsupported_native_context_transfer"));
        }
        let mut item = source.clone();
        if let Some(fields) = item.as_object_mut() {
            if !crate::client_tool_items::is_call(source)
                && !crate::client_tool_items::is_output(source)
            {
                fields.remove("id");
            }
            fields.remove("created_by");
        }
        adapted.push(item);
    }
    validate_items(&adapted)?;
    Ok(adapted)
}

pub(super) fn summary_projection(item: &Value) -> Option<Value> {
    let text = item["summary"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then(|| json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}))
}

impl SessionInner {
    pub(super) async fn native_conversation_import(
        &self,
        items: &[Value],
        provenance: &std::collections::BTreeMap<
            String,
            crate::conversation_routes::ConversationItemProvenance,
        >,
        operation: &str,
    ) -> Result<(Vec<Value>, Option<branch::NativeHistoryRef>)> {
        let record = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(|| fault("session_not_initialized"))?
            .record
            .clone();
        let config = self.provider_config(&record).await?;
        let provider_binding = binding(&record, &config)?;
        let context = super::native_items::ItemContext::new(
            &record.owner_id,
            &record.agent_id,
            &record.pinned,
            &config,
        )?;
        let adapted = super::native_items::resolve_local_conversation(
            self,
            &context,
            items,
            provenance,
            &system_prefix(&record),
        )
        .await?;
        let reference = self
            .native_import(&adapted, &provider_binding, operation)
            .await?;
        Ok((super::native_items::validation_input(&adapted)?, reference))
    }

    pub(super) async fn warm_native_input(&self) -> Result<()> {
        let record = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(|| fault("session_missing"))?
            .record
            .clone();
        let config = self.provider_config(&record).await?;
        if config.protocol == ProviderProtocol::Responses {
            self.native_generation_input(&config).await?;
        }
        Ok(())
    }

    pub(super) fn validate_native_generation(
        config: &ProviderConfig,
        request: &crate::responses_wire::ResponseRequest,
        bindings: &crate::tool_rendering::ToolBindings<'_>,
        definitions: &[tools::ToolDefinition],
        options: &mut provider::GenerationOptions,
    ) -> Result<()> {
        if config.protocol != ProviderProtocol::Responses {
            return Ok(());
        }
        let mut controls = request.native_controls();
        if let Some(choice) = controls.get_mut("tool_choice") {
            *choice = bindings.rewrite_choice(choice.clone(), provider::is_deepseek(config))?;
        }
        options.native = Some(
            provider::responses::PreparedRequest::new_with_effects(
                config,
                &[],
                &native_tools(
                    Some(request),
                    bindings,
                    definitions,
                    false,
                    provider::is_deepseek(config),
                )?,
                &controls,
                provider::controls::HarnessControlEffects {
                    max_tool_calls: true,
                    ..Default::default()
                },
            )
            .map_err(Error::RustError)?,
        );
        Ok(())
    }

    pub(super) fn native_branch_ref(&self) -> Result<Option<branch::NativeHistoryRef>> {
        let data = self.data.borrow();
        let data = data
            .as_ref()
            .ok_or_else(|| fault("session_not_initialized"))?;
        Ok(data
            .native_context
            .as_ref()
            .map(|context| context.history.clone()))
    }

    pub(super) async fn native_export(
        &self,
        reference: &branch::NativeHistoryRef,
    ) -> Result<Vec<Value>> {
        let session = self.snapshot()?.record.id;
        read_history(&self.state.storage(), reference, &session).await
    }

    pub(super) async fn native_import(
        &self,
        items: &[Value],
        provider_binding: &str,
        operation: &str,
    ) -> Result<Option<branch::NativeHistoryRef>> {
        let record = self.snapshot()?.record;
        let config = self.provider_config(&record).await?;
        if config.protocol != ProviderProtocol::Responses
            || binding(&record, &config)? != provider_binding
        {
            if portable_projection(items) {
                return Ok(None);
            }
            return Err(fault("unsupported_native_context_transfer"));
        }
        mutate_history(
            &self.state.storage(),
            &record.id,
            None,
            provider_binding,
            operation,
            Mutation::Replace,
            items,
        )
        .await
        .map(Some)
    }

    pub(super) async fn prepare_native_generation(
        &self,
        config: &ProviderConfig,
        definitions: &[tools::ToolDefinition],
        options: &mut provider::GenerationOptions,
    ) -> Result<()> {
        if config.protocol != ProviderProtocol::Responses {
            return Ok(());
        }
        let input = self.native_generation_input(config).await?;
        let (mut controls, tools) = {
            let data = self.data.borrow();
            let run = data
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .ok_or_else(|| fault("native_context_run_missing"))?;
            let request = run.response.as_ref().map(|response| &response.request);
            (
                request
                    .map(|request| request.native_controls())
                    .unwrap_or_else(|| provider::responses::option_controls(options)),
                native_tools(
                    request,
                    &crate::tool_rendering::ToolBindings {
                        hosted: run
                            .response
                            .as_ref()
                            .map(|response| &response.bound_tools)
                            .unwrap_or(&crate::hosted_bindings::BoundTools::default()),
                        mcp: run
                            .response
                            .as_ref()
                            .map(|response| &response.mcp_bindings)
                            .unwrap_or(&crate::mcp_runtime::McpBindings::default()),
                        discovery: run
                            .response
                            .as_ref()
                            .and_then(|response| response.discovery.as_ref()),
                        programmatic: request.is_some_and(|request| {
                            request
                                .tools
                                .iter()
                                .any(|tool| tool.kind == "programmatic_tool_calling")
                        }),
                    },
                    definitions,
                    run.tool_budget_exhausted(),
                    provider::is_deepseek(config),
                )?,
            )
        };
        for (key, value) in provider::responses::option_controls(options)
            .as_object()
            .into_iter()
            .flatten()
        {
            if key == "text" {
                if let (Some(current), Some(format)) = (
                    controls.get_mut(key).and_then(Value::as_object_mut),
                    value.as_object(),
                ) {
                    current.extend(
                        format
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone())),
                    );
                    continue;
                }
            }
            controls[key] = value.clone();
        }
        if let Some(limit) = config.max_output_tokens {
            controls["max_output_tokens"] = json!(limit);
        }
        if let Some(choice) = controls.get_mut("tool_choice") {
            let data = self.data.borrow();
            if let Some(response) = data
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .and_then(|run| run.response.as_ref())
            {
                *choice = crate::tool_rendering::ToolBindings {
                    hosted: &response.bound_tools,
                    mcp: &response.mcp_bindings,
                    discovery: response.discovery.as_ref(),
                    programmatic: response
                        .request
                        .tools
                        .iter()
                        .any(|tool| tool.kind == "programmatic_tool_calling"),
                }
                .rewrite_choice(choice.clone(), provider::is_deepseek(config))?;
            }
        }
        options.native = Some(
            provider::responses::PreparedRequest::new_with_effects(
                config,
                &input,
                &tools,
                &controls,
                provider::controls::HarnessControlEffects {
                    max_tool_calls: true,
                    ..Default::default()
                },
            )
            .map_err(Error::RustError)?,
        );
        Ok(())
    }

    pub(super) async fn native_compaction_request(
        &self,
        config: &ProviderConfig,
        instruction: &Message,
    ) -> Result<Option<provider::responses::PreparedRequest>> {
        if config.protocol != ProviderProtocol::Responses {
            return Ok(None);
        }
        let mut input = self.native_generation_input(config).await?;
        let prefix = provider::responses::message_items(std::slice::from_ref(instruction))
            .map_err(Error::RustError)?;
        input.splice(..1, prefix);
        provider::responses::PreparedRequest::new(config, &input, &[], &json!({}))
            .map(Some)
            .map_err(Error::RustError)
    }

    async fn native_generation_input(&self, config: &ProviderConfig) -> Result<Vec<Value>> {
        let _guard = self.persistence.lock().await;
        let data = self.snapshot()?;
        let run = data
            .run
            .as_ref()
            .ok_or_else(|| fault("native_context_run_missing"))?;
        let provider_binding = binding(&data.record, config)?;
        let mut context = data.native_context.clone();
        if context
            .as_ref()
            .is_some_and(|ctx| ctx.history.provider_binding != provider_binding)
        {
            return Err(fault("native_context_provider_mismatch"));
        }
        let batch = run.native_input.as_ref();
        if context.is_none() {
            let end = batch.map_or(data.messages.len(), |batch| batch.message_begin);
            if end == 0 || end > data.messages.len() {
                return Err(fault("native_context_message_boundary"));
            }
            let mut initial = Vec::new();
            if let Some(batch) = batch {
                if batch.history_begin > data.history_instructions.len() {
                    return Err(fault("native_context_message_boundary"));
                }
                initial.extend(
                    provider::responses::message_items(
                        &data.history_instructions[..batch.history_begin],
                    )
                    .map_err(Error::RustError)?,
                );
            }
            initial.extend(
                provider::responses::message_items(&data.messages[1..end])
                    .map_err(Error::RustError)?,
            );
            let history = mutate_history(
                &self.state.storage(),
                &data.record.id,
                None,
                &provider_binding,
                &format!("init:{}", run.id),
                Mutation::Replace,
                &initial,
            )
            .await?;
            context = Some(NativeContext {
                history,
                messages_end: end,
            });
        }
        let mut context = context.unwrap();
        if let Some(batch) = batch {
            if batch.message_end > data.messages.len() || batch.message_begin > batch.message_end {
                return Err(fault("native_context_message_boundary"));
            }
            context.history = mutate_history(
                &self.state.storage(),
                &data.record.id,
                Some(&context.history),
                &provider_binding,
                &format!("input:{}", run.id),
                Mutation::Append,
                &batch.items,
            )
            .await?;
            context.messages_end = batch.message_end;
        }
        if context.messages_end > data.messages.len() {
            return Err(fault("native_context_message_boundary"));
        }
        let additions = provider::responses::message_items(&data.messages[context.messages_end..])
            .map_err(Error::RustError)?;
        if !additions.is_empty() {
            context.history = mutate_history(
                &self.state.storage(),
                &data.record.id,
                Some(&context.history),
                &provider_binding,
                &format!("messages:{}:{}", run.id, run.round),
                Mutation::Append,
                &additions,
            )
            .await?;
        }
        context.messages_end = data.messages.len();
        let mut input =
            provider::responses::message_items(&data.messages[..1]).map_err(Error::RustError)?;
        input.extend(read_history(&self.state.storage(), &context.history, &data.record.id).await?);
        let mut current = self.data.borrow_mut();
        let current = current.as_mut().unwrap();
        current.native_context = Some(context);
        current.run.as_mut().unwrap().native_input = None;
        Ok(input)
    }

    pub(super) async fn accept_native_turn(
        &self,
        run: &RunState,
        completion: &Completion,
    ) -> Result<()> {
        let Some(turn) = &completion.native else {
            return Ok(());
        };
        let _guard = self.persistence.lock().await;
        let (session, mut context, messages_end) = {
            let data = self.data.borrow();
            let data = data
                .as_ref()
                .ok_or_else(|| fault("session_not_initialized"))?;
            (
                data.record.id.clone(),
                data.native_context
                    .clone()
                    .ok_or_else(|| fault("native_context_unavailable"))?,
                data.messages.len(),
            )
        };
        context.history = mutate_history(
            &self.state.storage(),
            &session,
            Some(&context.history),
            &context.history.provider_binding,
            &format!("output:{}:{}", run.id, run.round),
            Mutation::Append,
            &turn.output,
        )
        .await?;
        context.messages_end = messages_end + 1;
        self.data.borrow_mut().as_mut().unwrap().native_context = Some(context);
        Ok(())
    }

    pub(super) async fn replace_native_after_compaction(&self, run: &RunState) -> Result<()> {
        let _guard = self.persistence.lock().await;
        let data = self.snapshot()?;
        let Some(context) = data.native_context else {
            return Ok(());
        };
        let mut items = provider::responses::message_items(&data.history_instructions)
            .map_err(Error::RustError)?;
        items.extend(
            provider::responses::message_items(&data.messages[1..]).map_err(Error::RustError)?,
        );
        let history = mutate_history(
            &self.state.storage(),
            &data.record.id,
            None,
            &context.history.provider_binding,
            &format!("compact:{}:{}", run.id, data.compact_count),
            Mutation::Replace,
            &items,
        )
        .await?;
        self.data.borrow_mut().as_mut().unwrap().native_context = Some(NativeContext {
            history,
            messages_end: data.messages.len(),
        });
        Ok(())
    }
}

fn native_tools(
    request: Option<&crate::responses_wire::ResponseRequest>,
    bindings: &crate::tool_rendering::ToolBindings<'_>,
    definitions: &[tools::ToolDefinition],
    exhausted: bool,
    bridge: bool,
) -> Result<Vec<Value>> {
    let Some(request) = request else {
        return provider::responses::tool_items(definitions).map_err(Error::RustError);
    };
    let retained = definitions
        .iter()
        .filter(|definition| {
            !bindings.is_server_alias(&definition.function.name)
                && !request
                    .function_tools()
                    .any(|tool| tool.name == definition.function.name)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut tools = provider::responses::tool_items(&retained).map_err(Error::RustError)?;
    let mut declared = bindings.render_native(
        &request.native_tool_items().map_err(Error::RustError)?,
        bridge,
        definitions.iter().any(|tool| tool.function.name == "time"),
    )?;
    if exhausted {
        declared.retain(|tool| {
            !tool["name"]
                .as_str()
                .is_some_and(|name| bindings.is_server_alias(name))
        });
    }
    tools.extend(declared);
    Ok(tools)
}

pub(super) fn completion_failure(completion: &Completion) -> Option<&'static str> {
    use provider::responses::TerminalStatus;
    let native = completion.native.as_ref()?;
    match native.status {
        TerminalStatus::Failed => Some("provider_response_failed"),
        TerminalStatus::Cancelled => Some("provider_response_cancelled"),
        _ if native.output.iter().any(|item| {
            !crate::client_tool_items::is_call(item)
                && !matches!(
                    item["type"].as_str(),
                    Some(
                        "message"
                            | "reasoning"
                            | "function_call"
                            | "custom_tool_call"
                            | "tool_search_call"
                            | "tool_search_output"
                    )
                )
        }) =>
        {
            Some("native_output_binding_required")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_private_state_needs_trusted_route_provenance() {
        let mut provenance = std::collections::BTreeMap::new();
        let reasoning = json!({"id":"ours","type":"reasoning","summary":[{"type":"summary_text","text":"visible summary"}],"encrypted_content":"vendor-opaque"});
        assert!(
            conversation_projection(std::slice::from_ref(&reasoning), &provenance, "target")
                .is_err()
        );
        provenance.insert(
            "ours".into(),
            crate::conversation_routes::ConversationItemProvenance {
                source_session_id: Some("source".into()),
                response_id: Some("response".into()),
                provider_binding: Some("foreign".into()),
            },
        );
        let portable =
            conversation_projection(std::slice::from_ref(&reasoning), &provenance, "target")
                .unwrap();
        assert_eq!(portable[0]["content"][0]["text"], "visible summary");
        assert!(
            !serde_json::to_string(&portable)
                .unwrap()
                .contains("vendor-opaque")
        );
        provenance.get_mut("ours").unwrap().provider_binding = Some("target".into());
        assert!(conversation_projection(&[reasoning], &provenance, "target").is_err());
    }

    #[test]
    fn operation_identity_is_length_delimited_and_content_bound() {
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
        assert_eq!(key(&["same", "op"]), key(&["same", "op"]));
        assert_ne!(key(&["same", "op"]), key(&["same", "other"]));
    }

    #[test]
    fn client_returns_close_call_group_before_queued_input_without_source_mutation() {
        let input = vec![
            json!({"type":"message","role":"user","content":"queued"}),
            json!({"type":"custom_tool_call_output","call_id":"call","output":"done"}),
        ];
        let canonical = input.clone();
        assert_eq!(ordered_input(input.clone(), false), canonical);
        let execution = ordered_input(input, true);
        assert_eq!(execution[0]["type"], "custom_tool_call_output");
        assert_eq!(canonical[0]["role"], "user");
    }

    #[test]
    fn native_items_require_typed_objects_and_portable_context_is_explicit() {
        assert!(
            validate_items(&[json!({"type":"reasoning","encrypted_content":"opaque"})]).is_ok()
        );
        assert!(validate_items(&[json!("not an item")]).is_err());
        assert!(validate_items(&[json!({"text":"missing type"})]).is_err());
        assert!(validate_items(&[json!({"role":"user","content":"SDK easy input"})]).is_ok());
        assert!(validate_items(&[json!({"role":"tool","content":"unpaired"})]).is_err());
        assert!(validate_items(&[json!({"role":"user","content":42})]).is_err());
        assert!(portable_projection(&[
            json!({"type":"reasoning"}),
            json!({"type":"message"})
        ]));
        assert!(!portable_projection(&[json!({"type":"custom_tool_call"})]));
    }

    #[test]
    fn caller_item_references_and_opaque_state_require_owned_resolution() {
        for kind in ["item_reference", "reasoning", "compaction"] {
            assert!(unresolved_input(&[
                json!({"type":kind,"id":"foreign","encrypted_content":"opaque"})
            ]));
        }
        assert!(!unresolved_input(&[
            json!({"type":"message","role":"user","content":"hello"})
        ]));
        assert!(!unresolved_input(&[
            json!({"type":"function_call_output","call_id":"owned","output":"done"})
        ]));
        // A trusted persisted provider turn uses the independent log validator.
        assert!(
            validate_items(&[json!({"type":"reasoning","encrypted_content":"opaque"})]).is_ok()
        );
    }
}
