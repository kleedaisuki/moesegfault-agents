//! Immutable cross-Session execution references, independent of model context.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::{Error, Result};

use crate::{
    code_contexts::{Allocation, ContextState, OwnedContext},
    workspace::{PythonMemoryTier, PythonRequest},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ticket {
    pub guest_session: String,
    pub guest_run: String,
    pub execution: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cell {
    pub context: OwnedContext,
    pub ticket: Ticket,
    pub request: PythonRequest,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:-".contains(&byte))
}
fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

impl Ticket {
    pub(crate) fn supervisor_id(&self, owner: &str, context: &OwnedContext) -> Result<String> {
        if !identifier(&self.guest_session)
            || !identifier(&self.guest_run)
            || self.execution.is_empty()
            || self.execution.len() > 1024
        {
            return Err(fail("invalid_code_resource_ticket"));
        }
        let bytes = serde_json::to_vec(&(owner, context, self))?;
        Ok(format!("borrowed_{:x}", Sha256::digest(bytes)))
    }

    /// A ticket is an idempotency identity, not authorization. The receiver must
    /// separately validate the owner and still-funded internal actor grant.
    pub(crate) fn matches_funding(&self, session: Option<&str>, run: &str) -> bool {
        session == Some(self.guest_session.as_str()) && run == self.guest_run
    }
}

impl Cell {
    pub(crate) fn validate_source(
        &self,
        owner: &str,
        allocation: &Allocation,
        now: u64,
    ) -> Result<()> {
        if allocation.account_id != owner
            || allocation.state != ContextState::Ready
            || allocation.expires_at_ms <= now
            || allocation.allocation_released_at_ms.is_some()
            || allocation.owned()? != self.context
            || self.request.container_id.as_deref() != Some(self.context.id.as_str())
            || self.request.network_policy.is_some()
        {
            return Err(fail("code_container_not_found"));
        }
        let memory = match self.request.memory_limit {
            PythonMemoryTier::OneGiB => agents_core::RuntimeProfile::Basic1g,
            PythonMemoryTier::FourGiB => agents_core::RuntimeProfile::Standard4g,
        };
        if memory != self.context.profile {
            return Err(fail("code_memory_profile_mismatch"));
        }
        if self.request.code.is_empty()
            || self.request.code.len() > 65536
            || self.request.code.contains('\0')
            || !(1..=300_000).contains(&self.request.timeout_ms)
            || !(1..=4 * 1024 * 1024).contains(&self.request.max_output_bytes)
        {
            return Err(fail("invalid_code_request"));
        }
        self.ticket.supervisor_id(owner, &self.context)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_borrowed_ticket_is_stable_but_cannot_change_owner_generation_or_funding() {
        let context = OwnedContext {
            id: format!("cntr_{}", "a".repeat(32)),
            source_session: "source".into(),
            profile: agents_core::RuntimeProfile::Basic1g,
            daemon_boot: "boot".into(),
            policy_revision: "policy".into(),
        };
        let ticket = Ticket {
            guest_session: "guest".into(),
            guest_run: "run".into(),
            execution: "cell".into(),
        };
        let id = ticket.supervisor_id("owner", &context).unwrap();
        assert_eq!(id, ticket.supervisor_id("owner", &context).unwrap());
        assert_ne!(id, ticket.supervisor_id("other", &context).unwrap());
        let mut replacement = context.clone();
        replacement.daemon_boot = "replacement".into();
        assert_ne!(id, ticket.supervisor_id("owner", &replacement).unwrap());
        assert!(ticket.matches_funding(Some("guest"), "run"));
        assert!(!ticket.matches_funding(Some("source"), "run"));
        assert!(!ticket.matches_funding(Some("guest"), "other-run"));
    }
}
