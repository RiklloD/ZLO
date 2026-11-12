//! Shared harness for the `first_party_builtin_tools_*` test targets: the
//! common import prelude, runtime builders, recording doubles, and fixtures.
//! Each target compiles this module and uses only part of it.
#![allow(dead_code)]

use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    net::IpAddr,
    path::Path,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
#[cfg(feature = "test-support")]
use chrono::{DateTime, Utc};
use ironclaw_authorization::GrantAuthorizer;
use ironclaw_event_log::InMemoryAuditSink;
use ironclaw_extension_registry::ExtensionRegistry;
use ironclaw_filesystem::{DiskFilesystem, InMemoryBackend, RootFilesystem};
use ironclaw_host_api::process::{
    CommandExecutionOutput, CommandExecutionRequest, RuntimeProcessError, SandboxCommandTransport,
};
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_api::runtime_policy::{
    ApprovalPolicy, AuditMode, DeploymentMode, EffectiveRuntimePolicy, FilesystemBackendKind,
    NetworkMode, ProcessBackendKind, RuntimeProfile, SecretMode,
};
use ironclaw_host_api::{
    Timestamp,
    action::{NetworkPolicy, NetworkScheme, NetworkTargetPattern},
    capability::{CapabilityGrant, CapabilitySet, EffectKind, GrantConstraints},
    dispatch::{DispatchFailureDetail, DispatchInputIssueCode},
    execution_policy::TurnExecutionPolicy,
    http::{
        RuntimeHttpEgress, RuntimeHttpEgressError, RuntimeHttpEgressRequest,
        RuntimeHttpEgressResponse, RuntimeHttpSavedBody,
    },
    ids::{
        AgentId, CapabilityGrantId, CapabilityId, ExtensionId, PackageId, ProjectId, RunId,
        TenantId, ThreadId, UserId,
    },
    mount::{MountGrant, MountPermissions, MountView},
    path::{HostPath, MountAlias, ScopedPath, VirtualPath},
    resource::{ResourceEstimate, ResourceScope},
    runtime::{RuntimeKind, TrustClass},
    scope::{ExecutionContext, Principal},
};
use ironclaw_host_runtime::{
    APPLY_PATCH_CAPABILITY_ID, ATTACH_WORKSPACE_FILE_TO_REPLY_CAPABILITY_ID,
    CapabilitySurfaceVersion, DOCUMENT_EDIT_CAPABILITY_ID, ECHO_CAPABILITY_ID, GLOB_CAPABILITY_ID,
    GREP_CAPABILITY_ID, HTML_TO_PDF_CAPABILITY_ID, HTTP_CAPABILITY_ID, HTTP_SAVE_CAPABILITY_ID,
    HostRuntime, HostRuntimeServices, JSON_CAPABILITY_ID, LIST_DIR_CAPABILITY_ID,
    MEMORY_READ_CAPABILITY_ID, MEMORY_SEARCH_CAPABILITY_ID, MEMORY_TREE_CAPABILITY_ID,
    MEMORY_WRITE_CAPABILITY_ID, NATIVE_MEMORY_FIRST_PARTY_PROVIDER, OUTBOUND_DELIVER_CAPABILITY_ID,
    PLAN_UPDATE_CAPABILITY_ID, READ_FILE_CAPABILITY_ID, RuntimeCapabilityFailure,
    RuntimeCapabilityOutcome, RuntimeProcessPort, SHELL_CAPABILITY_ID,
    SKILL_AUTO_ACTIVATE_SET_CAPABILITY_ID, SKILL_INSTALL_CAPABILITY_ID, SKILL_LIST_CAPABILITY_ID,
    SKILL_REMOVE_CAPABILITY_ID, SKILL_UPDATE_CAPABILITY_ID, SPAWN_SUBAGENT_CAPABILITY_ID,
    TIME_CAPABILITY_ID, TRACE_COMMONS_ACCOUNT_LOGIN_LINK_CAPABILITY_ID,
    TRACE_COMMONS_CREDITS_CAPABILITY_ID, TRACE_COMMONS_ONBOARD_CAPABILITY_ID,
    TRACE_COMMONS_PROFILE_SET_CAPABILITY_ID, TRACE_COMMONS_PROFILE_TOKEN_CAPABILITY_ID,
    TRACE_COMMONS_STATUS_CAPABILITY_ID, TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID,
    TRIGGER_PAUSE_CAPABILITY_ID, TRIGGER_REMOVE_CAPABILITY_ID, TRIGGER_RESUME_CAPABILITY_ID,
    TRIGGER_RUN_CAPABILITY_ID, TRIGGER_STATUS_CAPABILITY_ID, ToolCallHttpEgress, TriggerCreateHook,
    UserSandboxProcessPort, WRITE_FILE_CAPABILITY_ID, builtin_first_party_handlers,
    builtin_first_party_handlers_with_trigger_create_hook,
    builtin_first_party_handlers_with_trigger_services, builtin_first_party_package,
    native_memory_first_party_package, register_native_memory_tools,
};
#[cfg(feature = "test-support")]
use ironclaw_host_runtime::{
    TriggerManagementClock, builtin_first_party_handlers_with_trigger_clock,
};
use ironclaw_network::{
    NetworkHttpEgress, NetworkHttpError, NetworkHttpResponse, NetworkHttpTransport,
    NetworkResolver, NetworkTransportRequest,
};
use ironclaw_resources::InMemoryResourceGovernor;
use ironclaw_secrets::SecretStore;
use ironclaw_triggers::{
    InMemoryTriggerRepository, MissingTriggerActiveRunLookup, TriggerError,
    TriggerManualFireRunner, TriggerRecord, TriggerRepository, TriggerRunRecord,
};
use ironclaw_trust::{
    AdminConfig, AdminEntry, AuthorityCeiling, EffectiveTrustClass, HostTrustAssignment,
    HostTrustPolicy, TrustDecision, TrustProvenance,
};
use serde_json::{Value, json};

pub(crate) mod context;
pub(crate) mod doubles;
pub(crate) mod fixtures;
pub(crate) mod invoke;
pub(crate) mod runtimes;
pub(crate) mod triggers;

use self::context::*;
use self::doubles::*;
use self::fixtures::*;
use self::runtimes::*;
use self::triggers::*;
