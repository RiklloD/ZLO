//! Reborn agent-loop driver registry.
//!
//! `ironclaw_turns` owns the neutral driver/profile contracts. This module is
//! the Reborn composition layer that stores concrete driver instances and
//! freezes descriptor metadata at startup.

use std::{collections::HashMap, error::Error, fmt, sync::Arc};

use ironclaw_loop_contracts::{
    AgentLoopDriver, AgentLoopDriverDescriptor, CheckpointSchemaId, LoopDriverId,
};
use ironclaw_turns::RunProfileVersion;

/// Exact persisted identity for a registered loop driver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoopDriverRegistryKey {
    pub id: LoopDriverId,
    pub version: RunProfileVersion,
    pub checkpoint_schema_id: Option<CheckpointSchemaId>,
    pub checkpoint_schema_version: Option<RunProfileVersion>,
}

impl LoopDriverRegistryKey {
    pub fn from_descriptor(descriptor: &AgentLoopDriverDescriptor) -> Result<Self, String> {
        validate_descriptor(descriptor)?;
        Ok(Self {
            id: descriptor.id.clone(),
            version: descriptor.version,
            checkpoint_schema_id: descriptor.checkpoint_schema_id.clone(),
            checkpoint_schema_version: descriptor.checkpoint_schema_version,
        })
    }
}

impl fmt::Display for LoopDriverRegistryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.id.as_str(), self.version.as_u64())?;
        if let (Some(schema_id), Some(schema_version)) = (
            self.checkpoint_schema_id.as_ref(),
            self.checkpoint_schema_version,
        ) {
            write!(
                f,
                " checkpoint {}@{}",
                schema_id.as_str(),
                schema_version.as_u64()
            )?;
        }
        Ok(())
    }
}

/// Host-service dependency level for a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequirementLevel {
    Required,
    Optional,
    Unsupported,
}

/// Static requirements Reborn checks before selecting a driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverRequirements {
    pub model: RequirementLevel,
    pub prompt: RequirementLevel,
    pub transcript: RequirementLevel,
    pub checkpoint: RequirementLevel,
    pub input_polling: RequirementLevel,
    pub capabilities: RequirementLevel,
    pub progress_events: RequirementLevel,
}

impl DriverRequirements {
    pub fn all_optional() -> Self {
        Self {
            model: RequirementLevel::Optional,
            prompt: RequirementLevel::Optional,
            transcript: RequirementLevel::Optional,
            checkpoint: RequirementLevel::Optional,
            input_polling: RequirementLevel::Optional,
            capabilities: RequirementLevel::Optional,
            progress_events: RequirementLevel::Optional,
        }
    }
}

impl Default for DriverRequirements {
    fn default() -> Self {
        Self::all_optional()
    }
}

/// Whether a driver can satisfy production readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    Production,
    /// Fake/reference drivers are allowed only in explicit standalone/test readiness.
    Reference,
}

/// Concrete driver plus registration-time metadata snapshot.
pub struct RegisteredDriver {
    key: LoopDriverRegistryKey,
    driver: Arc<dyn AgentLoopDriver>,
    descriptor: AgentLoopDriverDescriptor,
    requirements: DriverRequirements,
    kind: DriverKind,
}

impl RegisteredDriver {
    pub fn key(&self) -> &LoopDriverRegistryKey {
        &self.key
    }

    pub fn driver(&self) -> Arc<dyn AgentLoopDriver> {
        Arc::clone(&self.driver)
    }

    pub fn descriptor(&self) -> &AgentLoopDriverDescriptor {
        &self.descriptor
    }

    pub fn requirements(&self) -> &DriverRequirements {
        &self.requirements
    }

    pub fn kind(&self) -> DriverKind {
        self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverRegistryError {
    InvalidDescriptor { reason: String },
    DuplicateRegistration { key: LoopDriverRegistryKey },
}

impl fmt::Display for DriverRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDescriptor { reason } => write!(f, "invalid driver descriptor: {reason}"),
            Self::DuplicateRegistration { key } => {
                write!(f, "duplicate driver registration for {key}")
            }
        }
    }
}

impl Error for DriverRegistryError {}

#[derive(Default)]
pub struct DriverRegistry {
    entries: HashMap<LoopDriverRegistryKey, RegisteredDriver>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a concrete driver and cache its validated descriptor snapshot.
    pub fn register_driver(
        &mut self,
        driver: Arc<dyn AgentLoopDriver>,
        requirements: DriverRequirements,
        kind: DriverKind,
    ) -> Result<LoopDriverRegistryKey, DriverRegistryError> {
        let descriptor = driver.descriptor();
        let key = LoopDriverRegistryKey::from_descriptor(&descriptor)
            .map_err(|reason| DriverRegistryError::InvalidDescriptor { reason })?;
        if self.entries.contains_key(&key) {
            return Err(DriverRegistryError::DuplicateRegistration { key });
        }

        self.entries.insert(
            key.clone(),
            RegisteredDriver {
                key: key.clone(),
                driver,
                descriptor,
                requirements,
                kind,
            },
        );
        Ok(key)
    }

    pub fn get(&self, key: &LoopDriverRegistryKey) -> Option<&RegisteredDriver> {
        self.entries.get(key)
    }

    pub fn requirements_snapshot(&self) -> HashMap<LoopDriverRegistryKey, DriverRequirements> {
        self.entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.requirements().clone()))
            .collect()
    }
}

fn validate_descriptor(descriptor: &AgentLoopDriverDescriptor) -> Result<(), String> {
    if descriptor.version.as_u64() == 0 {
        return Err("driver version must be greater than zero".to_string());
    }
    match (
        descriptor.checkpoint_schema_id.as_ref(),
        descriptor.checkpoint_schema_version,
    ) {
        (Some(_), Some(version)) if version.as_u64() == 0 => {
            Err("checkpoint schema version must be greater than zero".to_string())
        }
        (Some(_), Some(_)) | (None, None) => Ok(()),
        (Some(_), None) | (None, Some(_)) => Err(
            "checkpoint schema id and checkpoint schema version must both be present or both absent"
                .to_string(),
        ),
    }
}
