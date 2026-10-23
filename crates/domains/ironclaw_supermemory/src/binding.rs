//! Which Supermemory containers a caller reads and writes.
//!
//! [`SupermemoryBindingSource`] is the seam per-profile settings plug into: it
//! answers "which containers does this thread/caller use". The default
//! [`StaticSupermemoryBindingSource`] returns one global binding from config.

use std::fmt;

use async_trait::async_trait;
use ironclaw_host_api::{
    ids::{AgentId, InvocationId, ProjectId, TenantId, ThreadId, UserId},
    resource::ResourceScope,
    turn::TurnScope,
};
use serde::{Deserialize, Serialize};

use crate::error::SupermemoryError;

/// Default primary container when config names none (the Hermes `ironclaw`
/// profile's container).
pub(crate) const DEFAULT_PRIMARY_CONTAINER: &str = "ironclaw";
const MAX_CONTAINER_TAG_CHARS: usize = 100;
/// Same budget as a profile's free-text settings (4 KiB).
pub(crate) const MAX_INSTRUCTIONS_CHARS: usize = 4096;

/// A Supermemory container tag: 1 to 100 ASCII letters, digits, `-`, `_`, `.`
/// (the API's documented grammar).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ContainerTag(String);

impl ContainerTag {
    fn validate(value: &str) -> Result<(), SupermemoryError> {
        if value.is_empty() || value.chars().count() > MAX_CONTAINER_TAG_CHARS {
            return Err(SupermemoryError::InvalidContainer {
                reason: format!("must be 1 to {MAX_CONTAINER_TAG_CHARS} characters"),
            });
        }
        if !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        {
            return Err(SupermemoryError::InvalidContainer {
                reason: "only ASCII letters, digits, '-', '_' and '.' are allowed".to_string(),
            });
        }
        Ok(())
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, SupermemoryError> {
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

impl TryFrom<String> for ContainerTag {
    type Error = SupermemoryError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl AsRef<str> for ContainerTag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ContainerTag> for String {
    fn from(tag: ContainerTag) -> Self {
        tag.0
    }
}

/// One profile's containers, as in a Hermes `supermemory.json`: the primary
/// container is recalled every turn and receives every write; the read
/// containers are recalled and searchable but never written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupermemoryBinding {
    pub primary_container: String,
    pub read_containers: Vec<String>,
    /// Profile-authored guidance on how to use the containers (Hermes
    /// `custom_container_instructions`), appended to the recall guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl SupermemoryBinding {
    /// Validated primary tag.
    pub fn primary(&self) -> Result<ContainerTag, SupermemoryError> {
        ContainerTag::new(self.primary_container.as_str())
    }

    /// Validated read tags, without the primary and without duplicates.
    pub fn readable(&self) -> Result<Vec<ContainerTag>, SupermemoryError> {
        let mut tags: Vec<ContainerTag> = Vec::new();
        for raw in &self.read_containers {
            let tag = ContainerTag::new(raw.as_str())?;
            if tag.as_str() != self.primary_container && !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        Ok(tags)
    }

    /// Trimmed instructions, at most [`MAX_INSTRUCTIONS_CHARS`]; `None` when blank.
    pub fn instructions(&self) -> Option<String> {
        let text = self.instructions.as_deref()?.trim();
        (!text.is_empty()).then(|| text.chars().take(MAX_INSTRUCTIONS_CHARS).collect())
    }

    /// Primary first, then the read containers.
    pub fn all_containers(&self) -> Result<Vec<ContainerTag>, SupermemoryError> {
        let mut tags = vec![self.primary()?];
        tags.extend(self.readable()?);
        Ok(tags)
    }
}

/// Who is asking: the trusted run scope plus the acting user. Built by the
/// adapters from the run's typed identities, never from display strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupermemoryCaller {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub agent_id: Option<AgentId>,
    pub project_id: Option<ProjectId>,
    pub thread_id: Option<ThreadId>,
}

impl SupermemoryCaller {
    pub fn from_turn_scope(scope: &TurnScope, user_id: UserId) -> Self {
        Self {
            tenant_id: scope.tenant_id.clone(),
            user_id,
            agent_id: scope.agent_id.clone(),
            project_id: scope.project_id.clone(),
            thread_id: Some(scope.thread_id.clone()),
        }
    }

    pub fn from_resource_scope(scope: &ResourceScope) -> Self {
        Self {
            tenant_id: scope.tenant_id.clone(),
            user_id: scope.user_id.clone(),
            agent_id: scope.agent_id.clone(),
            project_id: scope.project_id.clone(),
            thread_id: scope.thread_id.clone(),
        }
    }

    /// Scope stamped on the outbound network request.
    pub(crate) fn network_scope(&self) -> ResourceScope {
        ResourceScope {
            tenant_id: self.tenant_id.clone(),
            user_id: self.user_id.clone(),
            agent_id: self.agent_id.clone(),
            project_id: self.project_id.clone(),
            mission_id: None,
            thread_id: self.thread_id.clone(),
            invocation_id: InvocationId::new(),
        }
    }
}

/// Resolves the container binding for a caller. `Ok(None)` means Supermemory
/// is off for that caller even when a key is set.
///
/// Per-profile settings implement this (profile of the caller's thread →
/// that profile's `supermemory.json` values) and replace the static default at
/// composition.
#[async_trait]
pub trait SupermemoryBindingSource: Send + Sync {
    async fn binding_for(
        &self,
        caller: &SupermemoryCaller,
    ) -> Result<Option<SupermemoryBinding>, SupermemoryError>;
}

/// One binding for every caller, read from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticSupermemoryBindingSource {
    binding: Option<SupermemoryBinding>,
}

impl StaticSupermemoryBindingSource {
    pub fn new(binding: Option<SupermemoryBinding>) -> Self {
        Self { binding }
    }

    /// `SUPERMEMORY_CONTAINER_TAG` (primary, default `ironclaw`) and
    /// `SUPERMEMORY_READ_CONTAINERS` (comma-separated). Invalid tags fail here,
    /// at startup, rather than on the first turn.
    pub fn from_env() -> Result<Self, SupermemoryError> {
        Self::from_values(
            std::env::var("SUPERMEMORY_CONTAINER_TAG").ok().as_deref(),
            std::env::var("SUPERMEMORY_READ_CONTAINERS").ok().as_deref(),
        )
    }

    pub(crate) fn from_values(
        primary: Option<&str>,
        read: Option<&str>,
    ) -> Result<Self, SupermemoryError> {
        let primary = primary
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_PRIMARY_CONTAINER)
            .to_string();
        let read_containers = read
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect();
        let binding = SupermemoryBinding {
            primary_container: primary,
            read_containers,
            instructions: None,
        };
        binding.all_containers()?;
        Ok(Self::new(Some(binding)))
    }
}

#[async_trait]
impl SupermemoryBindingSource for StaticSupermemoryBindingSource {
    async fn binding_for(
        &self,
        _caller: &SupermemoryCaller,
    ) -> Result<Option<SupermemoryBinding>, SupermemoryError> {
        Ok(self.binding.clone())
    }
}
