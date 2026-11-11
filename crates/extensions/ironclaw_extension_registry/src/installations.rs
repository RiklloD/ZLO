//! Durable extension records: package definitions, installations,
//! memberships, and credential bindings, persisted as CAS-mutated typed rows.
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ironclaw_filesystem::{
    CasApply, CasExpectation, CasUpdateError, Entry, FilesystemError, Filter, IndexKey, IndexKind,
    IndexName, IndexSpec, IndexValue, Page, RecordKind, RecordVersion, RootFilesystem,
    ScopedFilesystem, VersionedEntry, cas_update,
};
use ironclaw_host_api::{
    approval::sha256_digest_token,
    host_port::HostPortCatalog,
    ids::{ExtensionId, SecretHandle, UserId},
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, ScopedPath, VirtualPath},
    resource::ResourceScope,
};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::resolved::{PackageRootBinding, ResolvedExtensionManifest};
use crate::{ExtensionManifestV2, HostApiContractRegistry, ManifestSource, ManifestV2Error};
use crate::{PackageDefinitionAdmissionOutcome, PackageDefinitionRetention};

mod error;
mod ids;
mod installation;
mod manifest_record;
mod port;
mod records;
mod rows;
mod store;
mod validation;
mod wire;

#[cfg(test)]
mod test_support;

pub use self::error::ExtensionInstallationError;
pub use self::ids::{
    ExtensionCredentialBinding, ExtensionCredentialHandle, ExtensionInstallationId,
    ExtensionManifestRef, ExtensionRemovalChannelId, ExtensionRemovalCleanupAdapterId,
    ExtensionRemovalCleanupBinding, ExtensionRemovalCleanupRequirement, InstallationIncarnationId,
    ManifestHash,
};
pub use self::installation::{
    ExtensionInstallation, ExtensionInstallationPersistedParts, InstallationOwner,
};
pub use self::manifest_record::ExtensionManifestRecord;
pub use self::port::{ExtensionInstallationStorePort, MembershipDeactivation};
pub use self::store::ExtensionInstallationStore;

use self::records::*;
use self::rows::*;
use self::validation::*;
use self::wire::*;
