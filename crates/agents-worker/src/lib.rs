mod allowances;
mod auth;
mod auxiliary_context;
mod auxiliary_routes;
mod auxiliary_source;
mod chat_relay;
mod checkpoint_gc;
mod cleanup;
mod client_tool_items;
mod code_borrowed_client;
mod code_borrowing;
mod code_context_routes;
mod code_contexts;
mod code_executor_runtime;
mod code_executor_state;
mod code_executor_transport;
mod code_owned_bindings;
mod code_runtime;
mod context_management;
mod context_prefix;
mod conversation_meter;
mod conversation_quota;
mod conversation_routes;
mod conversation_state;
mod custom_tool_formats;
mod deepseek_image_tokens;
mod deepseek_tokenizer;
mod execution_leases;
mod exports;
mod file_index;
mod file_index_scheduler;
mod file_ingest;
mod file_search;
mod file_search_routes;
mod file_vector;
mod hosted_artifacts;
mod hosted_bindings;
mod hosted_image;
mod hosted_search;
mod hosted_service_metering;
mod hosted_tools;
mod image_artifacts;
mod image_generation;
mod input_image_metadata;
mod mcp_bindings;
mod mcp_funding;
mod mcp_headers;
mod mcp_items;
mod mcp_runtime;
mod mcp_transport;
mod metering;
mod model_operation;
mod native_window;
mod observation_registry;
mod observer_transport;
mod programmatic_metering;
mod programmatic_runtime;
mod programmatic_tools;
mod prompt_templates;
mod provider;
mod public_image_input;
mod reconcile;
mod resources;
mod response_execution;
mod response_image_projection;
mod response_index;
mod response_limits;
mod response_preflight;
mod response_routes;
mod response_websocket;
mod response_websocket_auth;
mod response_websocket_executor;
mod responses_attachments;
mod responses_auxiliary;
mod responses_buffer;
mod responses_schema;
mod responses_wire;
mod routes;
mod runtime_profiles;
mod search_byok;
mod search_deepseek;
mod session;
mod session_storage;
mod session_views;
mod skills;
mod space;
mod store;
mod telemetry;
mod tool_discovery;
mod tool_rendering;
mod tool_service_routes;
mod tools;
mod util;
mod vector_store_routes;
mod workspace;
mod workspace_blobs;
mod workspace_versions;

pub use auth::AuthSession;
pub use conversation_state::ConversationState;
pub use model_operation::ModelOperation;
pub use session::{Session, Session4G};
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
        if file_index_scheduler::run(&env).await.is_err() {
            util::emit("file_search.scheduler_deferred", serde_json::json!({}));
        }
        if reconcile::run(env).await.is_err() {
            util::emit("storage.reconciliation_failed", serde_json::json!({}));
        }
    });
}
