# IronClaw Desktop backlog

Open items found while building and testing the GPUI client. Newest first.

## Closed 23.10.2026 (backlog pass)

- Service lifetime: the app loads the launchd job on open and unloads it on
  quit (`app/src/service.rs`); "Try again" on the connect screen starts it
  after a Stop. The gateway only stops fast once no client holds a stream:
  with the app still connected, `launchctl unload` took 5 s.
- macOS service status: `launchd_status` now reads the dictionary form of
  `launchctl list <label>` (fork fix, test
  `launchd_status_reads_the_dictionary_form`).
- Diagnostics: `DiagnosticSnapshot`, `PromptDiagnostic`, the update batch
  types and the three response wrappers derive `Deserialize`; the client
  decodes the contract types instead of a partial mirror.
- Quick-switch trap: setting the active model (Settings or provider save with
  "activate") now makes it the quick-switch default and clears the caller's
  pick, in the gateway (test
  `set_active_moves_the_quick_switch_default_to_the_new_model`). Web UI and
  CLI get the fix too.
- Per-conversation logs: each run executes inside a `turn_run` span carrying
  `thread_id` and `run_id`, so `/logs?thread_id=` finds its lines.
- Config validate: removed from the app; the route has no service behind it.
- Suggested tasks: removed from the app (not wanted). That also retires the
  stuck-spinner report and the empty "New conversation" threads.
- Thread titles: one cache (`app/src/titles.rs`) names threads everywhere.
  Threads missing from the list (automation runs) are fetched once: title,
  else the automation's name, else the first line of the first message.
  Memory > threads and notifications/alerts use it.

## Found during 4.2 (23.10.2026)

- Gateway trap: once a quick-switch model list exists for the active
  provider, chats run on that list's default or the person's pick, and the
  active model set in Settings is ignored. The desktop app's "Use" button
  updates the list default and clears the pick to stay consistent; the web UI
  and CLI do not. There is no route to delete the list. Worth an upstream
  issue.
- Once, after "Suggest tasks", the app kept showing the spinner although the
  gateway had finished. Not reproduced with logging on (polls arrived every
  second and the cards appeared). If it comes back, check whether the window
  repaints while another app is in front.
- The suggestion generator leaves an empty, untitled thread in the sidebar
  ("New conversation"). Gateway behavior; the web UI likely shows it too.

## Found during 4.4 Operator (23.10.2026)

- macOS service status always reads "stopped". `launchd_status` in
  `ironclaw_operator/src/operator_service_lifecycle.rs` parses the table form of
  `launchctl list`, but `launchctl list com.ironclaw.reborn` prints a dictionary
  (`"PID" = 4289;`). Upstream-worthy fix. The app treats "gateway answered" as
  running and says so on the service card.
- Stop from the app also unloads the launchd job (`launchctl unload -w`); the
  only way back is `ironclaw service start` in a terminal. Start/install are
  excluded. Follow-up: let the connect screen run that command locally.
- `DiagnosticSnapshot`, `PromptDiagnostic` and their response wrappers are
  Serialize-only, so the client decodes a partial shape. Adding Deserialize
  upstream removes that.
- Config validate always returns "the effective config service is not wired
  yet" on this install.
- Per-conversation logs are always empty: log lines carry the run id, not the
  conversation id.

## Native notifications (fixed 23.10.2026)

macOS refused notifications from the ad-hoc signed bundle ("Notifications are
not allowed for this application"). Fixed by signing with the user's Apple
Development certificate, which also needed Apple's WWDR G3 intermediate in the
login keychain, and turning IronClaw on in System Settings > Notifications.
`bundle.sh` now picks the Apple Development identity on its own.

Also learned: the gateway inbox records "run finished" only for background
runs (automations), and those items arrive already resolved. Chat runs are
detected from the live stream instead (`RunEnded` in `chat.rs`).

## Desktop app follow-ups (23.10.2026)

- Automation-run threads are not in the sidebar list (the gateway lists only
  session threads), so their notifications say "Conversation" instead of a
  title.
- Memory > threads in Files shows thread ids; map them to titles.

## Approval prompts never appear (found 22.10.2026, root cause 23.10.2026)

Shell commands, file writes and `rm -rf` in the workspace all ran without an
approval gate, so the desktop app's approval card was never exercised against
a live gateway.

Cause: the operator setting `agent.auto_approve_tools` is `true` with source
`default` (`GET /api/webchat/v2/operator/config`). The approval gate
(`crates/kernel/ironclaw_approvals/src/profile_gate.rs`, step 6) lets every
durable-approval-eligible tool through when it is on. `/session` reports it as
`features.global_auto_approve`. It is a setting, not a bug.

Status 23.10.2026: the Settings page has the switch and a warning while it is
on. It is currently OFF on our install (set during testing). With it off,
Approve and Decline were tested live and work. "Always allow" tested live
23.10.2026: it stores `tool.builtin.shell = always_allow` as an override
(reset to Default afterwards).

## Shell `workdir: /workspace` fails (found 22.10.2026, root cause 23.10.2026)

Cause: `HostProcessPort` scopes `/workspace` per caller to
`<workspace_root>/tenants/<tenant>/users/<user>`
(`crates/kernel/ironclaw_host_runtime/src/process_aliases.rs`,
`scoped_to_caller`), but nothing creates that directory on disk. The file
tools write through the DB-backed filesystem, so it never appears, and
`spawn` fails with ENOENT on the missing cwd.

Fixed 23.10.2026 in `process_port.rs`: `run_command` creates the caller's
scoped root (only the root, never a subdirectory the model names) before
spawning. The output rewrite now includes the scoped aliases, so `pwd` prints
`/workspace` instead of `/workspace/tenants/reborn-cli[REDACTED_HOST_PATH]`.
Regression test: `scoped_workspace_is_created_for_a_fresh_caller`. Verified
live with the folder deleted first. Upstream-worthy as a PR to nearai/ironclaw.

## Closed in phase 4.1 (23.10.2026)

- `.app` bundle with icon (`bundle.sh`), installed to `~/Applications`.
- Approval cards collapse to one line: Approved, Always allowed, Declined, or
  Answered (after a reload, since the projection does not carry the choice).
- Reconnect: the stream probes the gateway's TCP port once a second while it
  is down, and the connect screen retries 1, 2, 5, 10, 30 s. Measured: 4 s
  from gateway up to app reconnected (was up to 30 s).
- Accessibility labels on every icon-only button.
- "Open at login" switch in Settings (LaunchAgent, bundle only).

## Skills page limits (23.10.2026)

- The gateway serves content and the auto-activate toggle only for editable
  (user/installed) skills. Built-in skills return 404 / 400, so the app hides
  View and the switch for them, like the web UI does.
- No skill catalog is configured (`registry_url` is empty), so catalog search
  always returns nothing. The app says so.
- Install from catalog and remove are untested live because there is no
  catalog and no user skill yet.

## Projects are gated off

`/session` reports `features.reborn_projects: false` (env flag
`IRONCLAW_REBORN_PROJECTS`). The desktop Projects page only appears when the
gateway enables it.
