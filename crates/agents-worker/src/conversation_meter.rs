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
}

enum Scope {
    Request,
    Response { finished: bool },
    Session,
}

impl ConversationOperation {
    /// Fund before the first routing query, including reads that return 404.
    pub(crate) async fn account(env: &Env, owner: &str) -> Result<Self> {
        crate::allowances::sync(env, owner).await?;
        let operation = Self {
            run_id: format!("conversation:{}", uuid::Uuid::new_v4()),
            session_id: None,
            parent_event: Some(event("worker")?),
            parent_resource: Resource::Worker,
            scope: Scope::Request,
            started_at_ms: util::now_ms(),
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
        })
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
        let meter = self.meter(env, owner)?;
        meter.require_d1(&event).await?;
        let result = statement.all().await?;
        meter.record_d1(&event, &result).await?;
        Ok(result)
    }

    /// Never reuse a grant after an ambiguous transport failure. A retry gets a
    /// new observation while the old uncertain observation keeps its budget hold.
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
        let mut response = result?;
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
    if row.session_id.is_some()
        || row.idempotency_key != format!("maintenance:{run}")
        || row.request_hash != format!("account:{}:{owner}:{run}", owner.len())
    {
        return Err(fail("conversation_response_funding_conflict"));
    }
    Ok(Some(row.state))
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
    grant.require_funding(env, owner).await?;
    let meter = grant.meter(env, owner)?;
    meter
        .require_observation(&grant.event, Resource::DurableObject, "wall_ms")
        .await?;
    let started = util::now_ms();
    let result = work.await;
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
