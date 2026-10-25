//! Shared, platform-independent contracts for the agent control and execution planes.

pub mod agent;
pub mod context;
pub mod session;
pub mod tool_services;
pub mod usage;

pub use agent::*;
pub use context::*;
pub use session::*;
pub use tool_services::*;
pub use usage::*;

pub const SESSION_IDLE_TIMEOUT_MS: u64 = 10 * 60 * 1_000;
pub const API_VERSION: &str = "v1";
