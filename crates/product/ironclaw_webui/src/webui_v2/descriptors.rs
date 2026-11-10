//! Host-owned route descriptors for the Reborn WebChat v2 surface.
//!
//! Host composition consumes [`webui_v2_routes`] and mounts the matching
//! handler from [`crate::webui_v2::handlers`] under each descriptor's pattern. The
//! descriptor is the contract: changing a route's policy here changes what
//! host composition enforces before the handler runs.
// arch-exempt: large_file, missing aggregation is a typed route-family registry that can assemble split descriptor modules into this canonical policy table, plan #6524

// arch-exempt: large_file, one descriptor function per WebUI v2 route mirrors the handler/router split tracked for this whole module family, plan #5985

// arch-exempt: large_file, this is the 1:1 descriptor counterpart to the handler table's own large_file exemption — both stay flat until the same WebUI route split lands, plan #5985

use ironclaw_host_api::ingress::{
    AllowedEffectPath, AuditTraceClass, BodyLimitPolicy, CorsPolicy, IngressAuthPolicy,
    IngressAuthScheme, IngressPolicy, IngressPolicyParts, IngressRouteDescriptor, ListenerClass,
    RateLimitPolicy, RateLimitScope, StreamingMode, WebSocketOriginPolicy,
};
use ironclaw_host_api::{action::NetworkMethod, ingress::IngressScopeSource};
use std::num::{NonZeroU32, NonZeroU64};

mod run_action_descriptors;

pub use run_action_descriptors::{
    WEBUI_V2_PATTERN_CANCEL_RUN, WEBUI_V2_PATTERN_RESOLVE_GATE, WEBUI_V2_PATTERN_RETRY_RUN,
    WEBUI_V2_ROUTE_CANCEL_RUN, WEBUI_V2_ROUTE_RESOLVE_GATE, WEBUI_V2_ROUTE_RETRY_RUN,
};
use run_action_descriptors::{
    cancel_run_descriptor, resolve_gate_descriptor, retry_run_descriptor,
};

pub const WEBUI_V2_ROUTE_CREATE_THREAD: &str = "webui.v2.create_thread";
pub const WEBUI_V2_ROUTE_DELETE_THREAD: &str = "webui.v2.delete_thread";
pub const WEBUI_V2_ROUTE_GET_SESSION: &str = "webui.v2.get_session";
pub const WEBUI_V2_ROUTE_SESSION_CHANNEL_MESSAGE: &str = "webui.v2.session_channel_message";
pub const WEBUI_V2_ROUTE_LIST_THREADS: &str = "webui.v2.list_threads";
pub const WEBUI_V2_ROUTE_LIST_NOTIFICATIONS: &str = "webui.v2.list_notifications";
pub const WEBUI_V2_ROUTE_MARK_NOTIFICATION_READ: &str = "webui.v2.mark_notification_read";
pub const WEBUI_V2_ROUTE_MARK_ALL_NOTIFICATIONS_READ: &str = "webui.v2.mark_all_notifications_read";
pub const WEBUI_V2_ROUTE_ARCHIVE_NOTIFICATION: &str = "webui.v2.archive_notification";
pub const WEBUI_V2_ROUTE_GET_TIMELINE: &str = "webui.v2.get_timeline";
pub const WEBUI_V2_ROUTE_GET_RUN_ARTIFACT: &str = "webui.v2.get_run_artifact";
pub const WEBUI_V2_ROUTE_GET_THREAD_ARTIFACT: &str = "webui.v2.get_thread_artifact";
pub const WEBUI_V2_ROUTE_GET_ATTACHMENT: &str = "webui.v2.get_attachment";
pub const WEBUI_V2_ROUTE_STREAM_EVENTS: &str = "webui.v2.stream_events";
pub const WEBUI_V2_ROUTE_LIST_COMMANDS: &str = "webui.v2.list_commands";
pub const WEBUI_V2_ROUTE_EXECUTE_COMMAND: &str = "webui.v2.execute_command";
pub const WEBUI_V2_ROUTE_LIST_AUTOMATIONS: &str = "webui.v2.list_automations";
pub const WEBUI_V2_ROUTE_RUN_AUTOMATION: &str = "webui.v2.run_automation";
pub const WEBUI_V2_ROUTE_PAUSE_AUTOMATION: &str = "webui.v2.pause_automation";
pub const WEBUI_V2_ROUTE_RESUME_AUTOMATION: &str = "webui.v2.resume_automation";
pub const WEBUI_V2_ROUTE_RENAME_AUTOMATION: &str = "webui.v2.rename_automation";
pub const WEBUI_V2_ROUTE_DELETE_AUTOMATION: &str = "webui.v2.delete_automation";
pub const WEBUI_V2_ROUTE_SUGGESTIONS_LIST: &str = "webui.v2.suggestions.list";
pub const WEBUI_V2_ROUTE_SUGGESTIONS_GENERATE: &str = "webui.v2.suggestions.generate";
pub const WEBUI_V2_ROUTE_SUGGESTION_START: &str = "webui.v2.suggestions.start";
pub const WEBUI_V2_ROUTE_SUGGESTION_DISMISS: &str = "webui.v2.suggestions.dismiss";
pub const WEBUI_V2_ROUTE_TRACE_CREDITS: &str = "webui.v2.trace_credits";
pub const WEBUI_V2_ROUTE_TRACE_ACCOUNT_TRACES: &str = "webui.v2.trace_account_traces";
pub const WEBUI_V2_ROUTE_TRACE_HOLD_AUTHORIZE: &str = "webui.v2.authorize_trace_hold";
pub const WEBUI_V2_ROUTE_TRACE_ACCOUNT_LOGIN_LINK: &str = "webui.v2.trace_account_login_link";
pub const WEBUI_V2_ROUTE_LIST_OUTBOUND_DELIVERY_TARGETS: &str =
    "webui.v2.list_outbound_delivery_targets";
pub const WEBUI_V2_ROUTE_GET_NOTIFICATION_CHANNELS: &str = "webui.v2.get_notification_channels";
pub const WEBUI_V2_ROUTE_SET_NOTIFICATION_CHANNELS: &str = "webui.v2.set_notification_channels";
pub const WEBUI_V2_ROUTE_NOTIFICATION_SETUP_STATUS: &str = "webui.v2.notification_setup_status";
pub const WEBUI_V2_ROUTE_NOTIFICATION_SETUP_ENABLE: &str = "webui.v2.notification_setup_enable";
pub const WEBUI_V2_ROUTE_NOTIFICATION_SETUP_DISABLE: &str = "webui.v2.notification_setup_disable";
pub const WEBUI_V2_ROUTE_LIST_EXTENSIONS: &str = "webui.v2.list_extensions";
pub const WEBUI_V2_ROUTE_LIST_EXTENSION_REGISTRY: &str = "webui.v2.list_extension_registry";
pub const WEBUI_V2_ROUTE_INSTALL_EXTENSION: &str = "webui.v2.install_extension";
pub const WEBUI_V2_ROUTE_IRONHUB_DELIVER_INSTALL: &str = "webui.v2.ironhub_deliver_install";
pub const WEBUI_V2_ROUTE_REGISTER_HOSTED_MCP_EXTENSION: &str =
    "webui.v2.register_hosted_mcp_extension";
pub const WEBUI_V2_ROUTE_IMPORT_EXTENSION: &str = "webui.v2.import_extension";
pub const WEBUI_V2_ROUTE_REMOVE_EXTENSION: &str = "webui.v2.remove_extension";
pub const WEBUI_V2_ROUTE_GET_EXTENSION_SETUP: &str = "webui.v2.get_extension_setup";
pub const WEBUI_V2_ROUTE_SETUP_EXTENSION: &str = "webui.v2.setup_extension";
pub const WEBUI_V2_ROUTE_LIST_SKILLS: &str = "webui.v2.list_skills";
pub const WEBUI_V2_ROUTE_SEARCH_SKILLS: &str = "webui.v2.search_skills";
pub const WEBUI_V2_ROUTE_INSTALL_SKILL: &str = "webui.v2.install_skill";
pub const WEBUI_V2_ROUTE_GET_SKILL: &str = "webui.v2.get_skill";
pub const WEBUI_V2_ROUTE_UPDATE_SKILL: &str = "webui.v2.update_skill";
pub const WEBUI_V2_ROUTE_REMOVE_SKILL: &str = "webui.v2.remove_skill";
pub const WEBUI_V2_ROUTE_SET_SKILL_AUTO_ACTIVATE: &str = "webui.v2.set_skill_auto_activate";
pub const WEBUI_V2_ROUTE_SET_AUTO_ACTIVATE_LEARNED: &str = "webui.v2.set_auto_activate_learned";
pub const WEBUI_V2_ROUTE_LIST_SETTINGS_TOOLS: &str = "webui.v2.settings.list_tools";
pub const WEBUI_V2_ROUTE_SET_SETTINGS_TOOLS_AUTO_APPROVE: &str =
    "webui.v2.settings.set_tools_auto_approve";
pub const WEBUI_V2_ROUTE_SET_SETTINGS_TOOL_PERMISSION: &str =
    "webui.v2.settings.set_tool_permission";
pub const WEBUI_V2_ROUTE_GET_LLM_CONFIG: &str = "webui.v2.get_llm_config";
pub const WEBUI_V2_ROUTE_UPSERT_LLM_PROVIDER: &str = "webui.v2.upsert_llm_provider";
pub const WEBUI_V2_ROUTE_DELETE_LLM_PROVIDER: &str = "webui.v2.delete_llm_provider";
pub const WEBUI_V2_ROUTE_SET_ACTIVE_LLM: &str = "webui.v2.set_active_llm";
pub const WEBUI_V2_ROUTE_GET_USER_MODEL_CATALOG: &str = "webui.v2.get_user_model_catalog";
pub const WEBUI_V2_ROUTE_SET_USER_MODEL_POLICY: &str = "webui.v2.set_user_model_policy";
pub const WEBUI_V2_ROUTE_GET_USER_MODEL_PREFERENCE: &str = "webui.v2.get_user_model_preference";
pub const WEBUI_V2_ROUTE_SET_USER_MODEL_PREFERENCE: &str = "webui.v2.set_user_model_preference";
pub const WEBUI_V2_ROUTE_TEST_LLM_CONNECTION: &str = "webui.v2.test_llm_connection";
pub const WEBUI_V2_ROUTE_LIST_LLM_MODELS: &str = "webui.v2.list_llm_models";
pub const WEBUI_V2_ROUTE_START_NEARAI_LOGIN: &str = "webui.v2.start_nearai_login";
pub const WEBUI_V2_ROUTE_COMPLETE_NEARAI_WALLET_LOGIN: &str =
    "webui.v2.complete_nearai_wallet_login";
pub const WEBUI_V2_ROUTE_START_CODEX_LOGIN: &str = "webui.v2.start_codex_login";
pub const WEBUI_V2_ROUTE_OPERATOR_GET_SETUP: &str = "webui.v2.operator.get_setup";
pub const WEBUI_V2_ROUTE_OPERATOR_RUN_SETUP: &str = "webui.v2.operator.run_setup";
pub const WEBUI_V2_ROUTE_OPERATOR_LIST_CONFIG: &str = "webui.v2.operator.list_config";
pub const WEBUI_V2_ROUTE_OPERATOR_GET_CONFIG_KEY: &str = "webui.v2.operator.get_config_key";
pub const WEBUI_V2_ROUTE_OPERATOR_SET_CONFIG_KEY: &str = "webui.v2.operator.set_config_key";
pub const WEBUI_V2_ROUTE_OPERATOR_VALIDATE_CONFIG: &str = "webui.v2.operator.validate_config";
pub const WEBUI_V2_ROUTE_OPERATOR_DIAGNOSTICS: &str = "webui.v2.operator.diagnostics";
pub const WEBUI_V2_ROUTE_OPERATOR_STATUS: &str = "webui.v2.operator.status";
pub const WEBUI_V2_ROUTE_OPERATOR_LOGS: &str = "webui.v2.operator.logs";
pub const WEBUI_V2_ROUTE_OPERATOR_SERVICE_LIFECYCLE: &str = "webui.v2.operator.service_lifecycle";
pub const WEBUI_V2_ROUTE_INSPECTOR_SNAPSHOT: &str = "webui.v2.operator.inspector_snapshot";
pub const WEBUI_V2_ROUTE_INSPECTOR_PROMPT: &str = "webui.v2.operator.inspector_prompt";
pub const WEBUI_V2_ROUTE_INSPECTOR_TOOL: &str = "webui.v2.operator.inspector_tool";
pub const WEBUI_V2_ROUTE_INSPECTOR_UPDATES: &str = "webui.v2.operator.inspector_updates";
pub const WEBUI_V2_ROUTE_OPERATOR_LIST_EXTENSION_CONFIGURATION: &str =
    "webui.v2.operator.list_extension_configuration";
pub const WEBUI_V2_ROUTE_OPERATOR_REPLACE_EXTENSION_CONFIGURATION: &str =
    "webui.v2.operator.replace_extension_configuration";
pub const WEBUI_V2_ROUTE_LOGS: &str = "webui.v2.logs";
pub const WEBUI_V2_ROUTE_LIST_PROJECT_FILES: &str = "webui.v2.list_project_files";
pub const WEBUI_V2_ROUTE_STAT_PROJECT_FILE: &str = "webui.v2.stat_project_file";
pub const WEBUI_V2_ROUTE_READ_PROJECT_FILE: &str = "webui.v2.read_project_file";
pub const WEBUI_V2_ROUTE_INDEX_PROJECT_FILES: &str = "webui.v2.index_project_files";
pub const WEBUI_V2_ROUTE_LIST_FS_MOUNTS: &str = "webui.v2.list_fs_mounts";
pub const WEBUI_V2_ROUTE_BROWSE_FS_DIR: &str = "webui.v2.browse_fs_dir";
pub const WEBUI_V2_ROUTE_STAT_FS_PATH: &str = "webui.v2.stat_fs_path";
pub const WEBUI_V2_ROUTE_READ_FS_FILE: &str = "webui.v2.read_fs_file";
pub const WEBUI_V2_ROUTE_LIST_PROJECTS: &str = "webui.v2.list_projects";
pub const WEBUI_V2_ROUTE_CREATE_PROJECT: &str = "webui.v2.create_project";
pub const WEBUI_V2_ROUTE_GET_PROJECT: &str = "webui.v2.get_project";
pub const WEBUI_V2_ROUTE_UPDATE_PROJECT: &str = "webui.v2.update_project";
pub const WEBUI_V2_ROUTE_DELETE_PROJECT: &str = "webui.v2.delete_project";
pub const WEBUI_V2_ROUTE_LIST_PROJECT_MEMBERS: &str = "webui.v2.list_project_members";
pub const WEBUI_V2_ROUTE_ADD_PROJECT_MEMBER: &str = "webui.v2.add_project_member";
pub const WEBUI_V2_ROUTE_UPDATE_PROJECT_MEMBER: &str = "webui.v2.update_project_member";
pub const WEBUI_V2_ROUTE_REMOVE_PROJECT_MEMBER: &str = "webui.v2.remove_project_member";
pub const WEBUI_V2_ROUTE_ADMIN_LIST_USERS: &str = "webui.v2.admin.list_users";
pub const WEBUI_V2_ROUTE_ADMIN_CREATE_USER: &str = "webui.v2.admin.create_user";
pub const WEBUI_V2_ROUTE_ADMIN_GET_USER: &str = "webui.v2.admin.get_user";
pub const WEBUI_V2_ROUTE_ADMIN_UPDATE_USER: &str = "webui.v2.admin.update_user";
pub const WEBUI_V2_ROUTE_ADMIN_DELETE_USER: &str = "webui.v2.admin.delete_user";
pub const WEBUI_V2_ROUTE_ADMIN_SET_USER_STATUS: &str = "webui.v2.admin.set_user_status";
pub const WEBUI_V2_ROUTE_ADMIN_SET_USER_ROLE: &str = "webui.v2.admin.set_user_role";
pub const WEBUI_V2_ROUTE_ADMIN_LIST_USER_SECRETS: &str = "webui.v2.admin.list_user_secrets";
pub const WEBUI_V2_ROUTE_ADMIN_PUT_USER_SECRET: &str = "webui.v2.admin.put_user_secret";
pub const WEBUI_V2_ROUTE_ADMIN_DELETE_USER_SECRET: &str = "webui.v2.admin.delete_user_secret";
pub const WEBUI_V2_ROUTE_ADMIN_LIST_THREAD_SCRAPE_THREADS: &str =
    "webui.v2.admin.thread_scrape.list_threads";
pub const WEBUI_V2_ROUTE_ADMIN_GET_THREAD_SCRAPE_ARTIFACT: &str =
    "webui.v2.admin.thread_scrape.get_thread_artifact";
pub const WEBUI_V2_ROUTE_ADMIN_GET_THREAD_SCRAPE_RUN_ARTIFACT: &str =
    "webui.v2.admin.thread_scrape.get_run_artifact";

// Orchestrator routes (delegated tasks, thread queue, runs, lineage, fork, merge-back).
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_TASKS: &str = "webui.v2.orchestrator.thread_tasks";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_QUEUE: &str = "webui.v2.orchestrator.thread_queue";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_QUEUE_MUTATE: &str = "webui.v2.orchestrator.queue_mutate";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_LINEAGE: &str = "webui.v2.orchestrator.thread_lineage";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_RUNS: &str = "webui.v2.orchestrator.thread_runs";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_TELEMETRY: &str =
    "webui.v2.orchestrator.thread_telemetry";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_CONTEXT: &str = "webui.v2.orchestrator.thread_context";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_FORK: &str = "webui.v2.orchestrator.thread_fork";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_ROUTE: &str = "webui.v2.orchestrator.thread_route";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_ROUTE_SET: &str =
    "webui.v2.orchestrator.thread_route_set";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_MERGE_BACK: &str =
    "webui.v2.orchestrator.thread_merge_back";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_WORKTREE: &str =
    "webui.v2.orchestrator.thread_worktree";
pub const WEBUI_V2_ROUTE_THREAD_GIT: &str = "webui.v2.orchestrator.thread_git";
pub const WEBUI_V2_ROUTE_THREAD_GIT_CHANGE: &str = "webui.v2.orchestrator.thread_git_change";
pub const WEBUI_V2_ROUTE_THREAD_CONSOLE: &str = "webui.v2.orchestrator.thread_console";
pub const WEBUI_V2_ROUTE_THREAD_CONSOLE_CONTROL: &str =
    "webui.v2.orchestrator.thread_console_control";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_ACTION: &str = "webui.v2.orchestrator.thread_action";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_PROVIDERS: &str = "webui.v2.orchestrator.providers";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_TASK: &str = "webui.v2.orchestrator.task";
pub const WEBUI_V2_ROUTE_ORCHESTRATOR_TASK_CANCEL: &str = "webui.v2.orchestrator.task_cancel";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_TASKS: &str =
    "/api/webchat/v2/threads/{thread_id}/tasks";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_QUEUE: &str =
    "/api/webchat/v2/threads/{thread_id}/queue";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_QUEUE_MUTATE: &str =
    "/api/webchat/v2/threads/{thread_id}/queue";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_LINEAGE: &str =
    "/api/webchat/v2/threads/{thread_id}/lineage";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_RUNS: &str =
    "/api/webchat/v2/threads/{thread_id}/runs";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_TELEMETRY: &str =
    "/api/webchat/v2/threads/{thread_id}/telemetry";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_CONTEXT: &str =
    "/api/webchat/v2/threads/{thread_id}/context";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_ROUTE: &str =
    "/api/webchat/v2/threads/{thread_id}/route";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_ROUTE_SET: &str =
    "/api/webchat/v2/threads/{thread_id}/route";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_FORK: &str =
    "/api/webchat/v2/threads/{thread_id}/fork";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_MERGE_BACK: &str =
    "/api/webchat/v2/threads/{thread_id}/merge-back";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_WORKTREE: &str =
    "/api/webchat/v2/threads/{thread_id}/worktree";
/// GET (a read) and POST (a change) of the thread's repository on the backend.
pub const WEBUI_V2_PATTERN_THREAD_GIT: &str = "/api/webchat/v2/threads/{thread_id}/git";
/// GET (output after a cursor) and POST (start / cancel / clear) of its console.
pub const WEBUI_V2_PATTERN_THREAD_CONSOLE: &str = "/api/webchat/v2/threads/{thread_id}/console";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_ACTION: &str =
    "/api/webchat/v2/threads/{thread_id}/action";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_PROVIDERS: &str = "/api/webchat/v2/providers";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_TASK: &str = "/api/webchat/v2/tasks/{task_id}";
pub const WEBUI_V2_PATTERN_ORCHESTRATOR_TASK_CANCEL: &str =
    "/api/webchat/v2/tasks/{task_id}/cancel";
pub const WEBUI_V2_ROUTE_SCHEDULES_LIST: &str = "webui.v2.schedules.list";
pub const WEBUI_V2_ROUTE_SCHEDULE_CREATE: &str = "webui.v2.schedules.create";
pub const WEBUI_V2_ROUTE_SCHEDULE_PREVIEW: &str = "webui.v2.schedules.preview";
pub const WEBUI_V2_ROUTE_SCHEDULE_UPDATE: &str = "webui.v2.schedules.update";
pub const WEBUI_V2_ROUTE_SCHEDULE_DELETE: &str = "webui.v2.schedules.delete";
pub const WEBUI_V2_ROUTE_SCHEDULE_RUN: &str = "webui.v2.schedules.run";
pub const WEBUI_V2_ROUTE_SCHEDULE_ROTATE_SECRET: &str = "webui.v2.schedules.rotate_secret";
pub const WEBUI_V2_ROUTE_SCHEDULE_SECRET: &str = "webui.v2.schedules.secret";
pub const WEBUI_V2_PATTERN_SCHEDULES: &str = "/api/webchat/v2/schedules";
pub const WEBUI_V2_PATTERN_SCHEDULE_PREVIEW: &str = "/api/webchat/v2/schedules/preview";
pub const WEBUI_V2_PATTERN_SCHEDULE: &str = "/api/webchat/v2/schedules/{schedule_id}";
pub const WEBUI_V2_PATTERN_SCHEDULE_RUN: &str = "/api/webchat/v2/schedules/{schedule_id}/run";
pub const WEBUI_V2_PATTERN_SCHEDULE_ROTATE_SECRET: &str =
    "/api/webchat/v2/schedules/{schedule_id}/rotate-secret";
pub const WEBUI_V2_PATTERN_SCHEDULE_SECRET: &str = "/api/webchat/v2/schedules/{schedule_id}/secret";
pub const WEBUI_V2_ROUTE_PROFILES_LIST: &str = "webui.v2.profiles.list";
pub const WEBUI_V2_ROUTE_PROFILE_CREATE: &str = "webui.v2.profiles.create";
pub const WEBUI_V2_ROUTE_PROFILE_GET: &str = "webui.v2.profiles.get";
pub const WEBUI_V2_ROUTE_PROFILE_UPDATE: &str = "webui.v2.profiles.update";
pub const WEBUI_V2_ROUTE_PROFILE_DELETE: &str = "webui.v2.profiles.delete";
pub const WEBUI_V2_ROUTE_PROFILE_THREAD_CREATE: &str = "webui.v2.profiles.thread_create";
pub const WEBUI_V2_ROUTE_PROFILES_IMPORT_HERMES: &str = "webui.v2.profiles.import_hermes";
pub const WEBUI_V2_PATTERN_PROFILES: &str = "/api/webchat/v2/profiles";
pub const WEBUI_V2_PATTERN_PROFILE: &str = "/api/webchat/v2/profiles/{profile_id}";
pub const WEBUI_V2_PATTERN_PROFILE_THREADS: &str = "/api/webchat/v2/profiles/{profile_id}/threads";
pub const WEBUI_V2_PATTERN_PROFILES_IMPORT_HERMES: &str = "/api/webchat/v2/profiles/import/hermes";

// Usage route (subscription usage windows for the Usage page).
pub const WEBUI_V2_ROUTE_USAGE_REPORT: &str = "webui.v2.usage.report";
pub const WEBUI_V2_PATTERN_USAGE_REPORT: &str = "/api/webchat/v2/usage";
pub const WEBUI_V2_ROUTE_USAGE_HISTORY: &str = "webui.v2.usage.history";
pub const WEBUI_V2_PATTERN_USAGE_HISTORY: &str = "/api/webchat/v2/usage/history";

// Memory page routes (local memory documents + Supermemory) and the hooks view.
pub const WEBUI_V2_ROUTE_MEMORY_TREE: &str = "webui.v2.memory.tree";
pub const WEBUI_V2_ROUTE_MEMORY_WRITE: &str = "webui.v2.memory.write";
pub const WEBUI_V2_ROUTE_MEMORY_DOCUMENT: &str = "webui.v2.memory.document";
pub const WEBUI_V2_ROUTE_MEMORY_ARCHIVE: &str = "webui.v2.memory.archive";
pub const WEBUI_V2_ROUTE_MEMORY_SEARCH: &str = "webui.v2.memory.search";
pub const WEBUI_V2_ROUTE_SUPERMEMORY_STATUS: &str = "webui.v2.memory.supermemory.status";
pub const WEBUI_V2_ROUTE_SUPERMEMORY_DOCUMENTS: &str = "webui.v2.memory.supermemory.documents";
pub const WEBUI_V2_ROUTE_SUPERMEMORY_SEARCH: &str = "webui.v2.memory.supermemory.search";
pub const WEBUI_V2_ROUTE_SUPERMEMORY_DELETE_DOCUMENT: &str =
    "webui.v2.memory.supermemory.delete_document";
pub const WEBUI_V2_ROUTE_SUPERMEMORY_FORGET: &str = "webui.v2.memory.supermemory.forget";
pub const WEBUI_V2_ROUTE_HOOKS_CATALOG: &str = "webui.v2.hooks.catalog";
pub const WEBUI_V2_PATTERN_MEMORY_DOCUMENTS: &str = "/api/webchat/v2/memory/documents";
pub const WEBUI_V2_PATTERN_MEMORY_DOCUMENT: &str = "/api/webchat/v2/memory/documents/content";
pub const WEBUI_V2_PATTERN_MEMORY_ARCHIVE: &str = "/api/webchat/v2/memory/documents/archive";
pub const WEBUI_V2_PATTERN_MEMORY_SEARCH: &str = "/api/webchat/v2/memory/search";
pub const WEBUI_V2_PATTERN_SUPERMEMORY: &str = "/api/webchat/v2/memory/supermemory";
pub const WEBUI_V2_PATTERN_SUPERMEMORY_DOCUMENTS: &str =
    "/api/webchat/v2/memory/supermemory/documents";
pub const WEBUI_V2_PATTERN_SUPERMEMORY_SEARCH: &str = "/api/webchat/v2/memory/supermemory/search";
pub const WEBUI_V2_PATTERN_SUPERMEMORY_DELETE_DOCUMENT: &str =
    "/api/webchat/v2/memory/supermemory/documents/delete";
pub const WEBUI_V2_PATTERN_SUPERMEMORY_FORGET: &str =
    "/api/webchat/v2/memory/supermemory/memories/forget";
pub const WEBUI_V2_PATTERN_HOOKS: &str = "/api/webchat/v2/hooks";

// Computer-use routes (a thread's session + latest frame; Stop / take over).
pub const WEBUI_V2_ROUTE_COMPUTER_USE_SESSION: &str = "webui.v2.computer_use.session";
pub const WEBUI_V2_ROUTE_COMPUTER_USE_CONTROL: &str = "webui.v2.computer_use.control";
pub const WEBUI_V2_PATTERN_COMPUTER_USE: &str = "/api/webchat/v2/threads/{thread_id}/computer-use";
// The live view: every session of the caller, and one window's picture.
pub const WEBUI_V2_ROUTE_COMPUTER_USE_STATE: &str = "webui.v2.computer_use.state";
pub const WEBUI_V2_PATTERN_COMPUTER_USE_STATE: &str = "/api/webchat/v2/computer-use";
pub const WEBUI_V2_ROUTE_COMPUTER_USE_FRAME: &str = "webui.v2.computer_use.frame";
pub const WEBUI_V2_PATTERN_COMPUTER_USE_FRAME: &str =
    "/api/webchat/v2/threads/{thread_id}/computer-use/frame";
// This host's computer-use readiness (engine, macOS permissions); operator only.
pub const WEBUI_V2_ROUTE_COMPUTER_USE_READINESS: &str = "webui.v2.computer_use.readiness";
pub const WEBUI_V2_PATTERN_COMPUTER_USE_READINESS: &str = "/api/webchat/v2/computer-use/readiness";
// Inbox and goal routes (v2 stage 5).
pub const WEBUI_V2_ROUTE_INBOX_LIST: &str = "webui.v2.inbox.list";
pub const WEBUI_V2_ROUTE_INBOX_MUTATE: &str = "webui.v2.inbox.mutate";
pub const WEBUI_V2_ROUTE_THREAD_GOAL_GET: &str = "webui.v2.goals.get";
pub const WEBUI_V2_ROUTE_THREAD_GOAL_SET: &str = "webui.v2.goals.set";
pub const WEBUI_V2_ROUTE_TASK_MODELS_GET: &str = "webui.v2.task_models.get";
pub const WEBUI_V2_ROUTE_TASK_MODELS_SET: &str = "webui.v2.task_models.set";
pub const WEBUI_V2_PATTERN_INBOX: &str = "/api/webchat/v2/inbox";
// History search across the caller's past chats.
pub const WEBUI_V2_ROUTE_HISTORY_SEARCH: &str = "webui.v2.history.search";
pub const WEBUI_V2_PATTERN_HISTORY_SEARCH: &str = "/api/webchat/v2/history/search";
pub const WEBUI_V2_PATTERN_INBOX_THREAD: &str = "/api/webchat/v2/inbox/{thread_id}";
pub const WEBUI_V2_PATTERN_THREAD_GOAL: &str = "/api/webchat/v2/threads/{thread_id}/goal";
pub const WEBUI_V2_PATTERN_TASK_MODELS: &str = "/api/webchat/v2/llm/task-models";
// Terminal session import (status, settings and "Import now").
pub const WEBUI_V2_ROUTE_SESSION_IMPORT_STATUS: &str = "webui.v2.session_import.status";
pub const WEBUI_V2_ROUTE_SESSION_IMPORT_RUN: &str = "webui.v2.session_import.run";
pub const WEBUI_V2_PATTERN_SESSION_IMPORT: &str = "/api/webchat/v2/session-imports";
pub const WEBUI_V2_ROUTE_SESSION_IMPORT_PROJECTS: &str = "webui.v2.session_import.projects";
pub const WEBUI_V2_PATTERN_SESSION_IMPORT_PROJECTS: &str =
    "/api/webchat/v2/session-imports/projects";
pub const WEBUI_V2_PATTERN_CREATE_THREAD: &str = "/api/webchat/v2/threads";
pub const WEBUI_V2_PATTERN_LIST_THREADS: &str = "/api/webchat/v2/threads";
pub const WEBUI_V2_PATTERN_LIST_NOTIFICATIONS: &str = "/api/webchat/v2/notifications";
pub const WEBUI_V2_PATTERN_MARK_NOTIFICATION_READ: &str =
    "/api/webchat/v2/notifications/{notification_id}/read";
pub const WEBUI_V2_PATTERN_MARK_ALL_NOTIFICATIONS_READ: &str =
    "/api/webchat/v2/notifications/read-all";
pub const WEBUI_V2_PATTERN_ARCHIVE_NOTIFICATION: &str =
    "/api/webchat/v2/notifications/{notification_id}/archive";
pub const WEBUI_V2_PATTERN_DELETE_THREAD: &str = "/api/webchat/v2/threads/{thread_id}";
pub const WEBUI_V2_PATTERN_GET_SESSION: &str = "/api/webchat/v2/session";
pub const WEBUI_V2_PATTERN_SESSION_CHANNEL_MESSAGE: &str =
    "/api/webchat/v2/channels/{extension_id}/messages";
pub const WEBUI_V2_PATTERN_GET_TIMELINE: &str = "/api/webchat/v2/threads/{thread_id}/timeline";
pub const WEBUI_V2_PATTERN_GET_RUN_ARTIFACT: &str =
    "/api/webchat/v2/threads/{thread_id}/runs/{run_id}/artifact";
pub const WEBUI_V2_PATTERN_GET_THREAD_ARTIFACT: &str =
    "/api/webchat/v2/threads/{thread_id}/artifact";
pub const WEBUI_V2_PATTERN_LOGS: &str = "/api/webchat/v2/logs";
pub const WEBUI_V2_PATTERN_GET_ATTACHMENT: &str =
    "/api/webchat/v2/threads/{thread_id}/messages/{message_id}/attachments/{attachment_id}";
pub const WEBUI_V2_PATTERN_STREAM_EVENTS: &str = "/api/webchat/v2/threads/{thread_id}/events";
pub const WEBUI_V2_PATTERN_LIST_COMMANDS: &str = "/api/webchat/v2/commands";
pub const WEBUI_V2_PATTERN_EXECUTE_COMMAND: &str = "/api/webchat/v2/threads/{thread_id}/commands";
pub const WEBUI_V2_PATTERN_LIST_AUTOMATIONS: &str = "/api/webchat/v2/automations";
pub const WEBUI_V2_PATTERN_RUN_AUTOMATION: &str = "/api/webchat/v2/automations/{automation_id}/run";
pub const WEBUI_V2_PATTERN_PAUSE_AUTOMATION: &str =
    "/api/webchat/v2/automations/{automation_id}/pause";
pub const WEBUI_V2_PATTERN_RESUME_AUTOMATION: &str =
    "/api/webchat/v2/automations/{automation_id}/resume";
// Intentional dual-method resource path: POST renames an automation and DELETE
// removes it. Keep the route ids separate so host policy/audit stays action-specific.
pub const WEBUI_V2_PATTERN_AUTOMATION_DETAIL: &str = "/api/webchat/v2/automations/{automation_id}";
pub const WEBUI_V2_PATTERN_SUGGESTIONS_LIST: &str = "/api/webchat/v2/suggestions";
pub const WEBUI_V2_PATTERN_SUGGESTIONS_GENERATE: &str = "/api/webchat/v2/suggestions/generate";
pub const WEBUI_V2_PATTERN_SUGGESTION_DETAIL: &str = "/api/webchat/v2/suggestions/{suggestion_id}";
pub const WEBUI_V2_PATTERN_SUGGESTION_START: &str =
    "/api/webchat/v2/suggestions/{suggestion_id}/start";
pub const WEBUI_V2_PATTERN_TRACE_CREDITS: &str = "/api/webchat/v2/traces/credit";
pub const WEBUI_V2_PATTERN_TRACE_ACCOUNT_TRACES: &str = "/api/webchat/v2/traces/account";
pub const WEBUI_V2_PATTERN_TRACE_HOLD_AUTHORIZE: &str =
    "/api/webchat/v2/traces/holds/{submission_id}/authorize";
pub const WEBUI_V2_PATTERN_TRACE_ACCOUNT_LOGIN_LINK: &str =
    "/api/webchat/v2/traces/account-login-link";
pub const WEBUI_V2_PATTERN_OUTBOUND_DELIVERY_TARGETS: &str = "/api/webchat/v2/outbound/targets";
pub const WEBUI_V2_PATTERN_NOTIFICATION_CHANNELS: &str =
    "/api/webchat/v2/outbound/notification-channels";
pub const WEBUI_V2_PATTERN_NOTIFICATION_SETUP_STATUS: &str =
    "/api/webchat/v2/channels/{extension_id}/notifications";
pub const WEBUI_V2_PATTERN_NOTIFICATION_SETUP_ENABLE: &str =
    "/api/webchat/v2/channels/{extension_id}/notifications/enable";
pub const WEBUI_V2_PATTERN_NOTIFICATION_SETUP_DISABLE: &str =
    "/api/webchat/v2/channels/{extension_id}/notifications/disable";
pub const WEBUI_V2_PATTERN_ADMIN_USERS: &str = "/api/webchat/v2/admin/users";
pub const WEBUI_V2_PATTERN_ADMIN_USER: &str = "/api/webchat/v2/admin/users/{user_id}";
pub const WEBUI_V2_PATTERN_ADMIN_USER_STATUS: &str = "/api/webchat/v2/admin/users/{user_id}/status";
pub const WEBUI_V2_PATTERN_ADMIN_USER_ROLE: &str = "/api/webchat/v2/admin/users/{user_id}/role";
pub const WEBUI_V2_PATTERN_ADMIN_USER_SECRETS: &str =
    "/api/webchat/v2/admin/users/{user_id}/secrets";
pub const WEBUI_V2_PATTERN_ADMIN_USER_SECRET: &str =
    "/api/webchat/v2/admin/users/{user_id}/secrets/{handle}";
pub const WEBUI_V2_PATTERN_ADMIN_THREAD_SCRAPE_THREADS: &str =
    "/api/webchat/v2/admin/users/{user_id}/thread-scrape/threads";
pub const WEBUI_V2_PATTERN_ADMIN_THREAD_SCRAPE_ARTIFACT: &str =
    "/api/webchat/v2/admin/users/{user_id}/thread-scrape/threads/{thread_id}/artifact";
pub const WEBUI_V2_PATTERN_ADMIN_THREAD_SCRAPE_RUN_ARTIFACT: &str = "/api/webchat/v2/admin/users/{user_id}/thread-scrape/threads/{thread_id}/runs/{run_id}/artifact";
pub const WEBUI_V2_PATTERN_LIST_EXTENSIONS: &str = "/api/webchat/v2/extensions";
pub const WEBUI_V2_PATTERN_LIST_EXTENSION_REGISTRY: &str = "/api/webchat/v2/extensions/registry";
pub const WEBUI_V2_PATTERN_INSTALL_EXTENSION: &str = "/api/webchat/v2/extensions/install";
pub const WEBUI_V2_PATTERN_IRONHUB_DELIVER_INSTALL: &str = "/api/webchat/v2/ironhub/install";
pub const WEBUI_V2_PATTERN_REGISTER_HOSTED_MCP_EXTENSION: &str =
    "/api/webchat/v2/extensions/register-hosted-mcp";
pub const WEBUI_V2_PATTERN_IMPORT_EXTENSION: &str = "/api/webchat/v2/extensions/import";
pub const WEBUI_V2_PATTERN_REMOVE_EXTENSION: &str =
    "/api/webchat/v2/extensions/{package_id}/remove";
pub const WEBUI_V2_PATTERN_SETUP_EXTENSION: &str = "/api/webchat/v2/extensions/{package_id}/setup";
pub const WEBUI_V2_PATTERN_LIST_SKILLS: &str = "/api/webchat/v2/skills";
pub const WEBUI_V2_PATTERN_SEARCH_SKILLS: &str = "/api/webchat/v2/skills/search";
pub const WEBUI_V2_PATTERN_INSTALL_SKILL: &str = "/api/webchat/v2/skills/install";
pub const WEBUI_V2_PATTERN_SKILL_DETAIL: &str = "/api/webchat/v2/skills/{name}";
pub const WEBUI_V2_PATTERN_SET_SKILL_AUTO_ACTIVATE: &str =
    "/api/webchat/v2/skills/{name}/auto-activate";
pub const WEBUI_V2_PATTERN_SET_AUTO_ACTIVATE_LEARNED: &str =
    "/api/webchat/v2/skills/auto-activate-learned";
pub const WEBUI_V2_PATTERN_SETTINGS_TOOLS: &str = "/api/webchat/v2/settings/tools";
pub const WEBUI_V2_PATTERN_SETTINGS_TOOL_PERMISSION: &str =
    "/api/webchat/v2/settings/tools/{capability_id}";
pub const WEBUI_V2_PATTERN_GET_LLM_CONFIG: &str = "/api/webchat/v2/llm/providers";
pub const WEBUI_V2_PATTERN_UPSERT_LLM_PROVIDER: &str = "/api/webchat/v2/llm/providers";
pub const WEBUI_V2_PATTERN_DELETE_LLM_PROVIDER: &str =
    "/api/webchat/v2/llm/providers/{provider_id}/delete";
pub const WEBUI_V2_PATTERN_SET_ACTIVE_LLM: &str = "/api/webchat/v2/llm/active";
pub const WEBUI_V2_PATTERN_USER_MODEL_CATALOG: &str = "/api/webchat/v2/llm/models";
pub const WEBUI_V2_PATTERN_USER_MODEL_POLICY: &str = "/api/webchat/v2/llm/model-policy";
pub const WEBUI_V2_PATTERN_USER_MODEL_PREFERENCE: &str = "/api/webchat/v2/llm/model-preference";
pub const WEBUI_V2_PATTERN_TEST_LLM_CONNECTION: &str = "/api/webchat/v2/llm/test-connection";
pub const WEBUI_V2_PATTERN_LIST_LLM_MODELS: &str = "/api/webchat/v2/llm/list-models";
pub const WEBUI_V2_PATTERN_START_NEARAI_LOGIN: &str = "/api/webchat/v2/llm/nearai/login";
pub const WEBUI_V2_PATTERN_COMPLETE_NEARAI_WALLET_LOGIN: &str = "/api/webchat/v2/llm/nearai/wallet";
pub const WEBUI_V2_PATTERN_START_CODEX_LOGIN: &str = "/api/webchat/v2/llm/codex/login";
pub const WEBUI_V2_PATTERN_OPERATOR_SETUP: &str = "/api/webchat/v2/operator/setup";
pub const WEBUI_V2_PATTERN_OPERATOR_CONFIG: &str = "/api/webchat/v2/operator/config";
pub const WEBUI_V2_PATTERN_OPERATOR_CONFIG_KEY: &str = "/api/webchat/v2/operator/config/{key}";
pub const WEBUI_V2_PATTERN_OPERATOR_CONFIG_VALIDATE: &str =
    "/api/webchat/v2/operator/config/validate";
pub const WEBUI_V2_PATTERN_OPERATOR_DIAGNOSTICS: &str = "/api/webchat/v2/operator/diagnostics";
pub const WEBUI_V2_PATTERN_OPERATOR_STATUS: &str = "/api/webchat/v2/operator/status";
pub const WEBUI_V2_PATTERN_OPERATOR_LOGS: &str = "/api/webchat/v2/operator/logs";
pub const WEBUI_V2_PATTERN_OPERATOR_SERVICE_LIFECYCLE: &str = "/api/webchat/v2/operator/service";
pub const WEBUI_V2_PATTERN_INSPECTOR_SNAPSHOT: &str =
    "/api/webchat/v2/operator/inspector/threads/{thread_id}/runs/{run_id}";
pub const WEBUI_V2_PATTERN_INSPECTOR_PROMPT: &str =
    "/api/webchat/v2/operator/inspector/threads/{thread_id}/runs/{run_id}/prompt";
pub const WEBUI_V2_PATTERN_INSPECTOR_TOOL: &str =
    "/api/webchat/v2/operator/inspector/threads/{thread_id}/runs/{run_id}/tools/{activity_id}";
pub const WEBUI_V2_PATTERN_INSPECTOR_UPDATES: &str =
    "/api/webchat/v2/operator/inspector/threads/{thread_id}/runs/{run_id}/events";
pub const WEBUI_V2_PATTERN_OPERATOR_EXTENSION_CONFIGURATION: &str =
    "/api/webchat/v2/operator/extension-configuration";
pub const WEBUI_V2_PATTERN_OPERATOR_EXTENSION_CONFIGURATION_GROUP: &str =
    "/api/webchat/v2/operator/extension-configuration/{group_id}";
pub const WEBUI_V2_PATTERN_LIST_PROJECT_FILES: &str = "/api/webchat/v2/threads/{thread_id}/files";
pub const WEBUI_V2_PATTERN_STAT_PROJECT_FILE: &str =
    "/api/webchat/v2/threads/{thread_id}/files/stat";
pub const WEBUI_V2_PATTERN_READ_PROJECT_FILE: &str =
    "/api/webchat/v2/threads/{thread_id}/files/content";
pub const WEBUI_V2_PATTERN_INDEX_PROJECT_FILES: &str =
    "/api/webchat/v2/threads/{thread_id}/files/index";
pub const WEBUI_V2_PATTERN_LIST_FS_MOUNTS: &str = "/api/webchat/v2/fs/mounts";
pub const WEBUI_V2_PATTERN_BROWSE_FS_DIR: &str = "/api/webchat/v2/fs/list";
pub const WEBUI_V2_PATTERN_STAT_FS_PATH: &str = "/api/webchat/v2/fs/stat";
pub const WEBUI_V2_PATTERN_READ_FS_FILE: &str = "/api/webchat/v2/fs/content";
pub const WEBUI_V2_PATTERN_LIST_PROJECTS: &str = "/api/webchat/v2/projects";
pub const WEBUI_V2_PATTERN_CREATE_PROJECT: &str = "/api/webchat/v2/projects";
pub const WEBUI_V2_PATTERN_PROJECT_DETAIL: &str = "/api/webchat/v2/projects/{project_id}";
pub const WEBUI_V2_PATTERN_PROJECT_MEMBERS: &str = "/api/webchat/v2/projects/{project_id}/members";
pub const WEBUI_V2_PATTERN_PROJECT_MEMBER_DETAIL: &str =
    "/api/webchat/v2/projects/{project_id}/members/{user_id}";

/// Return the default production [`IngressRouteDescriptor`] set for the
/// WebChat v2 beta route surface.
///
/// Host composition calls this once at startup, validates the descriptors
/// against its own mount table, and refuses to bind any route whose policy
/// the host cannot enforce. QA-only regression artifact exports are omitted
/// unless composition explicitly opts in through
/// [`webui_v2_routes_with_artifact_flags`].
pub fn webui_v2_routes() -> Vec<IngressRouteDescriptor> {
    webui_v2_routes_with_artifact_flags(false, false)
}

/// Combined artifact-surface policy: QA-only caller-owned exports plus the
/// independently gated admin cross-user thread scraping.
///
/// The two flags are deliberately separate: enabling QA self-export must
/// never silently mount tenant-wide admin transcript access. The same two
/// captured deployment flags must feed this table and
/// [`crate::webui_v2::WebUiV2State`] so host middleware never advertises a
/// route the inner router does not mount.
pub fn webui_v2_routes_with_artifact_flags(
    regression_artifact_export_enabled: bool,
    admin_thread_scrape_enabled: bool,
) -> Vec<IngressRouteDescriptor> {
    let mut routes = vec![
        get_session_descriptor(),
        create_thread_descriptor(),
        delete_thread_descriptor(),
        session_channel_message_descriptor(),
        list_threads_descriptor(),
        list_notifications_descriptor(),
        mark_notification_read_descriptor(),
        mark_all_notifications_read_descriptor(),
        archive_notification_descriptor(),
        get_timeline_descriptor(),
        logs_descriptor(),
        get_attachment_descriptor(),
        stream_events_descriptor(),
        cancel_run_descriptor(),
        orchestrator_thread_tasks_descriptor(),
        orchestrator_thread_queue_descriptor(),
        orchestrator_queue_mutate_descriptor(),
        orchestrator_thread_lineage_descriptor(),
        orchestrator_thread_runs_descriptor(),
        orchestrator_thread_telemetry_descriptor(),
        orchestrator_thread_context_descriptor(),
        orchestrator_thread_fork_descriptor(),
        orchestrator_thread_route_descriptor(),
        orchestrator_thread_route_set_descriptor(),
        orchestrator_providers_descriptor(),
        orchestrator_thread_merge_back_descriptor(),
        orchestrator_thread_worktree_descriptor(),
        thread_git_descriptor(),
        thread_git_change_descriptor(),
        thread_console_descriptor(),
        thread_console_control_descriptor(),
        orchestrator_thread_action_descriptor(),
        orchestrator_task_descriptor(),
        orchestrator_task_cancel_descriptor(),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SCHEDULES_LIST,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_SCHEDULES,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SCHEDULE_CREATE,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_SCHEDULES,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SCHEDULE_PREVIEW,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_SCHEDULE_PREVIEW,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SCHEDULE_UPDATE,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_SCHEDULE,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SCHEDULE_DELETE,
            NetworkMethod::Delete,
            WEBUI_V2_PATTERN_SCHEDULE,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SCHEDULE_RUN,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_SCHEDULE_RUN,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SCHEDULE_ROTATE_SECRET,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_SCHEDULE_ROTATE_SECRET,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SCHEDULE_SECRET,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_SCHEDULE_SECRET,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_PROFILES_LIST,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_PROFILES,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_PROFILE_CREATE,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_PROFILES,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_PROFILE_GET,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_PROFILE,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_PROFILE_UPDATE,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_PROFILE,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_PROFILE_DELETE,
            NetworkMethod::Delete,
            WEBUI_V2_PATTERN_PROFILE,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_PROFILE_THREAD_CREATE,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_PROFILE_THREADS,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_PROFILES_IMPORT_HERMES,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_PROFILES_IMPORT_HERMES,
        ),
        usage_report_descriptor(),
        usage_history_descriptor(),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_MEMORY_TREE,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_MEMORY_DOCUMENTS,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_MEMORY_WRITE,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_MEMORY_DOCUMENTS,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_MEMORY_DOCUMENT,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_MEMORY_DOCUMENT,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_MEMORY_ARCHIVE,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_MEMORY_ARCHIVE,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_MEMORY_SEARCH,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_MEMORY_SEARCH,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_SUPERMEMORY_STATUS,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_SUPERMEMORY,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_SUPERMEMORY_DOCUMENTS,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_SUPERMEMORY_DOCUMENTS,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_SUPERMEMORY_SEARCH,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_SUPERMEMORY_SEARCH,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_SUPERMEMORY_DELETE_DOCUMENT,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_SUPERMEMORY_DELETE_DOCUMENT,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_SUPERMEMORY_FORGET,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_SUPERMEMORY_FORGET,
        ),
        memory_route_descriptor(
            WEBUI_V2_ROUTE_HOOKS_CATALOG,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_HOOKS,
        ),
        computer_use_session_descriptor(),
        computer_use_control_descriptor(),
        computer_use_live_descriptor(
            WEBUI_V2_ROUTE_COMPUTER_USE_STATE,
            WEBUI_V2_PATTERN_COMPUTER_USE_STATE,
        ),
        computer_use_live_descriptor(
            WEBUI_V2_ROUTE_COMPUTER_USE_FRAME,
            WEBUI_V2_PATTERN_COMPUTER_USE_FRAME,
        ),
        computer_use_readiness_descriptor(),
        // Same read/mutation policy shape as the profile routes.
        profile_route_descriptor(
            WEBUI_V2_ROUTE_INBOX_LIST,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_INBOX,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_INBOX_MUTATE,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_INBOX_THREAD,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SESSION_IMPORT_STATUS,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_SESSION_IMPORT,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SESSION_IMPORT_RUN,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_SESSION_IMPORT,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_SESSION_IMPORT_PROJECTS,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_SESSION_IMPORT_PROJECTS,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_HISTORY_SEARCH,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_HISTORY_SEARCH,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_THREAD_GOAL_GET,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_THREAD_GOAL,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_THREAD_GOAL_SET,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_THREAD_GOAL,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_TASK_MODELS_GET,
            NetworkMethod::Get,
            WEBUI_V2_PATTERN_TASK_MODELS,
        ),
        profile_route_descriptor(
            WEBUI_V2_ROUTE_TASK_MODELS_SET,
            NetworkMethod::Post,
            WEBUI_V2_PATTERN_TASK_MODELS,
        ),
        resolve_gate_descriptor(),
        retry_run_descriptor(),
        list_commands_descriptor(),
        execute_command_descriptor(),
        list_automations_descriptor(),
        pause_automation_descriptor(),
        run_automation_descriptor(),
        resume_automation_descriptor(),
        rename_automation_descriptor(),
        delete_automation_descriptor(),
        suggestions_list_descriptor(),
        suggestions_generate_descriptor(),
        suggestion_start_descriptor(),
        suggestion_dismiss_descriptor(),
        trace_credits_descriptor(),
        trace_account_traces_descriptor(),
        trace_account_login_link_descriptor(),
        authorize_trace_hold_descriptor(),
        list_outbound_delivery_targets_descriptor(),
        get_notification_channels_descriptor(),
        set_notification_channels_descriptor(),
        notification_setup_status_descriptor(),
        notification_setup_enable_descriptor(),
        notification_setup_disable_descriptor(),
        list_extensions_descriptor(),
        list_extension_registry_descriptor(),
        install_extension_descriptor(),
        ironhub_deliver_install_descriptor(),
        register_hosted_mcp_extension_descriptor(),
        import_extension_descriptor(),
        remove_extension_descriptor(),
        get_extension_setup_descriptor(),
        setup_extension_descriptor(),
        list_skills_descriptor(),
        search_skills_descriptor(),
        install_skill_descriptor(),
        get_skill_descriptor(),
        update_skill_descriptor(),
        remove_skill_descriptor(),
        set_skill_auto_activate_descriptor(),
        set_auto_activate_learned_descriptor(),
        list_settings_tools_descriptor(),
        set_settings_tools_auto_approve_descriptor(),
        set_settings_tool_permission_descriptor(),
        get_llm_config_descriptor(),
        upsert_llm_provider_descriptor(),
        delete_llm_provider_descriptor(),
        set_active_llm_descriptor(),
        get_user_model_catalog_descriptor(),
        get_user_model_preference_descriptor(),
        set_user_model_preference_descriptor(),
        set_user_model_policy_descriptor(),
        test_llm_connection_descriptor(),
        list_llm_models_descriptor(),
        start_nearai_login_descriptor(),
        complete_nearai_wallet_login_descriptor(),
        start_codex_login_descriptor(),
        operator_get_setup_descriptor(),
        operator_run_setup_descriptor(),
        operator_list_config_descriptor(),
        operator_get_config_key_descriptor(),
        operator_set_config_key_descriptor(),
        operator_validate_config_descriptor(),
        operator_diagnostics_descriptor(),
        operator_status_descriptor(),
        operator_logs_descriptor(),
        operator_service_lifecycle_descriptor(),
        inspector_snapshot_descriptor(),
        inspector_prompt_descriptor(),
        inspector_tool_descriptor(),
        inspector_updates_descriptor(),
        operator_list_extension_configuration_descriptor(),
        operator_replace_extension_configuration_descriptor(),
        list_project_files_descriptor(),
        stat_project_file_descriptor(),
        read_project_file_descriptor(),
        index_project_files_descriptor(),
        list_fs_mounts_descriptor(),
        browse_fs_dir_descriptor(),
        stat_fs_path_descriptor(),
        read_fs_file_descriptor(),
        list_projects_descriptor(),
        create_project_descriptor(),
        get_project_descriptor(),
        update_project_descriptor(),
        delete_project_descriptor(),
        list_project_members_descriptor(),
        add_project_member_descriptor(),
        update_project_member_descriptor(),
        remove_project_member_descriptor(),
        admin_list_users_descriptor(),
        admin_create_user_descriptor(),
        admin_get_user_descriptor(),
        admin_update_user_descriptor(),
        admin_delete_user_descriptor(),
        admin_set_user_status_descriptor(),
        admin_set_user_role_descriptor(),
        admin_list_user_secrets_descriptor(),
        admin_put_user_secret_descriptor(),
        admin_delete_user_secret_descriptor(),
    ];
    if regression_artifact_export_enabled {
        routes.push(get_run_artifact_descriptor());
        routes.push(get_thread_artifact_descriptor());
    }
    if admin_thread_scrape_enabled {
        routes.push(admin_list_thread_scrape_threads_descriptor());
        routes.push(admin_get_thread_scrape_artifact_descriptor());
        routes.push(admin_get_thread_scrape_run_artifact_descriptor());
    }
    routes
}

/// Returns whether a route id belongs to any operator-wide WebUI config surface.
pub fn is_webui_v2_operator_webui_config_route_id(route_id: &str) -> bool {
    matches!(
        route_id,
        WEBUI_V2_ROUTE_GET_LLM_CONFIG
            | WEBUI_V2_ROUTE_UPSERT_LLM_PROVIDER
            | WEBUI_V2_ROUTE_DELETE_LLM_PROVIDER
            | WEBUI_V2_ROUTE_SET_ACTIVE_LLM
            | WEBUI_V2_ROUTE_SET_USER_MODEL_POLICY
            | WEBUI_V2_ROUTE_TEST_LLM_CONNECTION
            | WEBUI_V2_ROUTE_LIST_LLM_MODELS
            | WEBUI_V2_ROUTE_START_NEARAI_LOGIN
            | WEBUI_V2_ROUTE_COMPLETE_NEARAI_WALLET_LOGIN
            | WEBUI_V2_ROUTE_START_CODEX_LOGIN
    ) || matches!(
        route_id,
        WEBUI_V2_ROUTE_OPERATOR_GET_SETUP
            | WEBUI_V2_ROUTE_OPERATOR_RUN_SETUP
            | WEBUI_V2_ROUTE_OPERATOR_LIST_CONFIG
            | WEBUI_V2_ROUTE_OPERATOR_GET_CONFIG_KEY
            | WEBUI_V2_ROUTE_OPERATOR_SET_CONFIG_KEY
            | WEBUI_V2_ROUTE_OPERATOR_VALIDATE_CONFIG
            | WEBUI_V2_ROUTE_OPERATOR_DIAGNOSTICS
            | WEBUI_V2_ROUTE_OPERATOR_STATUS
            | WEBUI_V2_ROUTE_OPERATOR_LOGS
            | WEBUI_V2_ROUTE_OPERATOR_SERVICE_LIFECYCLE
            | WEBUI_V2_ROUTE_INSPECTOR_SNAPSHOT
            | WEBUI_V2_ROUTE_INSPECTOR_PROMPT
            | WEBUI_V2_ROUTE_INSPECTOR_TOOL
            | WEBUI_V2_ROUTE_INSPECTOR_UPDATES
            | WEBUI_V2_ROUTE_OPERATOR_LIST_EXTENSION_CONFIGURATION
            | WEBUI_V2_ROUTE_OPERATOR_REPLACE_EXTENSION_CONFIGURATION
            // The zip-upload import route is admin-only (#5499): classifying
            // it here makes composition strip it from deployments without an
            // operator surface and pre-gate non-operator callers before the
            // request body is buffered, instead of relying on the
            // handler-level capability check alone.
            | WEBUI_V2_ROUTE_IMPORT_EXTENSION
            // Host git and a host shell on the backend's machine for a
            // remote client (ZLO remote environments): operator only.
            | WEBUI_V2_ROUTE_THREAD_GIT
            | WEBUI_V2_ROUTE_THREAD_GIT_CHANGE
            | WEBUI_V2_ROUTE_THREAD_CONSOLE
            | WEBUI_V2_ROUTE_THREAD_CONSOLE_CONTROL
    )
}

fn get_session_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_GET_SESSION,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_GET_SESSION,
    )
}

fn create_thread_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_CREATE_THREAD,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_CREATE_THREAD,
        16,
    )
}

fn session_channel_message_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_SESSION_CHANNEL_MESSAGE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SESSION_CHANNEL_MESSAGE,
        mutation_policy(
            // Message bodies carry user text plus optional base64-encoded inline
            // attachments. 14 MiB matches the gateway-wide body budget and covers
            // base64 of the 10 MiB decoded per-message attachment cap (the service
            // enforces the 5 MiB-per-file / 10 MiB-total decoded budgets).
            body_limit_kib(14 * 1024),
            mutation_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::TurnCoordinator,
        ),
    )
}

fn admin_list_users_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ADMIN_LIST_USERS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ADMIN_USERS,
    )
}

fn admin_create_user_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ADMIN_CREATE_USER,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ADMIN_USERS,
        16,
    )
}

fn admin_get_user_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ADMIN_GET_USER,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ADMIN_USER,
    )
}

fn admin_update_user_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ADMIN_UPDATE_USER,
        NetworkMethod::Patch,
        WEBUI_V2_PATTERN_ADMIN_USER,
        16,
    )
}

fn admin_delete_user_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_ADMIN_DELETE_USER,
        NetworkMethod::Delete,
        WEBUI_V2_PATTERN_ADMIN_USER,
    )
}

fn admin_set_user_status_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ADMIN_SET_USER_STATUS,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ADMIN_USER_STATUS,
        4,
    )
}

fn admin_set_user_role_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ADMIN_SET_USER_ROLE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ADMIN_USER_ROLE,
        4,
    )
}

fn admin_list_user_secrets_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ADMIN_LIST_USER_SECRETS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ADMIN_USER_SECRETS,
    )
}

fn admin_put_user_secret_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ADMIN_PUT_USER_SECRET,
        NetworkMethod::Put,
        WEBUI_V2_PATTERN_ADMIN_USER_SECRET,
        16,
    )
}

fn admin_delete_user_secret_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_ADMIN_DELETE_USER_SECRET,
        NetworkMethod::Delete,
        WEBUI_V2_PATTERN_ADMIN_USER_SECRET,
    )
}

fn admin_list_thread_scrape_threads_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ADMIN_LIST_THREAD_SCRAPE_THREADS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ADMIN_THREAD_SCRAPE_THREADS,
    )
}

fn admin_get_thread_scrape_artifact_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_ADMIN_GET_THREAD_SCRAPE_ARTIFACT,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ADMIN_THREAD_SCRAPE_ARTIFACT,
        read_policy(
            thread_artifact_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
            StreamingMode::None,
        ),
    )
}

fn admin_get_thread_scrape_run_artifact_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ADMIN_GET_THREAD_SCRAPE_RUN_ARTIFACT,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ADMIN_THREAD_SCRAPE_RUN_ARTIFACT,
    )
}

fn delete_thread_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_DELETE_THREAD,
        NetworkMethod::Delete,
        WEBUI_V2_PATTERN_DELETE_THREAD,
    )
}

fn list_project_files_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_LIST_PROJECT_FILES,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_PROJECT_FILES,
    )
}

/// One walk of up to a few hundred listings: a tighter per-caller rate than
/// a single listing.
fn index_project_files_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_INDEX_PROJECT_FILES,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_INDEX_PROJECT_FILES,
        read_policy(
            rate_limit_per_caller(30, 60),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
            StreamingMode::None,
        ),
    )
}

fn stat_project_file_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_STAT_PROJECT_FILE,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_STAT_PROJECT_FILE,
    )
}

fn read_project_file_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_READ_PROJECT_FILE,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_READ_PROJECT_FILE,
    )
}

fn list_fs_mounts_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_LIST_FS_MOUNTS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_FS_MOUNTS,
    )
}

fn list_projects_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_LIST_PROJECTS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_PROJECTS,
    )
}

fn browse_fs_dir_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_BROWSE_FS_DIR,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_BROWSE_FS_DIR,
    )
}

fn create_project_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_CREATE_PROJECT,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_CREATE_PROJECT,
        16,
    )
}

fn get_project_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_GET_PROJECT,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_PROJECT_DETAIL,
    )
}

fn stat_fs_path_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_STAT_FS_PATH,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_STAT_FS_PATH,
    )
}

fn update_project_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_UPDATE_PROJECT,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_PROJECT_DETAIL,
        16,
    )
}

fn delete_project_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_DELETE_PROJECT,
        NetworkMethod::Delete,
        WEBUI_V2_PATTERN_PROJECT_DETAIL,
    )
}

fn list_project_members_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_LIST_PROJECT_MEMBERS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_PROJECT_MEMBERS,
    )
}

fn read_fs_file_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_READ_FS_FILE,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_READ_FS_FILE,
    )
}

fn add_project_member_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ADD_PROJECT_MEMBER,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_PROJECT_MEMBERS,
        16,
    )
}

fn update_project_member_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_UPDATE_PROJECT_MEMBER,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_PROJECT_MEMBER_DETAIL,
        16,
    )
}

fn remove_project_member_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_REMOVE_PROJECT_MEMBER,
        NetworkMethod::Delete,
        WEBUI_V2_PATTERN_PROJECT_MEMBER_DETAIL,
    )
}

fn get_timeline_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_GET_TIMELINE,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_GET_TIMELINE,
    )
}

fn get_run_artifact_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_GET_RUN_ARTIFACT,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_GET_RUN_ARTIFACT,
    )
}

fn get_thread_artifact_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_GET_THREAD_ARTIFACT,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_GET_THREAD_ARTIFACT,
        read_policy(
            thread_artifact_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProjectionOnly,
            StreamingMode::None,
        ),
    )
}

fn get_attachment_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_GET_ATTACHMENT,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_GET_ATTACHMENT,
        read_policy(
            read_rate_limit(),
            AuditTraceClass::UserAction,
            // Reads workspace-backed attachment bytes through the product
            // service — more than a projection read, so the effect path is
            // ProductSurface to keep the fail-closed ingress boundary honest.
            AllowedEffectPath::ProductSurface,
            StreamingMode::None,
        ),
    )
}

fn stream_events_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_STREAM_EVENTS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_STREAM_EVENTS,
        read_policy(
            stream_rate_limit(),
            AuditTraceClass::StreamingSubscription,
            AllowedEffectPath::ProjectionOnly,
            StreamingMode::Sse,
        ),
    )
}

fn list_threads_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_LIST_THREADS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_THREADS,
    )
}

fn list_notifications_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_LIST_NOTIFICATIONS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_NOTIFICATIONS,
    )
}

fn mark_notification_read_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_MARK_NOTIFICATION_READ,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_MARK_NOTIFICATION_READ,
    )
}

fn mark_all_notifications_read_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_MARK_ALL_NOTIFICATIONS_READ,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_MARK_ALL_NOTIFICATIONS_READ,
    )
}

fn archive_notification_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_ARCHIVE_NOTIFICATION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ARCHIVE_NOTIFICATION,
    )
}

fn list_commands_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_LIST_COMMANDS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_COMMANDS,
    )
}

fn execute_command_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_EXECUTE_COMMAND,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_EXECUTE_COMMAND,
        4,
    )
}

fn list_automations_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_LIST_AUTOMATIONS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_AUTOMATIONS,
    )
}

fn run_automation_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_RUN_AUTOMATION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_RUN_AUTOMATION,
        mutation_policy(
            BodyLimitPolicy::NoBody,
            rate_limit_per_caller(12, 60),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
        ),
    )
}

fn pause_automation_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_PAUSE_AUTOMATION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_PAUSE_AUTOMATION,
    )
}

fn resume_automation_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_RESUME_AUTOMATION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_RESUME_AUTOMATION,
    )
}

fn rename_automation_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_RENAME_AUTOMATION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_AUTOMATION_DETAIL,
        4,
    )
}

fn delete_automation_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_DELETE_AUTOMATION,
        NetworkMethod::Delete,
        WEBUI_V2_PATTERN_AUTOMATION_DETAIL,
    )
}

fn suggestions_list_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_SUGGESTIONS_LIST,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_SUGGESTIONS_LIST,
    )
}

fn suggestions_generate_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_SUGGESTIONS_GENERATE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SUGGESTIONS_GENERATE,
        mutation_policy(
            body_limit_kib(4),
            // Each accepted request can launch a provider-backed agent run.
            rate_limit_per_caller(10, 60),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
        ),
    )
}

fn suggestion_start_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_SUGGESTION_START,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SUGGESTION_START,
    )
}

fn suggestion_dismiss_descriptor() -> IngressRouteDescriptor {
    bodyless_mutation_route(
        WEBUI_V2_ROUTE_SUGGESTION_DISMISS,
        NetworkMethod::Delete,
        WEBUI_V2_PATTERN_SUGGESTION_DETAIL,
    )
}

fn trace_credits_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_TRACE_CREDITS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_TRACE_CREDITS,
    )
}

fn trace_account_traces_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_TRACE_ACCOUNT_TRACES,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_TRACE_ACCOUNT_TRACES,
    )
}

fn authorize_trace_hold_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_TRACE_HOLD_AUTHORIZE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_TRACE_HOLD_AUTHORIZE,
        mutation_policy(
            // The submission id is in the path; no request body.
            BodyLimitPolicy::NoBody,
            mutation_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
        ),
    )
}

fn trace_account_login_link_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_TRACE_ACCOUNT_LOGIN_LINK,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_TRACE_ACCOUNT_LOGIN_LINK,
        mutation_policy(
            // Caller-scoped mint; no request body.
            BodyLimitPolicy::NoBody,
            // Deliberately tighter than the standard mutation limit: each
            // call mints a one-time account-access credential server-side.
            rate_limit_per_caller(10, 60),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
        ),
    )
}

fn list_outbound_delivery_targets_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_LIST_OUTBOUND_DELIVERY_TARGETS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_OUTBOUND_DELIVERY_TARGETS,
    )
}

fn get_notification_channels_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_GET_NOTIFICATION_CHANNELS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_NOTIFICATION_CHANNELS,
    )
}

fn set_notification_channels_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SET_NOTIFICATION_CHANNELS,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_NOTIFICATION_CHANNELS,
        8,
    )
}

fn orchestrator_thread_tasks_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_TASKS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_TASKS,
    )
}

fn orchestrator_thread_telemetry_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_TELEMETRY,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_TELEMETRY,
    )
}

fn orchestrator_thread_context_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_CONTEXT,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_CONTEXT,
    )
}

fn orchestrator_thread_queue_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_QUEUE,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_QUEUE,
    )
}

fn orchestrator_queue_mutate_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_QUEUE_MUTATE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ORCHESTRATOR_QUEUE_MUTATE,
        64,
    )
}

fn orchestrator_thread_lineage_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_LINEAGE,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_LINEAGE,
    )
}

fn orchestrator_thread_runs_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_RUNS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_RUNS,
    )
}

fn orchestrator_thread_route_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_ROUTE,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_ROUTE,
    )
}

fn orchestrator_providers_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_PROVIDERS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ORCHESTRATOR_PROVIDERS,
    )
}

fn orchestrator_thread_route_set_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_ROUTE_SET,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_ROUTE_SET,
        16,
    )
}

fn orchestrator_thread_fork_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_FORK,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_FORK,
        64,
    )
}

fn orchestrator_thread_merge_back_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_MERGE_BACK,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_MERGE_BACK,
        64,
    )
}

fn orchestrator_thread_worktree_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_WORKTREE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_WORKTREE,
        16,
    )
}

fn thread_git_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_THREAD_GIT,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_THREAD_GIT,
        read_policy(
            rate_limit_per_caller(240, 60),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
            StreamingMode::None,
        ),
    )
}

fn thread_git_change_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_THREAD_GIT_CHANGE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_THREAD_GIT,
        256,
    )
}

/// Polled while a command runs, so its read budget is wider.
fn thread_console_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_THREAD_CONSOLE,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_THREAD_CONSOLE,
        read_policy(
            rate_limit_per_caller(600, 60),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
            StreamingMode::None,
        ),
    )
}

fn thread_console_control_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_THREAD_CONSOLE_CONTROL,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_THREAD_CONSOLE,
        16,
    )
}

fn orchestrator_thread_action_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_THREAD_ACTION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ORCHESTRATOR_THREAD_ACTION,
        16,
    )
}

fn orchestrator_task_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_TASK,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_ORCHESTRATOR_TASK,
    )
}

fn orchestrator_task_cancel_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_ORCHESTRATOR_TASK_CANCEL,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_ORCHESTRATOR_TASK_CANCEL,
        64,
    )
}

/// Profile and schedule routes: reads on the read policy; writes carry
/// SOUL.md (at most 24 KiB) or a schedule prompt (at most 32 KiB), escaped as
/// JSON, so 64 KiB bodies.
fn profile_route_descriptor(
    route_id: &'static str,
    method: NetworkMethod,
    pattern: &'static str,
) -> IngressRouteDescriptor {
    let policy = match method {
        NetworkMethod::Get => read_policy(
            read_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
            StreamingMode::None,
        ),
        _ => mutation_policy(
            body_limit_kib(64),
            mutation_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
        ),
    };
    descriptor(route_id, method, pattern, policy)
}

/// Memory page and hooks routes. Reads use the standard read budget; the
/// document save carries a whole document, so its body cap sits just above
/// `MAX_MEMORY_DOCUMENT_BYTES` (512 KiB) plus JSON escaping headroom; the
/// other mutations carry an id or a path.
fn memory_route_descriptor(
    route_id: &'static str,
    method: NetworkMethod,
    pattern: &'static str,
) -> IngressRouteDescriptor {
    let policy = match method {
        NetworkMethod::Get => read_policy(
            read_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
            StreamingMode::None,
        ),
        _ => mutation_policy(
            body_limit_kib(if route_id == WEBUI_V2_ROUTE_MEMORY_WRITE {
                1024
            } else {
                4
            }),
            mutation_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
        ),
    };
    descriptor(route_id, method, pattern, policy)
}

fn notification_setup_status_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_NOTIFICATION_SETUP_STATUS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_NOTIFICATION_SETUP_STATUS,
    )
}

fn notification_setup_enable_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_NOTIFICATION_SETUP_ENABLE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_NOTIFICATION_SETUP_ENABLE,
        mutation_policy(
            // The largest setup payload today is a web-app subscription: a
            // ~1 KiB endpoint URL plus two short base64url keys; 8 KiB leaves
            // defensive headroom for any channel's opaque payload.
            body_limit_kib(8),
            mutation_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
        ),
    )
}

fn notification_setup_disable_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_NOTIFICATION_SETUP_DISABLE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_NOTIFICATION_SETUP_DISABLE,
        8,
    )
}

fn list_extensions_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_LIST_EXTENSIONS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_EXTENSIONS,
    )
}

fn list_extension_registry_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_LIST_EXTENSION_REGISTRY,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_EXTENSION_REGISTRY,
    )
}

fn install_extension_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_INSTALL_EXTENSION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_INSTALL_EXTENSION,
        16,
    )
}

fn ironhub_deliver_install_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_IRONHUB_DELIVER_INSTALL,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_IRONHUB_DELIVER_INSTALL,
        16,
    )
}

fn register_hosted_mcp_extension_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_REGISTER_HOSTED_MCP_EXTENSION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_REGISTER_HOSTED_MCP_EXTENSION,
        16,
    )
}

/// `POST /api/webchat/v2/extensions/import` — admin uploads a standalone tool
/// bundle (zip). 8 MiB cap covers a WASM module plus manifest/schemas/prompts.
fn import_extension_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_IMPORT_EXTENSION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_IMPORT_EXTENSION,
        8192,
    )
}

fn remove_extension_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_REMOVE_EXTENSION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_REMOVE_EXTENSION,
        4,
    )
}

fn get_extension_setup_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_GET_EXTENSION_SETUP,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_SETUP_EXTENSION,
    )
}

fn setup_extension_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SETUP_EXTENSION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SETUP_EXTENSION,
        16,
    )
}

fn list_skills_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_LIST_SKILLS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LIST_SKILLS,
    )
}

fn search_skills_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SEARCH_SKILLS,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SEARCH_SKILLS,
        4,
    )
}

fn install_skill_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_INSTALL_SKILL,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_INSTALL_SKILL,
        128,
    )
}

fn get_skill_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_GET_SKILL,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_SKILL_DETAIL,
    )
}

fn update_skill_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_UPDATE_SKILL,
        NetworkMethod::Put,
        WEBUI_V2_PATTERN_SKILL_DETAIL,
        128,
    )
}

fn remove_skill_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_REMOVE_SKILL,
        NetworkMethod::Delete,
        WEBUI_V2_PATTERN_SKILL_DETAIL,
        4,
    )
}

fn set_skill_auto_activate_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SET_SKILL_AUTO_ACTIVATE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SET_SKILL_AUTO_ACTIVATE,
        4,
    )
}

fn set_auto_activate_learned_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SET_AUTO_ACTIVATE_LEARNED,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SET_AUTO_ACTIVATE_LEARNED,
        4,
    )
}

fn list_settings_tools_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_LIST_SETTINGS_TOOLS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_SETTINGS_TOOLS,
    )
}

fn set_settings_tools_auto_approve_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SET_SETTINGS_TOOLS_AUTO_APPROVE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SETTINGS_TOOLS,
        4,
    )
}

fn set_settings_tool_permission_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SET_SETTINGS_TOOL_PERMISSION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SETTINGS_TOOL_PERMISSION,
        4,
    )
}

fn get_llm_config_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_GET_LLM_CONFIG,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_GET_LLM_CONFIG,
    )
}

fn upsert_llm_provider_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_UPSERT_LLM_PROVIDER,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_UPSERT_LLM_PROVIDER,
        16,
    )
}

fn delete_llm_provider_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_DELETE_LLM_PROVIDER,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_DELETE_LLM_PROVIDER,
        4,
    )
}

fn set_active_llm_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SET_ACTIVE_LLM,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_SET_ACTIVE_LLM,
        4,
    )
}

fn get_user_model_catalog_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_GET_USER_MODEL_CATALOG,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_USER_MODEL_CATALOG,
    )
}

fn get_user_model_preference_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_GET_USER_MODEL_PREFERENCE,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_USER_MODEL_PREFERENCE,
    )
}

fn set_user_model_preference_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SET_USER_MODEL_PREFERENCE,
        NetworkMethod::Put,
        WEBUI_V2_PATTERN_USER_MODEL_PREFERENCE,
        4,
    )
}

fn set_user_model_policy_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_SET_USER_MODEL_POLICY,
        NetworkMethod::Put,
        WEBUI_V2_PATTERN_USER_MODEL_POLICY,
        64,
    )
}

fn test_llm_connection_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_TEST_LLM_CONNECTION,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_TEST_LLM_CONNECTION,
        16,
    )
}

fn list_llm_models_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_LIST_LLM_MODELS,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_LIST_LLM_MODELS,
        16,
    )
}

fn start_nearai_login_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_START_NEARAI_LOGIN,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_START_NEARAI_LOGIN,
        4,
    )
}

fn complete_nearai_wallet_login_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_COMPLETE_NEARAI_WALLET_LOGIN,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_COMPLETE_NEARAI_WALLET_LOGIN,
        4,
    )
}

fn start_codex_login_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_START_CODEX_LOGIN,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_START_CODEX_LOGIN,
        4,
    )
}

fn operator_get_setup_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_OPERATOR_GET_SETUP,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_OPERATOR_SETUP,
    )
}

fn operator_run_setup_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_OPERATOR_RUN_SETUP,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_OPERATOR_SETUP,
        16,
    )
}

fn operator_list_config_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_OPERATOR_LIST_CONFIG,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_OPERATOR_CONFIG,
    )
}

fn operator_get_config_key_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_OPERATOR_GET_CONFIG_KEY,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_OPERATOR_CONFIG_KEY,
    )
}

fn operator_set_config_key_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_OPERATOR_SET_CONFIG_KEY,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_OPERATOR_CONFIG_KEY,
        16,
    )
}

fn operator_validate_config_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_OPERATOR_VALIDATE_CONFIG,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_OPERATOR_CONFIG_VALIDATE,
        16,
    )
}

fn operator_diagnostics_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_OPERATOR_DIAGNOSTICS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_OPERATOR_DIAGNOSTICS,
    )
}

fn operator_status_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_OPERATOR_STATUS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_OPERATOR_STATUS,
    )
}

fn operator_logs_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_OPERATOR_LOGS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_OPERATOR_LOGS,
    )
}

fn inspector_snapshot_descriptor() -> IngressRouteDescriptor {
    inspector_read_descriptor(
        WEBUI_V2_ROUTE_INSPECTOR_SNAPSHOT,
        WEBUI_V2_PATTERN_INSPECTOR_SNAPSHOT,
    )
}

fn inspector_prompt_descriptor() -> IngressRouteDescriptor {
    inspector_read_descriptor(
        WEBUI_V2_ROUTE_INSPECTOR_PROMPT,
        WEBUI_V2_PATTERN_INSPECTOR_PROMPT,
    )
}

fn inspector_tool_descriptor() -> IngressRouteDescriptor {
    inspector_read_descriptor(
        WEBUI_V2_ROUTE_INSPECTOR_TOOL,
        WEBUI_V2_PATTERN_INSPECTOR_TOOL,
    )
}

fn inspector_read_descriptor(
    route_id: &'static str,
    pattern: &'static str,
) -> IngressRouteDescriptor {
    descriptor(
        route_id,
        NetworkMethod::Get,
        pattern,
        read_policy(
            rate_limit_per_caller(30, 60),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProjectionOnly,
            StreamingMode::None,
        ),
    )
}

fn inspector_updates_descriptor() -> IngressRouteDescriptor {
    descriptor(
        WEBUI_V2_ROUTE_INSPECTOR_UPDATES,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_INSPECTOR_UPDATES,
        read_policy(
            rate_limit_per_caller(12, 60),
            AuditTraceClass::StreamingSubscription,
            AllowedEffectPath::ProjectionOnly,
            StreamingMode::Sse,
        ),
    )
}

fn logs_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_LOGS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_LOGS,
    )
}

fn operator_service_lifecycle_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_OPERATOR_SERVICE_LIFECYCLE,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_OPERATOR_SERVICE_LIFECYCLE,
        4,
    )
}

fn operator_list_extension_configuration_descriptor() -> IngressRouteDescriptor {
    projection_read_route(
        WEBUI_V2_ROUTE_OPERATOR_LIST_EXTENSION_CONFIGURATION,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_OPERATOR_EXTENSION_CONFIGURATION,
    )
}

fn operator_replace_extension_configuration_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_OPERATOR_REPLACE_EXTENSION_CONFIGURATION,
        NetworkMethod::Put,
        WEBUI_V2_PATTERN_OPERATOR_EXTENSION_CONFIGURATION_GROUP,
        16,
    )
}

/// A read on the standard read budget that may reach the product surface.
fn read_route(
    route_id: &'static str,
    method: NetworkMethod,
    pattern: &'static str,
) -> IngressRouteDescriptor {
    descriptor(
        route_id,
        method,
        pattern,
        read_policy(
            read_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
            StreamingMode::None,
        ),
    )
}

/// A read on the standard read budget confined to projections.
fn projection_read_route(
    route_id: &'static str,
    method: NetworkMethod,
    pattern: &'static str,
) -> IngressRouteDescriptor {
    descriptor(
        route_id,
        method,
        pattern,
        read_policy(
            read_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProjectionOnly,
            StreamingMode::None,
        ),
    )
}

/// A product-surface mutation on the standard mutation budget with a body of
/// at most `body_kib` KiB.
fn mutation_route(
    route_id: &'static str,
    method: NetworkMethod,
    pattern: &'static str,
    body_kib: u64,
) -> IngressRouteDescriptor {
    descriptor(
        route_id,
        method,
        pattern,
        mutation_policy(
            body_limit_kib(body_kib),
            mutation_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
        ),
    )
}

/// A product-surface mutation on the standard mutation budget that accepts
/// no request body.
fn bodyless_mutation_route(
    route_id: &'static str,
    method: NetworkMethod,
    pattern: &'static str,
) -> IngressRouteDescriptor {
    descriptor(
        route_id,
        method,
        pattern,
        mutation_policy(
            BodyLimitPolicy::NoBody,
            mutation_rate_limit(),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
        ),
    )
}

fn descriptor(
    route_id: &str,
    method: NetworkMethod,
    pattern: &str,
    policy: IngressPolicy,
) -> IngressRouteDescriptor {
    IngressRouteDescriptor::new(route_id.to_string(), method, pattern.to_string(), policy)
        .expect("webui v2 route descriptor must validate at startup") // safety: route_id/pattern are crate-local literals known to satisfy IngressRouteId / IngressRoutePattern; policy is constructed by sibling helpers that validate their own inputs
}

fn mutation_policy(
    body_limit: BodyLimitPolicy,
    rate_limit: RateLimitPolicy,
    audit: AuditTraceClass,
    effect_path: AllowedEffectPath,
) -> IngressPolicy {
    IngressPolicy::new(IngressPolicyParts {
        listener_class: ListenerClass::LocalGateway,
        auth: bearer_required(),
        scope_source: IngressScopeSource::AuthenticatedCaller,
        body_limit,
        rate_limit,
        cors: CorsPolicy::SameOriginOnly,
        websocket_origin: WebSocketOriginPolicy::NotApplicable,
        streaming: StreamingMode::None,
        audit,
        effect_path,
    })
    .expect("webui v2 mutation policy must validate") // safety: all parts are crate-local constants; the combination (LocalGateway + bearer required + AuthenticatedCaller + None streaming) is a permitted shape, locked in by the descriptor contract test
}

fn read_policy(
    rate_limit: RateLimitPolicy,
    audit: AuditTraceClass,
    effect_path: AllowedEffectPath,
    streaming: StreamingMode,
) -> IngressPolicy {
    IngressPolicy::new(IngressPolicyParts {
        listener_class: ListenerClass::LocalGateway,
        auth: bearer_required(),
        scope_source: IngressScopeSource::AuthenticatedCaller,
        body_limit: BodyLimitPolicy::NoBody,
        rate_limit,
        cors: CorsPolicy::SameOriginOnly,
        websocket_origin: WebSocketOriginPolicy::NotApplicable,
        streaming,
        audit,
        effect_path,
    })
    .expect("webui v2 read policy must validate") // safety: streaming is either None or Sse (both permitted with bearer + AuthenticatedCaller); other parts are crate-local constants
}

fn bearer_required() -> IngressAuthPolicy {
    IngressAuthPolicy::Required {
        schemes: vec![IngressAuthScheme::BearerToken],
    }
}

fn body_limit_kib(kib: u64) -> BodyLimitPolicy {
    let bytes = kib
        .checked_mul(1024)
        .and_then(NonZeroU64::new)
        .expect("body limit must be non-zero"); // safety: all call sites pass crate-local positive constants (4, 16, 1024); overflow at u64 * 1024 is impossible for these
    BodyLimitPolicy::Limited { max_bytes: bytes }
}

fn mutation_rate_limit() -> RateLimitPolicy {
    rate_limit_per_caller(60, 60)
}

fn read_rate_limit() -> RateLimitPolicy {
    rate_limit_per_caller(120, 60)
}

fn thread_artifact_rate_limit() -> RateLimitPolicy {
    rate_limit_per_caller(6, 60)
}

fn stream_rate_limit() -> RateLimitPolicy {
    // Budget for the SSE (`stream_events`) route. SSE sessions are long-lived; the
    // per-tenant/user concurrency cap (3 streams, enforced in
    // `WebUiV2State::SseCapacity`) does the real bounding. The
    // request-rate window here is just for burst protection against
    // reconnect storms.
    //
    // Set to 30/60s. The SPA's one-owner reconnect policy opens at most
    // seven streams per minute for one continuously failing tab (1, 2, 4,
    // 8, 16, then 30-second bounded backoff). Three legitimate tabs therefore
    // stay below this budget with headroom for reload and network transitions.
    // The route also retains the `?token=…` compatibility escape hatch, so a
    // materially higher budget would widen the replay surface for a token
    // captured from browser history or proxy logs. The concurrency cap remains
    // the primary bound on healthy long-lived streams; this request-rate window
    // bounds churn from broken or hostile clients.
    rate_limit_per_caller(30, 60)
}

fn rate_limit_per_caller(max: u32, window_secs: u32) -> RateLimitPolicy {
    RateLimitPolicy::Limited {
        scope: RateLimitScope::PerCaller,
        max_requests: NonZeroU32::new(max).expect("max_requests must be non-zero"), // safety: all call sites pass crate-local positive constants (12, 60, 120, 900)
        window_seconds: NonZeroU32::new(window_secs).expect("window_seconds must be non-zero"), // safety: all call sites pass crate-local positive constants (60)
    }
}

fn computer_use_session_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_COMPUTER_USE_SESSION,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_COMPUTER_USE,
    )
}

/// The viewer polls these while an agent drives apps: the state about once a
/// second and one window's picture at up to 10 frames a second.
/// Operator read of host facts; the product surface refuses other callers.
fn computer_use_readiness_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_COMPUTER_USE_READINESS,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_COMPUTER_USE_READINESS,
    )
}

fn computer_use_live_descriptor(
    route: &'static str,
    pattern: &'static str,
) -> IngressRouteDescriptor {
    descriptor(
        route,
        NetworkMethod::Get,
        pattern,
        read_policy(
            rate_limit_per_caller(900, 60),
            AuditTraceClass::UserAction,
            AllowedEffectPath::ProductSurface,
            StreamingMode::None,
        ),
    )
}

fn computer_use_control_descriptor() -> IngressRouteDescriptor {
    mutation_route(
        WEBUI_V2_ROUTE_COMPUTER_USE_CONTROL,
        NetworkMethod::Post,
        WEBUI_V2_PATTERN_COMPUTER_USE,
        4,
    )
}

fn usage_history_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_USAGE_HISTORY,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_USAGE_HISTORY,
    )
}

fn usage_report_descriptor() -> IngressRouteDescriptor {
    read_route(
        WEBUI_V2_ROUTE_USAGE_REPORT,
        NetworkMethod::Get,
        WEBUI_V2_PATTERN_USAGE_REPORT,
    )
}
