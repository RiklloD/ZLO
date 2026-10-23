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

Local workaround applied: `mkdir -p
~/.ironclaw/reborn/workspace/tenants/reborn-cli/users/reborn-cli`. Verified:
`pwd` with `workdir: /workspace` now succeeds.

Proper fix (upstream-worthy): create the scoped directory before spawning in
`HostProcessPort::run_command`, with a regression test. Related: the output
rewrite uses the unscoped alias list, so `pwd` prints
`/workspace/tenants/reborn-cli[REDACTED_HOST_PATH]` instead of `/workspace`.

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
