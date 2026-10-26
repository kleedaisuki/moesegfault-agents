//! Canonical hosted output publication, independent of a particular service runtime.
use super::*;
use sha2::{Digest, Sha256};

#[path = "session_hosted_code.rs"]
mod code;

pub(super) enum HostedDispatch {
    Ready(Value),
    Pending,
}

pub(super) fn hosted_item_id(execution: &str) -> String {
    format!("hosted_{:x}", Sha256::digest(execution.as_bytes()))
}

pub(super) fn permanent_publication_error(error: &Error) -> bool {
    matches!(error, Error::RustError(code) if matches!(code.as_str(),
        "response_output_limit" | "invalid_canonical_output_id" | "invalid_canonical_output" | "canonical_output_changed"))
}

fn phase_event(kind: &str, terminal: bool) -> Option<&'static str> {
    match (kind, terminal) {
        ("code_interpreter_call", false) => Some("response.code_interpreter_call.in_progress"),
        ("code_interpreter_call", true) => Some("response.code_interpreter_call.completed"),
        ("image_generation_call", false) => Some("response.image_generation_call.in_progress"),
        ("image_generation_call", true) => Some("response.image_generation_call.completed"),
        ("web_search_call", false) => Some("response.web_search_call.in_progress"),
        ("web_search_call", true) => Some("response.web_search_call.completed"),
        ("file_search_call", false) => Some("response.file_search_call.in_progress"),
        ("file_search_call", true) => Some("response.file_search_call.completed"),
        _ => None,
    }
}

fn initial_item(item: &Value) -> Value {
    let mut initial = item.clone();
    if item.get("status").is_some() {
        initial["status"] = json!("in_progress");
    }
    match item["type"].as_str() {
        Some("code_interpreter_call") => {
            initial["code"] = json!("");
            initial["outputs"] = json!([]);
        }
        Some("image_generation_call") => initial["result"] = Value::Null,
        Some("file_search_call") => initial["results"] = Value::Null,
        _ => {}
    }
    initial
}

fn accepts_terminal_update(item: &Value) -> bool {
    item["status"] == "in_progress"
        || (item["type"] == "code_interpreter_call" && item["status"] == "interpreting")
}

fn code_start_events(item: &Value, index: usize) -> Vec<(&'static str, Value)> {
    if item["type"] != "code_interpreter_call" {
        return vec![];
    }
    let Some(code) = item["code"].as_str() else {
        return vec![];
    };
    let mut events = vec![
        (
            "response.code_interpreter_call.in_progress",
            json!({"item_id":item["id"],"output_index":index}),
        ),
        (
            "response.code_interpreter_call_code.delta",
            json!({"item_id":item["id"],"output_index":index,"delta":code}),
        ),
        (
            "response.code_interpreter_call_code.done",
            json!({"item_id":item["id"],"output_index":index,"code":code}),
        ),
    ];
    if matches!(item["status"].as_str(), Some("interpreting" | "completed")) {
        events.push((
            "response.code_interpreter_call.interpreting",
            json!({"item_id":item["id"],"output_index":index}),
        ));
    }
    events
}

impl SessionInner {
    pub(super) async fn publish_hosted_item(&self, item: &Value, terminal: bool) -> Result<()> {
        self.publish_tool_item(item, terminal, initial_item(item), |index, is_new| {
            let mut events = if is_new {
                code_start_events(item, index)
            } else {
                vec![]
            };
            let kind = item["type"].as_str().unwrap_or("");
            if let Some(event) =
                phase_event(kind, terminal).filter(|_| kind != "code_interpreter_call" || terminal)
            {
                events.push((event, json!({"item_id":item["id"],"output_index":index})));
            }
            events
        })
        .await
    }

    pub(super) async fn dispatch_code_tool(
        &self,
        record: &SessionRecord,
        execution: &str,
        call: &provider::ToolCall,
    ) -> Result<Option<HostedDispatch>> {
        if crate::hosted_tools::Kind::from_alias(&call.function.name)
            != Some(crate::hosted_tools::Kind::CodeInterpreter)
        {
            return Ok(None);
        }
        let binding = self
            .snapshot()?
            .run
            .as_ref()
            .and_then(|run| run.response.as_ref())
            .and_then(|response| response.bound_tools.lookup(&call.function.name))
            .cloned()
            .filter(|binding| binding.allows(crate::hosted_bindings::Caller::Direct))
            .ok_or_else(|| Error::RustError("hosted_tool_not_bound".into()))?;
        self.dispatch_hosted_code(record, execution, call, &binding)
            .await
            .map(Some)
    }

    pub(super) async fn drain_cancelled_code(&self) -> Result<bool> {
        let data = self.snapshot()?;
        let Some(run) = data.run.as_ref() else {
            return Ok(false);
        };
        let Some(call) = next_tool(&data.messages, run.response.as_ref()) else {
            return Ok(false);
        };
        let execution = execution_key(&run.id, run.round, &call.id);
        match self
            .dispatch_code_tool(&data.record, &execution, &call)
            .await?
        {
            None => Ok(false),
            Some(HostedDispatch::Pending) => Ok(true),
            Some(HostedDispatch::Ready(output)) => {
                let message = Message::tool_result_content(&call.id, output);
                {
                    let mut slot = self.data.borrow_mut();
                    let current = slot
                        .as_mut()
                        .ok_or_else(|| Error::RustError("session_not_initialized".into()))?;
                    current.messages.push(message.clone());
                    current.journal.push(message);
                }
                self.persist().await?;
                Ok(false)
            }
        }
    }

    /// Commit canonical items/events before exposing them to any transport.
    /// Re-entering a published operation does not append another copy or event.
    pub(super) async fn publish_tool_item(
        &self,
        item: &Value,
        terminal: bool,
        initial: Value,
        extra_events: impl FnOnce(usize, bool) -> Vec<(&'static str, Value)>,
    ) -> Result<()> {
        let id = item["id"]
            .as_str()
            .ok_or_else(|| Error::RustError("invalid_hosted_item".into()))?;
        self.reserve_response_events();
        let (response_id, events) = {
            let mut data = self.data.borrow_mut();
            let response = data
                .as_mut()
                .and_then(|data| data.run.as_mut())
                .and_then(|run| run.response.as_mut())
                .ok_or_else(|| Error::RustError("hosted_response_missing".into()))?;
            if response.deleted {
                // DELETE hides the canonical result, not a license to strand a
                // known paid result or an unacknowledged supervisor artifact.
                return Ok(());
            }
            let existing = response.output.iter().position(|value| value["id"] == id);
            if existing.is_some_and(|index| response.output[index] == *item) {
                // The prior persist may have failed after mutating warm memory.
                // A replay must prove durability before returning a tool result.
                drop(data);
                self.persist().await?;
                return Ok(());
            }
            if existing.is_some_and(|index| !accepts_terminal_update(&response.output[index])) {
                return Err(Error::RustError("hosted_item_identity_conflict".into()));
            }
            let index = existing.unwrap_or(response.output.len());
            let output_change = response.prepare_output(existing, item.clone())?;
            let mut events = Vec::new();
            if existing.is_none() {
                events.push(response.event(
                    "response.output_item.added",
                    json!({"output_index":index,"item":initial}),
                )?);
            }
            response.apply_output(output_change)?;
            for (kind, fields) in extra_events(index, existing.is_none()) {
                events.push(response.event(kind, fields)?);
            }
            if terminal {
                events.push(response.event(
                    "response.output_item.done",
                    json!({"output_index":index,"item":item}),
                )?);
            }
            (response.id.clone(), events)
        };
        {
            let mut barrier = self.response_publication_barrier.borrow_mut();
            for event in events {
                barrier.begin(&response_id, &event)?;
            }
        }
        // persist() flushes the existing shared publication barrier only after
        // canonical state is durable. A failed write retains its outbox in warm
        // memory; an identical-item retry persists and flushes that same outbox.
        self.persist().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpreting_code_accepts_one_terminal_transition_not_another_execution() {
        for status in ["in_progress", "interpreting"] {
            assert!(accepts_terminal_update(
                &json!({"type":"code_interpreter_call","status":status})
            ));
        }
        for status in ["completed", "failed", "incomplete"] {
            assert!(!accepts_terminal_update(
                &json!({"type":"code_interpreter_call","status":status})
            ));
        }
        assert!(!accepts_terminal_update(
            &json!({"type":"image_generation_call","status":"interpreting"})
        ));
    }

    #[test]
    fn code_lifecycle_uses_current_sdk_event_names_and_real_source() {
        let source = "print('你好 🐈')\n";
        let item = json!({"id":"ci_known","type":"code_interpreter_call","code":source,
            "container_id":"cntr_real","status":"interpreting","outputs":[]});
        let events = code_start_events(&item, 3);
        assert_eq!(
            events.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            vec![
                "response.code_interpreter_call.in_progress",
                "response.code_interpreter_call_code.delta",
                "response.code_interpreter_call_code.done",
                "response.code_interpreter_call.interpreting"
            ]
        );
        assert_eq!(events[1].1["delta"], source);
        assert_eq!(events[2].1["code"], source);
        assert!(
            events
                .iter()
                .all(|(_, fields)| fields["item_id"] == "ci_known" && fields["output_index"] == 3)
        );
        let mut failed = item;
        failed["status"] = json!("failed");
        assert_eq!(code_start_events(&failed, 3).len(), 3);
        assert!(code_start_events(&json!({"type":"mcp_call"}), 3).is_empty());
    }

    #[test]
    fn only_defined_hosted_phase_events_are_published() {
        assert!(phase_event("web_search_call", true).is_some());
        for kind in [
            "tool_search_call",
            "tool_search_output",
            "mcp_approval_request",
        ] {
            assert!(phase_event(kind, false).is_none());
            assert!(phase_event(kind, true).is_none());
        }
    }

    #[test]
    fn initial_items_do_not_duplicate_final_logs_images_or_invent_status_fields() {
        let code = initial_item(
            &json!({"id":"item","type":"code_interpreter_call","status":"completed","code":"print(1)","outputs":[{"type":"logs","logs":"1"}]}),
        );
        assert_eq!(code["code"], "");
        assert_eq!(code["outputs"], json!([]));
        let approval = json!({"id":"approval","type":"mcp_approval_request","name":"remote"});
        assert_eq!(initial_item(&approval), approval);
    }
}
