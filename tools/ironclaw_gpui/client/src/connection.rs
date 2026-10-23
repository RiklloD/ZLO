//! Finds the local gateway and its bearer token the same way `ironclaw serve`
//! resolves them, so the desktop app connects with zero configuration.
//!
//! Precedence, highest first:
//! - URL: `IRONCLAW_DESKTOP_URL`, else `http://{listen_host}:{listen_port}`
//!   from `$IRONCLAW_REBORN_HOME/config.toml` `[webui]`, else
//!   `http://127.0.0.1:3000`.
//! - Token: the env var named by `[webui] env_token_var` (default
//!   `IRONCLAW_REBORN_WEBUI_TOKEN`), else `$IRONCLAW_REBORN_HOME/webui-token`
//!   (written by `ironclaw onboard`).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::ClientError;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 3000;
const DEFAULT_TOKEN_VAR: &str = "IRONCLAW_REBORN_WEBUI_TOKEN";
const TOKEN_FILENAME: &str = "webui-token";
/// Same cap the CLI applies before trusting the file.
const TOKEN_FILE_MAX_BYTES: u64 = 4096;

/// Bearer token. Debug output is redacted so it never lands in logs.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn new(raw: impl Into<String>) -> Result<Self, ClientError> {
        let raw = raw.into().trim().to_string();
        if raw.is_empty() {
            return Err(ClientError::Config("the WebUI token is empty".into()));
        }
        Ok(Self(raw))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct Connection {
    /// Gateway origin without a trailing slash, e.g. `http://127.0.0.1:3000`.
    pub base_url: String,
    pub token: Token,
    /// Where the token came from, shown on the connect screen.
    pub token_source: String,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    webui: WebuiSection,
}

#[derive(Debug, Default, Deserialize)]
struct WebuiSection {
    listen_host: Option<String>,
    listen_port: Option<u16>,
    env_token_var: Option<String>,
}

/// `$IRONCLAW_REBORN_HOME`, else `~/.ironclaw/reborn`.
pub fn reborn_home() -> Result<PathBuf, ClientError> {
    if let Some(home) = std::env::var_os("IRONCLAW_REBORN_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ClientError::Config("HOME is not set".into()))?;
    Ok(PathBuf::from(home).join(".ironclaw").join("reborn"))
}

impl Connection {
    /// Resolve the local gateway from env + the IronClaw home directory.
    pub fn discover() -> Result<Self, ClientError> {
        let home = reborn_home()?;
        let config = read_config(&home.join("config.toml"))?;

        let base_url = match std::env::var("IRONCLAW_DESKTOP_URL") {
            Ok(url) if !url.trim().is_empty() => url.trim().trim_end_matches('/').to_string(),
            _ => {
                let host = config.webui.listen_host.as_deref().unwrap_or(DEFAULT_HOST);
                // A wildcard bind is reachable on loopback.
                let host = match host {
                    "0.0.0.0" | "::" | "[::]" => DEFAULT_HOST,
                    other => other,
                };
                let port = config.webui.listen_port.unwrap_or(DEFAULT_PORT);
                format!("http://{host}:{port}")
            }
        };

        let token_var = config
            .webui
            .env_token_var
            .as_deref()
            .unwrap_or(DEFAULT_TOKEN_VAR);
        if let Ok(value) = std::env::var(token_var)
            && !value.trim().is_empty()
        {
            return Ok(Self {
                base_url,
                token: Token::new(value)?,
                token_source: format!("${token_var}"),
            });
        }

        let token_path = home.join(TOKEN_FILENAME);
        let token = read_token_file(&token_path)?;
        Ok(Self {
            base_url,
            token,
            token_source: token_path.display().to_string(),
        })
    }
}

fn read_config(path: &Path) -> Result<ConfigFile, ClientError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .map_err(|e| ClientError::Config(format!("cannot parse {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFile::default()),
        Err(e) => Err(ClientError::Config(format!(
            "cannot read {}: {e}",
            path.display()
        ))),
    }
}

fn read_token_file(path: &Path) -> Result<Token, ClientError> {
    let meta = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ClientError::Config(format!(
                "no WebUI token at {}. Run `ironclaw onboard` first.",
                path.display()
            ))
        } else {
            ClientError::Config(format!("cannot read {}: {e}", path.display()))
        }
    })?;
    if meta.len() > TOKEN_FILE_MAX_BYTES {
        return Err(ClientError::Config(format!(
            "{} is larger than {TOKEN_FILE_MAX_BYTES} bytes",
            path.display()
        )));
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| ClientError::Config(format!("cannot read {}: {e}", path.display())))?;
    Token::new(raw)
}
