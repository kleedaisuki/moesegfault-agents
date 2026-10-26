mod file_import;
mod interpreter;
mod interpreter_capabilities;
mod interpreter_egress;
mod interpreter_files;
mod interpreter_limits;
mod interpreter_network;
mod interpreter_process;
mod process;
mod snapshot;

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::StreamExt;
use process::{ExecRequest, ExecResult};
use sha2::{Digest, Sha256};
use snapshot::SnapshotStore;
use subtle::ConstantTimeEq;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use tracing::Instrument;

const MAX_ARCHIVE: u64 = snapshot::MAX_ARCHIVE_BYTES;

struct Active {
    id: String,
    cancel: CancellationToken,
}

struct App {
    token_hash: [u8; 32],
    store: Arc<SnapshotStore>,
    gate: Arc<Mutex<()>>,
    active: Mutex<Option<Active>>,
    completed: Mutex<VecDeque<ExecResult>>,
    seen: Mutex<VecDeque<String>>,
    boot_id: String,
    revision: AtomicU64,
    dirty_bytes: AtomicU64,
    last_snapshot: Mutex<Option<(String, u64)>>,
    fingerprint: Mutex<String>,
    interpreter: Arc<interpreter::Interpreter>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();
    process::require_supported_platform()?;
    let token = std::env::var("MAGENTS_DAEMON_TOKEN")?;
    anyhow::ensure!(
        token.len() >= 32,
        "MAGENTS_DAEMON_TOKEN must contain at least 32 bytes"
    );
    let root =
        PathBuf::from(std::env::var("MAGENTS_STATE_DIR").unwrap_or("/var/lib/magents".into()));
    let interpreter = Arc::new(interpreter::Interpreter::new(&root)?);
    let store = SnapshotStore::open(root, 512 * 1024 * 1024, 100_000)?;
    process::prepare_workspace(&store.current()?)?;
    let (logical_bytes, fingerprint) = store
        .fingerprint()
        .unwrap_or((512 * 1024 * 1024, String::new()));
    let initial_dirty = if std::fs::read_dir(store.current()?)?.next().is_some() {
        logical_bytes.max(1)
    } else {
        0
    };
    let state = Arc::new(App {
        token_hash: Sha256::digest(token.as_bytes()).into(),
        store: Arc::new(store),
        gate: Arc::new(Mutex::new(())),
        active: Mutex::new(None),
        completed: Mutex::new(VecDeque::new()),
        seen: Mutex::new(VecDeque::new()),
        boot_id: uuid::Uuid::new_v4().to_string(),
        revision: AtomicU64::new(0),
        dirty_bytes: AtomicU64::new(initial_dirty),
        last_snapshot: Mutex::new(None),
        fingerprint: Mutex::new(fingerprint),
        interpreter,
    });
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!(event = "workspace.ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn router(state: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/checkpoint-ack", post(checkpoint_ack))
        .route("/v1/exec", post(exec))
        .route("/v1/exec/{id}", get(exec_status).delete(cancel_exec))
        .route(
            "/v1/python",
            post(interpreter::execute).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route(
            "/v1/python/capabilities",
            get(interpreter_capabilities::capabilities),
        )
        .route(
            "/v1/python/contexts",
            post(interpreter::prepare_context)
                .get(interpreter::current_context)
                .layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route(
            "/v1/python/contexts/{id}",
            get(interpreter::inspect_context).delete(interpreter::delete_context),
        )
        .route(
            "/v1/python/{id}",
            get(interpreter::status).delete(interpreter::cancel),
        )
        .route("/v1/python/files", get(interpreter::list_files))
        .route("/v1/python/source/{cursor}", get(interpreter::export_file))
        .route(
            "/v1/python/files/{id}",
            put(interpreter::import).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/python/artifacts/{id}",
            get(interpreter::artifact).delete(interpreter::acknowledge),
        )
        .route("/v1/snapshot", get(snapshot))
        .route(
            "/v1/files/{id}",
            put(file_import::import).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/restore",
            post(restore).layer(DefaultBodyLimit::disable()),
        )
        .layer(DefaultBodyLimit::max(32 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

async fn health(State(state): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"status":"ok", "boot_id":state.boot_id}))
}

async fn status(State(state): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(
        serde_json::json!({"running":state.active.lock().await.is_some(), "dirty_bytes":state.dirty_bytes.load(Ordering::SeqCst)}),
    )
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointAck {
    sha256: String,
}

async fn checkpoint_ack(State(state): State<Arc<App>>, Json(ack): Json<CheckpointAck>) -> Response {
    let Ok(_guard) = state.gate.try_lock() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    let saved = state.last_snapshot.lock().await;
    match saved.as_ref() {
        Some((hash, revision)) if hash == &ack.sha256 => {
            if *revision == state.revision.load(Ordering::SeqCst) {
                state.dirty_bytes.store(0, Ordering::SeqCst);
            }
            StatusCode::NO_CONTENT.into_response()
        }
        _ => error(
            StatusCode::CONFLICT,
            "checkpoint hash is not the latest snapshot",
        ),
    }
}

async fn authenticate(State(state): State<Arc<App>>, request: Request, next: Next) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let actual: [u8; 32] = Sha256::digest(token.unwrap_or("").as_bytes()).into();
    if token.is_none() || !bool::from(actual.ct_eq(&state.token_hash)) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let traceparent = request
        .headers()
        .get("traceparent")
        .and_then(|h| h.to_str().ok())
        .filter(|h| valid_traceparent(h))
        .unwrap_or("");
    let request_id = uuid::Uuid::new_v4().to_string();
    let span = tracing::info_span!("workspace.http", request_id = %request_id, traceparent, method = %request.method(), path = diagnostic_path(request.uri().path()));
    let started = std::time::Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;
    span.in_scope(|| {
        tracing::info!(
            event = "workspace.http_complete",
            status = response.status().as_u16(),
            duration_ms = started.elapsed().as_millis() as u64
        )
    });
    response
        .headers_mut()
        .insert("x-request-id", request_id.parse().unwrap());
    response
}

fn valid_traceparent(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some("00"), Some(trace), Some(span), Some(flags), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let hex = |field: &str| {
        field
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    trace.len() == 32
        && span.len() == 16
        && flags.len() == 2
        && hex(trace)
        && hex(span)
        && hex(flags)
        && trace.bytes().any(|b| b != b'0')
        && span.bytes().any(|b| b != b'0')
}

fn diagnostic_path(path: &str) -> &str {
    if path.starts_with("/v1/python/source/") {
        "/v1/python/source/:file"
    } else {
        path
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({"error":message}))).into_response()
}

fn internal(_err: impl std::fmt::Display) -> Response {
    // Error chains can contain private workspace paths. Export only an event
    // category by default; opt-in diagnostics belong to the control plane.
    tracing::error!(event = "workspace.error");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "workspace operation failed",
    )
}

async fn record_tree(state: &App, tree: Option<(u64, String)>) {
    if let Some((bytes, fingerprint)) = tree {
        let mut previous = state.fingerprint.lock().await;
        if *previous == fingerprint {
            return;
        }
        *previous = fingerprint;
        state.revision.fetch_add(1, Ordering::SeqCst);
        state.dirty_bytes.fetch_add(bytes.max(1), Ordering::SeqCst);
        return;
    }
    // Unsupported entries or failed stats must never turn a possible mutation
    // into a clean workspace. Checkpoint creation will report the exact cause.
    state.revision.fetch_add(1, Ordering::SeqCst);
    state
        .dirty_bytes
        .fetch_add(512 * 1024 * 1024, Ordering::SeqCst);
}

async fn exec(State(state): State<Arc<App>>, Json(request): Json<ExecRequest>) -> Response {
    if let Err(message) = request.validate() {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let Ok(guard) = state.gate.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    let mut seen = state.seen.lock().await;
    if seen.iter().any(|id| id == &request.id) {
        return error(StatusCode::CONFLICT, "execution id already exists");
    }
    seen.push_back(request.id.clone());
    if seen.len() > 128 {
        seen.pop_front();
    }
    drop(seen);
    let cancel = CancellationToken::new();
    *state.active.lock().await = Some(Active {
        id: request.id.clone(),
        cancel: cancel.clone(),
    });
    let workspace = match state.store.current() {
        Ok(path) => path,
        Err(err) => {
            *state.active.lock().await = None;
            return internal(err);
        }
    };
    // A detached task owns the process and lock: losing the HTTP client does not
    // create an untracked command, and the caller can reconnect by execution id.
    let task_state = state.clone();
    let handle = tokio::spawn(
        async move {
            let _guard = guard;
            let result = process::execute(request, workspace, cancel).await;
            let scan_store = task_state.store.clone();
            let tree = tokio::task::spawn_blocking(move || scan_store.fingerprint())
                .await
                .ok()
                .and_then(Result::ok);
            record_tree(&task_state, tree).await;
            if let Ok(result) = &result {
                let mut completed = task_state.completed.lock().await;
                completed.push_back(result.clone());
                if completed.len() > 128 {
                    completed.pop_front();
                }
            }
            *task_state.active.lock().await = None;
            result
        }
        .in_current_span(),
    );
    match handle.await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(err)) => internal(err),
        Err(err) => internal(err),
    }
}

async fn exec_status(State(state): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if let Some(result) = state
        .completed
        .lock()
        .await
        .iter()
        .find(|r| r.id == id)
        .cloned()
    {
        return Json(result).into_response();
    }
    if state
        .active
        .lock()
        .await
        .as_ref()
        .is_some_and(|a| a.id == id)
    {
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"id":id,"status":"running"})),
        )
            .into_response();
    }
    error(StatusCode::NOT_FOUND, "execution not found")
}

async fn cancel_exec(State(state): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let active = state.active.lock().await;
    match active.as_ref().filter(|a| a.id == id) {
        Some(active) => {
            active.cancel.cancel();
            StatusCode::NO_CONTENT.into_response()
        }
        None => error(StatusCode::NOT_FOUND, "execution not running"),
    }
}

async fn snapshot(State(state): State<Arc<App>>) -> Response {
    let Ok(guard) = state.gate.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    let store = state.store.clone();
    let task_state = state.clone();
    let (artifact, _guard, revision) = match tokio::task::spawn_blocking(move || {
        process::quiesce()?;
        let artifact = store.create()?;
        Ok::<_, anyhow::Error>((artifact, guard, task_state.revision.load(Ordering::SeqCst)))
    })
    .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => {
            tracing::warn!(event = "workspace.snapshot_rejected");
            return error(StatusCode::UNPROCESSABLE_ENTITY, &err.to_string());
        }
        Err(err) => return internal(err),
    };
    let file = match tokio::fs::File::open(&artifact.path).await {
        Ok(file) => file,
        Err(err) => return internal(err),
    };
    // The stream owns the open descriptor; unlink prevents unbounded archive
    // accumulation, including when a client disconnects midway through a body.
    if let Err(err) = tokio::fs::remove_file(&artifact.path).await {
        return internal(err);
    }
    let metadata = artifact.metadata.clone();
    *state.last_snapshot.lock().await = Some((metadata.sha256.clone(), revision));
    let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, "application/zstd".parse().unwrap());
    headers.insert(header::CONTENT_LENGTH, metadata.archive_bytes.into());
    for (key, value) in [
        ("x-snapshot-sha256", metadata.sha256),
        (
            "x-snapshot-logical-bytes",
            metadata.logical_bytes.to_string(),
        ),
        (
            "x-snapshot-archive-bytes",
            metadata.archive_bytes.to_string(),
        ),
        ("x-snapshot-entries", metadata.entries.to_string()),
        (
            "x-snapshot-format-version",
            metadata.format_version.to_string(),
        ),
    ] {
        headers.insert(key, value.parse().unwrap());
    }
    response
}

async fn restore(State(state): State<Arc<App>>, headers: HeaderMap, body: Body) -> Response {
    let Some(hash) = headers
        .get("x-snapshot-sha256")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return error(StatusCode::BAD_REQUEST, "valid x-snapshot-sha256 required");
    };
    let Ok(guard) = state.gate.clone().try_lock_owned() else {
        return error(StatusCode::CONFLICT, "workspace busy");
    };
    let incoming = match state.store.incoming() {
        Ok(file) => file,
        Err(err) => return internal(err),
    };
    let mut output = match incoming.reopen() {
        Ok(file) => tokio::fs::File::from_std(file),
        Err(err) => return internal(err),
    };
    let mut stream = body.into_data_stream();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return error(StatusCode::BAD_REQUEST, "incomplete archive body"),
        };
        size += chunk.len() as u64;
        if size > MAX_ARCHIVE {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "archive exceeds 256 MiB");
        }
        if let Err(err) = output.write_all(&chunk).await {
            return internal(err);
        }
    }
    if let Err(err) = output.sync_all().await {
        return internal(err);
    }
    drop(output);
    let store = state.store.clone();
    let hash = hash.to_ascii_lowercase();
    match tokio::task::spawn_blocking(move || {
        process::quiesce()?;
        let result = store.restore(incoming.path(), &hash)?;
        process::prepare_workspace(&store.current()?)?;
        let (_, fingerprint) = store.fingerprint()?;
        Ok::<_, anyhow::Error>((result, guard, fingerprint))
    })
    .await
    {
        Ok(Ok((result, _guard, fingerprint))) => {
            state.revision.fetch_add(1, Ordering::SeqCst);
            state.dirty_bytes.store(0, Ordering::SeqCst);
            *state.last_snapshot.lock().await = None;
            *state.fingerprint.lock().await = fingerprint;
            Json(result).into_response()
        }
        Ok(Err(err)) => {
            tracing::warn!(event = "workspace.restore_rejected");
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("invalid or unsafe snapshot: {err}"),
            )
        }
        Err(err) => internal(err),
    }
}

#[cfg(test)]
fn test_dir() -> tempfile::TempDir {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.cache/workspace-tests");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("test-")
        .tempdir_in(root)
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use tower::ServiceExt;

    const TOKEN: &str = "test-only-token-with-at-least-32-bytes";

    #[test]
    fn trace_correlation_requires_valid_nonzero_w3c_identity() {
        assert!(valid_traceparent(
            "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"
        ));
        assert!(!valid_traceparent(
            "00-00000000000000000000000000000000-0123456789abcdef-01"
        ));
        assert!(!valid_traceparent(&"-".repeat(55)));
        assert!(!valid_traceparent(
            "ff-0123456789abcdef0123456789abcdef-0123456789abcdef-01"
        ));
    }

    fn app(root: PathBuf) -> Arc<App> {
        let interpreter = Arc::new(interpreter::Interpreter::new(&root).unwrap());
        Arc::new(App {
            token_hash: Sha256::digest(TOKEN.as_bytes()).into(),
            store: Arc::new(SnapshotStore::open(root, 1024 * 1024, 100).unwrap()),
            gate: Arc::new(Mutex::new(())),
            active: Mutex::new(None),
            completed: Mutex::new(VecDeque::new()),
            seen: Mutex::new(VecDeque::new()),
            boot_id: uuid::Uuid::new_v4().to_string(),
            revision: AtomicU64::new(0),
            dirty_bytes: AtomicU64::new(0),
            last_snapshot: Mutex::new(None),
            fingerprint: Mutex::new(String::new()),
            interpreter,
        })
    }

    fn request(method: &str, path: &str, body: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    #[tokio::test]
    async fn all_routes_require_internal_auth() {
        let temp = crate::test_dir();
        let app = router(app(temp.path().to_owned()));
        for path in [
            "/health",
            "/v1/status",
            "/v1/snapshot",
            "/v1/exec/run-1",
            "/v1/files/file-1",
            "/v1/python",
            "/v1/python/contexts",
            "/v1/python/cell-1",
            "/v1/python/files",
            "/v1/python/files/file-1",
            "/v1/python/source/70726976617465",
            "/v1/python/artifacts/pyart-1",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[test]
    fn diagnostic_routes_do_not_encode_user_file_paths() {
        assert_eq!(
            diagnostic_path("/v1/python/source/70726976617465"),
            "/v1/python/source/:file"
        );
        assert_eq!(diagnostic_path("/health"), "/health");
    }

    #[tokio::test]
    async fn health_exposes_stable_boot_identity_not_credentials() {
        let temp = crate::test_dir();
        let state = app(temp.path().to_owned());
        let app = router(state.clone());
        let response = app.oneshot(request("GET", "/health", "")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("x-request-id"));
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["boot_id"], state.boot_id);
        assert!(!String::from_utf8_lossy(&body).contains(TOKEN));
    }

    #[tokio::test]
    async fn late_checkpoint_ack_preserves_newer_dirty_state() {
        let temp = crate::test_dir();
        let state = app(temp.path().to_owned());
        state.revision.store(2, Ordering::SeqCst);
        state.dirty_bytes.store(123, Ordering::SeqCst);
        *state.last_snapshot.lock().await = Some(("abc".into(), 1));
        let app = router(state.clone());
        let response = app
            .clone()
            .oneshot(request("POST", "/v1/checkpoint-ack", r#"{"sha256":"abc"}"#))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(state.dirty_bytes.load(Ordering::SeqCst), 123);
        *state.last_snapshot.lock().await = Some(("def".into(), 2));
        let response = app
            .oneshot(request("POST", "/v1/checkpoint-ack", r#"{"sha256":"def"}"#))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(state.dirty_bytes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn invalid_limits_fail_before_execution() {
        let temp = crate::test_dir();
        let app = router(app(temp.path().to_owned()));
        let response = app
            .oneshot(request(
                "POST",
                "/v1/exec",
                r#"{"id":"run","command":"true","timeout_ms":600001}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn file_import_rejects_invalid_or_incomplete_bodies_before_publication() {
        let temp = crate::test_dir();
        let state = app(temp.path().to_owned());
        let app = router(state.clone());
        let digest = hex::encode(Sha256::digest(b"expected"));
        let cases = [
            ("8", "invalid", "expected", StatusCode::BAD_REQUEST),
            (
                "67108865",
                digest.as_str(),
                "",
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
            (
                "8",
                digest.as_str(),
                "short",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            ("4", digest.as_str(), "longer", StatusCode::BAD_REQUEST),
            ("8", digest.as_str(), "incorrect", StatusCode::BAD_REQUEST),
        ];
        for (length, hash, body, expected) in cases {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri("/v1/files/file-1")
                        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                        .header(header::CONTENT_LENGTH, length)
                        .header("x-content-sha256", hash)
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        assert!(!state.store.current().unwrap().join("uploads").exists());
        assert_eq!(state.revision.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn read_only_operations_never_accumulate_dirty_bytes() {
        let temp = crate::test_dir();
        let state = app(temp.path().to_owned());
        *state.fingerprint.lock().await = "baseline".into();
        for size in [0, 64 * 1024 * 1024, 512 * 1024 * 1024] {
            for _ in 0..16 {
                record_tree(&state, Some((size, "baseline".into()))).await;
            }
            assert_eq!(state.dirty_bytes.load(Ordering::SeqCst), 0);
            assert_eq!(state.revision.load(Ordering::SeqCst), 0);
        }
        record_tree(&state, Some((0, "deletion".into()))).await;
        assert_eq!(state.dirty_bytes.load(Ordering::SeqCst), 1);
        assert_eq!(state.revision.load(Ordering::SeqCst), 1);
        for _ in 0..16 {
            record_tree(&state, Some((0, "deletion".into()))).await;
        }
        assert_eq!(state.dirty_bytes.load(Ordering::SeqCst), 1);
    }
}
