//! `zlo remote enable|status|disable` — reach this backend from the
//! owner's other machines over their tailnet.
//!
//! `enable` checks Tailscale, refuses Funnel and a port someone else
//! serves, records the owner's tailnet login, writes `[remote]`, and runs
//! `tailscale serve --bg --https=<port> http://127.0.0.1:<identity_port>`.
//! `disable` turns off only that one port (`serve --https=<port> off`, never
//! `serve reset`) and sets `enabled = false`. The backend itself stays on
//! loopback; `zlo serve` starts the identity listener on its next
//! start.

use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Args, Subcommand};
use ironclaw_config::{
    DEFAULT_REMOTE_HTTPS_PORT, DEFAULT_REMOTE_IDENTITY_PORT, RebornConfigFile, RemoteSection,
    write_remote_section,
};
use serde::Serialize;

use crate::context::RebornCliContext;
use crate::tailscale::{self, SystemTailscale, TailscaleCli};

#[derive(Debug, Args)]
pub(crate) struct RemoteCommand {
    #[command(subcommand)]
    action: RemoteAction,
}

#[derive(Debug, Subcommand)]
enum RemoteAction {
    /// Publish this backend on your tailnet with `tailscale serve` (never Funnel).
    Enable {
        /// HTTPS port `tailscale serve` publishes.
        #[arg(long, default_value_t = DEFAULT_REMOTE_HTTPS_PORT)]
        https_port: u16,
        /// Allow one-click pairing even though other people have login
        /// accounts on this machine.
        #[arg(long)]
        trust_local_users: bool,
        /// Output `{"tailnet_url", "one_click", "restart_required"}` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the remote-access configuration and serve state.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Stop publishing this backend on the tailnet.
    Disable,
}

impl RemoteCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        let config_path = context.boot_config().home().config_file_path();
        match self.action {
            RemoteAction::Enable {
                https_port,
                trust_local_users,
                json,
            } => {
                let cli = locate()?;
                let other_accounts = crate::host_info::detect_other_login_accounts()?;
                let outcome = enable(
                    &cli,
                    &config_path,
                    EnableOptions {
                        https_port,
                        trust_local_users,
                    },
                    &other_accounts,
                    identity_port_is_listening,
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                } else {
                    println!("Remote access enabled: {}", outcome.tailnet_url);
                    if outcome.one_click {
                        println!("One-click pairing is on for your tailnet login.");
                    } else if !other_accounts.is_empty() {
                        println!(
                            "One-click pairing is off: other login accounts exist on this machine \
                             ({}). Pair with `zlo pair`, or re-run with --trust-local-users.",
                            other_accounts.join(", ")
                        );
                    }
                    if outcome.restart_required {
                        println!("Restart `zlo serve` to start the identity listener.");
                    }
                }
                Ok(())
            }
            RemoteAction::Status { json } => {
                let cli = SystemTailscale::locate();
                let status = status(
                    cli.as_ref().map(|cli| cli as &dyn TailscaleCli),
                    &config_path,
                    &context,
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                } else {
                    print_status(&status);
                }
                Ok(())
            }
            RemoteAction::Disable => {
                let cli = locate()?;
                let turned_off = disable(&cli, &config_path)?;
                if turned_off {
                    println!("Remote access disabled; the tailnet port is no longer served.");
                } else {
                    println!("Remote access disabled.");
                }
                println!("Restart `zlo serve` to stop the identity listener.");
                Ok(())
            }
        }
    }
}

fn locate() -> anyhow::Result<SystemTailscale> {
    SystemTailscale::locate().ok_or_else(|| {
        anyhow::anyhow!(
            "Tailscale is not installed: no `tailscale` on PATH and no {}",
            tailscale::MACOS_APP_TAILSCALE
        )
    })
}

fn identity_port_is_listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_millis(300),
    )
    .is_ok()
}

struct EnableOptions {
    https_port: u16,
    trust_local_users: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct EnableOutcome {
    tailnet_url: String,
    one_click: bool,
    restart_required: bool,
}

fn load_remote(config_path: &Path) -> anyhow::Result<RemoteSection> {
    Ok(RebornConfigFile::load(config_path)
        .map_err(anyhow::Error::from)?
        .and_then(|file| file.remote)
        .unwrap_or_default())
}

fn enable(
    cli: &dyn TailscaleCli,
    config_path: &Path,
    options: EnableOptions,
    other_accounts: &[String],
    identity_listening: impl Fn(u16) -> bool,
) -> anyhow::Result<EnableOutcome> {
    if options.https_port == 0 {
        anyhow::bail!("--https-port must be greater than 0");
    }
    let me = tailscale::status(cli)?;
    if me.backend_state != "Running" {
        anyhow::bail!(
            "Tailscale is not running (state: {}). Start Tailscale and sign in, then retry.",
            me.backend_state
        );
    }
    if me.dns_name.is_empty() {
        anyhow::bail!("Tailscale reports no MagicDNS name for this machine; enable MagicDNS");
    }
    let owner = me.login_name.ok_or_else(|| {
        anyhow::anyhow!("Tailscale reports no user login for this machine (tagged node?)")
    })?;

    let existing = load_remote(config_path)?;
    let identity_port = existing
        .identity_port
        .unwrap_or(DEFAULT_REMOTE_IDENTITY_PORT);
    let target = tailscale::identity_target(identity_port);
    let port = tailscale::serve_port(cli, options.https_port)?;
    if port.funnel {
        anyhow::bail!(
            "Funnel is on for HTTPS port {}; IronClaw never publishes to the internet. Turn \
             Funnel off for that port or choose another --https-port.",
            options.https_port
        );
    }
    let already_ours = port.serves_only(&target);
    if port.in_use && !already_ours {
        anyhow::bail!(
            "HTTPS port {} is already served by something else on this machine; choose another \
             --https-port",
            options.https_port
        );
    }

    let tailnet_url = tailscale::tailnet_url(&me.dns_name, options.https_port);
    let section = RemoteSection {
        enabled: Some(true),
        identity_port: Some(identity_port),
        owner_logins: Some(vec![owner]),
        trust_local_users: Some(options.trust_local_users),
        tailnet_url: Some(tailnet_url.clone()),
        https_port: Some(options.https_port),
    };
    if !already_ours {
        tailscale::serve_on(cli, options.https_port, &target)?;
    }
    if let Err(error) = write_remote_section(config_path, &section) {
        // Do not leave the tailnet port published without the config that
        // says so: undo our own serve entry, then report the write failure.
        if !already_ours && let Err(undo) = tailscale::serve_off(cli, options.https_port) {
            return Err(anyhow::Error::from(error)
                .context(format!("and undoing `tailscale serve` failed too: {undo}")));
        }
        return Err(anyhow::Error::from(error).context("could not write [remote] to config.toml"));
    }
    let settings = section
        .resolve()
        .context("[remote] did not resolve after enabling")?;
    Ok(EnableOutcome {
        tailnet_url,
        one_click: settings.one_click(!other_accounts.is_empty()),
        restart_required: !identity_listening(identity_port),
    })
}

/// Turn off our serve entry (only when the port still points at our
/// identity target) and set `enabled = false`. Returns whether a serve
/// entry was removed.
fn disable(cli: &dyn TailscaleCli, config_path: &Path) -> anyhow::Result<bool> {
    let existing = load_remote(config_path)?;
    let https_port = existing.https_port.unwrap_or(DEFAULT_REMOTE_HTTPS_PORT);
    let target = tailscale::identity_target(
        existing
            .identity_port
            .unwrap_or(DEFAULT_REMOTE_IDENTITY_PORT),
    );
    let port = tailscale::serve_port(cli, https_port)?;
    let ours = port.serves_only(&target);
    if ours {
        tailscale::serve_off(cli, https_port)?;
    }
    write_remote_section(
        config_path,
        &RemoteSection {
            enabled: Some(false),
            ..existing
        },
    )
    .context("could not write [remote] to config.toml")?;
    Ok(ours)
}

#[derive(Debug, Serialize)]
struct RemoteStatus {
    enabled: bool,
    tailnet_url: Option<String>,
    owner_logins: Vec<String>,
    identity_port: u16,
    https_port: u16,
    /// `serving`, `not_serving`, `served_by_other`, `funnel_on`, or
    /// `tailscale_missing`.
    serve_state: &'static str,
    /// Paired devices that are not revoked, when the backend answered.
    active_devices: Option<usize>,
}

fn status(
    cli: Option<&dyn TailscaleCli>,
    config_path: &Path,
    context: &RebornCliContext,
) -> anyhow::Result<RemoteStatus> {
    let section = load_remote(config_path)?;
    let identity_port = section
        .identity_port
        .unwrap_or(DEFAULT_REMOTE_IDENTITY_PORT);
    let https_port = section.https_port.unwrap_or(DEFAULT_REMOTE_HTTPS_PORT);
    let serve_state = match cli {
        None => "tailscale_missing",
        Some(cli) => {
            let port = tailscale::serve_port(cli, https_port)?;
            serve_state(&port, &tailscale::identity_target(identity_port))
        }
    };
    let active_devices = crate::backend_client::RunningBackend::from_context(context)
        .ok()
        .and_then(|backend| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime.block_on(backend.list_devices()).ok()
        })
        .map(|list| {
            list.devices
                .iter()
                .filter(|d| d.revoked_at.is_none())
                .count()
        }); // silent-ok: device count is optional context; the backend may simply not be running
    Ok(RemoteStatus {
        enabled: section.enabled == Some(true),
        tailnet_url: section.tailnet_url,
        owner_logins: section.owner_logins.unwrap_or_default(),
        identity_port,
        https_port,
        serve_state,
        active_devices,
    })
}

fn serve_state(port: &tailscale::ServePort, target: &str) -> &'static str {
    if port.funnel {
        "funnel_on"
    } else if port.serves_only(target) {
        "serving"
    } else if port.in_use {
        "served_by_other"
    } else {
        "not_serving"
    }
}

fn print_status(status: &RemoteStatus) {
    println!(
        "Remote access: {}",
        if status.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    if let Some(url) = &status.tailnet_url {
        println!("Tailnet URL:   {url}");
    }
    if !status.owner_logins.is_empty() {
        println!("Owner:         {}", status.owner_logins.join(", "));
    }
    println!(
        "Serve:         {} (https {} -> 127.0.0.1:{})",
        status.serve_state, status.https_port, status.identity_port
    );
    match status.active_devices {
        Some(count) => println!("Devices:       {count} paired"),
        None => println!("Devices:       backend not reachable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tailscale::tests::{FakeTailscale, SERVE_EMPTY, STATUS_RUNNING};

    fn config(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("config.toml")
    }

    fn options() -> EnableOptions {
        EnableOptions {
            https_port: 443,
            trust_local_users: false,
        }
    }

    #[test]
    fn enable_records_the_owner_writes_remote_and_serves_the_identity_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeTailscale::new(STATUS_RUNNING, SERVE_EMPTY);
        let outcome = enable(&fake, &config(&dir), options(), &[], |_| false).expect("enable");
        assert_eq!(
            outcome,
            EnableOutcome {
                tailnet_url: "https://studio.tail9915dd.ts.net".into(),
                one_click: true,
                restart_required: true,
            }
        );
        let calls = fake.calls();
        assert!(
            calls.contains(&vec![
                "serve".into(),
                "--bg".into(),
                "--https=443".into(),
                "http://127.0.0.1:3001".into()
            ]),
            "{calls:?}"
        );
        assert!(
            calls.iter().all(|call| !call.contains(&"reset".to_string())
                && !call.contains(&"funnel".to_string())),
            "{calls:?}"
        );
        let remote = load_remote(&config(&dir)).expect("load");
        assert_eq!(remote.enabled, Some(true));
        assert_eq!(
            remote.owner_logins,
            Some(vec!["owner@example.com".to_string()])
        );
        assert_eq!(remote.identity_port, Some(3001));
    }

    #[test]
    fn other_local_accounts_turn_one_click_off_unless_trusted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let others = vec!["guest".to_string()];
        let fake = FakeTailscale::new(STATUS_RUNNING, SERVE_EMPTY);
        let off = enable(&fake, &config(&dir), options(), &others, |_| true).expect("enable");
        assert!(!off.one_click);
        assert!(!off.restart_required, "identity listener already up");
        let trusted = enable(
            &FakeTailscale::new(STATUS_RUNNING, SERVE_EMPTY),
            &config(&dir),
            EnableOptions {
                https_port: 443,
                trust_local_users: true,
            },
            &others,
            |_| true,
        )
        .expect("enable");
        assert!(trusted.one_click);
    }

    #[test]
    fn enable_refuses_funnel_a_foreign_port_and_a_stopped_tailscale() {
        let dir = tempfile::tempdir().expect("tempdir");
        let funnel = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"studio.tail9915dd.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3001"}}}},"AllowFunnel":{"studio.tail9915dd.ts.net:443":true}}"#;
        let error = enable(
            &FakeTailscale::new(STATUS_RUNNING, funnel),
            &config(&dir),
            options(),
            &[],
            |_| false,
        )
        .expect_err("funnel");
        assert!(error.to_string().contains("Funnel"), "{error}");

        let foreign = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"studio.tail9915dd.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        let fake = FakeTailscale::new(STATUS_RUNNING, foreign);
        let error = enable(&fake, &config(&dir), options(), &[], |_| false).expect_err("foreign");
        assert!(error.to_string().contains("already served"), "{error}");
        assert!(
            fake.calls()
                .iter()
                .all(|call| call.first().map(String::as_str) != Some("serve")
                    || call.get(1).map(String::as_str) == Some("status")),
            "nothing was served: {:?}",
            fake.calls()
        );

        let stopped = r#"{"BackendState":"Stopped"}"#;
        let error = enable(
            &FakeTailscale::new(stopped, SERVE_EMPTY),
            &config(&dir),
            options(),
            &[],
            |_| false,
        )
        .expect_err("stopped");
        assert!(error.to_string().contains("not running"), "{error}");
        assert!(!config(&dir).exists(), "no config written on refusal");
    }

    #[test]
    fn enable_is_idempotent_when_the_port_already_serves_us() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ours = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"studio.tail9915dd.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3001"}}}}}"#;
        let fake = FakeTailscale::new(STATUS_RUNNING, ours);
        enable(&fake, &config(&dir), options(), &[], |_| false).expect("enable");
        assert!(
            !fake
                .calls()
                .iter()
                .any(|call| call.contains(&"--bg".to_string())),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    fn disable_turns_off_only_our_port_and_never_resets() {
        let dir = tempfile::tempdir().expect("tempdir");
        enable(
            &FakeTailscale::new(STATUS_RUNNING, SERVE_EMPTY),
            &config(&dir),
            options(),
            &[],
            |_| false,
        )
        .expect("enable");
        let ours = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"studio.tail9915dd.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3001"}}}}}"#;
        let fake = FakeTailscale::new(STATUS_RUNNING, ours);
        assert!(disable(&fake, &config(&dir)).expect("disable"));
        assert!(
            fake.calls()
                .contains(&vec!["serve".into(), "--https=443".into(), "off".into()])
        );
        assert!(
            fake.calls()
                .iter()
                .all(|call| !call.contains(&"reset".to_string()))
        );
        assert_eq!(
            load_remote(&config(&dir)).expect("load").enabled,
            Some(false)
        );

        // Someone else took the port since: leave it alone.
        let foreign = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"x:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        let fake = FakeTailscale::new(STATUS_RUNNING, foreign);
        assert!(!disable(&fake, &config(&dir)).expect("disable"));
        assert!(
            !fake
                .calls()
                .iter()
                .any(|call| call.contains(&"off".to_string()))
        );
    }

    #[test]
    fn serve_state_names_each_case() {
        let target = "http://127.0.0.1:3001";
        let parse = |json: &str| tailscale::parse_serve_port(json, 443).expect("parse");
        assert_eq!(serve_state(&parse("{}"), target), "not_serving");
        assert_eq!(
            serve_state(
                &parse(
                    r#"{"TCP":{"443":{}},"Web":{"h:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3001"}}}}}"#
                ),
                target
            ),
            "serving"
        );
        assert_eq!(
            serve_state(&parse(r#"{"TCP":{"443":{}}}"#), target),
            "served_by_other"
        );
        assert_eq!(
            serve_state(&parse(r#"{"AllowFunnel":{"h:443":true}}"#), target),
            "funnel_on"
        );
    }
}
