# IronClaw Desktop backlog

Open items found while building and testing the GPUI client. Newest first.

## Desktop app follow-ups (23.10.2026)

- Package as a macOS `.app` bundle. GPUI logs "system notifications disabled:
  not running from an app bundle", so native notifications for approvals and
  finished runs need the bundle.
- Icon-only buttons (new conversation, delete, send, run now) have no
  accessibility label yet; rows and nav items do.
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

Still to do:
- Decide whether our install keeps it on. Turning it off makes every shell,
  write and network call ask first.
- Surface the setting in the desktop app (settings page, phase 3) and show a
  warning while it is on.
- With it off, test the approval card end to end: approve once, always allow,
  decline.

## Shell `workdir: /workspace` fails (found 22.10.2026)

The agent's shell calls failed with "No such file or directory (os error 2)"
when it passed `workdir: /workspace`; it recovered by retrying with `.`.
Backend issue, not the app. Check how `builtin.shell` maps the virtual
`/workspace` path to the host directory.

## Projects are gated off

`/session` reports `features.reborn_projects: false` (env flag
`IRONCLAW_REBORN_PROJECTS`). The desktop Projects page only appears when the
gateway enables it.
