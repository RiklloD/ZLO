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
| list_commands, execute_command | todo [4.2] |
| get_attachment (+ attachments on send) | todo [4.2] |
| suggestions.list, suggestions.generate, suggestions.start, suggestions.dismiss | todo [4.2] |
| get_user_model_catalog, get_user_model_preference, set_user_model_preference, set_user_model_policy | todo [4.2] |
| get_run_artifact, get_thread_artifact | todo [4.2] |

## Notifications

| Route | Status |
|---|---|
| list_notifications, mark_notification_read, mark_all_notifications_read, archive_notification | done |
| get_notification_channels, set_notification_channels, list_outbound_delivery_targets | todo [4.3] |
| notification_setup_status, notification_setup_enable, notification_setup_disable | todo [4.3] |

## Automations

| Route | Status |
|---|---|
| list_automations, pause_automation, resume_automation, run_automation, delete_automation | done |
| rename_automation | todo [4.2] |

## Files and projects

| Route | Status |
|---|---|
| list_fs_mounts, browse_fs_dir, stat_fs_path, read_fs_file | done |
| list_projects, create_project, update_project | done |
| get_project, delete_project, list_project_files, stat_project_file, read_project_file | todo [4.2] |
| list_project_members, add_project_member, update_project_member, remove_project_member | excluded: single user (decided 23.10.2026) |

## Extensions

| Route | Status |
|---|---|
| list_extensions, list_extension_registry, install_extension, remove_extension | done |
| get_extension_setup, setup_extension (API keys, config fields) | done |
| OAuth setup start, channel pairing | todo [4.3] |
| import_extension, register_hosted_mcp_extension, ironhub_deliver_install | todo [4.3] |
| operator_list_extension_configuration, operator_replace_extension_configuration | todo [4.3] |

## Skills

| Route | Status |
|---|---|
| list_skills, get_skill, search_skills, install_skill, remove_skill, set_skill_auto_activate | done |
| update_skill (edit SKILL.md), set_auto_activate_learned | todo [4.3] |

## Settings and models

| Route | Status |
|---|---|
| settings.list_tools, auto-approve switch, per-tool permission | done |
| get_llm_config, upsert_llm_provider (API key), set_active_llm, list_llm_models | done |
| delete_llm_provider, test_llm_connection | todo [4.3] |
| start_nearai_login, complete_nearai_wallet_login, start_codex_login | todo [4.3] |

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
| trace_credits, trace_account_traces, trace_account_login_link, authorize_trace_hold | todo [4.5] |

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
