use std::time::Duration;

use agents_auth::billing::Resource;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue};
use worker::{
    Container, Delay, Env, Error, Headers, Method, RequestInit, Response, ResponseBody, Result,
    State,
};

use crate::{
    resources::ResourceMeter,
    telemetry::{NativeSpan, TraceContext},
};

#[path = "workspace_lease.rs"]
mod lease;

use lease::ComputeLease;

const PORT: u16 = 8080;
const BATCH_TOOLS: u64 = 8;
const BATCH_BYTES: u64 = 64 * 1024 * 1024;

/// A session DO is the sole writer. Its caller must serialize workspace operations,
/// including checkpoint/release, against tool execution.
pub struct Workspace<'a> {
    state: &'a State,
    env: &'a Env,
    prefix: String,
    owner: String,
    session: String,
    run: Option<String>,
    trace: Option<TraceContext>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckpointStats {
    pub archive_bytes: u64,
    pub logical_bytes: u64,
    pub entries: u64,
    pub r2_write_operations: u64,
}

#[derive(Serialize, Deserialize)]
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

#[derive(Default, Serialize, Deserialize)]
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
            state,
            env,
            prefix: format!("workspaces/{}/{}", digest_key(owner), digest_key(session)),
            owner: owner.into(),
            session: session.into(),
            run: None,
            trace: None,
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

    pub fn for_run(mut self, run: &str) -> Self {
        self.run = Some(run.into());
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

    async fn finish_clock(&self, key: &str, resource: Resource) -> Result<()> {
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
            .record_wall_time(event, resource, clock.since_ms, end)
            .await?;
        clock.since_ms = end;
        clock.run = None;
        clock.event = None;
        clock.end_ms = None;
        storage.put(key, &clock).await
    }

    async fn arm_clock(&self, key: &str, resource: Resource, unit: &str) -> Result<()> {
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
        self.finish_clock(key, resource.clone()).await?;
        clock = storage.get::<UsageClock>(key).await?.unwrap_or_default();
        let event = format!("workspace:{}:{}", unit, uuid::Uuid::new_v4());
        self.lease()
            .meter(run)
            .await?
            .require_observation(&event, resource, unit)
            .await?;
        clock.run = Some(run.into());
        clock.event = Some(event);
        storage.put(key, &clock).await
    }

    /// Arm a compute interval against the independent warm workspace lease.
    pub async fn arm_usage(&self) -> Result<()> {
        self.arm_clock("workspace:container-clock", Resource::Container, "wall_ms")
            .await
    }

    /// Renew only existing compute. Workspace-enabled time-only sessions never
    /// create a reservation or container through this hook.
    pub async fn ensure_compute_lease(&self) -> Result<()> {
        if !self.container()?.running() {
            return Ok(());
        }
        self.renew_compute_lease().await
    }

    async fn renew_compute_lease(&self) -> Result<()> {
        let leases = self.lease();
        let Some(renewal) = leases.prepare().await? else {
            return Ok(());
        };
        self.finish_clock("workspace:container-clock", Resource::Container)
            .await?;
        if let Some(previous) = &renewal.previous {
            leases.settle(&previous.id).await?;
        }
        leases.activate(&renewal.next).await?;
        self.arm_usage().await
    }

    /// Close an observed interval without settling the independent warm lease.
    /// Normally release and lease renewal perform this automatically.
    pub async fn meter_usage(&self) -> Result<()> {
        self.finish_clock("workspace:container-clock", Resource::Container)
            .await
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
    ) -> Result<Response> {
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
        self.container()?
            .get_tcp_port(PORT)?
            .fetch(format!("http://workspace{path}"), Some(init))
            .await
    }

    async fn json_request(&self, method: Method, path: &str, value: &Value) -> Result<Response> {
        self.request(
            method,
            path,
            Some(JsValue::from_str(&value.to_string())),
            &[("Content-Type", "application/json")],
        )
        .await
    }

    /// Reconcile a process incarnation, not just DO memory: a live container may
    /// outlast an evicted DO, and must never be overwritten by an older snapshot.
    pub async fn restore_if_needed(&self) -> Result<()> {
        let container = self.container()?;
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
            start_container(&container, &self.token().await?)?;
        }
        self.ensure_compute_lease().await?;
        self.arm_usage().await?;
        let mut health = None;
        for _ in 0..60 {
            if let Ok(mut response) = self.request(Method::Get, "/health", None, &[]).await {
                if response.status_code() == 200 {
                    health = Some(response.json::<Value>().await?);
                    break;
                }
            }
            Delay::from(Duration::from_millis(500)).await;
        }
        let health = health.ok_or_else(|| {
            Error::RustError("Workspace did not become ready in 30 seconds".into())
        })?;
        let boot = health["boot_id"]
            .as_str()
            .ok_or_else(|| Error::RustError("Workspace health omitted boot_id".into()))?;
        let storage = self.state.storage();
        if storage.get::<String>("workspace:boot").await?.as_deref() == Some(boot) {
            return Ok(());
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
            if manifest.format_version != 1
                || !manifest
                    .object_key
                    .starts_with(&format!("{}/snapshots/", self.prefix))
            {
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
        Ok(())
    }

    pub async fn execute_bash(&self, args: Value) -> Result<Value> {
        self.execute_bash_with_id(&uuid::Uuid::new_v4().to_string(), args)
            .await
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
                            "Bash result uncertain; execution {id} must not be retried automatically: {error}"
                        )));
                    }
                    response
                }
            }
        };
        let mut response = ensure_success(response)?;
        let mut result: Value = response.json().await?;
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

    async fn recover_execution(&self, id: &str, args: &Value) -> Result<Response> {
        let polls = args["timeout_ms"].as_u64().unwrap_or(120_000).min(600_000) / 500 + 20;
        for _ in 0..polls {
            let response = self
                .request(Method::Get, &format!("/v1/exec/{id}"), None, &[])
                .await?;
            if response.status_code() != 202 {
                return Ok(response);
            }
            Delay::from(Duration::from_millis(500)).await;
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

    pub async fn checkpoint(&self) -> Result<CheckpointStats> {
        if !self.container()?.running() {
            return Ok(CheckpointStats::default());
        }
        self.arm_usage().await?;
        let mut status = ensure_success(self.request(Method::Get, "/v1/status", None, &[]).await?)?;
        let dirty = status.json::<Value>().await?["dirty_bytes"]
            .as_u64()
            .ok_or_else(|| Error::RustError("workspace_dirty_measurement_missing".into()))?;
        if dirty == 0 {
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
        let stats = CheckpointStats {
            archive_bytes: numeric_header(headers, "X-Snapshot-Archive-Bytes")?,
            logical_bytes: numeric_header(headers, "X-Snapshot-Logical-Bytes")?,
            entries: numeric_header(headers, "X-Snapshot-Entries")?,
            r2_write_operations: 2,
        };
        let key = format!(
            "{}/snapshots/{sha256}/{}.tar.zst",
            self.prefix,
            uuid::Uuid::new_v4()
        );
        let bucket = self.env.bucket("WORKSPACES")?;
        let (_, body) = response.into_parts();
        let ResponseBody::Stream(stream) = body else {
            return Err(Error::RustError(
                "Workspace snapshot is not streamable".into(),
            ));
        };
        // Native streams bypass WASM memory. R2 verifies the compressed digest
        // before the manifest becomes visible, retaining the last good snapshot.
        let archive_write = self.require_r2("class_a").await?;
        crate::checkpoint_gc::record_upload_intent(self.env, &self.owner, &self.session, &key)
            .await?;
        let uploaded = bucket
            .put(&key, stream)
            .sha256(checksum)
            .execute()
            .await?
            .ok_or_else(|| Error::RustError("Workspace snapshot upload rejected".into()))?;
        crate::reconcile::register_storage_object(
            self.env,
            &self.owner,
            &self.session,
            &key,
            uploaded.size(),
            worker::Date::now().as_millis(),
        )
        .await?;
        self.record_r2(&archive_write, "class_a").await?;
        if uploaded.size() != stats.archive_bytes {
            return Err(Error::RustError(
                "Workspace snapshot upload size mismatch".into(),
            ));
        }
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
        let manifest_write = self.require_r2("class_a").await?;
        let manifest_json = serde_json::to_string(&manifest)?;
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
        if let Some(previous) = &previous {
            let old_manifest_identity =
                format!("{}/manifest.json@{}", self.prefix, previous.generation);
            crate::reconcile::remove_storage_object(self.env, &old_manifest_identity, published_at)
                .await?;
        }
        self.record_r2(&manifest_write, "class_a").await?;
        self.state
            .storage()
            .put("workspace:manifest", &manifest)
            .await?;
        if let Some(previous) = previous.filter(|old| old.object_key != key) {
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

    /// Never release a dirty workspace when publication fails. DO alarm retry
    /// policy belongs to the session owner, which must keep this call serialized.
    pub async fn release(&self) -> Result<CheckpointStats> {
        let stats = self.checkpoint().await?;
        if self.container()?.running() {
            self.arm_usage().await?;
            self.container()?.destroy(None).await?;
            self.finish_clock("workspace:container-clock", Resource::Container)
                .await?;
            self.state
                .storage()
                .put("workspace:container-clock", UsageClock::default())
                .await?;
        } else if self
            .state
            .storage()
            .get::<UsageClock>("workspace:container-clock")
            .await?
            .is_some_and(|clock| clock.quantity != 0)
        {
            return Err(Error::RustError(
                "workspace_container_exit_unreconciled".into(),
            ));
        }
        if let Some(lease) = self.lease().current().await? {
            self.lease().settle(&lease.id).await?;
            self.lease().clear().await?;
        }
        Ok(stats)
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

fn ensure_success(response: Response) -> Result<Response> {
    if (200..300).contains(&response.status_code()) {
        return Ok(response);
    }
    Err(Error::RustError(format!(
        "Workspace daemon returned HTTP {}",
        response.status_code()
    )))
}

async fn snapshot_response(mut response: Response) -> Result<Response> {
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

fn start_container(container: &Container, token: &str) -> Result<()> {
    // worker 0.8.7's ContainerStartupOptions -> Object conversion recursively
    // invokes itself. Use the same platform API with an explicit dictionary.
    let options = js_sys::Object::new();
    let env = js_sys::Object::new();
    js_sys::Reflect::set(&env, &"MAGENTS_DAEMON_TOKEN".into(), &token.into())?;
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
}
