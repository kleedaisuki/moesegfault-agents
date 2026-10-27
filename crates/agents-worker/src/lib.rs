mod allowances;
mod auth;
mod checkpoint_gc;
mod cleanup;
mod exports;
mod metering;
mod provider;
mod reconcile;
mod resources;
mod routes;
mod session;
mod session_storage;
mod skills;
mod store;
mod telemetry;
mod tools;
mod util;
mod workspace;

pub use auth::AuthSession;
#[cfg(not(target_arch = "wasm32"))]
#[doc(hidden)]
pub use provider::{FunctionCall, Message, ToolCall};
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
        if checkpoint_gc::run(&env).await.is_err() {
            util::emit("checkpoint.gc_deferred", serde_json::json!({}));
        }
        if reconcile::run(env).await.is_err() {
            util::emit("storage.reconciliation_failed", serde_json::json!({}));
        }
    });
}
