//! Host-owned configuration. Computer use is off unless
//! `COMPUTER_USE_ENABLED=true`; the binary path comes from the host, never
//! from a tool call.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::ComputerUseError;

/// Host environment variables the server process inherits. Everything else is
/// scrubbed; the server talks to its own app agent, which owns the macOS
/// Accessibility and Screen Recording grants.
pub(crate) const INHERITED_ENV: &[&str] = &[
    "HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
];

#[derive(Debug, Clone)]
pub struct ComputerUseConfig {
    /// The `OpenComputerUse` executable.
    pub binary: PathBuf,
    /// Arguments that start it as an MCP server (`mcp`).
    pub args: Vec<String>,
    /// Extra environment for the server, on top of [`INHERITED_ENV`].
    pub env: Vec<(String, String)>,
    pub start_timeout: Duration,
    pub call_timeout: Duration,
    /// A session unused this long loses its process (it restarts on next use).
    pub idle_reap: Duration,
    /// After Stop, new actions on that thread are refused this long.
    pub stop_hold: Duration,
    /// Largest JSON-RPC line read from the server (screenshots are inline).
    pub max_message_bytes: usize,
    /// Largest base64 screenshot kept as a session frame.
    pub max_frame_bytes: usize,
    pub max_sessions: usize,
}

impl ComputerUseConfig {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            args: vec!["mcp".to_string()],
            env: Vec::new(),
            start_timeout: Duration::from_secs(20),
            call_timeout: Duration::from_secs(90),
            idle_reap: Duration::from_secs(15 * 60),
            stop_hold: Duration::from_secs(15),
            max_message_bytes: 32 * 1024 * 1024,
            max_frame_bytes: 16 * 1024 * 1024,
            max_sessions: 4,
        }
    }

    /// `None` when computer use is off (the default).
    pub fn from_env() -> Result<Option<Self>, ComputerUseError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub(crate) fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ComputerUseError> {
        let invalid = |reason: String| ComputerUseError::InvalidConfig { reason };
        let enabled = lookup("COMPUTER_USE_ENABLED").unwrap_or_default();
        match enabled.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "false" | "no" | "off" => return Ok(None),
            "1" | "true" | "yes" | "on" => {}
            other => return Err(invalid(format!("COMPUTER_USE_ENABLED={other}"))),
        }
        let binary = lookup("COMPUTER_USE_BINARY")
            .map(PathBuf::from)
            .ok_or_else(|| invalid("COMPUTER_USE_BINARY is required".to_string()))?;
        if !binary.is_absolute() || !binary.is_file() {
            return Err(invalid(
                "COMPUTER_USE_BINARY must be an absolute path to the OpenComputerUse executable"
                    .to_string(),
            ));
        }
        let mut config = Self::new(binary);
        if let Some(namespace) =
            lookup("COMPUTER_USE_AGENT_SOCKET_NAMESPACE").filter(|value| !value.trim().is_empty())
        {
            config.env.push((
                "OPEN_COMPUTER_USE_AGENT_SOCKET_NAMESPACE".to_string(),
                namespace.trim().to_string(),
            ));
        }
        if let Some(raw) = lookup("COMPUTER_USE_CALL_TIMEOUT_SECS") {
            let secs = raw
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|secs| (1..=600).contains(secs))
                .ok_or_else(|| invalid(format!("COMPUTER_USE_CALL_TIMEOUT_SECS={raw}")))?;
            config.call_timeout = Duration::from_secs(secs);
        }
        Ok(Some(config))
    }
}
