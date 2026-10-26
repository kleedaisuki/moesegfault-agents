//! Same durable dispatch fence as managed Python, without allocating a CF VM.
use crate::{
    code_executor_transport::{Executor, ExecutorProfile, Probe},
    resources::ResourceMeter,
    space::OwnedFile,
    workspace::{PythonArtifact, PythonInput, PythonOutcome, PythonRequest},
};
use agents_auth::billing::Resource;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use worker::{Env, Error, ResponseBody, Result, Storage};

const PENDING: &str = "code-executor:publication-pending";

#[derive(Serialize, Deserialize)]
struct Dispatch {
    context: String,
    boot: String,
    request_hash: String,
}

pub(crate) struct RemoteRuntime<'a> {
    storage: Storage,
    env: &'a Env,
    owner: String,
    session: String,
    run: String,
    profile: ExecutorProfile,
    executor: Executor,
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

impl<'a> RemoteRuntime<'a> {
    pub(crate) async fn new(
        storage: Storage,
        env: &'a Env,
        owner: &str,
        session: &str,
        run: &str,
        profile: ExecutorProfile,
        trace: Option<crate::telemetry::TraceContext>,
    ) -> Result<Self> {
        let executor = profile.open(env, owner, trace).await?;
        Ok(Self {
            storage,
            env,
            owner: owner.into(),
            session: session.into(),
            run: run.into(),
            profile,
            executor,
        })
    }

    pub(crate) async fn prepare_python_context(&self, request: &PythonRequest) -> Result<String> {
        if !self
            .profile
            .capabilities
            .memory_limits
            .contains(&request.memory_limit)
        {
            return Err(fail("code_executor_memory_unsupported"));
        }
        let context = self.executor.prepare(request).await?;
        self.storage
            .put(
                &format!("code-executor:boot:{}", context.container_id),
                &context.boot_id,
            )
            .await?;
        Ok(context.container_id)
    }
    pub(crate) async fn python_context_live(&self, cid: &str, boot: &str) -> Result<bool> {
        self.executor.inspect(cid, boot).await
    }

    pub(crate) async fn boot(&self, cid: &str) -> Result<String> {
        self.storage
            .get::<String>(&format!("code-executor:boot:{cid}"))
            .await?
            .ok_or_else(|| fail("code_executor_boot_proof_missing"))
    }

    pub(crate) async fn execute_python(
        &self,
        execution: &str,
        request: &PythonRequest,
    ) -> Result<PythonOutcome> {
        let id = digest(execution);
        let context = request
            .container_id
            .as_deref()
            .ok_or_else(|| fail("code_executor_context_unprepared"))?;
        let boot = self.boot(context).await?;
        let hash = digest(&serde_json::to_string(request)?);
        let key = format!("code-executor:dispatch:{id}");
        let previous = self.storage.get::<Dispatch>(&key).await?;
        if previous.is_none() && self.cancelled(&id).await? {
            return Err(fail("code_cancelled_before_dispatch"));
        }
        if previous
            .as_ref()
            .is_some_and(|previous| previous.boot != boot || previous.request_hash != hash)
        {
            return Err(fail("code_executor_outcome_unknown"));
        }
        match self.observe(&id, context).await? {
            Probe::Complete(outcome) => Ok(outcome),
            Probe::Pending => self.wait_python(&id, context).await,
            Probe::Absent if previous.is_some() => Err(fail("code_executor_outcome_unknown")),
            Probe::Absent => {
                if self.executor.boot().await? != boot {
                    return Err(fail("code_container_not_found"));
                }
                self.storage
                    .put(
                        &key,
                        Dispatch {
                            context: context.into(),
                            boot,
                            request_hash: hash,
                        },
                    )
                    .await?;
                self.pending(&id, true).await?;
                if self.cancelled(&id).await? {
                    self.pending(&id, false).await?;
                    return Err(fail("code_cancelled_before_dispatch"));
                }
                let outcome = self.executor.submit(&id, request).await;
                if matches!(&outcome,Err(Error::RustError(code)) if code=="code_cancelled_before_dispatch")
                {
                    self.pending(&id, false).await?;
                }
                outcome
            }
        }
    }

    async fn observe(&self, id: &str, context: &str) -> Result<Probe> {
        let result = self.executor.probe(id, context).await;
        if matches!(&result,Err(Error::RustError(code)) if code=="code_cancelled_before_dispatch") {
            self.pending(id, false).await?;
        }
        result
    }

    async fn wait_python(&self, id: &str, context: &str) -> Result<PythonOutcome> {
        let end = crate::util::now_ms().saturating_add(340_000);
        while crate::util::now_ms() < end {
            worker::Delay::from(std::time::Duration::from_millis(250)).await;
            match self.observe(id, context).await? {
                Probe::Complete(outcome) => return Ok(outcome),
                Probe::Pending => {}
                Probe::Absent => return Err(fail("code_executor_outcome_unknown")),
            }
        }
        Err(fail("code_executor_outcome_unknown"))
    }

    pub(crate) async fn python_dispatch_exists(&self, execution: &str) -> Result<bool> {
        Ok(self
            .storage
            .get::<Dispatch>(&format!("code-executor:dispatch:{}", digest(execution)))
            .await?
            .is_some())
    }
    pub(crate) async fn poll_python(&self, execution: &str) -> Result<Option<PythonOutcome>> {
        let id = digest(execution);
        let dispatch = self
            .storage
            .get::<Dispatch>(&format!("code-executor:dispatch:{id}"))
            .await?;
        if dispatch.is_none() && self.cancelled(&id).await? {
            return Err(fail("code_cancelled_before_dispatch"));
        }
        let dispatch = dispatch.ok_or_else(|| fail("code_executor_outcome_unknown"))?;
        match self.observe(&id, &dispatch.context).await? {
            Probe::Complete(outcome) => Ok(Some(outcome)),
            Probe::Pending => Ok(None),
            Probe::Absent => Err(fail("code_executor_outcome_unknown")),
        }
    }

    async fn cancelled(&self, id: &str) -> Result<bool> {
        Ok(self
            .storage
            .get::<bool>(&format!("code-executor:cancel:{id}"))
            .await?
            .unwrap_or(false))
    }
    pub(crate) async fn cancel_python(&self, execution: &str) -> Result<()> {
        let id = digest(execution);
        self.storage
            .put(&format!("code-executor:cancel:{id}"), true)
            .await?;
        self.executor.cancel(&id).await
    }

    pub(crate) async fn import_python_file(
        &self,
        file: &OwnedFile,
        context: Option<&str>,
    ) -> Result<PythonInput> {
        let owned = crate::space::get_owned_file(self.env, &self.owner, &file.id)
            .await?
            .filter(|current| {
                current.content_key() == file.content_key()
                    && current.sha256 == file.sha256
                    && current.bytes == file.bytes
            })
            .ok_or_else(|| fail("file_not_found"))?;
        if file.account_id != self.owner || file.bytes > 64 * 1024 * 1024 {
            return Err(fail("invalid_code_input"));
        }
        let meter = ResourceMeter::new(self.env, &self.owner, &self.session, &self.run)?;
        let event = format!("code-executor:input:{}", uuid::Uuid::new_v4());
        meter
            .require_observation(&event, Resource::R2Operation, "class_b")
            .await?;
        let object = self
            .env
            .bucket("WORKSPACES")?
            .get(owned.content_key())
            .execute()
            .await?;
        meter
            .record_observation(&event, Resource::R2Operation, "class_b", 1)
            .await?;
        let object = object
            .filter(|object| object.size() == owned.bytes)
            .ok_or_else(|| fail("file_not_ready"))?;
        let ResponseBody::Stream(stream) = object
            .body()
            .ok_or_else(|| fail("file_not_ready"))?
            .response_body()?
        else {
            return Err(fail("file_not_streamable"));
        };
        self.executor.import(file, context, stream.into()).await
    }

    pub(crate) async fn python_artifact(&self, artifact: &PythonArtifact) -> Result<Vec<u8>> {
        self.executor.artifact(artifact).await
    }
    pub(crate) async fn ack_python_artifact(&self, id: &str) -> Result<()> {
        self.executor.acknowledge(id).await
    }
    pub(crate) async fn complete_python_publication(&self, execution: &str) -> Result<()> {
        self.pending(&digest(execution), false).await
    }

    async fn pending(&self, id: &str, add: bool) -> Result<()> {
        let mut entries = self
            .storage
            .get::<BTreeSet<String>>(PENDING)
            .await?
            .unwrap_or_default();
        if add {
            if entries.len() >= 128 && !entries.contains(id) {
                return Err(fail("code_publication_backlog"));
            }
            entries.insert(id.into());
        } else {
            entries.remove(id);
        }
        if entries.is_empty() {
            self.storage.delete(PENDING).await?;
        } else {
            self.storage.put(PENDING, entries).await?;
        }
        Ok(())
    }
}

pub(crate) async fn publication_pending(storage: &Storage) -> Result<bool> {
    Ok(storage
        .get::<BTreeSet<String>>(PENDING)
        .await?
        .is_some_and(|pending| !pending.is_empty()))
}
