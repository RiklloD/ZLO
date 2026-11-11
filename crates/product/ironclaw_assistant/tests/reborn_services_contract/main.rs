//! Contract tests for WebUI-facing RebornServices service.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use chrono::Utc;

use ironclaw_approvals::{
    AutoApproveSettingInput, AutoApproveSettingKey, AutoApproveSettingRecord,
    AutoApproveSettingStorePort, CapabilityPermissionOverrideStorePort,
    CapabilityPermissionStoreError, PersistentApprovalAction, PersistentApprovalPolicy,
    PersistentApprovalPolicyError, PersistentApprovalPolicyInput, PersistentApprovalPolicyKey,
    PersistentApprovalPolicyStorePort, ToolPermissionOverride, ToolPermissionOverrideInput,
    ToolPermissionOverrideKey,
};

use ironclaw_assistant::EXTENSION_REGISTER_HOSTED_MCP_CAPABILITY_ID;

use ironclaw_assistant::inspector_store::{
    DiagnosticStoreError, DiagnosticStoreLimits, DiagnosticStorePort, InMemoryDiagnosticStore,
};

use ironclaw_assistant::{
    ADMIN_THREAD_SCRAPE_ARTIFACT_VIEW, ADMIN_THREAD_SCRAPE_RUN_ARTIFACT_VIEW,
    ADMIN_THREAD_SCRAPE_THREADS_VIEW, ADMIN_USER_DELETE_CAPABILITY_ID,
    ADMIN_USER_DELETE_SECRET_CAPABILITY_ID, ADMIN_USER_PUT_SECRET_CAPABILITY_ID,
    ADMIN_USER_SECRETS_VIEW, ADMIN_USER_SET_ROLE_CAPABILITY_ID,
    ADMIN_USER_SET_STATUS_CAPABILITY_ID, ADMIN_USER_UPDATE_CAPABILITY_ID, ADMIN_USER_VIEW,
    ADMIN_USERS_VIEW, AUTOMATION_DELETE_CAPABILITY_ID, AUTOMATION_LIST_DEFAULT_PAGE_SIZE,
    AUTOMATION_LIST_MAX_PAGE_SIZE, AUTOMATION_PAUSE_CAPABILITY_ID, AUTOMATION_RENAME_CAPABILITY_ID,
    AUTOMATION_RESUME_CAPABILITY_ID, AUTOMATION_RUN_CAPABILITY_ID,
    AUTOMATION_RUN_HISTORY_DEFAULT_PAGE_SIZE, AUTOMATION_RUN_HISTORY_MAX_PAGE_SIZE,
    AUTOMATION_TRIGGER_THREAD_SOURCE_TAG, AUTOMATIONS_VIEW, ApprovalInteractionActionView,
    ApprovalInteractionDecision, ApprovalInteractionScope, ApprovalInteractionService,
    AuthInteractionDecision, AuthInteractionService, AutomationListRequest,
    AutomationProductService, ChannelConnectionRequirement, CommandResultView,
    EXTENSION_IMPORT_CAPABILITY_ID, EXTENSION_SETUP_SUBMIT_CAPABILITY_ID, EXTENSION_SETUP_VIEW,
    EXTENSIONS_VIEW, EmptyProductCommandInput, ExtensionCredentialSetupService,
    ExtensionCredentialStatusRequest, ExtensionCredentialSubmitRequest, FS_LIST_VIEW,
    FS_MOUNTS_VIEW, FS_STAT_VIEW, FilesystemBrowseReader, FsMount, GLOBAL_AUTO_APPROVE_VIEW,
    LLM_ACTIVE_SET_CAPABILITY_ID, LLM_CONFIG_VIEW, LLM_PROVIDER_DELETE_CAPABILITY_ID,
    LLM_PROVIDER_UPSERT_CAPABILITY_ID, LOGS_VIEW, LifecycleChannelDirections,
    LifecycleExtensionCredentialRequirement, LifecycleExtensionCredentialSetup,
    LifecycleExtensionOnboarding, LifecycleExtensionRuntimeKind, LifecycleExtensionSource,
    LifecycleExtensionSummary, LifecycleInstalledExtensionSummary, LifecyclePackageKind,
    LifecyclePackageRef, LifecycleProductAction, LifecycleProductPayload, LifecycleProductResponse,
    LifecycleReadinessBlocker, ListPendingApprovalsRequest, ListPendingApprovalsResponse,
    ListPendingAuthInteractionsRequest, ListPendingAuthInteractionsResponse,
    OPERATOR_CONFIG_KEY_VIEW, OPERATOR_CONFIG_LIST_VIEW,
    OPERATOR_CONFIG_SET_AUTO_APPROVE_CAPABILITY_ID,
    OPERATOR_CONFIG_SET_TOOL_PERMISSION_CAPABILITY_ID, OPERATOR_CONFIG_VALIDATE_VIEW,
    OPERATOR_DIAGNOSTICS_VIEW, OPERATOR_LOGS_VIEW, OPERATOR_SETUP_RUN_CAPABILITY_ID,
    OPERATOR_SETUP_VIEW, OPERATOR_STATUS_VIEW, OUTBOUND_DELIVERY_TARGETS_VIEW,
    OutboundPreferencesProductService, PRODUCT_COMMAND_EXECUTE_COMMAND_ID,
    PRODUCT_COMMAND_LIST_COMMAND_ID, PRODUCT_NEW_COMMAND_OPERATION_ID,
    PRODUCT_STATUS_COMMAND_OPERATION_ID, PROJECT_DELETE_CAPABILITY_ID, PROJECT_FS_LIST_VIEW,
    PROJECT_FS_STAT_VIEW, PROJECT_MEMBER_ADD_CAPABILITY_ID, PROJECT_MEMBER_REMOVE_CAPABILITY_ID,
    PROJECT_MEMBER_UPDATE_CAPABILITY_ID, PROJECT_MEMBERS_VIEW, PROJECT_UPDATE_CAPABILITY_ID,
    PROJECT_VIEW, PROJECTS_VIEW, PendingApprovalInteractionView, ProductAgentBoundCaller,
    ProductCapabilityInvoker, ProductNewCommandInput, ProductNewCommandOutput,
    ProductStatusCommandInput, ProductSurfaceFailure, ProjectCaller, ProjectFilesystemReader,
    ProjectFsEntry, ProjectFsEntryKind, ProjectFsError, ProjectFsFile, ProjectFsStat,
    RUN_ARTIFACT_SCHEMA, RUN_ARTIFACT_VIEW, RebornAccountTracesResponse, RebornAddMemberRequest,
    RebornAttachmentRequest, RebornAutomationInfo, RebornAutomationMutationResponse,
    RebornAutomationRecentRunInfo, RebornAutomationRecentRunStatus, RebornAutomationRequest,
    RebornAutomationRunMutationResult, RebornAutomationRunMutationStatus,
    RebornAutomationRunStatus, RebornAutomationSource, RebornAutomationState,
    RebornChannelConnectAction, RebornChannelConnectStrategy, RebornCreateProjectRequest,
    RebornDeleteProjectRequest, RebornDeleteThreadRequest, RebornExecuteProductCommandRequest,
    RebornExecuteProductCommandResponse, RebornExtensionListResponse, RebornExtensionSurface,
    RebornFsListRequest, RebornFsListResponse, RebornFsMountsRequest, RebornFsMountsResponse,
    RebornFsStatRequest, RebornFsStatResponse, RebornGetProjectRequest, RebornGetRunStateRequest,
    RebornGlobalAutoApproveRequest, RebornGlobalAutoApproveResponse, RebornListAutomationsResponse,
    RebornListMembersRequest, RebornListMembersResponse, RebornListProjectsRequest,
    RebornListProjectsResponse, RebornListThreadsResponse, RebornOperatorCommandPlaneResponse,
    RebornOperatorConfigDiagnosticSeverity, RebornOperatorConfigGetResponse,
    RebornOperatorConfigListResponse, RebornOperatorConfigSetRequest,
    RebornOperatorConfigValidateResponse, RebornOperatorLogsQuery, RebornOperatorSetupRequest,
    RebornOperatorSetupStatus, RebornOperatorSurfaceStatus,
    RebornOutboundDeliveryTargetCapabilities, RebornOutboundDeliveryTargetDescription,
    RebornOutboundDeliveryTargetId, RebornOutboundDeliveryTargetListResponse,
    RebornOutboundDeliveryTargetOption, RebornOutboundDeliveryTargetSummary,
    RebornProductCommandListResponse, RebornProjectFsListRequest, RebornProjectFsListResponse,
    RebornProjectFsStatRequest, RebornProjectFsStatResponse, RebornProjectInfo,
    RebornProjectMemberInfo, RebornProjectMemberStatus, RebornProjectResponse, RebornProjectRole,
    RebornProjectState, RebornRemoveMemberRequest, RebornRenameAutomationProductRequest,
    RebornResolveGateResponse, RebornRunArtifact, RebornRunArtifactRequest, RebornServices,
    RebornSetNotificationChannelsRequest, RebornSetupExtensionResponse, RebornSkillContentResponse,
    RebornSkillInfo, RebornSkillListResponse, RebornSkillSearchResponse, RebornSkillSourceKind,
    RebornSkillTrustLevel, RebornStreamEventsRequest, RebornSubmitTurnResponse,
    RebornThreadArtifact, RebornThreadArtifactRequest, RebornTimelineRequest,
    RebornTimelineResponse, RebornTraceCreditsResponse, RebornTraceHoldAuthorizeProductRequest,
    RebornTraceHoldAuthorizeResponse, RebornUpdateMemberRoleRequest, RebornUpdateProjectRequest,
    ResolveApprovalInteractionRequest, ResolveApprovalInteractionResponse,
    ResolveAuthInteractionRequest, ResolveAuthInteractionResponse, SKILL_CONTENT_VIEW,
    SKILL_SEARCH_VIEW, SKILLS_VIEW, SkillsProductService, StaticOperatorStatusService,
    THREAD_ARTIFACT_MAX_MESSAGES, THREAD_ARTIFACT_VIEW, THREAD_DELETE_CAPABILITY_ID, THREADS_VIEW,
    TIMELINE_VIEW, TRACE_ACCOUNT_TRACES_VIEW, TRACE_CREDITS_VIEW, TRACE_HOLD_AUTHORIZE_COMMAND,
    TriggerRunThreadScope, approval_gate_ref, automation_trigger_thread_metadata_json,
};

use ironclaw_assistant::{
    RebornAdminCreateUserRequest, RebornAdminDeleteSecretProductRequest,
    RebornAdminPutSecretProductRequest, RebornAdminPutSecretRequest,
    RebornAdminSetRoleProductRequest, RebornAdminSetRoleRequest,
    RebornAdminSetStatusProductRequest, RebornAdminSetStatusRequest,
    RebornAdminThreadScrapeArtifactRequest, RebornAdminThreadScrapeListRequest,
    RebornAdminThreadScrapeRunArtifactRequest, RebornAdminUpdateUserProductRequest,
    RebornAdminUpdateUserRequest, RebornAdminUserListQuery, RebornAdminUserListResponse,
    RebornAdminUserRequest, RebornAdminUserResponse, RebornAdminUserSecretsListResponse,
};

use ironclaw_attachments::{InboundAttachmentLander, InboundAttachmentReader};

use ironclaw_auth::{
    AuthAccountLastError, AuthAccountState, ChannelAuthAccountState, ChannelConnectionService,
    CredentialAccountId, CredentialAccountProjection, CredentialAccountStatus,
};

use ironclaw_extension_contracts::external::ExternalConversationRef;

use ironclaw_extension_contracts::hosted_mcp::HostedMcpAuthSelection;

use ironclaw_extension_contracts::{
    state::{InstallationState, LifecyclePublicState},
    surface::CapabilitySurfaceKind,
};

use ironclaw_host_api::attachment::InboundAttachment;

use ironclaw_host_api::product_adapter::{
    AdapterInstallationId, ProductAdapterError, ProductAdapterId, ProductSurfaceRejectionKind,
    ProtocolAuthFailure, RedactedString,
};

use ironclaw_host_api::turn::{
    AcceptedMessageRef, CapabilityActivityId, EventCursor, ReplyTargetBindingRef, RunProfileId,
    RunProfileVersion, SanitizedFailure, TurnActor, TurnGateRef, TurnId, TurnRunId, TurnScope,
    TurnStatus,
};

use ironclaw_host_api::{
    capability::{EffectKind, PermissionMode},
    ids::{
        ActivityId, AgentId, ApprovalRequestId, CapabilityId, ExtensionId, InvocationId, ProjectId,
        ResultRef, SecretHandle, TenantId, ThreadId, UserId,
    },
    resolution::{Outcome, OutcomeRefs, Resolution, ResultPreviewMeta, ToolVerdict},
    resource::ResourceScope,
    result_meta::{ResultProgress, TerminateHint},
    safe_summary::SafeSummary,
    scope::Principal,
};

use ironclaw_loop_contracts::{LoopModelRouteSnapshot, LoopModelUsage};

use ironclaw_product_contracts::admin_users::{
    AdminCreateUserFields, AdminCreatedUser, AdminUserError, AdminUserRecord, AdminUserRole,
    AdminUserSecretMeta, AdminUserService, AdminUserStatus,
};

use ironclaw_product_contracts::channel_config::ChannelConfigProductService;

use ironclaw_product_contracts::inbound::ProductRejectionKind;

use ironclaw_product_contracts::inbound_requests::{
    ProductCancelRunRequest, ProductCreateThreadRequest, ProductListAutomationsRequest,
    ProductListThreadsRequest, ProductRenameAutomationRequest, ProductResolveGateRequest,
    ProductRetryRunRequest, ProductSetupExtensionRequest, ProductSubmitTurnRequest,
};

use ironclaw_product_contracts::inspector::{
    BoundedDiagnosticText, DiagnosticActivityEvent, DiagnosticCursor, DiagnosticModelCallId,
    DiagnosticScope, DiagnosticSnapshot, DiagnosticUpdateBatch, InspectorModelCallStatus,
    ModelCallDiagnostic, PromptDiagnostic, ToolExecutionDiagnostic,
};

use ironclaw_product_contracts::ironhub::{
    IRONHUB_DELIVER_INSTALL_COMMAND_ID, IronhubInstallDeliveryRequest,
    IronhubInstallDeliveryResult, IronhubLinkError, IronhubLinkService, IronhubRegisterRequest,
};

use ironclaw_product_contracts::lifecycle_service::{
    LifecycleProductContext, LifecycleProductService,
};

use ironclaw_product_contracts::notification_inbox::{
    NOTIFICATIONS_MARK_ALL_READ_COMMAND_ID, NOTIFICATIONS_MARK_READ_COMMAND_ID, NOTIFICATIONS_VIEW,
    ProductListNotificationsResponse, ProductNotificationKind, ProductNotificationMutationRequest,
    ProductNotificationMutationResponse,
};

use ironclaw_product_contracts::operator_llm::{
    ActiveModelReader, CodexLoginStart, LlmActiveSelection, LlmConfigService,
    LlmConfigServiceError, LlmConfigSnapshot, LlmModelsResult, LlmProbeRequest, LlmProbeResult,
    LlmProviderView, NearAiLoginRequest, NearAiLoginStart, NearAiWalletLoginRequest,
    NearAiWalletLoginResult, SetActiveLlmRequest, SetUserModelPreferenceRequest,
    UpsertLlmProviderRequest, UserModelCatalog, UserModelPreference,
};

use ironclaw_product_contracts::operator_service::{
    OperatorLogsService, OperatorServiceLifecycleService, OperatorStatusService,
};

use ironclaw_product_contracts::operator_tools::{
    RebornOperatorToolCatalog, RebornOperatorToolInfo,
};

use ironclaw_product_contracts::outbound::{
    ProductOutboundEnvelope, ProductOutboundPayload, ProductOutboundTarget, ProjectionCursor,
};

use ironclaw_product_contracts::package_lifecycle::ChannelConfigField as RebornChannelConfigField;

use ironclaw_product_contracts::product_wire::{
    RebornLogLevel, RebornLogQueryRequest, RebornLogQueryResponse, RebornOperatorStatusCheck,
    RebornOperatorStatusResponse, RebornOperatorStatusSeverity, RebornOperatorStatusState,
    RebornServiceLifecycleAction, RebornServiceLifecycleRequest, RebornServiceLifecycleResponse,
    RebornServiceLifecycleState,
};

use ironclaw_product_contracts::project_service::{ProjectService, ProjectServiceError};

use ironclaw_product_contracts::projection::ProjectionStream;

use ironclaw_product_contracts::projection::{
    ProjectionStreamSubscription, ProjectionSubscriptionRequest,
};

use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode,
    ProductSurfaceErrorKind, ProductSurfaceInvokeRequest, ProductSurfaceStreamRequest,
    ProductSurfaceValidationCode,
};

use ironclaw_product_contracts::views::{RebornViewPage, RebornViewQuery};

use ironclaw_threads::{
    AcceptInboundMessageRequest, AcceptedInboundMessage, AcceptedInboundMessageReplay,
    AppendAssistantDraftRequest, AppendCapabilityDisplayPreviewRequest,
    AppendToolResultReferenceRequest, AttachmentKind, AttachmentRef, BoundedThreadMessageSnapshot,
    BoundedThreadMessages, BoundedThreadMessagesRequest, ContextMessage, ContextMessages,
    ContextWindow, CreateSummaryArtifactRequest, EnsureThreadRequest, InMemorySessionThreadService,
    ListThreadsForScopeRequest, ListThreadsForScopeResponse, LoadContextMessagesRequest,
    LoadContextWindowRequest, MessageContent, MessageKind, MessageStatus, RedactMessageRequest,
    ReplayAcceptedInboundMessageRequest, SessionThreadError, SessionThreadRecord,
    SessionThreadService, SummaryArtifact, ThreadHistory, ThreadHistoryRequest, ThreadMessageId,
    ThreadMessageRange, ThreadMessageRecord, ThreadScope, UpdateAssistantDraftRequest,
    UpdateToolResultReferenceRequest,
};

use ironclaw_triggers::AutomationName;

use ironclaw_turns::test_support::in_memory_agent_turn_runtime;

use ironclaw_turns::{
    AdmissionRejection, AdmissionRejectionReason, CancelRunRequest, CancelRunResponse,
    DefaultTurnCoordinator, GetRunStateRequest, ResumeTurnPrecondition, ResumeTurnRequest,
    ResumeTurnResponse, RetryTurnRequest, RetryTurnResponse, SubmitTurnRequest, SubmitTurnResponse,
    TurnCapacityResource, TurnCoordinator, TurnError, TurnOriginKind, TurnRunState,
};

use secrecy::SecretString;

use serde::Serialize;

use serde_json::json;

use tokio::sync::{Notify, mpsc, oneshot};

use tracing_test::traced_test;

mod support;
use support::*;

mod admin;
mod artifacts;
mod attachments;
mod automation_trigger_threads;
mod automations;
mod busy_replay;
mod error_taxonomy;
mod extensions;
mod gates;
mod operator_config;
mod operator_logs;
mod operator_setup;
mod outbound;
mod product_commands;
mod runs;
mod submit_turn;
mod surface;
mod thread_list_and_notifications;
mod threads_and_streams;
