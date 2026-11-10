//! The computers ZLO can connect to (T3 Code's environments), as saved on
//! this Mac, and the rules every address and pairing link passes.
//!
//! Wire contract v1 (`docs/internal/plans/2026-11-08-zlo-remote-environments.md`):
//! `~/Library/Application Support/ZLO/environments.json` holds
//! `{run_local_backend, active, environments: [{environment_id, label,
//! routes: [{kind, url, remote_port?}]}]}` and never a secret (`kind` is
//! `tailnet`, `direct` or `ssh`); device tokens live in the Keychain
//! (`crate::keychain`).
//!
//! Transport rule, one function for links, saved routes and
//! `IRONCLAW_DESKTOP_URL` ([`check_transport`]): https with normal
//! certificate checks, or plain http to this Mac only.

use std::path::{Path, PathBuf};

use ironclaw_product_contracts::environment_access::{
    ENVIRONMENT_PROTOCOL_VERSION, EnvironmentDescriptor, display_pairing_code,
    normalize_pairing_code,
};
use serde::{Deserialize, Serialize};

/// `active` when this Mac's own backend is the one in use.
pub const THIS_MAC: &str = "this-mac";

/// How a route reaches its computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    /// The `tailscale serve` HTTPS name (`https://studio.tail9915dd.ts.net`).
    Tailnet,
    /// Any other https origin, or this Mac's loopback.
    Direct,
    /// An SSH tunnel to the computer's loopback backend
    /// (`ssh://user@host:22`; `crate::ssh`). Tried after the others.
    Ssh,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub kind: RouteKind,
    /// Origin without a trailing slash; `ssh://user@host:port` for
    /// [`RouteKind::Ssh`].
    pub url: String,
    /// SSH only: the backend's loopback port on the computer, when not
    /// [`crate::ssh::DEFAULT_BACKEND_PORT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_port: Option<u16>,
}

impl Route {
    /// A route for an origin that already passed [`check_transport`].
    pub fn for_origin(origin: &str) -> Self {
        let kind = if host_of(origin).to_ascii_lowercase().ends_with(".ts.net") {
            RouteKind::Tailnet
        } else {
            RouteKind::Direct
        };
        Self {
            kind,
            url: origin.to_string(),
            remote_port: None,
        }
    }

    /// An SSH route (its target already passed `crate::ssh::parse_target`).
    pub fn ssh(target: &crate::ssh::SshTarget, remote_port: u16) -> Self {
        Self {
            kind: RouteKind::Ssh,
            url: target.to_url(),
            remote_port: (remote_port != crate::ssh::DEFAULT_BACKEND_PORT).then_some(remote_port),
        }
    }

    /// What a person reads for this route: the host, or `user@host · SSH`.
    pub fn caption(&self) -> String {
        match self.kind {
            RouteKind::Ssh => match crate::ssh::SshTarget::from_url(&self.url) {
                Ok(t) => format!("{} · SSH", t.destination()),
                Err(_) => "SSH".into(),
            },
            RouteKind::Tailnet | RouteKind::Direct => host_of(&self.url).to_string(),
        }
    }
}

/// The order routes are tried in: tailnet and direct ones as saved, then
/// SSH (a tunnel is opened only when nothing else answers).
pub fn preferred_routes(routes: &[Route]) -> Vec<&Route> {
    let (ssh, other): (Vec<&Route>, Vec<&Route>) =
        routes.iter().partition(|r| r.kind == RouteKind::Ssh);
    other.into_iter().chain(ssh).collect()
}

/// One saved computer. No secrets: its token is in the Keychain under its
/// `environment_id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedEnvironment {
    pub environment_id: String,
    pub label: String,
    /// In preference order.
    pub routes: Vec<Route>,
    /// The Keychain holds a migrated master token (an old `#token=` link),
    /// not a device token: re-pair for one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub legacy_token: bool,
}

/// Which computer ZLO works on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Active {
    #[default]
    ThisMac,
    Environment(String),
}

impl Active {
    fn from_wire(raw: &str) -> Self {
        match raw.trim() {
            "" | THIS_MAC => Active::ThisMac,
            id => Active::Environment(id.to_string()),
        }
    }

    fn to_wire(&self) -> String {
        match self {
            Active::ThisMac => THIS_MAC.to_string(),
            Active::Environment(id) => id.clone(),
        }
    }
}

/// `environments.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registry {
    pub run_local_backend: bool,
    pub active: Active,
    pub environments: Vec<SavedEnvironment>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            run_local_backend: true,
            active: Active::ThisMac,
            environments: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default = "yes")]
    run_local_backend: bool,
    #[serde(default)]
    active: String,
    #[serde(default)]
    environments: Vec<SavedEnvironment>,
}

fn yes() -> bool {
    true
}

impl Registry {
    pub fn get(&self, environment_id: &str) -> Option<&SavedEnvironment> {
        self.environments
            .iter()
            .find(|e| e.environment_id == environment_id)
    }

    /// Add or replace by id (a re-pair refreshes label and routes).
    pub fn upsert(&mut self, env: SavedEnvironment) {
        match self
            .environments
            .iter_mut()
            .find(|e| e.environment_id == env.environment_id)
        {
            Some(slot) => *slot = env,
            None => self.environments.push(env),
        }
    }

    /// Drop a computer; an active one hands over to this Mac.
    pub fn remove(&mut self, environment_id: &str) {
        self.environments
            .retain(|e| e.environment_id != environment_id);
        if self.active == Active::Environment(environment_id.to_string()) {
            self.active = Active::ThisMac;
        }
    }

    /// Whether ZLO should run (start, restart) the backend on this Mac.
    pub fn runs_local_backend(&self) -> bool {
        self.run_local_backend && self.active == Active::ThisMac
    }

    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(&RegistryFile {
            run_local_backend: self.run_local_backend,
            active: self.active.to_wire(),
            environments: self.environments.clone(),
        })
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let file: RegistryFile = serde_json::from_slice(bytes)?;
        let mut registry = Self {
            run_local_backend: file.run_local_backend,
            active: Active::from_wire(&file.active),
            environments: file.environments,
        };
        // An active id with no entry is a problem the connect screen shows
        // (never a silent switch), so it stays as written.
        registry
            .environments
            .dedup_by(|a, b| a.environment_id == b.environment_id);
        Ok(registry)
    }
}

/// Why `environments.json` could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RegistryError(pub String);

/// `~/Library/Application Support/ZLO/environments.json`.
pub fn registry_file(config_dir: &Path) -> PathBuf {
    config_dir.join("ZLO").join("environments.json")
}

/// The old single-computer file (url + master token), migrated once.
pub fn legacy_file(config_dir: &Path) -> PathBuf {
    config_dir.join("ZLO").join("gateway.json")
}

/// Read the registry. A missing file is the default (this Mac); an
/// unreadable one is an error the caller shows.
pub fn load_registry(path: &Path) -> Result<Option<Registry>, RegistryError> {
    match std::fs::read(path) {
        Ok(bytes) => Registry::from_json(&bytes).map(Some).map_err(|e| {
            RegistryError(format!(
                "ZLO could not read its list of computers ({}): {e}",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RegistryError(format!(
            "ZLO could not read its list of computers ({}): {e}",
            path.display()
        ))),
    }
}

/// The old `gateway.json`.
#[derive(Clone, Deserialize)]
pub struct LegacyGateway {
    pub url: String,
    pub token: String,
}

impl std::fmt::Debug for LegacyGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LegacyGateway")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// `Ok(None)` when there is nothing to migrate.
pub fn load_legacy(path: &Path) -> Result<Option<LegacyGateway>, RegistryError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            RegistryError(format!(
                "ZLO could not read the paired computer ({}): {e}",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RegistryError(format!(
            "ZLO could not read the paired computer ({}): {e}",
            path.display()
        ))),
    }
}

/// The routes a migrated or legacy-linked computer gets: its `tailscale
/// serve` URL first, then the old address if it still passes the transport
/// rule. Empty when neither does (the old link was plain http over the
/// tailnet and remote access is not enabled there).
pub fn routes_for(old_origin: &str, descriptor: &EnvironmentDescriptor) -> Vec<Route> {
    let mut routes = Vec::new();
    if let Some(tailnet) = descriptor.tailnet_url.as_deref()
        && let Ok(origin) = check_transport(tailnet)
    {
        routes.push(Route {
            kind: RouteKind::Tailnet,
            url: origin,
            remote_port: None,
        });
    }
    if let Ok(origin) = check_transport(old_origin)
        && !routes.iter().any(|r| r.url == origin)
    {
        routes.push(Route::for_origin(&origin));
    }
    routes
}

// ── transport rule ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("Paste the whole address, starting with https://.")]
    NotAUrl,
    #[error("The address is not a computer's address.")]
    BadHost,
    #[error(
        "Plain http is allowed only to this Mac. Use the computer's https address (its Tailscale name), or a pairing code."
    )]
    PlainHttp,
}

/// The one transport rule: `https://` anywhere (normal certificate checks),
/// `http://` only to loopback. Returns the normalized origin
/// (`scheme://authority`, lowercase scheme, no path or trailing slash).
pub fn check_transport(url: &str) -> Result<String, TransportError> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or(TransportError::NotAUrl)?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(TransportError::NotAUrl);
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains(char::is_whitespace)
        || host_of(authority).is_empty()
    {
        return Err(TransportError::BadHost);
    }
    if scheme == "http" && !is_loopback(host_of(authority)) {
        return Err(TransportError::PlainHttp);
    }
    Ok(format!("{scheme}://{authority}"))
}

fn is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// `https://studio.ts.net:8443/x` → `studio.ts.net`.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if let Some(v6) = authority.strip_prefix('[') {
        return v6.split(']').next().unwrap_or(authority);
    }
    authority.split(':').next().unwrap_or(authority)
}

// ── pairing input ────────────────────────────────────────────────────────

/// What a person pasted into "Add a computer".
#[derive(Clone, PartialEq, Eq)]
pub enum PairingInput {
    /// `https://host/pair#code=…` (`origin` set) or a bare code.
    Code {
        origin: Option<String>,
        code: String,
    },
    /// The legacy `…#token=` link (one release): a master token.
    LegacyToken { origin: String, token: String },
}

impl std::fmt::Debug for PairingInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code { origin, .. } => f
                .debug_struct("Code")
                .field("origin", origin)
                .field("code", &"<redacted>")
                .finish(),
            Self::LegacyToken { origin, .. } => f
                .debug_struct("LegacyToken")
                .field("origin", origin)
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairingInputError {
    #[error("Paste a pairing link (https://…/pair#code=…) or a code like 7KQ2-M9XD-4TRA.")]
    Unrecognized,
    #[error("A pairing code has 12 letters and digits, like 7KQ2-M9XD-4TRA.")]
    BadCode,
    #[error("The link has no code. It ends in #code=…")]
    NoCode,
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Parse a pasted link or code. A link's origin passes [`check_transport`].
pub fn parse_pairing_input(raw: &str) -> Result<PairingInput, PairingInputError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(PairingInputError::Unrecognized);
    }
    if !raw.contains("://") {
        return normalize_code(raw)
            .map(|code| PairingInput::Code { origin: None, code })
            .ok_or_else(|| {
                if raw.contains(['/', '#', '.', ':']) {
                    PairingInputError::Unrecognized
                } else {
                    PairingInputError::BadCode
                }
            });
    }
    let origin = check_transport(raw)?;
    let fragment = raw.split_once('#').map(|(_, f)| f).unwrap_or_default();
    let field = |name: &str| {
        fragment
            .split('&')
            .find_map(|kv| kv.strip_prefix(name))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    if let Some(code) = field("code=") {
        let code = normalize_code(&code).ok_or(PairingInputError::BadCode)?;
        return Ok(PairingInput::Code {
            origin: Some(origin),
            code,
        });
    }
    if let Some(token) = field("token=") {
        return Ok(PairingInput::LegacyToken { origin, token });
    }
    Err(PairingInputError::NoCode)
}

/// Any case, with or without dashes or spaces → `XXXX-XXXX-XXXX` (the
/// contract's Crockford rule).
pub fn normalize_code(raw: &str) -> Option<String> {
    normalize_pairing_code(raw).map(|c| display_pairing_code(&c))
}

/// The link a pairing code is shared as.
pub fn pairing_link(origin: &str, code: &str) -> String {
    format!("{}/pair#code={code}", origin.trim_end_matches('/'))
}

// ── descriptor check ─────────────────────────────────────────────────────

/// Why a descriptor blocks the connection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DescriptorProblem {
    #[error("This address reaches a different machine ({found}), not {expected}.")]
    DifferentMachine { expected: String, found: String },
    #[error("{label} runs a newer IronClaw than this ZLO understands. Update ZLO.")]
    UpdateApp { label: String },
    #[error("IronClaw on {label} is older than this ZLO. Update IronClaw on {label}.")]
    UpdateBackend { label: String },
}

/// Every connect: the id must match the saved one (when there is one) and
/// the protocol must be ours.
pub fn check_descriptor(
    descriptor: &EnvironmentDescriptor,
    expected: Option<(&str, &str)>,
) -> Result<(), DescriptorProblem> {
    if let Some((id, label)) = expected
        && descriptor.environment_id != id
    {
        return Err(DescriptorProblem::DifferentMachine {
            expected: label.to_string(),
            found: descriptor.label.clone(),
        });
    }
    let label = descriptor.label.clone();
    match descriptor
        .protocol_version
        .cmp(&ENVIRONMENT_PROTOCOL_VERSION)
    {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => Err(DescriptorProblem::UpdateApp { label }),
        std::cmp::Ordering::Less => Err(DescriptorProblem::UpdateBackend { label }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_product_contracts::environment_access::PairingMethod;

    fn descriptor(id: &str, protocol: u32, tailnet: Option<&str>) -> EnvironmentDescriptor {
        EnvironmentDescriptor {
            environment_id: id.into(),
            label: "Studio".into(),
            server_version: "0.41.0".into(),
            protocol_version: protocol,
            pairing: vec![PairingMethod::OneTimeCode],
            tailnet_url: tailnet.map(str::to_string),
            build_commit: None,
            self_update: false,
        }
    }

    #[test]
    fn the_transport_rule_allows_https_anywhere_and_http_only_to_loopback() {
        assert_eq!(
            check_transport("https://studio.tail9915dd.ts.net/"),
            Ok("https://studio.tail9915dd.ts.net".into())
        );
        assert_eq!(
            check_transport("HTTPS://box.example:8443/pair?x#y"),
            Ok("https://box.example:8443".into())
        );
        assert!(check_transport("http://127.0.0.1:3000").is_ok());
        assert!(check_transport("http://localhost:3000").is_ok());
        assert!(check_transport("http://[::1]:3000").is_ok());
        // The old "Tailscale counts as private" rule is gone.
        assert_eq!(
            check_transport("http://100.101.2.3:3000"),
            Err(TransportError::PlainHttp)
        );
        assert_eq!(
            check_transport("http://studio.tail1234.ts.net:3000"),
            Err(TransportError::PlainHttp)
        );
        assert_eq!(
            check_transport("http://192.168.1.4:3000"),
            Err(TransportError::PlainHttp)
        );
        assert_eq!(check_transport("ftp://x"), Err(TransportError::NotAUrl));
        assert_eq!(check_transport("studio:3000"), Err(TransportError::NotAUrl));
        assert_eq!(
            check_transport("https://user@evil"),
            Err(TransportError::BadHost)
        );
        assert_eq!(check_transport("https://"), Err(TransportError::BadHost));
    }

    #[test]
    fn pairing_links_codes_and_legacy_tokens_parse() {
        assert_eq!(
            parse_pairing_input(" https://studio.tail9915dd.ts.net/pair#code=7kq2-m9xd-4tra "),
            Ok(PairingInput::Code {
                origin: Some("https://studio.tail9915dd.ts.net".into()),
                code: "7KQ2-M9XD-4TRA".into()
            })
        );
        assert_eq!(
            parse_pairing_input("7kq2m9xd4tra"),
            Ok(PairingInput::Code {
                origin: None,
                code: "7KQ2-M9XD-4TRA".into()
            })
        );
        assert_eq!(
            parse_pairing_input("7KQ2 M9XD 4TRA"),
            Ok(PairingInput::Code {
                origin: None,
                code: "7KQ2-M9XD-4TRA".into()
            })
        );
        assert_eq!(
            parse_pairing_input("https://box.example#token=t0k"),
            Ok(PairingInput::LegacyToken {
                origin: "https://box.example".into(),
                token: "t0k".into()
            })
        );
        assert_eq!(
            parse_pairing_input("http://127.0.0.1:3000#a=b&token=t"),
            Ok(PairingInput::LegacyToken {
                origin: "http://127.0.0.1:3000".into(),
                token: "t".into()
            })
        );
        // An old plain-http tailnet link is refused now.
        assert_eq!(
            parse_pairing_input("http://100.101.2.3:3000#token=abc"),
            Err(PairingInputError::Transport(TransportError::PlainHttp))
        );
        assert_eq!(
            parse_pairing_input("https://box.example/pair"),
            Err(PairingInputError::NoCode)
        );
        assert_eq!(
            parse_pairing_input("https://box.example/pair#code=short"),
            Err(PairingInputError::BadCode)
        );
        assert_eq!(parse_pairing_input("ABC"), Err(PairingInputError::BadCode));
        assert_eq!(
            parse_pairing_input("studio.local:3000"),
            Err(PairingInputError::Unrecognized)
        );
        assert_eq!(
            parse_pairing_input(""),
            Err(PairingInputError::Unrecognized)
        );
        let debug = format!(
            "{:?}",
            parse_pairing_input("https://x.example#token=SECRET")
        );
        assert!(!debug.contains("SECRET"), "{debug}");
    }

    #[test]
    fn the_descriptor_check_names_the_machine_and_the_side_to_update() {
        let d = descriptor("env-1", ENVIRONMENT_PROTOCOL_VERSION, None);
        assert_eq!(check_descriptor(&d, Some(("env-1", "Studio"))), Ok(()));
        assert_eq!(check_descriptor(&d, None), Ok(()));
        assert!(matches!(
            check_descriptor(&d, Some(("env-2", "Mini"))),
            Err(DescriptorProblem::DifferentMachine { .. })
        ));
        let newer = descriptor("env-1", ENVIRONMENT_PROTOCOL_VERSION + 1, None);
        assert!(matches!(
            check_descriptor(&newer, None),
            Err(DescriptorProblem::UpdateApp { .. })
        ));
        let older = descriptor("env-1", ENVIRONMENT_PROTOCOL_VERSION - 1, None);
        let problem = check_descriptor(&older, None).unwrap_err();
        assert!(problem.to_string().contains("Update IronClaw on Studio"));
    }

    #[test]
    fn the_registry_round_trips_the_contract_shape_without_secrets() {
        let json = serde_json::json!({
            "run_local_backend": false,
            "active": "env-1",
            "environments": [{
                "environment_id": "env-1",
                "label": "Studio",
                "routes": [
                    {"kind": "tailnet", "url": "https://studio.tail9915dd.ts.net"},
                    {"kind": "direct", "url": "https://studio.example"}
                ]
            }]
        });
        let registry = Registry::from_json(json.to_string().as_bytes()).unwrap();
        assert!(!registry.run_local_backend);
        assert_eq!(registry.active, Active::Environment("env-1".into()));
        assert_eq!(registry.environments[0].routes[0].kind, RouteKind::Tailnet);
        assert!(!registry.runs_local_backend());
        let back: serde_json::Value = serde_json::from_slice(&registry.to_json().unwrap()).unwrap();
        assert_eq!(back, json);

        let empty = Registry::from_json(b"{}").unwrap();
        assert_eq!(empty, Registry::default());
        assert!(empty.runs_local_backend());
        assert_eq!(
            Registry::from_json(br#"{"active":"this-mac"}"#)
                .unwrap()
                .active,
            Active::ThisMac
        );
    }

    #[test]
    fn the_registry_file_distinguishes_missing_from_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry_file(dir.path());
        assert_eq!(load_registry(&path), Ok(None));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load_registry(&path).is_err());
        std::fs::write(&path, Registry::default().to_json().unwrap()).unwrap();
        assert_eq!(load_registry(&path), Ok(Some(Registry::default())));
    }

    #[test]
    fn removing_the_active_computer_hands_over_to_this_mac() {
        let mut r = Registry::default();
        r.upsert(SavedEnvironment {
            environment_id: "a".into(),
            label: "A".into(),
            routes: vec![],
            legacy_token: false,
        });
        r.upsert(SavedEnvironment {
            environment_id: "a".into(),
            label: "A2".into(),
            routes: vec![],
            legacy_token: true,
        });
        assert_eq!(r.environments.len(), 1);
        assert_eq!(r.environments[0].label, "A2");
        r.active = Active::Environment("a".into());
        r.remove("a");
        assert_eq!(r.active, Active::ThisMac);
        assert!(r.environments.is_empty());
    }

    #[test]
    fn migrated_routes_prefer_the_tailnet_url_and_drop_plain_http() {
        let d = descriptor("e", 1, Some("https://studio.tail9915dd.ts.net"));
        assert_eq!(
            routes_for("http://100.101.2.3:3000", &d),
            vec![Route {
                kind: RouteKind::Tailnet,
                url: "https://studio.tail9915dd.ts.net".into(),
                remote_port: None,
            }]
        );
        let none = descriptor("e", 1, None);
        assert!(routes_for("http://100.101.2.3:3000", &none).is_empty());
        assert_eq!(
            routes_for("https://box.example", &none),
            vec![Route {
                kind: RouteKind::Direct,
                url: "https://box.example".into(),
                remote_port: None,
            }]
        );
    }

    #[test]
    fn the_legacy_file_is_read_with_its_token_redacted_in_debug() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_file(dir.path());
        assert!(load_legacy(&path).unwrap().is_none());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            br#"{"url":"http://100.1.2.3:3000","token":"SECRET"}"#,
        )
        .unwrap();
        let legacy = load_legacy(&path).unwrap().unwrap();
        assert_eq!(legacy.url, "http://100.1.2.3:3000");
        assert!(!format!("{legacy:?}").contains("SECRET"));
    }

    #[test]
    fn an_ssh_route_round_trips_and_old_files_still_read() {
        // A file written before SSH routes: no `remote_port` anywhere.
        let old = br#"{"run_local_backend":true,"active":"this-mac","environments":[{"environment_id":"e","label":"Studio","routes":[{"kind":"tailnet","url":"https://studio.tail9915dd.ts.net"}]}]}"#;
        let registry = Registry::from_json(old).unwrap();
        assert_eq!(registry.environments[0].routes[0].remote_port, None);
        let back: serde_json::Value = serde_json::from_slice(&registry.to_json().unwrap()).unwrap();
        assert!(
            back["environments"][0]["routes"][0]
                .get("remote_port")
                .is_none()
        );

        let json = serde_json::json!({
            "run_local_backend": true,
            "active": "e",
            "environments": [{
                "environment_id": "e",
                "label": "Studio",
                "routes": [
                    {"kind": "ssh", "url": "ssh://me@studio.local:22"},
                    {"kind": "ssh", "url": "ssh://me@box:2222", "remote_port": 3005}
                ]
            }]
        });
        let registry = Registry::from_json(json.to_string().as_bytes()).unwrap();
        let routes = &registry.environments[0].routes;
        assert_eq!(routes[0].kind, RouteKind::Ssh);
        assert_eq!(routes[1].remote_port, Some(3005));
        assert_eq!(routes[0].caption(), "me@studio.local · SSH");
        let back: serde_json::Value = serde_json::from_slice(&registry.to_json().unwrap()).unwrap();
        assert_eq!(back, json);

        let target = crate::ssh::parse_target("me@box:2222").unwrap();
        assert_eq!(Route::ssh(&target, 3000).remote_port, None);
        assert_eq!(Route::ssh(&target, 3005).remote_port, Some(3005));
        assert_eq!(Route::ssh(&target, 3000).url, "ssh://me@box:2222");
    }

    #[test]
    fn ssh_routes_are_tried_last() {
        let ssh = Route {
            kind: RouteKind::Ssh,
            url: "ssh://me@studio:22".into(),
            remote_port: None,
        };
        let tailnet = Route::for_origin("https://studio.tail9915dd.ts.net");
        let direct = Route::for_origin("https://studio.example");
        let saved = vec![ssh.clone(), tailnet.clone(), direct.clone()];
        let order: Vec<RouteKind> = preferred_routes(&saved).iter().map(|r| r.kind).collect();
        assert_eq!(
            order,
            vec![RouteKind::Tailnet, RouteKind::Direct, RouteKind::Ssh]
        );
        assert_eq!(preferred_routes(std::slice::from_ref(&ssh))[0], &ssh);
        assert!(preferred_routes(&[]).is_empty());
    }
}
