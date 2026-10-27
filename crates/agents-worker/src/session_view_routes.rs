use super::*;

impl SessionInner {
    pub(super) fn lightweight_view(&self, request: &Request) -> Result<Response> {
        let borrowed = self.data.borrow();
        let data = borrowed
            .as_ref()
            .ok_or_else(|| Error::RustError("session_not_initialized".into()))?;
        let value = view_value(data);
        let etag = views::etag(&value)?;
        let validator = request.headers().get("If-None-Match")?;
        let mut response = if views::not_modified(validator.as_deref(), &etag) {
            Response::empty()?.with_status(304)
        } else {
            Response::from_json(&value)?
        };
        response.headers_mut().set("ETag", &etag)?;
        response
            .headers_mut()
            .set("Cache-Control", "private, no-store")?;
        Ok(response)
    }

    pub(super) fn journal_page(&self, request: &Request) -> Result<Response> {
        let borrowed = self.data.borrow();
        let data = borrowed
            .as_ref()
            .ok_or_else(|| Error::RustError("session_not_initialized".into()))?;
        let query = match views::PageQuery::parse(&request.url()?, &data.record.id) {
            Ok(query) => query,
            Err(message) => return util::json_error(400, "invalid_cursor", message),
        };
        match views::page(&data.record.id, &data.journal, query) {
            Ok(page) => Response::from_json(&page),
            Err(message) => util::json_error(400, "invalid_cursor", message),
        }
    }
}

fn view_value(data: &PersistedSession) -> Value {
    let local_calls = data
        .pending_client_tools
        .iter()
        .map(|call| {
            json!({
                "call_id":call.id,"name":call.function.name,
                "arguments":call.function.arguments
            })
        })
        .collect::<Vec<_>>();
    json!({
        "id":data.record.id,"agent_id":data.record.agent_id,
        "status":data.record.status,"created_at_ms":data.record.created_at_ms,
        "input_revision":data.record.input_revision,
        "last_input_at_ms":data.record.last_input_at_ms,
        "context_tokens":data.input_tokens,
        "context_window":data.record.pinned.model.context_window,
        "compact_count":data.compact_count,
        "pending_messages":data.pending,"message_count":data.journal.len(),
        "last_error":data.last_error,
        "checkpoint_pending":data.checkpoint_error.is_some(),
        "checkpoint_error":data.checkpoint_error,
        "billing_pending":data.last_error.as_deref()==Some("resource_reconciliation_pending"),
        "projection_pending":data.projection_pending,
        "archive_pending":data.archive_pending || data.archive_preparing,
        "workspace_snapshot":data.record.workspace_snapshot,
        "run_id":data.run.as_ref().map(|run| &run.id),
        "latest_response_id":data.response_head,"pending_local_calls":local_calls
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> PersistedSession {
        let agent: agents_core::CreateAgent = serde_json::from_value(json!({
            "name":"view-test","instructions":"p".repeat(64*1024)
        }))
        .unwrap();
        let agent = agent.into_config("a".into(), "owner".into(), 0);
        let record = SessionRecord {
            id: "s".into(),
            owner_id: "owner".into(),
            agent_id: "a".into(),
            status: SessionStatus::Idle,
            created_at_ms: 0,
            last_input_at_ms: 0,
            input_revision: 0,
            pinned: agents_core::PinnedContext::resolve(&agent, vec![], vec![]),
            workspace_snapshot: None,
            runtime_profile: agents_core::RuntimeProfile::Basic1g,
        };
        serde_json::from_value(json!({"record":record,"messages":[]})).unwrap()
    }

    #[test]
    fn view_omits_source_and_large_immutable_pin_but_tracks_assistant_changes() {
        let mut session = session();
        session.journal = vec![Message::text("tool", "x".repeat(1024 * 1024))];
        let initial = view_value(&session);
        assert!(initial.get("messages").is_none());
        assert!(initial.get("pinned").is_none());
        assert_eq!(initial["message_count"], 1);
        assert!(serde_json::to_vec(&initial).unwrap().len() < 1024);
        let etag = views::etag(&initial).unwrap();
        session.journal.push(Message::text("assistant", "done"));
        assert_eq!(session.record.input_revision, 0);
        assert_ne!(etag, views::etag(&view_value(&session)).unwrap());
    }

    #[test]
    fn checkpoint_reconciliation_and_client_handoff_invalidate_the_view() {
        let mut session = session();
        let initial = views::etag(&view_value(&session)).unwrap();
        session.checkpoint_error = Some("checkpoint_pending".into());
        assert_ne!(initial, views::etag(&view_value(&session)).unwrap());
        session.checkpoint_error = None;
        session.archive_preparing = true;
        assert_ne!(initial, views::etag(&view_value(&session)).unwrap());
        session.archive_preparing = false;
        session.projection_pending = true;
        assert_ne!(initial, views::etag(&view_value(&session)).unwrap());
        session.projection_pending = false;
        session.response_head = Some("resp-1".into());
        session.pending_client_tools.push(provider::ToolCall {
            id: "call-1".into(),
            kind: "function".into(),
            function: provider::FunctionCall {
                name: "local_lookup".into(),
                arguments: "{}".into(),
            },
        });
        let final_view = view_value(&session);
        assert_ne!(initial, views::etag(&final_view).unwrap());
        assert_eq!(final_view["pending_local_calls"][0]["call_id"], "call-1");
        assert_eq!(final_view["pending_local_calls"][0]["name"], "local_lookup");
        assert_eq!(final_view["latest_response_id"], "resp-1");
    }
}
