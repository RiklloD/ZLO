# ironclaw_orchestrator

The engine that owns every orchestrated thread: a Rust port of the t3code
Hermes plugin's orchestrator v2 (T3 Code's orchestration model). It holds the
delegation tree, the durable per-thread queue, turn admission for automatic
turns, forks and merge-back, Stop propagation, restart continuation and
usage-limit resume. Turns run on `TurnRuntime` implementations; IronClaw's
agent loop is the first (`ironclaw_assistant::orchestration`).

- **Family / layer:** `domains` / `substrates` · **Package:** `ironclaw_orchestrator`
- **Use this when:** you change orchestration semantics (tasks, queue, holds,
  forks, recovery) or add a runtime (stage 4 CLI drivers implement `TurnRuntime`).
- **Don't use this when:** you change transcripts (`ironclaw_threads`), turn
  admission inside one runtime (`ironclaw_turns`), or schedules
  (`ironclaw_triggers` routines: there is no second scheduler).

## Public surface

- `engine::Engine` — `open(store, default_runtime)`, `register_runtime`,
  `start(period)` (2 s tick), `tick`, `prepare_for_shutdown`, the product
  hooks `on_user_prompt` / `on_user_stop` / `ensure_thread` /
  `take_next_message_context`, and the operations behind the tools and
  routes (`delegate`, `task_status`, `task_list`, `cancel_task`, `queue_*`,
  `send`, `runs`, `fork`, `merge_back`, `lineage`, `capabilities`, `status`).
  It implements `runtime::TurnEventSink`.
- `runtime::TurnRuntime` — the runtime seam (create thread, start turn,
  steer, cancel, history, optional native fork, catalog) with
  `RuntimeCapabilities { native_fork, steer_in_turn, reports_usage,
  reports_restart_cuts }` and
  `TurnOutcome { status, final_text, error, usage: TurnUsage,
  usage_limit_resets_at, cut_by_restart }`.
- `tools::TOOLS` + `Engine::call_tool` — `t3_capabilities`, `t3_delegate`,
  `t3_task`, `t3_thread`, `t3_queue` schemas and dispatch.
- `store::Store` — the `orch_*` tables (ADR 0005).
- `handoff` — bounded portable context and merge-back deltas.
- `profiles` — stage 3 profile records (`ProfileOwner`, `ProfileRecord`,
  slug and settings validation) and their `Store` methods. Settings are the
  `ironclaw_product_contracts::profiles::ProfileSettings` wire shape.
- `schedules` + `schedule_webhooks` — `t3_schedule`: clock schedules fire
  on the engine tick through the host's `SchedulePlanner`; `{type:'webhook'}`
  schedules fire per request to their loopback URL
  (`/api/orchestrator/webhooks/<id>.<token>`, mounted by composition) with
  optional HMAC-SHA256 signatures, request placeholders in the prompt and
  the planner's unattended-prompt scan on the rendered text. A run posts
  into the schedule's thread, or into a fresh thread per run
  (`bind_to_current_thread=false`) on the same runtime, profile and model.
- `cli_approvals` — `orch_cli_approvals`, the durable record of CLI tool
  approvals the `ironclaw_cli_providers` bridge asks; open rows are closed
  as `interrupted` when the engine opens.
- `goals` — `orch_goals`; the active goal rides every turn
  (`TurnInput.goal`, and the profile SOUL context on IronClaw's loop).
- `worktree` — worktree isolation: `t3_delegate isolation='worktree'` (and
  `Engine::start_worktree` for a user thread) runs `git worktree add -b
  zlo/<slug>-<id>` from the thread folder's HEAD, records a
  `WorktreeBinding` in `orch_worktrees` and hands its path to every turn as
  `TurnInput.work_dir` (runtimes opt in with `supports_work_dir`; admission
  and route switches refuse runtimes that do not). A `shared` child of a
  worktree thread works in that worktree (`orch_worktree_shares`) without
  owning it. A child's task end settles it (clean and on its branch →
  removed with its branch, else kept); `t3_thread worktree_status |
  merge_worktree | remove_worktree` act on it. `.zlo/worktree.toml`
  `on_create` / `on_settle` hooks are snapshotted from the source checkout
  and run only once the person approved that exact content through the
  `HookApprover` gate (`orch_worktree_hook_trust`). Git (with repository
  hooks off) and the approved hooks run through the host's
  `HostCommandRunner` (`Engine::set_worktree_host`; composition wires the
  host process port and the CLI bridge as approver).

## Tables

`orch_threads` (thread registry: scope, nullable `profile_id`, runtime,
route, turn state), `orch_tasks`, `orch_queue`, `orch_holds`,
`orch_transfers`, `orch_recoveries`, `orch_live_work`, `orch_turns`
(per-turn status and usage), `orch_profiles` (per user: settings JSON,
`archived_at`; last use and thread count derive from `orch_threads`),
`orch_settings`, `orch_meta`, `orch_schedules`, `orch_inbox`, `orch_goals`,
`orch_cli_approvals`, `orch_worktrees` (append-only; a thread's newest row is
its binding), `orch_worktree_hook_trust`, `orch_worktree_shares`. First open imports
the legacy `ironclaw_threads` thread records once (marker
`legacy_threads_imported_v1`).

## Invariants

- Automatic turns start only on an idle, unheld thread, decided under the
  dispatch lock Stop also takes.
- A task reaches a terminal state exactly once (conditional updates).
- Stop and cancel write durable intent across the whole subtree before
  enforcing it.
- Restart continuation fires at most `MAX_CONTINUATION_CHAIN` (3) times in a
  row, never after a newer user message, a Stop, or six hours. It covers
  crashes only: a clean quit (`prepare_for_shutdown`, called from the
  runtime's shutdown) holds running threads as `interrupted`, their child
  tasks stay live, and nothing runs until the user releases the hold
  (Continue), which resumes the cut turn before the queue drains.

- Every subagent is an orchestrator thread (`t3_delegate`). Two kinds:
  `context: "inherit"` (a subagent on the caller's task: the parent's
  conversation rides ahead of the brief as a portable handoff, same profile)
  and `context: "brief"` (default: a side task, `target.profile` allowed).
  Runtimes switch off their agents' own subagent tools and add
  `tools::DELEGATION_PROMPT` (`prompts/delegation.md`) to the system prompt;
  IronClaw's loop (native API models) hides `builtin.spawn_subagent` on
  orchestrated turns and carries the same prompt in the turn's SOUL.md
  context.

- Worktree work is never lost: nothing runs with `--force` and nothing is
  committed for anyone; uncommitted changes block merge and removal, a HEAD
  off its branch is archived, a branch with unmerged commits is kept (plus
  `refs/zlo/archive/<branch>`), and a merge that does not finish is rolled
  back and verified against the saved HEAD. Lifecycle steps hold a
  reservation taken under the dispatch lock, so no turn starts in a
  worktree being merged or removed.

- A thread's profile is set once (`profile_id IS NULL` guard) and never
  changes; a thread without one runs as `default`. Delegated children inherit
  the parent's profile unless `target.profile` names another; forks keep it.
  Archiving a profile hides it from the list, its threads keep resolving it.

## Tests

```bash
cargo test -p ironclaw_orchestrator
cargo clippy -p ironclaw_orchestrator --all-targets -- -D warnings
```
