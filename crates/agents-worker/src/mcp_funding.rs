//! Fund metadata and its bounded credential-cleanup lifecycle independently of
//! model completion money. Tariffs are captured, not reselected after deployment.
use agents_auth::billing::Resource;
use serde::{Deserialize, Serialize};
use worker::{Env, Result};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct DiscoveryFunding {
    run: String,
    resource: Resource,
    pricing: crate::resources::ResourcePricing,
    started_ms: u64,
    ended_ms: Option<u64>,
    cleanup_pending: bool,
    cleanup_interval: Option<(u64, u64)>,
    settled: bool,
}

impl std::fmt::Debug for DiscoveryFunding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DiscoveryFunding")
            .field("settled", &self.settled)
            .finish_non_exhaustive()
    }
}

impl DiscoveryFunding {
    pub(crate) async fn start(env: &Env, owner: &str, resource: Resource) -> Result<Self> {
        Self::start_inner(env, owner, resource, true).await
    }

    pub(crate) async fn start_read(env: &Env, owner: &str, resource: Resource) -> Result<Self> {
        Self::start_inner(env, owner, resource, false).await
    }

    async fn start_inner(
        env: &Env,
        owner: &str,
        resource: Resource,
        cleanup_pending: bool,
    ) -> Result<Self> {
        if !matches!(resource, Resource::Worker | Resource::DurableObject) {
            return Err(crate::mcp_bindings::fail("invalid_mcp_discovery_resource"));
        }
        crate::allowances::sync(env, owner).await?;
        let pricing = crate::resources::ResourcePricing::parse(
            &env.var("RESOURCE_PRICING_JSON")?.to_string(),
        )?;
        let grant = Self {
            run: format!("mcp-discovery:{}", uuid::Uuid::new_v4()),
            resource,
            pricing,
            started_ms: crate::util::now_ms(),
            ended_ms: None,
            cleanup_pending,
            cleanup_interval: None,
            settled: false,
        };
        let meter = grant.meter(env, owner)?;
        meter.preflight(false)?;
        crate::reconcile::maintenance_account_reservation(env, owner, &grant.run).await?;
        meter
            .require_observation("discovery", grant.resource.clone(), "wall_ms")
            .await?;
        if cleanup_pending {
            meter
                .require_observation("cleanup", grant.resource.clone(), "wall_ms")
                .await?;
        }
        Ok(grant)
    }

    fn meter<'a>(
        &'a self,
        env: &'a Env,
        owner: &'a str,
    ) -> Result<crate::resources::ResourceMeter<'a>> {
        crate::resources::ResourceMeter::with_account_pricing(
            env,
            owner,
            &self.run,
            self.pricing.clone(),
        )
    }

    pub(crate) fn observe_end(&mut self) {
        self.ended_ms = Some(crate::util::now_ms());
    }

    /// Retain this receipt in the accepted discovery snapshot if accounting is
    /// unavailable. Reconciliation repeats no external MCP request.
    pub(crate) async fn finish(&mut self, env: &Env, owner: &str) -> Result<()> {
        if self.settled {
            return Ok(());
        }
        let ended = self
            .ended_ms
            .ok_or_else(|| crate::mcp_bindings::fail("mcp_discovery_duration_unknown"))?;
        self.meter(env, owner)?
            .record_wall_time("discovery", self.resource.clone(), self.started_ms, ended)
            .await?;
        if !self.cleanup_pending {
            self.meter(env, owner)?.settle_run().await?;
            self.settled = true;
        }
        Ok(())
    }

    pub(crate) fn capture_cleanup(&mut self, started: u64, ended: u64) {
        self.cleanup_interval.get_or_insert((started, ended));
    }

    pub(crate) async fn complete_cleanup(&mut self, env: &Env, owner: &str) -> Result<()> {
        self.finish(env, owner).await?;
        if !self.cleanup_pending {
            return Ok(());
        }
        let (started, ended) = self
            .cleanup_interval
            .ok_or_else(|| crate::mcp_bindings::fail("mcp_cleanup_duration_unknown"))?;
        self.meter(env, owner)?
            .record_wall_time("cleanup", self.resource.clone(), started, ended)
            .await?;
        self.cleanup_pending = false;
        self.meter(env, owner)?.settle_run().await?;
        self.settled = true;
        Ok(())
    }
}
