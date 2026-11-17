# IronClaw Reborn script runner contract

**Date:** 2026-04-25
**Status:** V1 contract slice
**Crate:** `crates/lanes/ironclaw_sandbox`
**Depends on:** `docs/internal/reborn/contracts/host-api.md`, `docs/internal/reborn/contracts/extensions.md`, `docs/internal/reborn/contracts/resources.md`, `docs/internal/reborn/contracts/dispatcher.md`

---

## 1. Purpose

`ironclaw_sandbox` provides the native CLI/software execution lane without requiring every useful tool to be rebuilt in WASM.

The public runtime kind is:

```text
RuntimeKind::Script
```

The V1 Docker/container executor remains available as an implementation backend, but Docker is not the public script contract. Extension manifests declare a semantic runner profile plus command metadata; the host selects the concrete backend for that runner. Manifests do not receive raw Docker flags, host paths, ambient environment, secrets, or network access by default.

---

## 2. Current V1 API

The crate exposes:

```rust
pub struct ScriptRuntime<B: ScriptBackend>;
pub struct ScriptRuntimeConfig;
pub trait ScriptBackend;
pub trait ScriptExecutor;
pub struct DockerScriptBackend;

pub struct ScriptExecutionRequest<'a>;
pub struct ScriptExecutionResult;
pub struct ScriptInvocation;
pub struct ScriptBackendRequest;
pub struct ScriptBackendOutput;
pub enum ScriptError;
```

The dispatcher composes a script runtime with:

```rust
RuntimeDispatcher::new(&registry, &fs, &governor)
    .with_script_runtime(&script_runtime)
```

and dispatches script capabilities through the same `dispatch_json` entry point as WASM capabilities.

---

## 3. Manifest-derived command contract

Script command metadata comes from a validated extension manifest:

```toml
[runtime]
kind = "script"
runner = "sandboxed_process"
command = "script-echo"
args = ["--json"]
```

At execution time, the runtime builds a `ScriptBackendRequest` from the manifest and the invocation:

```text
provider
capability_id
scope
runner
image (optional, only for Docker-backed runners)
command
args
stdin_json
```

Rules:

- runner/command/args come from the manifest, not model/user input
- invocation input is serialized as JSON and passed through stdin
- backend receives normalized runner fields, not raw Docker flags
- capability IDs must be declared by the package
- descriptor runtime must match package runtime
- legacy `backend = "docker"` manifests remain accepted for the optional Docker backend, but new manifests should use `runner = "sandboxed_process"` or another host-defined semantic runner profile

---

## 4. Resource lifecycle

The script runtime owns the script lane reserve/execute/reconcile/release protocol.
It holds no budget authority of its own: it is handed
`ironclaw_host_api::resource::RuntimeResourceBudget` — reserve / reconcile /
release, and nothing else — which the kernel implements over its
`ResourceGovernor` (`ironclaw_resources::GovernorRuntimeBudget`). The lane
cannot set limits, read account state, or name an account (#7067).

```text
validate package/capability/runtime
-> reserve(scope, estimate)
-> backend.execute(request)
-> enforce output limits
-> parse stdout JSON
-> reconcile(reservation_id, actual_usage)
```

Failure cleanup:

```text
validation fails before reserve -> no reservation
reserve fails -> no backend call
backend fails -> release reservation
non-zero exit -> release reservation
output limit fails -> release reservation
invalid JSON stdout -> release reservation
backend outlives max_wall_clock_ms + teardown_grace_ms -> release reservation, ScriptError::Timeout
success -> reconcile reservation
```

The backend is synchronous, so `ScriptRuntime::execute_extension_json` is
`async` and runs the backend on the Tokio blocking pool, never on an async
worker. It waits at most `max_wall_clock_ms + teardown_grace_ms`; a backend
still running then is abandoned on its blocking thread, the reservation is
released, and the call reports `ScriptError::Timeout`.

Admission is bounded by `ScriptRuntimeConfig::max_concurrent_executions`
slots. A call that finds no free slot is refused before reserving, with
`ScriptError::Resource` (`LimitExceeded`); it is never queued. The slot
(`ScriptExecutionSlot`) moves into the blocking job and is passed to the
backend, so it is released only when the backend really returns — never at
the timeout — or later, by work the backend keeps. A job that only gets a
blocking thread after its call timed out refuses to start the script.

Actual usage currently records:

- wall-clock milliseconds reported by the backend
- stdout bytes
- one process count per successful backend execution

---

## 5. Optional Docker backend posture

`DockerScriptBackend` invokes Docker only when the manifest resolves to `runner = "docker"` with an image. It uses normalized fields only:

```text
docker run --rm -i --name ironclaw-script-<uuid> \
  --label ironclaw.lane=script --label ironclaw.instance=<uuid per process> \
  --label ironclaw.host=<hostname> --label ironclaw.pid=<pid> \
  --network none <image> <command> <args...>
```

The container name is host-assigned per execution; the label marks every
script-lane container. On a wall-clock timeout the backend tears down in
bounded steps: kill the CLI child, wait a bounded time for it to exit,
`docker rm -f` the container by name (always — killing `docker run` does not
stop the container), wait again, then join the pipe threads with a deadline,
abandoning any that never close. It then returns its timeout error; no step
waits without a bound.

From the moment `docker run` is spawned, a container guard owns the
container name, the lane slot, and the CLI child. Every exit path — normal
exit, error (a failed poll, a missing pipe), timeout, or a panic unwinding
through the backend — settles through the guard's drop: the CLI child is
waited (killed first if still running) and the container is verified gone
(`docker ps -a --filter name=^/<name>$`, after giving `--rm` until the reap
bound and then `docker rm -f`). Only when both are confirmed is the slot
released. Otherwise the container, any unwaited CLI child, and the slot go to
an orphan reaper, which retries with backoff growing to a capped interval,
never drops ownership, catches a panic in any attempt as a failed attempt,
and releases the slot only once the container is gone and the child waited
— so capacity is never re-admitted over a live container and no CLI child is
left as a zombie.

All cleanup of one execution — timeout teardown, the guard's drop, pipe
joins, every `docker rm`/`ps` — shares one absolute deadline
(`TeardownBounds::total`, below `teardown_grace_ms`), started once; each step
gets only the time left, and whatever is unsettled when it passes goes to the
reaper. Every `docker` helper process is owned until waited: one that misses
its bound is killed and handed to a single janitor rather than dropped. The
reaper and the janitor each run one worker; if a worker cannot be started,
the orphans (slot and child) or helper children stay held and the next
hand-off retries the spawn. At composition time,
`DockerScriptBackend::reap_previous_instances` removes lane containers left on
this host by earlier instances whose process is gone; containers of a live
process (including another IronClaw on the same host), of another host, or
without full labels are left alone.

V1 default restrictions:

- no host path mounts
- no host environment forwarding
- no Docker socket exposure to extensions
- network disabled by default
- JSON input over stdin
- stdout is bounded and parsed as JSON
- stderr is bounded before surfacing in errors

Future PRs may add scoped filesystem mounts, artifact export, network policy, and secret-handle injection. Those must be explicit host-mediated additions, not ambient Docker options.

---

## 6. Dispatcher relationship

`ironclaw_dispatcher` selects the script lane when a declared capability has:

```text
RuntimeKind::Script
```

The dispatcher does not execute Docker itself. It calls the configured `ScriptExecutor`.

Fail-closed behavior:

- script capability with no configured script runtime -> `MissingRuntimeBackend { runtime: Script }`
- non-script runtime lanes remain unsupported until their crates land
- unknown capabilities and descriptor/package runtime mismatches fail before reservation

---

## 7. Non-goals

This contract does not add:

- arbitrary host shell access
- host filesystem mounts
- artifact export
- secret injection
- network access
- MCP protocol handling
- long-running process lifecycle
- product-surface orchestration

Those are separate slices.

---

## 8. Tests

Current contract tests cover:

- successful script execution reserves, invokes backend, parses stdout JSON, and reconciles
- budget denial prevents backend execution
- backend non-zero exit releases reservation
- output limit failure releases reservation
- non-script packages are rejected before reserving
- undeclared capabilities are rejected before reserving
- runner/command metadata comes from the manifest, not invocation input
- dispatcher routes script capabilities through a configured `ScriptExecutor`
- dispatcher fails before reservation when script backend is missing
