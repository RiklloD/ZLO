//! Several environments working together: peer links (one environment's
//! agents acting on another), the requests an agent raises for the person
//! (a secret, a machine to link), and the resource reading load balancing
//! picks a machine by.
//!
//! A peer link is a device grant on the *other* environment
//! ([`crate::environment_access::PairingCodeRequest::peer_link`]): the
//! linking side redeems a pairing code from there and keeps the token in its
//! own secret store. Calls go to that environment's [`PEER_TOOL_PATH`] with
//! the token; the receiving side runs its own orchestrator tool there,
//! fenced by the link's access and the calling agent's mode.
//!
//! Declared here because the callers (the WebUI transport, the orchestrator
//! tools, the desktop client) and the implementors (the orchestrator engine,
//! the WebUI peer client, the identity records) are different crates.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::environment_access::{DeviceId, EnvironmentId, PeerLinkAccess};

/// Receiving side: one orchestrator tool call from a linked environment.
/// `{tool}` is the tool name (`t3_delegate`, `t3_task`, ...).
pub const PEER_TOOL_PATH: &str = "/api/peer/v1/tools/{tool}";
/// Receiving side: CPU and memory headroom, for load balancing.
pub const PEER_RESOURCES_PATH: &str = "/api/peer/v1/resources";
/// Linking side: the environments this one links to.
pub const PEER_LINKS_PATH: &str = "/api/peer-links";
/// Linking side: `/{environment_id}` forgets one.
pub const PEER_LINK_PATH: &str = "/api/peer-links/{environment_id}";
/// Linking side: `/{environment_id}/preference` sets its balance preference.
pub const PEER_LINK_PREFERENCE_PATH: &str = "/api/peer-links/{environment_id}/preference";
/// Requests agents raised for the person (secrets, machines to link).
pub const AGENT_REQUESTS_PATH: &str = "/api/agent-requests";
/// `/{request_id}` answers one.
pub const AGENT_REQUEST_PATH: &str = "/api/agent-requests/{request_id}";

/// The concrete path of one peer tool call.
pub fn peer_tool_path(tool: &str) -> String {
    PEER_TOOL_PATH.replace("{tool}", tool)
}

/// Header a peer call carries the calling agent's own access in; the
/// receiving side only ever narrows the link's access with it.
pub const PEER_MODE_LIMIT_HEADER: &str = "x-zlo-peer-mode-limit";

/// Body of a [`PEER_TOOL_PATH`] call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerToolRequest {
    /// The tool's arguments, as the model wrote them (minus `environment`).
    #[serde(default)]
    pub input: Value,
    /// The calling agent's own access on the linking side.
    pub mode_limit: PeerLinkAccess,
    /// Namespace of the calling thread on the linking side. Every caller on
    /// one side shares the link's single grant, so retry keys are hashed
    /// with this before they leave.
    pub caller_key: String,
    /// The linking environment's display name, shown as "From <label>".
    pub origin_label: String,
}

/// Who is calling a peer tool, as the receiving transport established it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCaller {
    /// The link's device grant here.
    pub device_id: DeviceId,
    /// The linking environment's name (the grant's label).
    pub label: String,
    /// `min(link access, the agent's mode limit)`.
    pub access: PeerLinkAccess,
    pub caller_key: String,
}

/// Runs a linked environment's tool calls here. Implemented by the
/// orchestrator engine; the WebUI transport authenticates the link and
/// narrows its access before calling.
#[async_trait]
pub trait PeerToolHandler: Send + Sync {
    /// Run `tool` for `caller`. Never fails: correctable problems are the
    /// tool's `{ok: false, error, message}` document.
    async fn call_peer_tool(&self, caller: PeerCaller, tool: &str, input: Value) -> Value;

    /// This machine's current headroom.
    async fn resources(&self) -> EnvironmentResources;

    /// The tools a linked environment or outside agent may call here.
    fn tool_catalog(&self) -> Vec<PeerToolSpec>;
}

/// One callable tool, as MCP's `tools/list` describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerToolSpec {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Outside agents (Claude Code in a terminal, other MCP clients) reach this
/// environment's orchestrator tools here, with a paired device token whose
/// scopes set what they may do.
pub const OUTSIDE_MCP_PATH: &str = "/api/mcp";

/// CPU and memory headroom of one machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentResources {
    pub cpu_count: u32,
    /// 1-minute load average (0 where the platform reports none).
    pub load_1m: f64,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    /// Turns running on this machine right now.
    pub active_turns: u32,
}

impl EnvironmentResources {
    /// 0.0 (idle) .. 1.0+ (saturated): the busier of CPU and memory.
    pub fn pressure(&self) -> f64 {
        let cpu = if self.cpu_count == 0 {
            0.0
        } else {
            self.load_1m / f64::from(self.cpu_count)
        };
        let memory = if self.memory_total_bytes == 0 {
            0.0
        } else {
            1.0 - self.memory_available_bytes as f64 / self.memory_total_bytes as f64
        };
        cpu.max(memory)
    }

    /// Too busy to take new work automatically.
    pub fn is_full(&self) -> bool {
        self.pressure() >= 0.95
    }
}

/// How much automatic selection should favor a machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalancePreference {
    /// Favor it while it has headroom.
    Prefer,
    #[default]
    Normal,
    /// Reduce its share.
    LessOften,
    /// Never pick it automatically.
    ManualOnly,
}

impl BalancePreference {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "prefer" => Some(Self::Prefer),
            "normal" => Some(Self::Normal),
            "less_often" | "less-often" => Some(Self::LessOften),
            "manual_only" | "manual-only" | "manual" => Some(Self::ManualOnly),
            _ => None,
        }
    }

    /// Multiplier on pressure when ranking machines (lower wins).
    pub fn weight(self) -> Option<f64> {
        match self {
            Self::Prefer => Some(0.6),
            Self::Normal => Some(1.0),
            Self::LessOften => Some(1.6),
            Self::ManualOnly => None,
        }
    }
}

/// One environment this one links to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerLinkSummary {
    pub environment_id: String,
    pub label: String,
    /// Addresses to reach it, tried in order.
    pub urls: Vec<String>,
    pub access: PeerLinkAccess,
    pub linked_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub preference: BalancePreference,
    /// The last call failed with a revoked or expired link.
    #[serde(default)]
    pub revoked: bool,
}

/// `GET /api/peer-links` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerLinkList {
    pub links: Vec<PeerLinkSummary>,
}

/// `POST /api/peer-links` body: redeem a pairing code from the other
/// environment as a peer link.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkEnvironmentRequest {
    /// https, Tailscale, or loopback address of the other environment.
    pub url: String,
    pub pairing_code: String,
    /// Must match what the code was issued for.
    pub access: PeerLinkAccess,
}

impl fmt::Debug for LinkEnvironmentRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkEnvironmentRequest")
            .field("url", &self.url)
            .field("pairing_code", &"<redacted>")
            .field("access", &self.access)
            .finish()
    }
}

/// `POST /api/peer-links/{environment_id}/preference` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerLinkPreferenceRequest {
    pub preference: BalancePreference,
}

/// Why a peer-link call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PeerLinkError {
    /// No link to that environment (or it was forgotten here).
    #[error("no link to environment {0}")]
    NotLinked(String),
    /// The other environment revoked the link, or it expired.
    #[error("the link to {0} was revoked or has expired")]
    Revoked(String),
    /// The other side refused this call (fence, access, or beta off there).
    #[error("{0}")]
    Refused(String),
    /// None of its addresses answered; retry later.
    #[error("environment {0} is unreachable")]
    Unreachable(String),
    /// Bad input (address, code) from the caller.
    #[error("{0}")]
    Invalid(String),
    /// A local store failed; the cause was logged.
    #[error("peer link store unavailable")]
    Unavailable,
}

impl PeerLinkError {
    /// Worth retrying later without the person doing anything.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unreachable(_) | Self::Unavailable)
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::NotLinked(_) => "not_linked",
            Self::Revoked(_) => "link_revoked",
            Self::Refused(_) => "peer_refused",
            Self::Unreachable(_) => "peer_unreachable",
            Self::Invalid(_) => "invalid_request",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The linking side of peer links: stored links, and calls through them.
/// Implemented by the WebUI peer client over the identity records and the
/// operator secret store (the token never leaves it).
#[async_trait]
pub trait PeerLinkService: Send + Sync {
    /// This environment's own id and label (for `t3_capabilities`).
    async fn local_environment(&self) -> Result<(EnvironmentId, String), PeerLinkError>;

    async fn links(&self) -> Result<Vec<PeerLinkSummary>, PeerLinkError>;

    /// Redeem `request.pairing_code` at `request.url` and store the link.
    /// Linking an environment again replaces (renews) its link.
    async fn link(&self, request: LinkEnvironmentRequest)
    -> Result<PeerLinkSummary, PeerLinkError>;

    /// Forget a link here. The other side keeps its grant until it is
    /// revoked there.
    async fn forget(&self, environment_id: &str) -> Result<bool, PeerLinkError>;

    async fn set_preference(
        &self,
        environment_id: &str,
        preference: BalancePreference,
    ) -> Result<PeerLinkSummary, PeerLinkError>;

    /// Run `tool` on the linked environment.
    async fn call_tool(
        &self,
        environment_id: &str,
        tool: &str,
        request: PeerToolRequest,
    ) -> Result<Value, PeerLinkError>;

    /// The linked environment's headroom.
    async fn resources(&self, environment_id: &str) -> Result<EnvironmentResources, PeerLinkError>;
}

/// Records of the links this environment holds (no tokens: those live in
/// the operator secret store under [`peer_link_token_handle`]).
#[async_trait]
pub trait PeerLinkRecords: Send + Sync {
    async fn list(&self) -> Result<Vec<PeerLinkSummary>, PeerLinkError>;
    async fn get(&self, environment_id: &str) -> Result<Option<PeerLinkSummary>, PeerLinkError>;
    async fn put(&self, link: &PeerLinkSummary) -> Result<(), PeerLinkError>;
    async fn remove(&self, environment_id: &str) -> Result<bool, PeerLinkError>;
}

/// Name of the operator secret holding one link's device token.
pub fn peer_link_token_handle(environment_id: &str) -> String {
    format!("peer_link_token_{}", environment_id.replace('-', "_"))
}

// ── agent requests ──────────────────────────────────────────────────────

/// How an answered secret reaches the agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretDelivery {
    /// A one-use `secretRef` the agent passes to a tool that accepts one;
    /// the agent never sees the value.
    #[default]
    SecretRef,
    /// An owner-only file the agent's commands read; deleted after 24 hours.
    File,
}

impl SecretDelivery {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "secretRef" | "secret_ref" | "ref" => Some(Self::SecretRef),
            "file" => Some(Self::File),
            _ => None,
        }
    }
}

/// What an agent is waiting for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentRequestKind {
    /// A secret, typed into a private card instead of the chat.
    Secret {
        label: String,
        reason: String,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
        delivery: SecretDelivery,
    },
    /// Link another machine ("delegate this to my vps").
    EnvironmentLink {
        /// The person's words for the machine.
        #[serde(default)]
        hint: Option<String>,
        /// A known machine's id, when the agent named one.
        #[serde(default)]
        environment_id: Option<String>,
        /// An address the person gave the agent. Needs a pasted code.
        #[serde(default)]
        url: Option<String>,
        /// The access the agent asked for; the person may lower it.
        #[serde(default)]
        access: Option<PeerLinkAccess>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRequestStatus {
    Pending,
    Answered,
    Declined,
    Cancelled,
    TimedOut,
}

impl AgentRequestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Declined => "declined",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "answered" => Some(Self::Answered),
            "declined" => Some(Self::Declined),
            "cancelled" => Some(Self::Cancelled),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }
}

/// One request, as clients list it. Never carries a secret value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRequestView {
    pub request_id: String,
    pub thread_id: String,
    #[serde(default)]
    pub thread_title: Option<String>,
    pub status: AgentRequestStatus,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(flatten)]
    pub kind: AgentRequestKind,
}

/// `GET /api/agent-requests` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentRequestList {
    pub requests: Vec<AgentRequestView>,
}

/// `POST /api/agent-requests/{request_id}` body.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AgentRequestAnswer {
    /// Answer a secret request with the value itself, or with a secret
    /// reference (`op://vault/item/field`) read once now.
    Secret {
        value: String,
    },
    /// Answer a link request: an already-linked machine by id, or a new
    /// link from a pairing code at `url`.
    Link {
        #[serde(default)]
        environment_id: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        pairing_code: Option<String>,
        access: PeerLinkAccess,
    },
    Decline,
}

impl fmt::Debug for AgentRequestAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Secret { .. } => f.write_str("Secret { value: <redacted> }"),
            Self::Link {
                environment_id,
                url,
                access,
                ..
            } => f
                .debug_struct("Link")
                .field("environment_id", environment_id)
                .field("url", url)
                .field("pairing_code", &"<redacted>")
                .field("access", access)
                .finish(),
            Self::Decline => f.write_str("Decline"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentRequestError {
    #[error("no such request")]
    NotFound,
    #[error("the request is no longer open")]
    Closed,
    /// The answer cannot be used (wrong kind, unreadable reference, link
    /// failed). The request stays open.
    #[error("{0}")]
    Invalid(String),
    #[error("agent requests unavailable")]
    Unavailable,
}

/// Requests agents raise for the person. Implemented by the orchestrator
/// engine; the WebUI transport serves them to every client of this
/// environment, so any device can answer.
#[async_trait]
pub trait AgentRequestService: Send + Sync {
    async fn pending_requests(&self) -> Result<AgentRequestList, AgentRequestError>;

    async fn answer_request(
        &self,
        request_id: &str,
        answer: AgentRequestAnswer,
    ) -> Result<AgentRequestView, AgentRequestError>;
}

/// Reads a secret reference (`op://vault/item/field`) on this machine.
#[async_trait]
pub trait SecretReferenceReader: Send + Sync {
    /// Whether `raw` is a reference this reader understands.
    fn is_reference(&self, raw: &str) -> bool;

    /// The value it names, or why it could not be read (1Password locked,
    /// `op` missing). The message never contains the value.
    async fn read(&self, reference: &str) -> Result<secrecy::SecretString, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_takes_the_busier_resource() {
        let r = EnvironmentResources {
            cpu_count: 8,
            load_1m: 2.0,
            memory_total_bytes: 100,
            memory_available_bytes: 20,
            active_turns: 1,
        };
        assert!((r.pressure() - 0.8).abs() < 1e-9);
        assert!(!r.is_full());
    }

    #[test]
    fn answers_never_print_their_secrets() {
        let secret = AgentRequestAnswer::Secret {
            value: "hunter2".into(),
        };
        let link = AgentRequestAnswer::Link {
            environment_id: None,
            url: Some("https://vps.tail.ts.net".into()),
            pairing_code: Some("7KQ2-M9XD-4TRA".into()),
            access: PeerLinkAccess::Operate,
        };
        for rendered in [format!("{secret:?}"), format!("{link:?}")] {
            assert!(!rendered.contains("hunter2") && !rendered.contains("7KQ2"));
        }
    }

    #[test]
    fn request_views_flatten_their_kind() {
        let view = AgentRequestView {
            request_id: "r1".into(),
            thread_id: "t1".into(),
            thread_title: None,
            status: AgentRequestStatus::Pending,
            created_at: 1,
            expires_at: 2,
            kind: AgentRequestKind::Secret {
                label: "GitHub token".into(),
                reason: "push".into(),
                url: None,
                placeholder: None,
                delivery: SecretDelivery::File,
            },
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["kind"], "secret");
        assert_eq!(json["delivery"], "file");
        let back: AgentRequestView = serde_json::from_value(json).unwrap();
        assert_eq!(back, view);
    }
}
