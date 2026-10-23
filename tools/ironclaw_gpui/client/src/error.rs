use serde::Deserialize;

/// The gateway's redacted error envelope (`WebUiV2HttpErrorBody`): snake_case
/// codes only, never prose.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiErrorBody {
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub validation_code: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Config(String),
    #[error("cannot reach the IronClaw gateway: {0}")]
    Network(#[from] reqwest::Error),
    #[error("{message} (HTTP {status})")]
    Api {
        status: u16,
        message: String,
        body: Box<ApiErrorBody>,
    },
    #[error("unexpected response from the gateway: {0}")]
    Decode(String),
    /// A correctable refusal the orchestrator answers with HTTP 200 and
    /// `{"ok": false, "error": code, "message": text}`.
    #[error("{message}")]
    Refused { code: String, message: String },
}

impl ClientError {
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. })
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Api { status: 404, .. })
    }

    pub(crate) fn from_status(status: u16, text: &str) -> Self {
        let body: ApiErrorBody = serde_json::from_str(text).unwrap_or_default();
        // Same humanizing the SPA does: most specific code wins.
        let code = body
            .validation_code
            .as_deref()
            .or(body.kind.as_deref())
            .or(body.error.as_deref());
        let mut message = match code {
            Some(code) => humanize(code),
            None if !text.is_empty() && text.len() <= 200 && !text.starts_with('{') => {
                text.trim().to_string()
            }
            None => "Request failed".to_string(),
        };
        if let Some(field) = &body.field {
            message.push_str(&format!(" ({field})"));
        }
        Self::Api {
            status,
            message,
            body: Box::new(body),
        }
    }
}

fn humanize(token: &str) -> String {
    let words = token.replace(['_', '-'], " ");
    let mut chars = words.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
