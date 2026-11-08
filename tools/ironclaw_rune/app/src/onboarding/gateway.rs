//! The computers ZLO can work on (t3code's environments): this Mac's own
//! gateway, found the way `ironclaw serve` writes it
//! (`Connection::discover`), or another computer's, added from setup's
//! "Add a computer" with a pairing link `http://host:3000#token=…`.
//!
//! ZLO talks to one gateway at a time. A paired computer is saved in
//! `~/Library/Application Support/ZLO/gateway.json`, private to the user
//! (0600, like the gateway's own `webui-token`), and used until "This Mac"
//! is picked again, which deletes the file. `IRONCLAW_DESKTOP_URL` still
//! wins over both, as before.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use zlo_client::{Client, ClientError, Connection, Token};
use serde::{Deserialize, Serialize};

/// The command a person runs on the other computer for its link: its
/// Tailscale address, where plain http stays inside the tailnet's
/// WireGuard tunnel (any other address must be https, `parse_link`).
pub const LINK_COMMAND: &str = "echo \"http://$(tailscale ip -4 | head -n 1):3000#token=$(cat ~/.ironclaw/reborn/webui-token)\"";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Computer {
    #[default]
    ThisMac,
    /// A paired gateway by its origin (`http://studio.local:3000`).
    Remote(String),
}

impl Computer {
    pub fn label(&self) -> String {
        match self {
            Computer::ThisMac => "This Mac".into(),
            Computer::Remote(url) => host_of(url).to_string(),
        }
    }

    pub fn key(&self) -> String {
        match self {
            Computer::ThisMac => "this-mac".into(),
            Computer::Remote(url) => format!("remote-{url}"),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    url: String,
    token: String,
}

fn file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("ZLO").join("gateway.json"))
}

fn saved() -> Option<Saved> {
    let path = file()?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(error = %e, "paired computer unreadable; using this Mac");
            return None;
        }
    };
    match serde_json::from_slice::<Saved>(&bytes) {
        Ok(saved) => Some(saved),
        Err(e) => {
            tracing::warn!(error = %e, "paired computer unreadable; using this Mac");
            None
        }
    }
}

/// The paired computer, if one is saved. `IRONCLAW_DESKTOP_URL` decides
/// the gateway on its own, so then there is none to offer.
pub fn paired() -> Option<Computer> {
    if env_override() {
        return None;
    }
    saved().map(|s| Computer::Remote(s.url))
}

fn env_override() -> bool {
    std::env::var("IRONCLAW_DESKTOP_URL").is_ok_and(|v| !v.trim().is_empty())
}

/// What `workspace::connect` connects with, and which computer that is.
pub fn connection() -> Result<(Connection, Computer), ClientError> {
    if !env_override()
        && let Some(saved) = saved()
    {
        let conn = Connection {
            base_url: saved.url.clone(),
            token: Token::new(saved.token)?,
            token_source: "pairing link".into(),
        };
        return Ok((conn, Computer::Remote(saved.url)));
    }
    Ok((Connection::discover()?, Computer::ThisMac))
}

/// A pairing link split into the gateway origin and its token.
pub fn parse_link(link: &str) -> Result<(String, String), String> {
    let link = link.trim();
    let (base, fragment) = link
        .split_once('#')
        .ok_or("The link has no token. It ends in #token=…")?;
    let token = fragment
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("The link has no token. It ends in #token=…")?;
    let (scheme, rest) = base
        .split_once("://")
        .ok_or("Paste the whole link, starting with http:// or https://.")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err("Paste the whole link, starting with http:// or https://.".into());
    }
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || authority.contains(char::is_whitespace) {
        return Err("The link's address is not a computer's address.".into());
    }
    // The token opens everything the gateway can do: over plain http it
    // may only travel where the network itself is private.
    if scheme == "http" && !private_route(host_of(authority)) {
        return Err(
            "Plain http would send the token unencrypted. Use the computer's Tailscale address, or https."
                .into(),
        );
    }
    Ok((format!("{scheme}://{authority}"), token.to_string()))
}

/// Loopback, or a Tailscale address or name (WireGuard-encrypted).
fn private_route(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".ts.net") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            let [a, b, ..] = ip.octets();
            // 100.64.0.0/10: Tailscale's CGNAT range.
            ip.is_loopback() || (a == 100 && (64..128).contains(&b))
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            // fd7a:115c:a1e0::/48: Tailscale's ULA range.
            let s = ip.segments();
            ip.is_loopback() || (s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0)
        }
        Err(_) => false,
    }
}

/// `http://studio.local:3000` → `studio.local`.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    if let Some(v6) = rest.strip_prefix('[') {
        return v6.split(']').next().unwrap_or(rest);
    }
    rest.split(':').next().unwrap_or(rest)
}

/// Check a link reaches a gateway that takes its token (on the network
/// runtime of a throwaway backend), then save it as the paired computer.
pub async fn pair(url: String, token: String) -> Result<Computer, String> {
    let conn = Connection {
        base_url: url.clone(),
        token: Token::new(token.clone()).map_err(|e| e.to_string())?,
        token_source: "pairing link".into(),
    };
    let client = Client::new(conn).map_err(|e| e.to_string())?;
    match client.session().await {
        Ok(_) => {}
        Err(e) if e.is_unauthorized() => {
            return Err("That computer turned the token down. Copy a fresh link.".into());
        }
        Err(ClientError::Network(_)) => {
            return Err(format!(
                "Could not reach {}. Check that IronClaw runs there and listens on its network address.",
                host_of(&url)
            ));
        }
        Err(e) => return Err(e.to_string()),
    }
    save(&url, &token).map_err(|e| {
        tracing::warn!(error = %e, "paired computer not saved");
        "The computer could not be saved. Try again.".to_string()
    })?;
    Ok(Computer::Remote(url))
}

fn save(url: &str, token: &str) -> std::io::Result<()> {
    let path = file().ok_or_else(|| std::io::Error::other("no settings folder"))?;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no settings folder"))?;
    std::fs::create_dir_all(dir)?;
    let body = serde_json::to_vec(&Saved {
        url: url.into(),
        token: token.into(),
    })
    .map_err(std::io::Error::other)?;
    // Private from the first byte, then renamed over the old file.
    let tmp = dir.join(format!(".gateway.json.tmp-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    let written = f
        .write_all(&body)
        .and_then(|()| f.sync_all())
        .and_then(|()| std::fs::rename(&tmp, &path));
    if written.is_err() {
        // The temp file holds the token: never leave it behind.
        let _ = std::fs::remove_file(&tmp); // silent-ok: best-effort cleanup; the write error is returned
    }
    written
}

/// Back to this Mac: forget the paired computer.
pub fn forget() -> std::io::Result<()> {
    let Some(path) = file() else {
        return Ok(());
    };
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_links_split_into_origin_and_token() {
        assert_eq!(
            parse_link(" http://100.101.2.3:3000#token=abc123 "),
            Ok(("http://100.101.2.3:3000".into(), "abc123".into()))
        );
        assert!(parse_link("http://studio.tail1234.ts.net:3000#token=a").is_ok());
        assert!(parse_link("http://localhost:3000#token=a").is_ok());
        assert!(parse_link("http://[fd7a:115c:a1e0::1]:3000#token=a").is_ok());
        // The LAN in the clear: refused; https is fine.
        assert!(parse_link("http://studio.local:3000#token=a").is_err());
        assert!(parse_link("http://192.168.1.4:3000#token=a").is_err());
        assert!(parse_link("http://100.128.0.1:3000#token=a").is_err());
        assert!(parse_link("https://studio.local#token=a").is_ok());
        assert_eq!(
            parse_link("HTTPS://box.tail1234.ts.net/pair?x=1#a=b&token=t0k"),
            Ok(("https://box.tail1234.ts.net".into(), "t0k".into()))
        );
        assert!(parse_link("http://studio.local:3000").is_err(), "no token");
        assert!(parse_link("http://studio.local:3000#token=").is_err());
        assert!(parse_link("ftp://x#token=a").is_err());
        assert!(parse_link("studio.local:3000#token=a").is_err());
        assert!(parse_link("http://user@evil#token=a").is_err());
    }

    #[test]
    fn computers_are_named_by_their_host() {
        assert_eq!(host_of("http://studio.local:3000"), "studio.local");
        assert_eq!(host_of("http://[::1]:3000"), "::1");
        assert_eq!(Computer::ThisMac.label(), "This Mac");
        assert_eq!(Computer::Remote("http://box:3000".into()).label(), "box");
    }
}
