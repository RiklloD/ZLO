# ironclaw_supermemory

Supermemory (https://supermemory.ai) as IronClaw's memory for callers that
have it. With an API key in the secret store (`supermemory_api_key`) and a
container binding, Supermemory is the caller's only memory: automatic recall
before each turn, automatic capture after it, and the `supermemory.search` /
`supermemory.write` model tools, while the local `ironclaw.memory.*` recall
and tools are hidden for that caller. Without a key nothing here runs and
local `ironclaw_memory` works as before.

- **Family / layer:** `domains` / `substrates` · **Package:** `ironclaw_supermemory`
- **Use this when:** you change Supermemory recall, capture, container
  bindings, or the two tools' behaviour.
- **Don't use this when:** you change what memory means (`ironclaw_memory`),
  the local provider (`extensions/packages/memory-native`), or how the tools
  are registered and approval-gated (`ironclaw_assistant::supermemory`).

## Public surface

- `Supermemory` — `from_env(keys)`, `is_active`, `recall`, `capture`,
  `search`, `write` (primary container only, read back by id as evidence).
- `SupermemoryBindingSource` — the seam profiles plug into: which primary and
  read containers a caller uses. `StaticSupermemoryBindingSource` is the
  default, one global binding from `SUPERMEMORY_CONTAINER_TAG` (default
  `ironclaw`) and `SUPERMEMORY_READ_CONTAINERS` (comma-separated).
- `SupermemoryPromptContext` / `SupermemoryInteractionRecorder` — decorators
  over the `MemoryPromptContextService` and after-turn `MemoryService` seams;
  pass-throughs to local memory when Supermemory is off for the caller.
- `run_search` / `run_write`, schemas and ids for the tools.
- `SupermemoryConfig` — `SUPERMEMORY_BASE_URL`, `SUPERMEMORY_AUTO_RECALL`,
  `SUPERMEMORY_AUTO_CAPTURE`, `SUPERMEMORY_MAX_RECALL_RESULTS`.

## Behaviour (matches the Hermes `supermemory` plugin)

- Recall: `/v4/profile` on the primary container with the first 200
  characters of the user message (profile facts, recent context, relevant
  memories), plus a 3-hit `/v4/search` per read container; deduplicated,
  capped per section, `[3h ago] [87%]` annotations, untrusted-enveloped and
  credential-redacted like native memory snippets.
- Capture: after each completed turn, user and assistant text, injected
  `<supermemory-context>` blocks and inline base64 images removed, written as
  `[role: user]...[user:end]` with `customId` `<thread>_<date>_b<hour/4>` so a
  4-hour window appends to one document. Failed writes are retried with the
  next capture (bounded at 50 turns / 256 KiB).
- All HTTP goes through `ironclaw_network`'s policy egress (configured host
  only, public IPs only, 1 MiB response cap, 5 s timeout, 429 retried twice).
  The key is cached for 30 s and only ever sent in the `authorization` header.

## Tests

`cargo test -p ironclaw_supermemory` — hermetic: a scripted in-process egress
and an in-memory key store; no real API, no real secrets.
