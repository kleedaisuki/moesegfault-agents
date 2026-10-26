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

pub(crate) const RELEASE_UNUSED_SQL: &str = "UPDATE usage_reservations SET state='released',updated_at=CAST(?3 AS INTEGER) WHERE id=?1 AND account_id=?2 AND session_id IS NULL AND state='reserved' AND NOT EXISTS(SELECT 1 FROM resource_observations o WHERE o.run_id=?1 AND o.account_id=?2) AND NOT EXISTS(SELECT 1 FROM usage_ledger l WHERE l.reservation_id=?1)";

/// Unused is not an observed zero: release an untouched reserve, but settle
/// measured work and retain every unknown observation for reconciliation.
pub(crate) async fn finish_cleanup(
    env: &worker::Env,
    owner: &str,
    hold: &CleanupFunding,
) -> worker::Result<()> {
    use agents_auth::billing::ReservationState;
    #[derive(Deserialize)]
    struct State {
        state: ReservationState,
    }
    let db = env.d1("DB")?;
    db.prepare(RELEASE_UNUSED_SQL)
        .bind(&[
            hold.id.as_str().into(),
            owner.into(),
            crate::util::now_ms().to_string().into(),
        ])?
        .run()
        .await?;
    let state=db.prepare("SELECT state FROM usage_reservations WHERE id=? AND account_id=? AND session_id IS NULL")
        .bind(&[hold.id.as_str().into(),owner.into()])?.first::<State>(None).await?
        .ok_or_else(||worker::Error::RustError("code_executor_cleanup_unfunded".into()))?;
    if matches!(
        state.state,
        ReservationState::Released | ReservationState::Settled
    ) {
        return Ok(());
    }
    crate::resources::ResourceMeter::with_account_pricing(
        env,
        owner,
        &hold.id,
        hold.pricing.clone(),
    )?
    .settle_run()
    .await?;
    Ok(())
}
