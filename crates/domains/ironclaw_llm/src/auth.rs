//! Provider auth lookups used by provider resolution.
//!
//! - [`default_headers`]: provider-specific request headers (e.g. GitHub
//!   Copilot's editor-identity headers) merged into outbound requests.
//! - [`load_persisted_credentials`]: read credentials another CLI tool
//!   (e.g. Codex CLI) has already persisted.

use std::path::{Path, PathBuf};

use secrecy::SecretString;

use crate::codex_auth;
use crate::github_copilot_auth;

/// Identifies a backend for [`default_headers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthBackend {
    GithubCopilot,
    Gemini,
    OpenAiCodex,
}

/// Identifies a CLI-style credential file that another tool maintains and
/// `ironclaw_llm` can read directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// Codex CLI's `auth.json`. Loaded when the LLM config sees
    /// `LLM_USE_CODEX_AUTH=true`.
    CodexCli,
}

/// Credentials loaded from a CLI-style auth file (currently Codex CLI's
/// `auth.json`). The caller decides whether to use them as-is or override.
#[derive(Debug)]
pub struct PersistedCredentials {
    pub token: SecretString,
    pub refresh_token: Option<SecretString>,
    /// True when the credential is an OAuth subscription token (e.g. Codex
    /// ChatGPT mode); false for raw API keys. Affects routing/base-URL
    /// decisions on the caller side.
    pub is_subscription: bool,
    /// Provider-specific base URL to honour for this credential type.
    pub base_url: String,
    /// File the credentials were loaded from. Carried forward so the
    /// provider can persist refreshed tokens back to the same place.
    pub source_path: Option<PathBuf>,
}

/// Default request headers a backend wants on every API call.
pub fn default_headers(backend: AuthBackend) -> Vec<(String, String)> {
    match backend {
        AuthBackend::GithubCopilot => github_copilot_auth::default_headers(),
        AuthBackend::Gemini | AuthBackend::OpenAiCodex => Vec::new(),
    }
}

/// Load CLI-stored credentials. Returns `None` if the file is missing,
/// unreadable, or contains no usable credentials.
pub fn load_persisted_credentials(
    source: CredentialSource,
    override_path: Option<&Path>,
) -> Option<PersistedCredentials> {
    match source {
        CredentialSource::CodexCli => {
            let path = override_path
                .map(Path::to_path_buf)
                .unwrap_or_else(codex_auth::default_codex_auth_path);
            let creds = codex_auth::load_codex_credentials(&path)?;
            Some(PersistedCredentials {
                base_url: creds.base_url().to_string(),
                token: creds.token,
                refresh_token: creds.refresh_token,
                is_subscription: creds.is_chatgpt_mode,
                source_path: creds.auth_path,
            })
        }
    }
}
