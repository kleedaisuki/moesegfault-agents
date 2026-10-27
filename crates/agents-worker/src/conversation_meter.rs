//! Funded Conversation I/O. An unknown measurement retains its existing hold.

use std::future::Future;

use agents_auth::billing::Resource;
use serde::{Deserialize, Serialize};
use worker::{D1PreparedStatement, D1Result, Env, Error, Headers, Response, Result};

use crate::{reconcile, resources::ResourceMeter, util};

pub(crate) const HEADER: &str = "X-Agents-Conversation-Meter";
const MAX_GRANT_BYTES: usize = 1024;

/// This grant travels only through freshly constructed internal requests. It is
/// not an authentication token: the actor independently verifies its owner and
/// ResourceMeter verifies that owner/session's still-reserved funding row.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    run_id: String,
    session_id: Option<String>,
    event: String,
}

impl Grant {
    fn parse(value: &str) -> Result<Self> {
        if value.len() > MAX_GRANT_BYTES {
            return Err(fail("invalid_conversation_meter_grant"));
        }
        let grant: Self =
            serde_json::from_str(value).map_err(|_| fail("invalid_conversation_meter_grant"))?;
        if !identifier(&grant.run_id)
            || !identifier(&grant.event)
            || grant.session_id.as_ref().is_some_and(|id| !identifier(id))
        {
            return Err(fail("invalid_conversation_meter_grant"));
        }
        Ok(grant)
    }

    fn meter<'a>(&'a self, env: &'a Env, owner: &'a str) -> Result<ResourceMeter<'a>> {
        match self.session_id.as_deref() {
            Some(session) => ResourceMeter::new(env, owner, session, &self.run_id),
            None => ResourceMeter::account(env, owner, &self.run_id),
        }
    }

    async fn require_funding(&self, env: &Env, owner: &str) -> Result<()> {
        #[derive(Deserialize)]
        struct Funded {
            id: String,
        }
        let session = self
            .session_id
            .as_deref()
            .map(Into::into)
            .unwrap_or(wasm_bindgen::JsValue::NULL);
        let funded = env
            .d1("DB")?
            .prepare(FUNDING_SQL)
            .bind(&[
                self.run_id.as_str().into(),
                owner.into(),
                session,
                self.event.as_str().into(),
            ])?
            .first::<Funded>(None)
            .await?;
        if funded.is_none_or(|row| row.id != self.run_id) {
            return Err(fail("conversation_meter_funding_required"));
        }
        Ok(())
    }
}

const FUNDING_SQL: &str = "SELECT r.id FROM usage_reservations r WHERE r.id=?1 AND r.account_id=?2 AND r.session_id IS ?3 AND r.state='reserved' AND EXISTS(SELECT 1 FROM resource_observations o WHERE o.run_id=r.id AND o.account_id=r.account_id AND o.session_id IS r.session_id AND o.event_key=?4 AND o.resource='durable_object' AND o.unit='wall_ms' AND o.status='pending')";

pub(crate) struct ConversationOperation {
    run_id: String,
    session_id: Option<String>,
    parent_event: Option<String>,
    parent_resource: Resource,
    scope: Scope,
    started_at_ms: u64,
    trace: crate::telemetry::TraceContext,
}

enum Scope {
    Request,
    Response { finished: bool },
    Session,
}

impl ConversationOperation {
    /// Fund before the first routing query, including reads that return 404.
    pub(crate) async fn account(env: &Env, owner: &str) -> Result<Self> {
        Self::account_resource(env, owner, Resource::Worker).await
    }

    pub(crate) async fn account_resource(
        env: &Env,
        owner: &str,
        resource: Resource,
    ) -> Result<Self> {
        if !matches!(resource, Resource::Worker | Resource::DurableObject) {
            return Err(fail("invalid_conversation_meter_resource"));
        }
        crate::allowances::sync(env, owner).await?;
        let operation = Self {
            run_id: format!("conversation:{}", uuid::Uuid::new_v4()),
            session_id: None,
            parent_event: Some(event("worker")?),
            parent_resource: resource,
            scope: Scope::Request,
            started_at_ms: util::now_ms(),
            trace: crate::telemetry::TraceContext::from_traceparent(None),
        };
        operation.meter(env, owner)?.preflight(false)?;
        reconcile::maintenance_account_reservation(env, owner, &operation.run_id).await?;
        operation.require_parent(env, owner).await?;
        Ok(operation)
    }

    /// Retain one cleanup hold until canonical release, even when later model
    /// credit admission fails. Each invocation measures only its own RPC time.
    pub(crate) async fn response(env: &Env, owner: &str, response_id: &str) -> Result<Self> {
        Self::response_scope(env, owner, response_id, Resource::Worker).await
    }

    pub(crate) async fn response_actor(env: &Env, owner: &str, response_id: &str) -> Result<Self> {
        Self::response_scope(env, owner, response_id, Resource::DurableObject).await
    }

    async fn response_scope(
        env: &Env,
        owner: &str,
        response_id: &str,
        parent_resource: Resource,
    ) -> Result<Self> {
        let run_id = response_operation_id(response_id)?;
        let state = response_funding(env, owner, &run_id).await?;
        let finished = state.as_deref() == Some("settled");
        if state
            .as_deref()
            .is_some_and(|state| !matches!(state, "reserved" | "settled"))
        {
            return Err(fail("conversation_response_funding_unavailable"));
        }
        let operation = Self {
            run_id,
            session_id: None,
            parent_event: (!finished).then(|| event("parent")).transpose()?,
            parent_resource,
            scope: Scope::Response { finished },
            started_at_ms: util::now_ms(),
            trace: crate::telemetry::TraceContext::from_traceparent(None),
        };
        if finished {
            return Ok(operation);
        }
        operation.meter(env, owner)?.preflight(false)?;
        if state.is_none() {
            crate::allowances::sync(env, owner).await?;
            reconcile::maintenance_account_reservation(env, owner, &operation.run_id).await?;
        }
        operation.require_parent(env, owner).await?;
        Ok(operation)
    }

    /// The admitted caller measures its own wall time and owns settlement.
    pub(crate) fn session(run_id: &str, session_id: &str) -> Result<Self> {
        if !identifier(run_id) || !identifier(session_id) {
            return Err(fail("invalid_conversation_meter_grant"));
        }
        Ok(Self {
            run_id: run_id.into(),
            session_id: Some(session_id.into()),
            parent_event: None,
            parent_resource: Resource::DurableObject,
            scope: Scope::Session,
            started_at_ms: 0,
            trace: crate::telemetry::TraceContext::from_traceparent(None),
        })
    }

    /// Use only inside actor's funding-validated work. The enclosing caller owns
    /// this reservation and parent interval, whether account- or Session-scoped.
    pub(crate) fn actor_scope(headers: &Headers) -> Result<Self> {
        let raw = headers
            .get(HEADER)?
            .ok_or_else(|| fail("conversation_meter_grant_required"))?;
        let grant = Grant::parse(&raw)?;
        let trace = crate::telemetry::TraceContext::from_traceparent(
            headers.get("traceparent")?.as_deref(),
        );
        Ok(Self {
            run_id: grant.run_id,
            session_id: grant.session_id,
            parent_event: None,
            parent_resource: Resource::DurableObject,
            scope: Scope::Session,
            started_at_ms: 0,
            trace,
        })
    }

    pub(crate) fn with_traceparent(mut self, traceparent: Option<&str>) -> Self {
        if let Some(trace) = traceparent.and_then(crate::telemetry::TraceContext::parse) {
            self.trace = trace.child();
        }
        self
    }

    pub(crate) fn emit(&self, phase: &str, fields: serde_json::Value) {
        let mut value = serde_json::json!({"operation_id":self.run_id,"session_id":self.session_id,
            "trace_id":self.trace.trace_id(),"span_id":self.trace.span_id(),"phase":phase});
        if let (Some(target), Some(fields)) = (value.as_object_mut(), fields.as_object()) {
            target.extend(fields.clone());
        }
        util::emit("conversation.phase", value);
    }

    fn meter<'a>(&'a self, env: &'a Env, owner: &'a str) -> Result<ResourceMeter<'a>> {
        match self.session_id.as_deref() {
            Some(session) => ResourceMeter::new(env, owner, session, &self.run_id),
            None => ResourceMeter::account(env, owner, &self.run_id),
        }
    }

    async fn require_parent(&self, env: &Env, owner: &str) -> Result<()> {
        let Some(event) = self.parent_event.as_deref() else {
            return Ok(());
        };
        self.meter(env, owner)?
            .require_observation(event, self.parent_resource.clone(), "wall_ms")
            .await
    }

    pub(crate) fn id(&self) -> &str {
        &self.run_id
    }

    pub(crate) fn is_finished(&self) -> bool {
        matches!(self.scope, Scope::Response { finished: true })
    }

    pub(crate) async fn response_closed(
        &self,
        env: &Env,
        owner: &str,
        response: &str,
    ) -> Result<bool> {
        #[derive(Deserialize)]
        struct Funding {
            state: String,
        }
        let id = response_operation_id(response)?;
        let rows = self
            .query(
                env,
                owner,
                "response-closed",
                env.d1("DB")?
                    .prepare("SELECT state FROM usage_reservations WHERE id=?1 AND account_id=?2")
                    .bind(&[id.as_str().into(), owner.into()])?,
            )
            .await?
            .results::<Funding>()?;
        Ok(rows.first().is_some_and(|row| row.state == "settled"))
    }

    pub(crate) async fn settle_reclaimed_response(
        &self,
        env: &Env,
        owner: &str,
        response: &str,
    ) -> Result<()> {
        let run = response_operation_id(response)?;
        let rows = self.query(env, owner, "reclaimed-funding", env.d1("DB")?
            .prepare("SELECT state,request_hash,idempotency_key,session_id FROM usage_reservations WHERE id=?1 AND account_id=?2")
            .bind(&[run.as_str().into(), owner.into()])?).await?.results::<ResponseFunding>()?;
        let Some(row) = rows.first() else {
            return Ok(());
        };
        validate_response_funding(row, owner, &run)?;
        if row.state == "settled" {
            return Ok(());
        }
        if row.state != "reserved" {
            return Err(fail("conversation_response_funding_unavailable"));
        }
        // Settle only the original observed ledger. No interval is re-recorded,
        // no old price is recomputed, and settle_run keeps any pending hold.
        ResourceMeter::account(env, owner, &run)?
            .settle_run()
            .await?;
        Ok(())
    }

    fn require_open(&self) -> Result<()> {
        if self.is_finished() {
            return Err(fail("conversation_response_operation_finished"));
        }
        Ok(())
    }

    /// Missing metadata stays pending; affected-row counts are not a substitute.
    pub(crate) async fn query(
        &self,
        env: &Env,
        owner: &str,
        label: &str,
        statement: D1PreparedStatement,
    ) -> Result<D1Result> {
        self.require_open()?;
        let event = event(label)?;
        let started = util::now_ms();
        self.emit(
            "d1.started",
            serde_json::json!({"request_id":event,"request_path":label}),
        );
        let report = |phase: &str, error: &Error| {
            let mut fields = error_metadata(error, started);
            fields["request_id"] = serde_json::json!(event);
            fields["request_path"] = serde_json::json!(label);
            self.emit(phase, fields);
        };
        let meter = self.meter(env, owner)?;
        if let Err(error) = meter.require_d1(&event).await {
            report("d1.funding_failed", &error);
            return Err(error);
        }
        let funded = util::now_ms();
        let result = match statement.all().await {
            Ok(result) => result,
            Err(error) => {
                report("d1.rpc_failed", &error);
                return Err(error);
            }
        };
        let returned = util::now_ms();
        if let Err(error) = meter.record_d1(&event, &result).await {
            report("d1.observation_failed", &error);
            return Err(error);
        }
        let finished = util::now_ms();
        self.emit("d1.completed", serde_json::json!({"request_id":event,"request_path":label,
            "funding_ms":funded.saturating_sub(started),"rpc_ms":returned.saturating_sub(funded),
            "observation_ms":finished.saturating_sub(returned),"duration_ms":finished.saturating_sub(started)}));
        Ok(result)
    }

    /// Never reuse a grant after an ambiguous transport failure. A retry gets a
    /// new observation while the old uncertain observation keeps its budget hold.
    /// A fixed registry batch preserves exact measurements in this actor scope.
    pub(crate) async fn external_d1<F>(
        &self,
        env: &Env,
        owner: &str,
        label: &str,
        count: usize,
        work: F,
    ) -> Result<Vec<D1Result>>
    where
        F: Future<Output = Result<Vec<D1Result>>>,
    {
        self.require_open()?;
        let meter = self.meter(env, owner)?;
        let mut events = vec![];
        for _ in 0..count {
            let event = event(label)?;
            meter.require_d1(&event).await?;
            events.push(event);
        }
        let results = work.await?;
        if results.len() != events.len() {
            return Err(fail("conversation_d1_measurement_missing"));
        }
        for (event, result) in events.iter().zip(&results) {
            meter.record_d1(event, result).await?;
        }
        Ok(results)
    }

    pub(crate) async fn actor_headers(
        &self,
        env: &Env,
        owner: &str,
        label: &str,
        headers: &Headers,
    ) -> Result<()> {
        self.require_open()?;
        let grant = Grant {
            run_id: self.run_id.clone(),
            session_id: self.session_id.clone(),
            event: event(label)?,
        };
        grant
            .meter(env, owner)?
            .require_observation(&grant.event, Resource::DurableObject, "wall_ms")
            .await?;
        self.trace.child().inject(headers)?;
        headers.set(HEADER, &serde_json::to_string(&grant)?)
    }

    /// Finish one RPC. Response lifecycle funding deliberately remains reserved.
    pub(crate) async fn finish(&self, env: &Env, owner: &str) -> Result<()> {
        self.finish_interval(env, owner).await?;
        if matches!(self.scope, Scope::Request) {
            self.meter(env, owner)?.settle_run().await?;
        }
        Ok(())
    }

    /// Call instead of finish only after canonical terminal/rejection release.
    pub(crate) async fn finish_response(&self, env: &Env, owner: &str) -> Result<()> {
        if !matches!(self.scope, Scope::Response { .. }) {
            return Err(fail("conversation_response_scope_required"));
        }
        if self.is_finished() {
            return Ok(());
        }
        self.finish_interval(env, owner).await?;
        self.meter(env, owner)?.settle_run().await?;
        Ok(())
    }

    async fn finish_interval(&self, env: &Env, owner: &str) -> Result<()> {
        let Some(event) = self.parent_event.as_deref() else {
            return Ok(());
        };
        self.meter(env, owner)?
            .record_wall_time(
                event,
                self.parent_resource.clone(),
                self.started_at_ms,
                util::now_ms(),
            )
            .await
    }

    /// Accounting acknowledgment loss does not turn a definite committed HTTP
    /// result into a retriable mutation. The funding/observation rows remain.
    pub(crate) async fn finish_result(
        &self,
        env: &Env,
        owner: &str,
        result: Result<Response>,
        release: bool,
    ) -> Result<Response> {
        let accounting = if release {
            self.finish_response(env, owner).await
        } else {
            self.finish(env, owner).await
        };
        let response = result?;
        let headers = response.headers().clone();
        let mut response = response.with_headers(headers);
        response
            .headers_mut()
            .set("X-Agents-Operation", self.id())?;
        if accounting.is_err() {
            response
                .headers_mut()
                .set("X-Agents-Accounting-Status", "pending")?;
            util::emit(
                "conversation.accounting_deferred",
                serde_json::json!({"operation_id":self.id()}),
            );
        }
        Ok(response)
    }
}

#[derive(Deserialize)]
struct ResponseFunding {
    state: String,
    request_hash: String,
    idempotency_key: String,
    session_id: Option<String>,
}

async fn response_funding(env: &Env, owner: &str, run: &str) -> Result<Option<String>> {
    let row = env.d1("DB")?.prepare("SELECT state,request_hash,idempotency_key,session_id FROM usage_reservations WHERE id=? AND account_id=?")
        .bind(&[run.into(),owner.into()])?.first::<ResponseFunding>(None).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    // Reuse the original amount. A deployment's changed reservation default must
    // not require fresh credit or strand an already activated writer lease.
    validate_response_funding(&row, owner, run)?;
    Ok(Some(row.state))
}

fn validate_response_funding(row: &ResponseFunding, owner: &str, run: &str) -> Result<()> {
    if row.session_id.is_some()
        || row.idempotency_key != format!("maintenance:{run}")
        || row.request_hash != format!("account:{}:{owner}:{run}", owner.len())
    {
        return Err(fail("conversation_response_funding_conflict"));
    }
    Ok(())
}

fn response_operation_id(response: &str) -> Result<String> {
    if !response.starts_with("resp_")
        || response.len() > 128
        || !response
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(fail("invalid_response_id"));
    }
    Ok(format!("conv-response:{response}"))
}

/// Run only after independently checking the Conversation's owner binding.
/// Grant verification/funding precedes reading or writing its user-data keys.
pub(crate) async fn actor<F>(env: &Env, owner: &str, headers: &Headers, work: F) -> Result<Response>
where
    F: Future<Output = Result<Response>>,
{
    let raw = headers
        .get(HEADER)?
        .ok_or_else(|| fail("conversation_meter_grant_required"))?;
    let grant = Grant::parse(&raw)?;
    let operation = ConversationOperation::actor_scope(headers)?;
    let checking = util::now_ms();
    operation.emit(
        "grant.check",
        serde_json::json!({"grant_event":grant.event}),
    );
    if let Err(error) = grant.require_funding(env, owner).await {
        operation.emit("grant.failed", error_metadata(&error, checking));
        return Err(error);
    }
    operation.emit("grant.validated", serde_json::json!({"grant_event":grant.event,"duration_ms":util::now_ms().saturating_sub(checking)}));
    let meter = grant.meter(env, owner)?;
    meter
        .require_observation(&grant.event, Resource::DurableObject, "wall_ms")
        .await?;
    let started = util::now_ms();
    let result = work.await;
    operation.emit("actor.work_completed", serde_json::json!({"grant_event":grant.event,"duration_ms":util::now_ms().saturating_sub(started),"status":result.as_ref().ok().map(|response|response.status_code())}));
    let observed = meter
        .record_wall_time(
            &grant.event,
            Resource::DurableObject,
            started,
            util::now_ms(),
        )
        .await;
    if observed.is_err() {
        // The application may already have committed. Its definite response is
        // not converted into a retriable mutation just because accounting lost
        // an acknowledgment; the declared observation keeps its funding held.
        util::emit(
            "conversation.accounting_deferred",
            serde_json::json!({"operation_id":grant.run_id,"event_key":grant.event}),
        );
        return match result {
            Ok(mut response) => {
                response
                    .headers_mut()
                    .set("X-Agents-Accounting-Status", "pending")?;
                Ok(response)
            }
            Err(error) => Err(error),
        };
    }
    result
}

pub(crate) fn error_metadata(error: &Error, started: u64) -> serde_json::Value {
    let kind = match error {
        Error::D1(_) => "d1_error",
        Error::SerdeJsonError(_) => "json_decode",
        Error::SerdeWasmBindgenError(_) => "binding_decode",
        Error::BindingError(_) => "binding_error",
        Error::RustError(_) => "runtime_error",
        _ => "platform_error",
    };
    let text = error.to_string();
    let domain = match error {
        Error::RustError(value)
            if value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && ["conversation_", "resource_", "invalid_", "d1_", "budget_"]
                    .iter()
                    .any(|prefix| value.starts_with(prefix)) =>
        {
            Some(value.as_str())
        }
        _ => None,
    };
    let code = if let Some(code) = domain {
        code
    } else if text.contains("blockConcurrencyWhile") {
        "platform_concurrency_guard_timeout"
    } else {
        crate::routes::classify_error(&text).1
    };
    serde_json::json!({"error_type":kind,"error_code":code,"duration_ms":util::now_ms().saturating_sub(started)})
}

fn event(label: &str) -> Result<String> {
    if label.is_empty()
        || label.len() > 96
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:-".contains(&byte))
    {
        return Err(fail("invalid_conversation_meter_event"));
    }
    Ok(format!("conversation:{label}:{}", uuid::Uuid::new_v4()))
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| (33..=126).contains(&byte))
}

fn fail(code: &'static str) -> Error {
    Error::RustError(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_rejects_unknown_fields_and_control_characters() {
        assert!(
            Grant::parse(r#"{"run_id":"run","session_id":null,"event":"event","owner":"other"}"#)
                .is_err()
        );
        assert!(
            Grant::parse(r#"{"run_id":"run\nother","session_id":null,"event":"event"}"#).is_err()
        );
        assert!(Grant::parse(r#"{"run_id":"run","session_id":null,"event":"event"}"#).is_ok());
    }

    #[test]
    fn grant_has_a_bounded_private_header() {
        assert!(Grant::parse(&"x".repeat(MAX_GRANT_BYTES + 1)).is_err());
        assert!(!identifier(""));
        assert!(!identifier(&"a".repeat(257)));
        assert!(identifier("conversation:deferred-run"));
    }

    #[test]
    fn actor_event_does_not_accept_unbounded_or_injected_labels() {
        assert!(event("query\r\nheader").is_err());
        assert!(event(&"x".repeat(97)).is_err());
    }

    #[test]
    fn session_scope_does_not_create_a_second_worker_interval() {
        let operation = ConversationOperation::session("run", "session").unwrap();
        assert!(operation.parent_event.is_none());
        assert_eq!(operation.session_id.as_deref(), Some("session"));
    }

    #[test]
    fn response_cleanup_identity_is_stable_and_not_a_path() {
        assert_eq!(
            response_operation_id("resp_a").unwrap(),
            "conv-response:resp_a"
        );
        assert!(response_operation_id("resp_a/other").is_err());
        assert!(response_operation_id("conversation_a").is_err());
    }
}
