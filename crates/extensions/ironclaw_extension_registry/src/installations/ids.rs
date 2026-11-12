use super::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ManifestHash(String);

impl ManifestHash {
    pub fn new(value: impl Into<String>) -> Result<Self, ExtensionInstallationError> {
        let value = value.into();
        validate_nonempty_noncontrol("manifest_hash", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ManifestHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExtensionRemovalCleanupAdapterId(String);

impl ExtensionRemovalCleanupAdapterId {
    pub fn new(value: impl Into<String>) -> Result<Self, ExtensionInstallationError> {
        validate_cleanup_id(value.into(), "cleanup adapter").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for ExtensionRemovalCleanupAdapterId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ExtensionRemovalCleanupAdapterId {
    type Error = ExtensionInstallationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ExtensionRemovalCleanupAdapterId> for String {
    fn from(value: ExtensionRemovalCleanupAdapterId) -> Self {
        value.into_inner()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExtensionRemovalChannelId(String);

impl ExtensionRemovalChannelId {
    pub fn new(value: impl Into<String>) -> Result<Self, ExtensionInstallationError> {
        validate_cleanup_id(value.into(), "cleanup channel").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for ExtensionRemovalChannelId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ExtensionRemovalChannelId {
    type Error = ExtensionInstallationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ExtensionRemovalChannelId> for String {
    fn from(value: ExtensionRemovalChannelId) -> Self {
        value.into_inner()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum ExtensionRemovalCleanupBinding {
    ChannelConnection { channel: ExtensionRemovalChannelId },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionRemovalCleanupRequirement {
    pub adapter_id: ExtensionRemovalCleanupAdapterId,
    pub binding: ExtensionRemovalCleanupBinding,
}

impl ExtensionRemovalCleanupRequirement {
    pub fn channel_connection(
        adapter_id: ExtensionRemovalCleanupAdapterId,
        channel: ExtensionRemovalChannelId,
    ) -> Self {
        Self {
            adapter_id,
            binding: ExtensionRemovalCleanupBinding::ChannelConnection { channel },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ExtensionInstallationId(String);

impl ExtensionInstallationId {
    pub fn new(value: impl Into<String>) -> Result<Self, ExtensionInstallationError> {
        let value = value.into();
        validate_nonempty_noncontrol("installation_id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Opaque identity for one lifetime of an installation aggregate.
///
/// Reinstalling the same extension gets a different incarnation, preventing
/// a delayed preparation finalizer from committing into the replacement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct InstallationIncarnationId(String);

impl InstallationIncarnationId {
    pub fn new(value: impl Into<String>) -> Result<Self, ExtensionInstallationError> {
        let value = value.into();
        validate_nonempty_noncontrol("installation_incarnation_id", &value)?;
        Ok(Self(value))
    }

    pub fn fresh() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for InstallationIncarnationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ExtensionInstallationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExtensionInstallationId {
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
pub struct ExtensionCredentialHandle(String);

impl ExtensionCredentialHandle {
    pub fn new(value: impl Into<String>) -> Result<Self, ExtensionInstallationError> {
        let value = value.into();
        validate_nonempty_noncontrol("credential_handle", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExtensionCredentialHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExtensionCredentialHandle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionCredentialBinding {
    pub(super) credential_handle: ExtensionCredentialHandle,
    pub(super) secret_handle: SecretHandle,
}

impl ExtensionCredentialBinding {
    pub fn new(credential_handle: ExtensionCredentialHandle, secret_handle: SecretHandle) -> Self {
        Self {
            credential_handle,
            secret_handle,
        }
    }

    pub fn credential_handle(&self) -> &ExtensionCredentialHandle {
        &self.credential_handle
    }

    pub fn secret_handle(&self) -> &SecretHandle {
        &self.secret_handle
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionManifestRef {
    extension_id: ExtensionId,
    manifest_hash: Option<ManifestHash>,
}

impl ExtensionManifestRef {
    pub fn new(extension_id: ExtensionId, manifest_hash: Option<ManifestHash>) -> Self {
        Self {
            extension_id,
            manifest_hash,
        }
    }

    pub fn extension_id(&self) -> &ExtensionId {
        &self.extension_id
    }

    pub fn manifest_hash(&self) -> Option<&ManifestHash> {
        self.manifest_hash.as_ref()
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn removal_cleanup_ids_validate_and_round_trip_their_canonical_wire_values() {
        let adapter = ExtensionRemovalCleanupAdapterId::new("slack.personal")
            .expect("canonical cleanup adapter");
        assert_eq!(adapter.as_str(), "slack.personal");
        assert_eq!(adapter.as_ref(), "slack.personal");
        assert_eq!(adapter.clone().into_inner(), "slack.personal");
        assert_eq!(String::from(adapter.clone()), "slack.personal");
        assert_eq!(
            serde_json::from_str::<ExtensionRemovalCleanupAdapterId>(
                &serde_json::to_string(&adapter).expect("serialize adapter")
            )
            .expect("deserialize adapter"),
            adapter
        );

        let channel = ExtensionRemovalChannelId::new("slack").expect("canonical cleanup channel");
        assert_eq!(channel.as_str(), "slack");
        assert_eq!(channel.as_ref(), "slack");
        assert_eq!(channel.clone().into_inner(), "slack");
        assert_eq!(String::from(channel.clone()), "slack");
        assert_eq!(
            serde_json::from_str::<ExtensionRemovalChannelId>(
                &serde_json::to_string(&channel).expect("serialize channel")
            )
            .expect("deserialize channel"),
            channel
        );

        for invalid in ["", "Slack", "slack/connection", "-slack", "slack-"] {
            let wire = serde_json::to_string(invalid).expect("serialize invalid cleanup id");
            assert!(
                serde_json::from_str::<ExtensionRemovalCleanupAdapterId>(&wire).is_err(),
                "invalid cleanup adapter must be rejected: {invalid}"
            );
            assert!(
                serde_json::from_str::<ExtensionRemovalChannelId>(&wire).is_err(),
                "invalid cleanup channel must be rejected: {invalid}"
            );
        }
    }
}
