mod allowances;
mod auth;
mod checkpoint_gc;
mod cleanup;
mod exports;
mod metering;
mod provider;
mod reconcile;
mod resources;
mod response_index;
mod response_preflight;
mod response_routes;
mod responses_attachments;
mod responses_schema;
mod responses_wire;
mod routes;
mod session;
mod session_storage;
mod session_views;
mod skills;
mod space;
mod store;
mod telemetry;
mod tools;
mod util;
mod workspace;
mod workspace_versions;

pub use auth::AuthSession;
pub use session::Session;
use worker::*;

#[event(fetch)]
pub async fn fetch(req: Request, env: Env, ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();
    routes::handle(req, env, ctx).await
}

#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, ctx: ScheduleContext) {
    ctx.wait_until(async move {
        if auth::cleanup_expired(&env).await.is_err() {
            util::emit("auth.cleanup_failed", serde_json::json!({}));
        }
        if space::recover(&env).await.is_err() {
            util::emit("space.recovery_deferred", serde_json::json!({}));
        }
        if space::reconcile_attachment_pins(&env).await.is_err() {
            util::emit(
                "space.attachment_pin_recovery_deferred",
                serde_json::json!({}),
            );
        }
        if workspace_versions::cleanup_uploads(&env).await.is_err() {
            util::emit("workspace.version_recovery_deferred", serde_json::json!({}));
        }
        if checkpoint_gc::run(&env).await.is_err() {
            util::emit("checkpoint.gc_deferred", serde_json::json!({}));
        }
        if reconcile::run(env).await.is_err() {
            util::emit("storage.reconciliation_failed", serde_json::json!({}));
        }
    });
}
