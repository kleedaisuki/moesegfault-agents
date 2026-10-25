//! Immutable response boundaries; only an actual fork materializes its history.

use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct NativeHistoryRef {
    #[serde(default)]
    pub generation: String,
    pub source_session_id: String,
    pub end: usize,
    pub provider_binding: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkspaceSeed {
    Empty,
    Archive {
        archive: crate::workspace::OwnedArchiveRef,
    },
    // A live mutable checkpoint is not a historical snapshot.
    Unsealed,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct BranchPoint {
    pub response_id: String,
    pub source_session_id: String,
    pub source_end: usize,
    pub pin_hash: String,
    pub pending_calls: Vec<provider::ToolCall>,
    pub workspace: WorkspaceSeed,
    #[serde(default)]
    pub native: Option<NativeHistoryRef>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ImportedParent {
    pub response_id: String,
    pub seed_hash: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct BranchExport {
    pub owner_id: String,
    pub agent_id: String,
    pub child_response_id: String,
    pub pinned: agents_core::PinnedContext,
    pub point: BranchPoint,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub native_items: Option<Vec<Value>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportRequest {
    owner_id: String,
    parent_response_id: String,
    child_response_id: String,
    request: crate::responses_wire::ResponseRequest,
}

pub(crate) fn child_session_id(owner: &str, response: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"agents-response-branch-v1\0");
    digest.update((owner.len() as u64).to_be_bytes());
    digest.update(owner.as_bytes());
    digest.update(response.as_bytes());
    format!("branch_{:x}", digest.finalize())
}

fn project_branch_history(source: &[Message]) -> Result<(Vec<Message>, Vec<Message>)> {
    let mut messages = Vec::with_capacity(source.len());
    let mut pending = vec![];
    let mut calls = std::collections::BTreeSet::new();
    for message in source {
        match message.role.as_str() {
            "system" | "developer" => continue,
            "user" if !calls.is_empty() => {
                pending.push(message.clone());
                continue;
            }
            "assistant" => {
                if !calls.is_empty() {
                    return Err(Error::RustError(
                        "response_branch_tool_group_incomplete".into(),
                    ));
                }
                calls.extend(message.tool_calls.iter().map(|call| call.id.as_str()));
            }
            "tool" => {
                if !message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| calls.remove(id))
                {
                    return Err(Error::RustError(
                        "response_branch_tool_group_invalid".into(),
                    ));
                }
            }
            _ => {}
        }
        messages.push(message.clone());
        if calls.is_empty() {
            messages.append(&mut pending);
        }
    }
    Ok((messages, pending))
}

impl SessionInner {
    pub(super) async fn queue_response_archive_release(&self, id: &str) -> Result<()> {
        let mut data = self.snapshot()?;
        if !data
            .response_archive_releases
            .iter()
            .any(|entry| entry == id)
        {
            data.response_archive_releases.push(id.to_owned());
        }
        save_state(&self.state.storage(), &mut data).await?;
        self.data
            .borrow_mut()
            .as_mut()
            .unwrap()
            .response_archive_releases = data.response_archive_releases;
        self.arm_alarm().await
    }

    pub(super) async fn reconcile_response_archives(&self) -> Result<()> {
        let ready = self.data.borrow().as_ref().is_some_and(|data| {
            !data.response_archive_releases.is_empty()
                && data.response_archive_retry_at <= util::now_ms()
        });
        if !ready {
            return Ok(());
        }
        let _guard = self.persistence.lock().await;
        let pending = self.snapshot()?.response_archive_releases;
        for id in pending {
            if self
                .read_response(&id)
                .await?
                .is_some_and(|response| response.deleted)
            {
                let owner = self.snapshot()?.record.owner_id;
                if crate::workspace_blobs::release_response(&self.env, &owner, &id)
                    .await
                    .is_err()
                {
                    let mut data = self.snapshot()?;
                    data.response_archive_retry_attempts =
                        data.response_archive_retry_attempts.saturating_add(1);
                    let delay =
                        1000u64.saturating_mul(1u64 << data.response_archive_retry_attempts.min(8));
                    data.response_archive_retry_at = util::now_ms().saturating_add(delay);
                    save_state(&self.state.storage(), &mut data).await?;
                    let mut current = self.data.borrow_mut();
                    let current = current.as_mut().unwrap();
                    current.response_archive_retry_attempts = data.response_archive_retry_attempts;
                    current.response_archive_retry_at = data.response_archive_retry_at;
                    util::emit(
                        "response.archive_release_deferred",
                        json!({"session_id":data.record.id,"response_id":id}),
                    );
                    return Ok(());
                }
            }
            let mut data = self.snapshot()?;
            data.response_archive_releases.retain(|entry| entry != &id);
            data.response_archive_retry_at = 0;
            data.response_archive_retry_attempts = 0;
            save_state(&self.state.storage(), &mut data).await?;
            let mut current = self.data.borrow_mut();
            let current = current.as_mut().unwrap();
            current.response_archive_releases = data.response_archive_releases;
            current.response_archive_retry_at = 0;
            current.response_archive_retry_attempts = 0;
        }
        Ok(())
    }

    pub(super) async fn branch_dispatched(&self, id: &str) -> Result<bool> {
        Ok(self
            .state
            .storage()
            .get::<bool>(&format!("branch-dispatch:{id}"))
            .await?
            .unwrap_or(false))
    }

    pub(super) async fn prepare_response_branch(&self) -> Result<()> {
        let data = self.snapshot()?;
        let Some(run) = data.run.as_ref() else {
            return Ok(());
        };
        let Some(mut response) = run.response.clone() else {
            return Ok(());
        };
        if response.branch_point.is_some() || run.ended_at_ms.is_some() {
            return Ok(());
        }
        self.seal_branch_point(&mut response).await?;
        if run.admitted && response.request.store {
            let workspace = self.workspace(&data.record).for_run(&run.id);
            let archive = workspace.seal_response_archive(&response.id).await?;
            let point = response.branch_point.as_mut().unwrap();
            point.workspace = archive.map_or(WorkspaceSeed::Empty, |archive| {
                WorkspaceSeed::Archive { archive }
            });
            let metadata = workspace.snapshot_metadata().await?;
            self.data
                .borrow_mut()
                .as_mut()
                .unwrap()
                .record
                .workspace_snapshot = metadata;
        }
        let mut current = self.data.borrow_mut();
        let Some(current) = current
            .as_mut()
            .and_then(|data| data.run.as_mut())
            .filter(|current| current.id == run.id)
        else {
            return Err(Error::RustError("response_branch_run_changed".into()));
        };
        current.response.as_mut().unwrap().branch_point = response.branch_point;
        Ok(())
    }

    pub(super) async fn seal_branch_point(&self, response: &mut ResponseRun) -> Result<()> {
        if response.branch_point.is_some() {
            return Ok(());
        }
        let (source_session_id, source_end, pin_hash, checkpoint) = {
            let data = self.data.borrow();
            let data = data
                .as_ref()
                .ok_or_else(|| Error::RustError("session_not_initialized".into()))?;
            (
                data.record.id.clone(),
                data.journal.len(),
                data.record.pinned.prefix_hash(),
                data.record.workspace_snapshot.is_some(),
            )
        };
        let used_workspace = checkpoint
            || self
                .state
                .storage()
                .get::<String>("workspace:token")
                .await?
                .is_some();
        response.branch_point = Some(BranchPoint {
            response_id: response.id.clone(),
            source_session_id,
            source_end,
            pin_hash,
            pending_calls: response.awaiting_calls.clone(),
            workspace: if used_workspace {
                WorkspaceSeed::Unsealed
            } else {
                WorkspaceSeed::Empty
            },
            native: self.native_branch_ref()?,
        });
        Ok(())
    }

    pub(super) async fn branch_route(&self, mut req: Request) -> Result<Response> {
        if req.method() != Method::Post {
            return util::json_error(405, "method_not_allowed", "Method not allowed");
        }
        match req.path().as_str() {
            "/branches/export" => {
                let input: ExportRequest = req.json().await?;
                self.export_branch(input).await
            }
            "/branches/import" => {
                let input: BranchExport = req.json().await?;
                self.import_branch(input).await
            }
            _ => util::json_error(404, "not_found", "Branch operation not found"),
        }
    }

    async fn export_branch(&self, input: ExportRequest) -> Result<Response> {
        let _guard = self.persistence.lock().await;
        let data = self.snapshot()?;
        if self
            .read_response(&input.child_response_id)
            .await?
            .is_some()
        {
            return util::json_error(
                409,
                "response_already_admitted",
                "The response was admitted by its original session",
            );
        }
        if input.owner_id != data.record.owner_id {
            return util::json_error(404, "not_found", "Response not found");
        }
        let Some(mut response) = self.read_response(&input.parent_response_id).await? else {
            return util::json_error(404, "not_found", "Response not retained");
        };
        if response.deleted || !response.request.store {
            return util::json_error(404, "not_found", "Response not retained");
        }
        if !matches!(
            response.status.as_str(),
            "completed" | "failed" | "incomplete" | "cancelled"
        ) {
            return util::json_error(
                409,
                "parent_response_in_progress",
                "Wait until the parent response is terminal",
            );
        }
        // Only a provable current boundary can be upgraded from the old format.
        if response.branch_point.is_none()
            && data.run.is_none()
            && data.pending.is_empty()
            && data.response_head.as_deref() == Some(response.id.as_str())
        {
            self.seal_branch_point(&mut response).await?;
            self.write_response(&response).await?;
        }
        let Some(point) = response.branch_point else {
            return util::json_error(
                409,
                "response_history_unavailable",
                "This response predates retained branch boundaries",
            );
        };
        if point.source_session_id != data.record.id
            || point.pin_hash != data.record.pinned.prefix_hash()
            || point.source_end > data.journal.len()
        {
            return Err(Error::RustError("response_branch_boundary_corrupt".into()));
        }
        let normalized = match crate::response_preflight::validate(&input.request) {
            Ok(input) => input,
            Err(error) => return util::json_error(400, error.code, &error.message),
        };
        if let Err(code) =
            super::responses::validate_tool_outputs(&point.pending_calls, &normalized.tool_outputs)
        {
            return util::json_error(
                400,
                code,
                "Provide each outstanding function result exactly once before continuing",
            );
        }
        if matches!(point.workspace, WorkspaceSeed::Unsealed) {
            return util::json_error(
                409,
                "response_workspace_unsealed",
                "The historical workspace has no immutable retained archive",
            );
        }
        let native_items = match &point.native {
            Some(reference) => Some(self.native_export(reference).await?),
            None => None,
        };
        if let WorkspaceSeed::Archive { archive } = &point.workspace {
            crate::workspace_blobs::bind_transfer(
                &self.env,
                &data.record.owner_id,
                &input.child_response_id,
                archive,
            )
            .await?;
        }
        // Fence stale edge dispatches before returning an immutable transfer.
        self.state
            .storage()
            .put(
                &format!("branch-dispatch:{}", input.child_response_id),
                true,
            )
            .await?;
        Response::from_json(&BranchExport {
            owner_id: data.record.owner_id,
            agent_id: data.record.agent_id,
            child_response_id: input.child_response_id,
            pinned: data.record.pinned,
            messages: data.journal[..point.source_end].to_vec(),
            native_items,
            point,
        })
    }

    async fn import_branch(&self, seed: BranchExport) -> Result<Response> {
        let seed_hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&seed)?));
        let _guard = self.persistence.lock().await;
        let data = self.snapshot()?;
        if data.record.owner_id != seed.owner_id
            || data.record.id != child_session_id(&seed.owner_id, &seed.child_response_id)
        {
            return util::json_error(
                409,
                "branch_identity_mismatch",
                "Branch destination does not match its admission identity",
            );
        }
        if let Some(imported) = &data.imported_parent {
            if imported.seed_hash != seed_hash {
                return util::json_error(
                    409,
                    "branch_seed_conflict",
                    "Branch already has a different immutable parent",
                );
            }
            // A retry flushes the same state and never resets an admitted child.
            drop(_guard);
            self.persist().await?;
            return Response::from_json(&json!({"imported":true}));
        }
        if data.run.is_some() || !data.journal.is_empty() || data.record.input_revision != 0 {
            return util::json_error(
                409,
                "branch_destination_not_empty",
                "Branch destination already contains activity",
            );
        }
        if matches!(seed.point.workspace, WorkspaceSeed::Unsealed)
            || seed.point.native.is_some() != seed.native_items.is_some()
        {
            return util::json_error(
                409,
                "branch_seed_unavailable",
                "Branch context is not transferable",
            );
        }
        if data.record.agent_id == seed.agent_id && data.record.pinned != seed.pinned {
            return util::json_error(
                409,
                "branch_pin_mismatch",
                "A same-agent branch must retain its original pin",
            );
        }
        if let WorkspaceSeed::Archive { archive } = &seed.point.workspace {
            self.workspace(&data.record)
                .import_response_archive(archive, &seed.child_response_id)
                .await?;
        }
        let mut imported = data;
        if matches!(seed.point.workspace, WorkspaceSeed::Archive { .. }) {
            imported.record.workspace_snapshot =
                self.workspace(&imported.record).snapshot_metadata().await?;
        }
        imported.history_instructions = seed
            .messages
            .iter()
            .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
            .cloned()
            .collect();
        imported.messages = vec![Message::text("system", system_prefix(&imported.record))];
        // Source order records acceptance, not provider adjacency: user steering
        // accepted during a tool handoff follows its results in the model view.
        let (messages, pending) = project_branch_history(&seed.messages)?;
        imported.messages.extend(messages);
        imported.pending = pending;
        if let (Some(reference), Some(items)) = (&seed.point.native, &seed.native_items) {
            let history = self
                .native_import(
                    items,
                    &reference.provider_binding,
                    &format!("branch:{}", seed.child_response_id),
                )
                .await?;
            imported.native_context = history.map(|history| super::native::NativeContext {
                history,
                messages_end: imported.messages.len(),
            });
        }
        if imported.record.agent_id != seed.agent_id
            || (seed.point.native.is_some() && imported.native_context.is_none())
        {
            // Continuation signatures/reasoning belong to the original provider
            // route. The immutable source remains intact for export/audit.
            for message in &mut imported.messages {
                if message.role == "assistant" {
                    message.extra.clear();
                }
            }
        }
        imported.journal = seed.messages;
        imported.pending_client_tools = seed.point.pending_calls;
        imported.response_head = Some(seed.point.response_id.clone());
        imported.imported_parent = Some(ImportedParent {
            response_id: seed.point.response_id,
            seed_hash,
        });
        save_state(&self.state.storage(), &mut imported).await?;
        *self.data.borrow_mut() = Some(imported);
        // The child's current archive is now durable. A failed release is only
        // a bounded intermediate lease; it cannot invalidate accepted history.
        if crate::workspace_blobs::release_transfer(
            &self.env,
            &seed.owner_id,
            &seed.child_response_id,
        )
        .await
        .is_err()
        {
            util::emit(
                "response.branch_transfer_release_deferred",
                json!({"session_id":child_session_id(&seed.owner_id,&seed.child_response_id),"response_id":seed.child_response_id}),
            );
        }
        drop(_guard);
        self.arm_alarm().await?;
        Response::from_json(&json!({"imported":true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn assistant_calls(count: usize) -> Message {
        let mut message = Message::text("assistant", "");
        message.tool_calls = (0..count)
            .map(|index| provider::ToolCall {
                id: format!("call_{index}"),
                kind: "function".into(),
                function: provider::FunctionCall {
                    name: "local".into(),
                    arguments: "{}".into(),
                },
            })
            .collect();
        message
    }

    proptest! {
        #[test]
        fn branch_projection_preserves_source_and_defers_steering(
            call_count in 1usize..8, completed in 0usize..8,
            steering in proptest::collection::vec("[a-z]{1,12}", 0..12),
        ) {
            let completed = completed.min(call_count);
            let mut source = vec![assistant_calls(call_count)];
            let users = steering.iter().map(|text| Message::text("user", text)).collect::<Vec<_>>();
            source.extend(users.iter().cloned());
            source.extend((0..completed).map(|index| Message::tool_result(&format!("call_{index}"), "result")));
            let original = serde_json::to_value(&source).unwrap();
            let (messages, pending) = project_branch_history(&source).unwrap();
            prop_assert_eq!(serde_json::to_value(&source).unwrap(), original);
            prop_assert_eq!(messages.iter().skip(1).take(completed).filter(|message| message.role == "tool").count(), completed);
            if completed == call_count {
                prop_assert!(pending.is_empty());
                prop_assert_eq!(serde_json::to_value(&messages[completed+1..]).unwrap(), serde_json::to_value(&users).unwrap());
            } else {
                prop_assert_eq!(messages.len(), completed+1);
                prop_assert_eq!(serde_json::to_value(&pending).unwrap(), serde_json::to_value(&users).unwrap());
            }
        }
    }

    #[test]
    fn child_identity_is_retry_stable_and_owner_scoped() {
        assert_eq!(
            child_session_id("owner", "resp_a"),
            child_session_id("owner", "resp_a")
        );
        assert_ne!(
            child_session_id("owner", "resp_a"),
            child_session_id("other", "resp_a")
        );
        assert_ne!(child_session_id("a", "bc"), child_session_id("ab", "c"));
    }

    #[test]
    fn deferred_archive_release_keeps_closed_session_alive_without_delaying_work() {
        let mut data = super::super::tests::state();
        data.record.status = SessionStatus::Closed;
        data.response_archive_releases.push("resp_deleted".into());
        data.response_archive_retry_at = 12_000;
        assert_eq!(alarm_deadline(&data, 1000), Some(12_000));
        data.archive_pending = true;
        assert_eq!(alarm_deadline(&data, 1000), Some(1010));
        data.archive_pending = false;
        data.response_archive_releases.clear();
        assert_eq!(alarm_deadline(&data, 1000), None);
    }
}
