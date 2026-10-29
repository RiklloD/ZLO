//! Host-owned configuration. Computer use is on when the engine ships inside
//! ZLO.app (next to the backend) or `COMPUTER_USE_BINARY` names one, and
//! `COMPUTER_USE_ENABLED=false` turns it off. The binary path comes from the
//! host, never from a tool call.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::ComputerUseError;

/// Host environment variables the server process inherits. Everything else is
/// scrubbed.
pub(crate) const INHERITED_ENV: &[&str] = &[
    "HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
];

/// The engine bundled by `tools/ironclaw_gpui/bundle.sh`, relative to the
/// backend executable (`ZLO.app/Contents/MacOS/zlo-backend`).
const BUNDLED_ENGINE: &str = "../Helpers/ZLO Computer Use.app/Contents/MacOS/OpenComputerUse";

/// The engine runs in-process instead of proxying to its own app agent. Its
/// macOS permission checks then fall on the app that started the backend
/// (ZLO), so one grant to ZLO covers clicks, typing and screenshots.
const IN_PROCESS_ENV: (&str, &str) = ("OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY", "1");

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
    /// The patched fork (`Open Computer Use (Dev).app`): its extra tools, tree
    /// diffs and `OPEN_COMPUTER_USE_ACTION_*` switches.
    pub fork: bool,
    /// Screen-share the windows the agent drives to a viewer (on by default).
    pub live_preview: bool,
    /// One JSON line per tool call (tool, ok, code, seconds; never arguments).
    pub trace_path: Option<PathBuf>,
    /// Where each session's native-cursor observation file lives.
    pub cursor_dir: PathBuf,
}

impl ComputerUseConfig {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        let binary = binary.into();
        Self {
            fork: is_fork(&binary),
            live_preview: true,
            trace_path: None,
            cursor_dir: std::env::temp_dir().join("ironclaw-ocu"),
            binary,
            args: vec!["mcp".to_string()],
            env: vec![(IN_PROCESS_ENV.0.to_string(), IN_PROCESS_ENV.1.to_string())],
            start_timeout: Duration::from_secs(20),
            call_timeout: Duration::from_secs(90),
            idle_reap: Duration::from_secs(15 * 60),
            stop_hold: Duration::from_secs(15),
            max_message_bytes: 32 * 1024 * 1024,
            max_frame_bytes: 16 * 1024 * 1024,
            max_sessions: 4,
        }
    }

    /// `None` when computer use is off: no engine found, or turned off.
    pub fn from_env() -> Result<Option<Self>, ComputerUseError> {
        let bundled = std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.join(BUNDLED_ENGINE)))
            .filter(|path| path.is_file());
        Self::from_lookup(|name| std::env::var(name).ok(), bundled)
    }

    pub(crate) fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        bundled: Option<PathBuf>,
    ) -> Result<Option<Self>, ComputerUseError> {
        let invalid = |reason: String| ComputerUseError::InvalidConfig { reason };
        let enabled = lookup("COMPUTER_USE_ENABLED").unwrap_or_default();
        let required = match enabled.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "no" | "off" => return Ok(None),
            "" => false,
            "1" | "true" | "yes" | "on" => true,
            other => return Err(invalid(format!("COMPUTER_USE_ENABLED={other}"))),
        };
        let binary = match lookup("COMPUTER_USE_BINARY").filter(|v| !v.trim().is_empty()) {
            Some(raw) => {
                let binary = PathBuf::from(raw);
                if !binary.is_absolute() || !binary.is_file() {
                    return Err(invalid(
                        "COMPUTER_USE_BINARY must be an absolute path to the OpenComputerUse executable"
                            .to_string(),
                    ));
                }
                binary
            }
            None => match bundled {
                Some(binary) => binary,
                None if required => {
                    return Err(invalid(
                        "COMPUTER_USE_ENABLED=true but no engine is bundled and COMPUTER_USE_BINARY is unset"
                            .to_string(),
                    ));
                }
                None => return Ok(None),
            },
        };
        let mut config = Self::new(binary);
        if let Some(raw) = lookup("COMPUTER_USE_LIVE_PREVIEW") {
            config.live_preview = match raw.trim().to_ascii_lowercase().as_str() {
                "" | "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                other => return Err(invalid(format!("COMPUTER_USE_LIVE_PREVIEW={other}"))),
            };
        }
        config.trace_path = lookup("COMPUTER_USE_TRACE_FILE")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                lookup("HOME")
                    .map(|home| PathBuf::from(home).join(".ironclaw/logs/computer-use.jsonl"))
            });
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

/// The patched fork: the engine bundled into ZLO, or the Hermes build.
fn is_fork(binary: &std::path::Path) -> bool {
    let path = binary.to_string_lossy();
    path.contains("ZLO Computer Use.app")
        || path.contains("Numoh Computer Use.app")
        || path.contains("(Dev).app")
}
