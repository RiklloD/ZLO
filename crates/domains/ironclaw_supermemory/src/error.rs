//! Supermemory errors. Messages never carry the API key, request bodies or
//! raw provider responses.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SupermemoryError {
    #[error("supermemory is not enabled for this caller")]
    Disabled,
    #[error("invalid container tag: {reason}")]
    InvalidContainer { reason: String },
    #[error("invalid supermemory input: {reason}")]
    InvalidInput { reason: String },
    #[error("invalid supermemory configuration: {reason}")]
    InvalidConfig { reason: String },
    #[error("supermemory rejected the API key (HTTP {status})")]
    Unauthorized { status: u16 },
    #[error("supermemory rate limit persisted after retries")]
    RateLimited,
    #[error("supermemory returned HTTP {status}")]
    Http { status: u16 },
    #[error("supermemory request failed: {reason}")]
    Transport { reason: String },
    #[error("supermemory response could not be decoded: {reason}")]
    Decode { reason: String },
    #[error("supermemory API key could not be read: {reason}")]
    Secret { reason: String },
}

impl SupermemoryError {
    /// Stable, model-safe category label.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::InvalidContainer { .. } | Self::InvalidInput { .. } => "invalid_input",
            Self::InvalidConfig { .. } => "invalid_config",
            Self::Unauthorized { .. } => "unauthorized",
            Self::RateLimited => "rate_limited",
            Self::Http { .. } => "provider_error",
            Self::Transport { .. } => "unavailable",
            Self::Decode { .. } => "malformed_response",
            Self::Secret { .. } => "secret_unavailable",
        }
    }

    /// Whether the model can correct the failure by changing its arguments.
    pub fn is_input_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidContainer { .. } | Self::InvalidInput { .. }
        )
    }
}
