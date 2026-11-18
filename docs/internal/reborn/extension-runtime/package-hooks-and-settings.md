# Package hooks and settings

Status: implemented 2026-11-18. User-facing word: **plugin**; code vocabulary
stays **extension**. A plugin is an extension package — one directory with a
`manifest.toml` (`schema_version = "reborn.extension_manifest.v3"`). This page
covers two package surfaces the host executes on the package's behalf:
`[[hooks]]` and `[[settings]]`.

## `[[hooks]]`

Each entry is the hook crate's own manifest vocabulary
(`ironclaw_hooks::manifest::HookManifestEntry`) plus one package-level key,
`module`:

```toml
[[hooks]]
id = "block-switch"                 # manifest-local, unique
kind = "before_capability"          # before_capability | before_prompt |
                                    # after_model | after_capability |
                                    # after_checkpoint | event_triggered
scope = "own_capabilities"          # default; same_tenant needs requires_grant
phase = "policy"                    # default; validation/authorization refused
priority = 0                        # optional
description = "…"                   # optional
requires_grant = "…"                # optional; default-deny until host-verified
module = "wasm/hook.wasm"           # required iff body.mode = "wasm"
body = { mode = "wasm", export = "evaluate" }
# or a declarative predicate (no module):
# body = { mode = "predicate", spec = { type = "deny_capability", reason = "…",
#          when = { type = "name_equals", name = "<ext>.<tool>" } } }
```

Parse (`ironclaw_extension_registry::v3_package_surfaces`): table shape,
non-empty unique `id`, count ≤ 32, entry ≤ 8 KiB, `module` a package-relative
asset path that cannot escape the root, and `module` present exactly when the
body is `wasm`. The hook vocabulary itself is validated by the hook registrar.

Lifecycle (`ironclaw_composition::observability::hooks::live`): the per-run
hook dispatcher factory reads the **active** extension registry — the one the
activation transaction publishes into and deactivation/removal clears — so a
package's hooks register from the next run after activation and are gone from
the next run after deactivation or removal. In-flight runs keep their
dispatcher. Third-party hooks still require `HOOKS_ENABLED` and
`HOOKS_THIRD_PARTY_ENABLED`.

Per-user visibility: the active registry is tenant-global, so each run
installs only the packages whose installation is visible to the run's user —
the same owner projection the tool catalog and the model-visible capability
surface join (`ironclaw_extension_manager::installation_visibility`). A
private install contributes to its members' runs only, a tenant-shared one to
everyone's, and an active package with no installation row to nobody's (fail
closed). An unreadable owner map fails the run's host build. The owner
projection is consent-filtered (`ExtensionLifecycleManager::installation_owners`):
a third-party package's hooks reach only the runs of users whose own consent
grant covers its files.

Boot: the boot-time projection carries trusted (builtin / host-bundled)
packages only. Third-party packages are never discovered from
`/system/extensions` for hooks at boot; the live installer is their only path.

Trust: every package hook installs through `HookRegistrar` at the
`Installed` tier — restricted sinks (no `Allow`), no `Validation`/
`Authorization` phases, grants default-deny, per-extension and per-tenant
caps. WASM modules are read from the package root (≤ 2 MiB) once per package
content identity (version, manifest digest, root) and held as immutable bytes
next to the validated entries; a run that admits the package installs from
those exact bytes through a registrar-scoped module resolver
(`HookRegistrar::with_module_resolver`), so a concurrent run pruning the
shared cache cannot invalidate them. They run in the hook WASM sandbox (fuel,
memory, wall clock; no filesystem, network, clock or RNG imports).

Consent binding: a third-party package contributes hooks only when it was
published with its consent grant's per-file pins (`ExtensionPackage::
content_pins`); without them its hooks are quarantined ("no consent grant
binds this package's files"). `live::read_hook_module` is the single read of
hook module bytes and verifies that exact buffer against the pinned hash
before it is cached or compiled; a mismatch quarantines the package's hooks
("tampered"), and the package reads `tampered` on the next lifecycle list.
The validation cache key includes the pins, so a new grant revalidates from
freshly read, re-verified bytes.

WASM hook ABI: `ic:hooks/context@1` `ctx_size`/`ctx_read` expose a JSON
context blob. When the package declares settings, the blob carries the acting
user's effective values as a `settings` object, bound once per run.

## `[[settings]]`

```toml
[[settings]]
id = "units"                 # [a-z0-9][a-z0-9_.-]{0,63}, unique
kind = "choice"              # switch | choice | text | number
label = "Units"
description = "Temperature units"   # optional
section = "Display"                 # optional
default = "metric"                  # must fit kind (and options for choice)
options = [                         # choice only, 1..=64, unique values
  { value = "metric", label = "Metric" },
  { value = "imperial", label = "Imperial", description = "Fahrenheit" },
]
```

Values: `switch` → boolean, `number` → JSON number, `text` → string ≤ 2 KiB
without control characters (newline/tab allowed), `choice` → one listed
option value. Settings are never secrets: an id or label that looks like a
credential slot (`token`, `password`, `secret`, `api_key`, …) is refused at
parse with a pointer at `[[tools.credentials]]` + `[auth.<vendor>]`.

Storage (`ironclaw_extension_registry::settings::ExtensionSettingsStore`):
per user scope, one record per extension under the `/extension-settings`
mount, written with the bounded CAS helper; only explicitly set values are
stored. Effective value = stored value if it still fits the current
declaration, else the default (an upgrade that reshapes a setting degrades to
its new default).

Delivery to the plugin — no ambient state:

- WASM tools: the `context` JSON of the WIT `execute` request carries
  `"settings": { … }` next to `capability_id` (`ironclaw_host_runtime` WASM
  lane, via the `ExtensionSettingsReader` port). A read failure fails the
  invocation rather than running on defaults.
- WASM hooks: the context blob's `settings` object (above). On a read
  failure the policy is per hook kind: a WASM gate (`before_capability`) or
  mutator (`before_prompt`) fails the run's host build rather than being
  skipped (fail closed); after-fact observers are skipped for that run only
  (observer failure isolation), with a `hook.skipped_for_run` event on the
  `security_audit` tracing target, and the package's other hooks install.

Product face (`ironclaw_product_contracts::extension_settings`, answered by
`ironclaw_assistant::extension_settings_page`, sourced from
`ironclaw_extension_manager::extension_settings_product_service`):

- `GET /api/webchat/v2/extensions/{extension_id}/settings` →
  `{ "settings": [ { "id", "kind", "label", "description", "section": null|"…",
  "options": [ { "value", "label", "description": null|"…" } ], "default": <json>,
  "value": <json> } ] }`
- `PUT /api/webchat/v2/extensions/{extension_id}/settings/{setting_id}` body
  `{ "value": <json> }` → the updated setting object. A value that does not
  fit answers 400 with the standard validation body (`field: "value"`,
  `validation_code: "invalid_value"`) plus `"reason": "<plain text>"`; an
  unknown setting or inactive extension answers 404.
- Both operations first check the installation owner for the caller: an
  installation the caller may not see (another user's private plugin, or an
  active package with no installation row) answers 404, indistinguishable
  from an unknown extension.

## Known limits

- A `[[tools]]` entry with no `origin_gate_matrix` parses, but the approval
  gate treats a missing matrix as `Forbidden` for every origin
  (`ironclaw_approvals::profile_gate_policy`), so the tool is refused with
  `PolicyDenied` on every model call. Plugin authors must declare it (e.g.
  `origin_gate_matrix = { loop_run = "gated_unless_granted", product =
  "forbidden", automation = "forbidden" }`). Rejecting or defaulting a
  missing matrix for third-party manifests is the install/consent lifecycle's
  decision.

- The schema is read from the active package; an installed-but-inactive
  extension's settings are not listed (404).
- Removal clears the removing user's stored values
  (`ExtensionSettingsCleaner::clear_settings`, wired into
  `ExtensionLifecycleManager::remove`): settings are configuration, not LLM
  data, so a reinstall starts from the declared defaults. Other members keep
  theirs.
- The Settings page hook inventory (`/api/webchat/v2/hooks`) lists trusted
  hooks deployment-wide and, per caller, the hooks of active third-party
  packages visible to that caller (consent-filtered), with any live
  quarantine reason. Another user's private plugin is absent.
