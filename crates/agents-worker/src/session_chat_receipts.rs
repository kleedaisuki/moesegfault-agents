//! Immutable completion rendezvous for repairing an interrupted Chat observer.

use super::*;

const PREFIX: &str = "chat:terminal:";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u8,
    run_id: String,
    source_begin: usize,
    source_end: usize,
    final_message: Option<usize>,
    completion: Option<Value>,
    error: Option<String>,
    terminal: Value,
}

fn unavailable() -> Error {
    Error::RustError("chat_observation_unavailable".into())
}

fn known_unavailable(error: &Error) -> bool {
    matches!(error, Error::RustError(code) if code == "chat_observation_unavailable")
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("run_")
        .is_some_and(|tail| tail.len() == 32 && tail.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

impl Receipt {
    fn prepare(
        run: &RunState,
        result: &CompletionResult,
        journal: &[Message],
        model: &str,
    ) -> Result<Option<Self>> {
        let Some(begin) = run.source_begin.filter(|_| run.response.is_none()) else {
            // Old in-flight runs must still finish; their observation span is unknown.
            return Ok(None);
        };
        if begin > journal.len() {
            return Err(unavailable());
        }
        let (completion, final_message, error) = match result {
            Ok(value) => {
                let message = value
                    .pointer("/choices/0/message")
                    .ok_or_else(unavailable)?;
                let index = journal[begin..]
                    .iter()
                    .rposition(|entry| entry.role == "assistant")
                    .map(|index| begin + index)
                    .ok_or_else(unavailable)?;
                if !matches_message(&journal[index], message)? || value["id"] != run.id {
                    return Err(unavailable());
                }
                let metadata = completion_metadata(value)?;
                (Some(metadata), Some(index), None)
            }
            Err(error) => {
                if error.is_empty()
                    || error.len() > 128
                    || !error
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    // Do not expand an old error representation or block its original run.
                    return Ok(None);
                }
                (None, None, Some(error.clone()))
            }
        };
        Ok(Some(Self {
            version: 1,
            run_id: run.id.clone(),
            source_begin: begin,
            source_end: journal.len(),
            final_message,
            completion,
            error,
            terminal: observer::terminal_frame(&run.id, result, model, &run.total),
        }))
    }

    fn value(&self, id: &str, journal: &[Message]) -> Result<Value> {
        if self.version != 1
            || self.run_id != id
            || self.source_begin > self.source_end
            || self.source_end > journal.len()
        {
            return Err(unavailable());
        }
        if let Some(error) = &self.error {
            if self.completion.is_some()
                || self.final_message.is_some()
                || self.terminal.pointer("/error/code").and_then(Value::as_str)
                    != Some(error.as_str())
            {
                return Err(unavailable());
            }
            return Ok(json!({
                "run_id":id,"status":"failed","text":"",
                "completion":null,"terminal":self.terminal,
            }));
        }
        let final_index = self
            .final_message
            .filter(|index| (self.source_begin..self.source_end).contains(index))
            .ok_or_else(unavailable)?;
        if journal[final_index].role != "assistant" {
            return Err(unavailable());
        }
        let mut completion = self.completion.clone().ok_or_else(unavailable)?;
        if completion["id"] != id
            || self.terminal["id"] != id
            || self.terminal["choices"][0]["finish_reason"] != "stop"
            || !self.terminal.get("usage").is_some_and(Value::is_object)
        {
            return Err(unavailable());
        }
        let choice = completion
            .get_mut("choices")
            .and_then(Value::as_array_mut)
            .filter(|choices| choices.len() == 1)
            .and_then(|choices| choices.first_mut())
            .and_then(Value::as_object_mut)
            .ok_or_else(unavailable)?;
        if choice.contains_key("message") {
            return Err(unavailable());
        }
        choice.insert(
            "message".into(),
            serde_json::to_value(&journal[final_index])?,
        );
        let mut text = String::new();
        for entry in &journal[self.source_begin..self.source_end] {
            if entry.role != "assistant" || entry.content.is_null() {
                continue;
            }
            text.push_str(entry.content.as_str().ok_or_else(unavailable)?);
        }
        Ok(json!({
            "run_id":id,"status":"completed","text":text,
            "completion":completion,"terminal":self.terminal,
        }))
    }
}

fn matches_message(message: &Message, value: &Value) -> Result<bool> {
    let Message {
        role,
        content,
        tool_calls,
        tool_call_id,
        extra,
    } = message;
    let Some(object) = value.as_object() else {
        return Ok(false);
    };
    let fields =
        2 + usize::from(!tool_calls.is_empty()) + usize::from(tool_call_id.is_some()) + extra.len();
    if object.len() != fields
        || value["role"] != *role
        || value.get("content") != Some(content)
        || !extra
            .iter()
            .all(|(key, entry)| object.get(key) == Some(entry))
    {
        return Ok(false);
    }
    if let Some(id) = tool_call_id {
        if value.get("tool_call_id").and_then(Value::as_str) != Some(id.as_str()) {
            return Ok(false);
        }
    }
    if !tool_calls.is_empty() && value.get("tool_calls") != Some(&serde_json::to_value(tool_calls)?)
    {
        return Ok(false);
    }
    Ok(true)
}

fn completion_metadata(value: &Value) -> Result<Value> {
    let object = value.as_object().ok_or_else(unavailable)?;
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| choices.len() == 1)
        .ok_or_else(unavailable)?;
    let choice = choices[0].as_object().ok_or_else(unavailable)?;
    let choice = choice
        .iter()
        .filter(|(key, _)| key.as_str() != "message")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    let mut metadata = object
        .iter()
        .filter(|(key, _)| key.as_str() != "choices")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    metadata.insert("choices".into(), Value::Array(vec![Value::Object(choice)]));
    Ok(Value::Object(metadata))
}

impl SessionInner {
    // The caller holds persistence and has validated the matching run before
    // preparing this key. Readers take the same lock and hide prepared receipts.
    pub(super) async fn prepare_chat_receipt(
        &self,
        run: &RunState,
        result: &CompletionResult,
        completed: &PersistedSession,
    ) -> Result<()> {
        let Some(receipt) =
            Receipt::prepare(run, result, &completed.journal, &completed.record.agent_id)?
        else {
            return Ok(());
        };
        write_chunks(
            &self.state.storage(),
            &format!("{PREFIX}{}", run.id),
            &serde_json::to_vec(&receipt)?,
        )
        .await
    }

    pub(super) async fn chat_run_receipt(&self, request: Request) -> Result<Response> {
        let path = request.path();
        let id = path.strip_prefix("/chat-runs/").unwrap_or("");
        if request.method() != Method::Get || request.url()?.query().is_some() || !valid_id(id) {
            return util::json_error(400, "invalid_chat_run", "Use an exact Chat run ID");
        }
        let identity = self
            .data
            .borrow()
            .as_ref()
            .map(|data| (data.record.owner_id.clone(), data.record.id.clone()));
        let Some((owner, session)) = identity else {
            return util::json_error(
                404,
                "chat_observation_unavailable",
                "Chat run is unavailable",
            );
        };
        let operation = format!("chat_read:{}", uuid::Uuid::new_v4());
        crate::reconcile::maintenance_reservation(&self.env, &owner, &session, &operation).await?;
        let meter = crate::resources::ResourceMeter::new(&self.env, &owner, &session, &operation)?;
        meter
            .require_observation("chat.observe", Resource::DurableObject, "wall_ms")
            .await?;
        let started = util::now_ms();
        let result = self.read_chat_receipt(id).await;
        let measured = meter
            .record_wall_time(
                "chat.observe",
                Resource::DurableObject,
                started,
                util::now_ms(),
            )
            .await;
        if measured.is_ok() {
            meter.settle_run().await?;
        }
        measured?;
        match result {
            Err(error) if known_unavailable(&error) => util::json_error(
                404,
                "chat_observation_unavailable",
                "Chat run is unavailable",
            ),
            result => result,
        }
    }

    async fn read_chat_receipt(&self, id: &str) -> Result<Response> {
        let _guard = self.persistence.lock().await;
        let active = self.data.borrow().as_ref().and_then(|data| {
            data.run
                .as_ref()
                .filter(|run| run.id == id)
                .map(|run| run.source_begin.is_some() && run.response.is_none())
        });
        if active == Some(true) {
            return Ok(
                Response::from_json(&json!({"run_id":id,"status":"in_progress"}))?.with_status(202),
            );
        }
        if active == Some(false) {
            return util::json_error(
                404,
                "chat_observation_unavailable",
                "Chat run is unavailable",
            );
        }
        let Some(bytes) = read_chunks(&self.state.storage(), &format!("{PREFIX}{id}")).await?
        else {
            return util::json_error(
                404,
                "chat_observation_unavailable",
                "Chat run is unavailable",
            );
        };
        let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let borrowed = self.data.borrow();
        let data = borrowed.as_ref().ok_or_else(unavailable)?;
        Response::from_json(&receipt.value(id, &data.journal)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const ID: &str = "run_0123456789abcdef0123456789abcdef";

    fn run(begin: Option<usize>) -> RunState {
        serde_json::from_value(json!({
            "id":ID,"source_begin":begin,"round":1,"admitted":true,
            "total":Usage { prompt_tokens:12, completion_tokens:3, total_tokens:15,
                cached_tokens:4,reported:true,..Usage::default() },
            "pending_completion":null,"started_at_ms":1,
        }))
        .unwrap()
    }

    fn completion(message: &Message) -> CompletionResult {
        Ok(json!({
            "id":ID,"object":"chat.completion","created":123,"model":"agent",
            "choices":[{"index":0,"message":message,"finish_reason":"stop"}],
            "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,
                "prompt_tokens_details":{"cached_tokens":4}},
            "agents_billing_status":"reconciliation_pending",
        }))
    }

    #[test]
    fn receipt_references_output_and_reconstructs_only_its_immutable_source_span() {
        let mut journal = vec![
            Message::text("assistant", "Previous run must not leak."),
            Message::text("user", "New input"),
            Message::text("assistant", "Planning 雪."),
            Message::tool_result("server-tool", "Private tool result"),
            Message::text("user", "Steering is not assistant output"),
            Message::text("assistant", "Final 🙂."),
        ];
        let result = completion(journal.last().unwrap());
        let receipt = Receipt::prepare(&run(Some(2)), &result, &journal, "agent")
            .unwrap()
            .unwrap();
        assert!(
            receipt.completion.as_ref().unwrap()["choices"][0]
                .get("message")
                .is_none()
        );
        let stored = serde_json::to_vec(&receipt).unwrap();
        journal.push(Message::text("assistant", "A later run must not leak."));
        let restored: Receipt = serde_json::from_slice(&stored).unwrap();
        let value = restored.value(ID, &journal).unwrap();
        assert_eq!(value["text"], "Planning 雪.Final 🙂.");
        assert_eq!(value["completion"], result.unwrap());
        assert_eq!(value["terminal"], receipt.terminal);
        assert_eq!(value["terminal"]["created"], 123);
        assert_eq!(receipt.source_end, 6);
    }

    #[test]
    fn unknown_outcomes_and_legacy_spans_never_become_completed_receipts() {
        let failed = Err("provider_outcome_unknown".to_owned());
        let receipt = Receipt::prepare(&run(Some(0)), &failed, &[], "agent")
            .unwrap()
            .unwrap();
        let value = receipt.value(ID, &[]).unwrap();
        assert_eq!(value["status"], "failed");
        assert!(value["completion"].is_null());
        assert!(value["terminal"].get("choices").is_none());
        assert_eq!(
            value["terminal"]["error"]["code"],
            "provider_outcome_unknown"
        );
        assert!(
            Receipt::prepare(&run(None), &failed, &[], "agent")
                .unwrap()
                .is_none()
        );
        assert!(
            Receipt::prepare(
                &run(Some(0)),
                &Err("free form /private".into()),
                &[],
                "agent"
            )
            .unwrap()
            .is_none()
        );
        let mut old = serde_json::to_value(run(None)).unwrap();
        old.as_object_mut().unwrap().remove("source_begin");
        assert!(
            serde_json::from_value::<RunState>(old)
                .unwrap()
                .source_begin
                .is_none()
        );
    }

    #[test]
    fn mismatched_final_messages_and_invalid_ranges_fail_closed() {
        let journal = vec![Message::text("assistant", "Canonical")];
        assert!(
            Receipt::prepare(
                &run(Some(0)),
                &completion(&Message::text("assistant", "Other")),
                &journal,
                "agent"
            )
            .is_err()
        );
        let mut receipt =
            Receipt::prepare(&run(Some(0)), &completion(&journal[0]), &journal, "agent")
                .unwrap()
                .unwrap();
        assert!(!valid_id("run_/../secret"));
        assert!(valid_id(ID));
        assert!(
            receipt
                .value("run_ffffffffffffffffffffffffffffffff", &journal)
                .is_err()
        );
        receipt.source_end = 2;
        assert!(receipt.value(ID, &journal).is_err());
        receipt.source_end = 1;
        receipt.source_begin = 1;
        assert!(receipt.value(ID, &journal).is_err());
        receipt.source_begin = 0;
        receipt.completion.as_mut().unwrap()["choices"] = Value::Null;
        assert!(receipt.value(ID, &journal).is_err());
    }

    #[test]
    fn metadata_size_does_not_scale_with_output_text() {
        let bytes = |text: String| {
            let journal = vec![Message::text("assistant", text)];
            let receipt =
                Receipt::prepare(&run(Some(0)), &completion(&journal[0]), &journal, "agent")
                    .unwrap()
                    .unwrap();
            serde_json::to_vec(&receipt).unwrap().len()
        };
        assert_eq!(bytes("small".into()), bytes("large output".repeat(10_000)));
    }

    #[test]
    fn permanent_unavailability_is_distinct_from_retryable_storage_failure() {
        assert!(known_unavailable(&unavailable()));
        assert!(!known_unavailable(&Error::RustError(
            "storage_unreachable".into()
        )));
        let mut message = Message::text("assistant", "large final text".repeat(1000));
        message
            .extra
            .insert("reasoning_content".into(), json!("private continuation"));
        let exact = serde_json::to_value(&message).unwrap();
        assert!(matches_message(&message, &exact).unwrap());
        let mut altered = exact;
        altered["content"] = json!("different");
        assert!(!matches_message(&message, &altered).unwrap());
    }

    proptest! {
        #[test]
        fn arbitrary_unicode_output_uses_fixed_indices(parts in prop::collection::vec(".*", 1..12), start in 0usize..16) {
            let mut journal: Vec<Message> = parts.iter().map(|text| Message::text("assistant", text)).collect();
            let prepared = Receipt::prepare(&run(Some(start)), &completion(journal.last().unwrap()), &journal, "agent");
            if start >= parts.len() {
                prop_assert!(prepared.is_err());
            } else {
                let receipt = prepared.unwrap().unwrap();
                journal.push(Message::text("assistant", "later"));
                let value = receipt.value(ID, &journal).unwrap();
                prop_assert_eq!(value["text"].as_str().unwrap(), parts[start..].concat());
                prop_assert_eq!(receipt.source_end, parts.len());
            }
        }
    }
}
