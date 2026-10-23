mod allowances;
mod auth;
mod metering;
mod provider;
mod reconcile;
mod resources;
mod routes;
mod session;
mod skills;
mod store;
mod telemetry;
mod tools;
mod util;
mod workspace;

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
        if reconcile::run(env).await.is_err() {
            util::emit("storage.reconciliation_failed", serde_json::json!({}));
        }
    });
}
