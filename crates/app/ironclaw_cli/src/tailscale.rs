//! The one place `ironclaw` runs the Tailscale CLI.
//!
//! [`TailscaleCli`] is the seam: [`SystemTailscale`] spawns the real binary
//! (`tailscale` on `PATH`, else the macOS app's bundled binary), tests use a
//! scripted fake. Everything else here parses its JSON output and is pure.
//!
//! Only three commands ever run: `status --json`, `serve status --json`,
//! and `serve --bg --https=<port> <target>` / `serve --https=<port> off`.
//! Never `serve reset` (it would drop the user's other serve entries) and
//! never `funnel`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// Where the sandboxed macOS app keeps its CLI-capable binary.
pub(crate) const MACOS_APP_TAILSCALE: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

/// Captured output of one Tailscale CLI run.
#[derive(Debug, Clone)]
pub(crate) struct TailscaleOutput {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Runs the Tailscale CLI with the given arguments.
pub(crate) trait TailscaleCli {
    fn run(&self, args: &[&str]) -> anyhow::Result<TailscaleOutput>;
}

/// The real Tailscale CLI.
pub(crate) struct SystemTailscale {
    binary: PathBuf,
}

impl SystemTailscale {
    /// `tailscale` on `PATH`, else the macOS app binary. `None` when neither
    /// exists.
    pub(crate) fn locate() -> Option<Self> {
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        locate_binary(&path_var, Path::new(MACOS_APP_TAILSCALE)).map(|binary| Self { binary })
    }
}

impl TailscaleCli for SystemTailscale {
    fn run(&self, args: &[&str]) -> anyhow::Result<TailscaleOutput> {
        let output = Command::new(&self.binary)
            .args(args)
            .output()
            .map_err(|error| anyhow::anyhow!("could not run {}: {error}", self.binary.display()))?;
        Ok(TailscaleOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn locate_binary(path_var: &std::ffi::OsStr, app_binary: &Path) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join("tailscale"))
        .find(|candidate| candidate.is_file())
        .or_else(|| app_binary.is_file().then(|| app_binary.to_path_buf()))
}

/// What `remote enable` needs from `tailscale status --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TailnetSelf {
    pub(crate) backend_state: String,
    /// MagicDNS name without the trailing dot.
    pub(crate) dns_name: String,
    /// `User[Self.UserID].LoginName`.
    pub(crate) login_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StatusJson {
    backend_state: String,
    #[serde(rename = "Self")]
    self_node: Option<SelfJson>,
    #[serde(default)]
    user: BTreeMap<String, UserJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SelfJson {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "UserID", default)]
    user_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UserJson {
    #[serde(default)]
    login_name: Option<String>,
}

pub(crate) fn parse_status(json: &str) -> anyhow::Result<TailnetSelf> {
    let status: StatusJson = serde_json::from_str(json)
        .map_err(|error| anyhow::anyhow!("unexpected `tailscale status --json` output: {error}"))?;
    let self_node = status.self_node;
    let dns_name = self_node
        .as_ref()
        .map(|node| node.dns_name.trim_end_matches('.').to_string())
        .unwrap_or_default();
    let login_name = self_node
        .and_then(|node| node.user_id)
        .and_then(|id| status.user.get(&id.to_string()))
        .and_then(|user| user.login_name.clone())
        .filter(|login| !login.is_empty());
    Ok(TailnetSelf {
        backend_state: status.backend_state,
        dns_name,
        login_name,
    })
}

/// What `tailscale serve status --json` says about one HTTPS port.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ServePort {
    /// Funnel (public internet) is on for this port.
    pub(crate) funnel: bool,
    /// Something is configured on this port at all.
    pub(crate) in_use: bool,
    /// Proxy targets of the port's web handlers.
    pub(crate) proxies: Vec<String>,
}

impl ServePort {
    /// The port serves exactly `target` (ours) and nothing else.
    pub(crate) fn serves_only(&self, target: &str) -> bool {
        self.in_use && !self.proxies.is_empty() && self.proxies.iter().all(|p| p == target)
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct ServeConfigJson {
    #[serde(rename = "TCP", default)]
    tcp: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    web: BTreeMap<String, WebJson>,
    #[serde(default)]
    allow_funnel: BTreeMap<String, bool>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct WebJson {
    #[serde(default)]
    handlers: BTreeMap<String, HandlerJson>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct HandlerJson {
    #[serde(default)]
    proxy: Option<String>,
}

pub(crate) fn parse_serve_port(json: &str, port: u16) -> anyhow::Result<ServePort> {
    let config: ServeConfigJson = if json.trim().is_empty() {
        ServeConfigJson::default()
    } else {
        serde_json::from_str(json).map_err(|error| {
            anyhow::anyhow!("unexpected `tailscale serve status --json` output: {error}")
        })?
    };
    let suffix = format!(":{port}");
    let funnel = config
        .allow_funnel
        .iter()
        .any(|(host_port, on)| *on && host_port.ends_with(&suffix));
    let proxies: Vec<String> = config
        .web
        .iter()
        .filter(|(host_port, _)| host_port.ends_with(&suffix))
        .flat_map(|(_, web)| web.handlers.values().filter_map(|h| h.proxy.clone()))
        .collect();
    let in_use = config.tcp.contains_key(&port.to_string())
        || config
            .web
            .keys()
            .any(|host_port| host_port.ends_with(&suffix));
    Ok(ServePort {
        funnel,
        in_use,
        proxies,
    })
}

/// The loopback URL `tailscale serve` proxies to.
pub(crate) fn identity_target(identity_port: u16) -> String {
    format!("http://127.0.0.1:{identity_port}")
}

/// The HTTPS URL a MagicDNS name is served on.
pub(crate) fn tailnet_url(dns_name: &str, https_port: u16) -> String {
    if https_port == 443 {
        format!("https://{dns_name}")
    } else {
        format!("https://{dns_name}:{https_port}")
    }
}

pub(crate) fn status(cli: &dyn TailscaleCli) -> anyhow::Result<TailnetSelf> {
    let output = cli.run(&["status", "--json"])?;
    if output.stdout.trim().is_empty() {
        anyhow::bail!("`tailscale status --json` failed: {}", output.stderr.trim());
    }
    parse_status(&output.stdout)
}

pub(crate) fn serve_port(cli: &dyn TailscaleCli, port: u16) -> anyhow::Result<ServePort> {
    let output = cli.run(&["serve", "status", "--json"])?;
    if !output.success {
        anyhow::bail!(
            "`tailscale serve status --json` failed: {}",
            output.stderr.trim()
        );
    }
    parse_serve_port(&output.stdout, port)
}

pub(crate) fn serve_on(
    cli: &dyn TailscaleCli,
    https_port: u16,
    target: &str,
) -> anyhow::Result<()> {
    let https = format!("--https={https_port}");
    let output = cli.run(&["serve", "--bg", &https, target])?;
    if !output.success {
        anyhow::bail!("`tailscale serve` failed: {}", output.stderr.trim());
    }
    Ok(())
}

pub(crate) fn serve_off(cli: &dyn TailscaleCli, https_port: u16) -> anyhow::Result<()> {
    let https = format!("--https={https_port}");
    let output = cli.run(&["serve", &https, "off"])?;
    if !output.success {
        anyhow::bail!("`tailscale serve ... off` failed: {}", output.stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Status shaped like the Phase 0 notes (Tailscale 1.104.1 on macOS).
    pub(crate) const STATUS_RUNNING: &str = r#"{
      "Version": "1.104.1",
      "BackendState": "Running",
      "Self": {
        "ID": "nX1", "HostName": "Studio", "DNSName": "studio.tail9915dd.ts.net.",
        "OS": "macOS", "UserID": 1234567, "Online": true,
        "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"]
      },
      "Peer": {
        "nodekey:abc": { "DNSName": "air.tail9915dd.ts.net.", "OS": "macOS", "Online": false, "UserID": 1234567 }
      },
      "User": {
        "1234567": { "ID": 1234567, "LoginName": "owner@example.com", "DisplayName": "Owner" }
      },
      "MagicDNSSuffix": "tail9915dd.ts.net",
      "CertDomains": ["studio.tail9915dd.ts.net"]
    }"#;

    pub(crate) const SERVE_EMPTY: &str = "{}";

    /// Scripted CLI: answers by the first argument(s), records every call.
    pub(crate) struct FakeTailscale {
        pub(crate) status: String,
        pub(crate) serve_status: String,
        pub(crate) calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeTailscale {
        pub(crate) fn new(status: &str, serve_status: &str) -> Self {
            Self {
                status: status.to_string(),
                serve_status: serve_status.to_string(),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    impl TailscaleCli for FakeTailscale {
        fn run(&self, args: &[&str]) -> anyhow::Result<TailscaleOutput> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|a| a.to_string()).collect());
            let stdout = match args {
                ["status", "--json"] => self.status.clone(),
                ["serve", "status", "--json"] => self.serve_status.clone(),
                _ => String::new(),
            };
            Ok(TailscaleOutput {
                success: true,
                stdout,
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn status_yields_backend_state_dns_name_and_owner_login() {
        let parsed = parse_status(STATUS_RUNNING).expect("parses");
        assert_eq!(
            parsed,
            TailnetSelf {
                backend_state: "Running".into(),
                dns_name: "studio.tail9915dd.ts.net".into(),
                login_name: Some("owner@example.com".into()),
            }
        );
        let stopped = parse_status(r#"{"BackendState":"Stopped","Self":null,"User":null}"#);
        assert!(stopped.is_err() || stopped.is_ok_and(|s| s.login_name.is_none()));
        let needs_login = parse_status(r#"{"BackendState":"NeedsLogin"}"#).expect("parses");
        assert_eq!(needs_login.backend_state, "NeedsLogin");
        assert_eq!(needs_login.login_name, None);
        assert!(parse_status("not json").is_err());
    }

    #[test]
    fn serve_status_reports_funnel_foreign_and_own_handlers() {
        assert_eq!(
            parse_serve_port(SERVE_EMPTY, 443).expect("empty"),
            ServePort::default()
        );
        let ours = r#"{"TCP":{"443":{"HTTPS":true}},
            "Web":{"studio.tail9915dd.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3001"}}}}}"#;
        let port = parse_serve_port(ours, 443).expect("ours");
        assert!(port.in_use && !port.funnel);
        assert!(port.serves_only(&identity_target(3001)));
        assert!(!port.serves_only(&identity_target(4000)));
        assert!(!parse_serve_port(ours, 8443).expect("other").in_use);

        let funnel = r#"{"TCP":{"443":{"HTTPS":true}},
            "Web":{"studio.tail9915dd.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}},
            "AllowFunnel":{"studio.tail9915dd.ts.net:443":true}}"#;
        let port = parse_serve_port(funnel, 443).expect("funnel");
        assert!(port.funnel && port.in_use);
        assert!(!port.serves_only(&identity_target(3001)));

        let tcp_forward = r#"{"TCP":{"443":{"TCPForward":"127.0.0.1:22"}}}"#;
        let port = parse_serve_port(tcp_forward, 443).expect("tcp");
        assert!(port.in_use && port.proxies.is_empty());
    }

    #[test]
    fn urls_and_targets_are_loopback_and_https() {
        assert_eq!(identity_target(3001), "http://127.0.0.1:3001");
        assert_eq!(tailnet_url("a.ts.net", 443), "https://a.ts.net");
        assert_eq!(tailnet_url("a.ts.net", 8443), "https://a.ts.net:8443");
    }

    #[test]
    fn binary_lookup_prefers_path_then_app_bundle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = dir.path().join("Tailscale");
        assert_eq!(locate_binary(std::ffi::OsStr::new(""), &app), None);
        std::fs::write(&app, b"").expect("app");
        assert_eq!(
            locate_binary(std::ffi::OsStr::new(""), &app),
            Some(app.clone())
        );
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir(&bin_dir).expect("bin");
        std::fs::write(bin_dir.join("tailscale"), b"").expect("cli");
        assert_eq!(
            locate_binary(bin_dir.as_os_str(), &app),
            Some(bin_dir.join("tailscale"))
        );
    }
}
