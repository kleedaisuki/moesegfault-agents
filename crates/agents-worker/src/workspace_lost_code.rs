//! A lost kernel frees physical capacity, not its unknown financial/data history.
use super::*;

pub(crate) struct LostCodeIncarnation {
    lease: Option<String>,
    clock_event: Option<String>,
}

impl Workspace<'_> {
    /// Preserve the existing unexpected-exit journal without starting a VM,
    /// exporting a fictitious checkpoint, or guessing when the old process died.
    /// The owner still uses the retained lease for its final catalog writes.
    pub(crate) async fn observe_lost_code_incarnation(
        &self,
    ) -> Result<Option<LostCodeIncarnation>> {
        if self.container()?.running() {
            return Ok(None);
        }
        let storage = self.state.storage();
        let clock = storage
            .get::<UsageClock>(CONTAINER_CLOCK)
            .await?
            .unwrap_or_default();
        let lease = self.lease().current().await?;
        if clock.quantity != 0 {
            let current = lease
                .as_ref()
                .ok_or_else(|| Error::RustError("workspace_lost_lease_missing".into()))?;
            self.lease()
                .meter(&current.id)
                .await?
                .require_observation("container:unexpected_exit", Resource::Container, "wall_ms")
                .await?;
            let key = format!(
                "workspace:unreconciled:{}",
                clock.event.as_deref().unwrap_or("container")
            );
            storage.put(&key, &clock).await?;
            storage.put("workspace:code-loss", serde_json::json!({
                "lease_id":current.id,"boot_id":storage.get::<String>("workspace:boot").await?,
                "last_checkpoint_is_authoritative":true,
                "exit_timestamp_known":false,"python_heap_recoverable":false,
            })).await?;
        }
        Ok(Some(LostCodeIncarnation {
            lease: lease.map(|value| value.id),
            clock_event: clock.event,
        }))
    }

    /// Call only after the Code owner durably released catalog capacity. The
    /// usage row and pending observations remain reserved for reconciliation.
    pub(crate) async fn detach_lost_code_incarnation(
        &self,
        lost: &LostCodeIncarnation,
    ) -> Result<()> {
        if self.container()?.running() {
            return Err(Error::RustError("workspace_incarnation_changed".into()));
        }
        let storage = self.state.storage();
        let clock = storage
            .get::<UsageClock>(CONTAINER_CLOCK)
            .await?
            .unwrap_or_default();
        let lease = self.lease().current().await?;
        if lease.as_ref().map(|value| value.id.as_str()) != lost.lease.as_deref()
            || clock.event != lost.clock_event
        {
            return Err(Error::RustError("workspace_incarnation_changed".into()));
        }
        storage.put(CONTAINER_CLOCK, UsageClock::default()).await?;
        self.lease().clear().await?;
        // Never call settle(), set an invented end time, or remove the known
        // prior manifest. A later Auto allocation is a fresh kernel, not restore.
        Ok(())
    }
}
