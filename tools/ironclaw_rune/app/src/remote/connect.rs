//! Opening a connection (T3 Code's connection supervisor, one computer at
//! a time): resolve the active computer, read its descriptor (every
//! connect: the id must match, the protocol must be ours), then the session
//! and inbox on a fresh backend handle. Only then does the app switch to it,
//! so a switch keeps the old screen until the new computer answers.
//!
//! Failures split in two, as T3's `connection/model.ts`: unreachable ones
//! retry on a schedule (`workspace::connect_failed`), blocked ones (wrong
//! machine, protocol, token, unreadable registry) wait for the person.

use std::sync::Arc;
use std::time::Duration;

use zlo_client::environment_access::EnvironmentDescriptor;
use zlo_client::environment_calls::fetch_descriptor;
use zlo_client::environments::{
    Active, LegacyGateway, Route, RouteKind, SavedEnvironment, check_descriptor, check_transport,
    host_of, preferred_routes,
};
use zlo_client::inbox::InboxListResponse as InboxList;
use zlo_client::keychain::TokenStore;
use zlo_client::ssh::{self, SshError, SshTarget};
use zlo_client::wire::SessionInfo;
use zlo_client::{Backend, Client, ClientError, Connection, Token};

use super::{ConnTag, Switching, active_key};
use crate::jobs::Generation;
use crate::onboarding::gateway::Computer;
use crate::state::{Conn, State};

const DESCRIPTOR_TIMEOUT: Duration = Duration::from_secs(8);

/// Why a connect did not get through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Not reachable now: retried on a schedule.
    Unreachable(String),
    /// Needs the person: no automatic retry.
    Blocked(String),
}

/// What the active computer resolves to before any request.
#[derive(Clone, Debug)]
enum Target {
    ThisMac,
    /// `IRONCLAW_DESKTOP_URL`.
    Override,
    Environment(SavedEnvironment),
    /// `gateway.json`, moved into the registry first.
    Legacy(LegacyGateway),
}

struct Resolved {
    conn: Connection,
    computer: Computer,
    descriptor: Option<EnvironmentDescriptor>,
    /// The `gateway.json` computer, moved: save it, then delete the file.
    migrated: Option<SavedEnvironment>,
    /// The SSH tunnel the connection goes through (`tunnel::key`).
    tunnel: Option<String>,
}

/// App-only, nothing picked yet: the window asks for a computer.
pub(crate) const NEEDS_COMPUTER: &str = "Add a computer to work on.";

/// How the app waits: a full connecting screen, or the old screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Fresh,
    Switch,
}

fn target(s: &State) -> Result<Target, Problem> {
    if super::env_override() {
        return Ok(Target::Override);
    }
    if let Some(e) = &s.remote.registry_error {
        return Err(Problem::Blocked(format!(
            "{e}\n\nFix or delete that file, then try again. Nothing starts on this Mac until then."
        )));
    }
    if s.remote.legacy_active
        && let Some(legacy) = &s.remote.legacy
    {
        return Ok(Target::Legacy(legacy.clone()));
    }
    match &s.remote.registry.active {
        // A Light bundle has no backend of its own.
        Active::ThisMac if s.remote.app_only => Err(Problem::Blocked(NEEDS_COMPUTER.into())),
        Active::ThisMac => Ok(Target::ThisMac),
        Active::Environment(id) => s
            .remote
            .registry
            .get(id)
            .cloned()
            .map(Target::Environment)
            .ok_or_else(|| {
                Problem::Blocked(
                    "ZLO's list of computers names one it no longer has. Pick a computer in Settings → Computers."
                        .into(),
                )
            }),
    }
}

/// Connect to the active computer with the connecting screen (launch,
/// Try again, scheduled retries).
pub fn connect(s: &mut State) {
    start(s, Mode::Fresh);
}

/// Work on another computer. From a working screen the old one stays until
/// the new one answers; a failure leaves it in place with the reason on the
/// Computers page.
pub fn switch_to(s: &mut State, target: Active) {
    if s.remote.registry.active == target && !s.remote.legacy_active && s.remote.switching.is_none()
    {
        if !matches!(s.conn, Conn::Ready) {
            connect(s);
        }
        return;
    }
    let previous = s.remote.registry.active.clone();
    s.remote.registry.active = target.clone();
    s.remote.legacy_active = false;
    s.remote.problems.remove(&active_key(&target));
    let working = matches!(s.conn, Conn::Ready) && s.rt.backend.is_some();
    if working {
        s.remote.switching = Some(Switching { target, previous });
        start(s, Mode::Switch);
    } else {
        // Nothing to keep on screen: the pick is saved now.
        if let Err(e) = super::save_registry(s) {
            tracing::warn!(error = %e, "computer pick not saved");
        }
        s.workspace.connect_attempt = 0;
        s.launched = std::time::Instant::now();
        start(s, Mode::Fresh);
    }
}

fn start(s: &mut State, mode: Mode) {
    let generation = s.workspace.connect_gen.bump();
    if mode == Mode::Fresh {
        s.conn = Conn::Connecting;
        s.remote.switching = None;
    }
    super::sync_local_backend(s);
    if s.fixture {
        return;
    }
    let target = match target(s) {
        Ok(t) => t,
        Err(p) => {
            if super::needs_computer(s) && !cfg!(test) {
                // The "Add a computer" screen lists what the tailnet has.
                super::pairing::discover(s);
            }
            return failed(s, mode, p);
        }
    };
    if mode == Mode::Fresh {
        // The connecting and failed screens name the computer being tried.
        let next = match &target {
            Target::ThisMac | Target::Override => Computer::ThisMac,
            Target::Environment(env) => Computer::remote(&env.environment_id, &env.label),
            Target::Legacy(legacy) => Computer::remote("gateway.json", host_of(&legacy.url)),
        };
        if next.key() != s.onboarding.computer.key() {
            if let Some(id) = crate::workspace::active_id(s) {
                let old = s.onboarding.computer.key();
                s.remote.last_thread.insert(old, id.to_string());
            }
            reset_for_switch(s);
            s.rt.backend = None;
            s.remote.tag = ConnTag::default();
        }
        s.onboarding.computer = next;
    }
    let Some(net) = super::net() else {
        return failed(
            s,
            mode,
            Problem::Blocked("ZLO could not start its network runtime.".into()),
        );
    };
    let tokens = s.remote.tokens.clone();
    let handle = s.rt.handle.clone();
    net.spawn(async move {
        let resolved = resolve(target, tokens).await;
        handle.update(move |s, _| {
            if s.fixture || s.workspace.connect_gen != generation {
                return;
            }
            match resolved {
                Ok(r) => open(s, generation, mode, r),
                Err(p) => failed(s, mode, p),
            }
        });
    });
}

async fn resolve(target: Target, tokens: Arc<dyn TokenStore>) -> Result<Resolved, Problem> {
    match target {
        Target::ThisMac => this_mac(false).await,
        Target::Override => this_mac(true).await,
        Target::Environment(env) => environment(env, tokens).await,
        Target::Legacy(legacy) => {
            let host = host_of(&legacy.url).to_string();
            let paired = zlo_client::pairing::migrate(&legacy).await.map_err(|e| {
                let line = format!("ZLO is moving the paired computer {host} to its new list. {e}");
                if e.is_transient() {
                    Problem::Unreachable(line)
                } else {
                    Problem::Blocked(format!(
                        "{line}\n\nPick This Mac, or pair it again in Settings → Computers."
                    ))
                }
            })?;
            store_token(&tokens, &paired.environment.environment_id, &paired.token).await?;
            let mut resolved = environment(paired.environment.clone(), tokens).await?;
            resolved.migrated = Some(paired.environment);
            Ok(resolved)
        }
    }
}

async fn store_token(
    tokens: &Arc<dyn TokenStore>,
    environment_id: &str,
    token: &str,
) -> Result<(), Problem> {
    let (tokens, id, token) = (
        tokens.clone(),
        environment_id.to_string(),
        token.to_string(),
    );
    tokio::task::spawn_blocking(move || tokens.set(&id, &token))
        .await
        .map_err(|e| Problem::Blocked(format!("Keychain: {e}")))?
        .map_err(|e| Problem::Blocked(e.to_string()))
}

async fn this_mac(override_url: bool) -> Result<Resolved, Problem> {
    let conn = Connection::discover().map_err(|e| Problem::Blocked(e.to_string()))?;
    if override_url {
        check_transport(&conn.base_url).map_err(|e| {
            Problem::Blocked(format!("IRONCLAW_DESKTOP_URL ({}): {e}", conn.base_url))
        })?;
    }
    // This Mac's backend may predate the descriptor (it is whatever is
    // installed): a 404 is no reason to stop. A wrong protocol is.
    let descriptor = match fetch_descriptor(&conn.base_url, DESCRIPTOR_TIMEOUT).await {
        Ok(d) => {
            check_descriptor(&d, None).map_err(|e| Problem::Blocked(e.to_string()))?;
            Some(d)
        }
        Err(ClientError::Network(e)) => {
            return Err(Problem::Unreachable(ClientError::Network(e).to_string()));
        }
        Err(e) => {
            tracing::debug!(error = %e, "this Mac's backend serves no descriptor");
            None
        }
    };
    Ok(Resolved {
        conn,
        computer: Computer::ThisMac,
        descriptor,
        migrated: None,
        tunnel: None,
    })
}

/// Open (or reuse) the tunnel of an SSH route: its key and the origin the
/// client uses through it.
async fn open_tunnel(route: &Route, label: &str) -> Result<(String, String), Problem> {
    let target = SshTarget::from_url(&route.url).map_err(|_| {
        Problem::Blocked(format!(
            "{label}'s SSH address ({}) is not valid. Pair it again in Settings → Computers.",
            route.url
        ))
    })?;
    let port = route.remote_port.unwrap_or(ssh::DEFAULT_BACKEND_PORT);
    let key = super::tunnel::key(&target, port);
    let opened = tokio::task::spawn_blocking(move || super::tunnel::open(&target, port))
        .await
        .map_err(|e| Problem::Unreachable(format!("SSH tunnel: {e}")))?;
    match opened {
        Ok(local) => Ok((key, ssh::tunnel_origin(local))),
        Err(e) => Err(ssh_problem(e)),
    }
}

/// Key and host-key problems need the person; the rest may pass.
fn ssh_problem(e: SshError) -> Problem {
    match e {
        SshError::AuthFailed(_)
        | SshError::UnknownHostKey { .. }
        | SshError::BadTarget
        | SshError::NoSsh(_) => Problem::Blocked(e.to_string()),
        SshError::Unreachable(_)
        | SshError::TunnelFailed(_)
        | SshError::NotInstalled(_)
        | SshError::PairFailed { .. } => Problem::Unreachable(e.to_string()),
    }
}

async fn environment(
    env: SavedEnvironment,
    tokens: Arc<dyn TokenStore>,
) -> Result<Resolved, Problem> {
    let label = env.label.clone();
    let id = env.environment_id.clone();
    let lookup = {
        let (tokens, id) = (tokens.clone(), id.clone());
        tokio::task::spawn_blocking(move || tokens.get(&id))
            .await
            .map_err(|e| Problem::Blocked(format!("Keychain: {e}")))?
    };
    let token = match lookup {
        Ok(Some(t)) => Token::new(t).map_err(|e| Problem::Blocked(e.to_string()))?,
        Ok(None) => {
            return Err(Problem::Blocked(format!(
                "ZLO has no saved token for {label}. Pair it again in Settings → Computers."
            )));
        }
        Err(e) => return Err(Problem::Blocked(format!("{e}. Try again."))),
    };
    let mut last = Problem::Blocked(format!(
        "{label} has no address ZLO can use. Pair it again in Settings → Computers."
    ));
    // Tailnet and direct routes first; an SSH tunnel only when they do
    // not answer.
    for route in preferred_routes(&env.routes) {
        let (origin, tunnel) = if route.kind == RouteKind::Ssh {
            match open_tunnel(route, &label).await {
                Ok((key, origin)) => (origin, Some(key)),
                Err(p) => {
                    last = p;
                    continue;
                }
            }
        } else {
            match check_transport(&route.url) {
                Ok(o) => (o, None),
                Err(e) => {
                    last = Problem::Blocked(format!("{label} ({}): {e}", route.url));
                    continue;
                }
            }
        };
        match fetch_descriptor(&origin, DESCRIPTOR_TIMEOUT).await {
            Ok(d) => {
                check_descriptor(&d, Some((&id, &label)))
                    .map_err(|e| Problem::Blocked(e.to_string()))?;
                return Ok(Resolved {
                    conn: Connection {
                        base_url: origin,
                        token,
                        token_source: "Keychain".into(),
                    },
                    computer: Computer::remote(id, d.label.clone()),
                    descriptor: Some(d),
                    migrated: None,
                    tunnel,
                });
            }
            Err(ClientError::Network(_)) if tunnel.is_some() => {
                last = Problem::Unreachable(format!(
                    "The SSH tunnel to {label} is open, but IronClaw isn't answering there. Check that `ironclaw serve` runs on {label}."
                ));
            }
            Err(ClientError::Network(_)) => {
                last = Problem::Unreachable(format!(
                    "Could not reach {label} at {}. Check that it is awake, that IronClaw runs there, and that Tailscale is on.",
                    host_of(&origin)
                ));
            }
            Err(ClientError::Api { status: 404, .. } | ClientError::Decode(_)) => {
                last = Problem::Blocked(format!(
                    "IronClaw on {label} is older than this ZLO. Update IronClaw on {label}."
                ));
            }
            Err(e) => last = Problem::Unreachable(format!("{label}: {e}")),
        }
    }
    Err(last)
}

/// The descriptor checked out: build the backend handle and read the
/// session and inbox on it, still without touching the app.
fn open(s: &mut State, generation: Generation, mode: Mode, r: Resolved) {
    if let Some(env) = r.migrated.clone() {
        // Saved before `gateway.json` goes: a failed save keeps the old
        // file for the next launch.
        s.remote.registry.upsert(env.clone());
        s.remote.registry.active = Active::Environment(env.environment_id);
        match super::save_registry(s) {
            Ok(()) => super::forget_legacy(s),
            Err(e) => tracing::warn!(error = %e, "moved computer not saved; kept gateway.json"),
        }
    }
    let backend = match Client::new(r.conn).and_then(Backend::new) {
        Ok(b) => b,
        Err(e) => return failed(s, mode, Problem::Blocked(e.to_string())),
    };
    let handle = s.rt.handle.clone();
    let job = backend.spawn(|c| async move {
        let session = c.session().await?;
        let inbox = c.inbox().await?;
        Ok::<_, ClientError>((session, inbox))
    });
    let computer = r.computer;
    let descriptor = r.descriptor;
    let tunnel = r.tunnel;
    // The UI thread holds the handle until the answer: its runtime is
    // never dropped on one of its own threads.
    // Tagged with this attempt, so a stale answer cannot take a newer
    // attempt's handle. Replacing an older attempt's handle drops it here,
    // on the UI thread.
    s.remote.page.pending_backend = Some((generation, backend.clone()));
    backend.spawn(move |_| async move {
        let result = crate::jobs::flatten(job.await);
        handle.update(move |s, _| {
            if s.fixture {
                return;
            }
            let backend = match claim_pending(s, generation) {
                Claim::Stale => return,
                Claim::Missing => {
                    return failed(
                        s,
                        mode,
                        Problem::Unreachable("The connection was replaced. Trying again.".into()),
                    );
                }
                Claim::Mine(backend) => backend,
            };
            match result {
                Ok((session, inbox)) => {
                    s.remote.tunnel = tunnel;
                    switched(s, mode, backend, computer, descriptor, session, inbox)
                }
                Err(e) => failed(s, mode, problem_of(&computer, &e)),
            }
        });
    });
}

/// What a connect's answer finds in `pending_backend`.
pub(crate) enum Claim {
    /// A newer attempt is under way: drop the answer.
    Stale,
    /// Current, but its handle is gone (never expected).
    Missing,
    Mine(Backend),
}

/// Take the handle parked for `generation`, never a newer attempt's.
pub(crate) fn claim_pending(s: &mut State, generation: Generation) -> Claim {
    let mine = s
        .remote
        .page
        .pending_backend
        .as_ref()
        .is_some_and(|(g, _)| *g == generation);
    if s.workspace.connect_gen != generation {
        // Only this attempt's own handle, if still parked, goes.
        if mine {
            s.remote.page.pending_backend = None;
        }
        return Claim::Stale;
    }
    match s.remote.page.pending_backend.take() {
        Some((_, backend)) if mine => Claim::Mine(backend),
        other => {
            s.remote.page.pending_backend = other;
            Claim::Missing
        }
    }
}

fn problem_of(computer: &Computer, e: &ClientError) -> Problem {
    match (e, computer) {
        (ClientError::Network(_), _) => Problem::Unreachable(e.to_string()),
        (e, Computer::Remote(r)) if e.is_unauthorized() => Problem::Blocked(format!(
            "{} turned this Mac's token down (it may have been revoked). Pair it again in Settings → Computers.",
            r.label
        )),
        (e, Computer::ThisMac) if e.is_unauthorized() => Problem::Blocked(format!(
            "{e}\n\nThe WebUI token was rejected. Re-run `ironclaw onboard` to refresh it."
        )),
        (e, _) => Problem::Blocked(e.to_string()),
    }
}

/// The new computer answered: everything of the old one goes, and the app
/// reloads on the new one.
fn switched(
    s: &mut State,
    mode: Mode,
    backend: Backend,
    computer: Computer,
    descriptor: Option<EnvironmentDescriptor>,
    session: SessionInfo,
    inbox: InboxList,
) {
    let old = s.onboarding.computer.key();
    let changed = old != computer.key() || mode == Mode::Switch;
    if changed {
        if let Some(id) = crate::workspace::active_id(s) {
            s.remote.last_thread.insert(old, id.to_string());
        }
        reset_for_switch(s);
    }
    let key = computer.key();
    tracing::debug!(
        computer = %key,
        descriptor = descriptor.is_some(),
        "connected"
    );
    adopt(s, computer, descriptor);
    s.rt.backend = Some(backend);
    // Only the tunnel this connection goes through stays open.
    super::tunnel::retain(s.remote.tunnel.as_deref());
    if s.remote.switching.take().is_some()
        && let Err(e) = super::save_registry(s)
    {
        tracing::warn!(error = %e, "computer pick not saved");
    }
    super::sync_local_backend(s);
    let reopen = s.remote.last_thread.get(&key).cloned();
    crate::workspace::connected(s, session, inbox, reopen);
    super::pairing::connected(s);
}

/// The app is on `computer` now: a new connection tag, so every reply
/// asked on the old connection is dropped when it lands.
pub(crate) fn adopt(s: &mut State, computer: Computer, descriptor: Option<EnvironmentDescriptor>) {
    let key = computer.key();
    s.remote.problems.remove(&key);
    s.remote.descriptor = descriptor;
    s.remote.tag = ConnTag {
        environment: key,
        generation: Generation::unique(),
    };
    s.onboarding.computer = computer;
}

/// Per-computer state the new computer must not see.
pub(crate) fn reset_for_switch(s: &mut State) {
    s.chat = None;
    s.pages = Default::default();
    s.workspace.draft = None;
    s.workspace.telemetry = None;
    s.workspace.titles = Default::default();
    s.workspace.run_clock = Default::default();
    s.workspace.confirm_delete = None;
    s.workspace.unread = 0;
    s.remote.descriptor = None;
    s.remote.page.devices = None;
    s.remote.page.issued = None;
    crate::onboarding::computer_switched(s);
}

fn failed(s: &mut State, mode: Mode, problem: Problem) {
    if mode == Mode::Switch
        && let Some(sw) = s.remote.switching.take()
    {
        // The old computer stays on screen, and active.
        let key = active_key(&sw.target);
        s.remote.registry.active = sw.previous;
        let line = match &problem {
            Problem::Unreachable(m) | Problem::Blocked(m) => m.clone(),
        };
        tracing::warn!(computer = %key, problem = %line, "switch failed");
        s.remote.problems.insert(key, line);
        super::tunnel::retain(s.remote.tunnel.as_deref());
        super::sync_local_backend(s);
        return;
    }
    if let Computer::Remote(r) = &s.onboarding.computer
        && let Problem::Blocked(m) = &problem
    {
        s.remote
            .problems
            .insert(r.environment_id.clone(), m.clone());
    }
    crate::workspace::connect_failed(s, problem);
}

/// Move a `gateway.json` computer that is not the one in use into the
/// registry, on the side. Unreachable now: it stays for the next launch.
pub fn migrate_in_background(s: &mut State) {
    if s.fixture || s.remote.legacy_active || s.remote.registry_error.is_some() {
        return;
    }
    let (Some(legacy), Some(net)) = (s.remote.legacy.clone(), super::net()) else {
        return;
    };
    let tokens = s.remote.tokens.clone();
    let handle = s.rt.handle.clone();
    net.spawn(async move {
        let result = match zlo_client::pairing::migrate(&legacy).await {
            Ok(paired) => store_token(&tokens, &paired.environment.environment_id, &paired.token)
                .await
                .map(|()| paired.environment)
                .map_err(|p| format!("{p:?}")),
            Err(e) => Err(e.to_string()),
        };
        handle.update(move |s, _| match result {
            Ok(env) => {
                s.remote.registry.upsert(env);
                match super::save_registry(s) {
                    Ok(()) => super::forget_legacy(s),
                    Err(e) => tracing::warn!(error = %e, "moved computer not saved; kept gateway.json"),
                }
            }
            Err(e) => tracing::debug!(error = %e, "paired computer not moved yet; next launch tries again"),
        });
    });
}
