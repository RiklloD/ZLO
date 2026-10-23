//! Engine errors. Model- and user-correctable failures carry a stable code
//! (the T3 error vocabulary) so tools and routes can render them verbatim.

use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    /// A correctable request failure with a T3-compatible code
    /// (`invalid_request`, `task_not_found`, `depth_exceeded`, …).
    #[error("{code}: {message}")]
    Request { code: &'static str, message: String },
    #[error("orchestrator store failure: {0}")]
    Store(String),
    #[error("orchestrator store holds an unreadable row: {0}")]
    Corrupt(String),
    #[error("runtime failure: {0}")]
    Runtime(#[from] crate::runtime::RuntimeError),
}

impl OrchestratorError {
    pub fn request(code: &'static str, message: impl Into<String>) -> Self {
        Self::Request {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::request("invalid_request", message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::request(code, message)
    }

    /// Stable code for transports.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Request { code, .. } => code,
            Self::Store(_) | Self::Corrupt(_) => "orchestration_error",
            Self::Runtime(error) => error.code(),
        }
    }

    pub fn is_correctable(&self) -> bool {
        matches!(self, Self::Request { .. } | Self::Runtime(_))
    }

    /// Model-visible JSON body (`{ok: false, error, message}`). Store
    /// failures stay generic: their detail is logged, never shown.
    pub fn to_tool_json(&self) -> serde_json::Value {
        let message = match self {
            Self::Request { message, .. } => message.clone(),
            Self::Runtime(error) => error.to_string(),
            Self::Store(_) | Self::Corrupt(_) => "orchestrator storage is unavailable".to_string(),
        };
        json!({"ok": false, "error": self.code(), "message": message})
    }
}

impl From<libsql::Error> for OrchestratorError {
    fn from(error: libsql::Error) -> Self {
        Self::Store(error.to_string())
    }
}

impl From<ironclaw_libsql_runtime::LibSqlRuntimeError> for OrchestratorError {
    fn from(error: ironclaw_libsql_runtime::LibSqlRuntimeError) -> Self {
        Self::Store(error.to_string())
    }
}

impl From<ironclaw_host_api::error::HostApiError> for OrchestratorError {
    fn from(error: ironclaw_host_api::error::HostApiError) -> Self {
        Self::invalid(error.to_string())
    }
}

impl From<serde_json::Error> for OrchestratorError {
    fn from(error: serde_json::Error) -> Self {
        Self::Corrupt(error.to_string())
    }
}
