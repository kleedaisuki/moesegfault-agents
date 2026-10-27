//! The publisher sees one execution contract, regardless of resource ownership.
use crate::{
    code_borrowed_client::BorrowedExecutor,
    space::OwnedFile,
    workspace::{PythonArtifact, PythonInput, PythonOutcome, PythonRequest, Workspace},
};
use worker::{Error, Result};

pub(crate) enum CodeRuntime<'a> {
    Local(Workspace<'a>),
    Borrowed(BorrowedExecutor<'a>),
    Executor(crate::code_executor_runtime::RemoteRuntime<'a>),
}

impl CodeRuntime<'_> {
    pub(crate) async fn python_context_live(&self, cid: &str, boot: &str) -> Result<bool> {
        match self {
            Self::Local(workspace) => workspace.python_context_live(cid, boot).await,
            Self::Executor(runtime) => runtime.python_context_live(cid, boot).await,
            Self::Borrowed(executor) => {
                Ok(executor.prepare().await? == cid && executor.boot() == boot)
            }
        }
    }

    pub(crate) async fn boot(&self, cid: &str) -> Result<String> {
        match self {
            Self::Local(workspace) => workspace.python_boot().await,
            Self::Executor(runtime) => runtime.boot(cid).await,
            Self::Borrowed(executor) => Ok(executor.boot()),
        }
    }
    pub(crate) async fn python_dispatch_exists(&self, id: &str) -> Result<bool> {
        match self {
            Self::Local(workspace) => workspace.python_dispatch_exists(id).await,
            Self::Executor(runtime) => runtime.python_dispatch_exists(id).await,
            Self::Borrowed(_) => Err(Error::RustError("code_resource_binding_mismatch".into())),
        }
    }
    pub(crate) async fn ensure_code_lease(&self) -> Result<()> {
        match self {
            Self::Local(workspace) => workspace.ensure_code_lease().await,
            _ => Ok(()),
        }
    }

    pub(crate) async fn import_python_file(
        &self,
        file: &OwnedFile,
        context: Option<&str>,
    ) -> Result<PythonInput> {
        match self {
            Self::Local(workspace) => workspace.import_python_file(file, context).await,
            Self::Borrowed(_) => Err(Error::RustError("code_input_rejected".into())),
            Self::Executor(runtime) => runtime.import_python_file(file, context).await,
        }
    }
    pub(crate) async fn prepare_python_context(&self, request: &PythonRequest) -> Result<String> {
        match self {
            Self::Local(workspace) => workspace.prepare_python_context(request).await,
            Self::Borrowed(executor) => executor.prepare().await,
            Self::Executor(runtime) => runtime.prepare_python_context(request).await,
        }
    }
    pub(crate) async fn execute_python(
        &self,
        id: &str,
        request: &PythonRequest,
    ) -> Result<PythonOutcome> {
        match self {
            Self::Local(workspace) => workspace.execute_python(id, request).await,
            Self::Borrowed(executor) => executor.execute(request).await,
            Self::Executor(runtime) => runtime.execute_python(id, request).await,
        }
    }
    pub(crate) async fn poll_python(&self, id: &str) -> Result<Option<PythonOutcome>> {
        match self {
            Self::Local(workspace) => workspace.poll_python(id).await,
            Self::Borrowed(executor) => executor.poll().await,
            Self::Executor(runtime) => runtime.poll_python(id).await,
        }
    }
    pub(crate) async fn cancel_python(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(workspace) => workspace.cancel_python(id).await,
            Self::Borrowed(executor) => executor.cancel().await,
            Self::Executor(runtime) => runtime.cancel_python(id).await,
        }
    }
    pub(crate) async fn python_artifact(&self, artifact: &PythonArtifact) -> Result<Vec<u8>> {
        match self {
            Self::Local(workspace) => workspace.python_artifact(artifact).await,
            Self::Borrowed(executor) => executor.artifact(artifact).await,
            Self::Executor(runtime) => runtime.python_artifact(artifact).await,
        }
    }
    pub(crate) async fn ack_python_artifact(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(workspace) => workspace.ack_python_artifact(id).await,
            Self::Borrowed(executor) => executor.acknowledge(id).await,
            Self::Executor(runtime) => runtime.ack_python_artifact(id).await,
        }
    }
    pub(crate) async fn complete_python_publication(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(workspace) => workspace.complete_python_publication(id).await,
            Self::Borrowed(executor) => executor.complete().await,
            Self::Executor(runtime) => runtime.complete_python_publication(id).await,
        }
    }
}
