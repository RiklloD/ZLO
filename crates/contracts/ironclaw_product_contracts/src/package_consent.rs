//! Consent vocabulary for third-party extension packages ("plugins").
//!
//! A package the binary did not compile in may not activate until the user
//! grants consent to what it asks for. The grant is pinned to a digest of the
//! package's files, so a package whose files change after the grant stops
//! activating (`tampered`) until consent is granted again for the new digest.
//!
//! These are wire shapes only. The host derives the summary from the resolved
//! manifest and persists grants; no policy lives here.

use serde::{Deserialize, Serialize};

use ironclaw_host_api::error::HostApiError;

/// Hex SHA-256 over every file of an extension package (64 lowercase hex
/// characters, no algorithm prefix).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct PackageConsentDigest(String);

impl PackageConsentDigest {
    const HEX_LEN: usize = 64;

    fn validate(value: &str) -> Result<(), HostApiError> {
        if value.len() != Self::HEX_LEN
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(HostApiError::InvariantViolation {
                reason: "package consent digest must be 64 lowercase hex characters".to_string(),
            });
        }
        Ok(())
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, HostApiError> {
        let value = raw.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<String> for PackageConsentDigest {
    type Error = HostApiError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl AsRef<str> for PackageConsentDigest {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PackageConsentDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<PackageConsentDigest> for String {
    fn from(digest: PackageConsentDigest) -> Self {
        digest.0
    }
}

/// What one consent line is about. `hook`, `ui_slot`, and `setting` are
/// reserved for manifest sections that declare them; `service` covers
/// long-lived surfaces the host runs on the package's behalf (a channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleConsentRequestKind {
    Tool,
    Network,
    Credential,
    Hook,
    UiSlot,
    Setting,
    Service,
}

impl LifecycleConsentRequestKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Network => "network",
            Self::Credential => "credential",
            Self::Hook => "hook",
            Self::UiSlot => "ui_slot",
            Self::Setting => "setting",
            Self::Service => "service",
        }
    }
}

/// One thing the package asks for, phrased for a person deciding whether to
/// allow it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleConsentRequest {
    pub kind: LifecycleConsentRequestKind,
    /// Short label, e.g. the tool id or the network host.
    pub title: String,
    /// One plain sentence.
    pub detail: String,
}

/// Everything a package asks for, pinned to the digest a grant must name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleConsentSummary {
    pub digest: PackageConsentDigest,
    pub requests: Vec<LifecycleConsentRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_accepts_only_lowercase_sha256_hex() {
        let valid = "a".repeat(64);
        assert!(PackageConsentDigest::new(valid.clone()).is_ok());
        assert!(PackageConsentDigest::new("A".repeat(64)).is_err());
        assert!(PackageConsentDigest::new("a".repeat(63)).is_err());
        assert!(PackageConsentDigest::new(format!("sha256:{}", "a".repeat(57))).is_err());
        let wire: Result<PackageConsentDigest, _> =
            serde_json::from_value(serde_json::Value::String("zz".to_string()));
        assert!(wire.is_err(), "wire decoding revalidates");
        let round: PackageConsentDigest =
            serde_json::from_value(serde_json::Value::String(valid.clone())).expect("decodes");
        assert_eq!(round.as_str(), valid);
    }

    #[test]
    fn consent_summary_wire_shape_is_pinned() {
        let summary = LifecycleConsentSummary {
            digest: PackageConsentDigest::new("0".repeat(64)).expect("digest"),
            requests: vec![LifecycleConsentRequest {
                kind: LifecycleConsentRequestKind::UiSlot,
                title: "panel".to_string(),
                detail: "Shows a panel.".to_string(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&summary).expect("serializes"),
            serde_json::json!({
                "digest": "0".repeat(64),
                "requests": [{"kind": "ui_slot", "title": "panel", "detail": "Shows a panel."}],
            })
        );
        for kind in [
            LifecycleConsentRequestKind::Tool,
            LifecycleConsentRequestKind::Network,
            LifecycleConsentRequestKind::Credential,
            LifecycleConsentRequestKind::Hook,
            LifecycleConsentRequestKind::UiSlot,
            LifecycleConsentRequestKind::Setting,
            LifecycleConsentRequestKind::Service,
        ] {
            assert_eq!(
                serde_json::to_value(kind).expect("serializes"),
                serde_json::Value::String(kind.as_str().to_string())
            );
        }
    }
}
