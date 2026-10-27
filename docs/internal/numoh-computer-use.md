# Numoh computer use: why it works now

Written 26.10.2026 from the Hermes and Claude CLI threads that built and debugged
computer use, the `computer-use.jsonl` call log, and the backend thread database.
Times are local (CET).

## The short version

Computer use failed because of a stack of separate faults, not one bug. Each fix
uncovered the next one. The agent went from "runs nothing" to "runs but is blind"
to "sees but re-reads the screen like a goldfish" to "works". The last two fixes
that made it usable landed in commit `c79ef63b3` (26.10 22:21):

1. The Codex provider (gpt-6.1-sol) now keeps the model's encrypted reasoning
   between tool calls. Before, the model re-planned from zero on every step.
2. A `get_app_state` that returns the same tree as the previous read now says
   "nothing changed, act or answer" instead of repeating the tree.

Hermes never had either problem. Its Codex adapter replays reasoning items, and
its plugin returns tree diffs. That is why the same engine, the same 11 tools and
the same model worked in Hermes and failed in Numoh.

## Before and after, from the call log

| Thread | When | Model | Calls | Clicks | Outcome |
|---|---|---|---|---|---|
| `34ef2c8b` | 26.10 12:49 | gpt-6.1-sol | 6 reads, 1 click | 1 | Stopped by the user after 47 s, never got the senders |
| `5b196042` | 26.10 18:05 | gpt-6.1-sol | 8 reads, 7 of them byte-identical | 0 | Gave up: "Stopped. I haven't verified." |
| `fb33e122` | 26.10 20:49 | glm-5.3-flash | 17 (1 list, 4 reads, 7 clicks, 5 queries) | 7 | Answered with a table of the last 5 sent mails and replies |
| `93f211b1` | 26.10 22:22 | gpt-6.1-sol, Full access | 16 (11 reads, 5 clicks) | 5 | Answered with a summary of the 5 newest inbox mails, no approval asked |

Before the fixes the agent read and never acted. After them it alternates read,
click, read until it has the answer.

## The timeline of faults

### Layer 1: nothing ran (24.10 to 26.10 morning)

Commit `e7dc55b07` (24.10) ported the Hermes `computer-use-plus` plugin's session
layer to Rust as `crates/lanes/ironclaw_computer_use`, driving the same patched
Open Computer Use engine (forked from iFurySt) over stdio MCP. It had never run
live. The first live attempts on 26.10 hit, in order:

| Fault | Cause | Fix |
|---|---|---|
| Computer use was off | It needed `COMPUTER_USE_ENABLED` and `COMPUTER_USE_BINARY`, which nothing set | The engine now ships inside the app and turns on by itself (see layer 2) |
| Port was incomplete | The fork's guidance prompt and `OPEN_COMPUTER_USE_ACTION_RESULT=diff` / `ACTION_SCREENSHOT` were never copied over | Added: `prompts/fork_guidance.md`, env switches in `session.rs` |
| Approve returned HTTP 503 | The policy had no lease terms for `computer_use.*` | `[approval_defaults.computer_use]` in `crates/app/ironclaw_composition/src/builtin_capability_policy.toml` |
| Always allow returned HTTP 400 | No rule accepted a permanent approval for these tools | `crates/app/ironclaw_composition/src/runtime/approval.rs` |
| GLM calls failed with "invalid arguments" | GLM fills optional fields with junk (`"text_limit": "500"`, `"window_id": 0`) | Arguments are repaired against each tool's schema (`src/args.rs`) |
| macOS asked for Screen Recording every 2 minutes | The backend was ad-hoc signed with a name that changed every build, so the grant never stuck | Signed with the Apple Development cert under `com.numoh.backend` |

### Layer 2: it ran but was blind (26.10 11:24 to 12:28)

The Outlook runs called `get_app_state` 9 times and stopped. Outlook's
accessibility tree lists inbox rows as empty `row` items. Sender and subject are
only in the screenshot, and the model never got the screenshot.

| Fault | Fix |
|---|---|
| IronClaw tool results were text only, so the screenshot was dropped | Screenshots now reach vision models |
| The Codex provider discarded every image with a "not yet implemented" warning | Fixed in `openai_codex_provider.rs` (this also fixed user-uploaded images) |
| The vision-model list stopped at `gpt-4o`, so GPT 5 and 6 counted as text only | List updated |
| The model got `{"ok": true}` and needed a second `result_read` call to see the tree | Up to 20 KB of tree goes inline in the first result (`inline_observation` in `crates/product/ironclaw_assistant/src/computer_use.rs`) |
| Three app identities (Numoh, `~/.ironclaw/bin/ironclaw`, the engine) confused macOS permissions | One `Numoh.app`: backend at `Contents/MacOS/numoh-backend`, engine release `engine-v0.6.0` at `Contents/Helpers/Numoh Computer Use.app`, both built by `tools/ironclaw_gpui/bundle.sh` |

A Calculator test confirmed the model read the picture. Outlook still failed.

### Layer 3: it saw but did not trust what it saw (26.10 12:47 to 13:13)

A side-by-side with Hermes on the same Outlook prompt showed three differences:

| | Hermes | Numoh then | Fix |
|---|---|---|---|
| Screenshot placement | Inside each tool result | One frame as a separate message after the last result. The model said "no image came back to me" while reading it | Each result carries its own screenshot, the newest 3 stay in context (`MODEL_FRAMES` in `session.rs`, `runtime/computer_use_images.rs`) |
| First step | Calls the tools directly | Called `tool_search` first | `prompts/tool_names.md` says the tools are loaded, and `prompt.rs` strips the engine's "use tool_search" sentence |
| Approvals | None per call | Every click, key and read waited for the user | One approval per task (`approved_turns` in `ironclaw_assistant/src/computer_use.rs`) |

These were built and installed, and committed in `87f5b9fc4` (26.10 19:13).
Computer use still failed: thread `5b196042` read the same screen 8 times.

### Layer 4: the model forgot its plan on every step (26.10 18:56 to 20:30)

This was the root cause behind the read loops. A mock-server probe against the
real `OpenAiCodexProvider` reproduced it:

- The provider asked OpenAI for `reasoning.encrypted_content`, then threw it
  away. The SSE parser only kept `function_call` items, and the next request
  sent only `message`, `function_call` and `function_call_output`.
- With `store: false`, OpenAI's guidance is to send reasoning items back with
  the function call so the model can continue its reasoning. Without them,
  every `get_app_state` result reached a model that had no memory of why it
  asked. It answered by asking again.
- The loop breaker did not catch it: it kills a run after 8 identical outputs,
  and `5b196042` died at 7.

Fixes, both in `c79ef63b3`:

| Fix | Where | How |
|---|---|---|
| Reasoning carry | `crates/domains/ironclaw_llm/src/openai_codex_provider.rs` (`attach_reasoning_items`, `reasoning_items_from_signature`, `send_with_reasoning_recovery`) | Reasoning items are stored on the tool call they belong to and replayed before that `function_call`, only for the same provider and model. If OpenAI rejects an old blob, the request retries once without it. If the secret scanner flags a blob, that step drops its reasoning instead of failing |
| Unchanged-state signal | `crates/lanes/ironclaw_computer_use/src/session.rs` (`STATE_UNCHANGED`) | A read identical to the previous one returns "no tree changes, indexes still current, act or answer". Any action resets it |

### Layer 5: the fixed agent could not reach a model (26.10 20:39 to 20:47)

The first retest showed "Unknown error" and no tool calls. Two causes, neither
in computer use:

| Fault | Fix |
|---|---|
| The ChatGPT plan behind gpt-6.1-sol was out of usage. IronClaw read the error code from the wrong field, called it "Unknown error" and retried for 2 minutes | `is_usage_limit_code` in `openai_codex_provider.rs`: reports "quota exhausted" with OpenAI's message and does not retry |
| The next test thread ran on Claude Opus through the Claude CLI runtime, which has no computer-use tools (`--strict-mcp-config` with the orchestrator server only) | Not fixed. Use an IronClaw-native model (see open gaps) |

### Final piece: Full access (26.10 22:21)

`c79ef63b3` also made a profile set to Full access skip the computer-use approval
entirely, using the thread's profile scope. A tool set to Disabled stays
disabled. The 22:22 run used it and asked nothing.

## Why Hermes worked all along

| Piece | Hermes | Numoh before | Numoh now |
|---|---|---|---|
| Engine and 11 tools | Patched Open Computer Use | Same | Same |
| Reasoning between steps | Replayed (`codex_responses_adapter.py`) | Discarded | Replayed |
| Action results | Tree diffs | Full tree | Tree diffs |
| Repeated reads | Deduplicated | Full identical tree | "Nothing changed" |
| Screenshots | Inside each result, 3 kept | Dropped, then one detached frame | Inside each result, 3 kept |
| Tool discovery | Tools listed up front | `tool_search` first | Listed up front |
| Approvals | None per call | Every call | Once per task, or never on Full access |

The answer was simple, as you said. Hermes is no smarter. It just doesn't
throw away the model's memory or its eyes.

## What to keep in mind

- These fixes were tested on two real Outlook tasks, not a test suite. The 22:22
  run still read the screen 4 times in a row once, so read loops are reduced,
  not gone.
- The reasoning carry only applies to the OpenAI Codex provider. The GLM run
  succeeded without it, by using `query` to read rows the tree hides, so the
  unchanged-state signal and the layer 1 to 3 fixes carry most of the load for
  other providers.

## Open gaps

| Gap | Effect | Option |
|---|---|---|
| Claude CLI and Muse Code threads have no computer-use tools | Opus says "not available" and falls back to AppleScript | Add the bundled engine as a second MCP server in the CLI's `--mcp-config` (about 20 lines in `bridge.rs`), with once-per-task approval for `mcp__computer_use__*` |
| Text-only models get no vision fallback | They depend on `query` and the tree; apps that only show content in pixels stay hard | Route computer-use threads to a vision model, or add a vision-describe step like Hermes's `vision_analyze` |
| Loop breaker needs 8 identical outputs | A looping run burns up to 8 model calls before it stops | Lower the threshold for `computer_use.*` in `crates/loop/ironclaw_agent_loop/src/strategies/progress.rs` if loops come back |

## Where to look when it breaks again

| Question | Look at |
|---|---|
| Which calls ran, how long, ok or not | `~/.ironclaw/logs/computer-use.jsonl` (one line per call, keyed by session = thread id) |
| Which model and profile a thread used | `orch_threads` in `~/.ironclaw/reborn/local-dev/reborn-local-dev.db` |
| What the model saw and said | `root_filesystem_entries`, paths under `.../threads/<thread id>/messages/` |
| Why a model call failed | `~/.ironclaw/reborn/logs/serve.stderr.log` |
| Crate behaviour | `crates/lanes/ironclaw_computer_use/README.md` |

The source threads: Hermes `local-9defcc43…` (port, layers 1 to 3),
`local-5608e0a4…` "Find ironclaw computer use failure patterns" (layers 4 and 5),
and the Claude CLI thread that added Full access (`c79ef63b3`).
