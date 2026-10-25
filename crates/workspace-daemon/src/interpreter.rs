//! Managed Jupyter execution. The control plane publishes artifacts into Space.

use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, ensure};
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};

use crate::{
    Active, App, error, internal,
    interpreter_files::{self as files, Artifact, StoredArtifact},
    interpreter_process as sandbox,
};

// Four MiB of retained logs can require six JSON bytes per control byte.
const MAX_REPLY_BYTES: usize = 32 * 1024 * 1024;
const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
struct StartupFailure(String);

impl std::fmt::Display for StartupFailure {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(&self.0)
    }
}
impl std::error::Error for StartupFailure {}

fn startup_failure(value: &Value) -> Option<StartupFailure> {
    if value["type"] != "startup_error" {
        return None;
    }
    let stage = value["stage"]
        .as_str()
        .filter(|stage| {
            matches!(
                *stage,
                "import" | "construct" | "launch" | "channels" | "ready"
            )
        })
        .unwrap_or("unknown");
    let kind = value["kind"]
        .as_str()
        .filter(|kind| {
            matches!(
                *kind,
                "permission_denied"
                    | "dependency_unavailable"
                    | "os_error"
                    | "runtime_error"
                    | "initialization_error"
            )
        })
        .unwrap_or("unknown");
    let mut code = format!("code_kernel_{stage}_{kind}");
    if let Some(outcome) = value["outcome"]
        .as_str()
        .filter(|outcome| matches!(*outcome, "kernel_died" | "readiness_timeout"))
    {
        code.push('_');
        code.push_str(outcome);
    }
    if let Some(status) = value["exit_status"]
        .as_i64()
        .filter(|status| (-128..=255).contains(status))
    {
        code.push_str(&format!("_exit_{status}"));
    }
    if let Some(site) = value["site"].as_str().filter(|site| {
        matches!(
            *site,
            "libzmq_tcp"
                | "libzmq_ip"
                | "libzmq_signaler"
                | "libzmq_context"
                | "libzmq_ipc"
                | "kernel_iostream"
                | "kernel_debugger"
                | "kernel_application"
        )
    }) {
        code.push('_');
        code.push_str(site);
    }
    if let Some(errno) = value["errno_class"]
        .as_str()
        .filter(|errno| matches!(*errno, "permission_denied" | "address_family_unsupported"))
    {
        code.push('_');
        code.push_str(errno);
    }
    Some(StartupFailure(code))
}

#[derive(Debug)]
enum InputFailure {
    Integrity,
    Conflict,
    Limit,
    Expired,
}

impl std::fmt::Display for InputFailure {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(match self {
            Self::Integrity => "code input integrity mismatch",
            Self::Conflict => "code input conflict",
            Self::Limit => "code input limit exceeded",
            Self::Expired => "code container not found",
        })
    }
}

impl std::error::Error for InputFailure {}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CodeRequest {
    pub id: String,
    #[serde(default)]
    pub container_id: Option<String>,
    #[serde(default)]
    pub network_policy: Option<crate::interpreter_egress::NetworkPolicy>,
    #[serde(default)]
    pub memory_limit: Option<crate::interpreter_limits::MemoryLimit>,
    pub code: String,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "output_limit")]
    pub max_output_bytes: usize,
}

fn timeout() -> u64 {
    120_000
}
fn output_limit() -> usize {
    1024 * 1024
}

fn kernel_request(request: &CodeRequest) -> serde_json::Result<Vec<u8>> {
    // This is a credential boundary, not the private supervisor request. Keep
    // the bridge payload an explicit allowlist when control-plane fields grow.
    #[derive(Serialize)]
    struct KernelRequest<'a> {
        id: &'a str,
        code: &'a str,
        timeout_ms: u64,
        max_output_bytes: usize,
    }
    serde_json::to_vec(&KernelRequest {
        id: &request.id,
        code: &request.code,
        timeout_ms: request.timeout_ms,
        max_output_bytes: request.max_output_bytes,
    })
}

impl CodeRequest {
    fn validate(&self) -> Result<(), &'static str> {
        if self
            .network_policy
            .as_ref()
            .is_some_and(|policy| policy.validate().is_err())
        {
            return Err("invalid code network policy");
        }
        if !opaque_id(&self.id) {
            return Err("invalid code execution id");
        }
        if self
            .container_id
            .as_deref()
            .is_some_and(|id| !context_id(id))
        {
            return Err("invalid code container id");
        }
        if self.code.is_empty() || self.code.len() > 64 * 1024 || self.code.contains('\0') {
            return Err("code must contain 1 to 65536 bytes without NUL");
        }
        if !(1..=300_000).contains(&self.timeout_ms) {
            return Err("code timeout must be between 1 and 300000 ms");
        }
        if !(1..=4 * 1024 * 1024).contains(&self.max_output_bytes) {
            return Err("code output limit must be between 1 and 4194304 bytes");
        }
        Ok(())
    }
}

fn opaque_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn context_id(id: &str) -> bool {
    id.strip_prefix("cntr_")
        .is_some_and(|tail| tail.len() == 32 && tail.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CodeResult {
    pub id: String,
    pub container_id: Option<String>,
    pub outputs: Vec<Value>,
    pub artifacts: Vec<Artifact>,
    pub artifact_error: Option<String>,
    pub error: Option<String>,
    pub python_error: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub truncated: bool,
    pub duration_ms: u64,
}

impl CodeResult {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            container_id: None,
            outputs: vec![],
            artifacts: vec![],
            artifact_error: None,
            error: None,
            python_error: false,
            timed_out: false,
            cancelled: false,
            truncated: false,
            duration_ms: 0,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KernelReply {
    id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    outputs: Vec<Value>,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    artifact_error: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

struct Bridge {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Bridge {
    async fn read(&mut self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut limited = (&mut self.output).take((MAX_REPLY_BYTES + 1) as u64);
        limited.read_until(b'\n', &mut bytes).await?;
        ensure!(
            bytes.len() <= MAX_REPLY_BYTES && bytes.last() == Some(&b'\n'),
            "invalid code kernel reply"
        );
        Ok(bytes)
    }
}

struct ContextState {
    id: String,
    expired: bool,
    data: PathBuf,
    inputs: PathBuf,
    runtime: PathBuf,
    bridge: Option<Bridge>,
    inputs_index: BTreeMap<String, (u64, String)>,
    network_policy: Option<crate::interpreter_egress::NetworkPolicy>,
    gateway: Option<crate::interpreter_egress::Gateway>,
}

pub(super) struct Interpreter {
    root: PathBuf,
    spool: PathBuf,
    context: Mutex<Option<ContextState>>,
    active: Mutex<Option<String>>,
    seen: Mutex<VecDeque<String>>,
    completed: Mutex<VecDeque<CodeResult>>,
    artifacts: std::sync::Mutex<BTreeMap<String, StoredArtifact>>,
}

impl Interpreter {
    pub(super) fn new(root: &FsPath) -> anyhow::Result<Self> {
        let root = root.join("code");
        let spool = root.join("artifacts");
        protected_directory(&root, false)?;
        protected_directory(&spool, true)?;
        Ok(Self {
            root,
            spool,
            context: Mutex::new(None),
            active: Mutex::new(None),
            seen: Mutex::new(VecDeque::new()),
            completed: Mutex::new(VecDeque::new()),
            artifacts: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    async fn accept(&self, id: &str) -> bool {
        let mut seen = self.seen.lock().await;
        if seen.iter().any(|entry| entry == id) {
            return false;
        }
        seen.push_back(id.into());
        if seen.len() > 128 {
            seen.pop_front();
        }
        *self.active.lock().await = Some(id.into());
        true
    }

    async fn finish(&self, result: CodeResult) {
        let mut completed = self.completed.lock().await;
        completed.push_back(result);
        if completed.len() > 128 {
            completed.pop_front();
        }
        *self.active.lock().await = None;
    }

    async fn has_context(&self, id: &str) -> bool {
        let mut slot = self.context.lock().await;
        let Some(context) = slot
            .as_mut()
            .filter(|context| !context.expired && context.id == id)
        else {
            return false;
        };
        context
            .bridge
            .as_mut()
            .is_none_or(|bridge| matches!(bridge.child.try_wait(), Ok(None)))
    }

    fn create_context(&self) -> anyhow::Result<ContextState> {
        let id = format!("cntr_{}", uuid::Uuid::new_v4().simple());
        let root = self.root.join(&id);
        protected_directory(&root, false)?;
        let data = root.join("data");
        let runtime = root.join("runtime");
        let inputs = root.join("inputs");
        sandbox::directory(&data, true)?;
        sandbox::directory(&runtime, true)?;
        protected_directory(&inputs, false)?;
        Ok(ContextState {
            id,
            expired: false,
            data,
            inputs,
            runtime,
            bridge: None,
            inputs_index: BTreeMap::new(),
            network_policy: None,
            gateway: None,
        })
    }

    fn configure_network(
        &self,
        context: &mut ContextState,
        requested: Option<&crate::interpreter_egress::NetworkPolicy>,
    ) -> anyhow::Result<()> {
        use crate::interpreter_egress::{self as egress, NetworkPolicy};
        if let Some(current) = &context.network_policy {
            ensure!(
                requested.is_none_or(|policy| policy == current),
                "code network policy conflict"
            );
            return Ok(());
        }
        let policy = requested.cloned().unwrap_or_default();
        if matches!(policy, NetworkPolicy::Allowlist { .. }) {
            #[cfg(target_os = "linux")]
            {
                context.gateway = Some(egress::start(
                    &context.data.parent().unwrap().join("egress.sock"),
                    policy.clone(),
                )?);
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("network policy unsupported");
            }
        }
        context.network_policy = Some(policy);
        Ok(())
    }

    async fn retire(&self, slot: &mut Option<ContextState>) -> anyhow::Result<()> {
        let Some(context) = slot.as_mut() else {
            return Ok(());
        };
        context.expired = true;
        // Expiry revokes root egress authority even if later process or scratch
        // cleanup fails and this unusable context must remain as a fence.
        drop(context.gateway.take());
        // Retain the expired slot if the kill barrier or cleanup fails. No new
        // namespace may share this UID until every previous writer is gone.
        sandbox::stop()?;
        if let Some(mut bridge) = context.bridge.take() {
            bridge.child.wait().await?;
        }
        remove_context(&self.root, &context.id)?;
        *slot = None;
        Ok(())
    }

    async fn ensure_bridge(&self, context: &mut ContextState) -> anyhow::Result<()> {
        if let Some(bridge) = &mut context.bridge {
            ensure!(bridge.child.try_wait()?.is_none(), "code container expired");
            return Ok(());
        }
        crate::process::require_supported_platform()?;
        sandbox::stop()?;
        alias("/mnt/data", &context.data)?;
        alias("/mnt/inputs", &context.inputs)?;
        let mut command = Command::new("/usr/bin/python3");
        command
            .args([
                "-I",
                "/usr/local/lib/magents/python_kernel.py",
                "--directory",
            ])
            .arg(&context.data)
            .arg("--runtime")
            .arg(&context.runtime)
            .arg("--container-id")
            .arg(&context.id)
            .current_dir(&context.data)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", &context.runtime)
            .env("LANG", "C.UTF-8")
            .env("OMP_NUM_THREADS", "2")
            .env("OPENBLAS_NUM_THREADS", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if context.gateway.is_some() {
            command.env(
                "MAGENTS_EGRESS_SOCKET",
                context.data.parent().unwrap().join("egress.sock"),
            );
        }
        sandbox::configure(&mut command)
            .map_err(|_| StartupFailure("code_kernel_process_configuration_failed".into()))?;
        let mut child = command.spawn().map_err(|error| {
            StartupFailure(
                if error.kind() == std::io::ErrorKind::PermissionDenied {
                    "code_kernel_process_permission_denied"
                } else {
                    "code_kernel_process_os_error"
                }
                .into(),
            )
        })?;
        let input = child.stdin.take().context("code client stdin")?;
        let output = BufReader::new(child.stdout.take().context("code client stdout")?);
        let mut bridge = Bridge {
            child,
            input,
            output,
        };
        let ready: Value = serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(30), bridge.read())
                .await?
                .map_err(|_| StartupFailure("code_kernel_handshake_process_exit".into()))?,
        )?;
        if let Some(error) = startup_failure(&ready) {
            return Err(error.into());
        }
        ensure!(
            ready["type"] == "ready" && ready["container_id"] == context.id,
            "invalid code kernel readiness"
        );
        context.bridge = Some(bridge);
        Ok(())
    }

    async fn execute(&self, request: CodeRequest, cancel: CancellationToken) -> CodeResult {
        let started = Instant::now();
        let mut result = CodeResult::new(&request.id);
        let mut slot = self.context.lock().await;
        if slot.as_ref().is_some_and(|context| context.expired)
            && self.retire(&mut slot).await.is_err()
        {
            result.error = Some("code_container_unavailable".into());
            return result;
        }
        if slot.is_none() {
            if request.container_id.is_some() {
                result.error = Some("code_container_not_found".into());
                return result;
            }
            match self.create_context() {
                Ok(context) => *slot = Some(context),
                Err(_) => {
                    result.error = Some("code_container_unavailable".into());
                    return result;
                }
            }
        }
        let context = slot.as_mut().unwrap();
        result.container_id = Some(context.id.clone());
        if request
            .container_id
            .as_ref()
            .is_some_and(|id| id != &context.id)
        {
            result.error = Some("code_container_not_found".into());
            return result;
        }
        if self
            .configure_network(context, request.network_policy.as_ref())
            .is_err()
        {
            result.error = Some("code_network_policy_conflict".into());
            return result;
        }
        let outcome = self.cell(context, &request, cancel.clone()).await;
        match outcome {
            Ok(reply) => {
                result.python_error = reply.status.as_deref() == Some("error");
                result.truncated = reply.truncated;
                result.artifact_error = reply.artifact_error.map(|error| {
                    if error == "display_artifact_exceeds_limit" {
                        error
                    } else {
                        "invalid_display_artifact".into()
                    }
                });
                if let Some(error) = reply.error {
                    result.timed_out = error == "kernel_timeout";
                    result.error = Some(
                        if result.timed_out {
                            "code_timeout"
                        } else {
                            "code_outcome_unknown"
                        }
                        .into(),
                    );
                } else {
                    let data = context.data.clone();
                    let spool = self.spool.clone();
                    let id = context.id.clone();
                    let captured =
                        tokio::task::spawn_blocking(move || files::capture(&data, &spool, &id))
                            .await;
                    match captured {
                        Ok(captured) => {
                            result.artifact_error =
                                captured.error.map(str::to_owned).or(result.artifact_error);
                            let (outputs, truncated) = resolve_outputs(
                                reply.outputs,
                                &captured.files,
                                request.max_output_bytes,
                            );
                            result.outputs = outputs;
                            result.truncated |= truncated;
                            result.artifacts = captured
                                .files
                                .iter()
                                .map(|file| file.metadata.clone())
                                .collect();
                            let mut files = self.artifacts.lock().unwrap();
                            for file in captured.files {
                                files.insert(file.metadata.id.clone(), file);
                            }
                        }
                        Err(_) => result.error = Some("code_artifact_capture_failed".into()),
                    }
                }
            }
            Err(error) => {
                result.cancelled = cancel.is_cancelled();
                result.timed_out = !result.cancelled
                    && started.elapsed() >= Duration::from_millis(request.timeout_ms);
                result.error = if let Some(startup) = error.downcast_ref::<StartupFailure>() {
                    Some(startup.0.clone())
                } else {
                    Some(
                        if result.cancelled {
                            "code_cancelled"
                        } else if result.timed_out {
                            "code_timeout"
                        } else {
                            "code_outcome_unknown"
                        }
                        .into(),
                    )
                };
            }
        }
        if result.error.is_some() {
            let _ = self.retire(&mut slot).await;
        }
        result.duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        result
    }

    async fn cell(
        &self,
        context: &mut ContextState,
        request: &CodeRequest,
        cancel: CancellationToken,
    ) -> anyhow::Result<KernelReply> {
        let operation = async {
            self.ensure_bridge(context).await?;
            sandbox::resume()?;
            let bridge = context.bridge.as_mut().unwrap();
            let mut bytes = kernel_request(request)?;
            bytes.push(b'\n');
            bridge.input.write_all(&bytes).await?;
            bridge.input.flush().await?;
            let reply: KernelReply = serde_json::from_slice(&bridge.read().await?)?;
            ensure!(
                reply.id == request.id && reply.outputs.len() <= 128,
                "invalid code reply identity"
            );
            ensure!(
                reply.error.is_some() || matches!(reply.status.as_deref(), Some("ok" | "error")),
                "invalid code result"
            );
            sandbox::pause()?;
            Ok(reply)
        };
        tokio::select! {
            result = operation => result,
            _ = tokio::time::sleep(Duration::from_millis(request.timeout_ms)) => anyhow::bail!("code timeout"),
            _ = cancel.cancelled() => anyhow::bail!("code cancelled"),
        }
    }
}

fn remove_context(root: &FsPath, id: &str) -> anyhow::Result<()> {
    ensure!(context_id(id), "invalid retired code context");
    let path = root.join(id);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "invalid code context directory"
    );
    // The parent is root-owned and cannot be renamed by either execution UID.
    // remove_dir_all does not follow symlinks created inside untrusted scratch.
    std::fs::remove_dir_all(path).context("retire code context")
}

fn resolve_outputs(
    outputs: Vec<Value>,
    artifacts: &[StoredArtifact],
    mut budget: usize,
) -> (Vec<Value>, bool) {
    let mut result = Vec::new();
    let mut truncated = false;
    for output in outputs {
        if output["type"] == "logs" {
            let Some(logs) = output["logs"].as_str() else {
                truncated = true;
                continue;
            };
            let mut end = budget.min(logs.len());
            while !logs.is_char_boundary(end) {
                end -= 1;
            }
            truncated |= end != logs.len();
            budget -= end;
            if end > 0 {
                result.push(serde_json::json!({"type":"logs","logs":&logs[..end]}));
            }
            continue;
        }
        let Some(kind @ ("image" | "file")) = output["type"].as_str() else {
            truncated = true;
            continue;
        };
        let file = output["path"]
            .as_str()
            .and_then(|path| artifacts.iter().find(|file| file.metadata.path == path));
        if let Some(file) = file {
            result.push(serde_json::json!({"type":kind,"artifact_id":file.metadata.id,"mime_type":file.metadata.mime_type}));
        } else {
            truncated = true;
        }
    }
    (result, truncated)
}

fn protected_directory(path: &FsPath, private: bool) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if private { 0o700 } else { 0o711 }),
        )?;
    }
    let _ = private;
    Ok(())
}

fn alias(path: &str, directory: &FsPath) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = FsPath::new(path);
        let parent = path.parent().context("code alias parent")?;
        std::fs::create_dir_all(parent)?;
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            ensure!(
                metadata.file_type().is_symlink(),
                "code alias already exists"
            );
        }
        let temporary = parent.join(format!(".magents-code-{}", uuid::Uuid::new_v4()));
        std::os::unix::fs::symlink(directory, &temporary)?;
        std::fs::rename(temporary, path)?;
    }
    let _ = (path, directory);
    Ok(())
}

pub(super) async fn execute(
    State(state): State<Arc<App>>,
    Json(request): Json<CodeRequest>,
) -> Response {
    if let Err(message) = request.validate() {
        return error(StatusCode::BAD_REQUEST, message);
    }
    if let Err(message) = crate::interpreter_limits::verify(request.memory_limit) {
        return error(StatusCode::UNPROCESSABLE_ENTITY, message);
    }
    let Ok(guard) = state.gate.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    if let Some(id) = &request.container_id
        && !state.interpreter.has_context(id).await
    {
        return error(StatusCode::NOT_FOUND, "code container not found");
    }
    if !state.interpreter.accept(&request.id).await {
        return error(StatusCode::CONFLICT, "execution id already exists");
    }
    let cancel = CancellationToken::new();
    *state.active.lock().await = Some(Active {
        id: request.id.clone(),
        cancel: cancel.clone(),
    });
    let task = tokio::spawn(async move {
        let _guard = guard;
        let result = state.interpreter.execute(request, cancel).await;
        state.interpreter.finish(result.clone()).await;
        *state.active.lock().await = None;
        result
    });
    match task.await {
        Ok(result) => Json(result).into_response(),
        Err(error) => internal(error),
    }
}

pub(super) async fn status(State(state): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if let Some(result) = state
        .interpreter
        .completed
        .lock()
        .await
        .iter()
        .find(|entry| entry.id == id)
        .cloned()
    {
        return Json(result).into_response();
    }
    if state.interpreter.active.lock().await.as_deref() == Some(id.as_str()) {
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"id":id,"status":"running"})),
        )
            .into_response();
    }
    error(StatusCode::NOT_FOUND, "code execution not found")
}

pub(super) async fn cancel(State(state): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if state.interpreter.active.lock().await.as_deref() != Some(id.as_str()) {
        return error(StatusCode::NOT_FOUND, "code execution not running");
    }
    let active = state.active.lock().await;
    if let Some(active) = active.as_ref().filter(|entry| entry.id == id) {
        active.cancel.cancel();
        return StatusCode::NO_CONTENT.into_response();
    }
    error(StatusCode::NOT_FOUND, "code execution not running")
}

pub(super) async fn artifact(State(state): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let file = state
        .interpreter
        .artifacts
        .lock()
        .unwrap()
        .get(&id)
        .cloned();
    let Some(file) = file else {
        return error(StatusCode::NOT_FOUND, "code artifact not found");
    };
    let source = match files::open(&file) {
        Ok(file) => tokio::fs::File::from_std(file),
        Err(error) => return internal(error),
    };
    let mut response = Body::from_stream(ReaderStream::new(source)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        file.metadata.mime_type.parse().unwrap(),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, file.metadata.bytes.into());
    response
        .headers_mut()
        .insert("x-content-sha256", file.metadata.sha256.parse().unwrap());
    response
}

pub(super) async fn acknowledge(State(state): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let file = state
        .interpreter
        .artifacts
        .lock()
        .unwrap()
        .get(&id)
        .cloned();
    if let Some(file) = file {
        if let Err(error) = std::fs::remove_file(&file.file)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return internal(error);
        }
        state.interpreter.artifacts.lock().unwrap().remove(&id);
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FilesQuery {
    container_id: Option<String>,
    after: Option<String>,
    limit: Option<usize>,
}

pub(super) async fn list_files(
    State(state): State<Arc<App>>,
    Query(query): Query<FilesQuery>,
) -> Response {
    let Ok(_guard) = state.gate.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    let limit = query.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return error(StatusCode::BAD_REQUEST, "invalid code file page size");
    }
    let slot = state.interpreter.context.lock().await;
    let Some(context) = slot.as_ref().filter(|context| {
        !context.expired
            && query
                .container_id
                .as_ref()
                .is_none_or(|id| id == &context.id)
    }) else {
        return error(StatusCode::NOT_FOUND, "code container not found");
    };
    let data = context.data.clone();
    let id = context.id.clone();
    let freeze = context.bridge.is_some();
    drop(slot);
    let result = tokio::task::spawn_blocking(move || {
        if freeze {
            sandbox::pause()?;
        }
        files::list(&data, query.after.as_deref(), limit)
    })
    .await;
    match result {
        Ok(Ok(page)) => Json(serde_json::json!({"container_id":id,"files":page.files,"has_more":page.has_more,"next_cursor":page.next_cursor})).into_response(),
        Ok(Err(_)) => error(StatusCode::UNPROCESSABLE_ENTITY, "code file listing unavailable"),
        Err(failure) => internal(failure),
    }
}

pub(super) async fn export_file(
    State(state): State<Arc<App>>,
    Path(cursor): Path<String>,
) -> Response {
    let Ok(_guard) = state.gate.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    let slot = state.interpreter.context.lock().await;
    let Some(context) = slot.as_ref().filter(|context| !context.expired) else {
        return error(StatusCode::NOT_FOUND, "code container not found");
    };
    let data = context.data.clone();
    let id = context.id.clone();
    let freeze = context.bridge.is_some();
    let spool = state.interpreter.spool.clone();
    drop(slot);
    let result = tokio::task::spawn_blocking(move || {
        if freeze {
            sandbox::pause()?;
        }
        files::export(&data, &spool, &id, &cursor)
    })
    .await;
    match result {
        Ok(Ok(file)) => {
            let id = file.metadata.id.clone();
            state
                .interpreter
                .artifacts
                .lock()
                .unwrap()
                .insert(id.clone(), file);
            let mut response = artifact(State(state.clone()), Path(id.clone())).await;
            response
                .headers_mut()
                .insert("x-code-artifact", id.parse().unwrap());
            response
        }
        Ok(Err(_)) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "code file export unavailable or exceeds 16 MiB",
        ),
        Err(failure) => internal(failure),
    }
}

pub(super) async fn import(
    State(state): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !opaque_id(&id) {
        return error(StatusCode::BAD_REQUEST, "invalid code input id");
    }
    let Some(bytes) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return error(StatusCode::LENGTH_REQUIRED, "content-length required");
    };
    let Some(hash) = headers
        .get("x-content-sha256")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    else {
        return error(StatusCode::BAD_REQUEST, "valid x-content-sha256 required");
    };
    if bytes > MAX_INPUT_BYTES {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "code input exceeds 64 MiB");
    }
    let Ok(_guard) = state.gate.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    let container = headers
        .get("x-code-container")
        .and_then(|value| value.to_str().ok());
    if container.is_some_and(|id| !context_id(id)) {
        return error(StatusCode::BAD_REQUEST, "invalid code container id");
    }
    match state
        .interpreter
        .import_input(&id, container, bytes, &hash.to_ascii_lowercase(), body)
        .await
    {
        Ok(value) => Json(value).into_response(),
        Err(failure) => match failure.downcast_ref::<InputFailure>() {
            Some(InputFailure::Integrity) => error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "code input integrity mismatch",
            ),
            Some(InputFailure::Conflict) => error(StatusCode::CONFLICT, "code input conflict"),
            Some(InputFailure::Limit) => {
                error(StatusCode::PAYLOAD_TOO_LARGE, "code input limit exceeded")
            }
            Some(InputFailure::Expired) => error(StatusCode::NOT_FOUND, "code container not found"),
            None => internal(failure),
        },
    }
}

impl Interpreter {
    async fn import_input(
        &self,
        id: &str,
        container: Option<&str>,
        bytes: u64,
        hash: &str,
        body: Body,
    ) -> anyhow::Result<Value> {
        let mut slot = self.context.lock().await;
        if slot.as_ref().is_some_and(|context| context.expired) {
            if container.is_some() {
                return Err(InputFailure::Expired.into());
            }
            self.retire(&mut slot).await?;
        }
        if slot.is_none() {
            if container.is_some() {
                return Err(InputFailure::Expired.into());
            }
            *slot = Some(self.create_context()?);
        }
        let context = slot.as_mut().unwrap();
        if container.is_some_and(|id| id != context.id) {
            return Err(InputFailure::Expired.into());
        }
        if context
            .bridge
            .as_mut()
            .is_some_and(|bridge| !matches!(bridge.child.try_wait(), Ok(None)))
        {
            return Err(InputFailure::Expired.into());
        }
        if let Some(existing) = context.inputs_index.get(id) {
            if existing != &(bytes, hash.to_owned()) {
                return Err(InputFailure::Conflict.into());
            }
            return Ok(
                serde_json::json!({"container_id":context.id,"file_id":id,"path":format!("/mnt/inputs/{id}/content"),"bytes":bytes,"sha256":hash}),
            );
        }
        if context.inputs_index.len() >= 128
            || context
                .inputs_index
                .values()
                .map(|entry| entry.0)
                .sum::<u64>()
                .saturating_add(bytes)
                > 512 * 1024 * 1024
        {
            return Err(InputFailure::Limit.into());
        }
        let temporary = tempfile::NamedTempFile::new_in(&self.spool)?;
        let mut output = tokio::fs::File::from_std(temporary.reopen()?);
        let mut source = body.into_data_stream();
        let mut received = 0_u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = source.next().await {
            let chunk = chunk.map_err(|_| InputFailure::Integrity)?;
            received = received
                .checked_add(chunk.len() as u64)
                .context("code input size overflow")?;
            if received > bytes {
                return Err(InputFailure::Integrity.into());
            }
            digest.update(&chunk);
            output.write_all(&chunk).await?;
        }
        if received != bytes || hex::encode(digest.finalize()) != hash {
            return Err(InputFailure::Integrity.into());
        }
        output.sync_all().await?;
        drop(output);
        let directory = context.inputs.join(id);
        protected_directory(&directory, false)?;
        let target = directory.join("content");
        temporary
            .persist_noclobber(&target)
            .context("publish code input")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444))?;
        }
        context.inputs_index.insert(id.into(), (bytes, hash.into()));
        Ok(
            serde_json::json!({"container_id":context.id,"file_id":id,"path":format!("/mnt/inputs/{id}/content"),"bytes":bytes,"sha256":hash}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_diagnostics_accept_only_fixed_machine_labels() {
        let error=startup_failure(&serde_json::json!({"type":"startup_error","stage":"launch","kind":"permission_denied"})).unwrap();
        assert_eq!(error.0, "code_kernel_launch_permission_denied");
        let error=startup_failure(&serde_json::json!({"type":"startup_error","stage":"private-path-sentinel","kind":"private-secret-sentinel"})).unwrap();
        assert_eq!(error.0, "code_kernel_unknown_unknown");
        assert!(startup_failure(&serde_json::json!({"type":"ready"})).is_none());
    }

    #[test]
    fn retirement_removes_only_context_scratch_and_preserves_spool() {
        let root = tempfile::tempdir().unwrap();
        let spool = root.path().join("artifacts");
        std::fs::create_dir(&spool).unwrap();
        std::fs::write(spool.join("pending"), b"unpublished").unwrap();
        for _ in 0..12 {
            let id = format!("cntr_{}", uuid::Uuid::new_v4().simple());
            let scratch = root.path().join(&id).join("data");
            std::fs::create_dir_all(&scratch).unwrap();
            std::fs::write(scratch.join("output"), b"scratch").unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&spool, scratch.join("outside")).unwrap();
            remove_context(root.path(), &id).unwrap();
            remove_context(root.path(), &id).unwrap();
            assert!(!root.path().join(&id).exists());
        }
        #[cfg(unix)]
        {
            let id = format!("cntr_{}", uuid::Uuid::new_v4().simple());
            std::os::unix::fs::symlink(&spool, root.path().join(&id)).unwrap();
            assert!(remove_context(root.path(), &id).is_err());
        }
        assert!(remove_context(root.path(), "../artifacts").is_err());
        assert!(remove_context(root.path(), "artifacts").is_err());
        assert_eq!(
            std::fs::read(spool.join("pending")).unwrap(),
            b"unpublished"
        );
    }

    #[test]
    fn request_limits_preserve_execution_and_context_identity() {
        let mut request: CodeRequest =
            serde_json::from_value(serde_json::json!({"id":"run_1","code":"x = 42"})).unwrap();
        assert!(request.validate().is_ok());
        request.container_id = Some("fresh".into());
        assert!(request.validate().is_err());
        request.container_id = Some("cntr_0123456789abcdef0123456789abcdef".into());
        assert!(request.validate().is_ok());
        request.code.push('\0');
        assert!(request.validate().is_err());
    }

    #[test]
    fn bridge_ipc_contains_only_user_execution_payload_not_domain_credentials() {
        let request: CodeRequest = serde_json::from_value(serde_json::json!({
            "id":"run_credentials","code":"print(42)","memory_limit":"1g",
            "container_id":"cntr_0123456789abcdef0123456789abcdef",
            "network_policy":{"type":"allowlist","allowed_domains":["example.com"],
                "domain_secrets":[{"domain":"example.com","name":"Authorization","value":"private-sentinel"}]}
        })).unwrap();
        let bytes = kernel_request(&request).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 4);
        assert_eq!(value["code"], "print(42)");
        for private in [
            "private-sentinel",
            "network_policy",
            "domain_secrets",
            "Authorization",
            "container_id",
            "memory_limit",
        ] {
            assert!(!String::from_utf8_lossy(&bytes).contains(private));
        }
    }

    #[test]
    fn outputs_refer_only_to_captured_opaque_artifacts() {
        let (outputs, truncated) = resolve_outputs(
            vec![
                serde_json::json!({"type":"image","path":"/etc/shadow"}),
                serde_json::json!({"type":"logs","logs":"hello"}),
            ],
            &[],
            100,
        );
        assert!(truncated);
        assert_eq!(
            outputs,
            vec![serde_json::json!({"type":"logs","logs":"hello"})]
        );
    }

    #[test]
    fn supervisor_rechecks_the_combined_log_budget() {
        let (outputs, truncated) = resolve_outputs(
            vec![
                serde_json::json!({"type":"logs","logs":"雪🙂suffix"}),
                serde_json::json!({"type":"logs","logs":"more"}),
            ],
            &[],
            7,
        );
        assert!(truncated);
        assert_eq!(
            outputs,
            vec![serde_json::json!({"type":"logs","logs":"雪🙂"})]
        );
    }
}
