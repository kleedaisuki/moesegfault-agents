//! A dispatch identity consumes the response-wide built-in allowance once.
//! This is not an effect receipt: each tool's existing execution fence still
//! determines whether a recovered dispatch may execute or only reconcile.

use std::collections::BTreeSet;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    Unbounded,
    New,
    Replay,
    Exhausted,
}

fn admit(keys: &mut BTreeSet<String>, limit: Option<u32>, execution: &str) -> Admission {
    let Some(limit) = limit else {
        return Admission::Unbounded;
    };
    if keys.contains(execution) {
        return Admission::Replay;
    }
    if keys.len() >= limit as usize {
        return Admission::Exhausted;
    }
    keys.insert(execution.into());
    Admission::New
}

impl RunState {
    pub(super) fn tool_budget_exhausted(&self) -> bool {
        self.response
            .as_ref()
            .and_then(|response| response.request.max_tool_calls)
            .is_some_and(|limit| self.tool_dispatches.len() >= limit as usize)
    }
}

impl SessionInner {
    /// Unknown names do not consume authority or allowance. Caller-local tools
    /// leave the harness before reaching this function and are never counted.
    pub(super) async fn admit_tool_dispatch(
        &self,
        record: &SessionRecord,
        execution: &str,
        name: &str,
    ) -> Result<bool> {
        let admission = {
            let mut data = self.data.borrow_mut();
            let data = data
                .as_mut()
                .ok_or_else(|| Error::RustError("session_not_initialized".into()))?;
            let run = data
                .run
                .as_mut()
                .ok_or_else(|| Error::RustError("session_run_missing".into()))?;
            let hosted = run.response.as_ref().is_some_and(|response| {
                response
                    .bound_tools
                    .lookup(name)
                    .is_some_and(|binding| binding.allows(crate::hosted_bindings::Caller::Direct))
            });
            if !hosted && !record.pinned.tools.iter().any(|tool| tool.name == name) {
                return Ok(true);
            }
            let limit = run
                .response
                .as_ref()
                .and_then(|response| response.request.max_tool_calls);
            admit(&mut run.tool_dispatches, limit, execution)
        };
        if matches!(admission, Admission::New | Admission::Replay) {
            // Re-persist on replay too: a previous failed write can leave the
            // key in warm memory without proving its durable boundary.
            self.persist().await?;
        }
        Ok(admission != Admission::Exhausted)
    }
}

pub(super) fn filter_exhausted_tools(
    run: &RunState,
    record: &SessionRecord,
    definitions: &mut Vec<tools::ToolDefinition>,
) {
    if !run.tool_budget_exhausted() {
        return;
    }
    definitions.retain(|definition| {
        let name = &definition.function.name;
        !record.pinned.tools.iter().any(|tool| &tool.name == name)
            && !run
                .response
                .as_ref()
                .is_some_and(|response| response.bound_tools.lookup(name).is_some())
    });
}

pub(super) fn exhausted_result() -> String {
    json!({"error":{"code":"max_tool_calls_exceeded","message":"The response's built-in tool allowance is exhausted; no tool was executed."}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn one_budget_spans_tools_and_rounds_but_recovery_replays_do_not_spend_twice() {
        let mut keys = BTreeSet::new();
        let first = execution_key("run", 1, "call_a");
        let second = execution_key("run", 2, "call_a");
        assert_eq!(admit(&mut keys, Some(2), &first), Admission::New);
        assert_eq!(admit(&mut keys, Some(2), &first), Admission::Replay);
        let mut restored: BTreeSet<String> =
            serde_json::from_slice(&serde_json::to_vec(&keys).unwrap()).unwrap();
        assert_eq!(admit(&mut restored, Some(2), &second), Admission::New);
        assert_eq!(admit(&mut restored, Some(2), "third"), Admission::Exhausted);
        assert_eq!(admit(&mut restored, Some(2), &first), Admission::Replay);
        assert_eq!(restored.len(), 2);
        assert_eq!(admit(&mut BTreeSet::new(), Some(2), &first), Admission::New);
    }

    #[test]
    fn legacy_durable_runs_need_no_migration_and_have_no_tool_budget_fields() {
        let run: RunState = serde_json::from_value(json!({
            "id":"old_run","round":1,"admitted":true,"total":Usage::default(),
            "pending_completion":null,"started_at_ms":1,
        }))
        .unwrap();
        assert!(run.tool_dispatches.is_empty());
        assert!(!run.tool_budget_exhausted());
        assert!(
            serde_json::to_value(&run)
                .unwrap()
                .get("tool_dispatches")
                .is_none()
        );
    }

    #[test]
    fn unlimited_legacy_runs_have_no_extra_state_and_exhaustion_is_not_a_fake_terminal_reason() {
        let mut keys = BTreeSet::new();
        assert_eq!(admit(&mut keys, None, "one"), Admission::Unbounded);
        assert!(keys.is_empty());
        assert_eq!(admit(&mut keys, Some(0), "one"), Admission::Exhausted);
        let result: Value = serde_json::from_str(&exhausted_result()).unwrap();
        assert_eq!(result["error"]["code"], "max_tool_calls_exceeded");
        assert!(result.get("incomplete_details").is_none());
    }

    proptest! {
        #[test]
        fn arbitrary_replay_and_crash_boundaries_never_exceed_limit(
            limit in 0u32..32,
            calls in prop::collection::vec(0u8..64, 0..256),
        ) {
            let mut keys = BTreeSet::new();
            let mut accepted = BTreeSet::new();
            for call in calls {
                let id = format!("execution_{call}");
                let prior = keys.clone();
                let decision = admit(&mut keys, Some(limit), &id);
                match decision {
                    Admission::New => { accepted.insert(id); }
                    Admission::Replay => prop_assert!(prior.contains(&id)),
                    Admission::Exhausted => prop_assert_eq!(&keys, &prior),
                    Admission::Unbounded => prop_assert!(false),
                }
                keys = serde_json::from_slice(&serde_json::to_vec(&keys).unwrap()).unwrap();
                prop_assert!(keys.len() <= limit as usize);
                prop_assert_eq!(&keys, &accepted);
            }
        }
    }
}
