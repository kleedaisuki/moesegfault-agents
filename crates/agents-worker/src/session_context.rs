//! Read-only inference views of retained Response boundaries.
//!
//! Native items exclude the active protected prefix. Legacy messages include a
//! freshly rendered prefix; response-scoped guidance is returned separately so
//! continuation/count callers cannot accidentally inherit it.

use super::*;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ResponseContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<crate::tool_discovery::ToolRegistry>,
    #[serde(default)]
    pub general_pending_calls: Vec<super::native_output::GeneralPendingCall>,
    pub record: SessionRecord,
    pub response_id: String,
    pub instructions: String,
    pub response_instructions: Option<String>,
    pub messages: Vec<Message>,
    pub pending: Vec<Message>,
    pub history_instructions: Vec<Message>,
    pub native_items: Option<Vec<Value>>,
    pub native_provider_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_updates: Vec<usize>,
    pub pending_calls: Vec<provider::ToolCall>,
    pub source_end: usize,
    pub compacted: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextRequest {
    response_id: String,
}

struct ContextView {
    messages: Vec<Message>,
    pending: Vec<Message>,
    history_instructions: Vec<Message>,
    native: Option<branch::NativeHistoryRef>,
    pending_calls: Vec<provider::ToolCall>,
    source_end: usize,
    compacted: bool,
}

fn retained(response: &ResponseRun) -> bool {
    response.request.store && !response.deleted
}

fn terminal(response: &ResponseRun) -> bool {
    matches!(
        response.status.as_str(),
        "completed" | "failed" | "incomplete" | "cancelled"
    )
}

fn quiet_head(data: &PersistedSession, response: &ResponseRun) -> bool {
    data.response_head.as_deref() == Some(response.id.as_str())
        && data.run.is_none()
        && data.pending.is_empty()
        && !data.archive_pending
        && !data.archive_preparing
        && data.record.status != SessionStatus::Running
}

fn historical_prefix(record: &SessionRecord, instructions: &[Message]) -> String {
    let mut prefix = system_prefix(record);
    for instruction in instructions {
        prefix.push_str(&format!(
            "\n\nHistorical {} instruction:\n{}",
            instruction.role,
            instruction.content.as_str().unwrap_or("")
        ));
    }
    prefix
}

fn context_view(data: &PersistedSession, response: &ResponseRun) -> Result<ContextView> {
    let head = quiet_head(data, response);
    let (source_end, native, pending_calls) = match &response.branch_point {
        Some(point) => {
            if point.response_id != response.id
                || point.source_session_id != data.record.id
                || point.pin_hash != data.record.pinned.prefix_hash()
                || point.source_end > data.journal.len()
            {
                return Err(Error::RustError("response_context_boundary_corrupt".into()));
            }
            (
                point.source_end,
                point.native.clone(),
                point.pending_calls.clone(),
            )
        }
        None if head => (
            data.journal.len(),
            data.native_context
                .as_ref()
                .map(|context| context.history.clone()),
            response.awaiting_calls.clone(),
        ),
        None => {
            return Err(Error::RustError("response_history_unavailable".into()));
        }
    };
    let source = &data.journal[..source_end];
    let warm = head && source_end == data.journal.len();
    let history_instructions = if warm {
        data.history_instructions.clone()
    } else {
        source
            .iter()
            .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
            .cloned()
            .collect()
    };
    let (body, pending) = if warm {
        (data.messages.iter().skip(1).cloned().collect(), vec![])
    } else {
        branch::project_selected_history(
            source,
            response
                .branch_point
                .as_ref()
                .map_or(&[], |point| point.excluded_source_ranges.as_slice()),
        )?
    };
    let mut messages = Vec::with_capacity(body.len() + 1);
    messages.push(Message::text(
        "system",
        historical_prefix(&data.record, &history_instructions),
    ));
    messages.extend(body);
    Ok(ContextView {
        messages,
        pending,
        history_instructions,
        native,
        pending_calls,
        source_end,
        compacted: warm && data.compact_count > 0,
    })
}

impl SessionInner {
    pub(super) async fn response_context(&self, mut req: Request) -> Result<Response> {
        if req.method() != Method::Post {
            return util::json_error(405, "method_not_allowed", "Method not allowed");
        }
        let input: ContextRequest = match req.json().await {
            Ok(input) => input,
            Err(_) => {
                return util::json_error(400, "invalid_request", "A response_id is required");
            }
        };
        let _guard = self.persistence.lock().await;
        let Some(response) = self.read_response(&input.response_id).await? else {
            return util::json_error(404, "not_found", "Response not retained");
        };
        if !retained(&response) {
            return util::json_error(404, "not_found", "Response not retained");
        }
        if !terminal(&response) {
            return util::json_error(
                409,
                "parent_response_in_progress",
                "Wait until the source response is terminal",
            );
        }
        let selected = {
            let data = self.data.borrow();
            let data = data
                .as_ref()
                .ok_or_else(|| Error::RustError("session_not_initialized".into()))?;
            context_view(data, &response).map(|view| (data.record.clone(), view))
        };
        let (record, view) = match selected {
            Err(Error::RustError(code)) if code == "response_history_unavailable" => {
                return util::json_error(
                    409,
                    "response_history_unavailable",
                    "This response predates retained context boundaries",
                );
            }
            result => result?,
        };
        let native_items = match &view.native {
            Some(reference) => Some(self.native_export(reference).await?),
            None => None,
        };
        let context_revision = view.native.as_ref().and_then(|reference| {
            reference
                .context
                .as_ref()
                .map(|context| context.digest.clone())
        });
        let context_updates = view
            .native
            .as_ref()
            .map(super::native::context_positions)
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .map(|index| index - 1)
            .collect();
        let native_provider_binding = view.native.map(|reference| reference.provider_binding);
        Response::from_json(&ResponseContext {
            discovery: response.discovery.clone(),
            general_pending_calls: response.general_pending_calls.clone(),
            instructions: system_prefix(&record),
            record,
            response_id: response.id,
            response_instructions: response
                .effective_instructions
                .clone()
                .or(response.request.instructions),
            messages: view.messages,
            pending: view.pending,
            history_instructions: view.history_instructions,
            native_items,
            native_provider_binding,
            context_revision,
            context_updates,
            pending_calls: view.pending_calls,
            source_end: view.source_end,
            compacted: view.compacted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(data: &PersistedSession, end: usize) -> ResponseRun {
        let mut response: ResponseRun = serde_json::from_value(json!({
            "id":"resp_context", "request":{"model":data.record.agent_id,
                "input":"old input","instructions":"ONE_RESPONSE_ONLY"},
            "created_at":1,"input_items":[],"output":[],"events":[],
            "status":"completed","error":null,"incomplete_reason":null,
            "cancel_requested":false,"awaiting_calls":[],"usage":Usage::default(),
            "next_sequence":0,"stream_item":null,
        }))
        .unwrap();
        response.branch_point = Some(branch::BranchPoint {
            response_id: response.id.clone(),
            source_session_id: data.record.id.clone(),
            source_end: end,
            excluded_source_ranges: vec![],
            pin_hash: data.record.pinned.prefix_hash(),
            pending_calls: vec![],
            general_pending_calls: vec![],
            workspace: branch::WorkspaceSeed::Empty,
            native: None,
            discovery: None,
        });
        response
    }

    #[test]
    fn historical_view_excludes_future_source_and_response_guidance() {
        let mut data = super::super::tests::state();
        data.record.pinned.instructions = "FIXED_PREFIX".into();
        data.journal = vec![
            Message::text("developer", "HISTORICAL_POLICY"),
            Message::text("user", "past"),
            Message::text("assistant", "answer"),
            Message::text("user", "FUTURE_INPUT"),
        ];
        let response = response(&data, 3);
        let original = serde_json::to_value(&data.journal).unwrap();
        let view = context_view(&data, &response).unwrap();
        let rendered = serde_json::to_string(&view.messages).unwrap();
        assert!(rendered.contains("FIXED_PREFIX"));
        assert!(rendered.contains("HISTORICAL_POLICY"));
        assert!(!rendered.contains("ONE_RESPONSE_ONLY"));
        assert!(!rendered.contains("FUTURE_INPUT"));
        assert_eq!(serde_json::to_value(&data.journal).unwrap(), original);
        assert_eq!(view.source_end, 3);
        assert!(!view.compacted);
    }

    #[test]
    fn warm_view_keeps_compaction_but_rebuilds_protected_prefix() {
        let mut data = super::super::tests::state();
        data.record.pinned.instructions = "FIXED_PREFIX".into();
        data.journal = vec![
            Message::text("user", "past"),
            Message::text("assistant", "answer"),
        ];
        data.messages = vec![
            Message::text("system", "ONE_RESPONSE_ONLY"),
            Message::text("user", "COMPACTED_HISTORY"),
        ];
        data.compact_count = 1;
        let response = response(&data, 2);
        data.response_head = Some(response.id.clone());
        let view = context_view(&data, &response).unwrap();
        let rendered = serde_json::to_string(&view.messages).unwrap();
        assert!(view.compacted);
        assert!(rendered.contains("COMPACTED_HISTORY"));
        assert!(rendered.contains("FIXED_PREFIX"));
        assert!(!rendered.contains("ONE_RESPONSE_ONLY"));
    }

    #[test]
    fn old_response_boundary_is_inferred_only_for_a_quiet_current_head() {
        let mut data = super::super::tests::state();
        let mut response = response(&data, 0);
        response.branch_point = None;
        assert!(context_view(&data, &response).is_err());
        data.response_head = Some(response.id.clone());
        assert_eq!(context_view(&data, &response).unwrap().source_end, 0);
        data.pending.push(Message::text("user", "later steering"));
        assert!(context_view(&data, &response).is_err());
    }

    #[test]
    fn historical_handoff_steering_remains_pending_not_invalid_adjacency() {
        let mut data = super::super::tests::state();
        let mut assistant = Message::text("assistant", "");
        assistant.tool_calls = serde_json::from_value(json!([{
            "id":"call_context","type":"function","function":{"name":"local","arguments":"{}"}
        }]))
        .unwrap();
        data.journal = vec![assistant, Message::text("user", "queued steering")];
        let mut response = response(&data, 2);
        response.branch_point.as_mut().unwrap().pending_calls = data.journal[0].tool_calls.clone();
        let view = context_view(&data, &response).unwrap();
        assert_eq!(view.messages.len(), 2);
        assert_eq!(view.messages[1].role, "assistant");
        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.pending[0].content, "queued steering");
        assert_eq!(view.pending_calls[0].id, "call_context");
    }
}
