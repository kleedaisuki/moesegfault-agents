//! Canonical public projection of private provider-native output.
use super::*;
use std::collections::BTreeMap;

pub(super) fn call_item(item: &Value) -> bool {
    matches!(
        item["type"].as_str(),
        Some("function_call" | "custom_tool_call")
    ) || crate::client_tool_items::is_call(item)
}

fn direct_caller(item: &Value) -> Result<()> {
    // Native model JSON is not the runtime's authenticated programmatic RPC.
    // A caller field cannot manufacture that execution authority.
    if item
        .get("caller")
        .is_some_and(|caller| !caller.is_null() && caller["type"] != "direct")
    {
        return Err(Error::RustError("untrusted_programmatic_caller".into()));
    }
    Ok(())
}

pub(super) fn require_direct_permission(callers: Option<&Value>) -> Result<()> {
    if callers
        .filter(|value| !value.is_null())
        .is_some_and(|callers| {
            !callers
                .as_array()
                .is_some_and(|callers| callers.iter().any(|caller| caller == "direct"))
        })
    {
        return Err(Error::RustError("tool_caller_not_allowed".into()));
    }
    Ok(())
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct NativeStream {
    round: u64,
    base: usize,
    items: BTreeMap<usize, StreamItem>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StreamItem {
    id: String,
    index: usize,
    visible: bool,
    done: bool,
    #[serde(default)]
    deferred_done: bool,
    #[serde(default)]
    buffered_input: bool,
}

impl NativeStream {
    fn map(
        &mut self,
        round: u64,
        base: usize,
        response_id: &str,
        request: &crate::responses_wire::ResponseRequest,
        kind: &str,
        payload: &Value,
    ) -> Result<StreamItem> {
        let ordinal = payload["output_index"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| Error::RustError("native_stream_index_missing".into()))?;
        if self.round != round || self.items.is_empty() {
            *self = Self {
                round,
                base,
                items: BTreeMap::new(),
            };
        }
        if kind == "response.output_item.added" {
            let raw = &payload["item"];
            let visible = !call_item(raw) || declared(request, raw);
            let index = self.base + self.items.values().filter(|item| item.visible).count();
            let item = StreamItem {
                id: format!("item_{response_id}_{round}_{ordinal}"),
                index,
                visible,
                done: false,
                deferred_done: false,
                buffered_input: raw["type"] == "custom_tool_call",
            };
            if self.items.contains_key(&ordinal) {
                return Err(Error::RustError("native_stream_duplicate_item".into()));
            }
            self.items.insert(ordinal, item);
        }
        let item = self
            .items
            .get_mut(&ordinal)
            .ok_or_else(|| Error::RustError("native_stream_item_missing".into()))?;
        if item.done {
            return Err(Error::RustError("native_stream_item_already_done".into()));
        }
        if kind == "response.output_item.done" {
            item.done = true;
            item.deferred_done = true;
        }
        Ok(item.clone())
    }
}

fn completed_events(
    item: &Value,
    index: usize,
    buffered_input: bool,
    complete: bool,
) -> Vec<(&'static str, Value)> {
    let mut events = Vec::with_capacity(3);
    if complete {
        match item["type"].as_str() {
            Some("function_call") => events.push(("response.function_call_arguments.done", json!({"item_id":item["id"],"output_index":index,"arguments":item["arguments"],"name":item["name"]}))),
            Some("custom_tool_call") => {
                if buffered_input { events.push(("response.custom_tool_call_input.delta", json!({"item_id":item["id"],"output_index":index,"delta":item["input"]}))); }
                events.push(("response.custom_tool_call_input.done", json!({"item_id":item["id"],"output_index":index,"input":item["input"]})));
            }
            _ => {}
        }
    }
    events.push((
        "response.output_item.done",
        json!({"output_index":index,"item":item}),
    ));
    events
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct GeneralPendingCall {
    pub item_id: String,
    pub call_id: String,
    pub name: String,
    pub namespace: Option<String>,
    pub kind: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl std::fmt::Debug for GeneralPendingCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GeneralPendingCall")
            .field("item_id", &self.item_id)
            .field("call_id", &self.call_id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl GeneralPendingCall {
    pub(crate) fn from_item(item: &Value) -> Option<Self> {
        if let Some(family) = item["type"]
            .as_str()
            .and_then(crate::client_tool_items::Family::call)
        {
            // Only pairing/safety metadata is retained here; actions and patches
            // already have one canonical output and private native-log copy.
            let details = ["type", "call_id", "pending_safety_checks", "caller"]
                .iter()
                .filter_map(|key| item.get(*key).map(|value| ((*key).into(), value.clone())))
                .collect();
            return Some(Self {
                item_id: item["id"].as_str()?.into(),
                call_id: item["call_id"].as_str()?.into(),
                name: family.name().into(),
                namespace: None,
                kind: family.name().into(),
                input: String::new(),
                details: Some(Value::Object(details)),
            });
        }
        let kind = match item["type"].as_str()? {
            "function_call" => "function",
            "custom_tool_call" => "custom",
            _ => return None,
        };
        Some(Self {
            item_id: item["id"].as_str()?.into(),
            call_id: item["call_id"].as_str()?.into(),
            name: item["name"].as_str()?.into(),
            namespace: item["namespace"].as_str().map(str::to_owned),
            kind: kind.into(),
            input: item[if kind == "custom" {
                "input"
            } else {
                "arguments"
            }]
            .as_str()?
            .into(),
            details: None,
        })
    }

    pub(crate) fn required_input(&self) -> Option<Value> {
        let kind = match self.kind.as_str() {
            "function" => {
                return Some(
                    json!({"type":"function_call_output","call_id":self.call_id,"name":self.name}),
                );
            }
            "custom" => "custom_tool_call_output",
            "computer" => "computer_call_output",
            "shell" => "shell_call_output",
            "apply_patch" => "apply_patch_call_output",
            _ => return None,
        };
        Some(json!({"type":kind,"call_id":self.call_id}))
    }
}

fn complete_call(item: &Value, status: provider::responses::TerminalStatus) -> bool {
    item["status"] == "completed"
        || (item["status"].is_null() && status == provider::responses::TerminalStatus::Completed)
}

pub(crate) fn validate_returns(
    expected: &[GeneralPendingCall],
    input: &[Value],
) -> std::result::Result<(), &'static str> {
    let mut seen = BTreeMap::new();
    for item in input.iter().filter(|item| {
        matches!(
            item["type"].as_str(),
            Some("function_call_output" | "custom_tool_call_output")
        ) || crate::client_tool_items::is_output(item)
    }) {
        let id = if crate::client_tool_items::is_output(item) {
            crate::client_tool_items::result_id(item)
        } else {
            item["call_id"].as_str()
        }
        .ok_or("invalid_tool_output")?;
        if seen.insert(id, ()).is_some() {
            return Err("duplicate_tool_output");
        }
        let call = expected
            .iter()
            .find(|call| call.call_id == id)
            .ok_or("unexpected_tool_output")?;
        if let Some(details) = call
            .details
            .as_ref()
            .filter(|details| crate::client_tool_items::is_call(details))
        {
            crate::client_tool_items::validate_result(details, item)?;
            continue;
        }
        let required = if call.kind == "custom" {
            "custom_tool_call_output"
        } else {
            "function_call_output"
        };
        if item["type"] != required {
            return Err("tool_output_type_mismatch");
        }
    }
    if expected
        .iter()
        .any(|call| !seen.contains_key(call.call_id.as_str()))
    {
        return Err("missing_tool_output");
    }
    Ok(())
}

pub(crate) fn declared(request: &crate::responses_wire::ResponseRequest, item: &Value) -> bool {
    let Ok(tools) = request.native_tool_items() else {
        return false;
    };
    if crate::client_tool_items::is_call(item) {
        return tools
            .iter()
            .any(|tool| crate::client_tool_items::matches_declaration(tool, item));
    }
    let kind = if item["type"] == "custom_tool_call" {
        "custom"
    } else {
        "function"
    };
    tools.iter().any(|tool| {
        if let Some(namespace) = item["namespace"].as_str() {
            tool["type"] == "namespace"
                && tool["name"] == namespace
                && tool["tools"].as_array().is_some_and(|tools| {
                    tools
                        .iter()
                        .any(|tool| tool["type"] == kind && tool["name"] == item["name"])
                })
        } else {
            tool["type"] == kind && tool["name"] == item["name"]
        }
    })
}

pub(super) fn validate_policy(
    completion: &Completion,
    request: &crate::responses_wire::ResponseRequest,
    first_round: bool,
) -> Result<()> {
    let turn = completion
        .native
        .as_ref()
        .ok_or_else(|| Error::RustError("native_turn_missing".into()))?;
    let calls = turn
        .output
        .iter()
        .filter(|item| call_item(item))
        .collect::<Vec<_>>();
    let refusal = turn.output.iter().any(|item| {
        item["content"]
            .as_array()
            .is_some_and(|parts| parts.iter().any(|part| part["type"] == "refusal"))
    });
    let violation = request.tool_choice.as_ref().is_some_and(|choice| {
        if choice == "none" {
            return !calls.is_empty();
        }
        if !first_round || refusal || turn.status != provider::responses::TerminalStatus::Completed
        {
            return false;
        }
        if choice == "required" {
            return calls.is_empty();
        }
        if matches!(choice["type"].as_str(), Some("function" | "custom")) {
            return calls.is_empty()
                || calls.iter().any(|call| {
                    call["name"] != choice["name"]
                        || call.get("namespace") != choice.get("namespace")
                });
        }
        if let Some(family) = choice["type"]
            .as_str()
            .and_then(crate::client_tool_items::Family::choice)
        {
            return calls.is_empty()
                || calls.iter().any(|call| {
                    call["type"]
                        .as_str()
                        .and_then(crate::client_tool_items::Family::call)
                        != Some(family)
                });
        }
        false
    });
    if violation {
        return Err(Error::RustError(
            "provider_tool_choice_policy_violation".into(),
        ));
    }
    if request.parallel_tool_calls == Some(false) && calls.len() > 1 {
        return Err(Error::RustError(
            "provider_parallel_tool_policy_violation".into(),
        ));
    }
    let mut ids = BTreeMap::new();
    let declarations = request.native_tool_items().map_err(Error::RustError)?;
    for call in &calls {
        direct_caller(call)?;
        let call_id = call["call_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Error::RustError("invalid_native_tool_call".into()))?;
        if ids.insert(call_id, ()).is_some() {
            return Err(Error::RustError("duplicate_native_call_id".into()));
        }
        if !complete_call(call, turn.status) {
            continue;
        }
        if crate::client_tool_items::is_call(call) {
            if !declared(request, call) {
                return Err(Error::RustError(
                    "undeclared_native_client_tool_call".into(),
                ));
            }
            crate::client_tool_items::public_item(call).map_err(Error::RustError)?;
            continue;
        }
        let namespace = call["namespace"].as_str();
        let kind = if call["type"] == "custom_tool_call" {
            "custom"
        } else {
            "function"
        };
        let declaration = declarations.iter().find_map(|tool| {
            if let Some(namespace) = namespace {
                if tool["type"] != "namespace" || tool["name"] != namespace {
                    return None;
                }
                tool["tools"]
                    .as_array()?
                    .iter()
                    .find(|tool| tool["type"] == kind && tool["name"] == call["name"])
            } else if tool["type"] == kind && tool["name"] == call["name"] {
                Some(tool)
            } else {
                None
            }
        });
        require_direct_permission(declaration.and_then(|tool| tool.get("allowed_callers")))?;
        if kind == "custom" || namespace.is_some() {
            if declaration.is_none() {
                return Err(Error::RustError("undeclared_native_tool_call".into()));
            }
        }
        if kind == "custom" {
            if !call["input"].is_string() {
                return Err(Error::RustError("invalid_custom_tool_input".into()));
            }
            continue;
        }
        let arguments: Value = serde_json::from_str(
            call["arguments"]
                .as_str()
                .ok_or_else(|| Error::RustError("invalid_function_arguments".into()))?,
        )
        .map_err(|_| Error::RustError("invalid_function_arguments".into()))?;
        if let Some(tool) = declaration.filter(|tool| tool["strict"] == true) {
            crate::responses_schema::validate_instance(&tool["parameters"], &arguments)
                .map_err(|_| Error::RustError("invalid_function_arguments".into()))?;
        }
    }
    if !calls.is_empty() || refusal || turn.status != provider::responses::TerminalStatus::Completed
    {
        return Ok(());
    }
    if let Some(format) = request.text.as_ref().and_then(|text| text.get("format")) {
        if matches!(format["type"].as_str(), Some("json_object" | "json_schema")) {
            let value: Value =
                serde_json::from_str(completion.message.content.as_str().unwrap_or(""))
                    .map_err(|_| Error::RustError("invalid_structured_output".into()))?;
            if format["type"] == "json_object" && !value.is_object() {
                return Err(Error::RustError("invalid_structured_output".into()));
            }
            if format["type"] == "json_schema" {
                crate::responses_schema::validate_instance(&format["schema"], &value)
                    .map_err(|_| Error::RustError("invalid_structured_output".into()))?;
            }
        }
    }
    Ok(())
}

fn canonical_item(source: &Value, id: &str) -> Result<Value> {
    let mut item = source.clone();
    let fields = item
        .as_object_mut()
        .ok_or_else(|| Error::RustError("invalid_native_output".into()))?;
    fields.insert("id".into(), json!(id));
    // Use one typed allowlist for incremental events and final output. Unknown
    // vendor extensions are private even when their field names look harmless.
    provider::responses::public_item(&item)
        .map_err(Error::RustError)?
        .ok_or_else(|| Error::RustError("native_output_binding_required".into()))
}

fn events(item: &Value, index: usize) -> Vec<(&'static str, Value)> {
    let id = &item["id"];
    let mut added = item.clone();
    if added.get("status").is_some() {
        added["status"] = json!("in_progress");
    }
    let field = match item["type"].as_str() {
        Some("message") => Some("content"),
        Some("reasoning") => Some("summary"),
        _ => None,
    };
    if let Some(field) = field {
        added[field] = json!([]);
    }
    for field in ["arguments", "input"] {
        if added.get(field).is_some() {
            added[field] = json!("");
        }
    }
    let mut result = vec![(
        "response.output_item.added",
        json!({"output_index":index,"item":added}),
    )];
    match item["type"].as_str() {
        Some("message") => {
            for (part_index, part) in item["content"].as_array().into_iter().flatten().enumerate() {
                let mut start = part.clone();
                match part["type"].as_str() {
                    Some("output_text") => {
                        start["text"] = json!("");
                        start["annotations"] = json!([]);
                        start["logprobs"] = json!([]);
                    }
                    Some("refusal") => start["refusal"] = json!(""),
                    _ => {}
                }
                result.push(("response.content_part.added", json!({"item_id":id,"output_index":index,"content_index":part_index,"part":start})));
                match part["type"].as_str() {
                    Some("output_text") => {
                        let logprobs = part.get("logprobs").cloned().unwrap_or_else(|| json!([]));
                        result.push(("response.output_text.delta", json!({"item_id":id,"output_index":index,"content_index":part_index,"delta":part["text"],"logprobs":logprobs})));
                        for (annotation_index, annotation) in part["annotations"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .enumerate()
                        {
                            result.push(("response.output_text.annotation.added", json!({"item_id":id,"output_index":index,"content_index":part_index,"annotation_index":annotation_index,"annotation":annotation})));
                        }
                        result.push(("response.output_text.done", json!({"item_id":id,"output_index":index,"content_index":part_index,"text":part["text"],"logprobs":logprobs})));
                    }
                    Some("refusal") => {
                        result.push(("response.refusal.delta", json!({"item_id":id,"output_index":index,"content_index":part_index,"delta":part["refusal"]})));
                        result.push(("response.refusal.done", json!({"item_id":id,"output_index":index,"content_index":part_index,"refusal":part["refusal"]})));
                    }
                    _ => {}
                }
                result.push(("response.content_part.done", json!({"item_id":id,"output_index":index,"content_index":part_index,"part":part})));
            }
        }
        Some("reasoning") => {
            for (summary_index, part) in
                item["summary"].as_array().into_iter().flatten().enumerate()
            {
                result.push(("response.reasoning_summary_part.added", json!({"item_id":id,"output_index":index,"summary_index":summary_index,"part":{"type":"summary_text","text":""}})));
                result.push(("response.reasoning_summary_text.delta", json!({"item_id":id,"output_index":index,"summary_index":summary_index,"delta":part["text"]})));
                result.push(("response.reasoning_summary_text.done", json!({"item_id":id,"output_index":index,"summary_index":summary_index,"text":part["text"]})));
                result.push(("response.reasoning_summary_part.done", json!({"item_id":id,"output_index":index,"summary_index":summary_index,"part":part})));
            }
        }
        Some("function_call") => {
            result.push((
                "response.function_call_arguments.delta",
                json!({"item_id":id,"output_index":index,"delta":item["arguments"]}),
            ));
            result.push(("response.function_call_arguments.done", json!({"item_id":id,"output_index":index,"arguments":item["arguments"],"name":item["name"]})));
        }
        Some("custom_tool_call") => {
            result.push((
                "response.custom_tool_call_input.delta",
                json!({"item_id":id,"output_index":index,"delta":item["input"]}),
            ));
            result.push((
                "response.custom_tool_call_input.done",
                json!({"item_id":id,"output_index":index,"input":item["input"]}),
            ));
        }
        _ => {}
    }
    result.push((
        "response.output_item.done",
        json!({"output_index":index,"item":item}),
    ));
    result
}

impl SessionInner {
    pub(super) fn native_semantic_delta(&self, delta: &provider::responses::SemanticDelta) {
        if let Err(error) = self.publish_native_delta(delta.event_type(), &delta.payload) {
            if let Some(response) = self
                .data
                .borrow_mut()
                .as_mut()
                .and_then(|data| data.run.as_mut())
                .and_then(|run| run.response.as_mut())
            {
                response
                    .stream_error
                    .get_or_insert_with(|| error.to_string());
            }
        }
    }

    fn annotate_search_message(&self, item: &mut Value) {
        let data = self.data.borrow();
        if let Some(response) = data
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
        {
            crate::file_ingest::annotate_message(item, &response.output);
        }
    }

    fn annotate_search_part(&self, part: &mut Value) -> Vec<Value> {
        let data = self.data.borrow();
        data.as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .map(|response| crate::file_ingest::annotate_part(part, &response.output))
            .unwrap_or_default()
    }

    fn publish_native_delta(&self, kind: &str, payload: &Value) -> Result<()> {
        let mapped = {
            let mut data = self.data.borrow_mut();
            let Some(run) = data.as_mut().and_then(|data| data.run.as_mut()) else {
                return Ok(());
            };
            let Some(response) = run.response.as_mut() else {
                return Ok(());
            };
            if response.stream_error.is_some() {
                return Ok(());
            }
            response.native_stream.map(
                run.round,
                response.output.len(),
                &response.id,
                &response.request,
                kind,
                payload,
            )?
        };
        if !mapped.visible
            || (kind == "response.output_item.done" && mapped.deferred_done)
            || matches!(
                kind,
                "response.function_call_arguments.done" | "response.custom_tool_call_input.done"
            )
            || (mapped.buffered_input && kind == "response.custom_tool_call_input.delta")
        {
            return Ok(());
        }
        let mut fields = payload.clone();
        fields["output_index"] = json!(mapped.index);
        if fields.get("item_id").is_some() {
            fields["item_id"] = json!(mapped.id);
        }
        if let Some(item) = fields.get_mut("item") {
            *item = canonical_item(item, &mapped.id)?;
            if kind == "response.output_item.done" {
                self.annotate_search_message(item);
            }
        }
        if kind == "response.content_part.done" {
            if let Some(part) = fields.get_mut("part") {
                let first = part["annotations"].as_array().map_or(0, Vec::len);
                let added = self.annotate_search_part(part);
                for (offset, annotation) in added.into_iter().enumerate() {
                    self.response_event("response.output_text.annotation.added", json!({"item_id":mapped.id,"output_index":mapped.index,"content_index":fields["content_index"],"annotation_index":first+offset,"annotation":annotation}))?;
                }
            }
        }
        self.response_event(kind, fields)
    }

    pub(super) async fn project_native_output(&self, completion: &Completion) -> Result<bool> {
        let Some(turn) = &completion.native else {
            return Ok(false);
        };
        if let Some(response) = self
            .data
            .borrow_mut()
            .as_mut()
            .and_then(|data| data.run.as_mut())
            .and_then(|run| run.response.as_mut())
        {
            response.effective_service_tier = turn.effective_service_tier.clone();
        }
        let (response_id, request, mut index) = {
            let data = self.data.borrow();
            let Some(response) = data
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .and_then(|run| run.response.as_ref())
            else {
                return Ok(false);
            };
            (
                response.id.clone(),
                response.request.clone(),
                response.output.len(),
            )
        };
        let mut plan = {
            let mut data = self.data.borrow_mut();
            data.as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .plan_output()?
        };
        let mut streams = vec![];
        for (ordinal, raw) in turn.output.iter().enumerate() {
            if call_item(raw) && !declared(&request, raw) {
                continue;
            }
            let id = format!(
                "item_{response_id}_{}_{}",
                self.data
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .run
                    .as_ref()
                    .unwrap()
                    .round
                    .saturating_sub(1),
                ordinal
            );
            let mut item = canonical_item(raw, &id)?;
            if raw["type"] == "reasoning" {
                item["encrypted_content"] = json!(self.native_item_token(&id).await?);
            }
            self.annotate_search_message(&mut item);
            let streamed = {
                let data = self.data.borrow();
                let run = data.as_ref().unwrap().run.as_ref().unwrap();
                let stream = &run.response.as_ref().unwrap().native_stream;
                if stream.round == run.round.saturating_sub(1) {
                    stream.items.get(&ordinal).cloned()
                } else {
                    None
                }
            };
            if streamed.as_ref().is_some_and(|streamed| {
                !streamed.done || streamed.index != plan.next_index() || streamed.id != id
            }) {
                return Err(Error::RustError("native_stream_output_mismatch".into()));
            }
            plan.push(item)
                .map_err(|code| Error::RustError(code.into()))?;
            streams.push(streamed);
        }
        for (change, streamed) in plan.finish().into_iter().zip(streams) {
            let item = change.item();
            if let Some(streamed) = streamed {
                if streamed.deferred_done {
                    for (kind, value) in completed_events(
                        item,
                        index,
                        streamed.buffered_input,
                        complete_call(item, turn.status),
                    ) {
                        self.response_event(kind, value)?;
                    }
                }
            } else {
                for (kind, value) in events(item, index) {
                    self.response_event(kind, value)?;
                }
            }
            {
                let mut data = self.data.borrow_mut();
                let response = data
                    .as_mut()
                    .unwrap()
                    .run
                    .as_mut()
                    .unwrap()
                    .response
                    .as_mut()
                    .unwrap();
                if let Some(call) =
                    GeneralPendingCall::from_item(item).filter(|_| complete_call(item, turn.status))
                {
                    response.general_pending_calls.push(call);
                }
                response.apply_output(change)?;
            }
            index += 1;
            worker::Delay::from(std::time::Duration::ZERO).await;
        }
        if let Some(reason) = turn
            .incomplete_details
            .as_ref()
            .and_then(|details| details["reason"].as_str())
        {
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .run
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
                .incomplete_reason = Some(reason.into());
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_native_item_family_uses_the_validated_completion_boundary() {
        let request = request();
        for kind in [
            "message",
            "reasoning",
            "tool_search_call",
            "future_tool_call",
        ] {
            let mut stream = NativeStream::default();
            stream
                .map(
                    0,
                    0,
                    "resp_ours",
                    &request,
                    "response.output_item.added",
                    &json!({"output_index":0,"item":{"type":kind}}),
                )
                .unwrap();
            assert!(
                stream
                    .map(
                        0,
                        0,
                        "resp_ours",
                        &request,
                        "response.output_item.done",
                        &json!({"output_index":0,"item":{"type":kind}})
                    )
                    .unwrap()
                    .deferred_done
            );
        }
    }

    #[test]
    fn provider_caller_json_cannot_establish_programmatic_authority() {
        let base =
            json!({"type":"function_call","name":"ordinary","call_id":"call","arguments":"{}"});
        assert!(direct_caller(&base).is_ok());
        for caller in [json!(null), json!({"type":"direct"})] {
            let mut call = base.clone();
            call["caller"] = caller;
            assert!(direct_caller(&call).is_ok());
        }
        for caller in [
            json!({"type":"program","caller_id":"forged"}),
            json!({"type":"programmatic"}),
            json!({}),
        ] {
            let mut call = base.clone();
            call["caller"] = caller;
            assert!(direct_caller(&call).is_err());
        }
    }

    #[test]
    fn ordinary_function_caller_policy_is_enforced_without_a_discovery_registry() {
        let request: crate::responses_wire::ResponseRequest = serde_json::from_value(json!({"model":"agent","input":"hello","tools":[
            {"type":"function","name":"ordinary","parameters":{"type":"object"},"allowed_callers":["programmatic"]}
        ]})).unwrap();
        let call = json!({"type":"function_call","id":"item","call_id":"call","name":"ordinary","arguments":"{}","status":"completed"});
        let completion: Completion = serde_json::from_value(json!({"id":"provider","model":"native",
            "message":Message::text("assistant", ""),"finish_reason":"tool_calls","usage":Usage::default(),
            "native":{"provider_response_id":"provider","status":"completed","output":[call],"usage_details":null,"cache_write_tokens":null,"error":null,"incomplete_details":null}
        })).unwrap();
        request.validate().unwrap();
        assert!(validate_policy(&completion, &request, true).is_err());
        let mut direct_request = request;
        direct_request.tools[0].options.remove("allowed_callers");
        assert!(validate_policy(&completion, &direct_request, true).is_ok());
        let mut forged = completion;
        forged.native.as_mut().unwrap().output[0]["caller"] =
            json!({"type":"program","caller_id":"no-runtime-proof"});
        assert!(validate_policy(&forged, &direct_request, true).is_err());
    }

    fn request() -> crate::responses_wire::ResponseRequest {
        serde_json::from_value(json!({"model":"agent","input":"hello","tools":[
            {"type":"custom","name":"plain"},
            {"type":"namespace","name":"db","description":"Database operations","tools":[{"type":"custom","name":"sql"},{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"id":{"type":"integer"}},"required":["id"],"additionalProperties":false},"strict":true}]}
        ]})).unwrap()
    }

    #[test]
    fn stream_identity_and_public_indexes_ignore_private_harness_calls() {
        let request = request();
        let mut stream = NativeStream::default();
        let internal = stream
            .map(
                2,
                4,
                "resp_ours",
                &request,
                "response.output_item.added",
                &json!({"output_index":0,"item":{"type":"function_call","name":"bash"}}),
            )
            .unwrap();
        assert!(!internal.visible);
        let message = stream
            .map(
                2,
                4,
                "resp_ours",
                &request,
                "response.output_item.added",
                &json!({"output_index":1,"item":{"type":"message","id":"provider"}}),
            )
            .unwrap();
        assert_eq!(message.index, 4);
        assert_eq!(message.id, "item_resp_ours_2_1");
        let delta = stream
            .map(
                2,
                4,
                "resp_ours",
                &request,
                "response.output_text.delta",
                &json!({"output_index":1,"delta":"first"}),
            )
            .unwrap();
        assert_eq!(delta.id, message.id);
        assert!(!delta.done);
        assert!(
            stream
                .map(
                    2,
                    4,
                    "resp_ours",
                    &request,
                    "response.output_item.done",
                    &json!({"output_index":1})
                )
                .unwrap()
                .done
        );
        assert!(
            stream
                .map(
                    2,
                    4,
                    "resp_ours",
                    &request,
                    "response.output_text.delta",
                    &json!({"output_index":1})
                )
                .is_err()
        );
        let next = stream
            .map(
                3,
                5,
                "resp_ours",
                &request,
                "response.output_item.added",
                &json!({"output_index":0,"item":{"type":"reasoning"}}),
            )
            .unwrap();
        assert_eq!(next.index, 5);
        assert_eq!(next.id, "item_resp_ours_3_0");
    }

    #[test]
    fn namespaces_are_exact_and_custom_policy_uses_native_calls() {
        let mut request = request();
        let call = json!({"type":"custom_tool_call","call_id":"c1","name":"sql","namespace":"db","input":"select 1"});
        assert!(declared(&request, &call));
        assert!(!declared(
            &request,
            &json!({"type":"custom_tool_call","name":"sql"})
        ));
        let completion = Completion {
            id: "provider".into(),
            model: "native".into(),
            message: Message::text("assistant", ""),
            finish_reason: "stop".into(),
            usage: Usage::default(),
            native: Some(provider::responses::NativeTurn {
                provider_response_id: "provider".into(),
                status: provider::responses::TerminalStatus::Completed,
                output: vec![call],
                usage_details: None,
                cache_write_tokens: None,
                effective_service_tier: None,
                error: None,
                incomplete_details: None,
            }),
        };
        request.tool_choice = Some(json!("required"));
        assert!(validate_policy(&completion, &request, true).is_ok());
        request.tool_choice = Some(json!("none"));
        assert!(validate_policy(&completion, &request, true).is_err());
    }
    #[test]
    fn reasoning_is_private_but_summary_and_canonical_identity_survive() {
        let item = canonical_item(&json!({"type":"reasoning","id":"provider","summary":[{"type":"summary_text","text":"Plan"}],"content":[{"text":"private"}],"encrypted_content":"secret"}), "ours").unwrap();
        assert_eq!(item["id"], "ours");
        assert!(item.get("content").is_none());
        assert!(item.get("encrypted_content").is_none());
        let events = events(&item, 2);
        assert_eq!(events[1].0, "response.reasoning_summary_part.added");
        assert_eq!(events.last().unwrap().0, "response.output_item.done");
        assert!(!serde_json::to_string(&events).unwrap().contains("private"));
    }
    #[test]
    fn message_parts_keep_annotations_logprobs_refusal_and_phase() {
        let item = canonical_item(&json!({"type":"message","phase":"final_answer","content":[{"type":"output_text","text":"hello","annotations":[{"type":"url_citation","url":"https://example.com"}],"logprobs":[{"token":"hello","logprob":-0.1}]},{"type":"refusal","refusal":"no"}]}), "ours").unwrap();
        assert_eq!(item["phase"], "final_answer");
        let events = events(&item, 0);
        assert!(
            events
                .iter()
                .any(|(kind, _)| *kind == "response.output_text.annotation.added")
        );
        assert!(
            events
                .iter()
                .any(|(kind, _)| *kind == "response.refusal.done")
        );
    }
    #[test]
    fn custom_returns_require_exact_type_and_single_owned_call() {
        let call = GeneralPendingCall::from_item(&json!({"type":"custom_tool_call","id":"item","call_id":"call","name":"sql","namespace":"db","input":"SELECT 1"})).unwrap();
        assert!(
            validate_returns(
                std::slice::from_ref(&call),
                &[json!({"type":"custom_tool_call_output","call_id":"call","output":"1"})]
            )
            .is_ok()
        );
        assert_eq!(
            validate_returns(
                std::slice::from_ref(&call),
                &[json!({"type":"function_call_output","call_id":"call"})]
            ),
            Err("tool_output_type_mismatch")
        );
        assert_eq!(validate_returns(&[call], &[]), Err("missing_tool_output"));
    }
}
