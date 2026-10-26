//! Code memory has its own resource clock; closing a harness does not renew it.
use super::*;
use crate::code_executor_state::{CleanupFunding, ExecutionBackend, ExecutorKind};
use crate::{
    code_contexts::{self as catalog, Allocation, ContextState, IDLE_MS, OwnedContext},
    hosted_bindings::{CodeContainer, CodeOptions, NetworkPolicy, ResolvedTool},
    resources::ResourceMeter,
};
use std::collections::BTreeMap;

const KEY: &str = "code-resources.v1";
const HEARTBEAT_MS: u64 = 5 * 60 * 1000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Retirement {
    Idle,
    ExplicitDelete,
    ConfirmedLoss,
}

impl Retirement {
    fn revokes(self, allocation: &Allocation, now: u64) -> bool {
        self != Self::Idle
            || allocation.state != ContextState::Ready
            || allocation.expires_at_ms <= now
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Resource {
    allocation: Allocation,
    policy: NetworkPolicy,
    #[serde(default, skip_serializing_if = "ExecutionBackend::is_managed")]
    backend: ExecutionBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup: Option<CleanupFunding>,
    preparation_started: bool,
    namespace_retired: bool,
    retry_at_ms: u64,
    last_use: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_borrowed: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    current: Option<String>,
    resources: BTreeMap<String, Resource>,
}

impl Registry {
    fn deadline(&self, now: u64) -> Option<u64> {
        self.resources
            .values()
            .filter_map(|resource| match resource.allocation.state {
                ContextState::Ready => Some(
                    if resource.backend.is_managed() {
                        resource
                            .allocation
                            .expires_at_ms
                            .min(now.saturating_add(HEARTBEAT_MS))
                    } else {
                        resource.allocation.expires_at_ms
                    }
                    .max(now.saturating_add(1000)),
                ),
                ContextState::Expired | ContextState::Deleted => {
                    Some(resource.retry_at_ms.max(now.saturating_add(1000)))
                }
                ContextState::Reserved => Some(
                    resource
                        .allocation
                        .expires_at_ms
                        .max(now.saturating_add(1000)),
                ),
            })
            .min()
    }

    fn retained(&self, now: u64) -> bool {
        self.resources.values().any(|resource| {
            resource.backend.is_managed()
                && resource.allocation.allocation_released_at_ms.is_none()
                && (resource.allocation.state == ContextState::Reserved
                    || resource.allocation.state == ContextState::Ready
                        && resource.allocation.expires_at_ms > now)
        })
    }
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

impl SessionInner {
    async fn read_code_registry(&self) -> Result<Registry> {
        match read_chunks(&self.state.storage(), KEY).await? {
            None => Ok(Registry::default()),
            Some(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| fail("code_resource_state_corrupt"))
            }
        }
    }

    async fn update_code_registry<R>(
        &self,
        change: impl FnOnce(&mut Registry) -> Result<R>,
    ) -> Result<R> {
        let (result, first) = {
            let _guard = self.code_metadata.lock().await;
            let mut registry = self.read_code_registry().await?;
            let result = change(&mut registry)?;
            write_chunks(&self.state.storage(), KEY, &serde_json::to_vec(&registry)?).await?;
            self.code_deadline.set(registry.deadline(util::now_ms()));
            let mut data = self.data.borrow_mut();
            let data = data
                .as_mut()
                .ok_or_else(|| fail("session_not_initialized"))?;
            let first = !data.code_resources_seen;
            data.code_resources_seen = true;
            (result, first)
        };
        // Metadata never holds its mutex while acquiring the harness persistence
        // lock. Admission can already own the latter while inspecting a resource.
        if first {
            self.persist().await?;
        }
        self.arm_alarm().await?;
        Ok(result)
    }

    async fn resource_is_live(&self, record: &SessionRecord, resource: &Resource) -> Result<bool> {
        let Some(cid) = resource.allocation.container_id.as_deref() else {
            return Ok(false);
        };
        let boot = resource.allocation.daemon_boot_id.as_deref().unwrap_or("");
        match &resource.backend {
            ExecutionBackend::Managed => {
                self.workspace(record).python_context_live(cid, boot).await
            }
            ExecutionBackend::Executor { profile } => {
                profile
                    .open(&self.env, &record.owner_id, None)
                    .await?
                    .inspect(cid, boot)
                    .await
            }
        }
    }

    pub(super) async fn code_context_live(
        &self,
        record: &SessionRecord,
        cid: &str,
    ) -> Result<bool> {
        let registry = self.read_code_registry().await?;
        let Some(resource) = registry
            .resources
            .values()
            .find(|resource| resource.allocation.container_id.as_deref() == Some(cid))
        else {
            return Ok(false);
        };
        self.resource_is_live(record, resource).await
    }

    pub(super) async fn restore_code_resources(&self) -> Result<()> {
        let check = self.data.borrow().as_ref().is_some_and(|data| {
            data.code_resources_seen
                || data
                    .run
                    .as_ref()
                    .and_then(|run| run.response.as_ref())
                    .is_some_and(|response| {
                        response
                            .bound_tools
                            .lookup(crate::hosted_tools::Kind::CodeInterpreter.alias())
                            .is_some()
                    })
        });
        if check {
            self.code_deadline
                .set(self.read_code_registry().await?.deadline(util::now_ms()));
            self.arm_alarm().await?;
        }
        Ok(())
    }

    pub(super) async fn admit_local_code_resource(
        &self,
        record: &SessionRecord,
        run: &RunState,
    ) -> Result<()> {
        let Some(options) = run
            .response
            .as_ref()
            .and_then(|response| {
                response
                    .bound_tools
                    .lookup(crate::hosted_tools::Kind::CodeInterpreter.alias())
            })
            .and_then(|binding| match &binding.tool {
                ResolvedTool::Code { options } => Some(options.clone()),
                _ => None,
            })
        else {
            return Ok(());
        };
        if !options.is_local_to(&record.id) {
            return Ok(());
        }
        self.ensure_local_code_resource(record, &run.id, &options)
            .await
    }

    async fn ensure_local_code_resource(
        &self,
        record: &SessionRecord,
        run: &str,
        options: &CodeOptions,
    ) -> Result<()> {
        self.reconcile_code_resources().await?;
        let CodeContainer::Auto { network_policy, .. } = &options.container else {
            let CodeContainer::Existing { id, .. } = &options.container else {
                unreachable!()
            };
            let registry = self.read_code_registry().await?;
            let resource = registry
                .current
                .as_ref()
                .and_then(|key| registry.resources.get(key))
                .filter(|resource| {
                    resource.allocation.state == ContextState::Ready
                        && resource.allocation.container_id.as_deref() == Some(id.as_str())
                        && resource.allocation.expires_at_ms > util::now_ms()
                })
                .ok_or_else(|| fail("code_container_not_found"))?;
            if !self.resource_is_live(record, resource).await? {
                return Err(fail("code_container_not_found"));
            }
            return Ok(());
        };
        let current = self.read_code_registry().await?;
        if let Some(resource) = current
            .current
            .as_ref()
            .and_then(|id| current.resources.get(id))
        {
            if resource.allocation.state == ContextState::Ready {
                let live = resource.allocation.expires_at_ms > util::now_ms()
                    && self.resource_is_live(record, resource).await?;
                if !live {
                    let meter = ResourceMeter::new(&self.env, &record.owner_id, &record.id, run)?;
                    if !self
                        .retire_code_resource(
                            &resource.allocation.allocation_id,
                            Retirement::ConfirmedLoss,
                            &meter,
                        )
                        .await?
                    {
                        return Err(fail("code_resource_cleanup_pending"));
                    }
                }
            }
        }
        let backend = match &options.executor {
            Some(crate::hosted_bindings::ExecutorBinding::Pinned { profile }) => {
                ExecutionBackend::Executor {
                    profile: profile.clone(),
                }
            }
            Some(_) => return Err(fail("code_executor_profile_unresolved")),
            None => ExecutionBackend::Managed,
        };
        let network_policy = network_policy.clone();
        let now = util::now_ms();
        let mut allocation = self
            .update_code_registry(|registry| {
                if let Some(resource) = registry
                    .current
                    .as_ref()
                    .and_then(|id| registry.resources.get(id))
                {
                    if matches!(
                        resource.allocation.state,
                        ContextState::Reserved | ContextState::Ready
                    ) {
                        if serde_json::to_vec(&resource.policy)?
                            != serde_json::to_vec(&network_policy)?
                        {
                            return Err(fail("code_network_policy_conflict"));
                        }
                        return Ok(resource.allocation.clone());
                    }
                }
                let allocation = Allocation {
                    allocation_id: util::id("code-allocation"),
                    account_id: record.owner_id.clone(),
                    source_session_id: record.id.clone(),
                    source_profile: record.runtime_profile,
                    funding_run_id: run.into(),
                    executor_kind: backend.kind(),
                    memory_limit: if backend.is_managed() {
                        None
                    } else {
                        Some(options.allocated_memory)
                    },
                    cleanup_run_id: None,
                    executor_lease_key: match &backend {
                        ExecutionBackend::Executor { profile } => Some(profile.lease_key()),
                        _ => None,
                    },
                    container_id: None,
                    daemon_boot_id: None,
                    policy_revision: None,
                    state: ContextState::Reserved,
                    created_at_ms: now,
                    last_used_at_ms: now,
                    expires_at_ms: now.saturating_add(IDLE_MS),
                    ended_at_ms: None,
                    allocation_released_at_ms: None,
                };
                registry.current = Some(allocation.allocation_id.clone());
                registry.resources.insert(
                    allocation.allocation_id.clone(),
                    Resource {
                        allocation: allocation.clone(),
                        policy: network_policy,
                        backend: backend.clone(),
                        cleanup: None,
                        preparation_started: false,
                        namespace_retired: false,
                        retry_at_ms: 0,
                        last_use: None,
                        active_borrowed: None,
                    },
                );
                Ok(allocation)
            })
            .await?;
        if allocation.executor_kind == ExecutorKind::WorkspaceDaemon
            && allocation.cleanup_run_id.is_none()
        {
            let id = format!("code-cleanup:{}", allocation.allocation_id);
            let pricing = crate::resources::ResourcePricing::parse(
                &self.env.var("RESOURCE_PRICING_JSON")?.to_string(),
            )?;
            let reservation =
                crate::reconcile::maintenance_account_reservation(&self.env, &record.owner_id, &id)
                    .await?;
            if reservation.state != agents_auth::billing::ReservationState::Reserved {
                return Err(fail("code_executor_cleanup_unfunded"));
            }
            allocation = self
                .update_code_registry(|registry| {
                    let resource = registry
                        .resources
                        .get_mut(&allocation.allocation_id)
                        .ok_or_else(|| fail("code_resource_state_corrupt"))?;
                    resource.allocation.cleanup_run_id = Some(id.clone());
                    resource.cleanup = Some(CleanupFunding { id, pricing });
                    Ok(resource.allocation.clone())
                })
                .await?;
        }
        if allocation.state == ContextState::Reserved {
            let meter = ResourceMeter::new(&self.env, &record.owner_id, &record.id, run)?;
            let capacity = Store::new(&self.env)
                .account_plan(&record.owner_id)
                .await?
                .limits()
                .max_concurrent_sessions;
            catalog::claim(&self.env, &allocation, capacity, &meter).await?;
        }
        Ok(())
    }

    pub(super) async fn begin_local_code_prepare(
        &self,
        record: &SessionRecord,
        run: &str,
        options: &CodeOptions,
    ) -> Result<()> {
        self.ensure_local_code_resource(record, run, options)
            .await?;
        self.update_code_registry(|registry| {
            let resource = registry
                .current
                .as_ref()
                .and_then(|id| registry.resources.get_mut(id))
                .ok_or_else(|| fail("code_resource_not_admitted"))?;
            if !matches!(
                resource.allocation.state,
                ContextState::Reserved | ContextState::Ready
            ) || (resource.allocation.state == ContextState::Ready
                && resource.allocation.expires_at_ms <= util::now_ms())
            {
                return Err(fail("code_container_not_found"));
            }
            resource.preparation_started = true;
            Ok(())
        })
        .await
    }

    pub(super) async fn bind_local_code_context(
        &self,
        record: &SessionRecord,
        run: &str,
        execution: &str,
        cid: &str,
        _options: &CodeOptions,
        boot: &str,
    ) -> Result<()> {
        let boot = boot.to_owned();
        let now = util::now_ms();
        let (allocation, context, used_at) = self
            .update_code_registry(|registry| {
                let resource = registry
                    .current
                    .as_ref()
                    .and_then(|id| registry.resources.get_mut(id))
                    .ok_or_else(|| fail("code_resource_not_admitted"))?;
                if !matches!(
                    resource.allocation.state,
                    ContextState::Reserved | ContextState::Ready
                ) {
                    return Err(fail("code_container_not_found"));
                }
                if resource
                    .allocation
                    .container_id
                    .as_deref()
                    .is_some_and(|old| old != cid)
                {
                    return Err(fail("code_context_generation_changed"));
                }
                let context = OwnedContext {
                    id: cid.into(),
                    source_session: record.id.clone(),
                    profile: record.runtime_profile,
                    executor_kind: resource.allocation.executor_kind,
                    memory_limit: resource.allocation.memory_limit,
                    daemon_boot: boot,
                    policy_revision: resource
                        .allocation
                        .policy_revision
                        .clone()
                        .unwrap_or_else(|| util::id("code-policy")),
                };
                resource.allocation.container_id = Some(context.id.clone());
                resource.allocation.daemon_boot_id = Some(context.daemon_boot.clone());
                resource.allocation.policy_revision = Some(context.policy_revision.clone());
                resource.allocation.state = ContextState::Ready;
                if resource.last_use.as_deref() != Some(execution) {
                    resource.allocation.last_used_at_ms =
                        now.max(resource.allocation.last_used_at_ms);
                    resource.allocation.expires_at_ms =
                        resource.allocation.last_used_at_ms.saturating_add(IDLE_MS);
                    resource.last_use = Some(execution.into());
                }
                Ok((
                    resource.allocation.allocation_id.clone(),
                    context,
                    resource.allocation.last_used_at_ms,
                ))
            })
            .await?;
        let meter = ResourceMeter::new(&self.env, &record.owner_id, &record.id, run)?;
        catalog::bind(
            &self.env,
            &record.owner_id,
            &allocation,
            &context,
            used_at,
            &meter,
        )
        .await
    }

    pub(super) async fn finish_local_code_use(&self, run: &str) -> Result<()> {
        if !self
            .data
            .borrow()
            .as_ref()
            .is_some_and(|data| data.code_resources_seen)
        {
            return Ok(());
        }
        let record = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(|| fail("session_not_initialized"))?
            .record
            .clone();
        let unprepared = !self.workspace(&record).physical_workspace_running().await?;
        let unused = self
            .update_code_registry(|registry| {
                let Some(id) = registry.current.as_ref() else {
                    return Ok(None);
                };
                let resource = registry
                    .resources
                    .get_mut(id)
                    .ok_or_else(|| fail("code_resource_state_corrupt"))?;
                if resource.allocation.state == ContextState::Reserved
                    && (!resource.preparation_started
                        || (unprepared && resource.backend.is_managed()))
                    && resource.allocation.funding_run_id == run
                {
                    resource.allocation.state = ContextState::Expired;
                    resource.allocation.ended_at_ms = Some(util::now_ms());
                    return Ok(Some(resource.allocation.allocation_id.clone()));
                }
                Ok(None)
            })
            .await?;
        if let Some(id) = unused {
            let meter = ResourceMeter::new(&self.env, &record.owner_id, &record.id, run)?;
            catalog::end(
                &self.env,
                &record.owner_id,
                &id,
                false,
                util::now_ms(),
                &meter,
            )
            .await?;
            catalog::release_allocation(&self.env, &record.owner_id, &id, util::now_ms(), &meter)
                .await?;
            let cleanup = self
                .read_code_registry()
                .await?
                .resources
                .get(&id)
                .and_then(|resource| resource.cleanup.clone());
            if let Some(cleanup) = cleanup {
                ResourceMeter::with_account_pricing(
                    &self.env,
                    &record.owner_id,
                    &cleanup.id,
                    cleanup.pricing,
                )?
                .settle_run()
                .await?;
            }
            self.update_code_registry(|registry| {
                registry.resources.remove(&id);
                if registry.current.as_ref() == Some(&id) {
                    registry.current = None;
                }
                Ok(())
            })
            .await?;
        }
        Ok(())
    }

    pub(super) async fn code_retains_compute(&self) -> Result<bool> {
        if !self
            .data
            .borrow()
            .as_ref()
            .is_some_and(|data| data.code_resources_seen)
        {
            return Ok(false);
        }
        Ok(self.read_code_registry().await?.retained(util::now_ms()))
    }

    pub(super) async fn retained_code_ids(&self) -> Result<Vec<String>> {
        if !self
            .data
            .borrow()
            .as_ref()
            .is_some_and(|data| data.code_resources_seen)
        {
            return Ok(vec![]);
        }
        Ok(self
            .read_code_registry()
            .await?
            .resources
            .into_values()
            .filter(|resource| {
                resource.allocation.state == ContextState::Ready
                    && resource.allocation.expires_at_ms > util::now_ms()
            })
            .filter_map(|resource| resource.allocation.container_id)
            .collect())
    }

    async fn retire_code_resource(
        &self,
        id: &str,
        cause: Retirement,
        meter: &ResourceMeter<'_>,
    ) -> Result<bool> {
        let resource = self
            .update_code_registry(|registry| {
                let resource = registry
                    .resources
                    .get_mut(id)
                    .ok_or_else(|| fail("code_container_not_found"))?;
                if !cause.revokes(&resource.allocation, util::now_ms()) {
                    return Ok(None);
                }
                resource.allocation.state = if cause == Retirement::ExplicitDelete {
                    ContextState::Deleted
                } else if resource.allocation.state == ContextState::Deleted {
                    ContextState::Deleted
                } else {
                    ContextState::Expired
                };
                resource
                    .allocation
                    .ended_at_ms
                    .get_or_insert_with(util::now_ms);
                resource.retry_at_ms = util::now_ms().saturating_add(1000);
                Ok(Some(resource.clone()))
            })
            .await?;
        let Some(resource) = resource else {
            return Ok(false);
        };
        let record = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(|| fail("session_not_initialized"))?
            .record
            .clone();
        if resource.allocation.allocation_released_at_ms.is_some() {
            if let Some(cleanup) = &resource.cleanup {
                ResourceMeter::with_account_pricing(
                    &self.env,
                    &record.owner_id,
                    &cleanup.id,
                    cleanup.pricing.clone(),
                )?
                .settle_run()
                .await?;
            }
            self.update_code_registry(|registry| {
                registry.resources.remove(id);
                if registry.current.as_deref() == Some(id) {
                    registry.current = None;
                }
                Ok(())
            })
            .await?;
            return Ok(true);
        }
        catalog::end(
            &self.env,
            &record.owner_id,
            id,
            resource.allocation.state == ContextState::Deleted,
            util::now_ms(),
            meter,
        )
        .await?;
        let Some(_operation) = self.code_operation.try_lock() else {
            if cause == Retirement::ExplicitDelete {
                if let Some(execution) = resource.active_borrowed.as_deref() {
                    match &resource.backend {
                        ExecutionBackend::Managed => {
                            self.workspace(&record).cancel_python(execution).await?
                        }
                        ExecutionBackend::Executor { profile } => {
                            use sha2::{Digest, Sha256};
                            profile
                                .open(&self.env, &record.owner_id, None)
                                .await?
                                .cancel(&format!("{:x}", Sha256::digest(execution.as_bytes())))
                                .await?;
                        }
                    }
                    return Ok(false);
                }
                let response = self
                    .data
                    .borrow()
                    .as_ref()
                    .and_then(|data| data.run.as_ref())
                    .and_then(|run| run.response.as_ref())
                    .map(|response| response.id.clone());
                if let Some(response) = response {
                    self.cancel_current_hosted_code(&response, &record).await?;
                }
            }
            return Ok(false);
        };
        let workspace = self.workspace(&record);
        if !resource.namespace_retired {
            let stopped = match &resource.backend {
                ExecutionBackend::Managed => {
                    let context = match resource.allocation.container_id.as_deref() {
                        Some(cid) => Some(cid.to_owned()),
                        None if resource.preparation_started => workspace.python_context().await?,
                        None => None,
                    };
                    match context {
                        Some(cid) => workspace.retire_python_context(&cid).await?,
                        None => true,
                    }
                }
                ExecutionBackend::Executor { profile } => {
                    let executor = profile.open(&self.env, &record.owner_id, None).await?;
                    let context = match resource.allocation.container_id.as_deref() {
                        Some(cid) => Some(cid.to_owned()),
                        None if resource.preparation_started => executor.current_context().await?,
                        None => None,
                    };
                    match context {
                        Some(cid) => executor.retire(&cid).await?,
                        None => true,
                    }
                }
            };
            if !stopped {
                return Ok(false);
            }
            self.update_code_registry(|registry| {
                if let Some(resource) = registry.resources.get_mut(id) {
                    resource.namespace_retired = true;
                }
                Ok(())
            })
            .await?;
        }
        let pending = match resource.backend {
            ExecutionBackend::Managed => workspace.python_publication_pending().await?,
            ExecutionBackend::Executor { .. } => {
                crate::code_executor_runtime::publication_pending(&self.state.storage()).await?
            }
        };
        if pending {
            return Ok(false);
        }
        let release_physical = !self.code_retains_compute().await?
            && (record.status == SessionStatus::Closed || !record.pinned.workspace)
            && (workspace.physical_workspace_running().await?
                || workspace.cleanup_funding().await?.is_some());
        let lost = if release_physical && !workspace.physical_workspace_running().await? {
            workspace.observe_lost_code_incarnation().await?
        } else {
            None
        };
        if release_physical && lost.is_none() {
            workspace.release_preserving_funding().await?;
            let snapshot = workspace.snapshot_metadata().await?;
            let changed = if let Some(data) = self.data.borrow_mut().as_mut() {
                let changed = data.record.workspace_snapshot != snapshot;
                data.record.workspace_snapshot = snapshot;
                changed
            } else {
                false
            };
            if changed {
                self.persist().await?;
            }
        }
        catalog::release_allocation(&self.env, &record.owner_id, id, util::now_ms(), meter).await?;
        self.update_code_registry(|registry| {
            if let Some(resource) = registry.resources.get_mut(id) {
                resource.allocation.allocation_released_at_ms = Some(util::now_ms());
            }
            Ok(())
        })
        .await?;
        if let Some(lost) = lost {
            workspace.detach_lost_code_incarnation(&lost).await?;
            util::emit(
                "code.incarnation_lost",
                json!({"session_id":record.id,"financial_outcome":"unknown","heap_recoverable":false}),
            );
        } else if release_physical {
            let outcome = workspace.release().await?;
            if outcome.accounting_pending {
                util::emit(
                    "code.cleanup_accounting_pending",
                    json!({"session_id":record.id}),
                );
            }
        }
        if let Some(cleanup) = resource.cleanup {
            if ResourceMeter::with_account_pricing(
                &self.env,
                &record.owner_id,
                &cleanup.id,
                cleanup.pricing,
            )?
            .settle_run()
            .await
            .is_err()
            {
                return Ok(true);
            }
        }
        self.update_code_registry(|registry| {
            registry.resources.remove(id);
            if registry.current.as_deref() == Some(id) {
                registry.current = None;
            }
            Ok(())
        })
        .await?;
        Ok(true)
    }

    pub(super) async fn reconcile_code_resources(&self) -> Result<()> {
        if self
            .code_deadline
            .get()
            .is_none_or(|deadline| deadline > util::now_ms())
        {
            return Ok(());
        }
        let record = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(|| fail("session_not_initialized"))?
            .record
            .clone();
        let workspace = self.workspace(&record);
        let warm = workspace.cleanup_funding().await?;
        let registry = self.read_code_registry().await?;
        let due = registry
            .resources
            .iter()
            .filter(|(_, resource)| {
                matches!(
                    resource.allocation.state,
                    ContextState::Expired | ContextState::Deleted
                ) && resource.retry_at_ms <= util::now_ms()
                    || matches!(
                        resource.allocation.state,
                        ContextState::Reserved | ContextState::Ready
                    ) && resource.allocation.expires_at_ms <= util::now_ms()
            })
            .take(8);
        let mut failed = false;
        for (id, resource) in due {
            let meter = if let Some(cleanup) = &resource.cleanup {
                ResourceMeter::with_account_pricing(
                    &self.env,
                    &record.owner_id,
                    &cleanup.id,
                    cleanup.pricing.clone(),
                )?
            } else if let Some(warm) = &warm {
                ResourceMeter::with_pricing(
                    &self.env,
                    &record.owner_id,
                    &record.id,
                    &warm.id,
                    warm.pricing.clone(),
                )?
            } else {
                if resource.allocation.allocation_released_at_ms.is_some() {
                    self.update_code_registry(|registry| {
                        registry.resources.remove(id);
                        if registry.current.as_deref() == Some(id) {
                            registry.current = None;
                        }
                        Ok(())
                    })
                    .await?;
                } else {
                    failed = true;
                }
                continue;
            };
            if self
                .retire_code_resource(id, Retirement::Idle, &meter)
                .await
                .is_err()
            {
                failed = true;
            }
        }
        let registry = self.read_code_registry().await?;
        if registry.retained(util::now_ms()) {
            workspace.configure_inactivity_if_running().await?;
        }
        self.code_deadline.set(if failed {
            Some(util::now_ms().saturating_add(10_000))
        } else {
            registry.deadline(util::now_ms())
        });
        Ok(())
    }

    pub(super) async fn code_resource_route(&self, mut request: Request) -> Result<Response> {
        let owner = self
            .data
            .borrow()
            .as_ref()
            .ok_or_else(|| fail("session_not_initialized"))?
            .record
            .owner_id
            .clone();
        if request.headers().get("X-Agents-Code-Owner")?.as_deref() != Some(owner.as_str()) {
            return util::json_error(404, "not_found", "Code container not found");
        }
        let headers = request.headers().clone();
        let operation = crate::conversation_meter::ConversationOperation::actor_scope(&headers)?;
        let meter = ResourceMeter::account(&self.env, &owner, operation.id())?;
        crate::conversation_meter::actor(&self.env, &owner, &headers, async {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Reference { allocation_id: String, container_id: String }
            let input: Reference = request.json().await?;
            let resource = self.read_code_registry().await?.resources.remove(&input.allocation_id)
                .filter(|resource| resource.allocation.account_id == owner && resource.allocation.container_id.as_deref() == Some(input.container_id.as_str()));
            let Some(resource) = resource else { return util::json_error(404,"not_found","Code container not found"); };
            match request.path().as_str() {
                "/code-resources/inspect" => {
                    let record = self.data.borrow().as_ref().unwrap().record.clone();
                    let running = resource.allocation.state == ContextState::Ready
                        && resource.allocation.expires_at_ms > util::now_ms()
                        && self.resource_is_live(&record,&resource).await?;
                    Response::from_json(&catalog::public(&resource.allocation, running))
                }
                "/code-resources/delete" => {
                    if self.retire_code_resource(&input.allocation_id, Retirement::ExplicitDelete, &meter).await? {
                        Ok(Response::empty()?.with_status(204))
                    } else {
                        util::json_error(409,"code_context_release_pending","Code execution or artifact publication is still being stopped; retry this deletion")
                    }
                }
                _ => util::json_error(404,"not_found","Unknown code resource operation"),
            }
        }).await
    }
}

impl SessionInner {
    pub(super) async fn local_code_backend(&self, cid: &str) -> Result<ExecutionBackend> {
        self.read_code_registry()
            .await?
            .resources
            .into_values()
            .find(|resource| resource.allocation.container_id.as_deref() == Some(cid))
            .map(|resource| resource.backend)
            .ok_or_else(|| fail("code_container_not_found"))
    }

    pub(super) async fn local_code_allocation(&self, cid: &str) -> Result<Option<Allocation>> {
        Ok(self
            .read_code_registry()
            .await?
            .resources
            .into_values()
            .find(|resource| resource.allocation.container_id.as_deref() == Some(cid))
            .map(|resource| resource.allocation))
    }

    pub(super) async fn finish_borrowed_code(&self, cid: &str, execution: &str) -> Result<()> {
        self.update_code_registry(|registry| {
            if let Some(resource) = registry
                .resources
                .values_mut()
                .find(|resource| resource.allocation.container_id.as_deref() == Some(cid))
            {
                if resource.active_borrowed.as_deref() == Some(execution) {
                    resource.active_borrowed = None;
                }
            }
            Ok(())
        })
        .await
    }

    pub(super) async fn touch_borrowed_code(
        &self,
        allocation: &str,
        execution: &str,
        meter: &ResourceMeter<'_>,
    ) -> Result<()> {
        let now = util::now_ms();
        let (owner, used_at) = self
            .update_code_registry(|registry| {
                let resource = registry
                    .resources
                    .get_mut(allocation)
                    .ok_or_else(|| fail("code_container_not_found"))?;
                if resource.allocation.state != ContextState::Ready
                    || resource.allocation.expires_at_ms <= now
                {
                    return Err(fail("code_container_not_found"));
                }
                resource.active_borrowed = Some(execution.into());
                if resource.last_use.as_deref() != Some(execution) {
                    resource.allocation.last_used_at_ms =
                        now.max(resource.allocation.last_used_at_ms);
                    resource.allocation.expires_at_ms =
                        resource.allocation.last_used_at_ms.saturating_add(IDLE_MS);
                    resource.last_use = Some(execution.into());
                }
                Ok((
                    resource.allocation.account_id.clone(),
                    resource.allocation.last_used_at_ms,
                ))
            })
            .await?;
        catalog::touch(&self.env, &owner, allocation, used_at, meter).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(now: u64) -> Registry {
        let resource = Resource {
            allocation: Allocation {
                allocation_id: "allocation".into(),
                account_id: "owner".into(),
                source_session_id: "source".into(),
                source_profile: agents_core::RuntimeProfile::Basic1g,
                funding_run_id: "run".into(),
                executor_kind: ExecutorKind::Managed,
                memory_limit: None,
                cleanup_run_id: None,
                executor_lease_key: None,
                container_id: Some(format!("cntr_{}", "a".repeat(32))),
                daemon_boot_id: Some("boot".into()),
                policy_revision: Some("policy".into()),
                state: ContextState::Ready,
                created_at_ms: now,
                last_used_at_ms: now,
                expires_at_ms: now + IDLE_MS,
                ended_at_ms: None,
                allocation_released_at_ms: None,
            },
            policy: NetworkPolicy::Disabled,
            backend: ExecutionBackend::Managed,
            cleanup: None,
            preparation_started: true,
            namespace_retired: false,
            retry_at_ms: 0,
            last_use: Some("execution".into()),
            active_borrowed: None,
        };
        Registry {
            current: Some("allocation".into()),
            resources: BTreeMap::from([("allocation".into(), resource)]),
        }
    }

    #[test]
    fn harness_timeout_does_not_expire_code_or_renew_its_resource_clock() {
        let now = 1000;
        let state = registry(now);
        assert!(state.retained(now + 10 * 60 * 1000));
        assert_eq!(state.deadline(now), Some(now + HEARTBEAT_MS));
        assert_eq!(state.deadline(now + 19 * 60 * 1000), Some(now + IDLE_MS));
        assert!(!state.retained(now + IDLE_MS));
        let restored: Registry =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(
            restored.resources["allocation"].allocation.last_used_at_ms,
            now
        );
        assert_eq!(
            restored.resources["allocation"].allocation.expires_at_ms,
            now + IDLE_MS
        );
    }

    #[test]
    fn a_stale_due_scan_cannot_revoke_a_subsequently_renewed_kernel() {
        let mut state = registry(1000);
        let allocation = &mut state.resources.get_mut("allocation").unwrap().allocation;
        let old_deadline = allocation.expires_at_ms;
        assert!(Retirement::Idle.revokes(allocation, old_deadline));
        allocation.expires_at_ms = old_deadline + IDLE_MS;
        assert!(!Retirement::Idle.revokes(allocation, old_deadline));
        assert!(Retirement::ExplicitDelete.revokes(allocation, old_deadline));
        assert!(Retirement::ConfirmedLoss.revokes(allocation, old_deadline));
    }

    #[test]
    fn reserved_prepare_and_failed_release_always_keep_a_cleanup_alarm() {
        let now = 1000;
        let mut state = registry(now);
        let resource = state.resources.get_mut("allocation").unwrap();
        resource.allocation.state = ContextState::Reserved;
        assert_eq!(state.deadline(now), Some(now + IDLE_MS));
        let resource = state.resources.get_mut("allocation").unwrap();
        resource.allocation.state = ContextState::Deleted;
        resource.retry_at_ms = now + 5000;
        assert!(!state.retained(now));
        assert_eq!(state.deadline(now), Some(now + 5000));
    }
}
