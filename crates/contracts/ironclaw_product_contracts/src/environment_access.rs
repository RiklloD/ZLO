//! Remote access to one IronClaw installation (an "environment"): the public
//! descriptor a client reads before pairing, one-time pairing codes, and the
//! per-device tokens they turn into.
//!
//! Two consumers share these shapes: the WebUI transport that serves them
//! and the ZLO desktop client that calls them. The behavior behind them
//! (code storage, device records, the Tailscale identity listener) lives in
//! the host; see `docs/internal/plans/2026-11-08-zlo-remote-environments.md`
//! ("Wire contract v1").

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Path of the public descriptor route.
pub const ENVIRONMENT_DESCRIPTOR_PATH: &str = "/.well-known/ironclaw/environment";
/// Operator route that issues a one-time pairing code.
pub const PAIRING_CODES_PATH: &str = "/api/auth/pairing-codes";
/// Public route that redeems a pairing code for a device token.
pub const PAIR_PATH: &str = "/api/auth/pair";
/// Public route, identity listener only: pair by tailnet identity.
pub const PAIR_TAILNET_PATH: &str = "/api/auth/pair/tailnet";
/// Operator route listing paired devices; `/{device_id}` revokes one.
pub const DEVICES_PATH: &str = "/api/auth/devices";

/// The remote-access protocol this build speaks. A client and a backend
/// connect only when both report the same value.
pub const ENVIRONMENT_PROTOCOL_VERSION: u32 = 1;

/// Prefix of every device token (`icd_<device_id>_<secret>`).
pub const DEVICE_TOKEN_PREFIX: &str = "icd_";

/// `GET /.well-known/ironclaw/environment`. Public, so it carries only what
/// a client needs to recognize the machine and decide how to pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentDescriptor {
    /// Permanent id of this installation (a UUID), independent of address.
    pub environment_id: String,
    /// Display name, the host name unless the owner renamed it.
    pub label: String,
    pub server_version: String,
    pub protocol_version: u32,
    /// How a new device may pair right now.
    pub pairing: Vec<PairingMethod>,
    /// The `tailscale serve` HTTPS URL while remote access is enabled.
    pub tailnet_url: Option<String>,
    /// Full commit hash the backend was built from, when the build knew it.
    /// Absent from backends older than self-update.
    #[serde(default)]
    pub build_commit: Option<String>,
    /// `[update] method` is set up, so `POST /api/operator/update` can run
    /// (`crate::self_update`). `false` from older backends.
    #[serde(default)]
    pub self_update: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingMethod {
    /// A one-time code issued by the owner.
    OneTimeCode,
    /// One click for the owner's own tailnet login.
    TailnetIdentity,
}

/// `POST /api/auth/pairing-codes` response.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCodeIssued {
    /// `XXXX-XXXX-XXXX`, Crockford base32, single use.
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

impl fmt::Debug for PairingCodeIssued {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingCodeIssued")
            .field("code", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// What a pairing device says about itself; shown in the device list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairClient {
    pub label: String,
    pub os: String,
    pub app_version: String,
}

/// `POST /api/auth/pair` body.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub code: String,
    pub client: PairClient,
}

impl fmt::Debug for PairRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairRequest")
            .field("code", &"<redacted>")
            .field("client", &self.client)
            .finish()
    }
}

/// `POST /api/auth/pair/tailnet` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailnetPairRequest {
    pub client: PairClient,
}

/// Response of both pair routes: the device token, shown exactly once.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub environment_id: String,
    pub device_id: String,
    /// `icd_<device_id>_<secret>`; the backend keeps only its SHA-256.
    pub token: String,
}

impl fmt::Debug for PairedDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairedDevice")
            .field("environment_id", &self.environment_id)
            .field("device_id", &self.device_id)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePairedBy {
    OneTimeCode,
    TailnetIdentity,
}

/// One row of `GET /api/auth/devices`. Revoked devices stay listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub device_id: String,
    pub label: String,
    pub os: String,
    pub app_version: String,
    pub paired_by: DevicePairedBy,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// The device whose token made this request.
    pub current: bool,
}

/// `GET /api/auth/devices` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceList {
    pub devices: Vec<DeviceSummary>,
}

/// The `error` code of a failed pairing call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingErrorCode {
    /// Unknown, expired, already used, or malformed code (one answer for all).
    InvalidCode,
    /// Tailnet login is not one of the owner's.
    NotOwner,
    /// One-click tailnet pairing is turned off on this machine.
    TailnetPairingDisabled,
}

/// Crockford base32 alphabet used by pairing codes (no I, L, O, U).
pub const PAIRING_CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters in a pairing code, without the display dashes.
pub const PAIRING_CODE_LEN: usize = 12;

/// Normalize a typed pairing code: drop dashes and spaces, uppercase, and
/// require exactly [`PAIRING_CODE_LEN`] Crockford characters. Returns the
/// 12-character canonical form, or `None` for anything malformed. The
/// redeem route answers `invalid_code` for `None` exactly as for an unknown
/// code, so this is not an oracle.
pub fn normalize_pairing_code(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(PAIRING_CODE_LEN);
    for ch in raw.chars() {
        if ch == '-' || ch == ' ' {
            continue;
        }
        let upper = ch.to_ascii_uppercase();
        if !upper.is_ascii() || !PAIRING_CODE_ALPHABET.contains(&(upper as u8)) {
            return None;
        }
        if out.len() == PAIRING_CODE_LEN {
            return None;
        }
        out.push(upper);
    }
    (out.len() == PAIRING_CODE_LEN).then_some(out)
}

/// Render a canonical 12-character code as `XXXX-XXXX-XXXX`.
pub fn display_pairing_code(canonical: &str) -> String {
    let chars: Vec<char> = canonical.chars().collect();
    chars
        .chunks(4)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// Why a [`DeviceId`] or [`EnvironmentId`] failed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvironmentAccessIdError {
    #[error("device id must be 32 lowercase hex characters")]
    InvalidDeviceId,
    #[error("environment id must be a lowercase hyphenated UUID")]
    InvalidEnvironmentId,
}

/// Id of one paired device: 32 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct DeviceId(String);

impl DeviceId {
    fn validate(s: &str) -> Result<(), EnvironmentAccessIdError> {
        if s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(())
        } else {
            Err(EnvironmentAccessIdError::InvalidDeviceId)
        }
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, EnvironmentAccessIdError> {
        let s = raw.into();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    /// The device id a well-formed `icd_<device_id>_<secret>` token names.
    /// Says nothing about whether the token is valid.
    pub fn from_device_token(token: &str) -> Option<Self> {
        let rest = token.strip_prefix(DEVICE_TOKEN_PREFIX)?;
        let (id, secret) = rest.split_once('_')?;
        if secret.is_empty() {
            return None;
        }
        Self::new(id).ok()
    }
}

impl TryFrom<String> for DeviceId {
    type Error = EnvironmentAccessIdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl AsRef<str> for DeviceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<DeviceId> for String {
    fn from(id: DeviceId) -> Self {
        id.0
    }
}

/// Permanent id of one installation: a lowercase hyphenated UUID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct EnvironmentId(String);

impl EnvironmentId {
    fn validate(s: &str) -> Result<(), EnvironmentAccessIdError> {
        let bytes = s.as_bytes();
        let ok = bytes.len() == 36
            && bytes.iter().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => *b == b'-',
                _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
            });
        if ok {
            Ok(())
        } else {
            Err(EnvironmentAccessIdError::InvalidEnvironmentId)
        }
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, EnvironmentAccessIdError> {
        let s = raw.into();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<String> for EnvironmentId {
    type Error = EnvironmentAccessIdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl AsRef<str> for EnvironmentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvironmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<EnvironmentId> for String {
    fn from(id: EnvironmentId) -> Self {
        id.0
    }
}

/// The installation's permanent identity: id plus display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentIdentity {
    pub environment_id: EnvironmentId,
    pub label: String,
}

/// Failure of an [`EnvironmentAccessService`] call. Coarse on purpose: the
/// transport maps each variant to a fixed status and code, and backend
/// detail stays in server-side logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvironmentAccessError {
    /// Unknown, expired, used or malformed pairing code (one answer for all).
    #[error("invalid pairing code")]
    InvalidCode,
    /// No device record with that id.
    #[error("device not found")]
    DeviceNotFound,
    /// The store failed; the cause was logged by the implementor.
    #[error("environment access store unavailable")]
    Unavailable,
}

/// Remote-access records of one installation: its permanent identity,
/// one-time pairing codes, and the device grants they turn into.
///
/// Declared here because its caller (the WebUI transport) and its
/// implementor (`ironclaw_identity`, below product) are different crates.
/// The implementor owns hashing, expiry and the single-use guarantee; the
/// caller owns HTTP, authority checks and the tailnet owner decision.
#[async_trait::async_trait]
pub trait EnvironmentAccessService: Send + Sync {
    /// The installation's permanent id and label.
    async fn environment(&self) -> Result<EnvironmentIdentity, EnvironmentAccessError>;

    /// Issue a new single-use code, valid five minutes.
    async fn issue_pairing_code(&self) -> Result<PairingCodeIssued, EnvironmentAccessError>;

    /// Consume `code` (any case, dashes optional) and create a device grant.
    /// Concurrent redemptions of one code: exactly one succeeds.
    async fn redeem_pairing_code(
        &self,
        code: &str,
        client: PairClient,
    ) -> Result<PairedDevice, EnvironmentAccessError>;

    /// Create a device grant for a caller whose tailnet login the transport
    /// already matched against the owner list.
    async fn pair_tailnet_owner(
        &self,
        tailnet_login: &str,
        client: PairClient,
    ) -> Result<PairedDevice, EnvironmentAccessError>;

    /// The device a presented `icd_` token belongs to, when the token is
    /// live (known, matching hash, not revoked). Records `last_seen_at`.
    async fn authenticate_device(
        &self,
        token: &str,
    ) -> Result<Option<DeviceId>, EnvironmentAccessError>;

    /// Every device, revoked ones included. `current` marks `current_device`.
    async fn list_devices(
        &self,
        current_device: Option<&DeviceId>,
    ) -> Result<DeviceList, EnvironmentAccessError>;

    /// Revoke a device. Revoking a revoked device succeeds and keeps the
    /// first `revoked_at`.
    async fn revoke_device(&self, device_id: &DeviceId) -> Result<(), EnvironmentAccessError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_matches_the_wire_contract() {
        let json = serde_json::json!({
            "environment_id": "0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10",
            "label": "Studio",
            "server_version": "0.41.0",
            "protocol_version": 1,
            "pairing": ["one_time_code", "tailnet_identity"],
            "tailnet_url": "https://studio.tail9915dd.ts.net",
            "build_commit": "94172fa26",
            "self_update": true
        });
        let parsed: EnvironmentDescriptor = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            parsed.pairing,
            vec![PairingMethod::OneTimeCode, PairingMethod::TailnetIdentity]
        );
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }

    #[test]
    fn descriptors_from_backends_before_self_update_still_parse() {
        let parsed: EnvironmentDescriptor = serde_json::from_value(serde_json::json!({
            "environment_id": "0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10",
            "label": "Studio",
            "server_version": "0.41.0",
            "protocol_version": 1,
            "pairing": ["one_time_code"],
            "tailnet_url": null
        }))
        .unwrap();
        assert_eq!(parsed.build_commit, None);
        assert!(!parsed.self_update);
    }

    #[test]
    fn secrets_never_reach_debug_output() {
        let paired = PairedDevice {
            environment_id: "env".into(),
            device_id: "dev".into(),
            token: "icd_dev_SUPERSECRET".into(),
        };
        let code = PairingCodeIssued {
            code: "7KQ2-M9XD-4TRA".into(),
            expires_at: Utc::now(),
        };
        let request = PairRequest {
            code: "7KQ2-M9XD-4TRA".into(),
            client: PairClient {
                label: "Air".into(),
                os: "macOS".into(),
                app_version: "0.1.0".into(),
            },
        };
        for rendered in [
            format!("{paired:?}"),
            format!("{code:?}"),
            format!("{request:?}"),
        ] {
            assert!(
                !rendered.contains("SUPERSECRET") && !rendered.contains("7KQ2"),
                "{rendered}"
            );
        }
    }

    #[test]
    fn error_codes_are_snake_case() {
        assert_eq!(
            serde_json::to_value(PairingErrorCode::TailnetPairingDisabled).unwrap(),
            "tailnet_pairing_disabled"
        );
    }

    #[test]
    fn pairing_codes_normalize_case_and_dashes() {
        assert_eq!(
            normalize_pairing_code("7kq2-m9xd-4tra").as_deref(),
            Some("7KQ2M9XD4TRA")
        );
        assert_eq!(
            normalize_pairing_code("7KQ2M9XD4TRA").as_deref(),
            Some("7KQ2M9XD4TRA")
        );
        assert_eq!(display_pairing_code("7KQ2M9XD4TRA"), "7KQ2-M9XD-4TRA");
        for bad in [
            "",
            "7KQ2-M9XD-4TR",
            "7KQ2-M9XD-4TRAX",
            "7KQ2-M9XD-4TRI",
            "ÄKQ2M9XD4TRA",
        ] {
            assert_eq!(normalize_pairing_code(bad), None, "{bad}");
        }
    }

    #[test]
    fn device_ids_come_from_well_formed_tokens_only() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            DeviceId::from_device_token(&format!("icd_{id}_c2VjcmV0")).map(DeviceId::into_inner),
            Some(id.to_string())
        );
        assert!(DeviceId::from_device_token(&format!("icd_{id}_")).is_none());
        assert!(DeviceId::from_device_token("icd_ABC_secret").is_none());
        assert!(DeviceId::from_device_token("master-token").is_none());
        assert!(serde_json::from_value::<DeviceId>(serde_json::json!("XYZ")).is_err());
    }

    #[test]
    fn environment_ids_are_lowercase_uuids() {
        assert!(EnvironmentId::new("0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10").is_ok());
        assert!(EnvironmentId::new("0F6C2D0E-3B0A-4A51-9B7E-6B1C8F1F2A10").is_err());
        assert!(EnvironmentId::new("studio").is_err());
    }
}
