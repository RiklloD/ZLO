//! "Triage an issue": the operator route a client calls to start an in-app
//! triage session (`docs/using/triage.mdx`). The backend writes the session
//! files (machine context and the playbook prompt) under its data folder,
//! starts fetching the source of its own build, and answers the composer
//! text that links the prompt file. Shared by the WebUI transport and the
//! ZLO client; the session writer and the source clone live in the `zlo`
//! binary.

use serde::{Deserialize, Serialize};

/// `POST` prepares a session and answers [`PrepareTriageResponse`].
pub const OPERATOR_TRIAGE_PATH: &str = "/api/operator/triage";
/// The file each session's playbook prompt is written to. A composer draft
/// is a triage draft while it links a file of this name.
pub const TRIAGE_PROMPT_FILE_NAME: &str = "triage-playbook.md";
/// Most characters of one [`TriageClient`] field the backend keeps.
pub const MAX_TRIAGE_CLIENT_FIELD_CHARS: usize = 80;

/// Which client the user pressed "Triage an issue" in. Free text written
/// into the session's context file, so the backend bounds and cleans each
/// field; none of it selects a path or a command.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageClient {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareTriageRequest {
    #[serde(default)]
    pub client: TriageClient,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareTriageResponse {
    /// The composer text: a sentence linking the prompt file as a file
    /// mention, ending in a space for the user's description.
    pub prompt: String,
    /// The backend's version, for the draft's machine line.
    pub server_version: String,
}
