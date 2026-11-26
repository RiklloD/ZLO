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

use zlo_client::environment_access::EnvironmentDescriptor;
use zlo_client::environment_calls::fetch_descriptor;
use zlo_client::environments::{
    Active, LegacyGateway, Route, RouteKind, SavedEnvironment, check_descriptor,
    check_learned_transport, check_transport, host_of, preferred_routes,
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

/// Why a connect did not get through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Not reachable now: retried on a schedule.
    Unreachable(String),
    /// Needs the person: no automatic retry.
    Blocked(String),
}

/// What a computer resolves to before any request.
#[derive(Clone, Debug)]
pub(super) enum Target {
    ThisMac,
    /// `IRONCLAW_DESKTOP_URL`.
    Override,
    Environment(SavedEnvironment),
    /// `gateway.json`, moved into the registry first.
    Legacy(LegacyGateway),
}

pub(super) struct Resolved {
    pub(super) conn: Connection,
    pub(super) computer: Computer,
    pub(super) descriptor: Option<EnvironmentDescriptor>,
    /// The `gateway.json` computer, moved: save it, then delete the file.
    pub(super) migrated: Option<SavedEnvironment>,
    /// The SSH tunnel lease the connection goes through (`tunnel::Lease`),
    /// held from open until adoption or cancellation so `retain_tunnels`
    /// cannot close it mid-handshake.
    pub(super) tunnel: Option<super::tunnel::Lease>,
    /// Which of the computer's routes answered, in `preferred_routes` order
    /// (0 = its first). A later one starts the fail-back check.
    pub(super) route_rank: usize,
}

/// App-only, nothing picked yet: the window asks for a computer.
pub(crate) const NEEDS_COMPUTER: &str = "Add a computer to work on.";

/// How the app waits: a full connecting screen, or the old screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Fresh,
    Switch,
    /// The same computer over a better route: nothing on screen changes,
    /// and a failure leaves the working connection alone.
    Refresh,
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
    if s.remote.switching.is_none()
        && s.remote.legacy_active
        && let Some(legacy) = &s.remote.legacy
    {
        return Ok(Target::Legacy(legacy.clone()));
    }
    let active = s
        .remote
        .switching
        .as_ref()
        .map(|sw| &sw.target)
        .unwrap_or(&s.remote.registry.active);
    match active {
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

/// Reconnect to the computer in use without touching the screen: its first
/// route that answers wins (`failback`).
pub fn refresh(s: &mut State) {
    if !matches!(s.conn, Conn::Ready) || s.remote.switching.is_some() {
        return;
    }
    start(s, Mode::Refresh);
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
    s.remote.problems.remove(&active_key(&target));
    let working = matches!(s.conn, Conn::Ready) && s.rt.backend.is_some();
    if working {
        // Keep `registry.active` on the currently focused computer until the
        // switch commits so background saves never persist an uncommitted pick.
        s.remote.switching = Some(Switching { target, previous });
        start(s, Mode::Switch);
    } else {
        // Nothing to keep on screen: the pick is saved now.
        s.remote.registry.active = target;
        s.remote.legacy_active = false;
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
        // Reconcile peers independently of whether the focused connect succeeds.
        super::peers::sync(s);
    }
    let Some(net) = super::net() else {
        return failed(
            s,
            mode,
            Problem::Blocked("ZLO could not start its network runtime.".into()),
        );
    };
    let tokens = s.remote.tokens.clone();
    let delivery = s.rt.delivery();
    // Only a backend this app starts, its own bundled one, makes its token
    // on first start (`cargo run` on an installed CLI does not).
    let own_backend = super::runs_local_backend(s) && crate::service::provisions_token();
    net.spawn(async move {
        let resolved = resolve(target, tokens, own_backend).await;
        delivery.deliver(move |s, _| {
            if s.fixture || s.workspace.connect_gen != generation {
                // Superseded: whatever it opened (a tunnel, also one a failed
                // resolve dropped) has no adopter.
                drop(resolved);
                super::retain_tunnels(s);
                return false;
            }
            match resolved {
                Ok(r) => open(s, generation, mode, r),
                Err(p) => failed(s, mode, p),
            }
            true
        });
    });
}

pub(super) async fn resolve(
    target: Target,
    tokens: Arc<dyn TokenStore>,
    own_backend: bool,
) -> Result<Resolved, Problem> {
    match target {
        Target::ThisMac => this_mac(false, own_backend).await,
        Target::Override => this_mac(true, false).await,
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

async fn this_mac(override_url: bool, own_backend: bool) -> Result<Resolved, Problem> {
    let conn = Connection::discover().map_err(|e| discover_problem(&e, own_backend))?;
    if override_url {
        check_transport(&conn.base_url).map_err(|e| {
            Problem::Blocked(format!("IRONCLAW_DESKTOP_URL ({}): {e}", conn.base_url))
        })?;
    }
    // This Mac's backend may predate the descriptor (it is whatever is
    // installed): a 404 is no reason to stop. A wrong protocol is.
    let descriptor = match fetch_descriptor(&conn.base_url, super::DESCRIPTOR_TIMEOUT).await {
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
        route_rank: 0,
    })
}

/// Why This Mac could not be found. A missing token, when this app runs
/// the backend, means it has not finished its first start (it writes the
/// token then): unreachable, so the launch keeps waiting and retries
/// instead of stopping on a screen that asks for a terminal command. A
/// backend the app does not start (the switch off, `IRONCLAW_DESKTOP_URL`)
/// makes no token, so there it stays the person's to fix.
pub(super) fn discover_problem(e: &ClientError, own_backend: bool) -> Problem {
    if e.is_no_token() && own_backend {
        Problem::Unreachable(e.to_string())
    } else {
        Problem::Blocked(e.to_string())
    }
}

/// Open (or reuse) the tunnel of an SSH route: its lease and the origin the
/// client uses through it.
async fn open_tunnel(
    route: &Route,
    label: &str,
) -> Result<(super::tunnel::Lease, String), Problem> {
    let target = SshTarget::from_url(&route.url).map_err(|_| {
        Problem::Blocked(format!(
            "{label}'s SSH address ({}) is not valid. Pair it again in Settings → Computers.",
            route.url
        ))
    })?;
    let port = route.remote_port.unwrap_or(ssh::DEFAULT_BACKEND_PORT);
    let opened = tokio::task::spawn_blocking(move || super::tunnel::open(&target, port))
        .await
        .map_err(|e| Problem::Unreachable(format!("SSH tunnel: {e}")))?;
    match opened {
        Ok((local, lease)) => Ok((lease, ssh::tunnel_origin(local))),
        Err(e) => Err(ssh_problem(e)),
    }
}

/// Start the backend over an SSH route and wait for its descriptor
/// through the open tunnel.
async fn start_over_ssh(route: &Route, origin: &str) -> Option<EnvironmentDescriptor> {
    let target = SshTarget::from_url(&route.url).ok()?;
    let started = tokio::task::spawn_blocking(move || ssh::start_backend(&target, ssh::run_ssh))
        .await
        .ok()?;
    if let Err(e) = started {
        tracing::debug!(error = %e, "could not start the backend over SSH");
        return None;
    }
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if let Ok(d) = fetch_descriptor(origin, super::DESCRIPTOR_TIMEOUT).await {
            return Some(d);
        }
    }
    None
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
    // not answer. Learned routes go only to confirmed tailnet peers.
    let mut peers: Option<Vec<std::net::IpAddr>> = None;
    for (rank, route) in preferred_routes(&env.routes).into_iter().enumerate() {
        let (origin, tunnel) = if route.kind == RouteKind::Ssh {
            match open_tunnel(route, &label).await {
                Ok((key, origin)) => (origin, Some(key)),
                Err(p) => {
                    last = p;
                    continue;
                }
            }
        } else if route.learned {
            if peers.is_none() {
                peers = Some(
                    tokio::task::spawn_blocking(zlo_client::tailnet::peer_addresses)
                        .await
                        .unwrap_or_default(),
                );
            }
            match check_learned_transport(&route.url, peers.as_deref().unwrap_or_default()) {
                Ok(o) => (o, None),
                // Not a tailnet peer right now: skip it quietly.
                Err(_) => continue,
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
        match fetch_descriptor(&origin, super::DESCRIPTOR_TIMEOUT).await {
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
                    route_rank: rank,
                });
            }
            Err(ClientError::Network(_)) if tunnel.is_some() => {
                // Nothing answers through the tunnel: start `zlo serve`
                // there (ZLO owns that server), then look again.
                if let Some(d) = start_over_ssh(route, &origin).await {
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
                        route_rank: rank,
                    });
                }
                last = Problem::Unreachable(format!(
                    "The SSH tunnel to {label} is open, but IronClaw isn't answering there. Check that `zlo serve` runs on {label}."
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
        Err(e) => {
            drop(r.tunnel);
            super::retain_tunnels(s);
            return failed(s, mode, Problem::Blocked(e.to_string()));
        }
    };
    let delivery = s.rt.delivery();
    let job = backend.spawn(|c| async move {
        let session = c.session().await?;
        let inbox = c.inbox().await?;
        Ok::<_, ClientError>((session, inbox))
    });
    let computer = r.computer;
    let descriptor = r.descriptor;
    let tunnel = r.tunnel;
    let route_rank = r.route_rank;
    // The UI thread holds the handle until the answer: its runtime is
    // never dropped on one of its own threads.
    // Tagged with this attempt, so a stale answer cannot take a newer
    // attempt's handle. Replacing an older attempt's handle drops it here,
    // on the UI thread.
    s.remote.page.pending_backend = Some((generation, backend.clone()));
    backend.spawn(move |_| async move {
        let result = crate::jobs::flatten(job.await);
        delivery.deliver(move |s, tx| {
            if s.fixture {
                return false;
            }
            let backend = match claim_pending(s, generation) {
                Claim::Stale => {
                    let had_tunnel = tunnel.is_some();
                    drop(tunnel);
                    if had_tunnel {
                        super::retain_tunnels(s);
                    }
                    return false;
                }
                Claim::Missing => {
                    drop(tunnel);
                    super::retain_tunnels(s);
                    failed(
                        s,
                        mode,
                        Problem::Unreachable("The connection was replaced. Trying again.".into()),
                    );
                    return true;
                }
                Claim::Mine(backend) => backend,
            };
            match result {
                Ok((session, inbox)) => {
                    s.remote.tunnel = tunnel.as_ref().map(|l| l.key().to_string());
                    drop(tunnel);
                    s.remote.route_rank = route_rank;
                    switched(s, mode, backend, computer, descriptor, session, inbox, tx);
                }
                Err(e) => {
                    drop(tunnel);
                    super::retain_tunnels(s);
                    // The launch screen giving way to the failure is the
                    // answer the person is waiting on: it crossfades (a
                    // reply lands at once otherwise). A retry that fails
                    // again leaves the screen as it was.
                    let before = std::mem::discriminant(&s.conn);
                    failed(s, mode, problem_of(&computer, &e));
                    if std::mem::discriminant(&s.conn) != before {
                        tx.animated();
                    }
                }
            }
            true
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

pub(super) fn problem_of(computer: &Computer, e: &ClientError) -> Problem {
    match (e, computer) {
        (ClientError::Network(_), _) => Problem::Unreachable(e.to_string()),
        (e, Computer::Remote(r)) if e.is_unauthorized() => Problem::Blocked(format!(
            "{} turned this Mac's token down (it may have been revoked). Pair it again in Settings → Computers.",
            r.label
        )),
        (e, Computer::ThisMac) if e.is_unauthorized() => Problem::Blocked(format!(
            "{e}\n\nThe program answering on this Mac's ZLO address turned ZLO's key down. \
             Another app may be using that address, or ZLO's engine was set up by hand. \
             Triage looks into it for you."
        )),
        (e, _) => Problem::Blocked(e.to_string()),
    }
}

/// The new computer answered: everything of the old one goes, and the app
/// reloads on the new one. `tx` is the reply's commit, which
/// `workspace::connected` animates when it replaces the launch screen.
#[allow(clippy::too_many_arguments)]
pub(super) fn switched(
    s: &mut State,
    mode: Mode,
    backend: Backend,
    computer: Computer,
    descriptor: Option<EnvironmentDescriptor>,
    session: SessionInfo,
    inbox: InboxList,
    tx: &mut rune::Tx,
) {
    let old = s.onboarding.computer.key();
    let changed = old != computer.key() || mode == Mode::Switch;
    // Prompts sent just before leaving a computer: it becomes a peer below,
    // and follows them until its backend lists their runs.
    let mut left_sent = None;
    if changed {
        if let Some(id) = crate::workspace::active_id(s) {
            s.remote.last_thread.insert(old.clone(), id.to_string());
        }
        if old != computer.key() {
            let mut sent = std::mem::take(&mut s.workspace.inbox.sent);
            sent.retain(|_, entry| entry.live());
            left_sent = Some((old, sent)).filter(|(_, sent)| !sent.is_empty());
        }
        reset_for_switch(s);
    }
    let key = computer.key();
    tracing::debug!(
        computer = %key,
        descriptor = descriptor.is_some(),
        "connected"
    );
    // Commit the pending switch target before saving routes or registry.
    let committed_switch = if let Some(sw) = s.remote.switching.take() {
        s.remote.registry.active = sw.target;
        s.remote.legacy_active = false;
        true
    } else {
        false
    };
    // The routes it reports now (its private listener's tailnet
    // addresses) replace the learned ones; the person's are untouched.
    let learned_routes = if let (Computer::Remote(r), Some(d)) = (&computer, descriptor.as_ref()) {
        s.remote
            .registry
            .learn_routes(&r.environment_id, &d.addresses)
    } else {
        false
    };
    adopt(s, computer, descriptor);
    s.rt.backend = Some(backend);
    super::failback::schedule(s);
    // Keep the focused computer's tunnel and every peer's tunnel open.
    super::retain_tunnels(s);
    if (committed_switch || learned_routes)
        && let Err(e) = super::save_registry(s)
    {
        tracing::warn!(error = %e, "computer pick not saved");
    }
    super::sync_local_backend(s);
    // A route refresh keeps the thread in view (its stream reopens on the
    // new route); a switch reopens what was last open on that computer.
    let reopen = if mode == Mode::Refresh {
        crate::workspace::active_id(s).map(str::to_string)
    } else {
        s.remote.last_thread.get(&key).cloned()
    };
    crate::workspace::connected(s, session, inbox, reopen, tx);
    super::pairing::connected(s);
    super::linked::start_polling(s);
    super::peers::sync(s);
    if let Some((key, sent)) = left_sent {
        super::peers::carry_sent(s, &key, sent);
    }
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
    // An installer of this Mac's agents stops: its rescan would reach the
    // other computer.
    crate::cli_install::computer_switched(s);
    // An unsent triage draft moves to the next computer instead.
    crate::workspace::carry_triage(s);
    s.workspace.draft = None;
    s.workspace.telemetry = None;
    s.workspace.titles = Default::default();
    s.workspace.agents.reset_for_switch();
    s.workspace.run_clock = Default::default();
    s.workspace.confirm_delete = None;
    s.workspace.inbox.reset_for_switch();
    s.workspace.timers.inbox = None;
    s.workspace.timers.wake = None;
    s.workspace.focused_unread = 0;
    s.workspace.unread = 0;
    crate::workspace::reset_for_switch(s);
    // Warm threads belong to the computer they were fetched from; the same
    // thread id on another computer is another thread.
    s.prewarm = Default::default();
    s.remote.descriptor = None;
    s.remote.focused_computer_readiness = None;
    s.remote.page.devices = None;
    s.remote.page.issued = None;
    crate::onboarding::computer_switched(s);
}

fn failed(s: &mut State, mode: Mode, problem: Problem) {
    if mode == Mode::Refresh {
        // The working connection stays; the check tries again later.
        tracing::debug!(
            ?problem,
            "route refresh failed; keeping the connection in use"
        );
        // A tunnel the failed candidate opened has no adopter.
        super::retain_tunnels(s);
        super::failback::schedule(s);
        return;
    }
    // The attempt a thread click rode on is over.
    s.remote.pending_thread = None;
    // A tunnel the failed attempt opened has no adopter.
    super::retain_tunnels(s);
    if mode == Mode::Switch
        && let Some(sw) = s.remote.switching.take()
    {
        // The old computer stays on screen, and active (`registry.active` was
        // never mutated to `sw.target`).
        let key = active_key(&sw.target);
        s.remote.registry.active = sw.previous;
        s.workspace.carried_draft = None;
        let line = match &problem {
            Problem::Unreachable(m) | Problem::Blocked(m) => m.clone(),
        };
        tracing::warn!(computer = %key, problem = %line, "switch failed");
        s.remote.problems.insert(key, line);
        super::retain_tunnels(s);
        super::sync_local_backend(s);
        super::peers::sync(s);
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
    super::peers::sync(s);
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
                super::peers::sync(s);
            }
            Err(e) => tracing::debug!(error = %e, "paired computer not moved yet; next launch tries again"),
        });
    });
}
