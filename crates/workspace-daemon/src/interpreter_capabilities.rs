//! Report only memory profiles the running kernel can actually guarantee.
use super::*;

pub(super) async fn capabilities(State(_state): State<Arc<App>>) -> Response {
    use crate::interpreter_limits::{MemoryLimit, verify};
    let memory_limits: Vec<MemoryLimit> = [MemoryLimit::OneGiB, MemoryLimit::FourGiB]
        .into_iter()
        .filter(|memory| verify(Some(*memory)).is_ok())
        .collect();
    Json(serde_json::json!({
        "protocol":"workspace-daemon","context_scope":"instance","version":1,"memory_limits":memory_limits,
        "network_disabled":true,"network_allowlist":cfg!(target_os="linux"),
        "max_execution_ms":300000,"max_artifact_bytes":16*1024*1024,
        "atomic_boot_header":true,
    }))
    .into_response()
}
