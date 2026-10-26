//! Canonical hosted Python execution and recoverable Space publication.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::{Delay, Error, Result};

use super::{
    HostedDispatch, SessionInner, SessionRecord, execution_key, hosted_item_id, next_tool, provider,
};
use crate::{
    hosted_artifacts::{self, ArtifactIdentity, ArtifactRef},
    hosted_bindings::{CodeContainer, MemoryTier, NetworkPolicy, ResolvedBinding, ResolvedTool},
    session_storage::{read_chunks, write_chunks},
    workspace::{PythonMemoryTier, PythonNetworkPolicy, PythonOutcome, PythonRequest},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    code: String,
}

struct CodeExecution<'a> {
    key: &'a str,
    id: &'a str,
    source: &'a str,
    run: &'a str,
    options: &'a crate::hosted_bindings::CodeOptions,
}

#[derive(Serialize, Deserialize)]
struct CodeOperation {
    fingerprint: String,
    supervisor_id: String,
    context: Option<String>,
    imported: usize,
    #[serde(default)]
    prepared: bool,
    submitted: bool,
    published: Vec<Option<ArtifactRef>>,
    #[serde(default)]
    deleted: BTreeSet<usize>,
    acknowledged: BTreeSet<usize>,
    publication_complete: bool,
    failure: Option<String>,
    result: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum CodeAction {
    CancelUnsubmitted,
    CancelAndPoll,
    Execute,
    DrainKnown,
}

fn action(cancelled: bool, submitted: bool, known: bool) -> CodeAction {
    match (known, cancelled, submitted) {
        (true, _, _) => CodeAction::DrainKnown,
        (false, true, false) => CodeAction::CancelUnsubmitted,
        (false, true, true) => CodeAction::CancelAndPoll,
        _ => CodeAction::Execute,
    }
}

fn cancel_unsubmitted(operation: &mut CodeOperation) {
    operation.submitted = false;
    operation.failure = Some("response_cancelled".into());
    operation.publication_complete = true;
    operation.result = Some(
        json!({"cancelled":true,"error":"response_cancelled","outputs":[],"files":[]}).to_string(),
    );
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

fn identity(
    owner: &str,
    session: &str,
    execution: &str,
    binding: &ResolvedBinding,
    arguments: &str,
) -> Result<String> {
    let bytes = serde_json::to_vec(&(owner, session, execution, binding, arguments))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn scope_identity(owner: &str, session: &str, execution: &str) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(owner, session, execution))?)
    ))
}

async fn save(storage: &worker::Storage, key: &str, operation: &CodeOperation) -> Result<()> {
    write_chunks(storage, key, &serde_json::to_vec(operation)?).await
}

async fn pending() -> Result<HostedDispatch> {
    Delay::from(std::time::Duration::from_millis(500)).await;
    Ok(HostedDispatch::Pending)
}

fn permanent(error: &Error) -> Option<&'static str> {
    let Error::RustError(code) = error else {
        return None;
    };
    match code.as_str() {
        "code_container_not_found" | "code_context_generation_changed" => {
            Some("code_container_not_found")
        }
        "file_not_found" => Some("file_not_found"),
        "code_memory_profile_mismatch" => Some("code_memory_profile_mismatch"),
        "code_network_policy_conflict" => Some("code_network_policy_conflict"),
        "invalid_code_input" | "code_input_rejected" => Some("code_input_rejected"),
        "invalid_code_request" | "invalid_code_execution_id" | "code_request_rejected" => {
            Some("code_request_rejected")
        }
        _ => None,
    }
}

impl SessionInner {
    fn hosted_code_cancel_requested(&self) -> bool {
        self.data
            .borrow()
            .as_ref()
            .and_then(|data| data.run.as_ref())
            .and_then(|run| run.response.as_ref())
            .is_some_and(|response| response.cancel_requested)
    }

    /// The cancellation endpoint may call this outside the long-running step
    /// gate after persisting its flag. It never imports, restores or executes.
    pub(in crate::session) async fn cancel_current_hosted_code(
        &self,
        response_id: &str,
        record: &SessionRecord,
    ) -> Result<()> {
        let execution = {
            let data = self.data.borrow();
            let Some(data) = data.as_ref() else {
                return Ok(());
            };
            let Some(run) = data.run.as_ref() else {
                return Ok(());
            };
            if run
                .response
                .as_ref()
                .is_none_or(|response| response.id != response_id)
            {
                return Ok(());
            }
            let Some(call) = next_tool(&data.messages, run.response.as_ref()) else {
                return Ok(());
            };
            if crate::hosted_tools::Kind::from_alias(&call.function.name)
                != Some(crate::hosted_tools::Kind::CodeInterpreter)
            {
                return Ok(());
            }
            execution_key(&run.id, run.round, &call.id)
        };
        let scope = scope_identity(&record.owner_id, &record.id, &execution)?;
        let key = format!("hosted:code:{scope}");
        let storage = self.state.storage();
        let Some(bytes) = read_chunks(&storage, &key).await? else {
            return Ok(());
        };
        let operation: CodeOperation =
            serde_json::from_slice(&bytes).map_err(|_| fail("hosted_code_operation_corrupt"))?;
        if operation.supervisor_id != format!("pyexec_{scope}") {
            return Err(fail("hosted_code_operation_conflict"));
        }
        if operation.result.is_some()
            || read_chunks(&storage, &format!("{key}:outcome"))
                .await?
                .is_some()
        {
            return Ok(());
        }
        self.workspace(record)
            .cancel_python(&operation.supervisor_id)
            .await
    }
    pub(in crate::session) async fn dispatch_hosted_code(
        &self,
        record: &SessionRecord,
        execution_id: &str,
        call: &provider::ToolCall,
        binding: &ResolvedBinding,
    ) -> Result<HostedDispatch> {
        match self
            .hosted_code_step(record, execution_id, call, binding)
            .await
        {
            Ok(dispatch) => Ok(dispatch),
            Err(error) if super::permanent_publication_error(&error) => Err(error),
            Err(error) if permanent(&error).is_some() => Ok(HostedDispatch::Ready(
                json!({"error":permanent(&error).unwrap(),"outputs":[],"files":[]})
                    .to_string()
                    .into(),
            )),
            Err(Error::RustError(code))
                if matches!(
                    code.as_str(),
                    "hosted_code_binding_mismatch"
                        | "hosted_code_operation_conflict"
                        | "hosted_code_operation_corrupt"
                        | "hosted_code_outcome_corrupt"
                        | "run_missing"
                ) =>
            {
                Err(Error::RustError(code))
            }
            Err(_) => pending().await,
        }
    }

    async fn hosted_code_step(
        &self,
        record: &SessionRecord,
        execution_id: &str,
        call: &provider::ToolCall,
        binding: &ResolvedBinding,
    ) -> Result<HostedDispatch> {
        let ResolvedTool::Code { options } = &binding.tool else {
            return Err(fail("hosted_code_binding_mismatch"));
        };
        let arguments: Arguments = match serde_json::from_str(&call.function.arguments) {
            Ok(arguments) => arguments,
            Err(_) => {
                return Ok(HostedDispatch::Ready(
                    json!({"error":"invalid_code_arguments"}).to_string().into(),
                ));
            }
        };
        if arguments.code.is_empty()
            || arguments.code.len() > 65536
            || arguments.code.contains('\0')
        {
            return Ok(HostedDispatch::Ready(
                json!({"error":"invalid_code_arguments"}).to_string().into(),
            ));
        }
        let fingerprint = identity(
            &record.owner_id,
            &record.id,
            execution_id,
            binding,
            &call.function.arguments,
        )?;
        let scope = scope_identity(&record.owner_id, &record.id, execution_id)?;
        let key = format!("hosted:code:{scope}");
        let storage = self.state.storage();
        let mut operation: CodeOperation = match read_chunks(&storage, &key).await? {
            Some(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| fail("hosted_code_operation_corrupt"))?
            }
            None => {
                let operation = CodeOperation {
                    fingerprint: fingerprint.clone(),
                    supervisor_id: format!("pyexec_{scope}"),
                    context: match &options.container {
                        CodeContainer::Existing { id } => Some(id.clone()),
                        CodeContainer::Auto { .. } => None,
                    },
                    imported: 0,
                    prepared: false,
                    submitted: false,
                    published: vec![],
                    deleted: BTreeSet::new(),
                    acknowledged: BTreeSet::new(),
                    publication_complete: false,
                    failure: None,
                    result: None,
                };
                save(&storage, &key, &operation).await?;
                operation
            }
        };
        if operation.fingerprint != fingerprint
            || operation.supervisor_id != format!("pyexec_{scope}")
        {
            return Err(fail("hosted_code_operation_conflict"));
        }
        let run_id = self
            .snapshot()?
            .run
            .as_ref()
            .ok_or_else(|| fail("run_missing"))?
            .id
            .clone();
        if !operation.submitted
            && operation.result.is_none()
            && read_chunks(&storage, &format!("{key}:outcome"))
                .await?
                .is_none()
        {
            self.begin_local_code_prepare(record, &run_id, options)
                .await?;
        }
        let _resource = self.code_operation.lock().await;
        let workspace = self.workspace(record).for_run(&run_id);
        if operation.failure.is_none()
            && operation.result.is_none()
            && action(
                self.hosted_code_cancel_requested(),
                operation.submitted,
                false,
            ) == CodeAction::CancelUnsubmitted
            && storage
                .get::<Value>(&format!("{key}:outcome"))
                .await?
                .is_none()
        {
            cancel_unsubmitted(&mut operation);
            save(&storage, &key, &operation).await?;
        }
        if operation.failure.is_none() && operation.result.is_none() {
            let progress = self
                .advance_hosted_code(
                    &workspace,
                    record,
                    CodeExecution {
                        key: &key,
                        id: execution_id,
                        source: &arguments.code,
                        run: &run_id,
                        options,
                    },
                    &mut operation,
                )
                .await;
            if let Err(error) = progress {
                if read_chunks(&storage, &format!("{key}:outcome"))
                    .await?
                    .is_some()
                {
                    return pending().await;
                }
                if matches!(&error, Error::RustError(code) if code == "code_cancelled_before_dispatch")
                {
                    cancel_unsubmitted(&mut operation);
                    save(&storage, &key, &operation).await?;
                    return Ok(HostedDispatch::Ready(operation.result.unwrap().into()));
                }
                let Some(code) = permanent(&error) else {
                    return pending().await;
                };
                operation.failure = Some(code.into());
                operation.result = Some(json!({"error":code,"outputs":[],"files":[]}).to_string());
                save(&storage, &key, &operation).await?;
            }
        }
        if let Some(result) = operation.result.clone() {
            let outcome = read_chunks(&storage, &format!("{key}:outcome"))
                .await?
                .map(|bytes| {
                    serde_json::from_slice::<PythonOutcome>(&bytes)
                        .map_err(|_| fail("hosted_code_outcome_corrupt"))
                })
                .transpose()?;
            if let Some(context) = operation.context.as_deref() {
                let origin = public_origin(&self.env);
                let item = public_item(
                    &hosted_item_id(execution_id),
                    context,
                    &arguments.code,
                    outcome.as_ref(),
                    &operation,
                    origin.as_deref(),
                );
                if let Err(error) = self.publish_hosted_item(&item, true).await {
                    if super::permanent_publication_error(&error) {
                        return Err(error);
                    }
                    if matches!(&error, Error::RustError(code) if code == "hosted_response_deleted")
                    {
                        return Ok(HostedDispatch::Ready(result.into()));
                    }
                    return pending().await;
                }
            }
            return Ok(HostedDispatch::Ready(result.into()));
        }
        pending().await
    }

    async fn advance_hosted_code(
        &self,
        workspace: &crate::workspace::Workspace<'_>,
        record: &SessionRecord,
        execution: CodeExecution<'_>,
        operation: &mut CodeOperation,
    ) -> Result<()> {
        let CodeExecution {
            key,
            id: execution_id,
            source: code,
            run,
            options,
        } = execution;
        let storage = self.state.storage();
        let outcome_key = format!("{key}:outcome");
        let outcome = match read_chunks(&storage, &outcome_key).await? {
            Some(bytes) => serde_json::from_slice::<PythonOutcome>(&bytes)
                .map_err(|_| fail("hosted_code_outcome_corrupt"))?,
            None if action(
                self.hosted_code_cancel_requested(),
                operation.submitted,
                false,
            ) == CodeAction::CancelAndPoll =>
            {
                workspace.cancel_python(&operation.supervisor_id).await?;
                let outcome = workspace
                    .poll_python(&operation.supervisor_id)
                    .await
                    .map_err(|error| match &error {
                        Error::RustError(code) if code == "code_cancelled_before_dispatch" => error,
                        _ => fail("code_outcome_unknown"),
                    })?
                    .ok_or_else(|| fail("hosted_code_pending"))?;
                write_chunks(&storage, &outcome_key, &serde_json::to_vec(&outcome)?).await?;
                outcome
            }
            None => {
                if !operation.submitted {
                    for id in options.file_ids().iter().skip(operation.imported) {
                        if self.hosted_code_cancel_requested() {
                            cancel_unsubmitted(operation);
                            save(&storage, key, operation).await?;
                            return Ok(());
                        }
                        let file = crate::space::get_owned_file(&self.env, &record.owner_id, id)
                            .await?
                            .ok_or_else(|| fail("file_not_found"))?;
                        let imported = workspace
                            .import_python_file(&file, operation.context.as_deref())
                            .await?;
                        operation.context = Some(imported.container_id);
                        operation.imported += 1;
                        save(&storage, key, operation).await?;
                    }
                    if self.hosted_code_cancel_requested() {
                        cancel_unsubmitted(operation);
                        save(&storage, key, operation).await?;
                        return Ok(());
                    }
                    if !operation.prepared {
                        let request = python_request(options, operation.context.clone(), code);
                        operation.context = Some(workspace.prepare_python_context(&request).await?);
                        self.bind_local_code_context(
                            record,
                            run,
                            execution_id,
                            operation.context.as_deref().unwrap(),
                            options,
                        )
                        .await?;
                        operation.prepared = true;
                        save(&storage, key, operation).await?;
                    }
                    if self.hosted_code_cancel_requested() {
                        cancel_unsubmitted(operation);
                        save(&storage, key, operation).await?;
                        return Ok(());
                    }
                    let item = json!({"id":hosted_item_id(execution_id),
                        "type":"code_interpreter_call","container_id":operation.context,
                        "code":code,"status":"interpreting","outputs":[]});
                    self.publish_hosted_item(&item, false).await?;
                    operation.submitted = true;
                    save(&storage, key, operation).await?;
                    if self.hosted_code_cancel_requested() {
                        cancel_unsubmitted(operation);
                        save(&storage, key, operation).await?;
                        return Ok(());
                    }
                }
                let request = python_request(options, operation.context.clone(), code);
                let outcome = workspace
                    .execute_python(&operation.supervisor_id, &request)
                    .await?;
                // Commit exact terminal bytes before publishing or acknowledging
                // anything. Large log buffers are not rewritten per artifact.
                write_chunks(&storage, &outcome_key, &serde_json::to_vec(&outcome)?).await?;
                outcome
            }
        };
        if let Some(context) = outcome.container_id.as_ref() {
            operation.context = Some(context.clone());
        }
        if operation.published.is_empty() {
            operation
                .published
                .resize_with(outcome.artifacts.len(), || None);
            save(&storage, key, operation).await?;
        }
        if operation.published.len() != outcome.artifacts.len()
            || operation
                .deleted
                .iter()
                .any(|ordinal| *ordinal >= outcome.artifacts.len())
        {
            return Err(fail("hosted_code_operation_corrupt"));
        }
        let container = operation.context.clone();
        for (ordinal, artifact) in outcome.artifacts.iter().enumerate() {
            if !publication_settled(operation, ordinal) {
                let bytes = workspace.python_artifact(artifact).await?;
                let publication = hosted_artifacts::publish_bytes(
                    &self.env,
                    &record.owner_id,
                    ArtifactIdentity::InterpreterVersion {
                        container_id: container
                            .as_deref()
                            .ok_or_else(|| fail("hosted_code_outcome_corrupt"))?,
                        artifact_id: &artifact.id,
                    },
                    &artifact.filename,
                    &artifact.mime_type,
                    &bytes,
                )
                .await;
                match publication {
                    Ok(published) => operation.published[ordinal] = Some(published),
                    // Only the publication helper's accepted delete receipt is
                    // terminal. Missing daemon spools and other 404s still retry.
                    Err(error) if accepted_deletion(&error) => {
                        operation.deleted.insert(ordinal);
                    }
                    Err(error) => return Err(error),
                }
                save(&storage, key, operation).await?;
            }
        }
        // Every ordinal has a durable publication or accepted user-deletion
        // receipt before any spool deletion. Retries preserve those identities.
        for (ordinal, artifact) in outcome.artifacts.iter().enumerate() {
            if operation.acknowledged.contains(&ordinal) {
                continue;
            }
            workspace.ack_python_artifact(&artifact.id).await?;
            operation.acknowledged.insert(ordinal);
            save(&storage, key, operation).await?;
        }
        if !operation.publication_complete {
            workspace
                .complete_python_publication(&operation.supervisor_id)
                .await?;
            operation.publication_complete = true;
        }
        operation.result = Some(model_result(&outcome, operation).to_string());
        save(&storage, key, operation).await
    }
}

fn public_origin(env: &worker::Env) -> Option<String> {
    let configured = env
        .var("PUBLIC_API_URL")
        .or_else(|_| env.var("APP_ORIGIN"))
        .ok()?
        .to_string();
    let url = url::Url::parse(&configured).ok()?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

fn python_request(
    options: &crate::hosted_bindings::CodeOptions,
    context: Option<String>,
    code: &str,
) -> PythonRequest {
    let network_policy = match &options.container {
        CodeContainer::Existing { .. } => None,
        CodeContainer::Auto { network_policy, .. } => Some(match network_policy {
            NetworkPolicy::Disabled => PythonNetworkPolicy::Disabled,
            NetworkPolicy::Allowlist { allowed_domains } => PythonNetworkPolicy::Allowlist {
                allowed_domains: allowed_domains.clone(),
                domain_secrets: vec![],
            },
        }),
    };
    PythonRequest {
        code: code.into(),
        container_id: context,
        timeout_ms: 120_000,
        max_output_bytes: 1024 * 1024,
        memory_limit: match options.allocated_memory {
            MemoryTier::OneGiB => PythonMemoryTier::OneGiB,
            MemoryTier::FourGiB => PythonMemoryTier::FourGiB,
        },
        network_policy,
    }
}

fn accepted_deletion(error: &Error) -> bool {
    matches!(error, Error::RustError(code) if code == "file_publication_deleted")
}

fn publication_settled(operation: &CodeOperation, ordinal: usize) -> bool {
    operation.deleted.contains(&ordinal)
        || operation
            .published
            .get(ordinal)
            .is_some_and(Option::is_some)
}

fn model_result(outcome: &PythonOutcome, operation: &CodeOperation) -> Value {
    let logs = outcome
        .outputs
        .iter()
        .filter_map(|output| {
            (output["type"] == "logs")
                .then(|| output["logs"].as_str())
                .flatten()
        })
        .collect::<Vec<_>>();
    json!({"container_id":operation.context,"outputs":logs.into_iter().map(|logs|json!({"type":"logs","logs":logs})).collect::<Vec<_>>(),
        "files":operation.published.iter().flatten().map(ArtifactRef::value).collect::<Vec<_>>(),
        "deleted_artifacts":operation.deleted.iter().take(128).map(|ordinal|json!({"ordinal":ordinal,"code":"file_publication_deleted"})).collect::<Vec<_>>(),
        "error":outcome.error,"python_error":outcome.python_error,"timed_out":outcome.timed_out,
        "cancelled":outcome.cancelled,"truncated":outcome.truncated,"artifact_error":outcome.artifact_error})
}

fn public_item(
    id: &str,
    context: &str,
    code: &str,
    outcome: Option<&PythonOutcome>,
    operation: &CodeOperation,
    origin: Option<&str>,
) -> Value {
    let mut outputs = vec![];
    if let Some(outcome) = outcome {
        for output in &outcome.outputs {
            if output["type"] == "logs" {
                if let Some(logs) = output["logs"].as_str() {
                    outputs.push(json!({"type":"logs","logs":logs}));
                }
                continue;
            }
            let Some(artifact_id) = output["artifact_id"].as_str() else {
                continue;
            };
            let Some(ordinal) = outcome
                .artifacts
                .iter()
                .position(|artifact| artifact.id == artifact_id)
            else {
                continue;
            };
            let Some(Some(file)) = operation.published.get(ordinal) else {
                continue;
            };
            if output["type"] == "image"
                && file.content_type.starts_with("image/")
                && let Some(origin) = origin
            {
                outputs
                    .push(json!({"type":"image","url":format!("{origin}{}",file.download_path())}));
            }
        }
    }
    let failed = operation.failure.is_some()
        || outcome.is_some_and(|outcome| outcome.python_error || outcome.error.is_some());
    json!({"id":id,"type":"code_interpreter_call","container_id":context,"code":code,
        "status":if failed {"failed"} else {"completed"},"outputs":outputs})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation() -> CodeOperation {
        CodeOperation {
            fingerprint: "a".repeat(64),
            supervisor_id: format!("pyexec_{}", "a".repeat(64)),
            context: Some("cntr_known".into()),
            imported: 0,
            prepared: true,
            submitted: true,
            published: vec![Some(ArtifactRef {
                file_id: "file-owned".into(),
                filename: "plot.png".into(),
                content_type: "image/png".into(),
                bytes: 3,
                sha256: "b".repeat(64),
            })],
            deleted: BTreeSet::new(),
            acknowledged: BTreeSet::new(),
            publication_complete: false,
            failure: None,
            result: None,
        }
    }

    fn outcome() -> PythonOutcome {
        PythonOutcome {
            id: "daemon".into(),
            container_id: Some("cntr_known".into()),
            outputs: vec![
                json!({"type":"logs","logs":"42"}),
                json!({"type":"image","artifact_id":"pyart_known","path":"/private/spool/secret"}),
            ],
            artifacts: vec![crate::workspace::PythonArtifact {
                id: "pyart_known".into(),
                filename: "plot.png".into(),
                path: "/private/spool/secret".into(),
                bytes: 3,
                sha256: "b".repeat(64),
                mime_type: "image/png".into(),
            }],
            artifact_error: None,
            error: None,
            python_error: false,
            timed_out: false,
            cancelled: false,
            truncated: false,
            duration_ms: 10,
        }
    }

    #[test]
    fn accepted_artifact_deletion_is_durable_and_does_not_invent_a_replacement_file() {
        let mut operation = operation();
        operation.published[0] = None;
        assert!(!publication_settled(&operation, 0));
        operation.deleted.insert(0);
        let restored: CodeOperation =
            serde_json::from_slice(&serde_json::to_vec(&operation).unwrap()).unwrap();
        assert!(publication_settled(&restored, 0));
        assert!(publication_settled(&restored, 0));
        assert!(restored.acknowledged.is_empty());
        let result = model_result(&outcome(), &restored);
        assert_eq!(result["files"], json!([]));
        assert_eq!(
            result["deleted_artifacts"],
            json!([{"ordinal":0,"code":"file_publication_deleted"}])
        );
        assert_eq!(
            public_item(
                "ci_stable",
                "cntr_known",
                "",
                Some(&outcome()),
                &restored,
                Some("https://configured.example")
            )["outputs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn only_an_accepted_publication_tombstone_settles_an_unpublished_artifact() {
        assert!(accepted_deletion(&fail("file_publication_deleted")));
        for code in [
            "file_not_found",
            "code_container_not_found",
            "404",
            "file_publication_failed",
        ] {
            assert!(!accepted_deletion(&fail(code)));
        }
        let operation = operation();
        let mut legacy = serde_json::to_value(&operation).unwrap();
        legacy.as_object_mut().unwrap().remove("deleted");
        let restored: CodeOperation = serde_json::from_value(legacy).unwrap();
        assert!(restored.deleted.is_empty());
        assert!(publication_settled(&restored, 0));
        assert_eq!(
            model_result(&outcome(), &restored)["files"][0]["file_id"],
            "file-owned"
        );
    }

    #[test]
    fn cancellation_never_overrides_known_publication_cleanup() {
        assert_eq!(action(true, false, false), CodeAction::CancelUnsubmitted);
        assert_eq!(action(true, true, false), CodeAction::CancelAndPoll);
        for submitted in [false, true] {
            assert_eq!(action(true, submitted, true), CodeAction::DrainKnown);
        }
        assert_eq!(action(false, true, false), CodeAction::Execute);
        let mut operation = operation();
        operation.submitted = false;
        cancel_unsubmitted(&mut operation);
        assert!(!operation.submitted && operation.publication_complete);
        let result: Value = serde_json::from_str(operation.result.as_deref().unwrap()).unwrap();
        assert_eq!(result["cancelled"], true);
        assert_eq!(result["files"], json!([]));
    }

    #[test]
    fn acknowledged_progress_remains_recoverable_during_cancellation() {
        let mut operation = operation();
        operation.acknowledged.insert(0);
        let restored: CodeOperation =
            serde_json::from_slice(&serde_json::to_vec(&operation).unwrap()).unwrap();
        assert!(restored.published[0].is_some() && restored.acknowledged.contains(&0));
        assert_eq!(
            action(true, restored.submitted, true),
            CodeAction::DrainKnown
        );
        assert!(!restored.publication_complete);
    }

    #[test]
    fn execution_scope_is_path_safe_and_changed_arguments_conflict_in_same_slot() {
        let scope = scope_identity("owner", "session", "run:a:round:2:call:b").unwrap();
        assert_eq!(scope.len(), 64);
        assert!(scope.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(
            scope,
            scope_identity("other", "session", "run:a:round:2:call:b").unwrap()
        );
        let binding = ResolvedBinding {
            tool: ResolvedTool::Code {
                options: crate::hosted_bindings::CodeOptions {
                    container: CodeContainer::Existing {
                        id: "cntr_known".into(),
                    },
                    allocated_memory: MemoryTier::OneGiB,
                },
            },
            allowed_callers: None,
        };
        assert_ne!(
            identity("owner", "session", "exec", &binding, "{\"code\":\"x=1\"}").unwrap(),
            identity("owner", "session", "exec", &binding, "{\"code\":\"x=2\"}").unwrap()
        );
        assert_eq!(
            scope_identity("owner", "session", "exec").unwrap(),
            scope_identity("owner", "session", "exec").unwrap()
        );
    }

    #[test]
    fn terminal_projection_uses_only_owned_published_image_urls() {
        let mut operation = operation();
        let outcome = outcome();
        let public = public_item(
            "ci_stable",
            "cntr_known",
            "print(42)",
            Some(&outcome),
            &operation,
            Some("https://configured.example"),
        );
        assert_eq!(
            public["outputs"][1]["url"],
            "https://configured.example/v1/files/file-owned/content"
        );
        assert!(!public.to_string().contains("/private"));
        assert_eq!(
            model_result(&outcome, &operation)["files"][0]["file_id"],
            "file-owned"
        );
        operation.published[0] = None;
        assert_eq!(
            public_item(
                "ci_stable",
                "cntr_known",
                "",
                Some(&outcome),
                &operation,
                Some("https://configured.example")
            )["outputs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn ordinary_python_exception_is_a_failed_native_item_without_rerun() {
        let mut outcome = outcome();
        outcome.python_error = true;
        let operation = operation();
        assert_eq!(
            public_item(
                "ci_stable",
                "cntr_known",
                "raise ValueError()",
                Some(&outcome),
                &operation,
                None
            )["status"],
            "failed"
        );
        assert!(permanent(&fail("code_outcome_unknown")).is_none());
        assert_eq!(
            permanent(&fail("code_container_not_found")),
            Some("code_container_not_found")
        );
    }

    #[test]
    fn operation_state_retains_published_reference_before_acknowledgement() {
        let operation = operation();
        let bytes = serde_json::to_vec(&operation).unwrap();
        let restored: CodeOperation = serde_json::from_slice(&bytes).unwrap();
        assert!(restored.published[0].is_some() && restored.acknowledged.is_empty());
        assert!(!restored.publication_complete && restored.submitted);
        assert!(
            restored
                .supervisor_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
    }
}
