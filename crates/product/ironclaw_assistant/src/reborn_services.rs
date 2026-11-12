//! WebUI-facing Reborn service service.
//!
//! This module is the stable high-level API beta WebUI route handlers use
//! instead of reaching into turn coordination, thread stores, runtime lanes, DB
//! stores, dispatchers, or capability hosts directly.

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::{Arc, Mutex as StdMutex, Weak},
    time::Duration,
};

use ironclaw_product_contracts::admin_users::{
    ADMIN_USER_LIST_DEFAULT_LIMIT, ADMIN_USER_LIST_MAX_LIMIT, AdminUserService, AdminUserStatus,
};
use ironclaw_product_contracts::channel_config::ChannelConfigProductService;
use ironclaw_product_contracts::lifecycle_service::{
    LifecycleProductContext, LifecycleProductService, LifecycleProductSurfaceContext,
};
use ironclaw_product_contracts::operator_llm::{
    ActiveModelReader, CodexLoginStart, LLM_USER_MODEL_POLICY_SET_CAPABILITY_ID,
    LLM_USER_MODEL_PREFERENCE_SET_CAPABILITY_ID, LlmConfigService, LlmConfigServiceError,
    LlmConfigSnapshot, LlmModelsResult, LlmProbeRequest, LlmProbeResult, NearAiLoginRequest,
    NearAiLoginStart, NearAiWalletLoginRequest, NearAiWalletLoginResult, USER_MODEL_CATALOG_VIEW,
    USER_MODEL_PREFERENCE_VIEW, UpsertLlmProviderRequest, UserModelCatalog, UserModelPreference,
};
use ironclaw_product_contracts::operator_service::{
    OperatorLogsService, OperatorServiceLifecycleService, OperatorStatusService,
    normalize_operator_log_context_value,
};
use ironclaw_product_contracts::operator_tools::{
    RebornOperatorToolCatalog, RebornOperatorToolInfo,
};
pub use ironclaw_product_contracts::product_wire::{
    RebornAdminThreadScrapeArtifactRequest, RebornAdminThreadScrapeListRequest,
    RebornAdminThreadScrapeRunArtifactRequest,
};
use ironclaw_product_contracts::projection::ProjectionStream;
use ironclaw_product_contracts::views::{RebornViewPage, RebornViewProvider, RebornViewQuery};

use async_trait::async_trait;
use chrono::Utc;
use futures::future::try_join_all;
use ironclaw_attachments::{InboundAttachmentLander, InboundAttachmentReader};
use ironclaw_auth::{
    AuthProductScope, AuthProviderId, ChannelConnectionService, CredentialAccountId,
    CredentialAccountProjection, CredentialAccountUpdateBinding, ProviderScope,
};
use ironclaw_host_api::product_adapter::{ProductAdapterError, ProductSurfaceRejectionKind};
use ironclaw_host_api::turn::{
    AcceptedMessageRef, IdempotencyKey, SanitizedCancelReason, TurnActor, TurnGateRef, TurnRunId,
    TurnScope, TurnStatus,
};
use ironclaw_host_api::{
    capability::{EffectKind, GrantConstraints, PermissionMode},
    ids::{
        ActivityId, AgentId, CapabilityId, ExtensionId, InvocationId, ProjectId, ResultRef,
        SecretHandle, TenantId, ThreadId, UserId,
    },
    resolution::{Outcome, OutcomeRefs, Resolution, ResultPreviewMeta, ToolVerdict},
    resource::ResourceScope,
    result_meta::{FailureKind, ResultProgress, TerminateHint},
    safe_summary::SafeSummary,
    scope::Principal,
};
use ironclaw_loop_host::{HostInputEnqueuePort, RejectingInputEnqueue};
use ironclaw_product_contracts::outbound::ProjectionCursor;
use ironclaw_product_contracts::projection::ProjectionSubscriptionRequest;
use ironclaw_product_contracts::suggestions::{
    SUGGESTION_DISMISS_COMMAND_ID, SUGGESTION_START_COMMAND_ID, SUGGESTIONS_GENERATE_COMMAND_ID,
    SUGGESTIONS_LIST_VIEW,
};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode, ProductSurfaceErrorKind,
    ProductSurfaceValidationCode,
};
use ironclaw_threads::{
    EnsureThreadRequest, SessionThreadError, SessionThreadRecord, SessionThreadService,
    ThreadHistory, ThreadHistoryRequest, ThreadMessageId, ThreadScope,
};
use ironclaw_triggers::{AutomationName, AutomationNameError};
use ironclaw_turns::{
    GetRunStateRequest, ResumeTurnPrecondition, ResumeTurnRequest, RetryTurnRequest,
    TurnCoordinator, TurnError,
};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, mpsc};
use url::Url;
use uuid::Uuid;

use crate::{
    ApprovalInteractionDecision, ApprovalInteractionService, AuthInteractionDecision,
    AuthInteractionRejectionKind, AuthInteractionService, CommandAudience, CommandResultField,
    CommandResultView, DecodeInboundAttachments, IntoProductInboundCommand,
    ListPendingApprovalsRequest, PRODUCT_LIFECYCLE_COMMAND_OPERATION_ID,
    PRODUCT_MODEL_COMMAND_OPERATION_ID, PRODUCT_NEW_COMMAND_OPERATION_ID,
    PRODUCT_STATUS_COMMAND_OPERATION_ID, PRODUCT_STOP_COMMAND_OPERATION_ID,
    PendingApprovalInteractionView, ProductCommand, ProductCommandDescriptor,
    ProductInboundCommand, ProductLifecycleCommandInput, ProductModelCommand,
    ProductModelCommandInput, ProductNewCommandInput, ProductNewCommandOutput,
    ProductStatusCommandInput, ProductStopCommandInput, ProductStopInvocation,
    ProductSurfaceFailure, ResolveApprovalInteractionRequest, ResolveApprovalInteractionResponse,
    ResolveAuthInteractionRequest, ResolveAuthInteractionResponse,
    UnsupportedLifecycleProductService,
    approval_interaction::RejectingApprovalInteractionService,
    auth_interaction::RejectingAuthInteractionService,
    declared_command_help_text, is_approval_gate_ref, is_auth_gate_ref,
    policy::{BeforeInboundPolicy, BeforeInboundPolicyOutcome, BeforeInboundPolicyRequest},
    product_command_descriptors, required_audience, thread_metadata_is_automation_trigger,
};
use ironclaw_extension_contracts::channel_adapter::ProductTriggerReason;
use ironclaw_product_contracts::inbound::{
    ProductRejection, ProductRejectionKind, parse_product_slash_command,
};
use ironclaw_product_contracts::inbound_requests::{
    ProductCancelRunRequest, ProductCreateThreadRequest, ProductGateResolution,
    ProductListAutomationsRequest, ProductListThreadsRequest, ProductRenameAutomationRequest,
    ProductResolveGateRequest, ProductRetryRunRequest, ProductSubmitTurnRequest,
};
use ironclaw_product_contracts::inspector::{
    INSPECTOR_PROMPT_VIEW, INSPECTOR_SNAPSHOT_VIEW, INSPECTOR_TOOL_VIEW, INSPECTOR_UPDATES_VIEW,
};

mod admin_configuration;
mod admin_users;
mod approval_settings;
mod extension_credentials;
mod extension_onboarding;
mod extension_setup_credentials;
mod extensions;
mod fs_browse;
mod inspector;
mod ironhub_link;
mod lifecycle_setup;
mod llm_config;
mod log_views;
mod notification_setup;
mod operator_command_views;
mod operator_config_views;
mod outbound_delivery_capability_surface;
mod outbound_preferences;
mod outbound_views;
mod product_capability_handlers;
mod product_commands;
mod project_fs;
mod projects;
// pub(crate): lib.rs re-exports `reborn_services::run_artifact::timings`
// directly, which needs this segment of the path visible crate-wide; the
// module's own contents stay unexported except through that re-export.
mod automations;
mod extension_ports;
mod gates;
mod notifications;
mod operator;
mod operator_config;
mod outbound;
mod projections;
pub(crate) mod run_artifact;
mod runs;
mod skills;
mod thread_artifact;
mod threads;
mod timings_source;
mod trace_credits;
mod types;
mod views;
mod wiring;

use admin_users::map_admin_user_error;
pub use admin_users::{
    ADMIN_USER_CREATE_COMMAND, ADMIN_USER_CREATE_COMMAND_ID, ADMIN_USER_DELETE_CAPABILITY,
    ADMIN_USER_DELETE_CAPABILITY_ID, ADMIN_USER_DELETE_SECRET_CAPABILITY,
    ADMIN_USER_DELETE_SECRET_CAPABILITY_ID, ADMIN_USER_DELETE_SECRET_COMMAND,
    ADMIN_USER_DELETE_SECRET_COMMAND_ID, ADMIN_USER_PUT_SECRET_CAPABILITY,
    ADMIN_USER_PUT_SECRET_CAPABILITY_ID, ADMIN_USER_SECRETS_VIEW, ADMIN_USER_SET_ROLE_CAPABILITY,
    ADMIN_USER_SET_ROLE_CAPABILITY_ID, ADMIN_USER_SET_STATUS_CAPABILITY,
    ADMIN_USER_SET_STATUS_CAPABILITY_ID, ADMIN_USER_UPDATE_CAPABILITY,
    ADMIN_USER_UPDATE_CAPABILITY_ID, ADMIN_USER_VIEW, ADMIN_USERS_VIEW,
};
pub use automations::{
    AUTOMATION_DELETE_CAPABILITY, AUTOMATION_DELETE_CAPABILITY_ID, AUTOMATION_DELETE_COMMAND,
    AUTOMATION_DELETE_COMMAND_ID, AUTOMATION_LIST_DEFAULT_PAGE_SIZE, AUTOMATION_LIST_MAX_PAGE_SIZE,
    AUTOMATION_PAUSE_CAPABILITY, AUTOMATION_PAUSE_CAPABILITY_ID, AUTOMATION_PAUSE_COMMAND,
    AUTOMATION_PAUSE_COMMAND_ID, AUTOMATION_RENAME_CAPABILITY, AUTOMATION_RENAME_CAPABILITY_ID,
    AUTOMATION_RENAME_COMMAND, AUTOMATION_RENAME_COMMAND_ID, AUTOMATION_RESUME_CAPABILITY,
    AUTOMATION_RESUME_CAPABILITY_ID, AUTOMATION_RESUME_COMMAND, AUTOMATION_RESUME_COMMAND_ID,
    AUTOMATION_RUN_CAPABILITY, AUTOMATION_RUN_CAPABILITY_ID, AUTOMATION_RUN_COMMAND,
    AUTOMATION_RUN_COMMAND_ID, AUTOMATION_RUN_HISTORY_DEFAULT_PAGE_SIZE,
    AUTOMATION_RUN_HISTORY_MAX_PAGE_SIZE, AUTOMATIONS_VIEW, AutomationListRequest,
    AutomationProductService, TriggerRunThreadScope, UnsupportedAutomationProductService,
};
use automations::{
    LegacyApprovalBackfillKey, NOTIFICATION_APPROVAL_QUERY_TIMEOUT, is_automation_trigger_thread,
    notification_approval_timeout_error,
};
use extension_ports::parse_credential_account_id;
pub use extension_ports::{
    EXTENSION_ACTIVATE_CAPABILITY, EXTENSION_ACTIVATE_CAPABILITY_ID, EXTENSION_IMPORT_CAPABILITY,
    EXTENSION_IMPORT_CAPABILITY_ID, EXTENSION_INSTALL_CAPABILITY, EXTENSION_INSTALL_CAPABILITY_ID,
    EXTENSION_REGISTER_HOSTED_MCP_CAPABILITY, EXTENSION_REGISTER_HOSTED_MCP_CAPABILITY_ID,
    EXTENSION_REMOVE_CAPABILITY, EXTENSION_REMOVE_CAPABILITY_ID, EXTENSION_SETUP_SUBMIT_CAPABILITY,
    EXTENSION_SETUP_SUBMIT_CAPABILITY_ID, ExtensionCredentialSetupService,
    ExtensionCredentialStatusRequest, ExtensionCredentialSubmitRequest,
    StaticChannelConnectionService,
};
pub use fs_browse::{
    ATTACHMENT_READ_COMMAND, ATTACHMENT_READ_COMMAND_ID, FS_LIST_VIEW, FS_MOUNTS_VIEW,
    FS_READ_COMMAND, FS_READ_COMMAND_ID, FS_STAT_VIEW,
};
use gates::{NOTICE_BLOCKED_APPROVAL, NOTICE_BLOCKED_AUTH, participant_denied};
pub use gates::{RESOLVE_GATE_COMMAND, RESOLVE_GATE_COMMAND_ID};
use llm_config::validate_llm_base_url;
pub use llm_config::{
    LLM_ACTIVE_SET_CAPABILITY, LLM_ACTIVE_SET_CAPABILITY_ID, LLM_CODEX_LOGIN_COMMAND,
    LLM_CODEX_LOGIN_COMMAND_ID, LLM_LIST_MODELS_COMMAND, LLM_LIST_MODELS_COMMAND_ID,
    LLM_NEARAI_LOGIN_COMMAND, LLM_NEARAI_LOGIN_COMMAND_ID, LLM_NEARAI_WALLET_LOGIN_COMMAND,
    LLM_NEARAI_WALLET_LOGIN_COMMAND_ID, LLM_PROVIDER_DELETE_CAPABILITY,
    LLM_PROVIDER_DELETE_CAPABILITY_ID, LLM_PROVIDER_UPSERT_CAPABILITY,
    LLM_PROVIDER_UPSERT_CAPABILITY_ID, LLM_TEST_CONNECTION_COMMAND, LLM_TEST_CONNECTION_COMMAND_ID,
};
use notifications::{map_notification_inbox_error, notification_recipient};
use operator::{
    OPERATOR_LOGS_MAX_LIMIT, OperatorSetupHostState, bounded_log_query,
    bounded_operator_logs_query, operator_diagnostics_surface_status,
    operator_doctor_setup_unavailable_diagnostic, operator_doctor_status_diagnostic,
    operator_doctor_status_response, operator_doctor_status_unavailable_diagnostic,
    operator_setup_validation_error, setup_response_from_llm_snapshot, validate_log_query_modes,
};
pub use operator::{
    OPERATOR_SERVICE_LIFECYCLE_COMMAND, OPERATOR_SERVICE_LIFECYCLE_COMMAND_ID,
    OPERATOR_SETUP_RUN_CAPABILITY, OPERATOR_SETUP_RUN_CAPABILITY_ID, StaticOperatorStatusService,
    UnsupportedOperatorLogsService, UnsupportedOperatorServiceLifecycleService,
    UnsupportedOperatorStatusService,
};
use operator_config::{
    AUTO_APPROVE_CONFIG_KEY, RebornOperatorApprovalConfig, TOOL_CONFIG_PREFIX,
    auto_approve_config_entry, find_operator_tool, operator_config_not_wired_response,
    operator_config_surface_not_wired_diagnostic, operator_config_unknown_key_error,
    operator_config_validation_diagnostics, operator_tool_permission_context,
    product_view_requires_operator_config, tool_config_entry, tool_config_entry_with_context,
};
pub use operator_config::{
    GLOBAL_AUTO_APPROVE_VIEW, OPERATOR_CONFIG_SET_AUTO_APPROVE_CAPABILITY,
    OPERATOR_CONFIG_SET_AUTO_APPROVE_CAPABILITY_ID, OPERATOR_CONFIG_SET_KEY_COMMAND,
    OPERATOR_CONFIG_SET_KEY_COMMAND_ID, OPERATOR_CONFIG_SET_TOOL_PERMISSION_CAPABILITY,
    OPERATOR_CONFIG_SET_TOOL_PERMISSION_CAPABILITY_ID,
};
pub use outbound::{
    NOTIFICATION_CHANNELS_SET_COMMAND, NOTIFICATION_CHANNELS_SET_COMMAND_ID,
    OutboundPreferencesProductService, UnsupportedOutboundPreferencesProductService,
};
use product_commands::product_command_input;
pub use product_commands::{
    PRODUCT_COMMAND_EXECUTE_COMMAND, PRODUCT_COMMAND_EXECUTE_COMMAND_ID,
    PRODUCT_COMMAND_LIST_COMMAND, PRODUCT_COMMAND_LIST_COMMAND_ID,
};
use projections::{
    PRODUCT_STREAM_FIRST_EVENT_WAIT, decode_product_surface_stream_request,
    encode_product_surface_stream_response, open_product_surface_event_subscription,
};
use projects::map_project_fs_error;
pub use projects::{
    PROJECT_CREATE_COMMAND, PROJECT_CREATE_COMMAND_ID, PROJECT_DELETE_CAPABILITY,
    PROJECT_DELETE_CAPABILITY_ID, PROJECT_FS_LIST_VIEW, PROJECT_FS_READ_COMMAND,
    PROJECT_FS_READ_COMMAND_ID, PROJECT_FS_STAT_VIEW, PROJECT_MEMBER_ADD_CAPABILITY,
    PROJECT_MEMBER_ADD_CAPABILITY_ID, PROJECT_MEMBER_REMOVE_CAPABILITY,
    PROJECT_MEMBER_REMOVE_CAPABILITY_ID, PROJECT_MEMBER_UPDATE_CAPABILITY,
    PROJECT_MEMBER_UPDATE_CAPABILITY_ID, PROJECT_MEMBERS_VIEW, PROJECT_UPDATE_CAPABILITY,
    PROJECT_UPDATE_CAPABILITY_ID, PROJECT_VIEW, PROJECTS_VIEW,
};
pub use runs::{
    CANCEL_RUN_COMMAND, CANCEL_RUN_COMMAND_ID, RETRY_RUN_COMMAND, RETRY_RUN_COMMAND_ID,
    SUBMIT_TURN_COMMAND, SUBMIT_TURN_COMMAND_ID,
};
use runs::{describe_turn_status, map_turn_error, parse_persisted_turn_run_id, parse_run_id_field};
pub use skills::{
    SKILL_AUTO_ACTIVATE_LEARNED_SET_CAPABILITY, SKILL_AUTO_ACTIVATE_LEARNED_SET_CAPABILITY_ID,
    SKILL_AUTO_ACTIVATE_SET_CAPABILITY, SKILL_AUTO_ACTIVATE_SET_CAPABILITY_ID, SKILL_CONTENT_VIEW,
    SKILL_INSTALL_CAPABILITY, SKILL_INSTALL_CAPABILITY_ID, SKILL_REMOVE_CAPABILITY,
    SKILL_REMOVE_CAPABILITY_ID, SKILL_SEARCH_VIEW, SKILL_UPDATE_CAPABILITY,
    SKILL_UPDATE_CAPABILITY_ID, SKILLS_VIEW, SkillsProductService, UnsupportedSkillsProductService,
};
use skills::{SkillActivationClearer, SkillActivationRecorder};
pub use thread_artifact::{
    ADMIN_THREAD_SCRAPE_ARTIFACT_VIEW, ADMIN_THREAD_SCRAPE_RUN_ARTIFACT_VIEW,
    ADMIN_THREAD_SCRAPE_THREADS_VIEW,
};
pub use threads::{
    CREATE_THREAD_COMMAND, CREATE_THREAD_COMMAND_ID, THREAD_DELETE_CAPABILITY,
    THREAD_DELETE_CAPABILITY_ID, THREADS_VIEW, TIMELINE_VIEW,
};
use threads::{
    ResolvedThreadAccess, ThreadOperationLocks, map_ownership_probe_error, map_thread_error,
    map_timeline_probe_error, parse_thread_id_field, thread_scope_from_turn_scope,
};
pub use trace_credits::{
    TRACE_ACCOUNT_LOGIN_LINK_COMMAND, TRACE_ACCOUNT_LOGIN_LINK_COMMAND_ID,
    TRACE_HOLD_AUTHORIZE_COMMAND, TRACE_HOLD_AUTHORIZE_COMMAND_ID,
};

// Crate-internal seam for the runtime communication context (#7247): the
// model-facing prompt slice reuses the extensions card's per-caller auth
// verdict instead of re-deriving "connected for this caller" a second way.
pub(crate) use extensions::{CallerExtensionAuth, caller_extension_auth};

use crate::conversation_binding::SessionLaneRejectingBindingResolver;
use crate::inbound_turn::{DefaultInboundTurnService, SessionSkillActivationPorts};
use crate::workflow::DefaultProductSurface;
pub use admin_configuration::{
    ADMIN_CONFIGURATION_REPLACE_CAPABILITY, ADMIN_CONFIGURATION_REPLACE_CAPABILITY_ID,
    ADMIN_CONFIGURATION_VIEW, RebornAdminConfigurationField, RebornAdminConfigurationGroup,
    RebornAdminConfigurationListResponse, RebornAdminConfigurationUse,
};
use admin_users::RejectingAdminUserService;
pub use admin_users::{
    RebornAdminCreateUserRequest, RebornAdminDeleteSecretProductRequest,
    RebornAdminPutSecretProductRequest, RebornAdminPutSecretRequest,
    RebornAdminSecretDeletedResponse, RebornAdminSecretResponse, RebornAdminSetRoleProductRequest,
    RebornAdminSetRoleRequest, RebornAdminSetStatusProductRequest, RebornAdminSetStatusRequest,
    RebornAdminUpdateUserProductRequest, RebornAdminUpdateUserRequest,
    RebornAdminUserCreatedResponse, RebornAdminUserDeletedResponse, RebornAdminUserListQuery,
    RebornAdminUserListResponse, RebornAdminUserRequest, RebornAdminUserResponse,
    RebornAdminUserSecretsListResponse,
};
use ironclaw_host_api::product_adapter::identity::{AdapterInstallationId, ProductAdapterId};
use ironclaw_product_contracts::inbound::ProductInboundAck;
use ironclaw_product_contracts::surface::ChannelInboundProductSurface;
pub use ironclaw_product_contracts::surface::{
    ChannelInboundSurfaceAdmission, ChannelInboundSurfaceOutcome,
    ChannelInboundSurfaceRejectedAdmission, ChannelInboundSurfaceRequest,
};
pub use trace_credits::{
    RebornAccountLoginLinkResponse, RebornAccountTrace, RebornAccountTracesResponse,
    RebornTraceCreditsResponse, RebornTraceHoldAuthorizeResponse, TRACE_ACCOUNT_TRACES_VIEW,
    TRACE_CREDITS_VIEW,
};

use approval_settings::{
    AUTO_APPROVE_DEFAULT_ENABLED, AutoApproveSettingKey, AutoApproveSettingStorePort,
    CapabilityPermissionOverrideStorePort, PersistentApprovalAction, PersistentApprovalPolicyError,
    PersistentApprovalPolicyInput, PersistentApprovalPolicyKey, PersistentApprovalPolicyStorePort,
    ToolPermissionOverride, ToolPermissionOverrideInput, ToolPermissionOverrideKey,
    ToolPermissionState, permission_mode_allows_persistent_approval,
};
pub use extensions::{EXTENSION_REGISTRY_VIEW, EXTENSIONS_VIEW};
pub use fs_browse::{
    FilesystemBrowseReader, FsMount, RebornFsListRequest, RebornFsListResponse, RebornFsMountInfo,
    RebornFsMountsRequest, RebornFsMountsResponse, RebornFsReadRequest, RebornFsStatRequest,
    RebornFsStatResponse,
};
use ironclaw_notifications::{
    LifecycleRef, ListNotificationsRequest, MarkAllNotificationsReadRequest,
    NOTIFICATION_PAGE_LIMIT_MAX, NoopNotificationInboxStore, NotificationAction,
    NotificationInboxStorePort, NotificationInitialState, NotificationKind,
    NotificationMutationRequest, NotificationRecipient, NotificationSeverity, NotificationSource,
    PublishNotificationRequest,
};
pub use ironclaw_product_contracts::descriptors::{
    EmptyProductCommandInput, ProductCapabilityDescriptor, ProductSurfaceCommandDescriptor,
    ProductView,
};
use ironclaw_product_contracts::ironhub::{
    IRONHUB_DELIVER_INSTALL_COMMAND_ID, IronhubInstallDeliveryRequest,
    IronhubInstallDeliveryResult, IronhubLinkService,
};
use ironclaw_product_contracts::notification_inbox::{
    NOTIFICATIONS_ARCHIVE_COMMAND_ID, NOTIFICATIONS_MARK_ALL_READ_COMMAND_ID,
    NOTIFICATIONS_MARK_READ_COMMAND_ID, NOTIFICATIONS_VIEW,
};
use ironclaw_product_contracts::notification_inbox::{
    ProductListNotificationsRequest, ProductListNotificationsResponse,
    ProductMarkAllNotificationsReadRequest, ProductNotification, ProductNotificationAction,
    ProductNotificationKind, ProductNotificationMutationRequest,
    ProductNotificationMutationResponse, ProductNotificationSeverity,
};
pub use ironclaw_product_contracts::package_lifecycle::ChannelConnectStrategy as RebornChannelConnectStrategy;
pub use ironclaw_product_contracts::product_wire::{
    RebornAccountBindingSource, RebornAttachmentBytes, RebornAttachmentRequest,
    RebornAutomationActiveHold, RebornAutomationHoldReason, RebornAutomationInfo,
    RebornAutomationMutationResponse, RebornAutomationRecentRunInfo,
    RebornAutomationRecentRunStatus, RebornAutomationRequest, RebornAutomationRunMutationResult,
    RebornAutomationRunMutationStatus, RebornAutomationRunStatus, RebornAutomationSource,
    RebornAutomationState, RebornCancelRunResponse, RebornChannelConnectAction,
    RebornCommandRejection, RebornDeleteThreadRequest, RebornDeleteThreadResponse,
    RebornExecuteProductCommandRequest, RebornExtensionActionResponse,
    RebornExtensionCredentialSetup, RebornExtensionOnboardingPayload,
    RebornExtensionOnboardingState, RebornExtensionRegistryEntry, RebornExtensionRegistryResponse,
    RebornExtensionSetupField, RebornExtensionSetupSecret, RebornExtensionSurface,
    RebornGetRunStateRequest, RebornGlobalAutoApproveRequest, RebornGlobalAutoApproveResponse,
    RebornListAutomationsResponse, RebornLogEntry, RebornLogQueryRequest, RebornLogQueryResponse,
    RebornNotificationChannel, RebornNotificationChannelsResponse,
    RebornNotificationSetupMutationRequest, RebornNotificationSetupRequest,
    RebornNotificationSetupStatusResponse, RebornOperatorArea, RebornOperatorCommandPlaneResponse,
    RebornOperatorConfigDiagnostic, RebornOperatorConfigDiagnosticSeverity,
    RebornOperatorConfigEntry, RebornOperatorConfigGetResponse, RebornOperatorConfigListResponse,
    RebornOperatorConfigSetProductRequest, RebornOperatorConfigSetRequest,
    RebornOperatorConfigValidateRequest, RebornOperatorConfigValidateResponse,
    RebornOperatorLogsQuery, RebornOperatorServiceLifecycleAction,
    RebornOperatorServiceLifecycleRequest, RebornOperatorSetupRequest, RebornOperatorSetupResponse,
    RebornOperatorSetupStatus, RebornOperatorSetupStep, RebornOperatorSetupStepStatus,
    RebornOperatorStatusCheck, RebornOperatorStatusResponse, RebornOperatorStatusSeverity,
    RebornOperatorStatusState, RebornOperatorSurfaceStatus,
    RebornOutboundDeliveryTargetCapabilities, RebornOutboundDeliveryTargetChannel,
    RebornOutboundDeliveryTargetDescription, RebornOutboundDeliveryTargetDisplayName,
    RebornOutboundDeliveryTargetId, RebornOutboundDeliveryTargetListResponse,
    RebornOutboundDeliveryTargetOption, RebornOutboundDeliveryTargetStatus,
    RebornOutboundDeliveryTargetSummary, RebornProductCommandInfo,
    RebornProductCommandListResponse, RebornRenameAutomationProductRequest,
    RebornResolveGateResponse, RebornResumeGateResponse, RebornRetryRunResponse,
    RebornServiceLifecycleAction, RebornServiceLifecycleRequest, RebornServiceLifecycleResponse,
    RebornServiceLifecycleState, RebornSetNotificationChannelsRequest,
    RebornSetupExtensionResponse, RebornSkillActionResponse, RebornSkillContentResponse,
    RebornSkillInfo, RebornSkillListResponse, RebornSkillSearchResponse, RebornSkillSourceKind,
    RebornSkillTrustLevel, RebornStreamEventsRequest, RebornStreamEventsResponse,
    RebornSubmitTurnResponse, RebornTimelineRequest, RebornTraceHoldAuthorizeProductRequest,
    SettingsToolPermissionState,
};
// A product-tier port gets exactly one import path (§11.2.4), so this is a
// private `use` and never a `pub use` — callers name the contracts crate.
use ironclaw_product_contracts::project_service::{ProjectService, ProjectServiceError};
pub use lifecycle_setup::EXTENSION_SETUP_VIEW;
pub use llm_config::LLM_CONFIG_VIEW;
pub use log_views::{LOGS_VIEW, OPERATOR_LOGS_VIEW};
pub use notification_setup::{
    ChannelNotificationSetupService, DeliveryClientBootstrap, DeliveryClientBootstrapError,
    NoDeliveryClientBootstrap, RegistrationChannelNotificationSetupService,
    UnsupportedChannelNotificationSetupService,
};
pub use operator_command_views::{
    OPERATOR_DIAGNOSTICS_VIEW, OPERATOR_SETUP_VIEW, OPERATOR_STATUS_VIEW,
};
pub use operator_config_views::{
    OPERATOR_CONFIG_KEY_VIEW, OPERATOR_CONFIG_LIST_VIEW, OPERATOR_CONFIG_VALIDATE_VIEW,
};
pub use outbound_delivery_capability_surface::{
    NOTIFICATION_CHANNELS_SET_MAX_ITEMS, NotificationChannelsSetInput,
    OUTBOUND_DELIVERY_TARGETS_LIST_CAPABILITY_ID, OUTBOUND_DELIVERY_TARGETS_LIST_DESCRIPTION,
    OUTBOUND_DELIVERY_TARGETS_LIST_PROVIDER_TOOL_NAME,
    OUTBOUND_NOTIFICATION_CHANNELS_SET_CAPABILITY_ID,
    OUTBOUND_NOTIFICATION_CHANNELS_SET_DESCRIPTION,
    OUTBOUND_NOTIFICATION_CHANNELS_SET_PROVIDER_TOOL_NAME, OutboundDeliveryCapabilityInputError,
    OutboundDeliveryTargetsListInput, list_outbound_delivery_targets_for_model,
    notification_channels_set_input_schema, notification_channels_set_operator_tool_info,
    outbound_delivery_synthetic_provider, outbound_delivery_targets_list_input_schema,
    parse_notification_channels_set_input, parse_outbound_delivery_targets_list_input,
    set_notification_channels_for_model,
};
pub use outbound_preferences::RebornOutboundPreferencesService;
pub use outbound_views::{NOTIFICATION_CHANNELS_VIEW, OUTBOUND_DELIVERY_TARGETS_VIEW};
pub use project_fs::{
    ProjectFilesystemReader, ProjectFsEntry, ProjectFsEntryKind, ProjectFsError, ProjectFsFile,
    ProjectFsStat, RebornProjectFsListRequest, RebornProjectFsListResponse,
    RebornProjectFsReadRequest, RebornProjectFsStatRequest, RebornProjectFsStatResponse,
};
pub use projects::{
    ProjectCaller, RebornAddMemberRequest, RebornCreateProjectRequest, RebornDeleteProjectRequest,
    RebornGetProjectRequest, RebornListMembersRequest, RebornListMembersResponse,
    RebornListProjectsRequest, RebornListProjectsResponse, RebornProjectInfo,
    RebornProjectMemberInfo, RebornProjectMemberStatus, RebornProjectResponse, RebornProjectRole,
    RebornProjectState, RebornRemoveMemberRequest, RebornUpdateMemberRoleRequest,
    RebornUpdateProjectRequest,
};
pub use run_artifact::{
    RUN_ARTIFACT_SCHEMA, RUN_ARTIFACT_VIEW, RebornRunArtifact, RebornRunArtifactRequest,
    RunArtifactLogs, RunArtifactMessage, RunArtifactRedaction, RunArtifactToolCall,
};
pub use thread_artifact::{
    RebornThreadArtifact, RebornThreadArtifactRequest, RunArtifactRunTimings,
    THREAD_ARTIFACT_MAX_MESSAGES, THREAD_ARTIFACT_SCHEMA, THREAD_ARTIFACT_VIEW,
};
pub use types::{
    RebornAuthAccount, RebornCreateThreadResponse, RebornExecuteProductCommandResponse,
    RebornExtensionInfo, RebornExtensionListResponse, RebornGetRunStateResponse,
    RebornListThreadsResponse, RebornProductCommandEffect, RebornTimelineResponse,
    RebornVendorAuthAccounts,
};
pub use views::UnavailableRebornViewProvider;
// The notification-setup descriptors live in
// `ironclaw_product_contracts::notification_setup` (transport/product
// boundary: transports consume the boundary crate). One import path, no
// re-export (§11.2.4).
use ironclaw_product_contracts::notification_setup::{
    NOTIFICATION_SETUP_DISABLE_COMMAND_ID, NOTIFICATION_SETUP_ENABLE_COMMAND_ID,
    NOTIFICATION_SETUP_STATUS_VIEW,
};

/// Product caller scope for actions that must run against a concrete agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductAgentBoundCaller {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub agent_id: AgentId,
    pub project_id: Option<ProjectId>,
}

impl ProductAgentBoundCaller {
    pub fn new(
        tenant_id: TenantId,
        user_id: UserId,
        agent_id: AgentId,
        project_id: Option<ProjectId>,
    ) -> Self {
        Self {
            tenant_id,
            user_id,
            agent_id,
            project_id,
        }
    }
}

fn caller_resource_scope(caller: &ProductSurfaceCaller) -> ResourceScope {
    ResourceScope {
        tenant_id: caller.tenant_id.clone(),
        user_id: caller.user_id.clone(),
        agent_id: caller.agent_id.clone(),
        project_id: caller.project_id.clone(),
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    }
}

fn product_view_forbidden() -> ProductSurfaceError {
    ProductSurfaceError::from_status(ProductSurfaceErrorCode::Forbidden, 403, false)
}

fn authorize_product_view(
    caller: &ProductSurfaceCaller,
    view_id: &str,
) -> Result<(), ProductSurfaceError> {
    if product_view_requires_operator_config(view_id) && !caller.operator_config {
        return Err(product_view_forbidden());
    }
    Ok(())
}

/// Product-side command membrane for the generic [`ProductSurface::invoke`]
/// conduit.
///
/// The concrete execution adapter lives in composition: this crate owns the
/// product contract and remains independent of runtime implementation crates.
/// The service is generic over this boundary so the production capability hot
/// path does not add another `Arc<dyn ...>` seam solely for test substitution.
#[async_trait]
pub trait ProductCapabilityInvoker: Send + Sync {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        capability: CapabilityId,
        input: serde_json::Value,
        activity_id: ActivityId,
    ) -> Result<Resolution, ProductSurfaceError>;

    /// Persist structured output produced by product-owned capability
    /// handlers through the same durable result boundary as runtime-owned
    /// capabilities.
    async fn complete_product_result(
        &self,
        _caller: ProductSurfaceCaller,
        _output: serde_json::Value,
        _activity_id: ActivityId,
        _summary: &'static str,
    ) -> Result<Resolution, ProductSurfaceError> {
        Err(ProductSurfaceError::service_unavailable(false))
    }
}

/// Fail-closed default for compositions that have not attached the product
/// capability membrane.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableProductCapabilityInvoker;

#[async_trait]
impl ProductCapabilityInvoker for UnavailableProductCapabilityInvoker {
    async fn invoke(
        &self,
        _caller: ProductSurfaceCaller,
        _capability: CapabilityId,
        _input: serde_json::Value,
        _activity_id: ActivityId,
    ) -> Result<Resolution, ProductSurfaceError> {
        Err(ProductSurfaceError::service_unavailable(false))
    }
}

/// Default service implementation composed at the WebUI boundary.
#[derive(Clone)]
pub struct RebornServices<
    I = UnavailableProductCapabilityInvoker,
    V = UnavailableRebornViewProvider,
> {
    product_capability_invoker: I,
    view_provider: V,
    thread_service: Arc<dyn SessionThreadService>,
    turn_coordinator: Arc<dyn TurnCoordinator>,
    input_enqueue: Arc<dyn HostInputEnqueuePort>,
    inbound_attachments: Option<Arc<dyn InboundAttachmentLander>>,
    project_filesystem: Option<Arc<dyn ProjectFilesystemReader>>,
    filesystem_browser: Option<Arc<dyn FilesystemBrowseReader>>,
    project_service: Option<Arc<dyn ProjectService>>,
    inbound_attachment_reader: Option<Arc<dyn InboundAttachmentReader>>,
    event_stream: Option<Arc<dyn ProjectionStream>>,
    lifecycle_service: Arc<dyn LifecycleProductService>,
    automation_service: Arc<dyn AutomationProductService>,
    skills_service: Arc<dyn SkillsProductService>,
    channel_connection_service: Arc<dyn ChannelConnectionService>,
    channel_config_service: Option<Arc<dyn ChannelConfigProductService>>,
    outbound_preferences_service: Arc<dyn OutboundPreferencesProductService>,
    notification_setup_service: Arc<dyn ChannelNotificationSetupService>,
    notification_inbox: Arc<dyn NotificationInboxStorePort>,
    legacy_approval_backfill_attempted: Arc<StdMutex<HashSet<LegacyApprovalBackfillKey>>>,
    session_inbound_ledger: Arc<dyn crate::ledger::IdempotencyLedger>,
    /// The session lane's product surface, built once: every input is an
    /// immutable builder-wired `Arc`.
    session_inbound_surface: Arc<std::sync::OnceLock<Arc<DefaultProductSurface>>>,
    session_channels:
        Option<Arc<dyn ironclaw_product_contracts::session_ingress::SessionChannelDirectory>>,
    operator_status: Arc<dyn OperatorStatusService>,
    operator_logs: Arc<dyn OperatorLogsService>,
    operator_service_lifecycle: Arc<dyn OperatorServiceLifecycleService>,
    approval_interactions: Arc<dyn ApprovalInteractionService>,
    auth_interactions: Arc<dyn AuthInteractionService>,
    admin_users: Arc<dyn AdminUserService>,
    extension_credentials: Option<Arc<dyn ExtensionCredentialSetupService>>,
    skill_activation_recorder: Option<Arc<SkillActivationRecorder>>,
    skill_activation_clearer: Option<Arc<SkillActivationClearer>>,
    llm_config: Option<Arc<dyn LlmConfigService>>,
    ironhub_link: Option<Arc<dyn IronhubLinkService>>,
    // arch-exempt: optional_arc, genuinely optional — the active-model reader is wired only when the runtime has an LLM reload handle; runtimes built without one, and tests, run without it (mirrors the sibling optional llm_config field), plan #5985
    active_model_reader: Option<Arc<dyn ActiveModelReader>>,
    operator_approval_config: Option<RebornOperatorApprovalConfig>,
    diagnostic_store: Arc<dyn crate::inspector_store::DiagnosticStorePort>,
    pub(crate) suggestions: Option<SuggestionsServices>,
    thread_operation_locks: Arc<ThreadOperationLocks>,
    // arch-exempt: optional_arc, native `ask_user` answers go to the run's external-tool catalog; graphs without a loop runtime (product-only tests) have none and refuse "answered", plan #plan-forms
    external_tool_catalog: Option<Arc<dyn ironclaw_turns::ExternalToolCatalog>>,
}

/// The suggestion surface needs both durable state and the canonical unbound
/// submission path. Keep them as one optional capability so a partially wired
/// surface cannot be represented by the composition root.
#[derive(Clone)]
pub(crate) struct SuggestionsServices {
    pub(crate) store: Arc<dyn crate::suggestions_store::SuggestionsStore>,
    pub(crate) unbound: Arc<crate::unbound_turn::UnboundTurnService>,
}

impl RebornServices<UnavailableProductCapabilityInvoker, UnavailableRebornViewProvider> {
    pub fn new(
        thread_service: Arc<dyn SessionThreadService>,
        turn_coordinator: Arc<dyn TurnCoordinator>,
    ) -> Self {
        Self::new_with_product_ports(
            thread_service,
            turn_coordinator,
            UnavailableProductCapabilityInvoker,
            UnavailableRebornViewProvider,
        )
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub fn new_with_product_ports(
        thread_service: Arc<dyn SessionThreadService>,
        turn_coordinator: Arc<dyn TurnCoordinator>,
        product_capability_invoker: I,
        view_provider: V,
    ) -> Self {
        Self {
            product_capability_invoker,
            view_provider,
            thread_service,
            turn_coordinator,
            input_enqueue: Arc::new(RejectingInputEnqueue),
            inbound_attachments: None,
            project_filesystem: None,
            filesystem_browser: None,
            project_service: None,
            inbound_attachment_reader: None,
            event_stream: None,
            lifecycle_service: Arc::new(UnsupportedLifecycleProductService::new_static(
                "reborn_lifecycle_service_unwired",
            )),
            automation_service: Arc::new(UnsupportedAutomationProductService::new_static()),
            skills_service: Arc::new(UnsupportedSkillsProductService::new_static()),
            channel_connection_service: Arc::new(StaticChannelConnectionService),
            channel_config_service: None,
            outbound_preferences_service: Arc::new(
                UnsupportedOutboundPreferencesProductService::new_static(),
            ),
            notification_setup_service: Arc::new(UnsupportedChannelNotificationSetupService),
            notification_inbox: Arc::new(NoopNotificationInboxStore),
            legacy_approval_backfill_attempted: Arc::new(StdMutex::new(HashSet::new())),
            session_inbound_ledger: Arc::new(
                crate::in_memory_ledger::InMemoryIdempotencyLedger::new(),
            ),
            session_inbound_surface: Arc::new(std::sync::OnceLock::new()),
            session_channels: None,
            operator_status: Arc::new(UnsupportedOperatorStatusService),
            operator_logs: Arc::new(UnsupportedOperatorLogsService),
            operator_service_lifecycle: Arc::new(UnsupportedOperatorServiceLifecycleService),
            approval_interactions: Arc::new(RejectingApprovalInteractionService),
            auth_interactions: Arc::new(RejectingAuthInteractionService),
            admin_users: Arc::new(RejectingAdminUserService),
            extension_credentials: None,
            skill_activation_recorder: None,
            skill_activation_clearer: None,
            llm_config: None,
            ironhub_link: None,
            active_model_reader: None,
            external_tool_catalog: None,
            operator_approval_config: None,
            diagnostic_store: Arc::new(crate::inspector_store::InMemoryDiagnosticStore::default()),
            suggestions: None,
            thread_operation_locks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    async fn invoke_json_capability<T>(
        &self,
        caller: ProductSurfaceCaller,
        capability: ProductCapabilityDescriptor,
        input: T,
        activity_id: ActivityId,
    ) -> Result<Resolution, ProductSurfaceError>
    where
        T: Serialize,
    {
        let input = serde_json::to_value(input).map_err(ProductSurfaceError::internal_from)?;
        self.product_capability_invoker
            .invoke(caller, capability.capability_id()?, input, activity_id)
            .await
    }

    fn api_capability_success(
        &self,
        activity_id: ActivityId,
        summary: &'static str,
    ) -> Result<Resolution, ProductSurfaceError> {
        Ok(Resolution::Done(Outcome {
            refs: OutcomeRefs {
                result: ResultRef::from_uuid(activity_id.as_uuid()),
                byte_len: 0,
                preview: None,
                preview_meta: ResultPreviewMeta::default(),
                origin: None,
                output_digest: None,
            },
            verdict: ToolVerdict::Success,
            summary: SafeSummary::new(summary).map_err(ProductSurfaceError::internal_from)?,
            progress: ResultProgress::MadeProgress,
            terminate_hint: TerminateHint::Continue,
        }))
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        capability: CapabilityId,
        input: serde_json::Value,
        activity_id: ActivityId,
    ) -> Result<Resolution, ProductSurfaceError> {
        if let Some(operation) =
            product_capability_handlers::ProductCapabilityHandler::parse(&capability)
        {
            let completion_caller = caller.clone();
            let run_result = operation.invoke(self, caller, input).await?;
            let summary = match run_result.as_ref().map(|result| result.status) {
                Some(RebornAutomationRunMutationStatus::Replayed) => {
                    "automation run was already submitted"
                }
                Some(RebornAutomationRunMutationStatus::Submitted) | None => {
                    operation.success_summary()
                }
            };
            if let Some(run_result) = run_result {
                let output =
                    serde_json::to_value(run_result).map_err(ProductSurfaceError::internal_from)?;
                return self
                    .product_capability_invoker
                    .complete_product_result(completion_caller, output, activity_id, summary)
                    .await;
            }
            return self.api_capability_success(activity_id, summary);
        }
        self.product_capability_invoker
            .invoke(caller, capability, input, activity_id)
            .await
    }

    pub async fn query(
        &self,
        caller: ProductSurfaceCaller,
        query: RebornViewQuery,
    ) -> Result<RebornViewPage, ProductSurfaceError> {
        authorize_product_view(&caller, &query.view_id)?;
        if self.view_provider.descriptor().id == query.view_id {
            return self
                .view_provider
                .query(caller, query.params, query.cursor)
                .await;
        }
        match query.view_id.as_str() {
            id if id == INSPECTOR_SNAPSHOT_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                inspector::snapshot(self.diagnostic_store.as_ref(), caller, request)
            }
            id if id == INSPECTOR_PROMPT_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                inspector::prompt(self.diagnostic_store.as_ref(), caller, request)
            }
            id if id == INSPECTOR_TOOL_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                inspector::tool(self.diagnostic_store.as_ref(), caller, request)
            }
            id if id == INSPECTOR_UPDATES_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                inspector::updates(
                    self.diagnostic_store.as_ref(),
                    caller,
                    request,
                    query.cursor,
                )
            }
            id if id == LOGS_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self.build_logs_view(caller, request, query.cursor).await?;
                let next_cursor = response.next_cursor.clone();
                views::view_page_with_cursor(response, next_cursor)
            }
            id if id == OPERATOR_LOGS_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self
                    .build_operator_logs_view(caller, request, query.cursor)
                    .await?;
                let next_cursor = response
                    .logs
                    .as_ref()
                    .and_then(|logs| logs.next_cursor.clone());
                views::view_page_with_cursor(response, next_cursor)
            }
            id if id == LLM_CONFIG_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_llm_config_view(caller).await?;
                views::view_page(response)
            }
            id if id == USER_MODEL_CATALOG_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_user_model_catalog_view(caller).await?;
                views::view_page(response)
            }
            id if id == USER_MODEL_PREFERENCE_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_user_model_preference_view(caller).await?;
                views::view_page(response)
            }
            id if id == ironclaw_product_contracts::history::HISTORY_SEARCH_VIEW.id => {
                let request: ironclaw_product_contracts::history::HistorySearchRequest =
                    views::parse_view_params(query.params)?;
                let scope =
                    crate::history_search::caller_thread_scope(&caller).ok_or_else(|| {
                        ProductSurfaceError::from_status(
                            ProductSurfaceErrorCode::InvalidRequest,
                            400,
                            false,
                        )
                    })?;
                // Indexing runs off the request path; this answer is a read.
                crate::history_search::spawn_index_catch_up(&self.thread_service, &scope);
                let response = crate::history_search::run_ready_history_search(
                    self.thread_service.as_ref(),
                    &scope,
                    &request,
                )
                .await?;
                views::view_page(response)
            }
            id if id == THREADS_VIEW.id => {
                let mut request: ProductListThreadsRequest =
                    views::parse_view_params(query.params)?;
                request.cursor = query.cursor.or(request.cursor);
                let response = self.build_threads_view(caller, request).await?;
                let next_cursor = response.next_cursor.clone();
                views::view_page_with_cursor(response, next_cursor)
            }
            id if id == NOTIFICATIONS_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self
                    .build_notifications_view(caller, request, query.cursor)
                    .await?;
                let next_cursor = response.next_cursor.clone();
                views::view_page_with_cursor(response, next_cursor)
            }
            id if id == AUTOMATIONS_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self.build_automations_view(caller, request).await?;
                views::view_page(response)
            }
            id if id == OUTBOUND_DELIVERY_TARGETS_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_outbound_delivery_targets_view(caller).await?;
                views::view_page(response)
            }
            id if id == NOTIFICATION_CHANNELS_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_notification_channels_view(caller).await?;
                views::view_page(response)
            }
            id if id == NOTIFICATION_SETUP_STATUS_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self
                    .notification_setup_service
                    .status(caller, request)
                    .await?;
                views::view_page(response)
            }
            id if id == TRACE_CREDITS_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_trace_credits_view(caller).await?;
                views::view_page(response)
            }
            id if id == TRACE_ACCOUNT_TRACES_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_trace_account_traces_view(caller).await?;
                views::view_page(response)
            }
            id if id == RUN_ARTIFACT_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let artifact = self.build_run_artifact(caller, request).await?;
                views::view_page(artifact)
            }
            id if id == THREAD_ARTIFACT_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let artifact = self.build_thread_artifact(caller, request).await?;
                views::view_page(artifact)
            }
            id if id == GLOBAL_AUTO_APPROVE_VIEW.id => {
                let _: RebornGlobalAutoApproveRequest = views::parse_view_params(query.params)?;
                let enabled = self.global_auto_approve_enabled(caller).await?;
                views::view_page(RebornGlobalAutoApproveResponse { enabled })
            }
            id if id == TIMELINE_VIEW.id => {
                let mut request: RebornTimelineRequest = views::parse_view_params(query.params)?;
                request.cursor = query.cursor.or(request.cursor);
                let response = self.get_timeline(caller, request).await?;
                views::view_page(response)
            }
            id if id == SUGGESTIONS_LIST_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.list_suggestions(caller).await?;
                views::view_page(response)
            }
            id if id == PROJECT_FS_LIST_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self.list_project_dir(caller, request).await?;
                views::view_page(response)
            }
            id if id == PROJECT_FS_STAT_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self.stat_project_path(caller, request).await?;
                views::view_page(response)
            }
            id if id == FS_MOUNTS_VIEW.id => {
                let _: RebornFsMountsRequest = views::parse_view_params(query.params)?;
                let response = self.list_fs_mounts(caller).await?;
                views::view_page(response)
            }
            id if id == FS_LIST_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self.browse_fs_dir(caller, request).await?;
                views::view_page(response)
            }
            id if id == FS_STAT_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self.stat_fs_path(caller, request).await?;
                views::view_page(response)
            }
            id if id == PROJECTS_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self.list_projects(caller, request).await?;
                views::view_page(response)
            }
            id if id == PROJECT_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self.get_project(caller, request).await?;
                views::view_page(response)
            }
            id if id == PROJECT_MEMBERS_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let response = self.list_project_members(caller, request).await?;
                views::view_page(response)
            }
            id if id == ADMIN_USERS_VIEW.id => {
                let mut request: RebornAdminUserListQuery = views::parse_view_params(query.params)?;
                request.cursor = query.cursor.or(request.cursor);
                let response = self.list_admin_users(caller, request).await?;
                views::view_page(response)
            }
            id if id == ADMIN_USER_VIEW.id => {
                let request: RebornAdminUserRequest = views::parse_view_params(query.params)?;
                let response = self.get_admin_user(caller, request.user_id).await?;
                views::view_page(response)
            }
            id if id == ADMIN_USER_SECRETS_VIEW.id => {
                let request: RebornAdminUserRequest = views::parse_view_params(query.params)?;
                let response = self
                    .list_admin_user_secrets(caller, request.user_id)
                    .await?;
                views::view_page(response)
            }
            id if id == ADMIN_THREAD_SCRAPE_THREADS_VIEW.id => {
                let mut request: RebornAdminThreadScrapeListRequest =
                    views::parse_view_params(query.params)?;
                request.cursor = query.cursor.or(request.cursor);
                let response = self
                    .list_admin_thread_scrape_threads(caller, request)
                    .await?;
                let next_cursor = response.next_cursor.clone();
                views::view_page_with_cursor(response, next_cursor)
            }
            id if id == ADMIN_THREAD_SCRAPE_ARTIFACT_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let artifact = self
                    .build_admin_thread_scrape_artifact(caller, request)
                    .await?;
                views::view_page(artifact)
            }
            id if id == ADMIN_THREAD_SCRAPE_RUN_ARTIFACT_VIEW.id => {
                let request = views::parse_view_params(query.params)?;
                let artifact = self
                    .build_admin_thread_scrape_run_artifact(caller, request)
                    .await?;
                views::view_page(artifact)
            }
            id if id == OPERATOR_CONFIG_LIST_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_operator_config_list_view(caller).await?;
                views::view_page(response)
            }
            id if id == OPERATOR_CONFIG_KEY_VIEW.id => {
                let response = self
                    .build_operator_config_key_view(caller, query.params)
                    .await?;
                views::view_page(response)
            }
            id if id == OPERATOR_CONFIG_VALIDATE_VIEW.id => {
                let response = self.build_operator_config_validate_view(query.params)?;
                views::view_page(response)
            }
            id if id == OPERATOR_SETUP_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_operator_setup_view(caller).await?;
                views::view_page(response)
            }
            id if id == EXTENSIONS_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = extensions::list_extensions(
                    Arc::clone(&self.lifecycle_service),
                    self.extension_credentials.clone(),
                    Arc::clone(&self.channel_connection_service),
                    self.session_channels.clone(),
                    caller,
                )
                .await?;
                views::view_page(response)
            }
            id if id == EXTENSION_REGISTRY_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = extensions::list_extension_registry(
                    self.lifecycle_service.as_ref(),
                    self.session_channels.as_deref(),
                    caller,
                )
                .await?;
                views::view_page(response)
            }
            id if id == EXTENSION_SETUP_VIEW.id => {
                let response = lifecycle_setup::setup_extension_view(
                    self.lifecycle_service.as_ref(),
                    self.extension_credentials.as_deref(),
                    self.channel_config_service.as_deref(),
                    caller,
                    query.params,
                )
                .await?;
                views::view_page(response)
            }
            id if id == SKILLS_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.skills_service.list_skills(caller).await?;
                views::view_page(response)
            }
            id if id == SKILL_SEARCH_VIEW.id => {
                let search_query = views::required_string_view_param(query.params, "query")?;
                let response = self
                    .skills_service
                    .search_skills(caller, search_query)
                    .await?;
                views::view_page(response)
            }
            id if id == SKILL_CONTENT_VIEW.id => {
                let name = views::required_string_view_param(query.params, "name")?;
                let response = self.skills_service.read_skill_content(caller, name).await?;
                views::view_page(response)
            }
            id if id == OPERATOR_DIAGNOSTICS_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_operator_diagnostics_view(caller).await?;
                views::view_page(response)
            }
            id if id == OPERATOR_STATUS_VIEW.id => {
                views::parse_empty_view_params(query.params)?;
                let response = self.build_operator_status_view(caller).await?;
                views::view_page(response)
            }
            _ => Err(ProductSurfaceError::not_found()),
        }
    }
}

#[async_trait]
impl<I, V> ironclaw_product_contracts::surface::ProductSurface for RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest,
    ) -> Result<
        ironclaw_product_contracts::surface::ProductSurfaceInvokeResponse,
        ironclaw_product_contracts::surface::ProductSurfaceError,
    > {
        if let Some(command) =
            product_capability_handlers::ProductCommandHandler::parse(&request.operation_id)
        {
            let output = command.invoke(self, caller, request.input).await?;
            return Ok(
                ironclaw_product_contracts::surface::ProductSurfaceInvokeResponse { output },
            );
        }
        let output = RebornServices::invoke(
            self,
            caller,
            request.operation_id,
            request.input,
            request.activity_id,
        )
        .await?;
        let output = serde_json::to_value(output).map_err(|error| {
            tracing::error!(%error, "failed to encode product surface invoke response");
            ironclaw_product_contracts::surface::ProductSurfaceError::internal()
        })?;
        Ok(ironclaw_product_contracts::surface::ProductSurfaceInvokeResponse { output })
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ironclaw_product_contracts::surface::ProductSurfaceQueryRequest,
    ) -> Result<
        ironclaw_product_contracts::surface::ProductSurfaceQueryPage,
        ironclaw_product_contracts::surface::ProductSurfaceError,
    > {
        let page = RebornServices::query(
            self,
            caller,
            RebornViewQuery {
                view_id: request.view_id,
                params: request.input,
                cursor: request.cursor,
            },
        )
        .await?;
        Ok(
            ironclaw_product_contracts::surface::ProductSurfaceQueryPage {
                items: vec![page.payload],
                next_cursor: page.next_cursor,
            },
        )
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ironclaw_product_contracts::surface::ProductSurfaceStreamRequest,
    ) -> Result<
        ironclaw_product_contracts::surface::ProductSurfaceStreamResponse,
        ironclaw_product_contracts::surface::ProductSurfaceError,
    > {
        let request = decode_product_surface_stream_request(request)?;
        if self
            .event_stream
            .as_ref()
            .is_some_and(|event_stream| event_stream.supports_subscription())
        {
            let subscription =
                open_product_surface_event_subscription(self, caller, request).await?;
            return match tokio::time::timeout(PRODUCT_STREAM_FIRST_EVENT_WAIT, subscription.next())
                .await
            {
                Ok(Some(Ok(mut response))) => {
                    response.subscription = Some(subscription);
                    Ok(response)
                }
                Ok(Some(Err(error))) => Err(error),
                Ok(None) | Err(_) => Ok(
                    ironclaw_product_contracts::surface::ProductSurfaceStreamResponse {
                        events: Vec::new(),
                        next_cursor: None,
                        subscription: None,
                    },
                ),
            };
        }
        let response = RebornServices::stream_events(self, caller, request).await?;
        encode_product_surface_stream_response(response)
    }
}

fn product_capability_input_error(field: &'static str) -> ProductSurfaceError {
    ProductSurfaceError::validation(field, ProductSurfaceValidationCode::InvalidValue)
}

fn product_secret_handle(handle: String) -> Result<SecretHandle, ProductSurfaceError> {
    SecretHandle::new(handle).map_err(|error| {
        tracing::debug!(%error, "admin user secret handle validation failed");
        product_capability_input_error("handle")
    })
}

fn segment(name: &str, value: &str) -> String {
    format!("{name}:{}:{value};", value.len())
}

fn map_adapter_error(error: ProductAdapterError) -> ProductSurfaceError {
    match error {
        ProductAdapterError::SurfaceRejected {
            kind,
            status_code,
            retryable,
            ..
        } => ProductSurfaceError::from_status_kind(
            code_for_status(status_code),
            kind_for_surface_rejection(kind),
            status_code,
            retryable,
        ),
        ProductAdapterError::SurfaceTransient { .. }
        | ProductAdapterError::EgressTransient { .. } => {
            ProductSurfaceError::service_unavailable(true)
        }
        ProductAdapterError::Authentication(_) => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::Unauthenticated, 401, false)
        }
        ProductAdapterError::MalformedInboundPayload { .. }
        | ProductAdapterError::InvalidIdentifier { .. } => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::InvalidRequest, 400, false)
        }
        ProductAdapterError::EgressDenied { .. }
        | ProductAdapterError::EgressUndeclaredHost { .. } => {
            ProductSurfaceError::from_status_kind(
                ProductSurfaceErrorCode::Forbidden,
                ProductSurfaceErrorKind::BlockedResource,
                403,
                false,
            )
        }
        ProductAdapterError::Internal { .. } => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::Internal, 500, false)
        }
    }
}

fn code_for_status(status_code: u16) -> ProductSurfaceErrorCode {
    match status_code {
        400 => ProductSurfaceErrorCode::InvalidRequest,
        401 => ProductSurfaceErrorCode::Unauthenticated,
        403 => ProductSurfaceErrorCode::Forbidden,
        404 => ProductSurfaceErrorCode::NotFound,
        409 => ProductSurfaceErrorCode::Conflict,
        429 => ProductSurfaceErrorCode::RateLimited,
        503 => ProductSurfaceErrorCode::Unavailable,
        _ => ProductSurfaceErrorCode::Internal,
    }
}

fn kind_for_surface_rejection(kind: ProductSurfaceRejectionKind) -> ProductSurfaceErrorKind {
    match kind {
        ProductSurfaceRejectionKind::ThreadBusy
        | ProductSurfaceRejectionKind::AdmissionRejected => ProductSurfaceErrorKind::Busy,
        ProductSurfaceRejectionKind::ScopeNotFound => ProductSurfaceErrorKind::NotFound,
        ProductSurfaceRejectionKind::Unauthorized => ProductSurfaceErrorKind::ParticipantDenied,
        ProductSurfaceRejectionKind::InvalidRequest => ProductSurfaceErrorKind::Validation,
        ProductSurfaceRejectionKind::Unavailable => ProductSurfaceErrorKind::ServiceUnavailable,
        ProductSurfaceRejectionKind::Conflict | ProductSurfaceRejectionKind::Ambiguous => {
            ProductSurfaceErrorKind::Conflict
        }
        ProductSurfaceRejectionKind::DuplicateAction => ProductSurfaceErrorKind::Duplicate,
        ProductSurfaceRejectionKind::ReplayUnavailable => {
            ProductSurfaceErrorKind::ReplayUnavailable
        }
    }
}

fn truncate_utf8_to_bytes(value: &str, max_bytes: usize) -> String {
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn product_agent_bound_caller_from_webui(
    caller: ProductSurfaceCaller,
) -> Option<ProductAgentBoundCaller> {
    let agent_id = caller.agent_id?;
    Some(ProductAgentBoundCaller::new(
        caller.tenant_id,
        caller.user_id,
        agent_id,
        caller.project_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::operator_config::{
        ToolPermissionUpdate, parse_tool_permission_state, tool_permission_state_wire,
    };
    use super::projects::map_project_service_error;
    use super::*;

    #[test]
    fn inspector_views_require_operator_config() {
        for view in [
            INSPECTOR_SNAPSHOT_VIEW.id,
            INSPECTOR_PROMPT_VIEW.id,
            INSPECTOR_TOOL_VIEW.id,
            INSPECTOR_UPDATES_VIEW.id,
        ] {
            assert!(product_view_requires_operator_config(view));
        }
    }

    /// The WebUI settings/tools request enum must use the exact wire strings
    /// the operator-config storage parser accepts and the entry writer emits.
    /// This pins the type link so the request vocabulary cannot drift from
    /// the approvals-owned resolved-state vocabulary (audit 2026-07, 6a).
    #[test]
    fn settings_tool_permission_state_wire_strings_stay_linked() {
        let cases = [
            (SettingsToolPermissionState::Default, "default", None),
            (
                SettingsToolPermissionState::AlwaysAllow,
                "always_allow",
                Some(ToolPermissionState::AlwaysAllow),
            ),
            (
                SettingsToolPermissionState::AskEachTime,
                "ask_each_time",
                Some(ToolPermissionState::AskEachTime),
            ),
            (
                SettingsToolPermissionState::Disabled,
                "disabled",
                Some(ToolPermissionState::Disabled),
            ),
        ];
        for (state, wire, resolved) in cases {
            let serialized = serde_json::to_value(state).unwrap();
            assert_eq!(serialized, serde_json::Value::String(wire.to_string()));
            assert_eq!(
                serde_json::from_value::<SettingsToolPermissionState>(serialized).unwrap(),
                state
            );
            // Round-trips through the storage parser the service applies on set.
            let update =
                parse_tool_permission_state(&serde_json::json!({ "state": wire })).unwrap();
            match (update, resolved) {
                (ToolPermissionUpdate::Default, None) => {}
                (ToolPermissionUpdate::State(parsed), Some(expected)) => {
                    assert_eq!(parsed, expected);
                    // The resolved states serialize to the same strings the
                    // config entry writer emits.
                    assert_eq!(tool_permission_state_wire(expected), wire);
                }
                _ => panic!("wire string {wire} no longer parses to the expected update"),
            }
        }
    }

    /// Every `ProjectServiceError` variant projects to a sanitized service error
    /// with the expected coarse code/status, and `InvalidInput`'s field name is
    /// carried through (it is a controlled constant, never backend text).
    #[test]
    fn project_service_error_maps_to_sanitized_service_error() {
        let not_found = map_project_service_error(ProjectServiceError::NotFound);
        assert_eq!(not_found.code, ProductSurfaceErrorCode::NotFound);
        assert_eq!(not_found.status_code, 404);

        let denied = map_project_service_error(ProjectServiceError::Denied);
        assert_eq!(denied.kind, ProductSurfaceErrorKind::ParticipantDenied);
        assert_eq!(denied.status_code, 403);

        let invalid = map_project_service_error(ProjectServiceError::InvalidInput {
            field: "project_id".to_string(),
        });
        assert_eq!(invalid.code, ProductSurfaceErrorCode::InvalidRequest);
        assert_eq!(invalid.status_code, 400);
        assert_eq!(invalid.field.as_deref(), Some("project_id"));

        let conflict = map_project_service_error(ProjectServiceError::Conflict);
        assert_eq!(conflict.code, ProductSurfaceErrorCode::Conflict);
        assert_eq!(conflict.status_code, 409);

        let unavailable = map_project_service_error(ProjectServiceError::Unavailable);
        assert_eq!(unavailable.code, ProductSurfaceErrorCode::Unavailable);
        assert_eq!(unavailable.status_code, 503);
        assert!(unavailable.retryable, "unavailable is retryable");

        let internal = map_project_service_error(ProjectServiceError::Internal);
        assert_eq!(internal.code, ProductSurfaceErrorCode::Internal);
        assert_eq!(internal.status_code, 500);
    }

    /// `require_project_service` returns `service_unavailable(false)` when no
    /// project service is wired (see the helper in this file). This locks the
    /// full shape of that sentinel — a clean, non-retryable 503 — so an unwired
    /// runtime returns a stable error rather than a panic or a 500.
    #[test]
    fn unwired_project_service_sentinel_is_503() {
        let unavailable = ProductSurfaceError::service_unavailable(false);
        assert_eq!(unavailable.code, ProductSurfaceErrorCode::Unavailable);
        assert_eq!(unavailable.status_code, 503);
        assert!(
            !unavailable.retryable,
            "false-arg sentinel is non-retryable"
        );
    }

    #[test]
    fn structured_finalization_errors_map_to_stable_product_statuses() {
        let conflict = map_thread_error(SessionThreadError::StructuredFinalizationConflict {
            turn_run_id: TurnRunId::new(),
        });
        assert_eq!(conflict.code, ProductSurfaceErrorCode::Conflict);
        assert_eq!(conflict.status_code, 409);
        assert!(!conflict.retryable);

        let invalid = map_thread_error(SessionThreadError::InvalidStructuredFinalization {
            reason: "malformed JSON".to_string(),
        });
        assert_eq!(invalid.code, ProductSurfaceErrorCode::Unavailable);
        assert_eq!(invalid.status_code, 503);
        assert!(invalid.retryable);
    }
}
