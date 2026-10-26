//! Guest-funded RPC transport to a source actor, not to its model harness.
use futures::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::{Env, Error, Headers, Method, Request, RequestInit, Response, Result};

use crate::{
    code_borrowing::Ticket,
    code_contexts::OwnedContext,
    conversation_meter::ConversationOperation,
    telemetry::TraceContext,
    workspace::{PythonArtifact, PythonOutcome, PythonRequest},
};

pub(crate) struct BorrowedExecutor<'a> {
    env: &'a Env,
    owner: &'a str,
    context: &'a OwnedContext,
    ticket: Ticket,
    trace: Option<TraceContext>,
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

impl<'a> BorrowedExecutor<'a> {
    pub(crate) fn new(
        env: &'a Env,
        owner: &'a str,
        context: &'a OwnedContext,
        ticket: Ticket,
        trace: Option<TraceContext>,
    ) -> Result<Self> {
        ticket.supervisor_id(owner, context)?;
        Ok(Self {
            env,
            owner,
            context,
            ticket,
            trace,
        })
    }

    async fn request(&self, action: &str, body: &Value) -> Result<Response> {
        let operation =
            ConversationOperation::session(&self.ticket.guest_run, &self.ticket.guest_session)?;
        let headers = Headers::new();
        headers.set("Content-Type", "application/json")?;
        headers.set("X-Agents-Code-Owner", self.owner)?;
        if let Some(trace) = &self.trace {
            trace.child().inject(&headers)?;
        }
        operation
            .actor_headers(self.env, self.owner, "code-resource", &headers)
            .await?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(body.to_string().into()));
        self.env
            .durable_object(crate::runtime_profiles::binding(self.context.profile))?
            .id_from_name(&format!("{}:{}", self.owner, self.context.source_session))?
            .get_stub()?
            .fetch_with_request(Request::new_with_init(
                &format!("https://code.internal/code-executions/{action}"),
                &init,
            )?)
            .await
    }

    fn reference(&self, artifact_id: Option<&str>) -> Value {
        json!({"context":self.context,"ticket":self.ticket,"artifact_id":artifact_id})
    }

    pub(crate) async fn prepare(&self) -> Result<String> {
        let response = self.request("inspect", &self.reference(None)).await?;
        if response.status_code() != 200 {
            return Err(fail("code_container_not_found"));
        }
        let value: Value = serde_json::from_slice(&read(response, 4096).await?)
            .map_err(|_| fail("code_resource_protocol_error"))?;
        if value["container_id"].as_str() != Some(self.context.id.as_str()) {
            return Err(fail("code_resource_protocol_error"));
        }
        Ok(self.context.id.clone())
    }

    pub(crate) async fn execute(&self, request: &PythonRequest) -> Result<PythonOutcome> {
        if request.container_id.as_deref() != Some(self.context.id.as_str())
            || request.network_policy.is_some()
        {
            return Err(fail("code_resource_binding_mismatch"));
        }
        let response = self
            .request(
                "execute",
                &json!({"context":self.context,"ticket":self.ticket,"request":request}),
            )
            .await?;
        self.outcome(response).await
    }

    async fn outcome(&self, response: Response) -> Result<PythonOutcome> {
        if response.status_code() != 200 {
            return Err(rejection(response).await);
        }
        let outcome: PythonOutcome =
            serde_json::from_slice(&read(response, 32 * 1024 * 1024).await?)
                .map_err(|_| fail("code_resource_protocol_error"))?;
        let id = self.ticket.supervisor_id(self.owner, self.context)?;
        let expected = format!("{:x}", Sha256::digest(id.as_bytes()));
        if outcome.id != expected
            || outcome.container_id.as_deref() != Some(self.context.id.as_str())
        {
            return Err(fail("code_resource_protocol_error"));
        }
        Ok(outcome)
    }

    pub(crate) async fn poll(&self) -> Result<Option<PythonOutcome>> {
        let response = self.request("poll", &self.reference(None)).await?;
        if response.status_code() == 202 {
            return Ok(None);
        }
        self.outcome(response).await.map(Some)
    }

    pub(crate) async fn cancel(&self) -> Result<()> {
        let response = self.request("cancel", &self.reference(None)).await?;
        match response.status_code() {
            200 | 202 | 204 => Ok(()),
            _ => Err(rejection(response).await),
        }
    }

    pub(crate) async fn artifact(&self, artifact: &PythonArtifact) -> Result<Vec<u8>> {
        if artifact.bytes > 16 * 1024 * 1024 {
            return Err(fail("code_artifact_invalid"));
        }
        let response = self
            .request("artifact", &self.reference(Some(&artifact.id)))
            .await?;
        if response.status_code() != 200 {
            return Err(fail("code_artifact_unavailable"));
        }
        let bytes = read(response, 16 * 1024 * 1024).await?;
        if bytes.len() as u64 != artifact.bytes
            || format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256
        {
            return Err(fail("code_artifact_integrity_error"));
        }
        Ok(bytes)
    }

    pub(crate) async fn acknowledge(&self, artifact: &str) -> Result<()> {
        self.empty("ack", Some(artifact)).await
    }
    pub(crate) async fn complete(&self) -> Result<()> {
        self.empty("complete", None).await
    }
    async fn empty(&self, action: &str, artifact: Option<&str>) -> Result<()> {
        let response = self.request(action, &self.reference(artifact)).await?;
        if response.status_code() == 204 {
            Ok(())
        } else {
            Err(fail("code_resource_publication_pending"))
        }
    }
}

async fn read(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut stream = response.stream()?;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(fail("code_resource_reply_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn rejection(response: Response) -> Error {
    let bytes = match read(response, 4096).await {
        Ok(bytes) => bytes,
        Err(_) => return fail("code_resource_outcome_unknown"),
    };
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    match value["error"]["code"].as_str() {
        Some("code_cancelled_before_dispatch") => fail("code_cancelled_before_dispatch"),
        Some("code_execution_lost") => fail("code_execution_lost"),
        Some("code_artifacts_lost") => fail("code_artifacts_lost"),
        Some("code_container_not_found") => fail("code_container_not_found"),
        Some("code_resource_execution_already_published") => {
            fail("code_resource_execution_already_published")
        }
        _ => fail("code_resource_outcome_unknown"),
    }
}
