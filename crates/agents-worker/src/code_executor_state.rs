//! Frozen executor authority is separate from the source actor's physical class.
use crate::{code_executor_transport::ExecutorProfile, resources::ResourcePricing};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum ExecutorKind {
    #[default]
    #[serde(rename = "platform")]
    Managed,
    #[serde(rename = "workspace_daemon")]
    WorkspaceDaemon,
}
impl ExecutorKind {
    pub(crate) fn is_managed(&self) -> bool {
        matches!(self, Self::Managed)
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Managed => "platform",
            Self::WorkspaceDaemon => "workspace_daemon",
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExecutionBackend {
    #[default]
    Managed,
    Executor {
        profile: ExecutorProfile,
    },
}
impl ExecutionBackend {
    pub(crate) fn is_managed(&self) -> bool {
        matches!(self, Self::Managed)
    }
    pub(crate) fn kind(&self) -> ExecutorKind {
        match self {
            Self::Managed => ExecutorKind::Managed,
            Self::Executor { .. } => ExecutorKind::WorkspaceDaemon,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CleanupFunding {
    pub id: String,
    pub pricing: ResourcePricing,
}
