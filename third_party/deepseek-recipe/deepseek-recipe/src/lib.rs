//! API protocol conversion and inference output parsing.
//!
//! [`request::ProtocolRequest::convert`] produces a shared
//! [`request::ConversationRequest`]. [`stream::StreamProcessor`] parses backend
//! output, passes it to the selected protocol's chunk generator
//! ([`stream::ChunkGenerator`]), and accumulates completion usage. The caller
//! renders and encodes the prompt, supplies the backend and its prompt usage,
//! and transports the protocol responses.

pub use protocol::anthropic;
pub use protocol::openai;

pub mod error_response;
mod protocol;
pub mod request;
pub mod response;
pub mod stream;

/// Deserialize the exact upstream wire body using the recipe's ordered JSON.
/// Keeps the host application's serde_json feature set independent.
pub fn responses_request_from_slice(bytes: &[u8]) -> Result<openai::ResponsesRequest, String> {
    serde_json::from_slice(bytes).map_err(|error| error.to_string())
}
