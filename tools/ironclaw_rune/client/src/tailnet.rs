//! Finding IronClaw on the user's tailnet: `tailscale status --json` lists
//! the peers, and each online peer's descriptor is probed at
//! `https://<MagicDNS name>/.well-known/ironclaw/environment` (the
//! `tailscale serve` URL `ironclaw remote enable` sets up).
//!
//! On macOS the CLI is the app's own binary
//! (`/Applications/Tailscale.app/Contents/MacOS/Tailscale`), not on `PATH`.
//! Tailscale missing or stopped is not an error: discovery is simply empty.

use std::path::{Path, PathBuf};

use ironclaw_product_contracts::environment_access::EnvironmentDescriptor;
use serde::Deserialize;

use crate::environment_calls::{PROBE_TIMEOUT, fetch_descriptor};

const MAC_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

/// One machine on the tailnet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// MagicDNS name without the trailing dot (`studio.tail9915dd.ts.net`).
    pub dns_name: String,
    pub host_name: String,
    pub online: bool,
}

impl Peer {
    pub fn origin(&self) -> String {
        format!("https://{}", self.dns_name)
    }
}

/// An online peer that answered as an IronClaw environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub peer: Peer,
    pub descriptor: EnvironmentDescriptor,
}

/// What discovery learned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Every peer, online or not (offline ones mark saved computers asleep).
    pub peers: Vec<Peer>,
    pub found: Vec<Found>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    #[serde(default)]
    backend_state: String,
    #[serde(default)]
    peer: Option<std::collections::HashMap<String, StatusPeer>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StatusPeer {
    #[serde(default, rename = "DNSName")]
    dns_name: String,
    #[serde(default)]
    host_name: String,
    #[serde(default)]
    online: bool,
}

/// The peers of a `tailscale status --json` document; `None` when
/// Tailscale is not running (`BackendState` other than `Running`).
pub fn parse_status(json: &[u8]) -> Result<Option<Vec<Peer>>, serde_json::Error> {
    let status: Status = serde_json::from_slice(json)?;
    if status.backend_state != "Running" {
        return Ok(None);
    }
    let mut peers: Vec<Peer> = status
        .peer
        .unwrap_or_default()
        .into_values()
        .filter_map(|p| {
            let dns = p.dns_name.trim_end_matches('.').to_ascii_lowercase();
            (!dns.is_empty()).then_some(Peer {
                dns_name: dns,
                host_name: p.host_name,
                online: p.online,
            })
        })
        .collect();
    peers.sort_by(|a, b| a.dns_name.cmp(&b.dns_name));
    Ok(Some(peers))
}

/// `tailscale` on `PATH`, else the macOS app's binary.
pub fn find_cli() -> Option<PathBuf> {
    find_cli_in(std::env::var_os("PATH").as_deref(), Path::new(MAC_APP_CLI))
}

fn find_cli_in(path: Option<&std::ffi::OsStr>, app: &Path) -> Option<PathBuf> {
    if let Some(path) = path {
        for dir in std::env::split_paths(path) {
            let candidate = dir.join("tailscale");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    app.is_file().then(|| app.to_path_buf())
}

/// Run `tailscale status --json` (blocking: call off the UI thread).
/// `None` when Tailscale is missing, stopped, or its answer unreadable.
pub fn status_peers() -> Option<Vec<Peer>> {
    let cli = find_cli()?;
    let out = std::process::Command::new(&cli)
        .args(["status", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| tracing::debug!(error = %e, "tailscale status did not run"))
        .ok()?;
    // A stopped Tailscale exits non-zero but still prints its state.
    match parse_status(&out.stdout) {
        Ok(peers) => peers,
        Err(e) => {
            tracing::debug!(error = %e, "tailscale status unreadable");
            None
        }
    }
}

/// Peers probed at the same time.
const PROBE_CONCURRENCY: usize = 8;

/// Probe the online peers, [`PROBE_CONCURRENCY`] at a time, each within
/// [`PROBE_TIMEOUT`].
pub async fn probe(peers: Vec<Peer>) -> Discovery {
    use futures::StreamExt;
    let online: Vec<Peer> = peers.iter().filter(|p| p.online).cloned().collect();
    let probes: Vec<_> = online
        .into_iter()
        .map(|peer| async move {
            match fetch_descriptor(&peer.origin(), PROBE_TIMEOUT).await {
                Ok(descriptor) => Some(Found { peer, descriptor }),
                Err(_) => None, // silent-ok: a peer without IronClaw is simply not listed
            }
        })
        .collect();
    let found = futures::stream::iter(probes)
        .buffered(PROBE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flatten()
        .collect();
    Discovery { peers, found }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like the dev Mac's output (plan, Phase 0 item 4).
    const STATUS: &str = r#"{
      "Version": "1.104.1",
      "BackendState": "Running",
      "MagicDNSSuffix": "tail9915dd.ts.net",
      "CertDomains": ["loriks-macbook-pro.tail9915dd.ts.net"],
      "Self": {"DNSName": "loriks-macbook-pro.tail9915dd.ts.net.", "HostName": "Loriks-MacBook-Pro",
               "OS": "macOS", "Online": true, "UserID": 1, "TailscaleIPs": ["100.101.2.3"]},
      "Peer": {
        "nodekey:aa": {"DNSName": "studio.tail9915dd.ts.net.", "HostName": "studio", "OS": "macOS", "Online": true, "UserID": 1},
        "nodekey:bb": {"DNSName": "mini.tail9915dd.ts.net.", "HostName": "mini", "OS": "linux", "Online": false, "UserID": 1},
        "nodekey:cc": {"DNSName": "", "HostName": "phone", "OS": "iOS", "Online": true, "UserID": 1}
      },
      "User": {"1": {"LoginName": "rikllodal@gmail.com"}}
    }"#;

    #[test]
    fn status_lists_peers_with_the_trailing_dot_stripped() {
        let peers = parse_status(STATUS.as_bytes()).unwrap().unwrap();
        assert_eq!(
            peers,
            vec![
                Peer {
                    dns_name: "mini.tail9915dd.ts.net".into(),
                    host_name: "mini".into(),
                    online: false
                },
                Peer {
                    dns_name: "studio.tail9915dd.ts.net".into(),
                    host_name: "studio".into(),
                    online: true
                },
            ]
        );
        assert_eq!(peers[1].origin(), "https://studio.tail9915dd.ts.net");
    }

    #[test]
    fn a_stopped_tailscale_has_no_peers() {
        let stopped = r#"{"BackendState": "Stopped", "Peer": null}"#;
        assert_eq!(parse_status(stopped.as_bytes()).unwrap(), None);
        let no_peers = r#"{"BackendState": "Running"}"#;
        assert_eq!(parse_status(no_peers.as_bytes()).unwrap(), Some(vec![]));
        assert!(parse_status(b"not json").is_err());
    }

    #[test]
    fn the_cli_is_found_on_path_before_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Tailscale");
        assert_eq!(find_cli_in(None, &app), None);
        std::fs::write(&app, b"").unwrap();
        assert_eq!(find_cli_in(None, &app), Some(app.clone()));
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("tailscale"), b"").unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        assert_eq!(find_cli_in(Some(&path), &app), Some(bin.join("tailscale")));
    }

    #[tokio::test]
    async fn offline_peers_are_never_probed() {
        let peers = vec![Peer {
            dns_name: "mini.invalid".into(),
            host_name: "mini".into(),
            online: false,
        }];
        let d = probe(peers.clone()).await;
        assert_eq!(d.peers, peers);
        assert!(d.found.is_empty());
    }
}
