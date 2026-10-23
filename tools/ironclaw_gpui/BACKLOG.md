# IronClaw Desktop backlog

Open items found while building and testing the GPUI client. Newest first.

## Desktop app follow-ups (23.10.2026)

- Package as a macOS `.app` bundle. GPUI logs "system notifications disabled:
  not running from an app bundle", so native notifications for approvals and
  finished runs need the bundle.
- Icon-only buttons (new conversation, delete, send, run now) have no
  accessibility label yet; rows, nav items and the remove buttons on
  Extensions/Skills do.
- Resolved approval cards stay in the transcript (greyed out). Consider
  collapsing them into a one-line "Approved"/"Declined" note.
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
Approve and Decline were tested live and work. "Always allow" is untested.

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

## Desktop app follow-ups found 23.10.2026

- After Approve, the approval card keeps saying "Approval required" until the
  thread reloads. It should switch to an approved/declined state.
- After a gateway restart the app waits 30 s before reconnecting. Retry sooner
  (for example 1, 2, 5, 10, 30 s).

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
