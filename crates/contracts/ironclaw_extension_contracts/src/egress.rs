//! Declared egress hosts, targets and credential handles.

use serde::{Deserialize, Deserializer, Serialize};

use ironclaw_host_api::product_adapter_error::ProductAdapterError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct DeclaredEgressHost(String);

impl DeclaredEgressHost {
    pub fn new(value: impl Into<String>) -> Result<Self, ProductAdapterError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProductAdapterError::InvalidIdentifier {
                kind: "declared_egress_host",
                reason: "must not be empty".into(),
            });
        }
        if value.len() > 253 {
            return Err(ProductAdapterError::InvalidIdentifier {
                kind: "declared_egress_host",
                reason: "must be at most 253 bytes".into(),
            });
        }
        if value
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'))
        {
            return Err(ProductAdapterError::InvalidIdentifier {
                kind: "declared_egress_host",
                reason: "must contain only ASCII alphanumeric, '.', '-' or '_'".into(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeclaredEgressHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DeclaredEgressHost {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct EgressCredentialHandle(String);

impl EgressCredentialHandle {
    pub fn new(value: impl Into<String>) -> Result<Self, ProductAdapterError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProductAdapterError::InvalidIdentifier {
                kind: "egress_credential_handle",
                reason: "must not be empty".into(),
            });
        }
        if value.len() > 256 {
            return Err(ProductAdapterError::InvalidIdentifier {
                kind: "egress_credential_handle",
                reason: "must be at most 256 bytes".into(),
            });
        }
        if value.chars().any(|c| c == '\0' || c.is_control()) {
            return Err(ProductAdapterError::InvalidIdentifier {
                kind: "egress_credential_handle",
                reason: "must not contain control characters".into(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EgressCredentialHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EgressCredentialHandle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Host-visible egress declaration advertised by an adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredEgressTarget {
    pub host: DeclaredEgressHost,
    pub credential_handle: Option<EgressCredentialHandle>,
}

impl DeclaredEgressTarget {
    pub fn new(
        host: DeclaredEgressHost,
        credential_handle: Option<EgressCredentialHandle>,
    ) -> Self {
        Self {
            host,
            credential_handle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::product_adapter_error::{ProtocolHttpEgressError, RedactedString};

    #[test]
    fn declared_host_rejects_invalid_chars() {
        assert!(DeclaredEgressHost::new("example.com/path").is_err());
        assert!(DeclaredEgressHost::new("ex ample.com").is_err());
        assert!(DeclaredEgressHost::new("ex@ample.com").is_err());
        assert!(DeclaredEgressHost::new("api.telegram.org").is_ok());
    }

    #[test]
    fn credential_handle_round_trips() {
        let h = EgressCredentialHandle::new("telegram_bot_token").expect("valid");
        let json = serde_json::to_string(&h).expect("serialize");
        let parsed: EgressCredentialHandle = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(h, parsed);
    }

    #[test]
    fn network_error_does_not_leak_inner_string() {
        let err = ProtocolHttpEgressError::Network(RedactedString::new(
            "connection refused at 10.0.0.1:443 with token=secret",
        ));
        let rendered = err.to_string();
        assert!(!rendered.contains("10.0.0.1"));
        assert!(!rendered.contains("secret"));
    }
}
