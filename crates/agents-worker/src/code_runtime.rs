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
}

impl CodeRuntime<'_> {
    pub(crate) async fn import_python_file(
        &self,
        file: &OwnedFile,
        context: Option<&str>,
    ) -> Result<PythonInput> {
        match self {
            Self::Local(workspace) => workspace.import_python_file(file, context).await,
            Self::Borrowed(_) => Err(Error::RustError("code_input_rejected".into())),
        }
    }
    pub(crate) async fn prepare_python_context(&self, request: &PythonRequest) -> Result<String> {
        match self {
            Self::Local(workspace) => workspace.prepare_python_context(request).await,
            Self::Borrowed(executor) => executor.prepare().await,
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
        }
    }
    pub(crate) async fn poll_python(&self, id: &str) -> Result<Option<PythonOutcome>> {
        match self {
            Self::Local(workspace) => workspace.poll_python(id).await,
            Self::Borrowed(executor) => executor.poll().await,
        }
    }
    pub(crate) async fn cancel_python(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(workspace) => workspace.cancel_python(id).await,
            Self::Borrowed(executor) => executor.cancel().await,
        }
    }
    pub(crate) async fn python_artifact(&self, artifact: &PythonArtifact) -> Result<Vec<u8>> {
        match self {
            Self::Local(workspace) => workspace.python_artifact(artifact).await,
            Self::Borrowed(executor) => executor.artifact(artifact).await,
        }
    }
    pub(crate) async fn ack_python_artifact(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(workspace) => workspace.ack_python_artifact(id).await,
            Self::Borrowed(executor) => executor.acknowledge(id).await,
        }
    }
    pub(crate) async fn complete_python_publication(&self, id: &str) -> Result<()> {
        match self {
            Self::Local(workspace) => workspace.complete_python_publication(id).await,
            Self::Borrowed(executor) => executor.complete().await,
        }
    }
}
