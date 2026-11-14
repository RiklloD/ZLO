# Reborn Operator Effective Config Contract

Issue: #4593

This document pins the WebUI v2 operator config surface that follows `GET /api/webchat/v2/operator/config`, `GET /api/webchat/v2/operator/config/{key}`, `POST /api/webchat/v2/operator/config/{key}`, and `POST /api/webchat/v2/operator/config/validate`.

## Supported first slice

The first implementation slice should expose the LLM default route through the existing typed LLM config service. The operator config facade must not bypass config layering or write TOML/secrets directly.

| Key | Read | Write | Value shape | Source metadata | Redaction |
| --- | --- | --- | --- | --- | --- |
| `provider.default` | yes | yes | string provider id or `null` | active LLM selection | not redacted |
| `model.default` | yes | yes | string model id or `null` | active LLM selection | not redacted |
| `provider.api_key` | yes | yes when active provider accepts API keys | redacted secret state | secret store or env metadata only | value always serialized as `null` |

## ProductSurface mutation path

Operator config reads are descriptor-backed ProductSurface query views:
`operator_config_list` for the full projection and `operator_config_key` for a
single key. The legacy `list_operator_config` and `get_operator_config_key`
methods are compatibility wrappers over those same query paths; ProductSurface
descriptors are the canonical API.

The global approval toggle `agent.auto_approve_tools` is supported as a boolean
operator config key. Its WebUI route still returns the typed operator config
projection, but the write itself must dispatch through `ProductSurface::invoke`
using the API-only first-party capability
`builtin.operator_config_set_auto_approve`, then read back the authoritative
tenant/user-scoped setting.

## Live reload of hand-edited `config.toml`

Operator-config writes above already take effect live (`set_active` writes
`[llm.default]`, then hot-swaps the provider). Re-selecting the provider the
slot already names keeps the slot's hand-set overrides (`base_url`,
`api_key_env`); switching provider resets them to the new provider's catalog
defaults. A selection the live swap refuses is not kept: `config.toml` is put
back (only while it still holds what the save wrote) and the save answers
`409 conflict` with `field: "llm.default"` — the file never names a provider
the running server is not serving. The whole save — snapshot, write, apply,
conditional restore — is one transaction, `LiveLlmDefault::write_and_apply`,
under the same lock SIGHUP reloads and the boot apply take (lock order: the
live-LLM lock, then the config file update lock inside it), and the
before/written texts are captured under the file lock by the write itself. A hand edit of `config.toml` is
applied by `ironclaw_operator::RebornConfigReloader::reload`, which `zlo serve`
runs on SIGHUP (`kill -HUP <pid>`) when stdin is not a terminal (the installed
service). Started from a terminal, SIGHUP keeps its default and stops the
server, so closing the terminal or an SSH drop still ends it.

- The file is re-parsed with the boot parser (inline-secret rejection, field
  validation), and a retired setup key is rejected as boot would reject it. A
  file that fails leaves the previous configuration fully in effect; the
  report's reason is a fixed category plus byte offset — never the parser's
  message, which can quote values, field names or enum variants.
- Changed keys are classified by `ironclaw_config::config_key_reloadability`.
  Only `llm.default` is live; it is applied by building the new provider chain
  first and swapping only on success, with the API key still resolved through
  its env var or the encrypted secret store. Clearing `[llm.default]` is
  restart-required (the swap seam cannot unwire a provider).
- `llm.default` is diffed against what was last applied by any path (boot,
  Settings, login, an earlier reload) — one `ironclaw_operator::LiveLlmDefault`
  owns that record and a lock spanning file read, resolution, key read and
  swap, so Settings saves and reloads never interleave and a hand edit that
  reverts a Settings change is applied.
- Every other changed key is reported as restart-required and never applied;
  the baseline is the snapshot `serve` booted from (never re-read), keeping its boot value, so it is reported on every
  reload until the process restarts. Unknown future sections default to
  restart-required.
- Reports carry key names only and are logged at `debug` on target
  `ironclaw::reborn::config_reload`, which the operator Logs panel captures by
  default.
- Provider swaps take effect per model call: a call already in flight finishes
  on the old provider; a multi-call turn that is mid-run picks up the new
  provider on its next call (same semantics as the Settings → Inference path).

## Precedence

Responses should include precedence metadata in this order:

1. Bootstrap environment and immutable process config.
2. Operator config file/default slot.
3. Secret store or provider credential environment metadata.
4. Provider catalog defaults.

The API should report effective values after this precedence has been applied. It must not collapse bootstrap config, DB-backed settings, and encrypted secrets into a single untyped layer.

## Required behavior

- `list` returns every supported key with `key`, `value`, `source`, `redacted`, and `mutable`.
- `get` returns one supported key and returns a loud error for unknown keys.
- `set provider.default` delegates to `LlmConfigService::set_active`.
- `set model.default` delegates to `LlmConfigService::set_active` using the active provider id.
- `set provider.api_key` delegates to `LlmConfigService::upsert_provider` using the active provider metadata and never echoes the submitted secret.
- `validate` returns an empty diagnostics list for supported keys that are currently writable.
- Unsupported, deprecated, immutable, missing-active-provider, and secret-not-supported keys return stable diagnostics.

## Key validation constraints

All operator config keys must satisfy the boundary-level validation rules:

- Must not be empty.
- Must not exceed 128 bytes. The limit is byte-based, matching the handler boundary check.
- Must not be the reserved word `validate`.
- Must contain only ASCII lowercase letters, digits, underscores (`_`), dots (`.`), or hyphens (`-`).

## Stable diagnostic reason codes

Existing reason codes remain stable:

- `operator_config_service_not_wired`
- `operator_config_secret_not_wired`
- `operator_config_deprecated`
- `operator_config_immutable`
- `operator_config_not_wired`
- `operator_config_unknown_key`

The LLM-backed slice may additionally return:

- `operator_config_missing_active_provider`
- `operator_config_secret_not_supported`

## Test requirements

Implementation PRs must drive the caller path, not only helper functions:

- Service-facade tests for list, get, set, and validate using a fake `LlmConfigService`.
- Router/handler tests proving key validation and secret-bearing bodies do not echo raw values.
- Redaction tests for `provider.api_key` serialization.
- Failure-path tests for unknown keys, malformed values, missing active provider, and providers that do not accept API keys.
- Precedence assertions on list responses.
