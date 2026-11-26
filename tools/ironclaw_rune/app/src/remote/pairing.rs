//! What Settings → Computers (and setup's Connect step) do: find IronClaw
//! on the tailnet, pair a computer (one click, a code, a link, or over SSH
//! with `user@host`), remove one,
//! the "Run IronClaw on this Mac" switch, and for the active computer its
//! pairing codes, paired devices and remote access.
//!
//! Gateway calls on the active computer go through `jobs::run`, so a reply
//! that lands after a switch is dropped; pairing and discovery run on the
//! shared network runtime (`remote::net`) since there is no token yet.

use std::time::Duration;

use rune::TextEdit;
use serde::Deserialize;
use zlo_client::environment_access::{DeviceSummary, PairClient};
use zlo_client::environments::{self, Active, PairingInput, Route, parse_pairing_input};
use zlo_client::pairing::{PairError, Paired};
use zlo_client::tailnet::{self, Discovery, Found};
use zlo_client::{Backend, ClientError};

use crate::jobs::{self, Generation};
use crate::state::State;

/// A pairing code on screen, with the link it is shared as.
#[derive(Clone, Debug)]
pub struct Issued {
    pub code: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub link: String,
}

#[derive(Default)]
pub struct RemoteAccess {
    pub busy: bool,
    pub error: Option<String>,
    pub note: Option<String>,
}

#[derive(Default)]
pub struct PageState {
    /// "Pairing link or code".
    pub input: TextEdit,
    /// "Over SSH": `user@host[:port]`.
    pub ssh_input: TextEdit,
    /// Pairing in flight: the computer's label or origin.
    pub busy: Option<String>,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// Where a bare code goes: (origin, label), picked from the tailnet or
    /// set by a one-click refusal.
    pub pair_target: Option<(String, String)>,
    pub discovery: Option<Discovery>,
    pub discovering: bool,
    /// The running scan is one the person asked for (Refresh): what it
    /// finds arrives with motion. A scan on its own lands at once.
    pub scan_shown: bool,
    /// `false` when Tailscale is missing or stopped (discovery hidden).
    pub tailscale: Option<bool>,
    discovery_gen: Generation,
    pub devices: Option<Result<Vec<DeviceSummary>, String>>,
    pub devices_loading: bool,
    pub issued: Option<Issued>,
    pub issuing: bool,
    pub issue_error: Option<String>,
    pub revoking: Option<String>,
    pub remote_access: RemoteAccess,
    /// The computer whose Remove waits for a confirm.
    pub confirm_remove: Option<String>,
    /// A connect's new backend handle until its first answer
    /// (`connect::open`).
    pub(crate) pending_backend: Option<(crate::jobs::Generation, Backend)>,
}

/// The Computers page (or setup's Connect step) opened.
pub fn opened(s: &mut State) {
    discover(s);
    load_devices(s);
    super::linked::opened(s);
}

/// A new computer answered: the page's per-computer parts reload.
pub fn connected(s: &mut State) {
    if s.page == crate::state::Page::Computers {
        load_devices(s);
        super::linked::opened(s);
    }
}

// ── discovery ────────────────────────────────────────────────────────────

/// `tailscale status --json` off the UI thread, then every online peer's
/// descriptor in parallel.
pub fn discover(s: &mut State) {
    if s.fixture || s.remote.page.discovering {
        return;
    }
    let Some(net) = super::net() else {
        return;
    };
    s.remote.page.discovering = true;
    let generation = s.remote.page.discovery_gen.bump();
    let handle = s.rt.handle.clone();
    net.spawn(async move {
        let peers = tokio::task::spawn_blocking(tailnet::status_peers)
            .await
            .ok()
            .flatten();
        let discovery = match peers {
            Some(peers) => Some(tailnet::probe(peers).await),
            None => None,
        };
        handle.update(move |s, tx| {
            let page = &mut s.remote.page;
            if page.discovery_gen != generation {
                return;
            }
            if std::mem::take(&mut page.scan_shown) {
                tx.animated();
            }
            page.discovering = false;
            page.tailscale = Some(discovery.is_some());
            page.discovery = discovery;
            super::peers::sync(s);
        });
    });
}

/// Found computers not saved yet.
pub fn found_unsaved(s: &State) -> Vec<&Found> {
    let Some(d) = &s.remote.page.discovery else {
        return Vec::new();
    };
    d.found
        .iter()
        .filter(|f| {
            s.remote
                .registry
                .get(&f.descriptor.environment_id)
                .is_none()
        })
        .collect()
}

// ── pairing ──────────────────────────────────────────────────────────────

/// What this Mac tells a computer about itself when pairing.
fn me() -> PairClient {
    let name =
        crate::system::computer_name().unwrap_or_else(|| crate::system::FALLBACK_NAME.to_string());
    PairClient {
        label: name,
        os: crate::system::OS_NAME.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
    }
}

/// "Pair": a pasted link or code.
pub fn submit(s: &mut State) {
    let page = &mut s.remote.page;
    if page.busy.is_some() {
        return;
    }
    let input = match parse_pairing_input(&page.input.text) {
        Ok(i) => i,
        Err(e) => {
            page.error = Some(e.to_string());
            return;
        }
    };
    match input {
        PairingInput::Code {
            origin: Some(origin),
            code,
        } => run_pair(s, origin.clone(), move |me| async move {
            zlo_client::pairing::with_code(&origin, &code, me).await
        }),
        PairingInput::Code { origin: None, code } => {
            let target = page.pair_target.clone().or_else(|| {
                // One computer found on the tailnet: the code is for it.
                match found_unsaved(s).as_slice() {
                    [only] => Some((only.peer.origin(), only.descriptor.label.clone())),
                    _ => None,
                }
            });
            let Some((origin, label)) = target else {
                s.remote.page.error = Some(
                    "Pick the computer the code is for (Use a code, next to it above), or paste the whole link."
                        .into(),
                );
                return;
            };
            run_pair(s, label, move |me| async move {
                zlo_client::pairing::with_code(&origin, &code, me).await
            })
        }
        PairingInput::LegacyToken { origin, token } => {
            run_pair(s, origin.clone(), move |_| async move {
                zlo_client::pairing::with_legacy_token(&origin, &token).await
            })
        }
    }
}

/// "Connect" over SSH: `ssh user@host zlo pair --json` makes a code
/// with the person's own key, a tunnel opens to the computer's loopback
/// backend, and the code is redeemed through it (`zlo_client::ssh`).
pub fn submit_ssh(s: &mut State) {
    let page = &mut s.remote.page;
    if page.busy.is_some() {
        return;
    }
    let target = match zlo_client::ssh::parse_target(&page.ssh_input.text) {
        Ok(t) => t,
        Err(e) => {
            page.error = Some(e.to_string());
            return;
        }
    };
    run_pair(s, target.destination(), move |me| async move {
        use zlo_client::ssh;
        let t = target.clone();
        let opened = tokio::task::spawn_blocking(move || {
            let code = match ssh::remote_pair_code(&t, ssh::run_ssh) {
                // Nothing runs there yet: start it (ZLO then owns it, and
                // removing the computer stops it), and ask again.
                Err(e) if ssh::backend_not_running(&e) => {
                    ssh::start_backend(&t, ssh::run_ssh)?;
                    let mut last = e;
                    let mut found = None;
                    for _ in 0..10 {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        match ssh::remote_pair_code(&t, ssh::run_ssh) {
                            Ok(code) => {
                                found = Some(code);
                                break;
                            }
                            Err(e) if ssh::backend_not_running(&e) => last = e,
                            Err(e) => return Err(e),
                        }
                    }
                    found.ok_or(last)?
                }
                // Not installed: start building it there.
                Err(ssh::SshError::NotInstalled(host)) => {
                    return Err(match ssh::install_backend(&t, ssh::run_ssh)? {
                        true => ssh::SshError::PairFailed {
                            host: host.clone(),
                            line: format!(
                                "ZLO isn't installed on {host} yet, so ZLO started building it there \
                                 (10 to 20 minutes; log in ~/.zlo/run/install.log). Pair again when it is done."
                            ),
                        },
                        false => ssh::SshError::PairFailed {
                            host: host.clone(),
                            line: format!(
                                "ZLO isn't installed on {host}, and it has no Rust to build it. Install \
                                 Rust there (rustup.rs) and pair again, or install ZLO there yourself."
                            ),
                        },
                    });
                }
                other => other?,
            };
            let port = code.backend_port.unwrap_or(ssh::DEFAULT_BACKEND_PORT);
            let (local, tunnel_lease) = super::tunnel::open(&t, port)?;
            Ok::<_, ssh::SshError>((code, port, local, tunnel_lease))
        })
        .await
        .map_err(|e| PairError::Other(format!("SSH: {e}")))?
        .map_err(|e| PairError::Other(e.to_string()))?;
        let (code, port, local, _tunnel_lease) = opened;
        let host = target.host.clone();
        zlo_client::pairing::over_ssh(
            &ssh::tunnel_origin(local),
            &code.code,
            Route::ssh(&target, port),
            me,
        )
        .await
        .map_err(|e| match e {
            // The tunnel's own address means nothing to the person.
            PairError::Unreachable(_) | PairError::NotIronClaw(_) => PairError::Other(format!(
                "The SSH tunnel to {host} is open, but IronClaw isn't answering there. Check that `zlo serve` runs on {host}."
            )),
            e => e,
        })
    });
}

/// One click on a computer found on the tailnet.
pub fn one_click(s: &mut State, origin: String, label: String) {
    if s.remote.page.busy.is_some() {
        return;
    }
    let target = (origin.clone(), label.clone());
    s.remote.page.pair_target = Some(target);
    run_pair(s, label, move |me| async move {
        zlo_client::pairing::by_tailnet(&origin, me).await
    });
}

/// A bare code will go to this computer.
pub fn use_code_for(s: &mut State, origin: String, label: String) {
    let page = &mut s.remote.page;
    page.pair_target = Some((origin, label));
    page.error = None;
    page.input.focused = true;
}

fn run_pair<F, Fut>(s: &mut State, what: String, pair: F)
where
    F: FnOnce(PairClient) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Paired, PairError>> + Send + 'static,
{
    let Some(net) = super::net() else {
        s.remote.page.error = Some("ZLO could not start its network runtime.".into());
        return;
    };
    let page = &mut s.remote.page;
    page.busy = Some(what);
    page.error = None;
    page.notice = None;
    if s.fixture {
        return;
    }
    let tokens = s.remote.tokens.clone();
    let handle = s.rt.handle.clone();
    net.spawn(async move {
        let me = tokio::task::spawn_blocking(me)
            .await
            .unwrap_or_else(|_| PairClient {
                label: "Mac".into(),
                os: "macOS".into(),
                app_version: env!("CARGO_PKG_VERSION").into(),
            });
        let result = match pair(me).await {
            // The token goes to the Keychain before anything is saved.
            Ok(paired) => {
                let (id, token) = (
                    paired.environment.environment_id.clone(),
                    paired.token.clone(),
                );
                match tokio::task::spawn_blocking(move || tokens.set(&id, &token)).await {
                    Ok(Ok(())) => Ok(paired),
                    Ok(Err(e)) => Err(PairError::Other(e.to_string())),
                    Err(e) => Err(PairError::Other(format!("Keychain: {e}"))),
                }
            }
            Err(e) => Err(e),
        };
        handle.update(move |s, _| paired(s, result));
    });
}

/// A pairing finished: save the computer and switch to it.
pub(crate) fn paired(s: &mut State, result: Result<Paired, PairError>) {
    s.remote.page.busy = None;
    let paired = match result {
        Ok(p) => p,
        Err(e) => {
            // A tunnel opened for this pairing is not needed any more.
            super::retain_tunnels(s);
            if matches!(e, PairError::NotOwner(_) | PairError::TailnetDisabled(_)) {
                // The code field is the way on: it goes to that computer.
                s.remote.page.input.focused = true;
            }
            s.remote.page.error = Some(e.to_string());
            return;
        }
    };
    let id = paired.environment.environment_id.clone();
    let label = paired.environment.label.clone();
    s.remote.registry.upsert(paired.environment);
    if let Err(e) = super::save_registry(s) {
        s.remote.page.error = Some(e);
        return;
    }
    let page = &mut s.remote.page;
    // A link or code is single use: clear it from the field.
    page.input.clear();
    page.ssh_input.clear();
    page.pair_target = None;
    page.notice = Some(format!("Paired with {label}."));
    s.onboarding.connect.pairing_open = false;
    s.remote.problems.remove(&id);
    super::switch_to(s, Active::Environment(id));
}

/// Remove a saved computer from this Mac: its entry and its Keychain
/// token. Nothing on that computer is touched (revoke it there, or from
/// its Paired devices).
pub fn remove(s: &mut State, environment_id: String) {
    s.remote.page.confirm_remove = None;
    let was_active = s.remote.registry.active == Active::Environment(environment_id.clone());
    let removed_routes = s
        .remote
        .registry
        .get(&environment_id)
        .map(|e| e.routes.clone())
        .unwrap_or_default();
    s.remote
        .registry
        .environments
        .retain(|e| e.environment_id != environment_id);
    s.remote.problems.remove(&environment_id);
    s.remote.last_thread.remove(&environment_id);
    super::updates::forget(s, &environment_id);
    // A server ZLO launched over SSH stops with its computer; one it found
    // running is left alone (the host's pid file says which).
    if !s.fixture {
        let ssh_targets: Vec<zlo_client::ssh::SshTarget> = removed_routes
            .iter()
            .filter(|r| r.kind == zlo_client::environments::RouteKind::Ssh)
            .filter_map(|r| zlo_client::ssh::SshTarget::from_url(&r.url).ok())
            .collect();
        if !ssh_targets.is_empty() {
            jobs::blocking(
                s,
                move || {
                    for t in &ssh_targets {
                        if let Err(e) =
                            zlo_client::ssh::stop_launched_backend(t, zlo_client::ssh::run_ssh)
                        {
                            tracing::debug!(error = %e, "launched backend not stopped");
                        }
                    }
                    Ok::<(), String>(())
                },
                |_, _, _| {},
            );
        }
    }
    if !s.fixture {
        let tokens = s.remote.tokens.clone();
        let id = environment_id.clone();
        jobs::blocking(
            s,
            move || tokens.delete(&id),
            |s, result, _| {
                if let Err(e) = result {
                    tracing::warn!(error = %e, "computer's token not removed from the Keychain");
                    s.remote.page.error =
                        Some(format!("{e}. Its token may still be in the Keychain."));
                }
            },
        );
    }
    if was_active {
        super::switch_to(s, Active::ThisMac);
    } else {
        if let Err(e) = super::save_registry(s) {
            s.remote.page.error = Some(e);
        }
        super::peers::sync(s);
    }
}

/// "Run IronClaw on this Mac".
pub fn set_run_local(s: &mut State, on: bool) {
    s.remote.registry.run_local_backend = on;
    if let Err(e) = super::save_registry(s) {
        s.remote.page.error = Some(e);
    }
    let stopped_managed = if !on && !s.fixture {
        // Stops only the backend this app started.
        crate::service::set_allowed(false);
        crate::service::stop()
    } else {
        false
    };
    if !on && stopped_managed && !super::backend_is_remote(s) && !super::env_override() {
        // The managed process was stopped: drop its dead client and leave
        // `Conn::Ready` so turning the switch back on reconnects cleanly.
        s.workspace.connect_gen.bump();
        s.remote.page.pending_backend = None;
        s.rt.backend = None;
        s.remote.tag = super::ConnTag::default();
        s.conn = crate::state::Conn::Failed {
            message: "\"Run IronClaw on this Mac\" is off in Settings → Computers. \
                      Turn it back on, start `zlo serve` yourself, or pick another computer."
                .into(),
            retry_in: None,
        };
    }
    super::sync_local_backend(s);
    super::peers::sync(s);
    if on && !super::backend_is_remote(s) && !matches!(s.conn, crate::state::Conn::Ready) {
        crate::workspace::reconnect(s);
    }
}

// ── the active computer: pairing codes, devices ─────────────────────────

/// Where a pairing link should point: the tailnet URL, else the address
/// ZLO reaches the computer at.
pub fn link_origin(s: &State) -> Option<String> {
    s.remote
        .descriptor
        .as_ref()
        .and_then(|d| d.tailnet_url.clone())
        .or_else(|| {
            s.rt.backend
                .as_ref()
                .map(|b| b.client().connection().base_url.clone())
        })
}

pub fn issue_code(s: &mut State) {
    issue_code_with(
        s,
        zlo_client::environment_access::PairingCodeRequest::default(),
    );
}

/// A code for a device that may only read threads (a phone, a dashboard).
pub fn issue_read_only_code(s: &mut State) {
    issue_code_with(
        s,
        zlo_client::environment_access::PairingCodeRequest {
            scopes: Some(vec![
                zlo_client::environment_access::DeviceScope::OrchestrationRead,
            ]),
            peer_link: None,
        },
    );
}

fn issue_code_with(s: &mut State, request: zlo_client::environment_access::PairingCodeRequest) {
    let page = &mut s.remote.page;
    if page.issuing {
        return;
    }
    page.issuing = true;
    page.issue_error = None;
    jobs::run(
        s,
        "pairing code",
        move |c| async move { c.issue_scoped_code(&request).await },
        |s, result, tx| {
            // The person's Make a code: the code wipes in.
            tx.animated();
            let origin = link_origin(s);
            let page = &mut s.remote.page;
            page.issuing = false;
            match result {
                Ok(issued) => {
                    let link = origin
                        .map(|o| environments::pairing_link(&o, &issued.code))
                        .unwrap_or_default();
                    page.issued = Some(Issued {
                        code: issued.code.clone(),
                        expires_at: issued.expires_at,
                        link,
                    });
                }
                Err(e) => page.issue_error = Some(backend_feature_error(&e)),
            }
        },
    );
}

/// A route the computer's IronClaw does not have yet.
fn backend_feature_error(e: &ClientError) -> String {
    if e.is_not_found() {
        "This computer's IronClaw is too old for pairing. Update it.".into()
    } else {
        e.to_string()
    }
}

pub fn load_devices(s: &mut State) {
    if s.rt.backend.is_none() || s.remote.page.devices_loading {
        return;
    }
    s.remote.page.devices_loading = true;
    jobs::run(
        s,
        "devices",
        |c| async move { c.devices().await },
        |s, result, _| {
            let page = &mut s.remote.page;
            page.devices_loading = false;
            page.devices = Some(
                result
                    .map(|l| l.devices)
                    .map_err(|e| backend_feature_error(&e)),
            );
        },
    );
}

pub fn revoke(s: &mut State, device_id: String) {
    if s.remote.page.revoking.is_some() {
        return;
    }
    s.remote.page.revoking = Some(device_id.clone());
    jobs::run(
        s,
        "revoke device",
        move |c| async move { c.revoke_device(&device_id).await },
        |s, result, _| {
            s.remote.page.revoking = None;
            match result {
                Ok(()) => load_devices(s),
                Err(e) => {
                    s.remote.page.devices = Some(Err(backend_feature_error(&e)));
                }
            }
        },
    );
}

// ── this Mac: let other devices in ───────────────────────────────────────

/// `remote enable --json`'s answer (wire contract v1, CLI).
#[derive(Debug, Deserialize)]
struct EnableReport {
    tailnet_url: Option<String>,
    #[serde(default)]
    one_click: bool,
    #[serde(default)]
    restart_required: bool,
}

/// Remote access is on for this Mac (its descriptor names a tailnet URL).
pub fn remote_access_on(s: &State) -> bool {
    !super::backend_is_remote(s)
        && s.remote
            .descriptor
            .as_ref()
            .is_some_and(|d| d.tailnet_url.is_some())
}

/// "Let my other devices use this Mac": `remote enable --json` /
/// `remote disable` with the backend binary ZLO runs.
pub fn set_remote_access(s: &mut State, on: bool) {
    let access = &mut s.remote.page.remote_access;
    if access.busy {
        return;
    }
    access.busy = true;
    access.error = None;
    access.note = None;
    if s.fixture {
        return;
    }
    jobs::blocking(
        s,
        move || run_remote_command(on),
        move |s, result, _| {
            let access = &mut s.remote.page.remote_access;
            access.busy = false;
            match result {
                Ok(report) => {
                    let restart = report.as_ref().is_some_and(|r| r.restart_required);
                    access.note = Some(match &report {
                        Some(r) => match (&r.tailnet_url, r.one_click) {
                            (Some(url), true) => format!(
                                "On at {url}. Your other devices on this tailnet can connect in one click."
                            ),
                            (Some(url), false) => format!(
                                "On at {url}. Other devices pair with a code (Pair a device)."
                            ),
                            (None, _) => "Remote access is on.".into(),
                        },
                        None => "Remote access is off.".into(),
                    });
                    if restart && !crate::service::restart() {
                        access.note = Some(
                            "Restart IronClaw on this Mac to finish (it was not started by ZLO)."
                                .into(),
                        );
                    }
                    refresh_descriptor(s, if restart { 3 } else { 0 });
                }
                Err(e) => access.error = Some(e),
            }
        },
    );
}

/// `Ok(Some)` after an enable, `Ok(None)` after a disable.
fn run_remote_command(on: bool) -> Result<Option<EnableReport>, String> {
    let binary =
        crate::service::backend_binary().ok_or("ZLO cannot find IronClaw's binary on this Mac.")?;
    let args: &[&str] = if on {
        &["remote", "enable", "--json"]
    } else {
        &["remote", "disable"]
    };
    let mut cmd = std::process::Command::new(&binary);
    cmd.args(args).stdin(std::process::Stdio::null());
    let home = zlo_client::prepare_reborn_home().map_err(|e| e.to_string())?;
    crate::service::set_home_env(&mut cmd, &home);
    let out = cmd
        .output()
        .map_err(|e| format!("Could not run IronClaw ({}): {e}", binary.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        let line = stderr.lines().rev().find(|l| !l.trim().is_empty());
        return Err(match line {
            Some(l)
                if l.contains("unrecognized subcommand") || l.contains("unexpected argument") =>
            {
                "This IronClaw has no `remote` command yet. Update IronClaw on this Mac.".into()
            }
            Some(l) => l.trim().to_string(),
            None => format!("`ironclaw {}` failed ({}).", args.join(" "), out.status),
        });
    }
    if !on {
        return Ok(None);
    }
    serde_json::from_slice::<EnableReport>(&out.stdout)
        .map(Some)
        .map_err(|e| format!("IronClaw answered `remote enable` in an unexpected way: {e}"))
}

/// Read the connected computer's descriptor again (after remote access
/// changed), `after_secs` later.
fn refresh_descriptor(s: &mut State, after_secs: u64) {
    let Some(origin) =
        s.rt.backend
            .as_ref()
            .map(|b| b.client().connection().base_url.clone())
    else {
        return;
    };
    let tag = s.remote.tag.clone();
    jobs::after(s, Duration::from_secs(after_secs), move |s, _| {
        let Some(net) = super::net() else {
            return;
        };
        let handle = s.rt.handle.clone();
        net.spawn(async move {
            let result =
                zlo_client::environment_calls::fetch_descriptor(&origin, super::DESCRIPTOR_TIMEOUT)
                    .await;
            handle.update(move |s, _| {
                if !super::lands(s, &tag) {
                    return;
                }
                match result {
                    Ok(d) => s.remote.descriptor = Some(d),
                    Err(e) => tracing::debug!(error = %e, "descriptor not refreshed"),
                }
            });
        });
    });
}
