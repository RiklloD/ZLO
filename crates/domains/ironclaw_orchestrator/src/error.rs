//! Engine errors. Model- and user-correctable failures carry a stable code
//! (the T3 error vocabulary) so tools and routes can render them verbatim.

use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    /// A correctable request failure with a T3-compatible code
    /// (`invalid_request`, `task_not_found`, `depth_exceeded`, …).
    #[error("{code}: {message}")]
    Request { code: &'static str, message: String },
    /// A delegated task was recorded but its child never started: the
    /// failure names the task, so a client can tell which call it ended.
    #[error("{code}: {message}")]
    StartFailed {
        code: &'static str,
        message: String,
        task_id: crate::model::TaskId,
    },
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
            Self::Request { code, .. } | Self::StartFailed { code, .. } => code,
            Self::Store(_) | Self::Corrupt(_) => "orchestration_error",
            Self::Runtime(error) => error.code(),
        }
    }

    pub fn is_correctable(&self) -> bool {
        matches!(
            self,
            Self::Request { .. } | Self::StartFailed { .. } | Self::Runtime(_)
        )
    }

    /// Model-visible JSON body (`{ok: false, error, message}`). Store
    /// failures stay generic: their detail is logged, never shown.
    pub fn to_tool_json(&self) -> serde_json::Value {
        let message = match self {
            Self::Request { message, .. } | Self::StartFailed { message, .. } => message.clone(),
            Self::Runtime(error) => error.to_string(),
            Self::Store(_) | Self::Corrupt(_) => "orchestrator storage is unavailable".to_string(),
        };
        let mut body = json!({"ok": false, "error": self.code(), "message": message});
        if let Self::StartFailed { task_id, .. } = self {
            body["taskId"] = json!(task_id.as_str());
        }
        body
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

#[cfg(test)]
mod tests {
    use super::OrchestratorError;
    use crate::model::TaskId;

    /// A child that never started still names its task, so the caller's
    /// transcript can pair the failed call with the failed task.
    #[test]
    fn a_failed_start_names_its_task_in_the_tool_body() {
        let error = OrchestratorError::StartFailed {
            code: "orchestration_error",
            message: "Could not start the child agent: no runtime".into(),
            task_id: TaskId::new("task_0123456789abcdef").unwrap(),
        };
        let body = error.to_tool_json();
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], "orchestration_error");
        assert_eq!(body["taskId"], "task_0123456789abcdef");
        assert!(error.is_correctable());
        let plain = OrchestratorError::invalid("task is required").to_tool_json();
        assert!(plain.get("taskId").is_none());
    }
}
