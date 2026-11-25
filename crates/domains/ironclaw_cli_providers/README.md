# ironclaw_cli_providers

Coding-agent CLIs as `ironclaw_orchestrator` turn runtimes: the Claude CLI,
registered as `claude-cli` (default account) when a `claude` binary is
found, plus `claude-cli-2` (`CLAUDE_CONFIG_DIR=~/.claude-account2`) only when
a different account is signed in there (opt-in:
`CLAUDE_CONFIG_DIR=~/.claude-account2 claude /login`, then restart); `antigravity` (Google's Antigravity agent over ACP), `muse-code` (Meta's Muse Code)
and `codex` (OpenAI's Codex CLI over `codex app-server`, on the user's own
`codex login`), each registered when installed
(`register_antigravity_runtime`, `register_muse_runtime`,
`register_codex_runtime`). Codex runs in the `workspace-write` sandbox with
the `untrusted` approval policy behind the bridge (read-only and never
asking without one); each approval is answered `accept` or `decline`, never
for the session.

Codex (checked against codex-cli `0.166.0`, `codex::EXPECTED_CLI_VERSION`)
also streams what its app-server reports while a turn runs: reply and
reasoning-summary deltas (the thinking row), tool rows with a bounded,
throttled, display-redacted tail of command output, and the turn's
aggregated diff, all as live-only bridge state (`LiveEvent::ToolOutput`,
`TurnDiff`, `Review`; projected as `cli_detail` items). The durable journal
keeps only the user prompt and the answer, as before. Runtime-native
operations go through `TurnRuntime::thread_action`: a review
(`review/start`, run as a turn whose findings are the answer; refused
unless the client's repo is the session's workspace root, read from the
per-turn records in `<state>/codex/sessions/<session>.jsonl`) and
compaction (`thread/compact/start`, now or after 4.5 idle minutes like
Claude CLI threads when idle compaction is on). One claim per thread admits
a turn, an action or a compaction at a time; a turn's teardown is a drop
guard, so a panicking or dropped turn task still frees the session and is
reported failed. Reverting a turn in Codex's session (`thread/revert`) is a
follow-up: see the orchestrator README.

- **Family / layer:** `domains` / `substrates` · **Package:** `ironclaw_cli_providers`
- **Use this when:** you change how a CLI agent runs a turn: process spawn,
  stream-json parsing, session resume, Stop, usage-limit mapping, telemetry.
- **Don't use this when:** you change orchestration semantics
  (`ironclaw_orchestrator`) or IronClaw's own loop runtime
  (`ironclaw_assistant::orchestration`).

## How a turn runs

One `claude -p --output-format stream-json --input-format stream-json
--verbose --include-partial-messages` process per turn; stdin stays open
for the control protocol until the result arrives. The thread's
`native_session_id` is minted at creation; the first turn passes
`--session-id`, later turns `--resume`. Stop sends SIGTERM to the process
group, SIGKILL after 2 s. A rejected `rate_limit_event`, the same reading
repeated on a blocked assistant frame (`api_error_params.rate_limit_info`,
whose reset counts only while still ahead), or a limit-shaped error result
becomes `usage_limit_resets_at`, and the error names the limit
(`usage_limit: the Claude 5-hour limit is reached`; weekly, weekly Opus,
weekly Sonnet, extra-usage). History reads the CLI's own session journal. The CLI child dies with the backend, so
`reports_restart_cuts` stays off and the engine's live-work rows cover cuts.

Wire types come from `claude-codes`, pinned to the checked CLI release
(`claude::EXPECTED_CLI_VERSION`, `2.1.328`); a different installed version
is logged at `debug`.

`ZLO_CLAUDE_AUTOCOMPACT` (tokens, 100000 to 1000000) is passed as
`--autocompact`: the CLI compacts at about that much context instead of its
default. It does not change the model's window; out-of-range values are
ignored. Idle compaction (below) is separate.

### Steering

With the bridge, a message sent while a turn runs is written into its open
stdin as a user message with `priority: "next"`: Claude reads it once the
tool calls in flight finish, in the same turn. A call still waiting for
approval is declined instead, the oldest one with the person's whole text
as its PreToolUse deny reason (Claude hands it to the model as the call's
result) and any others plainly; a question Claude asked is answered with
the text as typed. No steer frame follows once that answer is on the CLI's
stdin (the hook writer reports it through a `bridge::Receipt`); if it never
got there, the words go as a steer instead, so they arrive exactly once. A steer sent before the prompt is
written (the CLI has not acknowledged the hook registration yet) is held
and written right after the prompt, in order, never as a later turn; one
the turn never writes is reported unread. `claude/steer.rs` follows each command
written (the prompt and every steer, by uuid) through the CLI's
`command_lifecycle` frames (`queued`, `started`, `completed`, or dropped as
`cancelled` / `discarded` / `refused`) and the `user_message_uuids` echoes:

- stdin closes only once every command is done, not at the first `result`,
  so the approval hook can still answer a steer's tool calls. A steer the
  CLI never acknowledges stops holding stdin 15 s after it began waiting
  for it: an absolute deadline, so unrelated output does not extend it.
  The 60 s hook-registration wait is absolute too. A steer the CLI did
  acknowledge keeps stdin open while Claude works on it (its next native
  turn streams), but 90 s of silence with no tool call or hook answer in
  flight (`steer::IDLE_TIMEOUT`) closes input: a CLI that queued a steer
  and never got to it no longer holds the turn on "Working". A hook that
  finishes wakes that wait; a tool call whose result never comes stops
  counting as work after 30 min. Only `started` or a uuid echo proves a
  steer read (Claude also completes commands it never started), and that
  evidence still counts in the exit grace after input closed. The prompt
  and held steers are written off the output loop, each write bounded by
  `control::WRITE_TIMEOUT`; recovery keeps the order steers were accepted in.
- a steer the CLI never started (dropped, Stop, a crash, the turn ending
  first) is handed back through `TurnEventSink::steers_unread` (with the
  run id and each steer's own uuid) before the turn ends; the engine queues
  it under an id derived from thread, run and steer, so a repeated report
  never queues it twice, and it is sent as a later turn (after a Stop, once
  the thread is released) instead of being lost. A queue write that still
  fails after two retries comes back as undelivered: the turn then ends
  failed (a Stop stays cancelled) with an error that quotes each message
  and says it was not delivered.

A CLI without lifecycle frames or echoes keeps the old behaviour: stdin
closes at the first result and every steer counts as read.

On Codex and Muse a steer meets the turn at a `SteerDoor`
(`steer_door.rs`), and every accepted steer joins one channel the turn's
own loop reads in order: while the turn is still starting (host spawning,
session opening, `turn/start` unanswered) it is accepted at once (held);
once the turn has an id (which also lets Stop interrupt it) the requester
waits for the loop's verdict. The loop sends steers while it keeps reading
the agent's events (completion, approvals), never once Stop has won, and
cuts the reply at each one, so the transcript reads: reply so far, steer,
what followed (Codex and Muse alike, under the run id). A steer answered is
delivered; one refused, or whose frame never reached the agent, comes back
(a held one through `steers_unread`); one unanswered within 15 s counts as
delivered while the turn runs, and once the turn has ended without
answering it is *unconfirmed*: kept in the run with a "Steer delivery not
confirmed" notice, never requeued (it may have been read). At pending
approvals the steer declines them first and waits for those replies to
reach the agent (neither protocol carries a message on a decline).

#### One input owner per turn

Every driver has exactly one writer to its agent's input, fed by one ordered
mailbox: Claude's `claude/input.rs` owns stdin (hook registration, prompt
and held steers, every hook and question answer, each steer, the close); the
RPC drivers' writer task in `rpc.rs` owns theirs, and Codex/Muse steers and
Stop's interrupt are queued under the turn door's lock. Every write is
bounded (30 s); one that fails closes input for good and the turn ends in
bounded time, result or not. The reader never waits on a write.

A steer is delivered only on evidence (the write completed and, where the
protocol answers, the answer came for this turn), refused on a definite
failure (requeued by the engine, once), and unconfirmed only when it was
written and no answer came before the turn ended (kept with a notice, never
requeued). Gate replies a steer answered carry a `bridge::Receipt`: the
steer waits for them, and after its bound withdraws only replies the driver
has not taken yet (they then go plain), so the words never arrive twice.

### Task lists

Claude CLI 2.1.3xx has no `TodoWrite` in headless runs; with
`CLAUDE_CODE_ENABLE_TODO_TOOLS=1` (always set) it offers `TaskCreate`,
`TaskUpdate`, `TaskList` and `TaskGet`, deferred behind `ToolSearch`. The
hook allows those, and a `ToolSearch` that only loads them
(`select:TaskCreate,...`), without a gate: they touch nothing but the
session's task store, `<config>/tasks/<session>/<id>.json`. That is made
true, not assumed: the CLI's resolver prefers `CLAUDE_CODE_TASK_LIST_ID`
and a team's list over the session id, so every child gets
`CLAUDE_CODE_TASK_LIST_ID=<session>` and loses inherited team/list routing
(`CLAUDE_INTERNAL_SESSION_TASK_LIST_ID`, `CLAUDE_INTERNAL_ASSISTANT_TEAM_NAME`,
`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`, `CLAUDE_CODE_TEAMMATE_COMMAND`,
`CLAUDE_CODE_TEAM_TEARDOWN_PARK_TIMEOUT_MS`). After each successful call
the driver re-reads that store into the thread's plan (task order,
`pending` / `in_progress` / `completed`), so the list carries across turns.
The read (`claude/tasks.rs`) never follows a link (the store folder must be
a real directory; entries open `O_NOFOLLOW | O_NONBLOCK` and must be the
regular file listed, so a FIFO cannot hang it), skips entries over 64 KiB,
looks at 512 entries and 1 MiB at most, and logs any failure other than a
missing store at `debug`. Overlapping refreshes carry a generation; only
the newest publishes. `TodoWrite` is still handled for older CLIs. Any
other `ToolSearch` asks as before.

## Tools and approvals

With a `bridge::CliBridge` (wired by composition next to the engine), the
CLI has its tools and every call waits for a person:

- Before the prompt, the driver sends a stream-json `initialize` control
  request that registers a PreToolUse hook (matcher: every tool, MCP tools
  included). The prompt goes out only after the CLI acknowledges it; a
  refusal or no ack within 60 s fails the turn.
- Each `hook_callback` becomes a pending gate on the bridge. The desktop app
  shows it as a normal approval in the thread's stream (`gate.resolve`
  answers it, owner only). The driver answers the CLI with an explicit
  `allow` or `deny`, never an error frame: on an error the CLI falls back to
  its own rules, which auto-approve reads. Timeout (10 min), Stop, turn end
  and any failure deny.
- The CLI's own agents and teams are off: `--disallowedTools
  Agent,Task,Workflow,SendMessage,ListAgents,TaskStop`
  (`BUILTIN_SUBAGENT_TOOLS`), and the hook denies those tools outright as a
  backstop, even on a "Full access" profile. Subagents are orchestrator
  threads; ZLO's own Stop covers background shells.
- The CLI's artifact tools (`Artifact`, `ArtifactComments`, `ArtifactData`,
  `ArtifactCheck`, `DesignSync`) are disallowed too unless the account sets
  `ClaudeCliConfig::artifacts`, and their bundled skills go `off` (about 12k
  tokens a turn). They are not denied by the hook: the launch flag removes
  them from the model's tool list, and the setting is per account.
- Skills are listed by use, from the one skill usage ledger the harness and
  every CLI share (`ironclaw_skills::usage`, a per-user document on the
  persistence plane; `src/claude/skills.rs`): each turn's `init` frame
  records the account's catalog and its `Skill` calls count uses, merged
  read-only with `.claude.json`'s `skillUsage`. The profile plugin's
  `<plugin>:<name>` skills are kept under the bare `<name>` the harness and
  other CLIs use. The next turn passes `--settings {"skillOverrides":…}`:
  pins (set on the Skills page) decide first, profile plugin skills stay
  `on`, up to 12 skills scoring at least 0.5 (uses halved weekly, floor 0.1)
  stay `on`, the rest go `name-only` (still callable). A skill the turn's
  text asks for (`$name` or `/name`, the harness's mention grammar) is `on`
  whatever its pin. The pre-store state file
  (`~/.ironclaw/claude-skills/<runtime>.json`) is imported once and renamed
  `<runtime>.json.imported` only after the store took it (a failed import is
  retried next turn).
- Antigravity, Codex and Muse read the profile's skills index in their
  session notes (`src/skill_listing.rs`): name-only skills are listed by name
  and SKILL.md path, off ones are left out; a skill the turn's text asks for
  is listed in full and also named in that turn's context (the index reaches
  a session only once). A successful read of a listed SKILL.md counts one
  use: a typed read (ACP `read` call, Muse `read_file`), Codex's `read`
  command action, or a completed shell command that only reads the file
  (`cat`, `sed`, `head`, …, possibly inside `sh -c`); an `echo`, a failed
  read, or output naming the path does not. Every store call on the turn
  path is bounded (`SKILL_USAGE_ADVISORY_TIMEOUT`, 2 s); a stalled or failed
  store lists every skill in full and records nothing. Their own
  native skills (agy's `~/.gemini` skills, Codex's) are listed by the agent
  itself; ZLO has no knob for them.
- No allow rules are passed, "always allow" is not offered, prompts are
  answered "no" (`--permission-prompts none`), and
  `--dangerously-skip-permissions` is never used.
- The orchestrator tools (`t3_*`) come from one loopback MCP route
  (`POST /api/cli-bridge/mcp`, mounted by composition, 127.0.0.1 peers
  only). Each CLI process gets its own bearer token in a 0600
  `--mcp-config` file (`--strict-mcp-config` stays on); the token is bound
  to the thread and revoked when the turn ends. Calls run as the thread's
  owner scope.

Without a bridge every turn runs with `--tools ""` as before.

Every asked call is also recorded in the orchestrator's
`orch_cli_approvals` table (`register_cli_runtimes` points the bridge at
the engine's store): the tool, its preview, when it was asked, and how it
ended (`allowed`, `declined`, `timed_out`, `turn_ended`). A call that
cannot be recorded is denied. The live gate still dies with the backend
(so does the CLI process); the next engine open closes such rows as
`interrupted`.

While a turn runs, its reply text streams through the bridge into the
thread's event stream (`text`, `gate`, `run_status` projection items). The
prompt's `uuid` is the run id, so the session journal's rows carry it and
the durable reply replaces the live bubble.

Other drivers can reuse the bridge: `request_approval` for a tool call,
`turn_started`/`text_delta`/`turn_ended` for live state, `mint_token` +
`mcp_config_json` for the MCP route.

## Profiles and limits

A profile whose default provider is `claude-cli` or `claude-cli-2` runs its
threads here (`Engine::bind_profile_runtime`); the product surface answers
their submit, Stop and timeline from the engine. After each turn the CLI's
`unifiedWindows` limit reading is saved to `~/.ironclaw/usage/<runtime>.json`
for the Usage page.

## Telemetry

After each turn: context used (last assistant message: input + cache read +
cache creation), context max (`result.modelUsage[model].contextWindow`),
tokens per second over the streamed messages, and cache hit rate, reported
through `TurnEventSink::turn_telemetry` and served by
`GET /api/webchat/v2/threads/{thread_id}/telemetry`.

## Antigravity (`antigravity`)

Runs Google's Antigravity agent over ACP: the native `agy` CLI's
`agy acp` stdio server when the installed `agy` is 1.5 or later (found on
`PATH`, then `~/.local/bin`; checked against `launch::EXPECTED_CLI_VERSION`,
`1.5.11`), else the legacy one-file runtime (`agy_acp_server.par` beside
`localharness_external`, 1.3.0) where Hermes or T3 Code installed it
(`~/.hermes/plugin-data/t3code/antigravity-acp`, then
`~/.t3/tools/antigravity-acp`). The native launch is
`agy acp --gemini-dir <profile> --app-data-dir antigravity-acp
--disable-telemetry`, never `--dangerously-skip-permissions`. The protocol
is spoken directly over newline JSON-RPC (`src/rpc.rs`); the
`agent-client-protocol` crate is not used because it turns on
`serde_json`'s `preserve_order` for the whole workspace build.

- **Sign-in:** the profile is the one Hermes already signed in
  (`~/.hermes/plugin-data/t3code/antigravity/profile`) when it exists, else
  `~/.ironclaw/antigravity/profile`. Both launches keep the sign-in at
  `<profile>/antigravity-acp/acp_token.json`, so one sign-in serves either;
  `authenticated` is that file's presence. The agent inherits only an
  environment allowlist (`HOME`, `PATH`, locale, proxy and CA settings), so
  ambient cloud credentials and API keys never reach it, and `BROWSER` is
  `/usr/bin/true`: a missing sign-in fails the turn after `auth_timeout`
  instead of opening a browser. The user's global skills
  (`~/.gemini/config/skills`, `~/.gemini/antigravity-cli/skills`) are
  linked into the private profile; only a missing entry, an empty
  placeholder folder or a link to exactly that target is replaced (a link
  pointing elsewhere is the user's and stays), and nothing is changed when
  a folder on the way inside the profile is a symlink. Every folder ZLO
  creates in the profile is 0700, and ZLO's own profile and its `config`
  folder are tightened to 0700 (a shared Hermes profile keeps its mode);
  placeholders go with `remove_dir`, never a recursive removal. The links
  are made by path: another process of the same user racing them inside
  the private profile is outside the threat model.
- **Finding the agent:** `agy --version` runs asynchronously
  (`register_antigravity_runtime` and `register_cli_runtimes` are `async`)
  in its own process group, under one 5 s deadline for its first output
  line (4 KiB cap) and its exit; the group is killed afterwards, so a
  descendant holding stdout open neither blocks startup nor outlives it.
- **Admission:** one turn per thread, claimed atomically before any await
  (`slot.rs`, shared with Muse): a racing start is `Busy`, and the claim
  and a guard on the agent's process group live in the turn task, so a
  dropped, panicking or aborted turn still frees the thread and ends the
  agent; a report guard then tells the engine (`turn_finished`) and the
  bridge (`turn_ended`) the turn failed. The slot's cancellation flag
  exists from the claim on: a Stop while the turn still prepares sets it,
  the driver checks it before spawning and `fill` refuses a cancelled
  slot, so the agent is never launched (or is ended at once) and the turn
  is reported `Cancelled`; that Stop returns once the report is out.
- **Turns:** one agent process per turn: `initialize`, `authenticate`,
  `session/new` on the first turn (the agent's session id is kept in
  `~/.ironclaw/antigravity/sessions/<key>`), then `session/resume`. The
  thread's model and thinking level (`thought_level`, the catalog's
  `efforts`) are applied with `session/set_config_option`; a pre-1.5 model
  id such as `gemini-3.8-flash-high` is still passed through. The profile
  preamble, skills index and goal go in
  `_meta.antigravity.systemPrompt.append` on every `session/new` and
  `session/resume` when the agent offers it, so the prompt is the user's
  text verbatim and native commands (`/goal`, `/plan`, `/clear`) reach the
  agent; without it they go ahead of the prompt, except before a `/`
  command. A `/goal` run's `<!-- GOAL_COMPLETE -->` marker is dropped from
  the answer.
- **Steer:** on `agy acp` a message sent mid-turn is another
  `session/prompt` into the same session (`steer_in_turn`): the running
  prompt answers `end_turn`, the turn carries on under the new one, and the
  agent's tools and subagents are left alone. A steer is acknowledged only
  after its `session/prompt` frame is queued on the wire (frames go out in
  order), so steers sent at once all reach the agent. A steer while the
  ACP session is still opening is accepted at once and sent after the
  first prompt (or reported unread if the turn never gets there, or its
  frame never reached the agent); one at a pending permission rejects it
  first and waits for that reply to be written. A requester that gives up
  waiting (10 s) withdraws its request under a ticket the drive loop must
  take before sending, so a steer reported refused (and queued) is never
  also sent (`antigravity/steer.rs`). Once taken, the loop queues the
  prompt with no I/O in between, under the session lock Stop also takes
  for its cancel (no prompt goes out after a cancel), and the requester
  waits for the frame's actual write: a closed stdin refuses it. The superseded
  prompt's reply is still awaited: its `end_turn`/`cancelled` is ignored,
  but an error (a quota, a lost thinking signature, anything else) is kept
  and classified like the turn's own answer, so a later success does not
  mask it and a lost signature still retires the session pointer. The
  legacy runtime refuses (the engine queues); `SteerHow::Restart` stops
  and restarts.
- **Background commands:** `agy` ends a prompt while a command it
  backgrounded still runs and replies on its own when it finishes. The turn
  stays open until the command is done and that reply closes with a
  `usage_update` (or 60 s of silence with nothing running); a daemon
  (`IsDaemon`) never holds a turn, and an agent exit while held fails it.
  A hold is bounded: a command whose end the agent never reports, with no
  word from it for `background_hold` (30 minutes by default), ends the
  turn `Interrupted` with an "ended unverified" error, keeping what the
  agent said in the answer and the journal.
- **Stop:** `session/cancel` (which also ends `agy`'s background commands),
  then SIGTERM/SIGKILL of the process group after 2 s each.
- **Live turn:** reply text, thinking, tool rows with their output and
  Antigravity's own subagent calls (`invoke_subagent`, shown read-only as
  `subagent` rows) stream to the bridge; plan updates as before.
- **History:** IronClaw's own journal,
  `~/.ironclaw/antigravity/transcripts/<key>.jsonl` (user prompts and
  steers, answers, tool call names). When the agent has lost a thread's
  session, the fresh one gets a recap of that journal, fenced in
  `<prior_conversation>` tags and labeled untrusted data, not
  instructions; the tag inside the recapped text is defused
  (`prior-conversation`) so it cannot close the fence.
- **Models:** the list the last session reported
  (`~/.ironclaw/antigravity/models.json`, with the thinking levels), else
  Hermes's cache in the shared profile, else `default`. A signed-in catalog
  read refreshes a list older than an hour in the background
  (`session/new` + `session/close`, no prompt).
- **Limits:** a quota-shaped error (`RESOURCE_EXHAUSTED`, "quota", "rate
  limit") or a `refusal` stop becomes a `usage_limit:` failure;
  `quotaResetTimeStamp`, `quotaResetDelay` or `retryDelay` in the error
  sets `usage_limit_resets_at`. A model rejecting a saved thinking block
  (lost signature) retires the session pointer (renamed, never deleted), so
  the next turn starts a fresh session with the recap.
- **Telemetry:** context used and window from ACP `usage_update`; cache hit
  rate and TPS from `session/prompt`'s `usage` (`agy` 1.5 reports it; the
  1.3.0 runtime does not).
- **Tools:** the client offers no file system and no terminal, the session
  stays in the agent's ask-first `default` mode, and every
  `session/request_permission` goes through `permissions::decide`: with
  the bridge it waits as an approval in the thread and answers `allow_once`
  or `reject_once` (never `allow_always`); without, `reject_once`. A
  rejection's feedback steers the running turn on `agy acp`, else waits in
  the queue as the next turn's input. Native questions are cancelled
  without a bridge.
- **Not ported from T3 Code:** subscription usage windows (they need an
  HTTP call with the account's refresh token: `ironclaw_usage`'s job),
  reading `agy`'s SQLite session log for upstream errors reported as
  `end_turn`, detached subagent transcripts, managed downloads, and remote
  sign-in.

## Muse Code (`muse-code`)

Runs `muse serve` and speaks the Muse Session Protocol directly over the same
JSON-RPC module, mirroring the Hermes t3code driver and checked against Muse
Code `1.4.24` (`muse::EXPECTED_HOST_VERSION`). `muse-code-sdk` is not a
dependency: its facade has no `turn/interrupt`, `model/list` or
`usage/read`, and its spawn does not start a process group.

- **Turns:** one host per turn. The thread's `native_session_id` is a UUIDv7
  minted at creation; the first turn calls `session/start` with it, later
  turns `session/resume` (`excludeItems`). Commands carry UUIDv7 ids.
- **Live view (with a bridge, `muse/live.rs`):** reply and reasoning text
  (whole `item/*` snapshots and `item/delta` appends, only the unshown part
  sent), one tool row per tool item (`toolCall`, `userShell`, `subagent`,
  `workflow`; subject from the command, path, query or objective) with its
  streamed `visibleOutput`, and `session/todoListChanged` as the thread's
  plan. Muse's own housekeeping calls (`write_todos`, `request_user_input`,
  `workflow`) draw no row. The journal still keeps only prompt and answer.
- **Steer:** `turn/steer` with `expectedTurnId` into the running turn
  (`steer_in_turn`), through the shared `SteerDoor` (see Steering above);
  a refused steer is queued by the engine, `SteerHow::Restart` restarts.
  An idle compaction takes no steers (they queue).
- **Stop:** `turn/interrupt` for every turn Muse started on its own inside
  this one (a finished workflow) and then the admitted one; then
  SIGTERM/SIGKILL of the process group.
- **Fork:** no native fork; the engine's portable handoff starts a new
  session with the context (as t3code does).
- **Admission:** the same shared claim as Antigravity (`slot.rs`): one turn,
  compaction or probe per thread, taken before any await. A Stop before
  the host runs cancels the slot (the turn is reported `Cancelled`, a
  compaction answers `Busy`, and `muse serve` never launches), and a
  panicking or aborted turn task is reported failed.
- **Compaction:** Muse compacts near the window by itself
  (`RuntimeCompaction::automatic`); `ThreadAction::Compact` resumes the
  session on a host of its own, sends `session/compact` and waits up to
  5 minutes for the compaction item to settle (`noop` and a session with no
  turn yet answer "Nothing to compact yet."). The host holds the thread's
  turn slot meanwhile, so a turn is `Busy` and Stop ends it.
- **History:** IronClaw's journal,
  `~/.ironclaw/muse-code/transcripts/<session>.jsonl`, which also marks the
  session as started.
- **Models and sign-in:** `model/list` from a read-only host
  (`--no-session-log`), cached 10 minutes; MSP has no sign-in signal, so a
  non-empty catalog counts as authenticated, as in Hermes. Each model's
  reported `variants` become its `model_efforts` (known tiers only); a model
  that reports none, or an unlisted custom model id, takes
  `low`…`max`. A picked tier the model lacks is replaced by the model's
  `defaultReasoningEffort` before `turn/start`.
- **Sign-in only:** every inherited `META_API_KEY` (any spelling) is removed
  from the host's environment, so the `muse login` credential is used and no
  stray key bills another account. (t3code honors a key set on one provider
  instance; ZLO has no per-runtime environment, so it never passes one.)
- **Failures:** classes only, never the raw text (it carries request ids,
  paths and billing URLs): sign-in required; "does not exist or you lack
  access" (Muse lists models an account cannot use) names the model and
  asks for another; HTTP 402 / billing; otherwise `muse turn <terminal>
  (<kind>)`.
- **Limits:** a usage limit is `error.quota`, or a 429 whose text says
  quota or usage limit (credential proxies drop `quota`). It becomes a
  `usage_limit:` failure resuming at the latest future reset among
  `quota.resetsAtMs`, a "resets at <RFC 3339>" in the text, and the
  subscription windows at or over 100%. The `usage/read` / `usage/changed`
  windows are saved to `~/.ironclaw/usage/muse-code.json`
  (`{"observedAt", "usage"}`).
- **Telemetry:** context used and window from `session/contextUsage`
  (window falls back to the catalog's `contextLimit`), cache hit rate from
  the last `session/tokenUsage`, TPS as output tokens over the summed
  `durationMs` of the turn's model calls.
- **Tools:** `--disable-request-user-input`, no `--trust-workspace`
  (workspace rules cannot allow anything), approval mode `promptUnmatched`
  with a bridge (`denyUnmatched` without), and permission profile `:ask-me`
  on start, resume and compaction, because Muse's default profile may let
  its own AI reviewer decide a call. Every approval Muse raises goes through
  `permissions::decide`: with the bridge it waits for the owner and picks
  the once-scoped `approved` choice, else `denied` (or `abort`). Without a
  bridge, shell and writes are off too (`--disable-shell --disable-write`).
  t3code's "Full access" (`allowAll`, `:unrestricted`,
  `--disable-sandbox`) is deliberately not offered.
- **Orchestrator tools:** off by default (`MuseConfig::orchestrator_tools`).
  The MCP route does not gate calls itself, and that Muse asks before each
  MCP call under `:ask-me` is unverified live, so they stay off until a live
  canary shows it. When on, with a bridge whose MCP route is known, the turn
  asks for the `sessionMcp` capability and, when granted, names the route as
  an `optional` streamable-HTTP session MCP server (`ironclaw`, bearer token
  bound to the turn, revoked when it ends). Optional, because Muse fails a
  run whose required server is unreachable. Its calls are subject to the
  session's approval mode like any other.
- **Not ported:** update detection (t3code reads Meta's release channel
  over HTTP and drives the launcher's `MUSE_SYNC_UPDATE`; hosts here run
  with `MUSE_NO_AUTO_UPDATE=1`), retry rows (`turn/retryScheduled`),
  subagent child threads, and `view/gap` recovery (one host per turn).

## The approval seam

`permissions::decide` is the one function the Antigravity and Muse drivers
ask about a tool call. It turns the request into a bridge approval
(`CliBridge::request_approval`, the same gate the Claude driver uses) and
denies when there is no bridge. Each answer is for that one call: nothing
is remembered.

## Tests

`tests/claude_driver.rs` and `tests/agent_hosts.rs` run the drivers against
fake agent scripts. `tests/live_smoke.rs` holds ignored live canaries, one
short real turn per agent host:
`cargo test -p ironclaw_cli_providers --test live_smoke -- --ignored --nocapture`.

## Terminal session import (`session_import`)

Claude Code and Codex sessions the person ran in a terminal become ordinary
threads on `claude-cli` / `codex`, keyed by the CLI's native session id
(Goose's `session/import_formats/{claude_code,codex}.rs` in spirit).

- **Claude Code** (`~/.claude/projects/<dir>/<session>.jsonl`,
  `claude/import.rs`): only id, folder, title (`ai-title`/`custom-title`/
  `summary`, else the first prompt), times and a message count are read; the
  transcript stays the thread's history (`transcript::read`, now with each
  row's original time). `session_dir` resumes a session whose transcript
  lives in another project folder from the folder it recorded (`cwd`), so
  `claude --resume <id>` finds it.
- **Codex** (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`,
  `codex/import.rs`): the rollout folds into the driver's journal like a live
  turn (prompts; one reply per turn with `name()` tool labels; injected
  context blocks kept hidden), with `threads/<id>.id` (for `thread/resume`)
  and `threads/<id>.cwd` (the session's folder, read by `open`).
- **Skipped:** native ids already on a thread, Codex `originator: ironclaw`,
  subagent sessions, sessions without a prompt, anything under `~/.ironclaw`.
- **Incremental and crash-safe:** the ledger keeps a checkpoint per file:
  the offset after the last complete line, the file's identity (device and
  inode) and a digest of the 4 KiB before the offset. The same file with the
  same bytes up to the checkpoint is read from there; a replaced, truncated
  or in-place-rewritten file is read again from the start into a new
  generation and reconciled by message identity (role, time, visibility,
  text, tools) without dropping what was imported. Codex copies are written
  in bounded batches (256 messages, ending on a turn boundary); each line the
  import writes carries `"import": {"g", "e"}` (generation, source offset), so
  a batch written but not yet recorded is recognized on retry, and a torn
  last line is closed off before the next append. A new Codex import keys its
  session by the Codex thread id, so its saved id file is never mistaken for
  a session the app started, and an interrupted first import is retried.
  Once a turn ran here, the file is only noted as seen.
- **Files:** folders are walked without following symlinks; only regular
  files are opened (`O_NOFOLLOW | O_NONBLOCK`, then checked to be the listed
  file) and read up to their listed size (`session_import/files.rs`, 8 MiB
  line cap). Copies, saved ids and folder records are private (0700 folders,
  0600 files). A run stops at its limits (512 MiB read, 2 minutes, 50 000
  files per source) and counts the rest `deferred` for the next run.
- **Folder:** an imported session records its folder and that folder's
  identity (`workspace.rs`; Claude under `<work dir>-imports/`, Codex as
  `threads/<id>.cwd`). Both drivers check it before a turn: a missing or
  replaced folder fails the turn with a correctable error naming the folder,
  instead of resuming elsewhere.
- **Runs:** `SessionImporter::request` applies settings and starts a single
  background run when due (`Launch` once ever, `Background` every 15 min
  with the setting on, `Manual` always); `debug!` logging only.
