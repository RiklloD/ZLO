# IronClaw Desktop parity checklist

Phase 4 is done when every gateway route below is either covered by the
desktop app or excluded with a reason. Route ids come from
`webui_v2_routes_with_artifact_flags` in
`crates/product/ironclaw_webui/src/webui_v2/descriptors.rs`; re-derive the list
after an upstream merge, since new routes land there first.

Status: `done` (the app calls it), `todo` (planned step in brackets),
`excluded` (reason given). Started 23.10.2026.

## Chat and threads

| Route | Status |
|---|---|
| get_session, list_threads, create_thread, delete_thread | done |
| session_channel_message, get_timeline, stream_events | done |
| cancel_run, resolve_gate, retry_run | done |
| stream_events_ws | excluded: SSE carries the same frames |
| list_commands, execute_command | done |
| get_attachment (+ attachments on send) | done |
| suggestions.list, suggestions.generate, suggestions.start, suggestions.dismiss | done |
| get_user_model_catalog, get_user_model_preference, set_user_model_preference, set_user_model_policy | done (not live-tested: saving a quick-switch list cannot be undone through the API) |
| get_run_artifact, get_thread_artifact | excluded: debug export behind `regression_artifact_export`, off in this deployment (404) |

## Notifications

| Route | Status |
|---|---|
| list_notifications, mark_notification_read, mark_all_notifications_read, archive_notification | done |
| get_notification_channels, set_notification_channels, list_outbound_delivery_targets | done (Notifications page, Channels section) |
| notification_setup_status | done; enable/disable excluded: the only enrollable target is browser web push, which needs a browser push subscription |

## Automations

| Route | Status |
|---|---|
| list_automations, pause_automation, resume_automation, run_automation, delete_automation | done |
| rename_automation | done (not live-tested: no automation exists) |

## Files and projects

| Route | Status |
|---|---|
| list_fs_mounts, browse_fs_dir, stat_fs_path, read_fs_file | done |
| list_projects, create_project, update_project | done |
| delete_project, list_project_files, read_project_file | done |
| get_project | excluded: returns the same record as list_projects |
| stat_project_file | done (client call; the panel lists and opens files without a stat step) |
| list_project_members, add_project_member, update_project_member, remove_project_member | excluded: single user (decided 23.10.2026) |

## Extensions

| Route | Status |
|---|---|
| list_extensions, list_extension_registry, install_extension, remove_extension | done |
| get_extension_setup, setup_extension (API keys, config fields) | done |
| channel pairing (mint, status, unpair) | done (Connect on active channel extensions) |
| OAuth setup start | excluded: the OAuth callback reports to a browser popup; the app hands off with "Sign in via browser" |
| import_extension, register_hosted_mcp_extension | done (not live-tested: both install real extensions) |
| ironhub_deliver_install | excluded: a signed delivery IronHub posts, not a user action; the web UI has no caller either |
| operator_list_extension_configuration, operator_replace_extension_configuration | done (Operator > Extensions; blank secrets keep the stored value) |

## Skills

| Route | Status |
|---|---|
| list_skills, get_skill, search_skills, install_skill, remove_skill, set_skill_auto_activate | done |
| update_skill (edit SKILL.md), set_auto_activate_learned | done |

## Settings and models

| Route | Status |
|---|---|
| settings.list_tools, auto-approve switch, per-tool permission | done |
| get_llm_config, upsert_llm_provider (API key), set_active_llm, list_llm_models | done |
| delete_llm_provider, test_llm_connection | done |
| start_codex_login | done (device code shown in Settings, app waits until Codex is active) |
| start_nearai_login, complete_nearai_wallet_login | excluded: NEAR AI SSO refuses loopback origins (the web UI blocks it locally too) and wallet login needs a browser wallet; a NEAR AI API key works through Set key |

## Operator

| Route | Status |
|---|---|
| operator.get_setup, operator.run_setup (onboarding) | done |
| operator.list_config, get_config_key, set_config_key, validate_config | done |
| operator.status, operator.diagnostics | done |
| operator.service_lifecycle: status, stop | done |
| operator.service_lifecycle: start, install | excluded: the app reaches the service through the gateway, which is down whenever start is needed; see note below |
| logs (per conversation, in the inspector), operator.logs | done |
| operator.inspector_snapshot, inspector_prompt, inspector_tool, inspector_updates | done |

## TraceCommons

| Route | Status |
|---|---|
| trace_credits, trace_account_traces, trace_account_login_link, authorize_trace_hold | done: Settings > Trace Commons (login link opens in the browser) |

## Multi-user admin

| Route | Status |
|---|---|
| admin.* (users, roles, status, secrets), admin thread scrape | excluded: single user (decided 23.10.2026) |

## CLI commands with no GUI form

`completion`, `run`, `profile` stay terminal-only. `service install` sets up
the backend the app connects to, so it stays in the terminal too. Status and
stop are in the app (Operator page). Start and restart stay in the terminal
(`ironclaw service start`): the gateway is the only door to the service, and
it is down whenever start is needed. The contract has no restart action. A
later step could let the connect screen run `ironclaw service start` locally.
