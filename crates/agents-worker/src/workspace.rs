//! Workspace publication and physical release have independent commit points.
//!
//! `workspace:release-intent` is written only after a successful checkpoint and
//! before destroy. It captures the original clock and lease identity. A saved
//! destroy acknowledgement supplies the known clock end; a stopped container
//! without that acknowledgement leaves its financial hold unresolved. Neither
//! ledger errors nor missing exit timestamps undo confirmed physical release.
//! The completed outcome is saved before active pointers are detached, and
//! retries finish detachment using the saved identities. Pending accounting is
//! retained under `workspace:release-accounting:<id>` for reconciliation. No
//! intent plus an unexpectedly stopped dirty incarnation is a data-recovery
//! error, not a successful archive. The session owner serializes these flows.

use std::time::Duration;

use agents_auth::billing::Resource;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue};
use worker::{
    Container, Delay, Env, Error, Headers, Method, RequestInit, ResponseBody, Result, State,
};

use crate::{
    resources::ResourceMeter,
    telemetry::{NativeSpan, TraceContext},
};

#[path = "workspace_archives.rs"]
mod archives;
#[path = "workspace_download.rs"]
mod download;
#[path = "workspace_files.rs"]
mod files;
#[path = "workspace_interpreter.rs"]
mod interpreter;
#[path = "workspace_lease.rs"]
mod lease;
pub use interpreter::{
    PythonArtifact, PythonDomainSecret, PythonFilePage, PythonInput, PythonMemoryTier,
    PythonNetworkPolicy, PythonOutcome, PythonRequest,
};
#[path = "workspace_transport.rs"]
mod transport;

pub(crate) use crate::workspace_blobs::OwnedArchiveRef;
#[path = "workspace_lost_code.rs"]
mod lost_code;

use lease::ComputeLease;
pub(crate) use transport::{Deadline, TimedResponse};

const PORT: u16 = 8080;
const STARTUP_BUDGET_MS: u64 = 120_000;
const STARTUP_PROBE_MS: u64 = 5_000;
const INACTIVITY_FALLBACK_MS: u64 = 15 * 60 * 1000;
const BATCH_TOOLS: u64 = 8;
const BATCH_BYTES: u64 = 64 * 1024 * 1024;
const CONTAINER_CLOCK: &str = "workspace:container-clock";
const RELEASE_INTENT: &str = "workspace:release-intent";
const STARTUP_PENDING: &str = "workspace:startup-pending";

/// A session DO is the sole writer. Its caller must serialize workspace operations,
/// including checkpoint/release, against tool execution.
pub struct Workspace<'a> {
    runtime_profile: agents_core::RuntimeProfile,
    state: &'a State,
    env: &'a Env,
    prefix: String,
    owner: String,
    session: String,
    run: Option<String>,
    trace: Option<TraceContext>,
    code_existing_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckpointStats {
    pub archive_bytes: u64,
    pub logical_bytes: u64,
    pub entries: u64,
    pub r2_write_operations: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReleaseOutcome {
    pub checkpoint: CheckpointStats,
    pub accounting_pending: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct ReleaseIntent {
    id: String,
    checkpoint: CheckpointStats,
    clock: UsageClock,
    lease_id: Option<String>,
    destroyed_at_ms: Option<u64>,
    outcome: Option<ReleaseOutcome>,
    #[serde(default)]
    startup_pending: bool,
}

impl ReleaseIntent {
    fn completed_outcome(&self, running: bool) -> Result<Option<ReleaseOutcome>> {
        if running && self.outcome.is_some() {
            return Err(Error::RustError(
                "workspace_release_generation_conflict".into(),
            ));
        }
        Ok(self.outcome.clone())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    format_version: u8,
    object_key: String,
    sha256: String,
    stats: CheckpointStats,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    created_at_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct VersionCommit {
    operation: String,
    name: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct VersionRestore {
    operation: String,
    version_id: String,
    safety_version_id: String,
    manifest: Manifest,
}

const VERSION_RESTORE: &str = "workspace:version-restore";

#[derive(Clone, Default, Serialize, Deserialize)]
struct UsageClock {
    quantity: u64,
    since_ms: u64,
    run: Option<String>,
    event: Option<String>,
    end_ms: Option<u64>,
}

struct R2Observation {
    event: String,
    lease: Option<String>,
}

impl<'a> Workspace<'a> {
    pub fn new(state: &'a State, env: &'a Env, owner: &str, session: &str) -> Self {
        // Fixed-length, collision-resistant components cannot inject R2 path segments.
        Self {
            runtime_profile: agents_core::RuntimeProfile::Basic1g,
            state,
            env,
            prefix: format!("workspaces/{}/{}", digest_key(owner), digest_key(session)),
            owner: owner.into(),
            session: session.into(),
            run: None,
            trace: None,
            code_existing_only: false,
        }
    }

    pub async fn cleanup_funding(&self) -> Result<Option<crate::cleanup::CleanupHold>> {
        Ok(self
            .lease()
            .current()
            .await?
            .map(|lease| crate::cleanup::CleanupHold {
                id: lease.id,
                pricing: lease.pricing,
            }))
    }

    pub fn with_runtime_profile(mut self, profile: agents_core::RuntimeProfile) -> Self {
        self.runtime_profile = profile;
        self
    }

    pub fn for_run(mut self, run: &str) -> Self {
        self.run = Some(run.into());
        self
    }

    pub(crate) fn existing_code_only(mut self) -> Self {
        self.code_existing_only = true;
        self
    }

    pub fn with_traceparent(mut self, header: Option<String>) -> Self {
        self.trace = header.as_deref().and_then(TraceContext::parse);
        self
    }

    fn meter(&self) -> Result<ResourceMeter<'_>> {
        let run = self
            .run
            .as_deref()
            .ok_or_else(|| Error::RustError("workspace_resource_reservation_required".into()))?;
        ResourceMeter::new(self.env, &self.owner, &self.session, run)
    }

    fn lease(&self) -> ComputeLease<'_> {
        ComputeLease {
            state: self.state,
            env: self.env,
            owner: &self.owner,
            session: &self.session,
        }
    }

    async fn require_r2(&self, unit: &str) -> Result<R2Observation> {
        let event = format!("workspace:r2:{}", uuid::Uuid::new_v4());
        let lease = if self.run.is_some() {
            self.meter()?
                .require_observation(&event, Resource::R2Operation, unit)
                .await?;
            None
        } else {
            let lease = self
                .lease()
                .current()
                .await?
                .ok_or_else(|| Error::RustError("workspace_lease_missing".into()))?;
            self.lease()
                .meter(&lease.id)
                .await?
                .require_observation(&event, Resource::R2Operation, unit)
                .await?;
            Some(lease.id)
        };
        Ok(R2Observation { event, lease })
    }

    async fn record_r2(&self, observation: &R2Observation, unit: &str) -> Result<()> {
        if let Some(id) = &observation.lease {
            self.lease()
                .meter(id)
                .await?
                .record_observation(&observation.event, Resource::R2Operation, unit, 1)
                .await
        } else {
            self.meter()?
                .record_observation(&observation.event, Resource::R2Operation, unit, 1)
                .await
        }
    }

    async fn finish_container_clock(&self) -> Result<()> {
        let key = CONTAINER_CLOCK;
        let storage = self.state.storage();
        let Some(mut clock) = storage.get::<UsageClock>(key).await? else {
            return Ok(());
        };
        let (Some(run), Some(event)) = (&clock.run, &clock.event) else {
            return Ok(());
        };
        let end = *clock
            .end_ms
            .get_or_insert_with(|| worker::Date::now().as_millis());
        storage.put(key, &clock).await?;
        let lease = self.lease();
        let meter = lease.meter(run).await?;
        meter
            .record_wall_time(event, Resource::Container, clock.since_ms, end)
            .await?;
        clock.since_ms = end;
        clock.run = None;
        clock.event = None;
        clock.end_ms = None;
        storage.put(key, &clock).await
    }

    async fn arm_container_clock(&self) -> Result<()> {
        let key = CONTAINER_CLOCK;
        let lease = self
            .lease()
            .current()
            .await?
            .ok_or_else(|| Error::RustError("workspace_lease_missing".into()))?;
        let run = lease.id.as_str();
        let storage = self.state.storage();
        let Some(mut clock) = storage.get::<UsageClock>(key).await? else {
            return Ok(());
        };
        if clock.quantity == 0 {
            return Ok(());
        }
        if clock.run.as_deref() == Some(run) && clock.end_ms.is_none() {
            return Ok(());
        }
        self.finish_container_clock().await?;
        clock = storage.get::<UsageClock>(key).await?.unwrap_or_default();
        let event = format!("workspace:container:{}", uuid::Uuid::new_v4());
        self.lease()
            .meter(run)
            .await?
            .require_observation(&event, Resource::Container, "wall_ms")
            .await?;
        clock.run = Some(run.into());
        clock.event = Some(event);
        storage.put(key, &clock).await
    }

    /// Only allocation time belongs to the warm lease. Active harness DO time
    /// belongs to SessionInner::observed_step; never meter it from this clock.
    /// Idle controller overhead is included in the published workspace tariff.
    pub async fn arm_usage(&self) -> Result<()> {
        self.arm_container_clock().await
    }

    /// Renew only existing compute. Workspace-enabled time-only sessions never
    /// create a reservation or container through this hook.
    pub async fn ensure_compute_lease(&self) -> Result<()> {
        if !self.container()?.running() {
            return Ok(());
        }
        self.renew_compute_lease().await?;
        self.configure_inactivity_if_running().await
    }

    /// This native fallback survives DO inactivity, not DO reinstantiation.
    /// It never renews funding or changes the session's user-input deadline.
    pub async fn configure_inactivity_if_running(&self) -> Result<()> {
        let container = self.container()?;
        if !container.running() {
            return Ok(());
        }
        self.configure_inactivity(&container).await
    }

    async fn configure_inactivity(&self, container: &Container) -> Result<()> {
        let lease = self
            .lease()
            .current()
            .await?
            .ok_or_else(|| Error::RustError("workspace_lease_missing".into()))?;
        let timeout = inactivity_timeout_ms(lease.expires_ms, worker::Date::now().as_millis())?;
        let method = js_sys::Reflect::get(container.as_ref(), &"setInactivityTimeout".into())?
            .dyn_into::<js_sys::Function>()?;
        let promise = method
            .call1(container.as_ref(), &JsValue::from_f64(timeout as f64))?
            .dyn_into::<js_sys::Promise>()?;
        wasm_bindgen_futures::JsFuture::from(promise).await?;
        Ok(())
    }

    async fn renew_compute_lease(&self) -> Result<()> {
        self.renew_compute_lease_covering(None).await
    }

    pub(crate) async fn ensure_code_lease(&self) -> Result<()> {
        if !self.container()?.running() {
            return Err(Error::RustError("code_container_not_found".into()));
        }
        // Fund the independent idle clock and the bounded native prepare before
        // extending resource lifetime; background heartbeats never renew funds.
        let required =
            crate::util::now_ms().saturating_add(crate::code_contexts::IDLE_MS + 120_000);
        self.renew_compute_lease_covering(Some(required)).await?;
        self.configure_inactivity_if_running().await
    }

    async fn renew_compute_lease_covering(&self, minimum_until: Option<u64>) -> Result<()> {
        let leases = self.lease();
        let Some(renewal) = leases.prepare(minimum_until).await? else {
            return Ok(());
        };
        self.finish_container_clock().await?;
        if let Some(previous) = &renewal.previous {
            leases.settle(&previous.id).await?;
        }
        leases.activate(&renewal.next).await?;
        self.arm_usage().await
    }

    /// Close an observed interval without settling the independent warm lease.
    /// Normally release and lease renewal perform this automatically.
    pub async fn meter_usage(&self) -> Result<()> {
        self.finish_container_clock().await
    }

    fn container(&self) -> Result<Container> {
        self.state
            .container()
            .ok_or_else(|| Error::RustError("Session container binding is missing".into()))
    }

    async fn token(&self) -> Result<String> {
        let storage = self.state.storage();
        if let Some(token) = storage.get::<String>("workspace:token").await? {
            return Ok(token);
        }
        let token = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        storage.put("workspace:token", &token).await?;
        Ok(token)
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<JsValue>,
        extra: &[(&str, &str)],
    ) -> Result<TimedResponse> {
        let budget = if matches!(path, "/v1/snapshot" | "/v1/restore") {
            120_000
        } else {
            10_000
        };
        self.request_until(
            method,
            path,
            body,
            extra,
            worker::Date::now().as_millis().saturating_add(budget),
        )
        .await
    }

    async fn request_until(
        &self,
        method: Method,
        path: &str,
        body: Option<JsValue>,
        extra: &[(&str, &str)],
        expires_ms: u64,
    ) -> Result<TimedResponse> {
        let trace = self.trace.as_ref().map(TraceContext::child);
        let _span = NativeSpan::start("workspace.request", trace.as_ref());
        let headers = Headers::new();
        if let Some(trace) = trace {
            trace.inject(&headers)?;
        }
        headers.set("Authorization", &format!("Bearer {}", self.token().await?))?;
        for (name, value) in extra {
            headers.set(name, value)?;
        }
        let mut init = RequestInit::new();
        init.with_method(method)
            .with_headers(headers)
            .with_body(body);
        let deadline = Deadline::new(expires_ms)?;
        let native = web_sys::RequestInit::from(&init);
        native.set_signal(Some(&deadline.signal()));
        let request = worker::Request::from(web_sys::Request::new_with_str_and_init(
            &format!("http://workspace{path}"),
            &native,
        )?);
        let response = deadline
            .wait(self.container()?.get_tcp_port(PORT)?.fetch_request(request))
            .await?;
        Ok(TimedResponse { response, deadline })
    }

    async fn json_request(
        &self,
        method: Method,
        path: &str,
        value: &Value,
    ) -> Result<TimedResponse> {
        let budget = if path == "/v1/exec" {
            value["timeout_ms"]
                .as_u64()
                .unwrap_or(120_000)
                .min(600_000)
                .saturating_add(10_000)
        } else {
            10_000
        };
        self.request_until(
            method,
            path,
            Some(JsValue::from_str(&value.to_string())),
            &[("Content-Type", "application/json")],
            worker::Date::now().as_millis().saturating_add(budget),
        )
        .await
    }

    /// Reconcile a process incarnation, not just DO memory: a live container may
    /// outlast an evicted DO, and must never be overwritten by an older snapshot.
    pub async fn restore_if_needed(&self) -> Result<()> {
        if self.code_existing_only && !self.container()?.running() {
            return Err(Error::RustError("code_container_not_found".into()));
        }
        if self
            .state
            .storage()
            .get::<ReleaseIntent>(RELEASE_INTENT)
            .await?
            .is_some()
        {
            // Finish detachment even if the previous request died after saving
            // its outcome. Never inherit another incarnation's release journal.
            self.release().await?;
            self.state.storage().delete(RELEASE_INTENT).await?;
        }
        let container = self.container().map_err(|_| startup_unavailable())?;
        if !container.running() {
            if let Some(old) = self
                .state
                .storage()
                .get::<UsageClock>("workspace:container-clock")
                .await?
            {
                if old.quantity != 0 {
                    // An unexpected exit has no trustworthy exit timestamp.
                    // Preserve its pending budget hold for delayed reconciliation.
                    if let Some(lease) = self.lease().current().await? {
                        self.lease()
                            .meter(&lease.id)
                            .await?
                            .require_observation(
                                "container:unexpected_exit",
                                Resource::Container,
                                "wall_ms",
                            )
                            .await?;
                    }
                    let key = format!(
                        "workspace:unreconciled:{}",
                        old.event.as_deref().unwrap_or("container")
                    );
                    self.state.storage().put(&key, &old).await?;
                    self.state
                        .storage()
                        .put("workspace:container-clock", UsageClock::default())
                        .await?;
                    self.lease().clear().await?;
                }
            }
            self.renew_compute_lease().await?;
            // Until readiness and restore commit, the previous R2 manifest is
            // authoritative and this incarnation cannot have dispatched Bash.
            self.state.storage().put(STARTUP_PENDING, true).await?;
            self.state
                .storage()
                .put(
                    "workspace:container-clock",
                    UsageClock {
                        quantity: 1,
                        since_ms: worker::Date::now().as_millis(),
                        ..UsageClock::default()
                    },
                )
                .await?;
            self.arm_usage().await?;
            let image_source = self
                .env
                .var("DAEMON_IMAGE_SOURCE")
                .ok()
                .map(|value| value.to_string());
            start_container(
                &container,
                &self.token().await?,
                self.runtime_profile,
                image_source.as_deref(),
            )
            .map_err(|_| startup_unavailable())?;
            self.configure_inactivity(&container).await?;
        } else {
            self.ensure_compute_lease().await?;
        }
        self.arm_usage().await?;
        let mut health = None;
        // Cold image placement can outlast a short command. This budget is
        // independent of Bash timeout_ms and never starts a second container.
        let ready_until = worker::Date::now()
            .as_millis()
            .saturating_add(STARTUP_BUDGET_MS);
        while worker::Date::now().as_millis() < ready_until {
            let probe_until = startup_probe_until(worker::Date::now().as_millis(), ready_until);
            if let Ok(mut response) = self
                .request_until(Method::Get, "/health", None, &[], probe_until)
                .await
            {
                if startup_status_is_fatal(response.status_code()) {
                    return Err(startup_unavailable());
                }
                if response.status_code() == 200 {
                    match response.json::<Value>().await {
                        Ok(value) => {
                            health = Some(value);
                            break;
                        }
                        Err(_) if worker::Date::now().as_millis() >= probe_until => {}
                        Err(_) => return Err(startup_unavailable()),
                    }
                }
            }
            Delay::from(Duration::from_millis(
                ready_until
                    .saturating_sub(worker::Date::now().as_millis())
                    .min(500),
            ))
            .await;
        }
        let health = health.ok_or_else(|| Error::RustError("workspace_startup_timeout".into()))?;
        let boot = health["boot_id"].as_str().ok_or_else(startup_unavailable)?;
        let storage = self.state.storage();
        if storage.get::<String>("workspace:boot").await?.as_deref() == Some(boot) {
            if storage.get::<bool>(STARTUP_PENDING).await?.unwrap_or(false) {
                storage.put("workspace:tools", 0_u64).await?;
                storage.delete(STARTUP_PENDING).await?;
            }
            self.resume_version_restore().await?;
            self.reconcile_current_archive().await?;
            return Ok(());
        }
        if storage
            .get::<bool>("workspace:seed-pending")
            .await?
            .unwrap_or(false)
        {
            let seed = storage
                .get::<Manifest>("workspace:manifest")
                .await?
                .ok_or_else(|| Error::RustError("workspace_branch_seed_missing".into()))?;
            self.publish_manifest(&seed).await?;
            storage.delete("workspace:seed-pending").await?;
        }
        let bucket = self.env.bucket("WORKSPACES")?;
        let manifest_read = self.require_r2("class_b").await?;
        let manifest_object = bucket
            .get(format!("{}/manifest.json", self.prefix))
            .execute()
            .await?;
        self.record_r2(&manifest_read, "class_b").await?;
        if let Some(object) = manifest_object {
            if object.size() > 16 * 1024 {
                return Err(Error::RustError("Workspace manifest exceeds limit".into()));
            }
            let text = object
                .body()
                .ok_or_else(|| Error::RustError("Workspace manifest has no body".into()))?
                .text()
                .await?;
            let manifest: Manifest = serde_json::from_str(&text)?;
            if !self.valid_manifest_archive(&manifest).await? {
                return Err(Error::RustError("Unsupported workspace manifest".into()));
            }
            let snapshot_read = self.require_r2("class_b").await?;
            let snapshot = bucket.get(&manifest.object_key).execute().await?;
            self.record_r2(&snapshot_read, "class_b").await?;
            let snapshot =
                snapshot.ok_or_else(|| Error::RustError("Workspace snapshot is missing".into()))?;
            if snapshot.size() != manifest.stats.archive_bytes {
                return Err(Error::RustError("Workspace snapshot size mismatch".into()));
            }
            let body = snapshot
                .body()
                .ok_or_else(|| Error::RustError("Workspace snapshot has no body".into()))?
                .response_body()?;
            let ResponseBody::Stream(stream) = body else {
                return Err(Error::RustError(
                    "Workspace snapshot is not streamable".into(),
                ));
            };
            let response = self
                .request(
                    Method::Post,
                    "/v1/restore",
                    Some(stream.into()),
                    &[
                        ("Content-Type", "application/zstd"),
                        ("X-Snapshot-Sha256", &manifest.sha256),
                    ],
                )
                .await?;
            ensure_success(response)?;
            storage.put("workspace:manifest", &manifest).await?;
        }
        storage.put("workspace:boot", boot).await?;
        storage.put("workspace:tools", 0_u64).await?;
        storage.delete(STARTUP_PENDING).await?;
        self.resume_version_restore().await?;
        // Resolve a pending restore first: its promoted target must stay held.
        self.reconcile_current_archive().await?;
        Ok(())
    }

    async fn startup_pending(&self) -> Result<bool> {
        let storage = self.state.storage();
        if storage.get::<bool>(STARTUP_PENDING).await?.unwrap_or(false) {
            return Ok(true);
        }
        // Legacy compatibility is deliberately narrower than a failed task:
        // no committed boot and no dispatch fence prove it never reached Bash.
        if storage.get::<String>("workspace:boot").await?.is_some()
            || storage
                .get::<UsageClock>(CONTAINER_CLOCK)
                .await?
                .is_none_or(|clock| clock.quantity == 0)
        {
            return Ok(false);
        }
        let dispatches = storage
            .list_with_options(
                worker::ListOptions::new()
                    .prefix("workspace:dispatch:")
                    .limit(1),
            )
            .await?;
        if dispatches.size() != 0 {
            return Ok(false);
        }
        storage.put(STARTUP_PENDING, true).await?;
        Ok(true)
    }

    pub async fn execute_bash(&self, args: Value) -> Result<Value> {
        self.execute_bash_with_id(&uuid::Uuid::new_v4().to_string(), args)
            .await
    }

    /// A dispatch budget reservation is not an issued effect. Configuration
    /// revocation must let these existing operation fences reconcile first.
    pub(crate) async fn operation_issued(&self, name: &str, operation: &str) -> Result<bool> {
        let storage = self.state.storage();
        match name {
            "bash" => Ok(storage
                .get::<String>(&format!("workspace:dispatch:{}", digest_key(operation)))
                .await?
                .is_some()),
            "workspace_commit" => Ok(crate::workspace_versions::find_operation(
                self.env,
                &self.owner,
                &self.session,
                operation,
            )
            .await?
            .is_some()
                || crate::workspace_versions::pending_upload(
                    self.env,
                    &self.owner,
                    &self.session,
                    operation,
                )
                .await?
                .is_some()),
            "workspace_restore" => Ok(storage
                .get::<Value>(&format!(
                    "workspace:restore-result:{}",
                    digest_key(operation)
                ))
                .await?
                .is_some()
                || storage
                    .get::<VersionRestore>(VERSION_RESTORE)
                    .await?
                    .is_some_and(|intent| intent.operation == operation)),
            _ => Ok(false),
        }
    }

    pub async fn execute_bash_with_id(&self, execution_id: &str, mut args: Value) -> Result<Value> {
        self.restore_if_needed().await?;
        let object = args
            .as_object_mut()
            .ok_or_else(|| Error::RustError("Bash arguments must be an object".into()))?;
        let id = digest_key(execution_id);
        object.insert("id".into(), Value::String(id.clone()));
        let boot = self
            .state
            .storage()
            .get::<String>("workspace:boot")
            .await?
            .ok_or_else(|| Error::RustError("workspace_boot_missing".into()))?;
        let dispatch_key = format!("workspace:dispatch:{id}");
        let previous_boot = self.state.storage().get::<String>(&dispatch_key).await?;
        if previous_boot
            .as_ref()
            .is_some_and(|previous| previous != &boot)
        {
            return Err(Error::RustError("tool_outcome_unknown: workspace restarted after dispatch; command was not replayed".into()));
        }
        let count = self
            .state
            .storage()
            .get::<u64>("workspace:tools")
            .await?
            .unwrap_or(0)
            .saturating_add(1);
        if previous_boot.is_none() {
            // This durable fence precedes the non-idempotent HTTP side effect.
            self.state.storage().put(&dispatch_key, &boot).await?;
            self.state.storage().put("workspace:tools", count).await?;
        }
        // A transport failure is not permission to execute a shell command twice.
        let response = if previous_boot.is_some() {
            let response = self.recover_execution(&id, &args).await?;
            if response.status_code() != 200 {
                return Err(Error::RustError("tool_outcome_unknown: dispatch was recorded but daemon result is unavailable; command was not replayed".into()));
            }
            response
        } else {
            match self.json_request(Method::Post, "/v1/exec", &args).await {
                Ok(response) if response.status_code() == 409 => {
                    self.recover_execution(&id, &args).await?
                }
                Ok(response) => response,
                Err(error) => {
                    let response = self.recover_execution(&id, &args).await?;
                    if response.status_code() != 200 {
                        return Err(Error::RustError(format!(
                            "tool_outcome_unknown: execution {id} must not be retried automatically: {error}"
                        )));
                    }
                    response
                }
            }
        };
        let mut response = ensure_success(response)?;
        let mut result: Value = response
            .json()
            .await
            .map_err(|_| Error::RustError("tool_outcome_unknown".into()))?;
        let mut status = ensure_success(self.request(Method::Get, "/v1/status", None, &[]).await?)?;
        let status: Value = status.json().await?;
        let dirty = status["dirty_bytes"]
            .as_u64()
            .ok_or_else(|| Error::RustError("workspace_dirty_measurement_missing".into()))?;
        if dirty == 0 && count >= BATCH_TOOLS {
            self.state.storage().put("workspace:tools", 0_u64).await?;
        }
        if dirty != 0 && (count >= BATCH_TOOLS || dirty >= BATCH_BYTES) {
            match self.checkpoint().await {
                Ok(stats) => {
                    result["checkpoint"] = serde_json::to_value(stats)?;
                }
                Err(error) => {
                    // Preserve a completed command's result. The pending counter
                    // forces another attempt; release still fails closed.
                    worker::console_error!("workspace checkpoint deferred");
                    result["checkpoint_pending"] = json!(true);
                    result["checkpoint_error"] = json!(error.to_string());
                }
            }
        }
        Ok(result)
    }

    async fn recover_execution(&self, id: &str, args: &Value) -> Result<TimedResponse> {
        let end = worker::Date::now().as_millis().saturating_add(
            args["timeout_ms"]
                .as_u64()
                .unwrap_or(120_000)
                .min(600_000)
                .saturating_add(10_000),
        );
        while worker::Date::now().as_millis() < end {
            let probe_until = worker::Date::now()
                .as_millis()
                .saturating_add(10_000)
                .min(end);
            let response = match self
                .request_until(
                    Method::Get,
                    &format!("/v1/exec/{id}"),
                    None,
                    &[],
                    probe_until,
                )
                .await
            {
                Ok(response) => response,
                Err(_) => return Err(Error::RustError("tool_outcome_unknown".into())),
            };
            if response.status_code() != 202 {
                return Ok(response);
            }
            Delay::from(Duration::from_millis(
                end.saturating_sub(worker::Date::now().as_millis()).min(500),
            ))
            .await;
        }
        Err(Error::RustError(format!(
            "Bash result uncertain; execution {id} is still running and must not be replayed"
        )))
    }

    pub async fn snapshot_metadata(&self) -> Result<Option<agents_core::WorkspaceSnapshot>> {
        Ok(self
            .state
            .storage()
            .get::<Manifest>("workspace:manifest")
            .await?
            .map(|manifest| agents_core::WorkspaceSnapshot {
                generation: manifest.generation,
                manifest_key: format!("{}/manifest.json", self.prefix),
                sha256: manifest.sha256,
                format_version: manifest.format_version.into(),
                created_at_ms: manifest.created_at_ms,
                logical_bytes: manifest.stats.logical_bytes,
                archive_bytes: manifest.stats.archive_bytes,
                entries: manifest.stats.entries,
            }))
    }

    pub async fn download_checkpoint(
        &self,
        expected: &agents_core::WorkspaceSnapshot,
    ) -> Result<worker::Response> {
        download::checkpoint(self, expected).await
    }

    pub async fn commit_version(&self, operation_id: &str, name: Option<&str>) -> Result<Value> {
        if operation_id.is_empty()
            || name.is_some_and(|value| {
                value.is_empty() || value.len() > 100 || value.chars().any(char::is_control)
            })
        {
            return Err(Error::RustError("invalid_workspace_version".into()));
        }
        if let Some(version) = crate::workspace_versions::find_operation(
            self.env,
            &self.owner,
            &self.session,
            operation_id,
        )
        .await?
        {
            if version.name != name.unwrap_or("") {
                return Err(Error::RustError("workspace_operation_conflict".into()));
            }
            return Ok(version.public());
        }
        if let Some(upload) = crate::workspace_versions::pending_upload(
            self.env,
            &self.owner,
            &self.session,
            operation_id,
        )
        .await?
        {
            let payload: crate::workspace_versions::UploadPayload =
                serde_json::from_str(&upload.version_json)?;
            if payload.version.name != name.unwrap_or("") {
                return Err(Error::RustError("workspace_operation_conflict".into()));
            }
            let version = self.finish_version_upload(&upload).await?;
            return Ok(version.public());
        }
        self.restore_if_needed().await?;
        let commit = VersionCommit {
            operation: operation_id.into(),
            name: name.unwrap_or("").into(),
        };
        self.capture_checkpoint(Some(&commit)).await?;
        crate::workspace_versions::find_operation(
            self.env,
            &self.owner,
            &self.session,
            operation_id,
        )
        .await?
        .map(|version| version.public())
        .ok_or_else(|| Error::RustError("workspace_version_publication_missing".into()))
    }

    async fn publish_version(&self, commit: &VersionCommit, manifest: &Manifest) -> Result<()> {
        let id = format!(
            "wsv_{}",
            digest_key(&serde_json::to_string(&(
                &self.owner,
                &self.session,
                &commit.operation
            ))?)
        );
        let mut version = crate::workspace_versions::Version {
            id,
            account_id: self.owner.clone(),
            session_id: self.session.clone(),
            operation_id: commit.operation.clone(),
            name: commit.name.clone(),
            object_key: String::new(),
            sha256: manifest.sha256.clone(),
            format_version: manifest.format_version,
            generation: manifest.generation,
            archive_bytes: manifest.stats.archive_bytes,
            logical_bytes: manifest.stats.logical_bytes,
            entries: manifest.stats.entries,
            created_at_ms: crate::util::now_ms(),
        };
        let archive = self.owned_archive(manifest).await?;
        version.object_key = archive.object_key.clone();
        crate::workspace_versions::publish_shared(self.env, &version, &archive).await?;
        if archive.object_key != manifest.object_key {
            self.promote_manifest(manifest, &archive).await?;
        }
        Ok(())
    }

    async fn finish_version_upload(
        &self,
        upload: &crate::workspace_versions::Upload,
    ) -> Result<crate::workspace_versions::Version> {
        let payload: crate::workspace_versions::UploadPayload =
            serde_json::from_str(&upload.version_json)?;
        let version = payload.version;
        let prefix = format!("space-versions/{}/", digest_key(&self.owner));
        if upload.status != "pending"
            || version.account_id != self.owner
            || version.session_id != self.session
            || version.object_key != upload.object_key
            || !version.object_key.starts_with(&prefix)
            || !payload
                .source_key
                .starts_with(&format!("{}/snapshots/", self.prefix))
        {
            return Err(Error::RustError("invalid_workspace_version_upload".into()));
        }
        crate::workspace_versions::activate_upload(self.env, &version.object_key).await?;
        let bucket = self.env.bucket("WORKSPACES")?;
        let probe = self.require_r2("class_b").await?;
        let existing = bucket.head(&version.object_key).await?;
        self.record_r2(&probe, "class_b").await?;
        let (size, uploaded_at) = if let Some(object) = existing {
            (object.size(), object.uploaded().as_millis())
        } else {
            let read = self.require_r2("class_b").await?;
            let source = bucket.get(&payload.source_key).execute().await?;
            self.record_r2(&read, "class_b").await?;
            let source = source
                .ok_or_else(|| Error::RustError("workspace_version_source_unavailable".into()))?;
            if source.size() != version.archive_bytes {
                return Err(Error::RustError("workspace_version_size_mismatch".into()));
            }
            let ResponseBody::Stream(body) = source
                .body()
                .ok_or_else(|| Error::RustError("workspace_version_source_unavailable".into()))?
                .response_body()?
            else {
                return Err(Error::RustError("workspace_version_not_streamable".into()));
            };
            let write = self.require_r2("class_a").await?;
            let copied = Deadline::new(crate::util::now_ms().saturating_add(120_000))?
                .wait(
                    bucket
                        .put(&version.object_key, body)
                        .sha256(decode_sha256(&version.sha256)?)
                        .execute(),
                )
                .await?
                .ok_or_else(|| Error::RustError("workspace_version_copy_rejected".into()))?;
            self.record_r2(&write, "class_a").await?;
            (copied.size(), copied.uploaded().as_millis())
        };
        if size != version.archive_bytes {
            return Err(Error::RustError("workspace_version_size_mismatch".into()));
        }
        crate::reconcile::register_storage_object(
            self.env,
            &self.owner,
            &self.session,
            &version.object_key,
            size,
            uploaded_at,
        )
        .await?;
        crate::workspace_versions::publish(self.env, &version).await
    }

    pub async fn restore_version(&self, operation_id: &str, version_id: &str) -> Result<Value> {
        if operation_id.is_empty() {
            return Err(Error::RustError("invalid_workspace_restore".into()));
        }
        let result_key = format!("workspace:restore-result:{}", digest_key(operation_id));
        if let Some(result) = self.state.storage().get::<Value>(&result_key).await? {
            return matching_restore_result(result, version_id);
        }
        let version = crate::workspace_versions::find(self.env, &self.owner, version_id)
            .await?
            .ok_or_else(|| Error::RustError("workspace_version_not_found".into()))?;
        self.restore_if_needed().await?;
        if let Some(result) = self.state.storage().get::<Value>(&result_key).await? {
            return matching_restore_result(result, version_id);
        }
        let safety = self
            .commit_version(
                &format!("restore:{operation_id}:before"),
                Some("Before workspace restore"),
            )
            .await?;
        let safety_version_id = safety["id"]
            .as_str()
            .ok_or_else(|| Error::RustError("workspace_restore_backup_missing".into()))?
            .to_owned();
        let archive = self
            .owned_archive(&Manifest {
                format_version: version.format_version,
                object_key: version.object_key.clone(),
                sha256: version.sha256.clone(),
                generation: version.generation,
                created_at_ms: version.created_at_ms,
                stats: CheckpointStats {
                    archive_bytes: version.archive_bytes,
                    logical_bytes: version.logical_bytes,
                    entries: version.entries,
                    r2_write_operations: 0,
                },
            })
            .await?;
        crate::workspace_blobs::bind_session(self.env, &self.owner, &self.session, &archive)
            .await?;
        let previous = self
            .state
            .storage()
            .get::<Manifest>("workspace:manifest")
            .await?;
        let generation = self
            .state
            .storage()
            .get::<u64>("workspace:generation")
            .await?
            .unwrap_or(0)
            .max(previous.as_ref().map_or(0, |value| value.generation))
            .checked_add(1)
            .ok_or_else(|| Error::RustError("workspace_generation_exhausted".into()))?;
        self.state
            .storage()
            .put("workspace:generation", generation)
            .await?;
        let manifest = Manifest {
            format_version: version.format_version,
            object_key: archive.object_key,
            sha256: version.sha256.clone(),
            generation,
            created_at_ms: crate::util::now_ms(),
            stats: CheckpointStats {
                archive_bytes: version.archive_bytes,
                logical_bytes: version.logical_bytes,
                entries: version.entries,
                r2_write_operations: 1,
            },
        };
        self.state
            .storage()
            .put(
                VERSION_RESTORE,
                VersionRestore {
                    operation: operation_id.into(),
                    version_id: version_id.into(),
                    safety_version_id,
                    manifest,
                },
            )
            .await?;
        self.resume_version_restore().await?;
        self.state
            .storage()
            .get::<Value>(&result_key)
            .await?
            .ok_or_else(|| Error::RustError("workspace_restore_result_missing".into()))
    }

    /// Reapplying a pending replacement precedes all later execution. A lost
    /// acknowledgement can repeat the same atomic restore, never later Bash.
    async fn resume_version_restore(&self) -> Result<()> {
        let Some(intent) = self
            .state
            .storage()
            .get::<VersionRestore>(VERSION_RESTORE)
            .await?
        else {
            return Ok(());
        };
        let result_key = format!("workspace:restore-result:{}", digest_key(&intent.operation));
        if self
            .state
            .storage()
            .get::<Value>(&result_key)
            .await?
            .is_some()
        {
            self.state.storage().delete(VERSION_RESTORE).await?;
            return Ok(());
        }
        if !self.valid_manifest_archive(&intent.manifest).await? {
            return Err(Error::RustError("invalid_workspace_restore_intent".into()));
        }
        let read = self.require_r2("class_b").await?;
        let object = self
            .env
            .bucket("WORKSPACES")?
            .get(&intent.manifest.object_key)
            .execute()
            .await?;
        self.record_r2(&read, "class_b").await?;
        let object =
            object.ok_or_else(|| Error::RustError("workspace_restore_snapshot_missing".into()))?;
        if object.size() != intent.manifest.stats.archive_bytes {
            return Err(Error::RustError("workspace_restore_size_mismatch".into()));
        }
        let ResponseBody::Stream(body) = object
            .body()
            .ok_or_else(|| Error::RustError("workspace_restore_snapshot_missing".into()))?
            .response_body()?
        else {
            return Err(Error::RustError("workspace_restore_not_streamable".into()));
        };
        ensure_success(
            self.request(
                Method::Post,
                "/v1/restore",
                Some(body.into()),
                &[
                    ("Content-Type", "application/zstd"),
                    ("X-Snapshot-Sha256", &intent.manifest.sha256),
                ],
            )
            .await?,
        )?;
        self.publish_manifest(&intent.manifest).await?;
        self.state.storage().put("workspace:tools", 0_u64).await?;
        let version = crate::workspace_versions::find(self.env, &self.owner, &intent.version_id)
            .await?
            .ok_or_else(|| Error::RustError("workspace_version_not_found".into()))?;
        self.state
            .storage()
            .put(
                &result_key,
                json!({"restored":version.public(),"safety_version_id":intent.safety_version_id}),
            )
            .await?;
        self.state.storage().delete(VERSION_RESTORE).await?;
        Ok(())
    }

    pub async fn checkpoint(&self) -> Result<CheckpointStats> {
        if self.startup_pending().await? || !self.container()?.running() {
            return Ok(CheckpointStats::default());
        }
        self.checkpoint_active().await
    }

    async fn checkpoint_active(&self) -> Result<CheckpointStats> {
        self.resume_version_restore().await?;
        self.capture_checkpoint(None).await
    }

    async fn capture_checkpoint(&self, version: Option<&VersionCommit>) -> Result<CheckpointStats> {
        self.arm_usage().await?;
        let mut status = ensure_success(self.request(Method::Get, "/v1/status", None, &[]).await?)?;
        let dirty = status.json::<Value>().await?["dirty_bytes"]
            .as_u64()
            .ok_or_else(|| Error::RustError("workspace_dirty_measurement_missing".into()))?;
        let previous = self
            .state
            .storage()
            .get::<Manifest>("workspace:manifest")
            .await?;
        if dirty == 0 && (version.is_none() || previous.is_some()) {
            if let (Some(commit), Some(manifest)) = (version, previous.as_ref()) {
                self.publish_version(commit, manifest).await?;
            }
            self.state.storage().put("workspace:tools", 0_u64).await?;
            return Ok(CheckpointStats::default());
        }
        let response =
            snapshot_response(self.request(Method::Get, "/v1/snapshot", None, &[]).await?).await?;
        let headers = response.headers();
        if headers.get("X-Snapshot-Format-Version")?.as_deref() != Some("1") {
            return Err(Error::RustError(
                "Unsupported workspace snapshot format".into(),
            ));
        }
        let sha256 = required_header(headers, "X-Snapshot-Sha256")?;
        let checksum = decode_sha256(&sha256)?;
        let mut stats = CheckpointStats {
            archive_bytes: numeric_header(headers, "X-Snapshot-Archive-Bytes")?,
            logical_bytes: numeric_header(headers, "X-Snapshot-Logical-Bytes")?,
            entries: numeric_header(headers, "X-Snapshot-Entries")?,
            r2_write_operations: 1,
        };
        let key = if let Some(ready) = crate::workspace_blobs::find_content(
            self.env,
            &self.owner,
            1,
            &sha256,
            stats.archive_bytes,
        )
        .await?
        {
            ready.object_key
        } else {
            let candidate = OwnedArchiveRef {
                account_id: self.owner.clone(),
                object_key: self.archive_key(&sha256, 1),
                sha256: sha256.clone(),
                format_version: 1,
                generation: 0,
                archive_bytes: stats.archive_bytes,
                logical_bytes: stats.logical_bytes,
                entries: stats.entries,
            };
            crate::workspace_blobs::begin(self.env, &candidate).await?;
            let (_, body, deadline) = response.into_parts();
            let ResponseBody::Stream(stream) = body else {
                return Err(Error::RustError(
                    "Workspace snapshot is not streamable".into(),
                ));
            };
            // R2 checks the complete compressed digest before any strong ref.
            let archive_write = self.require_r2("class_a").await?;
            let uploaded = deadline
                .wait(
                    self.env
                        .bucket("WORKSPACES")?
                        .put(&candidate.object_key, stream)
                        .sha256(checksum)
                        .execute(),
                )
                .await?
                .ok_or_else(|| Error::RustError("Workspace snapshot upload rejected".into()))?;
            self.record_r2(&archive_write, "class_a").await?;
            crate::reconcile::register_account_storage_object(
                self.env,
                &self.owner,
                &candidate.object_key,
                uploaded.size(),
                uploaded.uploaded().as_millis(),
            )
            .await?;
            if uploaded.size() != stats.archive_bytes {
                return Err(Error::RustError(
                    "Workspace snapshot upload size mismatch".into(),
                ));
            }
            stats.r2_write_operations += 1;
            crate::workspace_blobs::complete(self.env, &candidate)
                .await?
                .object_key
        };
        let generation = self
            .state
            .storage()
            .get::<u64>("workspace:generation")
            .await?
            .unwrap_or(0)
            .max(previous.as_ref().map_or(0, |manifest| manifest.generation))
            .checked_add(1)
            .ok_or_else(|| Error::RustError("workspace_generation_exhausted".into()))?;
        self.state
            .storage()
            .put("workspace:generation", generation)
            .await?;
        let manifest = Manifest {
            format_version: 1,
            object_key: key.clone(),
            sha256: sha256.clone(),
            stats: stats.clone(),
            generation,
            created_at_ms: worker::Date::now().as_millis(),
        };
        if let Some(commit) = version {
            self.publish_version(commit, &manifest).await?;
        }
        self.publish_manifest(&manifest).await?;
        ensure_success(
            self.json_request(
                Method::Post,
                "/v1/checkpoint-ack",
                &json!({"sha256":sha256}),
            )
            .await?,
        )?;
        self.state.storage().put("workspace:tools", 0_u64).await?;
        Ok(stats)
    }

    async fn publish_manifest(&self, manifest: &Manifest) -> Result<()> {
        let shared = if archives::shared_archive_key(&manifest.object_key) {
            let archive = self.owned_archive(manifest).await?;
            if archive.object_key != manifest.object_key {
                return Err(Error::RustError("workspace_blob_identity_conflict".into()));
            }
            crate::workspace_blobs::bind_session(self.env, &self.owner, &self.session, &archive)
                .await?;
            true
        } else {
            false
        };
        let bucket = self.env.bucket("WORKSPACES")?;
        let previous = self
            .state
            .storage()
            .get::<Manifest>("workspace:manifest")
            .await?;
        let manifest_write = self.require_r2("class_a").await?;
        let manifest_json = serde_json::to_string(manifest)?;
        let manifest_bytes = manifest_json.len() as u64;
        bucket
            .put(format!("{}/manifest.json", self.prefix), manifest_json)
            .execute()
            .await?
            .ok_or_else(|| Error::RustError("Workspace manifest publication rejected".into()))?;
        let published_at = worker::Date::now().as_millis();
        let manifest_identity = format!("{}/manifest.json@{}", self.prefix, manifest.generation);
        crate::reconcile::register_storage_object(
            self.env,
            &self.owner,
            &self.session,
            &manifest_identity,
            manifest_bytes,
            published_at,
        )
        .await?;
        if let Some(previous) = previous
            .as_ref()
            .filter(|old| old.generation != manifest.generation)
        {
            let old_manifest_identity =
                format!("{}/manifest.json@{}", self.prefix, previous.generation);
            crate::reconcile::remove_storage_object(self.env, &old_manifest_identity, published_at)
                .await?;
        }
        self.record_r2(&manifest_write, "class_a").await?;
        self.state
            .storage()
            .put("workspace:manifest", manifest)
            .await?;
        if shared {
            crate::workspace_blobs::reconcile_session(
                self.env,
                &self.owner,
                &self.session,
                &manifest.object_key,
            )
            .await?;
        }
        if let Some(previous) = previous.filter(|old| old.object_key != manifest.object_key) {
            // Owner blob collection atomically checks every session/history ref.
            if archives::shared_archive_key(&previous.object_key)
                || crate::workspace_blobs::referenced(self.env, &previous.object_key).await?
                || crate::workspace_versions::referenced(self.env, &previous.object_key).await?
            {
                return Ok(());
            }
            // Publication is the commit point. Failed cleanup never invalidates
            // the new checkpoint; an orphan sweep can safely retry it later.
            match bucket.delete(&previous.object_key).await {
                Ok(()) => {
                    crate::reconcile::remove_storage_object(
                        self.env,
                        &previous.object_key,
                        worker::Date::now().as_millis(),
                    )
                    .await?
                }
                Err(_) => crate::util::emit("checkpoint.gc_deferred", serde_json::json!({})),
            }
        }
        Ok(())
    }

    /// Data publication precedes a durable release intent. Once stopped, billing
    /// uncertainty must retain its hold without preventing physical completion.
    pub async fn release(&self) -> Result<ReleaseOutcome> {
        self.release_inner(true).await
    }

    /// Retain cleanup funds until the resource catalog has acknowledged release.
    pub async fn release_preserving_funding(&self) -> Result<ReleaseOutcome> {
        self.release_inner(false).await
    }

    async fn release_inner(&self, settle: bool) -> Result<ReleaseOutcome> {
        let storage = self.state.storage();
        let mut intent = match storage.get::<ReleaseIntent>(RELEASE_INTENT).await? {
            Some(intent) => intent,
            None => {
                let clock = storage
                    .get::<UsageClock>(CONTAINER_CLOCK)
                    .await?
                    .unwrap_or_default();
                let running = self.container()?.running();
                let startup_pending = self.startup_pending().await?;
                validate_checkpoint_source(running, &clock, startup_pending)?;
                let checkpoint = if running && !startup_pending {
                    let checkpoint = self.checkpoint_active().await?;
                    self.arm_usage().await?;
                    checkpoint
                } else {
                    CheckpointStats::default()
                };
                let intent = ReleaseIntent {
                    id: uuid::Uuid::new_v4().to_string(),
                    checkpoint,
                    clock: storage
                        .get::<UsageClock>(CONTAINER_CLOCK)
                        .await?
                        .unwrap_or_default(),
                    lease_id: self.lease().current().await?.map(|lease| lease.id),
                    destroyed_at_ms: None,
                    outcome: None,
                    startup_pending,
                };
                storage.put(RELEASE_INTENT, &intent).await?;
                intent
            }
        };
        if let Some(outcome) = intent.completed_outcome(self.container()?.running())? {
            self.detach_release(&intent).await?;
            return Ok(outcome);
        }
        let running = self.container()?.running();
        if running || intent.startup_pending {
            if storage
                .get::<Value>("workspace:python-publication-pending")
                .await?
                .is_some()
            {
                return Err(Error::RustError("python_publication_pending".into()));
            }
            match self.container()?.destroy(None).await {
                Ok(()) => {
                    if running {
                        intent.destroyed_at_ms = Some(worker::Date::now().as_millis());
                    }
                    storage.put(RELEASE_INTENT, &intent).await?;
                }
                Err(error) if self.container()?.running() || intent.startup_pending => {
                    return Err(error);
                }
                Err(_) => {} // Confirmed stopped, but no trustworthy exit time.
            }
        }
        if self.container()?.running() {
            return Err(Error::RustError(
                "workspace_release_stop_unconfirmed".into(),
            ));
        }
        if !settle {
            return Ok(ReleaseOutcome {
                checkpoint: intent.checkpoint.clone(),
                accounting_pending: true,
            });
        }
        let accounting_pending = self.release_accounting(&intent).await.is_err();
        let outcome = ReleaseOutcome {
            checkpoint: intent.checkpoint.clone(),
            accounting_pending,
        };
        intent.outcome = Some(outcome.clone());
        // Save the physical result before clearing pointers. Every subsequent
        // retry returns the same status and completes any interrupted detachment.
        storage.put(RELEASE_INTENT, &intent).await?;
        self.detach_release(&intent).await?;
        Ok(outcome)
    }

    async fn release_accounting(&self, intent: &ReleaseIntent) -> Result<()> {
        let Some(lease_id) = &intent.lease_id else {
            return if intent.clock.quantity == 0 {
                Ok(())
            } else {
                Err(Error::RustError("workspace_release_lease_missing".into()))
            };
        };
        let leases = self.lease();
        if intent.clock.quantity != 0 {
            let (Some(end), Some(event)) = (intent.destroyed_at_ms, &intent.clock.event) else {
                // Leave the original observation and reservation held.
                return Err(Error::RustError("workspace_release_clock_unknown".into()));
            };
            leases
                .meter(lease_id)
                .await?
                .record_wall_time(event, Resource::Container, intent.clock.since_ms, end)
                .await?;
        }
        leases.settle(lease_id).await
    }

    async fn detach_release(&self, intent: &ReleaseIntent) -> Result<()> {
        let storage = self.state.storage();
        if intent
            .outcome
            .as_ref()
            .is_some_and(|outcome| outcome.accounting_pending)
        {
            storage
                .put(
                    &format!("workspace:release-accounting:{}", intent.id),
                    intent,
                )
                .await?;
        }
        if let Some(clock) = storage.get::<UsageClock>(CONTAINER_CLOCK).await? {
            if clock.run == intent.clock.run
                && clock.event == intent.clock.event
                && clock.since_ms == intent.clock.since_ms
            {
                storage.put(CONTAINER_CLOCK, UsageClock::default()).await?;
            }
        }
        if let Some(lease) = self.lease().current().await? {
            if intent.lease_id.as_deref() == Some(lease.id.as_str()) {
                self.lease().clear().await?;
            }
        }
        Ok(())
    }

    pub async fn cancel(&self, execution_id: &str) -> Result<()> {
        let id = digest_key(execution_id);
        ensure_success(
            self.request(Method::Delete, &format!("/v1/exec/{id}"), None, &[])
                .await?,
        )?;
        Ok(())
    }
}

fn startup_unavailable() -> Error {
    Error::RustError("workspace_startup_unavailable".into())
}

fn inactivity_timeout_ms(expires_ms: u64, now_ms: u64) -> Result<u64> {
    let remaining = expires_ms.saturating_sub(now_ms);
    if remaining == 0 {
        return Err(Error::RustError("workspace_compute_lease_expired".into()));
    }
    Ok(remaining.min(INACTIVITY_FALLBACK_MS))
}

fn matching_restore_result(result: Value, version_id: &str) -> Result<Value> {
    if result["restored"]["id"].as_str() != Some(version_id) {
        return Err(Error::RustError("workspace_operation_conflict".into()));
    }
    Ok(result)
}

fn startup_probe_until(now_ms: u64, ready_until: u64) -> u64 {
    now_ms.saturating_add(STARTUP_PROBE_MS).min(ready_until)
}

fn startup_status_is_fatal(status: u16) -> bool {
    (300..500).contains(&status) && !matches!(status, 408 | 425 | 429)
}

fn validate_checkpoint_source(
    running: bool,
    clock: &UsageClock,
    startup_pending: bool,
) -> Result<()> {
    if !running && clock.quantity != 0 && !startup_pending {
        return Err(Error::RustError(
            "workspace_container_exit_before_checkpoint".into(),
        ));
    }
    Ok(())
}

fn ensure_success(response: TimedResponse) -> Result<TimedResponse> {
    if (200..300).contains(&response.status_code()) {
        return Ok(response);
    }
    Err(Error::RustError(format!(
        "Workspace daemon returned HTTP {}",
        response.status_code()
    )))
}

async fn snapshot_response(mut response: TimedResponse) -> Result<TimedResponse> {
    if response.status_code() != 422 {
        return ensure_success(response);
    }
    // Paths in this response belong to the authenticated session user. Expose
    // them in tool diagnostics, never global logs or telemetry.
    let value: Value = response.json().await?;
    let reason = value["error"]
        .as_str()
        .unwrap_or("Workspace contains an entry that cannot be persisted");
    Err(Error::RustError(format!(
        "workspace_snapshot_rejected: {reason}"
    )))
}

fn start_container(
    container: &Container,
    token: &str,
    profile: agents_core::RuntimeProfile,
    image_source: Option<&str>,
) -> Result<()> {
    // worker 0.8.7's ContainerStartupOptions -> Object conversion recursively
    // invokes itself. Use the same platform API with an explicit dictionary.
    let options = js_sys::Object::new();
    let env = js_sys::Object::new();
    js_sys::Reflect::set(&env, &"MAGENTS_DAEMON_TOKEN".into(), &token.into())?;
    if let Some(source) = image_source.filter(|source| {
        source.len() == 40
            && source
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        let application_class = match profile {
            agents_core::RuntimeProfile::Basic1g => "Session",
            agents_core::RuntimeProfile::Standard4g => "Session4G",
        };
        let allocation = json!({"memory_limit":PythonMemoryTier::from_core(profile),"application_class":application_class,"image_source":source});
        js_sys::Reflect::set(
            &env,
            &"MAGENTS_CF_ALLOCATION".into(),
            &allocation.to_string().into(),
        )?;
    }
    js_sys::Reflect::set(&options, &"env".into(), &env)?;
    js_sys::Reflect::set(&options, &"enableInternet".into(), &JsValue::TRUE)?;
    let start = js_sys::Reflect::get(container.as_ref(), &"start".into())?
        .dyn_into::<js_sys::Function>()?;
    start.call1(container.as_ref(), &options)?;
    Ok(())
}

fn required_header(headers: &Headers, name: &str) -> Result<String> {
    headers
        .get(name)?
        .ok_or_else(|| Error::RustError(format!("Workspace snapshot omitted {name}")))
}

fn numeric_header(headers: &Headers, name: &str) -> Result<u64> {
    required_header(headers, name)?
        .parse()
        .map_err(|_| Error::RustError(format!("Invalid workspace snapshot {name}")))
}

fn decode_sha256(value: &str) -> Result<Vec<u8>> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::RustError("Invalid workspace snapshot SHA256".into()));
    }
    (0..64)
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&value[i..i + 2], 16)
                .map_err(|_| Error::RustError("Invalid SHA256".into()))
        })
        .collect()
}

fn digest_key(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_operation_identity_cannot_select_a_different_version() {
        let result = json!({"restored":{"id":"wsv_a"},"safety_version_id":"wsv_backup"});
        assert!(matching_restore_result(result.clone(), "wsv_a").is_ok());
        assert!(matching_restore_result(result, "wsv_b").is_err());
    }

    #[test]
    fn native_inactivity_duration_is_positive_and_clamped_to_existing_funding() {
        assert_eq!(inactivity_timeout_ms(1_800_000, 0).unwrap(), 900_000);
        assert_eq!(inactivity_timeout_ms(120_000, 0).unwrap(), 120_000);
        assert_eq!(inactivity_timeout_ms(120_000, 119_999).unwrap(), 1);
        assert!(inactivity_timeout_ms(120_000, 120_000).is_err());
        assert!(inactivity_timeout_ms(120_000, 120_001).is_err());
    }

    #[test]
    fn cold_start_probes_share_one_bounded_two_minute_budget() {
        let start = 10_000;
        let end = start + STARTUP_BUDGET_MS;
        assert_eq!(STARTUP_BUDGET_MS, 120_000);
        assert_eq!(startup_probe_until(start, end), start + 5_000);
        assert_eq!(startup_probe_until(end - 1, end), end);
        assert_eq!(startup_probe_until(end, end), end);
    }

    #[test]
    fn unrecoverable_health_status_has_a_fixed_safe_failure() {
        for status in [301, 400, 401, 403, 404] {
            assert!(startup_status_is_fatal(status));
        }
        for status in [200, 408, 425, 429, 500, 503] {
            assert!(!startup_status_is_fatal(status));
        }
        assert!(
            matches!(startup_unavailable(), Error::RustError(code) if code == "workspace_startup_unavailable")
        );
    }

    #[test]
    fn workspace_keys_are_fixed_length_and_unambiguous() {
        assert_eq!(digest_key("../owner").len(), 64);
        assert_ne!(digest_key("a/b"), digest_key("a%2fb"));
        assert!(digest_key("用户").bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn snapshot_digest_requires_exactly_32_bytes() {
        assert_eq!(decode_sha256(&"ab".repeat(32)).unwrap(), vec![0xab; 32]);
        assert!(decode_sha256(&"ab".repeat(31)).is_err());
        assert!(decode_sha256(&"zz".repeat(32)).is_err());
        assert!(decode_sha256(&"é".repeat(32)).is_err());
    }

    #[test]
    fn snapshot_digest_roundtrips_every_byte_in_both_cases() {
        for byte in 0_u8..=255 {
            let lower = format!("{byte:02x}").repeat(32);
            assert_eq!(decode_sha256(&lower).unwrap(), vec![byte; 32]);
            assert_eq!(
                decode_sha256(&lower.to_uppercase()).unwrap(),
                vec![byte; 32]
            );
        }
    }

    #[test]
    fn completed_release_retries_return_the_saved_pending_status() {
        let mut intent = ReleaseIntent {
            id: "release-1".into(),
            checkpoint: CheckpointStats::default(),
            clock: UsageClock::default(),
            lease_id: None,
            destroyed_at_ms: None,
            outcome: None,
            startup_pending: false,
        };
        assert!(intent.completed_outcome(true).unwrap().is_none());
        intent.outcome = Some(ReleaseOutcome {
            checkpoint: CheckpointStats::default(),
            accounting_pending: true,
        });
        assert!(
            intent
                .completed_outcome(false)
                .unwrap()
                .unwrap()
                .accounting_pending
        );
        assert!(intent.completed_outcome(true).is_err());
    }

    #[test]
    fn a_stopped_active_clock_requires_a_prior_checkpoint_intent() {
        let active = UsageClock {
            quantity: 1,
            ..UsageClock::default()
        };
        assert!(validate_checkpoint_source(false, &active, false).is_err());
        assert!(validate_checkpoint_source(true, &active, false).is_ok());
        assert!(validate_checkpoint_source(false, &UsageClock::default(), false).is_ok());
        assert!(validate_checkpoint_source(false, &active, true).is_ok());
    }

    #[test]
    fn startup_release_preserves_unknown_time_and_legacy_journals_do_not_gain_startup_proof() {
        let intent = ReleaseIntent {
            id: "startup-release".into(),
            checkpoint: CheckpointStats::default(),
            clock: UsageClock {
                quantity: 1,
                since_ms: 42,
                ..UsageClock::default()
            },
            lease_id: Some("lease-1".into()),
            destroyed_at_ms: None,
            outcome: None,
            startup_pending: true,
        };
        let mut wire = serde_json::to_value(intent).unwrap();
        let restored: ReleaseIntent = serde_json::from_value(wire.clone()).unwrap();
        assert!(restored.startup_pending);
        assert!(restored.destroyed_at_ms.is_none());
        assert!(restored.clock.end_ms.is_none());
        wire.as_object_mut().unwrap().remove("startup_pending");
        let legacy: ReleaseIntent = serde_json::from_value(wire).unwrap();
        assert!(!legacy.startup_pending);
    }
    #[test]
    fn pending_release_journal_preserves_original_clock_without_inventing_an_end() {
        let intent = ReleaseIntent {
            id: "release-1".into(),
            checkpoint: CheckpointStats {
                archive_bytes: 123,
                ..CheckpointStats::default()
            },
            clock: UsageClock {
                quantity: 1,
                since_ms: 42,
                run: Some("lease-1".into()),
                event: Some("clock-1".into()),
                end_ms: None,
            },
            lease_id: Some("lease-1".into()),
            destroyed_at_ms: None,
            outcome: Some(ReleaseOutcome {
                checkpoint: CheckpointStats::default(),
                accounting_pending: true,
            }),
            startup_pending: false,
        };
        let restored: ReleaseIntent =
            serde_json::from_str(&serde_json::to_string(&intent).unwrap()).unwrap();
        assert_eq!(restored.clock.since_ms, 42);
        assert_eq!(restored.clock.event.as_deref(), Some("clock-1"));
        assert_eq!(restored.lease_id.as_deref(), Some("lease-1"));
        assert_eq!(restored.destroyed_at_ms, None);
        assert_eq!(restored.clock.end_ms, None);
        assert!(restored.outcome.unwrap().accounting_pending);
        assert_eq!(restored.checkpoint.archive_bytes, 123);
    }
}
