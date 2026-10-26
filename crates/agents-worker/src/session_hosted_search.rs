//! Durable boundary around one hosted search call. Financial and publication
//! retries consume stored receipts; they never resubmit an external operation.

use super::*;
use crate::{
    hosted_bindings::ResolvedBinding,
    hosted_search::{HostedSearchOutcome, SearchPlan, SearchRouteKind},
    hosted_service_metering::{self as service_metering, KnownUsage, ServiceBudget, ServiceHold},
    search_byok::{SearchExchange, SearchFailure, SearchStatus},
};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchOperation {
    digest: String,
    admitted_at_ms: u64,
    plan: SearchPlan,
    hold: Option<ServiceHold>,
    dispatched: bool,
    request_started_ms: Option<u64>,
    outcome: Option<HostedSearchOutcome>,
    completed_at_ms: Option<u64>,
    known_usage: Option<KnownUsage>,
    settled: bool,
    published_result: Option<String>,
}

impl SessionInner {
    pub(super) async fn dispatch_hosted_search(
        &self,
        record: &SessionRecord,
        execution_id: &str,
        call: &provider::ToolCall,
        binding: &ResolvedBinding,
    ) -> Result<HostedDispatch> {
        let key = operation_key(execution_id);
        let digest = request_digest(&record.owner_id, &record.id, execution_id, binding, call)?;
        let mut operation = self.read_search_operation(&key).await?;
        if let Some(saved) = &operation {
            if saved.digest != digest {
                return Err(fault("hosted_search_operation_conflict"));
            }
            if let Some(result) = &saved.published_result {
                return Ok(HostedDispatch::Ready(result.clone().into()));
            }
        }
        // A cold recovery cannot distinguish a request lost before reaching the
        // provider from one whose result was lost. Never infer permission to retry.
        if operation
            .as_ref()
            .is_some_and(|saved| saved.dispatched && saved.outcome.is_none())
        {
            let saved = operation
                .as_mut()
                .ok_or_else(|| fault("hosted_search_operation_missing"))?;
            saved.outcome = Some(terminal_outcome(
                saved,
                "search_provider_outcome_unknown",
                true,
            ));
            saved.completed_at_ms = Some(util::now_ms());
            if self.save_search_operation(&key, saved).await.is_err() {
                return pending().await;
            }
        }
        if operation
            .as_ref()
            .is_some_and(|saved| saved.outcome.is_some())
        {
            return self
                .finish_hosted_search(&key, operation.as_mut().unwrap(), execution_id, record)
                .await;
        }
        if self.search_cancelled() {
            if let Some(saved) = operation.as_mut() {
                return self
                    .cancel_hosted_search(&key, saved, execution_id, record)
                    .await;
            }
            return Ok(HostedDispatch::Ready(cancelled_result().into()));
        }
        let trace = {
            let data = self.data.borrow();
            crate::telemetry::TraceContext::from_traceparent(
                data.as_ref()
                    .and_then(|data| data.run.as_ref())
                    .and_then(|run| run.traceparent.as_deref()),
            )
        };
        let prepared = crate::hosted_search::prepare(
            &self.env,
            &record.owner_id,
            binding,
            &call.function.arguments,
            execution_id,
            trace.as_ref(),
        )
        .await?;
        if operation.is_none() {
            let saved = SearchOperation {
                digest,
                admitted_at_ms: util::now_ms(),
                plan: prepared.plan().clone(),
                hold: None,
                dispatched: false,
                request_started_ms: None,
                outcome: None,
                completed_at_ms: None,
                known_usage: None,
                settled: false,
                published_result: None,
            };
            if self.save_search_operation(&key, &saved).await.is_err() {
                return pending().await;
            }
            operation = Some(saved);
        }
        let saved = operation
            .as_mut()
            .ok_or_else(|| fault("hosted_search_operation_missing"))?;
        if self.search_cancelled() {
            return self
                .cancel_hosted_search(&key, saved, execution_id, record)
                .await;
        }
        if saved.plan.route != SearchRouteKind::OpenaiResponsesByok
            && util::now_ms() > saved.admitted_at_ms.saturating_add(300_000)
        {
            // Abort via the same read-only restoration path: an earlier reserve
            // may exist even if its local hold copy never committed.
            return self
                .abort_hosted_search(
                    &key,
                    saved,
                    execution_id,
                    record,
                    "search_service_admission_expired",
                )
                .await;
        }
        if saved.hold.is_none() {
            let hold = match service_metering::prepare_hold(
                &self.env,
                &record.owner_id,
                Some(&record.id),
                execution_id,
                &saved.digest,
                saved.admitted_at_ms,
                budget(saved),
            )
            .await
            {
                Ok(hold) => hold,
                Err(_) => return pending().await,
            };
            saved.hold = Some(hold);
            if self.save_search_operation(&key, saved).await.is_err() {
                return pending().await;
            }
        }
        if self.search_cancelled() {
            return self
                .cancel_hosted_search(&key, saved, execution_id, record)
                .await;
        }
        let mut searching = terminal_outcome(saved, "search_pending", false)
            .public_item(&hosted_item_id(execution_id));
        searching["status"] = json!("searching");
        if let Err(error) = self.publish_hosted_item(&searching, false).await {
            if super::permanent_publication_error(&error) {
                return self
                    .abort_hosted_search(
                        &key,
                        saved,
                        execution_id,
                        record,
                        "search_publication_rejected",
                    )
                    .await;
            }
            return pending().await;
        }
        let started = util::now_ms();
        if saved
            .hold
            .as_ref()
            .ok_or_else(|| fault("hosted_search_hold_missing"))?
            .validate_dispatch_time(started)
            .is_err()
        {
            return self
                .abort_hosted_search(
                    &key,
                    saved,
                    execution_id,
                    record,
                    "search_service_admission_expired",
                )
                .await;
        }
        saved.dispatched = true;
        saved.request_started_ms = Some(started);
        if self.save_search_operation(&key, saved).await.is_err() {
            return pending().await;
        }
        // Cancellation may have arrived while the dispatch fence was being
        // committed. This live invocation still has proof that no HTTP ran.
        if self.search_cancelled() {
            saved.dispatched = false;
            return self
                .cancel_hosted_search(&key, saved, execution_id, record)
                .await;
        }
        let outcome = prepared.dispatch().await;
        let completed = util::now_ms();
        saved.known_usage = known_usage(&outcome, completed);
        saved.outcome = Some(outcome);
        saved.completed_at_ms = Some(completed);
        if self.save_search_operation(&key, saved).await.is_err() {
            return pending().await;
        }
        self.finish_hosted_search(&key, saved, execution_id, record)
            .await
    }

    fn search_cancelled(&self) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .is_some_and(|response| response.cancel_requested)
    }

    async fn read_search_operation(&self, key: &str) -> Result<Option<SearchOperation>> {
        read_chunks(&self.state.storage(), key)
            .await?
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|_| fault("hosted_search_operation_corrupt"))
            })
            .transpose()
    }

    async fn save_search_operation(&self, key: &str, operation: &SearchOperation) -> Result<()> {
        write_chunks(&self.state.storage(), key, &serde_json::to_vec(operation)?).await
    }

    async fn cancel_hosted_search(
        &self,
        key: &str,
        saved: &mut SearchOperation,
        execution_id: &str,
        record: &SessionRecord,
    ) -> Result<HostedDispatch> {
        self.abort_hosted_search(key, saved, execution_id, record, "search_cancelled")
            .await
    }

    async fn abort_hosted_search(
        &self,
        key: &str,
        saved: &mut SearchOperation,
        execution_id: &str,
        record: &SessionRecord,
        code: &str,
    ) -> Result<HostedDispatch> {
        saved.outcome = Some(terminal_outcome(saved, code, false));
        saved.completed_at_ms = Some(util::now_ms());
        saved.known_usage = None;
        if self.save_search_operation(key, saved).await.is_err() {
            return pending().await;
        }
        self.finish_hosted_search(key, saved, execution_id, record)
            .await
    }

    async fn finish_hosted_search(
        &self,
        key: &str,
        saved: &mut SearchOperation,
        execution_id: &str,
        record: &SessionRecord,
    ) -> Result<HostedDispatch> {
        if let Some(result) = &saved.published_result {
            return Ok(HostedDispatch::Ready(result.clone().into()));
        }
        if !saved.settled {
            if needs_hold_restore(saved) {
                let restored = service_metering::restore_existing_hold(
                    &self.env,
                    &record.owner_id,
                    Some(&record.id),
                    execution_id,
                    &saved.digest,
                    saved.admitted_at_ms,
                    budget(saved),
                )
                .await;
                match restored {
                    Ok(Some(hold)) => {
                        saved.hold = Some(hold);
                        if self.save_search_operation(key, saved).await.is_err() {
                            return pending().await;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => return pending().await,
                }
            }
            // A stored receipt cannot be published as financially complete
            // without its original reservation. Restoration never creates one.
            if missing_known_hold(saved) {
                return pending().await;
            }
            if !saved.dispatched {
                if let Some(hold) = &saved.hold {
                    if service_metering::release_known_not_dispatched(
                        &self.env,
                        &record.owner_id,
                        Some(&record.id),
                        hold,
                    )
                    .await
                    .is_err()
                    {
                        return pending().await;
                    }
                }
                saved.settled = true;
                if self.save_search_operation(key, saved).await.is_err() {
                    return pending().await;
                }
            }
            if let (Some(hold), Some(usage)) = (&saved.hold, &saved.known_usage) {
                if service_metering::settle_hold(
                    &self.env,
                    &record.owner_id,
                    Some(&record.id),
                    hold,
                    usage,
                )
                .await
                .is_err()
                {
                    return pending().await;
                }
                saved.settled = true;
                if self.save_search_operation(key, saved).await.is_err() {
                    return pending().await;
                }
            }
        }
        let outcome = saved
            .outcome
            .as_ref()
            .ok_or_else(|| fault("hosted_search_outcome_missing"))?;
        let item = outcome.public_item(&hosted_item_id(execution_id));
        if let Err(error) = self.publish_hosted_item(&item, true).await {
            if super::permanent_publication_error(&error) {
                return Err(error);
            }
            return pending().await;
        }
        let result = outcome.model_result().to_string();
        saved.published_result = Some(result.clone());
        if self.save_search_operation(key, saved).await.is_err() {
            return pending().await;
        }
        Ok(HostedDispatch::Ready(result.into()))
    }
}

fn needs_hold_restore(operation: &SearchOperation) -> bool {
    !operation.settled && operation.hold.is_none()
}

fn missing_known_hold(operation: &SearchOperation) -> bool {
    operation.dispatched && operation.known_usage.is_some() && operation.hold.is_none()
}

fn budget(operation: &SearchOperation) -> ServiceBudget {
    match operation.plan.route {
        SearchRouteKind::PlatformDeepseek | SearchRouteKind::DeepseekByok => {
            ServiceBudget::DeepseekRequest {
                platform_funded: operation.plan.platform_funded,
                not_after_ms: operation.admitted_at_ms.saturating_add(300_000),
                price_revision: crate::search_deepseek::PRICE_REVISION.into(),
            }
        }
        SearchRouteKind::OpenaiResponsesByok => ServiceBudget::ReportedCost {
            platform_funded: false,
            ceiling_microusd: 0,
            unit: "provider_request".into(),
            price_revision: "openai-responses-search-byok-v1".into(),
        },
    }
}

fn known_usage(outcome: &HostedSearchOutcome, completed_at_ms: u64) -> Option<KnownUsage> {
    let provider_request_id = outcome
        .exchange
        .response_id
        .clone()
        .or_else(|| outcome.exchange.request_id.clone());
    if let Some(receipt) = &outcome.receipt {
        return Some(KnownUsage::DeepseekRequest {
            receipt: receipt.clone(),
            started_at_ms: outcome.request_started_ms,
            completed_at_ms,
            provider_request_id,
        });
    }
    if outcome.plan.route == SearchRouteKind::OpenaiResponsesByok
        && !outcome
            .exchange
            .failure
            .as_ref()
            .is_some_and(|failure| failure.outcome_unknown)
    {
        return Some(KnownUsage::ReportedCost {
            units: 1,
            reported_cost_usd: outcome.exchange.usage.cost.clone(),
            provider_request_id,
        });
    }
    None
}

fn terminal_outcome(operation: &SearchOperation, code: &str, unknown: bool) -> HostedSearchOutcome {
    HostedSearchOutcome {
        exchange: SearchExchange {
            response_id: None,
            request_id: None,
            status: SearchStatus::Failed,
            evidence: None,
            usage: Usage::default(),
            raw_usage: None,
            incomplete_details: None,
            failure: Some(SearchFailure {
                code: code.into(),
                http_status: None,
                outcome_unknown: unknown,
            }),
        },
        receipt: None,
        platform_funded: operation.plan.platform_funded,
        request_started_ms: operation
            .request_started_ms
            .unwrap_or(operation.admitted_at_ms),
        plan: operation.plan.clone(),
    }
}

fn operation_key(execution_id: &str) -> String {
    format!(
        "hosted:search:{:x}",
        Sha256::digest(execution_id.as_bytes())
    )
}

fn request_digest(
    owner: &str,
    session: &str,
    execution_id: &str,
    binding: &ResolvedBinding,
    call: &provider::ToolCall,
) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            owner,
            session,
            execution_id,
            binding,
            call
        ))?)
    ))
}

fn cancelled_result() -> String {
    json!({"status":"failed","error":"search_cancelled","outcome_unknown":false}).to_string()
}

fn fault(code: &str) -> Error {
    Error::RustError(code.into())
}

async fn pending() -> Result<HostedDispatch> {
    Delay::from(std::time::Duration::from_millis(500)).await;
    Ok(HostedDispatch::Pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation() -> SearchOperation {
        SearchOperation {
            digest: "a".repeat(64),
            admitted_at_ms: 100,
            plan: SearchPlan {
                route: SearchRouteKind::PlatformDeepseek,
                platform_funded: true,
                action: crate::search_deepseek::DeepseekAction::Search {
                    query: "Rust".into(),
                },
            },
            hold: None,
            dispatched: false,
            request_started_ms: None,
            outcome: None,
            completed_at_ms: None,
            known_usage: None,
            settled: false,
            published_result: None,
        }
    }

    #[test]
    fn interrupted_dispatch_preserves_unknown_and_cannot_produce_usage_receipt() {
        let mut operation = operation();
        operation.dispatched = true;
        operation.request_started_ms = Some(120);
        let outcome = terminal_outcome(&operation, "search_provider_outcome_unknown", true);
        assert!(known_usage(&outcome, 140).is_none());
        assert!(outcome.exchange.failure.unwrap().outcome_unknown);
        let encoded = serde_json::to_vec(&operation).unwrap();
        let restored: SearchOperation = serde_json::from_slice(&encoded).unwrap();
        assert!(restored.dispatched && restored.outcome.is_none());
        assert_eq!(restored.admitted_at_ms, 100);
    }

    #[test]
    fn deepseek_budget_is_fixed_to_original_admission_and_provider_revision() {
        match budget(&operation()) {
            ServiceBudget::DeepseekRequest {
                platform_funded,
                not_after_ms,
                price_revision,
            } => {
                assert!(platform_funded);
                assert_eq!(not_after_ms, 300100);
                assert_eq!(price_revision, crate::search_deepseek::PRICE_REVISION);
            }
            _ => panic!("wrong search budget"),
        }
        assert_eq!(operation_key("tool-a"), operation_key("tool-a"));
        assert_ne!(operation_key("tool-a"), operation_key("tool-b"));
    }

    #[test]
    fn request_identity_binds_original_arguments_owner_session_and_service() {
        let binding: ResolvedBinding = serde_json::from_value(json!({
            "tool":{"kind":"search","options":{"type":"web_search"},
                "service":{"provider":"platform_deepseek"},"query_char_limit":128},
            "allowed_callers":null
        }))
        .unwrap();
        let call: provider::ToolCall = serde_json::from_value(json!({"id":"call1","type":"function",
            "function":{"name":"__agents_web_search","arguments":"{\"type\":\"search\",\"query\":\"Rust\"}"}})).unwrap();
        let digest = request_digest("owner", "session", "execution", &binding, &call).unwrap();
        assert_ne!(
            digest,
            request_digest("other", "session", "execution", &binding, &call).unwrap()
        );
        assert_ne!(
            digest,
            request_digest("owner", "other", "execution", &binding, &call).unwrap()
        );
        assert_ne!(
            digest,
            request_digest("owner", "session", "other", &binding, &call).unwrap()
        );
        let mut changed = call.clone();
        changed.function.arguments.push(' ');
        assert_ne!(
            digest,
            request_digest("owner", "session", "execution", &binding, &changed).unwrap()
        );
        let mut changed = binding.clone();
        if let crate::hosted_bindings::ResolvedTool::Search {
            query_char_limit, ..
        } = &mut changed.tool
        {
            *query_char_limit = 127;
        }
        assert_ne!(
            digest,
            request_digest("owner", "session", "execution", &changed, &call).unwrap()
        );
    }

    #[test]
    fn cancellation_and_known_receipts_restore_original_missing_hold_without_redispatch() {
        let mut operation = operation();
        // A reserve may have committed before its DO hold copy. Even an expired
        // never-dispatched cancellation must read back that same reservation.
        assert!(needs_hold_restore(&operation));
        assert!(!missing_known_hold(&operation));
        operation.outcome = Some(terminal_outcome(&operation, "search_cancelled", false));
        assert!(needs_hold_restore(&operation));
        assert_eq!(operation.admitted_at_ms, 100);
        operation.dispatched = true;
        operation.known_usage = Some(KnownUsage::ReportedCost {
            units: 1,
            reported_cost_usd: None,
            provider_request_id: Some("request".into()),
        });
        // Known usage must wait for read-only restoration, not reserve again or
        // skip settlement because the local copy is missing.
        assert!(needs_hold_restore(&operation));
        assert!(missing_known_hold(&operation));
        operation.settled = true;
        assert!(!needs_hold_restore(&operation));
    }
}
