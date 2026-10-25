//! Trusted transport context. None of these fields are accepted from public input.

use crate::conversation_routes::ConversationAdmission;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConnectionLease {
    pub connection_id: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OwnedParent {
    pub response_id: String,
    pub token: String,
    pub connection_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ConversationInput {
    pub admission: ConversationAdmission,
    pub reuse_head: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SourceRange {
    pub begin: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ExecutionContext {
    #[serde(default)]
    pub conversation: Option<ConversationInput>,
    #[serde(default)]
    pub connection_lease: Option<ConnectionLease>,
    #[serde(default)]
    pub parent_lease: Option<OwnedParent>,
    #[serde(default)]
    pub warmup: bool,
    #[serde(default)]
    pub steering_parent: Option<String>,
    #[serde(default)]
    pub suppress_observer: bool,
    #[serde(default)]
    pub inherited_prompt: Option<crate::prompt_templates::ResolvedPrompt>,
}

impl ExecutionContext {
    pub(crate) fn persisted(&self) -> Self {
        Self {
            conversation: None,
            connection_lease: self.connection_lease.clone(),
            parent_lease: self.parent_lease.clone(),
            warmup: self.warmup,
            steering_parent: self.steering_parent.clone(),
            suppress_observer: self.suppress_observer,
            inherited_prompt: None,
        }
    }
}
