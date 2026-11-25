//! Typed errors so the core and frontends react deterministically instead of
//! parsing error strings.

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("authentication required for provider {provider}")]
    AuthRequired { provider: String },

    #[error("cancelled")]
    Cancelled,

    /// An operation ran past its wall-clock budget and was stopped.
    #[error("timed out after {limit_ms} ms")]
    Timeout { limit_ms: u64 },

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("rate limited by {provider}{retry_after}", retry_after = retry_after.map(|s| format!("; retry in {s}s")).unwrap_or_default())]
    RateLimited {
        provider: String,
        retry_after: Option<u64>,
    },

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("unavailable: {0}")]
    Unavailable(String),

    #[error("internal error: {0}")]
    Internal(String),
}
