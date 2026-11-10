//! The computers ZLO works on (T3 Code's environments, with Tailscale for
//! the relay), and the one switch for "the backend runs on another
//! computer". Features that touch this Mac's disk, shell, git or
//! permissions on the backend's behalf ask [`backend_is_remote`] first and
//! take their remote branch (phase 7 of the remote environments plan).
//!
//! The registry is `~/Library/Application Support/ZLO/environments.json`
//! (no secrets; `zlo_client::environments`), each computer's device token
//! sits in the Keychain (`zlo_client::keychain`), and one computer is
//! active at a time. `connect.rs` resolves and opens a connection (the
//! descriptor first, every time), `pairing.rs` adds computers and runs the
//! Computers page's actions, `tunnel.rs` keeps the SSH route's tunnel.
//!
//! A Light bundle (`bundle.sh --light`, no `zlo-backend` inside) is
//! app-only ([`RemoteState::app_only`]): no backend on this Mac, This Mac
//! is not offered, and with no computer yet the window is "Add a
//! computer" ([`needs_computer`]). Plan:
//! `docs/internal/plans/2026-11-08-zlo-remote-environments.md`.

pub(crate) mod connect;
pub mod pairing;
pub mod tunnel;
pub mod updates;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use zlo_client::environment_access::EnvironmentDescriptor;
use zlo_client::environments::{self, Active, LegacyGateway, Registry, SavedEnvironment, host_of};
use zlo_client::keychain::TokenStore;
use zlo_client::tailnet::Discovery;

use crate::jobs::Generation;
use crate::onboarding::gateway::Computer;
use crate::state::{Conn, State};

/// How long a computer gets to answer with its descriptor.
const DESCRIPTOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

pub use connect::{connect, switch_to};

/// Every async gateway completion is tagged with the connection it was
/// asked on; one from an earlier connection (another computer, or before a
/// reconnect) is dropped (`jobs::run`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnTag {
    pub environment: String,
    pub generation: Generation,
}

/// What the Computers page says about one computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvState {
    Connected,
    Connecting,
    /// Its tailnet peer is offline.
    Asleep,
    /// Not usable until someone acts (re-pair, update a side), or not
    /// reachable on the last try.
    Blocked(String),
    Saved,
}

pub struct RemoteState {
    pub registry: Registry,
    /// `environments.json` could not be read: the connect screen says so
    /// and nothing starts a backend on this Mac.
    pub registry_error: Option<String>,
    /// The old `gateway.json` computer, not moved into the registry yet.
    pub legacy: Option<LegacyGateway>,
    /// ...and it is the computer in use (there was no registry yet).
    pub legacy_active: bool,
    pub tokens: Arc<dyn TokenStore>,
    /// The live connection's tag (`jobs::run`).
    pub tag: ConnTag,
    /// The connected computer's descriptor (`None` for a backend too old
    /// to serve one, this Mac only).
    pub descriptor: Option<EnvironmentDescriptor>,
    /// The last problem per computer (`environment_id`).
    pub problems: HashMap<String, String>,
    /// The thread last open per computer key, reopened on switching back.
    pub last_thread: HashMap<String, String>,
    /// A switch in flight: the old screen stays until the new one answers.
    pub switching: Option<Switching>,
    pub page: pairing::PageState,
    /// The SSH tunnel the connection in use goes through (`tunnel::key`).
    pub tunnel: Option<String>,
    /// A Light bundle: this Mac never runs a backend and is not offered
    /// as a computer.
    pub app_only: bool,
    /// "Check for updates" / "Update all machines" rows (`updates.rs`).
    pub updates: updates::UpdatesState,
}

#[derive(Clone, Debug)]
pub struct Switching {
    pub target: Active,
    pub previous: Active,
}

impl Default for RemoteState {
    fn default() -> Self {
        Self {
            registry: Registry::default(),
            registry_error: None,
            legacy: None,
            legacy_active: false,
            tokens: default_tokens(),
            tag: ConnTag::default(),
            descriptor: None,
            problems: HashMap::new(),
            last_thread: HashMap::new(),
            switching: None,
            page: Default::default(),
            tunnel: None,
            app_only: false,
            updates: Default::default(),
        }
    }
}

#[cfg(not(test))]
fn default_tokens() -> Arc<dyn TokenStore> {
    if rune::is_headless() {
        // Scenes and scripts never touch the user's Keychain.
        Arc::new(zlo_client::keychain::MemoryTokens::default())
    } else {
        Arc::new(zlo_client::keychain::Keychain)
    }
}

#[cfg(test)]
fn default_tokens() -> Arc<dyn TokenStore> {
    Arc::new(zlo_client::keychain::MemoryTokens::default())
}

// ── launch ───────────────────────────────────────────────────────────────

/// What `main` reads before the window exists: the registry and any
/// `gateway.json` still to move.
pub struct Launch {
    registry: Result<Option<Registry>, String>,
    legacy: Option<LegacyGateway>,
}

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir()
}

pub fn registry_path() -> Option<PathBuf> {
    config_dir().map(|d| environments::registry_file(&d))
}

fn legacy_path() -> Option<PathBuf> {
    config_dir().map(|d| environments::legacy_file(&d))
}

/// The problem line for an unreadable `gateway.json` with no computers
/// list beside it. "Use this Mac" clears it by saving a list.
fn legacy_unreadable(error: &str) -> String {
    format!("ZLO could not read the computer it was paired with (gateway.json): {error}")
}

impl Launch {
    pub fn read() -> Self {
        let registry = match registry_path() {
            Some(path) => environments::load_registry(&path).map_err(|e| e.0),
            None => Ok(None),
        };
        let legacy = legacy_path().map(|p| environments::load_legacy(&p).map_err(|e| e.0));
        Self::from_files(registry, legacy)
    }

    /// The launch decision from what the two files held.
    fn from_files(
        mut registry: Result<Option<Registry>, String>,
        legacy: Option<Result<Option<LegacyGateway>, String>>,
    ) -> Self {
        let legacy = match legacy {
            Some(Ok(legacy)) => legacy,
            Some(Err(e)) => {
                // Not moved, not deleted: it stays for the next launch.
                tracing::warn!(error = %e, "old paired computer unreadable");
                if matches!(registry, Ok(None)) {
                    // With no computers list yet, that file decides which
                    // computer is in use: unreadable, it is a connection
                    // problem, never a silent "This Mac".
                    registry = Err(legacy_unreadable(&e));
                }
                None
            }
            None => None,
        };
        Self { registry, legacy }
    }

    /// The legacy computer is the one in use: it was paired and there is
    /// no registry yet to say otherwise.
    fn legacy_active(&self) -> bool {
        self.legacy.is_some() && matches!(self.registry, Ok(None))
    }

    /// Whether ZLO may start this Mac's backend at launch.
    pub fn runs_local_backend(&self) -> bool {
        !crate::service::light_bundle()
            && local_backend_allowed(
                self.registry
                    .as_ref()
                    .map(|r| r.clone().unwrap_or_default()),
                self.legacy_active(),
                env_override(),
            )
    }
}

/// The start decision: never with `IRONCLAW_DESKTOP_URL`, an unreadable
/// registry, a computer not yet moved from `gateway.json`, the switch off,
/// or another computer active.
pub fn local_backend_allowed(
    registry: Result<Registry, &String>,
    legacy_active: bool,
    env_override: bool,
) -> bool {
    !env_override && !legacy_active && registry.is_ok_and(|r| r.runs_local_backend())
}

pub fn env_override() -> bool {
    std::env::var("IRONCLAW_DESKTOP_URL").is_ok_and(|v| !v.trim().is_empty())
}

impl RemoteState {
    /// Take what `main` read.
    pub fn launched(&mut self, launch: Launch) {
        self.legacy_active = launch.legacy_active();
        self.legacy = launch.legacy;
        match launch.registry {
            Ok(registry) => self.registry = registry.unwrap_or_default(),
            Err(e) => self.registry_error = Some(e),
        }
        self.set_app_only(crate::service::light_bundle());
    }

    /// App-only: a list left on This Mac (by the full app, or by default)
    /// starts on the first saved computer instead (in memory; the next pick
    /// is saved).
    pub fn set_app_only(&mut self, app_only: bool) {
        self.app_only = app_only;
        if app_only
            && self.registry.active == Active::ThisMac
            && let Some(first) = self.registry.environments.first()
        {
            self.registry.active = Active::Environment(first.environment_id.clone());
        }
    }
}

/// Whether ZLO may run (start, restart) the backend on this Mac now.
pub fn runs_local_backend(s: &State) -> bool {
    !s.remote.app_only
        && local_backend_allowed(
            match &s.remote.registry_error {
                Some(e) => Err(e),
                None => Ok(s.remote.registry.clone()),
            },
            s.remote.legacy_active,
            env_override(),
        )
}

/// Tell the service supervisor what it may do, and start the backend
/// when it may (it is a no-op when one already runs).
pub fn sync_local_backend(s: &State) {
    let allowed = runs_local_backend(s);
    crate::service::set_allowed(allowed);
    if allowed && !s.fixture {
        crate::service::start();
    }
}

/// Write the registry (small; on the UI thread, so writes stay ordered).
pub fn save_registry(s: &mut State) -> Result<(), String> {
    if s.fixture {
        return Ok(());
    }
    let path = registry_path().ok_or("No settings folder on this Mac.")?;
    let bytes = s.remote.registry.to_json().map_err(|e| e.to_string())?;
    crate::persist::atomic_write(&path, &bytes).map_err(|e| {
        tracing::warn!(error = %e, "computers list not saved");
        format!("ZLO could not save its list of computers: {e}")
    })?;
    // A registry written by hand is the new truth.
    s.remote.registry_error = None;
    Ok(())
}

/// The failed screen's "Use this Mac": an explicit pick, saved. An
/// unreadable computers list is set aside (`environments.json.unreadable`)
/// and a fresh one started; a `gateway.json` computer stays to be moved.
pub fn use_this_mac(s: &mut State) {
    if s.remote.registry_error.take().is_some()
        && let Some(path) = registry_path()
    {
        let aside = path.with_extension("json.unreadable");
        if let Err(e) = std::fs::rename(&path, &aside) {
            tracing::warn!(error = %e, "unreadable computers list not set aside");
        }
        s.remote.registry = Registry::default();
    }
    s.remote.legacy_active = false;
    s.remote.registry.active = Active::ThisMac;
    s.remote.switching = None;
    if let Err(e) = save_registry(s) {
        tracing::warn!(error = %e, "computer pick not saved");
    }
    s.workspace.connect_attempt = 0;
    s.launched = std::time::Instant::now();
    connect(s);
}

/// Delete `gateway.json` after its computer was saved in the registry.
fn forget_legacy(s: &mut State) {
    s.remote.legacy = None;
    s.remote.legacy_active = false;
    if s.fixture {
        return;
    }
    if let Some(path) = legacy_path()
        && let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(error = %e, "old paired computer file not removed");
    }
}

/// The shared network runtime for calls made without a connection yet
/// (descriptors, pairing, discovery). `None` if it cannot start.
pub(crate) fn net() -> Option<&'static tokio::runtime::Runtime> {
    static NET: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();
    NET.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("zlo-remote")
            .enable_all()
            .build()
            .map_err(|e| tracing::warn!(error = %e, "remote network runtime did not start"))
            .ok()
    })
    .as_ref()
}

// ── queries ──────────────────────────────────────────────────────────────

/// The backend ZLO talks to is not on this Mac.
pub fn backend_is_remote(s: &State) -> bool {
    matches!(s.onboarding.computer, Computer::Remote(_))
}

/// The computer the backend runs on, for "Runs on <computer>" lines.
pub fn computer_label(s: &State) -> String {
    s.onboarding.computer.label()
}

/// App-only with no computer to work on: the window asks for one.
pub fn needs_computer(s: &State) -> bool {
    s.remote.app_only
        && s.remote.registry.active == Active::ThisMac
        && !s.remote.legacy_active
        && s.remote.registry_error.is_none()
        && !env_override()
}

/// The remote's SSH destination (`user@host` of its SSH route, else its
/// tailnet name or address), `None` on this Mac.
pub fn ssh_host(s: &State) -> Option<String> {
    let Computer::Remote(r) = &s.onboarding.computer else {
        return None;
    };
    let routes = &s.remote.registry.get(&r.environment_id)?.routes;
    if let Some(target) = routes
        .iter()
        .filter(|r| r.kind == environments::RouteKind::Ssh)
        .find_map(|r| zlo_client::ssh::SshTarget::from_url(&r.url).ok())
    {
        return Some(target.destination());
    }
    Some(host_of(&routes.first()?.url).to_string())
}

/// How a file picked from this Mac's disk reaches the backend.
pub fn attachment_backend(s: &State) -> zlo_client::attachments::Backend {
    if backend_is_remote(s) {
        zlo_client::attachments::Backend::Remote
    } else {
        zlo_client::attachments::Backend::Local
    }
}

/// A send to a remote backend refuses files staged by this Mac's path.
pub fn check_outgoing(
    s: &State,
    files: &[zlo_client::attachments::StagedFile],
) -> Result<(), String> {
    if backend_is_remote(s) {
        zlo_client::attachments::local_paths_for_remote(files)
    } else {
        Ok(())
    }
}

/// The one-line reason a local-only feature is off while remote.
pub fn unavailable_line(computer: &str) -> String {
    format!("Runs on {computer} — not available remotely yet")
}

/// The key drafts, the selected thread and caches are scoped by.
pub fn scope(s: &State) -> String {
    s.onboarding.computer.key()
}

/// A completion asked on `tag` still applies.
pub fn lands(s: &State, tag: &ConnTag) -> bool {
    s.remote.tag == *tag
}

fn active_key(active: &Active) -> String {
    match active {
        Active::ThisMac => environments::THIS_MAC.to_string(),
        Active::Environment(id) => id.clone(),
    }
}

/// The saved computers, in the registry's order.
pub fn saved(s: &State) -> &[SavedEnvironment] {
    &s.remote.registry.environments
}

/// One computer's state (`key`: `this-mac` or an environment id).
pub fn state_of(s: &State, key: &str) -> EnvState {
    if let Some(sw) = &s.remote.switching
        && active_key(&sw.target) == key
    {
        return EnvState::Connecting;
    }
    let active = s.onboarding.computer.key() == key
        && (key == environments::THIS_MAC || s.rt.backend.is_some() || s.fixture);
    if active {
        match &s.conn {
            Conn::Ready => return EnvState::Connected,
            Conn::Connecting => return EnvState::Connecting,
            Conn::Failed { .. } => {}
        }
    }
    if asleep(s, key) {
        return EnvState::Asleep;
    }
    if let Some(problem) = s.remote.problems.get(key) {
        return EnvState::Blocked(problem.clone());
    }
    if active && let Conn::Failed { message, .. } = &s.conn {
        return EnvState::Blocked(first_line(message));
    }
    EnvState::Saved
}

fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or_default().to_string()
}

/// A saved computer whose tailnet peer reports `Online: false`.
fn asleep(s: &State, key: &str) -> bool {
    let (Some(discovery), Some(env)) = (&s.remote.page.discovery, s.remote.registry.get(key))
    else {
        return false;
    };
    asleep_in(discovery, env)
}

fn asleep_in(discovery: &Discovery, env: &SavedEnvironment) -> bool {
    env.routes.iter().any(|route| {
        let host = host_of(&route.url).to_ascii_lowercase();
        discovery
            .peers
            .iter()
            .any(|p| p.dns_name == host && !p.online)
    })
}

#[cfg(test)]
mod tests;
