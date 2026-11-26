# ZLO: Continuous Configuration & Agent-Context Sync Across Computers

Status: Research & design proposal (2026-11-26). Builds on
`docs/internal/plans/2026-11-26-zlo-two-backends.md` (multi-computer Rune) and
`docs/internal/plans/2026-11-18-zlo-multi-machine.md` (device scopes, peer links).

## Goal

A person runs ZLO's desktop app on a Mac (with a local backend) and pairs a
second ZLO backend on a Linux box (`tools/ironclaw_rune/docs/remote-computers.md`).
Threads started on the Linux box should behave identically to threads started on
the Mac — same settings, profiles, memory, skills, system prompts, LLM providers
and keys, MCP servers, extensions, hooks, tool permissions, automations, and the
exact context ZLO's CLI manager passes to Claude Code, Codex, Antigravity (`agy`),
and Muse Code — **except** macOS-only capabilities (computer use, voice,
SnapShots, macOS permissions), machine identity, device tokens, CLI logins, and
Mac-only paths. Sync must be **continuous** (not a one-time copy) and must
**never delete or overwrite** the box's own threads, runs, events, or history
(repo invariant: LLM data is never deleted).

---

## 1. Inventory of Backend & App State

### 1.1 Backend State Matrix

All virtual paths below live inside `CompositeRootFilesystem`
(`crates/app/ironclaw_composition/src/filesystem_assembly.rs:87-115`), backed by
`reborn-local-dev.db` (libSQL) or PostgreSQL for `/tenants`, `/memory`, `/events`,
`/system/settings`, `/system/extensions/.installations`
(`filesystem_assembly.rs:238-292`), and by `DiskFilesystem` for `/projects`,
`/system/extensions`, `/system/skills` (`filesystem_assembly.rs:147-176,333-368`).
`orch_*` and `trigger_*` tables live in the same SQLite/PostgreSQL database.

| Domain | What it is & where it lives (`file:line`) | Writers | Portability | Size / Churn |
|---|---|---|---|---|
| **Bootstrap config** | `~/.zlo/config.toml` (`ironclaw_config/src/home.rs:236-238`). Sections in `RebornConfigFile` (`config_file.rs:61-129`): `boot`, `identity`, `policy`, `drivers`, `harness`, `runner`, `skills`, `storage`, `llm.<slot>`, `webui`, `google`, `budget`, `trigger_poller`, `memory`, `embeddings`, `remote`, `update`. | `zlo onboard`, `zlo config`, `zlo remote`, `PUT /api/webchat/v2/operator/config/{key}` (`webui_v2/descriptors.rs:441`), manual edit. | **Split.** Portable: `policy`, `drivers`, `harness`, `runner`, `skills`, `llm`, `google`, `budget`, `trigger_poller`, `memory`, `embeddings`. Machine-local: `remote` (tailnet URL/owner/ports), `webui` (listen host/port/canonical_host), `storage` (DB backend/env vars), `update` (local repo path), `boot` (profile). | 1–5 KiB / Low |
| **Orchestrator settings** | `orch_settings` table (`key = 'settings'`, JSON `Settings` in `ironclaw_orchestrator/src/store.rs:23,544-582`, struct in `model.rs:482-511`): `continue_after_restart`, `resume_on_usage_limit`, `usage_limit_grace_ms`, `max_depth`, `max_running_per_parent`, `handoff_bytes`, `model_picker` (`hidden_providers`, `hidden_models`, `favorites`), `idle_compact_claude`. | `Store::set_settings` (`store.rs:559`), `profile_runtime.rs:288,300`, WebUI settings routes. | **Portable.** | < 4 KiB / Low–Med |
| **System settings mount** | `/system/settings/*` in DB (`filesystem_assembly.rs:259-263`): skill disk-import markers (`/system/settings/skill-disk-import`, `standalone_bootstrap_assembly.rs:63`), IronHub link state (`/system/settings/ironhub`, `ironclaw_extension_manager/src/ironhub/link_service.rs:30`). | Boot bootstrap, `zlo ironhub`, WebUI `/api/webchat/v2/ironhub/*`. | **Mixed.** Import markers are machine-local; IronHub link config is portable (token is a secret). | < 4 KiB / Rare |
| **Boot profiles vs. Agent profiles** | **Boot profiles** (`commands/profile.rs:5-63`): compiled-in `RebornProfile::all()` enum, not persisted. **Agent profiles**: `orch_profiles` table (`ironclaw_orchestrator/src/profiles.rs:20-31`) keyed by `(tenant_id, user_id, profile_id)`, holding `ProfileSettings` JSON (`ironclaw_product_contracts/src/profiles.rs:37-70`): `display_name`, `soul` (≤24 KiB), `allowed_capabilities`, `hidden_skills`, `enabled_mcp_servers`, `default_provider`, `default_model`, `working_folder`, `supermemory` (`container`, `read_containers`, `instructions`), `approval_mode` (`FullAccess`/`AskFirst`), plus `archived_at` soft-delete. Default folder when `working_folder` is `None`: `~/.ironclaw/profiles/<id>/` (`profiles.rs:61`). | `profiles.create` / `profiles.update` / `profiles.delete` / `profiles.import_hermes` (`ironclaw_product_contracts/src/profiles.rs:189-222`), `POST/PUT/DELETE /api/webchat/v2/profiles` (`descriptors.rs:247-250`). | **Portable with path rewrite.** `working_folder` holds absolute paths (`/Users/…` → `/home/…`); `~/` stays portable. `last_used_at` and `thread_count` are derived from `orch_threads` (`profiles.rs:170-177`) and never stored on the row. | 1–50 KiB / Med |
| **User & system skills** | User skills: DB-backed `/projects/tenants/<t>/users/<u>/skills/<name>/SKILL.md` + bundle files (`ironclaw_skills/src/scoped_management.rs:27-44`, `filesystem_assembly.rs:24-26`). System skills: disk `/system/skills` → `<storage_root>/system/skills` (`filesystem_assembly.rs:166-168`). CLI plugin mirror: derived cache at `~/.ironclaw/cli-skills/<profile>/` (`ironclaw_assistant/src/orchestration/profile_context.rs:4-5,74-103`). | `POST /api/webchat/v2/skills/install`, `PUT/DELETE /api/webchat/v2/skills/{name}` (`descriptors.rs:412-415`), `zlo skills`, agent skill-learning (`ironclaw_skills/src/learning.rs`). | **Portable.** CLI mirror is regenerated automatically per turn from the skill store (`profile_context.rs:88-103`), so it never needs syncing. | 10 KiB–5 MiB / Med |
| **ZLO-wide skill manager (usage & pins)** | `/skill-usage/listing.json` per user (`ironclaw_skills/src/usage/store.rs:20`, `SkillUsageDocument` in `usage.rs:10-16`): per-source catalogs, usage counters/timestamps (`SkillLedger`), and explicit user `SkillPin`s (`Full`/`NameOnly`/`Off`). Legacy migration file: `~/.ironclaw/claude-skills/<id>.json` (`claude/skills.rs:56-59`). | `PUT /api/webchat/v2/skill-listing/{name}/pin` (`descriptors.rs:418`), automatic recording on every harness & CLI turn (`skill_listing.rs:72-93`, `claude/skills.rs:12-16`). | **Portable (mergeable).** Pins are user config; usage counters are written by agents on *both* machines. | 2–50 KiB / High (per turn) |
| **Native memory** | DB-backed `/memory/tenants/<t>/users/<u>/agents/<a>/projects/<p>/<rel_path>` + `.meta`, `.chunks/*`, `.versions/*` sidecars (`ironclaw_memory/src/path.rs:75-83,240-260`, `filesystem_assembly.rs:178-198`). Includes identity docs (`MEMORY.md`, `USER.md`, `AGENTS.md`, `HEARTBEAT.md`, etc. — `ironclaw_memory/src/safety.rs:148-152`). | `ironclaw.memory.*` tools (agents on both machines), WebUI `/api/webchat/v2/memory/documents*` (`descriptors.rs:273-275`), `zlo memory`, background curation (`config_file.rs:198-231`). | **Portable (two-way merge).** Agents on both Mac and Linux write memories. `.chunks` / embeddings are recomputable on write (`config_file.rs:153-155`). | 10 KiB–50 MiB / High |
| **Supermemory integration** | Config from env vars (`SUPERMEMORY_BASE_URL`, `SUPERMEMORY_AUTO_RECALL`, `SUPERMEMORY_AUTO_CAPTURE`, `SUPERMEMORY_MAX_RECALL_RESULTS` — `ironclaw_supermemory/src/config.rs:45-56`). API key in secret store as `supermemory_api_key` (`config.rs:9`). Per-profile containers in `orch_profiles.settings.supermemory` (`profiles.rs:23-31`). | `POST/DELETE /api/webchat/v2/memory/supermemory*` (`descriptors.rs:277-283`), env vars, profile editor. | **Portable (cloud-backed).** Syncing `supermemory_api_key` + profile `supermemory` containers immediately shares the same cloud memory; env flags need `config.toml` or env parity. | < 1 KiB / Low |
| **Prompts (compiled & user-editable)** | **Compiled**: `prompts/*.md` across 23 crates (`include_str!()`), e.g., `DELEGATION_PROMPT`, `REPLY_VISUALS_PROMPT`, `ANTIGRAVITY_TOOLS_PROMPT` (`ironclaw_cli_providers/src/antigravity/context.rs:18-19`), protocol prompts in `ironclaw_loop_host/src/system_prompt_assets.rs:12-17`. **User-editable**: `<storage_root>/SYSTEM.md` (≤64 KiB, seeded on boot by `ironclaw_composition/src/root/default_system_prompt.rs:26-27,94-120`); per-profile `SOUL.md` in `orch_profiles.settings.soul`. | Manual edit of `SYSTEM.md`; profile editor for `SOUL.md`. | **Compiled**: ships with binary (protocol version match). **`SYSTEM.md` & `SOUL.md`**: **Portable.** | 5–30 KiB / Low |
| **LLM providers & model prefs** | Custom provider catalog: `~/.zlo/providers.json` (`ironclaw_config/src/home.rs:249-250`, `ironclaw_llm/src/registry.rs:1-21`). Active slot: `[llm.default]` in `config.toml` (`config_file.rs:83,660-672`). Policy & user prefs: `FilesystemModelSelectionPolicyStore` & `UserModelPreference` (`ironclaw_composition/src/product_surface.rs:445-453`), task models (`/api/webchat/v2/llm/task-models`, `descriptors.rs:326`), picker prefs in `orch_settings.model_picker`. | `POST/DELETE /api/webchat/v2/llm/providers*`, `PUT /llm/active`, `/llm/model-policy`, `/llm/model-preference`, `/llm/task-models` (`descriptors.rs:426-438`), `zlo models`. | **Portable** (except `base_url` pointing to `127.0.0.1`/Ollama on one host, and OAuth/Codex/NEAR AI logins tied to tokens). | 2–50 KiB / Low |
| **Secrets & credentials** | DB-backed `/tenants/<t>/users/<u>/secrets/...` (`ironclaw_secrets/src/secret_store.rs:13-27`): `StoredSecret` (AES-256-GCM + HKDF-SHA256 with per-record 32-byte salt and `(tenant, user, agent, project, handle)` AAD — `secret_store.rs:106-117`, `crypto.rs:1-24`), `StoredAccount` (`credential-accounts/`), `StoredSession` (`credential-sessions/`). Operator/system secrets live at `ResourceScope::system()` via `RuntimeOperatorSecretValueStore` (`operator_secret_store.rs:14-15,57-95`). Values may be `op://vault/item/field` references resolved at consumption time (`ironclaw_secrets/src/reference.rs:1-10,65-74`). Master key: `<storage_root>/.reborn-local-dev-secrets-master-key` (0600), else `SECRETS_MASTER_KEY`, else OS Keychain (`setup_transfer.rs:425-471`). | `OperatorSecretValueStore::put`, `SecretStorePort::put`, LLM provider setup, extension setup/OAuth, `t3_request_secret`. | **Portable with filtering & re-encryption.** Provider API keys (`openai`, `anthropic`, `supermemory_api_key`, etc.) and `op://` refs are portable. **Never sync**: `peer_link_token_*` handles (`peer_client.rs:75-80`), `secret-leases/*`, machine-bound OAuth sessions, or the raw master key over a hot API. | 1–50 KiB / Low |
| **Extensions & MCP servers** | Extension packages (WASM, manifests, assets): disk `/system/extensions/<id>/` (`filesystem_assembly.rs:162-164`). Installation records, owners, resolved manifests, and hosted HTTP MCP endpoint definitions: DB `/system/extensions/.installations/*` (`filesystem_assembly.rs:253-257`, `ironclaw_extension_host/src/product_lifecycle.rs:4474`, `mcp.rs:72-100`). Admin/extension settings: `/tenants/<t>/shared/admin-configuration` (`ironclaw_extension_manager/src/admin_configuration.rs:613-614`) and `/api/webchat/v2/extensions/{id}/settings/{setting_id}` (`descriptors.rs:287-289`). Per-profile filter: `orch_profiles.settings.enabled_mcp_servers`. | `zlo extension`, WebUI `/api/webchat/v2/extensions/*`, `/api/webchat/v2/operator/extension-configuration/*` (`descriptors.rs:456-458`). | **Mostly portable.** Extension manifests, hosted HTTP MCP configs, and extension settings are portable; WASM packages are portable bytecode; channel extensions (Slack/Telegram) must **not** run duplicate polling/webhooks on both machines simultaneously (see §3.4). | 10 KiB–20 MiB / Low |
| **Plugins (UI slots)** | No separate store: UI slots are declared in active extension manifests (`resolved.ui_slots`) and read from `/system/extensions/.installations` by `RuntimePluginUiHost` (`ironclaw_composition/src/plugin_ui_host.rs:43-85`). | Extension install/activation. | **Portable** (follows extensions). | Included above |
| **Hooks** | User/workspace hook files: `~/.ironclaw/hooks/` and workspace `hooks/` (`ironclaw_hooks/src/trust.rs:17`). Self-authored hook policies (`ironclaw_hooks/src/self_authored.rs:62-65`). Persistent predicate state: `hooks_predicate_invocations` and `hooks_predicate_values` tables (`ironclaw_hooks/src/libsql_backend/migrations/V1__predicate_state.sql:48-68`). Worktree hook approvals: `orch_worktree_hook_approvals` (`ironclaw_orchestrator/src/worktree/store.rs:35`). | User files, `GET/POST /api/webchat/v2/hooks` (`descriptors.rs:285`), agent self-authored hooks, worktree trust prompts. | **Mixed.** Hook definitions and self-authored rules are portable; `orch_worktree_hook_approvals` carries `repo_root` (needs path rewrite); `hooks_predicate_invocations` is runtime history (do not sync). | 1–100 KiB / Low |
| **Tool permissions & approval modes** | Global auto-approve toggle: `/approvals/auto-approve` in DB (`ironclaw_approvals/src/auto_approve.rs:46-48`). Per-tool overrides (`ask_each_time` / `always_allow` / `disabled`): `/tenants/<t>/users/<u>/approvals/*` (`capability_permission.rs:421-460,564-565`). Per-profile mode: `orch_profiles.settings.approval_mode` (`profile_approval_mode.rs:1-21`). Built-in policy floor: compiled `builtin_capability_policy.toml`. CLI approval audit log: `orch_cli_approvals` (`cli_approvals.rs:20-31`). | `PUT /api/webchat/v2/settings/tools`, `PUT /api/webchat/v2/settings/tools/{capability_id}` (`descriptors.rs:423-425`), profile editor, approval cards. | **Portable** for settings & overrides. `orch_cli_approvals` is runtime audit history (**never sync**). | 1–20 KiB / Low |
| **Triggers & automations** | Two systems: (1) Routine triggers in `trigger_records` (`ironclaw_triggers/src/libsql.rs:24,103-120`, run history in `trigger_run_history`); (2) Orchestrator schedules in `orch_schedules` (`ironclaw_orchestrator/src/schedules.rs:32-63`, bound to a `thread_id` or `new_thread=1`, with `webhook_token`/`signature`). | `POST/PUT/DELETE /api/webchat/v2/schedules*` (`descriptors.rs:233-239`), `t3_schedule` tool, trigger API. | **Caution — double-firing risk.** Trigger/schedule definitions are portable *except* thread-bound `orch_schedules` (the bound `thread_id` only exists on the origin host) and `trigger_poller.enabled` (having both machines fire the same cron schedule duplicates work). | 1–50 KiB / Low |
| **Usage, budgets & limits** | Default USD ceilings in `config.toml` `[budget]` (`config_file.rs:477-510`). Per-account runtime overrides: `/tenants/<t>/users/<u>/resources/*` (`ironclaw_resources/src/resource_store.rs:405-406`). Subscription rate-limit snapshots: `~/.ironclaw/usage/<runtime_id>.json` (`claude/mod.rs:125-127`, `muse/mod.rs:97-99`, `ironclaw_usage/src/`). | `budget_set` tool, CLI, automatic writes per CLI turn. | **Split.** `[budget]` & `/resources` policy ceilings are portable; `~/.ironclaw/usage/*.json` and spend ledgers are live runtime telemetry (**do not sync**). | 1–20 KiB / Med |
| **Machine identity & remote access** | `/tenants/<t>/shared/environment-access/*` (`setup_transfer.rs:117-122`): `environment_id` UUID, pairing code hashes, paired device token hashes (`icd_*`), `peer_links` records. Also `~/.zlo/webui-token`, `~/.zlo/.env`, `config.toml` `[remote]`. | `zlo remote`, `zlo pair`, `zlo devices`, `zlo environment link`. | **NEVER sync.** Machine-unique security boundary. | < 10 KiB / Rare |
| **macOS-only & desktop-app-local state** | Computer use (`crates/lanes/ironclaw_computer_use`, macOS Accessibility/Screen Recording); Rune app files (`tools/ironclaw_rune/app/src/persist.rs:1-15`): `prefs.json`, drafts, window frame, voice (`app/src/voice/`), SnapShots (`app/src/snapshot/`), `environments.json` & macOS Keychain device tokens (`remote-computers.md:297`). | Rune desktop app only. | **NEVER sync to Linux backend.** Rune already owns these locally on the Mac. | 5–50 KiB / Med |
| **Threads, runs, events, history** | `orch_threads`, `orch_turns`, `orch_tasks`, `orch_queue`, `orch_inbox`, `orch_steers`, `orch_worktrees`, `orch_telemetry`, `/events/*`, `/tenants/*/threads/*`, CLI transcripts (`~/.claude/projects`, `~/.ironclaw/{codex,antigravity,muse-code}/transcripts`). | Agents, users, scheduler. | **NEVER sync** (explicit requirement; repo rule: LLM data is never deleted). | 10 MiB–GiBs / Very high |

### 1.2 CLI Manager Deep Dive (`crates/domains/ironclaw_cli_providers`)

Understanding what each coding-agent CLI receives at spawn time explains *why*
syncing ZLO's own stores is almost sufficient — and exactly what still lives in
each CLI's native home directory.

**Shared ZLO-injected context** (built per turn from `orch_profiles` + user skill
store + `SkillUsageDocument` in `ironclaw_assistant/src/orchestration/profile_context.rs:58-84`
and `ironclaw_cli_providers/src/lib.rs:70-127`):
- **Profile `SOUL.md`** (`orch_profiles.settings.soul`)
- **Compiled ZLO notes**: `DELEGATION_PROMPT` (`ironclaw_orchestrator::tools`),
  `REPLY_VISUALS_PROMPT` (`ironclaw_host_api::reply_visuals`), and active thread goal (`lib.rs:46-48,89-98`)
- **Profile skills**: mirrored from DB skill store (minus `hidden_skills`) into
  `~/.ironclaw/cli-skills/<profile>/` as a Claude plugin folder (`profile_context.rs:74-103`),
  filtered/ranked by the shared `SkillUsageDocument` (`/skill-usage/listing.json`,
  `skill_listing.rs:1-17`)
- **Loopback MCP server**: ephemeral per-turn bearer token minted by `CliBridge`
  pointing at the local backend's `/api/mcp` for `t3_*` orchestrator tools (`bridge.rs`)
- **Tool approvals**: every CLI tool call is gated through `CliBridge` using the
  profile's `approval_mode` and global auto-approve setting (`permissions.rs`)

| Runtime | How ZLO passes context | What is read from the machine's own CLI home | Cannot be synced (manual on box) |
|---|---|---|---|
| **Claude CLI** (`claude-cli`, `claude-cli-2`) | `--append-system-prompt` (`SOUL.md` + delegation + visuals + goal, `claude/mod.rs:192-196,463-465`); `--plugin-dir ~/.ironclaw/cli-skills/<profile>` (`467-469`); `--settings <json>` with `skillOverrides` from shared usage ledger (`447-449`, `claude/skills.rs:5-20`); `--mcp-config <0600 file>` + `--strict-mcp-config` (`432-438`, blocks Claude's own MCP servers so only ZLO's loopback MCP loads); `--disallowedTools` for built-in subagents & artifacts (`446,490-502`); `--effort` / `--model` / `--autocompact` (`454-462`). | `CLAUDE_CONFIG_DIR` unset (`~/.claude`) or `~/.claude-account2` (`claude/mod.rs:475-478,1312-1354`): `.claude.json` (`oauthAccount` login + read-only `skillUsage` merge, `claude/skills.rs:12-14`), `~/.claude/CLAUDE.md`, `~/.claude/skills/`, `~/.claude/settings.json` (except overridden keys), `~/.claude/projects/` transcripts. | `claude` binary (`2.1.328`), OAuth login (`claude /login` → Keychain / `.claude.json`), optional second login (`CLAUDE_CONFIG_DIR=~/.claude-account2 claude /login`). Note: user-global `~/.claude/CLAUDE.md` and `~/.claude/skills/` are outside `~/.zlo` (see §5 Open Question 4). |
| **Codex CLI** (`codex`) | `codex app-server --disable multi_agent --disable multi_agent_v2 --listen stdio://` (`codex/mod.rs:470-487`). Strips `OPENAI_API_KEY` & `CODEX_API_KEY` (`480-481`). First turn of process prepends `session_preamble` (`SOUL.md` + delegation + visuals + skill index with `SKILL.md` paths) inline via `Primed` (`lib.rs:103-159`, `codex/mod.rs:174`). `thread/start` passes `cwd`, `approvalPolicy`, `sandbox`, `model` (`1044-1066`). State in `~/.ironclaw/codex/{threads,sessions,transcripts}` (`118-139`). | `CODEX_HOME` or `~/.codex` (`codex/mod.rs:1410-1428`): `auth.json` (ChatGPT login), `config.toml`, `AGENTS.md`, skills. | `codex` binary (`0.166.0`), `codex login` (`~/.codex/auth.json`). |
| **Antigravity** (`antigravity`) | `agy acp --gemini-dir <profile> --app-data-dir antigravity-acp --disable-telemetry` (`antigravity/launch.rs:65-77`), `env_clear()` keeping only 10 allowlisted vars (`29-43,80-85`). Session notes (`SOUL.md` + delegation + visuals + skill index + `ANTIGRAVITY_TOOLS_PROMPT`) + goal sent via `_meta.antigravity.systemPrompt.append` when supported, else inline on first turn (`antigravity/context.rs:80-123`, `antigravity/mod.rs:1249`). Loopback MCP passed in ACP `mcpServers` (`context.rs:167-175`). | Profile dir: `~/.hermes/plugin-data/t3code/antigravity/profile` if present, else `~/.ironclaw/antigravity/profile` (`antigravity/mod.rs:1933-1940`). `link_user_skills` symlinks 5 entries from `~/.gemini/` into the profile: `config/skills`, `antigravity-cli/skills`, `config/skills.json`, `AGENTS.md`, `GEMINI.md` (`antigravity/launch.rs:217-244`). | `agy` binary (`1.5.11`), Google OAuth sign-in (`<profile>/antigravity-acp/acp_token.json` / keyring, `launch.rs:5`, `mod.rs:1970`). |
| **Muse Code** (`muse-code`) | `muse serve --disable-request-user-input` with `promptUnmatched` + `:ask-me` (`muse/mod.rs:11-24,62-70,212-220`). `Primed` prepends `session_preamble` (`SOUL.md` + delegation + visuals + skill index) (`154,182`). Session MCP off by default (`orchestrator_tools: false`, `102-106`). State in `~/.ironclaw/muse-code` (`1471-1487`). | Muse's own login & host config. | `muse` binary (`1.4.24`), Muse sign-in. |

**Key takeaway from §1.2:** Because ZLO's CLI manager derives `SOUL.md`, skills
(plugin dir + index + usage ranking), approval mode, and ZLO MCP config from
ZLO's own database (`orch_profiles`, `/projects/.../skills`, `/skill-usage/listing.json`,
`/approvals/*`), **syncing ZLO's own configuration domains automatically brings
all four CLI runtimes into parity** — with the single caveat of any extra
user-global CLI files (`~/.claude/CLAUDE.md`, `~/.gemini/AGENTS.md`, etc.) the
person placed outside `~/.zlo`.

---

## 2. What Already Exists

### 2.1 `zlo export` / `zlo import` / `zlo transfer`

Implemented in `crates/app/ironclaw_cli/src/setup_bundle/` (`export.rs`,
`import.rs`, `format.rs`, `paths.rs`, `ssh.rs`) and
`crates/app/ironclaw_composition/src/setup_transfer.rs`.

- **Archive format** (`format.rs:1-56`): STREAM-encrypted AES-256-GCM in 1 MiB
  chunks, PBKDF2-HMAC-SHA256 (600k iterations), authenticated header AAD,
  trailing `manifest.json` with per-item SHA-256.
- **Included** (`setup_bundle/export.rs:249-311`):
  - Consistent SQLite snapshot (`VACUUM INTO`) of every `<storage>/reborn-local-dev.db`,
    with `/tenants/*/shared/environment-access*` deleted and re-vacuumed
    (`setup_transfer.rs:126-176`).
  - The raw secrets master key (`read_standalone_secret_master_key`,
    `setup_transfer.rs:425-471`) stored as `ItemKind::SecretsMasterKey` and
    installed as 0600 `.reborn-local-dev-secrets-master-key` on import (`import.rs:478-483`).
  - Every other regular file under `~/.zlo/` (`config.toml`, `providers.json`,
    `SYSTEM.md`, `/system/extensions`, `/system/skills`, workspace files).
- **Excluded** (`setup_bundle/export.rs:55-98,313-322`): `logs/`, `update/`,
  `bin/`, `webui-token`, `cache/`, `tmp/`, `.env*`, `retired-*`, top-level
  `*.pid`/`*.lock`/`*.sock`, WAL/SHM files, symlinks, environment identity &
  paired devices, `~/.ironclaw/*` CLI state, and project source repos.
- **Path rewriting** (`setup_bundle/paths.rs:13-74`, `setup_transfer.rs:65-72,269-309`):
  `HomePathRewrite` maps source `data_folder` → target `data_folder`, then
  `source_home` → `target_home` on bounded values in top-level `.toml`/`.json`
  files and 6 SQLite columns (`orch_worktrees.{path,repo_root,source_dir}`,
  `orch_worktree_hook_approvals.repo_root`, `orch_session_imports.path`,
  `orch_profiles.settings`). `[remote]` in `config.toml` is preserved from the
  target (`import.rs:424-446`), and existing environment identity/devices are
  copied back via `ATTACH DATABASE` (`setup_transfer.rs:182-264`).
- **Why it cannot be reused as-is for continuous sync**:
  1. **Offline only:** `commands/import.rs:45-73,98-103` refuses if the backend
     service or loopback port is running.
  2. **Whole-database replacement:** `import.rs:320-347,529-534` moves the entire
     existing `~/.zlo` aside (`~/.zlo.before-import-<ts>`) and drops in the
     source's `reborn-local-dev.db` verbatim — wiping all of the Linux box's own
     threads, runs, events, and history from the live DB.
  3. **Passphrase interactive & SSH-coupled:** `zlo transfer` (`commands/transfer.rs:33-107`)
     requires SSH + a manual passphrase every run, and copies the raw master key
     (unnecessary and undesirable once both backends already have their own
     master keys).
  4. **No incremental/domain granularity or two-way merge:** Always transfers the
     entire DB including gigabytes of threads/events.

*Reusable pieces:* `HomePathRewrite` (`paths.rs:13-74`) and the classification of
machine-local `config.toml` sections (`[remote]`, `[webui]`, `[storage]`,
`[update]`, `[boot]`) are directly reusable.

### 2.2 Peer Links & Multi-Computer Rune Connections

- **Peer links** (`docs/internal/plans/2026-11-18-zlo-multi-machine.md`,
  `ironclaw_webui/src/peer_client.rs`, `peer_routes.rs`): Backend-to-backend
  HTTPS (or loopback) with a 30-day device token stored in
  `OperatorSecretValueStore` (`peer_link_token_<env_id>`), verified against
  `GET /.well-known/ironclaw/environment` (`peer_client.rs:97-135`). Currently
  exposes only `POST /api/peer/tools/{tool}` (orchestrator delegation tools) and
  `GET /api/peer/resources`. Access levels are `PeerLinkAccess` (`Observe`,
  `Operate`, `Full`), separate from `DeviceScope` (`device_scopes.rs:48-50`
  exempts `/api/peer/` from `DeviceScope` checks because peer routes enforce
  `PeerLinkAccess` directly).
- **Rune multi-computer `Peers` engine** (`docs/internal/plans/2026-11-26-zlo-two-backends.md`,
  `tools/ironclaw_rune/app/src/remote/peers.rs`): Keeps **every** paired
  computer (plus This Mac's backend) connected simultaneously with warm `Backend`
  clients, per-computer device tokens in the macOS Keychain, automatic route
  resolution (Tailscale HTTPS → private tailnet → SSH tunnel), and event-driven
  refresh (`peers::refresh`, no standing timers). A paired Mac device already
  holds `DeviceScope::SettingsManage` (default `DeviceScope::all()`,
  `environment_access.rs:128-142`) on the Linux box.

---

## 3. Design Options for Continuous Sync

### Option A: Desktop App (Rune) as Carrier over Operator/ProductSurface Routes

The Mac's Rune app already maintains warm authenticated connections to both
**This Mac** and the **Linux box** (`peers.rs`). Both backends expose two new
`ProductSurface`-backed routes under `/api/webchat/v2/operator/config-sync`:
1. `GET /api/webchat/v2/operator/config-sync/manifest` — returns the backend's
   per-domain content hashes (`sha256`), monotonic revision, and metadata
   (cheap: ~1 KiB JSON, ETag-friendly).
2. `POST /api/webchat/v2/operator/config-sync/export` — exports only the
   requested domains (those whose hash differs from the target's last applied
   hash) as a versioned `ConfigSnapshotV1` JSON (plus base64/binary blobs for
   skill/extension assets).
3. `POST /api/webchat/v2/operator/config-sync/apply` — atomically validates,
   path-rewrites (`HomePathRewrite`), and applies each domain through the
   owning domain services (never raw SQL over `orch_threads`/`/events`), returning
   a per-domain `ApplyReport` + any server-side merge deltas (e.g., memory docs
   created on the box).

- **End-to-end flow:** When the box connects (or when This Mac's manifest
  revision advances after a settings/profile/skill/memory edit, or on user
  "Sync now"), Rune fetches both manifests, diffs per-domain hashes, pulls only
  changed domains from the Mac, and POSTs them to the box. For two-way domains
  (memory, skill usage), Rune also pulls the box's newer items back to the Mac.
- **Code needed:**
  - `ironclaw_product_contracts`: new `config_sync` module (descriptors,
    `ConfigSyncManifest`, `ConfigSnapshotV1`, per-domain DTOs, `SyncDirection`).
  - `ironclaw_assistant`: `ConfigSyncService` implementing export/apply via
    existing `ProductSurface` / store ports (profiles, skills, skill-usage,
    memory, approvals, extensions, hooks, triggers/schedules, LLM/config,
    secrets).
  - `ironclaw_composition`: wire `ConfigSyncService` with `RebornHome`,
    `OperatorSecretValueStore`, `SecretStorePort`, and `Engine`.
  - `ironclaw_webui`: 3 descriptors in `webui_v2/descriptors.rs` under
    `operator/config-sync/*`, guarded by `DeviceScope::SettingsManage` (plus a
    new `DeviceScope::SecretsSync` or explicit opt-in for secret values; see
    below).
  - `tools/ironclaw_rune`: `remote/config_sync.rs` hooked into `peers.rs`
    (on-connect + post-mutation trigger + Settings → Computers UI).
- **Failure modes:** Mac desktop app closed → box doesn't receive updates until
  Mac wakes and Rune opens (acceptable: Mac is the author of config changes
  anyway, and Mac's backend only runs while Mac is awake). Partial apply failure
  → per-domain isolation ensures one bad domain (e.g., missing extension binary)
  does not block profiles/skills/memory; Rune surfaces per-domain error badge
  and retries with backoff.
- **Security:**
  - In transit: HTTPS over WireGuard (Tailscale) or SSH tunnel — same verified
    channel Rune already uses.
  - At rest: Each backend keeps its **own** secrets master key. Secrets are
    decrypted on export via `OperatorSecretValueStore::read` / `SecretStorePort`
    (only for allowlisted syncable handles) and re-encrypted on apply under the
    box's master key. Never writes plaintext secrets to disk on either machine.
  - Authorization: Requires `DeviceScope::SettingsManage` (and optionally a
    dedicated `DeviceScope::SecretsExport` or explicit per-computer opt-in so a
    read-only/operate-only paired device can never read or push secrets).
    Only the desktop app on the owner's Mac pushes to computers the owner
    explicitly enabled sync for.
- **Conflict handling:** Deterministic per-domain rules (§3.4): Mac-authoritative
  for configuration; timestamp + content-hash merge (with conflict copy, never
  data loss) for memory; additive max/sum merge for skill usage counters.
- **Effort:** Medium (~3–4 weeks across 5 small PRs).

### Option B: Backend-to-Backend over a Peer Link

The Linux box pulls (or the Mac pushes) the same `ConfigSnapshotV1` directly
between backends over `ironclaw_webui::peer_client::HttpPeerLinkService`
(`POST /api/peer/config-sync/*`).

- **End-to-end flow:** Requires the user to establish a peer link
  (`zlo environment link`) from the box to the Mac (or vice versa), with the
  Mac also running `zlo remote enable` (so the box can reach the Mac over
  Tailscale). A background poller on the box pulls manifests periodically or on
  Mac push notifications.
- **Code needed:** All the backend contract/service work of Option A, **plus**
  extending `PeerLinkAccess` (today `Full` gives agent delegation, not secret
  export — we would need a new `ConfigSync` grant), extending `HttpPeerLinkService`,
  adding a background sync scheduler in `ironclaw_assistant` / `composition`,
  and still adding Rune UI to configure and observe it.
- **Failure modes:** Today, a Mac laptop behind NAT without `zlo remote enable`
  cannot be reached by the Linux box (`remote-computers.md:85` — remote access
  on the Mac is opt-in and off by default). Peer links expire every 30 days
  (`environment_access.rs:27`). If the Mac sleeps, the box polls a dead URL.
- **Security:** Larger blast radius — a compromised Linux box holding a peer
  link token with config-read/secret-export access could pull all Mac secrets
  without the user's laptop app even being open.
- **Effort:** High (~5–6 weeks); strictly a superset of Option A's backend work
  plus peer-link auth, background scheduling, and bidirectional reachability.

### Option C: File-Level Sync of Selected Mounts (rsync / Syncthing / Taildrive)

Sync `~/.zlo/config.toml`, `~/.zlo/providers.json`, `~/.zlo/SYSTEM.md`,
`~/.zlo/local-dev/system/{skills,extensions}`, and/or SQLite files at the OS
filesystem layer.

- **Why this fails:** Almost all agent-shaping state (`orch_profiles`,
  `orch_settings`, `/projects/.../skills`, `/memory/*`, `/approvals/*`,
  `/system/extensions/.installations`, `/tenants/.../secrets/*`,
  `trigger_records`, `orch_schedules`) lives **inside `reborn-local-dev.db`**
  (`filesystem_assembly.rs:24-26,238-292`). You cannot file-sync individual
  tables or virtual mounts out of a live SQLite database without corrupting it
  or overwriting `orch_threads` and `/events`. File sync also cannot rewrite
  `/Users/…` → `/home/…` paths, re-encrypt secrets between different master
  keys, or strip machine-local `[remote]`/`[webui]` TOML sections.
- **Effort:** Low to hack together, impossible to make safe or complete.

### 3.4 Per-Domain Direction & Deletion Policy

| Domain | Direction | Conflict Rule | Deletion Propagation |
|---|---|---|---|
| **Config (`config.toml` portable subset)** | Mac → Box | Mac wins on synced keys; box keeps `[remote]`, `[webui]`, `[storage]`, `[update]`, `[boot]`. | Removing a key on Mac removes it on box (returns to compiled default). |
| **Orchestrator `orch_settings`** | Mac → Box | Mac wins (`model_picker`, `idle_compact_claude`, limits). | N/A (fixed schema with defaults). |
| **Profiles (`orch_profiles`)** | Mac → Box | Mac wins per `profile_id` (with `HomePathRewrite` on `working_folder`). | **Soft-archive**: archiving a profile on Mac sets `archived_at` on box (`profiles.rs:333-353`). Existing box threads keep resolving it. |
| **User & system skills** | Mac → Box (or 2-way if box agents learn skills) | Mac wins by default; if box has a skill created more recently than last sync, keep both or flag (default: Mac wins for edited skills, preserve box-only learned skills until explicitly deleted). | Explicit delete tombstone (`deleted_at` per skill name) removes from active skill store on box; does not delete past turns. |
| **Skill usage & pins (`/skill-usage/listing.json`)** | **Two-way merge** | Pins: last-write-wins by timestamp. Usage ledger: per-`(source, skill)` take `max(count)` / `max(last_used_at)` so usage on both machines accumulates cleanly. | Unpinning sets explicit `Auto` pin with timestamp. |
| **Native memory (`/memory/*`)** | **Two-way merge** | Per `(scope, relative_path)` compare `updated_at` & content SHA-256 against last-synced base hash. If only one side changed → fast-forward. If both changed → keep newer as primary and archive older via `.versions/` (never discard LLM-written memory). | Memory archive (`POST /memory/documents/archive`) propagates as archive; **never hard-delete**. |
| **Prompts (`SYSTEM.md`)** | Mac → Box | Mac wins. | Restoring default writes default seed. |
| **LLM catalog (`providers.json`, policies, task models)** | Mac → Box | Mac wins; skip `base_url` entries pointing to `127.0.0.1` unless explicitly allowed. | Removing a custom provider on Mac removes it from box's `providers.json`. |
| **Secrets (allowlisted keys & `op://` refs)** | Mac → Box (opt-in, default on when sync enabled) | Mac wins for synced handles (`llm.*`, `supermemory_api_key`, extension API keys). Box keeps its own `peer_link_token_*`, `webui-token`, and local OAuth sessions. | Deleting an allowlisted secret on Mac deletes that handle on box. |
| **Extensions & MCP servers** | Mac → Box | Mac wins for manifest/install state, hosted HTTP MCP configs, and extension settings. **Channel extensions** (Slack/Telegram) sync *disabled* by default on box unless user opts in, preventing duplicate bot polling. | Deactivating/removing an extension on Mac deactivates it on box. |
| **Hooks** | Mac → Box | Mac wins for hook definitions & self-authored rules; `orch_worktree_hook_approvals` path-rewritten. Never sync `hooks_predicate_invocations`. | Removing a hook definition removes it on box. |
| **Tool permissions & approvals** | Mac → Box | Mac wins for `/approvals/auto-approve` and per-tool overrides. Never sync `orch_cli_approvals`. | Clearing an override reverts to default. |
| **Automations (`trigger_records`, `orch_schedules`)** | Mac → Box (**paused by default on box**) | Sync definitions so they are visible/runnable manually, but set `enabled = false` on box (and skip thread-bound `orch_schedules` whose `thread_id` does not exist on box) unless the user explicitly designates the box as the automation runner. | Deleting/pausing on Mac pauses/archives on box. |
| **Budgets (`[budget]`, `/resources`)** | Mac → Box | Mac wins for policy ceilings; never sync spend counters. | Clearing an override reverts to `[budget]`. |
| **Identity, devices, peer links, CLI logins, macOS-only, threads/runs/events** | **Never synced** | N/A | N/A |

---

## 4. Recommendation

### 4.1 Recommended Architecture: Option A (Rune Carrier) over a Backend-Owned `ConfigSyncService`

Choose **Option A** — the Rune desktop app orchestrates sync between This Mac
and each paired computer over the existing authenticated HTTPS/SSH connections,
calling a backend-owned `ConfigSyncService` behind `ProductSurface`.

**Why Option A wins:**
1. **Works out of the box with today's topology:** The Mac laptop does *not*
   need `zlo remote enable` or inbound reachability from the Linux box; Rune
   already keeps both backends connected (`peers.rs`).
2. **Respects all repo invariants:** All reads/writes go through `ProductSurface`
   commands/views and owning domain services (no transport mutating stores,
   dual-backend libSQL + Postgres parity, no touching `orch_threads` or `/events`).
3. **Least-privilege security:** The Linux box never holds a credential that can
   pull secrets from the Mac. The Mac's Rune app initiates every sync using the
   owner's local session + the box's Keychain device token.
4. **Future-proof for Option B:** Because the snapshot format (`ConfigSnapshotV1`)
   and apply engine live in `ironclaw_product_contracts` + `ironclaw_assistant`,
   exposing the exact same service over `HttpPeerLinkService` later for headless
   server-to-server sync is a thin transport adapter.

### 4.2 Wire Shape (`ConfigSnapshotV1`)

All paths inside the snapshot are normalized with `$ZLO_HOME` and `$HOME` tokens
on export (reusing `HomePathRewrite` in reverse) and expanded to the target's
`RebornHome` + `HOME` on apply.

```json
{
  "schema_version": 1,
  "source_environment_id": "uuid-of-mac",
  "source_os": "macos",
  "generated_at": "2026-11-26T12:00:00Z",
  "domains": {
    "config": {
      "hash": "sha256:...",
      "revision": 42,
      "payload": { "policy": {}, "llm": {}, "budget": {}, "embeddings": {}, "memory": {}, "skills": {}, "drivers": {}, "runner": {}, "google": {} }
    },
    "orchestrator_settings": { "hash": "sha256:...", "revision": 7, "payload": { "...Settings..." } },
    "profiles": {
      "hash": "sha256:...",
      "revision": 19,
      "items": [
        { "profile_id": "default", "updated_at": 1732600000000, "archived_at": null, "settings": { "working_folder": "$HOME/src/app", "...": "..." } }
      ]
    },
    "skills": {
      "hash": "sha256:...",
      "items": [{ "name": "deploy", "sha256": "...", "files": [{ "path": "SKILL.md", "bytes_b64": "..." }] }],
      "tombstones": [{ "name": "old-skill", "deleted_at": 1732500000000 }]
    },
    "skill_usage": { "hash": "sha256:...", "document": { "...SkillUsageDocument..." } },
    "memory": {
      "hash": "sha256:...",
      "documents": [{ "scope": {}, "relative_path": "MEMORY.md", "updated_at": "...", "sha256": "...", "archived": false, "content_utf8": "..." }]
    },
    "prompts": { "hash": "sha256:...", "system_md": "..." },
    "llm_catalog": { "hash": "sha256:...", "providers_json": [], "model_policy": {}, "user_preference": {}, "task_models": {} },
    "approvals": { "hash": "sha256:...", "global_auto_approve": true, "tool_overrides": [] },
    "extensions": { "hash": "sha256:...", "installations": [], "settings": [], "packages": [] },
    "hooks": { "hash": "sha256:...", "definitions": [], "self_authored": [], "worktree_approvals": [] },
    "secrets": {
      "hash": "sha256:...",
      "entries": [{ "scope": "system", "handle": "openai_api_key", "kind": "value_or_op_ref", "value": "..." }]
    }
  }
}
```

`GET /api/webchat/v2/operator/config-sync/manifest` returns only
`{ schema_version, environment_id, domains: { <domain>: { hash, updated_at, item_count } } }`.
Unchanged domains are omitted from `export` and `apply`.
For `secrets`, the backend **never** includes secret values unless the request
sets `"include_secrets": true` AND the caller passes a `DeviceScope::SettingsManage`
+ `SecretSyncPolicy::Enabled` check (and `apply` returns only counts/handles,
never echoes secret values).

### 4.3 Phased Implementation Plan

Each phase is a self-contained PR with dual-backend parity and `tests/integration/`
coverage:

- **Phase 1 — Contracts, Path Normalizer & Core Config Domains (Config, Settings, Prompts, Approvals, Profiles)**
  - Crates: `ironclaw_config` (extract portable-section merge helper),
    `ironclaw_product_contracts::config_sync` (DTOs + `ProductView` / `ProductSurfaceCommandDescriptor`),
    `ironclaw_assistant::config_sync` (`ConfigSyncService`),
    `ironclaw_composition` (wire service + `HomePathRewrite` moved from CLI to shared helper),
    `ironclaw_webui` (`GET /manifest`, `POST /export`, `POST /apply` under `/api/webchat/v2/operator/config-sync/*`, guarded by `DeviceScope::SettingsManage`).
  - Tests: `tests/integration/` driving two in-process runtimes — verify profiles
    (with `/Users/a/src` → `/home/b/src`), `SYSTEM.md`, `orch_settings`,
    `config.toml` (preserving target `[remote]`/`[webui]`), and approval settings
    sync cleanly while **box's `orch_threads` and `/events` remain untouched**
    (asserted at DB seam).
- **Phase 2 — Skills, Skill Usage Ledger & Two-Way Memory Merge**
  - Crates: `ironclaw_skills` (export/import bundle + ledger merge helper),
    `ironclaw_memory` (list-with-hashes + three-way merge with `.versions/` conflict preservation),
    `ironclaw_assistant::config_sync`.
  - Tests: `tests/integration/` — two-way memory edits on both runtimes produce
    zero data loss (conflict archived in `.versions/`); skill usage counts merge
    monotonically; `CliProfileContext` on target rebuilds `~/.ironclaw/cli-skills/<profile>/`
    automatically on next CLI turn.
- **Phase 3 — LLM Catalog & Secrets (Re-encrypted per Host)**
  - Crates: `ironclaw_llm` (`providers.json` merge), `ironclaw_secrets` /
    `ironclaw_composition::operator_secret_store` (allowlist filter excluding
    `peer_link_token_*` and leases; `op://` refs copied as-is; values decrypted
    and re-encrypted under target's master key).
  - Tests: `tests/integration/` — two runtimes with *different* master keys sync
    an API key and an `op://` reference; verify target decrypts with its own key
    and `peer_link_token_*` is never exported.
- **Phase 4 — Extensions, MCP Servers, Hooks & Automations (Safe-Disabled Defaults)**
  - Crates: `ironclaw_extension_host`, `ironclaw_hooks`, `ironclaw_triggers`,
    `ironclaw_orchestrator` (schedules), `ironclaw_assistant::config_sync`.
  - Tests: `tests/integration/` — hosted HTTP MCP extension syncs and activates
    on target; channel extension (e.g., Slack) and cron triggers sync in
    *paused/disabled* state on target so they do not double-fire.
- **Phase 5 — Rune Desktop App Carrier & Settings UI**
  - Crates: `tools/ironclaw_rune/client` (typed `config_sync` endpoints),
    `tools/ironclaw_rune/app/src/remote/config_sync.rs` (hooks into `peers.rs`
    on peer ready + post-settings-mutation debounce; respects energy rules — no
    standing timer; stores per-computer sync prefs in `environments.json`).
  - UI in **Settings → Computers → \<Computer\>**:
    - Toggle: **Sync configuration from This Mac** (default off on existing
      computers, offered on newly paired computers).
    - Status line: `Synced 2m ago` / `Syncing…` / `Paused` / `Error: <domain>` +
      **Sync now** button.
    - Per-domain checkboxes (Settings & Profiles, Skills & Usage, Memory,
      Providers & Keys, Extensions & MCP, Hooks, Automations) so the person can
      exclude any domain (e.g., Secrets or Automations) per computer.
  - Tests: Rune unit tests over two `test_gateway` instances + headless scenes
    (`computers-sync-idle`, `computers-sync-error`).

### 4.4 What the Person Must Still Do by Hand on the Linux Box

1. **Tailscale** (installed, signed in to the same tailnet, `zlo remote enable`).
2. **Install & sign in to coding-agent CLIs** they plan to use on the box:
   - `claude` (`claude /login`, plus optional `CLAUDE_CONFIG_DIR=~/.claude-account2 claude /login`)
   - `codex` (`codex login`)
   - `agy` (Google sign-in into Antigravity profile)
   - `muse` (Muse sign-in)
3. **1Password CLI (`op`)** — only if any synced secrets use `op://` references
   (`OP_SERVICE_ACCOUNT_TOKEN` or Connect on the Linux box, `reference.rs:6-10`).
4. **Clone git repositories** referenced by profile `working_folder`s (after
   `$HOME` rewrite, the UI can list any missing folders just like `zlo import`
   does at `setup_bundle/import.rs:540-552`).

---

## 5. Open Questions

1. **Secret sync opt-in & scope:** Should syncing plaintext API keys over the
   paired HTTPS channel be covered by existing `DeviceScope::SettingsManage`
   (already granted to paired Macs), or gated behind a new `DeviceScope::SecretsSync`
   plus an explicit confirmation toggle in Settings → Computers?
2. **Two-way vs. one-way for profiles & skills:** If the person edits a profile
   or an agent learns a new skill while focused on the Linux box, should Rune
   sync it *back* to the Mac (true two-way with last-write-wins), or should the
   Mac remain the single source of truth for config while only memory + skill
   usage merge two-way?
3. **Active channels & cron automations:** When Slack/Telegram or scheduled
   triggers exist on the Mac, should they sync to the box *paused* (recommended,
   prevents double replies/fires) or should there be a single "Primary runner
   for automations & channels" selector in Settings → Computers?
4. **External CLI-home files (`~/.claude/CLAUDE.md`, `~/.claude/skills/`,
   `~/.gemini/AGENTS.md`, `~/.codex/AGENTS.md`):** These live outside `~/.zlo`
   and outside ZLO's skill store. Should v1 leave them out of scope (recommending
   moving skills/rules into ZLO profiles/skills so all CLIs share them), or add
   an explicit optional "CLI home rules/skills" sync domain?
5. **Local-only LLM endpoints (`ollama` / `localhost`):** When `providers.json`
   or `[embeddings]` points to `http://127.0.0.1:11434`, should sync copy it
   verbatim (assuming the box also runs Ollama) or skip loopback `base_url`s
   with a warning?
6. **Hot-reload of `config.toml` after apply:** Today `config.toml` is read at
   boot (`ironclaw_config/src/config_file.rs:3-4`), with partial live reload in
   `commands/serve_reload.rs`. Which synced `config.toml` sections (e.g.,
   `[embeddings]`, `[memory]`, `[runner]`) require a backend restart on the box
   vs. hot-reload, and should Rune offer a one-click "Restart Linux backend"
   banner when those sections change?

## 6. Decisions (2026-11-26)

The open questions above, settled:

1. **Secret scope.** `DeviceScope::SettingsManage` plus the per-computer
   sync switch; no new scope. Provider and extension keys sync by default
   when sync is on ("Providers & keys" can be unticked per computer).
2. **Direction.** This Mac is the source for config, settings, profiles,
   skills, prompts, providers, approvals, extensions and hooks. Items that
   exist only on the other computer (a skill an agent wrote there, a profile
   made there) are kept, never deleted by sync; on a conflict the Mac's
   version wins. Memory and the skill-usage ledger merge both ways.
3. **Channels and automations** arrive paused on the other computer. A
   "primary runner" selector is a later change.
4. **CLI-home rules.** A sixth phase adds an optional `cli_home` domain: the
   person's own rules and skills outside ZLO's stores (`~/.claude/CLAUDE.md`,
   `~/.claude/skills/`, `~/.codex/AGENTS.md`, `~/.gemini/AGENTS.md` and the
   Antigravity equivalents), files only, never logins, tokens or settings
   files that can hold them.
5. **Loopback endpoints** (`127.0.0.1`, `localhost`) in the provider catalog
   or `[embeddings]` are skipped, and the sync status says which.
6. **Restart.** `apply` reports `restart_required` with the sections that
   need it; the app shows one action, "Restart IronClaw on <computer>", and
   never restarts a backend by itself.
7. **Default.** Sync is on for a paired computer unless the person turns it
   off (Settings → Computers), because the box is meant to mirror the Mac.
8. **Never synced**, as listed in 3.4: identity, devices, peer links, CLI
   logins, macOS-only state, threads, runs, events and history.

## 7. As built (2026-11-26)

Phases 1, 2 and 5 are in. What differs from the sketch above:

- **Carrier.** The app compares the two manifests and moves only domains
  whose hashes differ (`zlo_client::config_sync::sync_once`). It checks when
  a computer connects, when the window comes back, when a page is left and
  on "Sync now"; there is no timer. Snapshots pass through the app as JSON
  it does not interpret.
- **Pages.** `skills` and `memory` are *itemized*: exported alone, one page
  at a time (about 2 MiB of content), after the receiver's index
  (`index_only`), with content only for items the receiver lacks (`have`,
  `after`). No request exceeds the 4 MiB snapshot limit. An item larger
  than a page is not carried and is reported.
- **Two-way.** `memory` and `skill_usage` get a return leg (target to
  source) with `merge_back`, which only merges.
  - Skill usage: uses merge (larger count, later use); pins mirror the Mac.
  - Memory: three-way against a per-peer base kept under
    `/system/settings/config-sync/memory-base/`. A document changed on one
    side moves; one removed on a side stays removed there; one changed on
    both is settled once (the longer wins when it continues the other,
    otherwise the Mac's text takes the path and the other is kept under
    `sync-conflicts/<hash>/…` on both). Archiving does not propagate yet.
- **Skills.** One-way, whole bundles, Mac wins; skills only the target has
  are kept; system skills are never carried.
- **Settled differences.** A domain that differs by design (the target
  keeps items of its own) is remembered by its pair of hashes for the app
  session and not carried again until either side changes.
- **Failure isolation.** A domain a backend cannot read is left out of its
  manifest; a domain whose store fails on apply is reported as skipped and
  the others still apply.

Left: phase 3 (provider catalog and keys), phase 4 (extensions, hooks,
automations), phase 6 (CLI home rules files).
