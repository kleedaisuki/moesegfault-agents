//! Identity protocol policy and credential custody, independent of the Worker transport.
pub mod billing;
pub mod oidc;
pub mod secrets;

pub use oidc::*;
pub use secrets::*;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid authentication configuration")]
    Configuration,
    #[error("invalid authentication transaction")]
    Transaction,
    #[error("invalid token")]
    Token,
    #[error("credential encryption failed")]
    Encryption,
    #[error("secure randomness unavailable")]
    Random,
}
