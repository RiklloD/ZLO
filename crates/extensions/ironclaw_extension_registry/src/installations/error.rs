use super::*;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExtensionInstallationError {
    #[error(transparent)]
    Manifest(#[from] ManifestV2Error),
    #[error("invalid extension manifest: {reason}")]
    InvalidManifest { reason: String },
    #[error("invalid {field}: {reason}")]
    InvalidValue { field: &'static str, reason: String },
    #[error("installation owner member set must not be empty")]
    EmptyOwnerMembers,
    #[error("legacy tenant installation owner must be canonicalized before member removal")]
    LegacyTenantOwnerNotCanonicalized,
    #[error("installation references unknown extension manifest {extension_id}")]
    UnknownManifest { extension_id: ExtensionId },
    #[error(
        "installation extension {extension_id} does not match manifest extension {manifest_extension_id}"
    )]
    ManifestExtensionMismatch {
        extension_id: ExtensionId,
        manifest_extension_id: ExtensionId,
    },
    #[error(
        "installation manifest hash does not match registered manifest hash for {extension_id}"
    )]
    ManifestHashMismatch { extension_id: ExtensionId },
    #[error("installation {installation_id} was not found")]
    InstallationNotFound {
        installation_id: ExtensionInstallationId,
    },
    #[error("installation {installation_id} has another membership mutation in progress")]
    MembershipMutationInProgress {
        installation_id: ExtensionInstallationId,
    },
    #[error(
        "installation {installation_id} preparation finalization no longer matches its pending aggregate"
    )]
    PreparationFinalizationRejected {
        installation_id: ExtensionInstallationId,
    },
    #[error("extension manifest {extension_id} was not found")]
    ManifestNotFound { extension_id: ExtensionId },
    #[error("invalid installation: {reason}")]
    InvalidInstallation { reason: String },
    /// The backing installation store could not serve the operation
    /// (IO/backend failure). Retryable, unlike the malformed-request
    /// variants: callers map this to their transient error class (#4091).
    #[error("extension installation store unavailable: {reason}")]
    StoreUnavailable { reason: String },
    #[error("duplicate credential binding {handle}")]
    DuplicateCredentialBinding { handle: ExtensionCredentialHandle },
    #[error("conflicting manifest references for extension {extension_id}")]
    ConflictingManifestReference { extension_id: ExtensionId },
    #[error("conflicting preparation states for extension {extension_id}")]
    ConflictingPreparationState { extension_id: ExtensionId },
    #[error("conflicting installation incarnations for extension {extension_id}")]
    ConflictingInstallationIncarnation { extension_id: ExtensionId },
    #[error("conflicting admission identities for extension {extension_id}")]
    ConflictingAdmissionIdentity { extension_id: ExtensionId },
    #[error(
        "package definition for extension {extension_id} conflicts with the admitted definition"
    )]
    PackageDefinitionConflict { extension_id: ExtensionId },
    #[error("conflicting credential bindings for extension {extension_id} and handle {handle}")]
    ConflictingCredentialBinding {
        extension_id: ExtensionId,
        handle: ExtensionCredentialHandle,
    },
    /// An operation that held a preparation lease was rejected, and releasing
    /// that lease also failed. Displays as the original rejection (the error
    /// the caller must act on; classify it via [`Self::rejection`]); the
    /// release failure is the [`std::error::Error::source`]. The store has
    /// already scheduled a bounded, identity-checked in-process repair of the
    /// lease, so the installation becomes visible again without a restart.
    #[error("{rejection}")]
    RejectedWithLeaseCleanupPending {
        rejection: Box<ExtensionInstallationError>,
        #[source]
        cleanup: Box<ExtensionInstallationError>,
    },
}

impl ExtensionInstallationError {
    /// The error that decided the operation's outcome: the original rejection
    /// when a lease release failure is attached as context, otherwise `self`.
    /// Error classifiers (transient vs. invalid request) must match on this.
    pub fn rejection(&self) -> &Self {
        match self {
            Self::RejectedWithLeaseCleanupPending { rejection, .. } => rejection.rejection(),
            other => other,
        }
    }

    /// The failed lease release attached to a rejection, if any (also exposed
    /// as [`std::error::Error::source`]).
    pub fn lease_cleanup_error(&self) -> Option<&Self> {
        match self {
            Self::RejectedWithLeaseCleanupPending { cleanup, .. } => Some(cleanup),
            _ => None,
        }
    }
}
