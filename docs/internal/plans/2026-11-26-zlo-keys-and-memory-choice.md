# ZLO: Keys page, Supermemory fix, memory choice at setup

Date: 2026-11-26. Asked by the user after the Memory page said "No
Supermemory API key is stored" although they had registered one.

## Root cause

- ZLO reads the key from the operator secret store under
  `supermemory_api_key` (`ironclaw_supermemory::SUPERMEMORY_API_KEY_SECRET`).
- Nothing in ZLO ever writes that handle. The app has no screen for it; the
  Memory page's "Add the key in Settings" points at nothing.
- The user's key lives in Hermes: `SUPERMEMORY_API_KEY` in ten
  `~/.hermes/profiles/*/.env` files (one value). The Hermes profile import
  (`ironclaw_assistant::orchestration::profiles::import_hermes`) copies each
  profile's `supermemory.json` container but never the key, so every
  imported profile has a container and no key.
- Second bug on the same page: the profile picker shows "Default" twice (the
  no-profile choice and the profile whose id is `default`).

## Decisions

1. **Settings → Keys** (Agent group, after Providers): one page for every key
   ZLO holds that is not a model-provider login. Rows:
   - *Services* — keys an owning crate declares (Supermemory now). Stored,
     replaced, removed; tested before storing; "Use the key from Hermes"
     when one is found there.
   - *Plugins* — installed plugins that need a token; set through the
     plugin's existing setup submit.
   - *Model providers* — status only, managed on Providers (one home per
     key).
   Supermemory is not turned into a plugin: it is a built-in memory backend
   wired below the extension layer; a plugin wrapper would add a lifecycle
   for no gain.
2. **Backend**: `ironclaw_product_contracts::service_keys` (DTOs + ports, no
   vendor names), descriptors and verifiers supplied by owning crates
   (`ironclaw_supermemory`), the `ServiceKeysSurface` decorator in
   `ironclaw_assistant`, wired in composition. Routes are operator-only
   (same gate as LLM provider keys): these are deployment-wide secrets.
   Set = verify with the provider first, store only on success (pre-validate
   pattern), then read back `contains`. A provider that cannot be reached
   stores the key as *unverified* and says so.
3. **Hermes import carries the key**: the explicit "Use the key from Hermes"
   action, and the existing Hermes profile import, store `supermemory_api_key`
   when ZLO has none. Never overwrites a stored key.
4. **Setup gets a Memory step** (Connect → Agents → Memory → Projects): pick
   Supermemory (paste or import the key; tested) or a local knowledge wiki.
   One of the two is required to continue; a step already satisfied (key
   stored, or wiki present) shows as done.
5. **Local knowledge wiki** = Karpathy's LLM wiki, as Hermes' `llm-wiki`
   skill does it, but inside ZLO local memory (`wiki/`), so the Memory page
   shows it, config sync carries it, and agents reach it through memory
   tools:
   - `memory.wiki.status` / `memory.wiki.init`: scaffold `wiki/SCHEMA.md`,
     `wiki/index.md`, `wiki/log.md` (templates in
     `ironclaw_assistant/prompts/memory_wiki/`); never overwrites.
   - Bundled runtime skill `skills/llm-wiki` (ported from Hermes, MIT),
     rewritten for ZLO memory tools.
   - CLI agents (Claude Code, Codex, Antigravity) get `memory_tree`,
     `memory_read`, `memory_search`, `memory_write` on the loopback MCP
     route, so they can maintain the wiki too.

## Out of scope (recorded)

- Supermemory auto-recall/capture for CLI-runtime turns (native loop only
  today).
- The parked "learns you" background review (`parked-memory-learning`).
- Config sync of keys (sync phase 3).
