# Reborn Operator Observability Backend Contracts

Issues: #4595, #4596, #4597, #4598

PR #4608 added the WebUI v2 route shells and ProductSurface DTOs for operator status, logs, and service lifecycle. The remaining work is to wire concrete backend behavior behind those stable surfaces.

## Operator status (#4595)

`GET /api/webchat/v2/operator/status` should report a bounded, redacted snapshot of the current Reborn host.

The WebUI route must consume the descriptor-backed ProductSurface query view
`operator_status`; the legacy `get_operator_status` facade method is a
compatibility wrapper over that same query path; the ProductSurface read
surface remains the canonical API.

Required payload shape, nested under the `operator_status` field of the root
`RebornOperatorCommandPlaneResponse`:

- `state`: one of the stable `RebornOperatorStatusState` values.
- `checks`: individual readiness checks with stable ids, severity, status, message, and remediation.
- No raw host paths, secrets, tokens, command lines, provider payloads, or unbounded logs.

Minimum backend inputs:

- Reborn composition readiness state.
- Runtime process binding/readiness state.
- Storage/secrets configuration readiness, reported as booleans or stable diagnostic ids only.
- LLM config availability, reported without API keys or provider error strings.

## Operator doctor diagnostics (#4596)

`GET /api/webchat/v2/operator/diagnostics` is the canonical Reborn doctor
surface. It must aggregate existing typed service evidence rather than
implementing a separate diagnostic command plane.

The WebUI route must consume the descriptor-backed ProductSurface query view
`operator_diagnostics`; the legacy `get_operator_diagnostics` facade method is
a compatibility wrapper over that same query path; the ProductSurface read
surface remains the canonical API.

Required behavior:

- Include the current operator status payload when status is available.
- Convert non-ready status checks into stable diagnostic reason codes under the
  `status` owning area. Status check ids may enter the public reason-code suffix
  only when they match lowercase snake-case `[a-z][a-z0-9_]{0,63}` and do not
  look secret/path-bearing; otherwise use a stable status/state fallback reason
  code and sanitize display fields.
- Include setup diagnostics for provider/model/profile/WebUI access without
  echoing secrets or provider/backend error details.
- Include effective-config diagnostics for unsupported, immutable, deprecated,
  secret-backed, or unknown settings.
- Continue returning a typed diagnostics payload when one subsystem is
  unavailable; a missing setup/status service should become a sanitized
  diagnostic instead of failing the entire doctor route.
- CLI doctor commands, when retained, should be wrappers around this same
  service/API evidence and not a parallel diagnostic implementation.

## Operator logs (#4597)

`GET /api/webchat/v2/operator/logs` should return bounded, cursor-paginated,
redacted log entries nested under the `logs` field of the root
`RebornOperatorCommandPlaneResponse`.

Required behavior:

- Respect the facade limit clamp before hitting the backend.
- Support optional level and target filters using stable enum/string values.
- Support `tail=true` for newest entries in chronological order and
  `follow=true` with an opaque cursor for newer entries when the backend can
  retain an in-process cursor window.
- Reject requests that set both `tail=true` and `follow=true`.
- Return opaque cursors only; clients must not parse cursor internals.
- Redact secrets, tokens, credentials, raw request bodies, host-sensitive paths
  with either slash or backslash separators, and provider payload details.
- Return `service_unavailable` or an unavailable command-plane payload when no concrete backend is wired.

Initial backend options:

- In-process bounded ring buffer for current-process structured events,
  including bounded tail/follow over retained entries.
- Optional journald/systemd integration behind a separate service implementation.
- File-tail support only if path allowlisting and redaction are enforced before response construction.

## Service lifecycle (#4598)

`POST /api/webchat/v2/operator/service` should expose lifecycle status and
control only where the host deployment owns a manageable service unit, with the
payload nested under the `service_lifecycle` field of the root
`RebornOperatorCommandPlaneResponse`.

Required behavior:

- `status` is safe in every deployment and may report `unsupported`.
- `install`, `start`, and `stop` fail closed unless a concrete host service manager is explicitly wired.
- Commands must not shell out through untrusted input.
- Responses include stable state, message, and diagnostics without raw command output.

Initial backend:

- Local composition wires an allowlisted launchd/systemd user-service manager for
  the fixed `com.ironclaw.reborn` / `ironclaw-reborn.service` unit.
- Unsupported OS targets return `unsupported` with remediation instead of
  pretending success.
- The backend returns stable typed state only; raw command output and host paths
  are not surfaced to the browser.

## Readiness probe and metrics export

Two transport routes sit beside the command plane above; both are specified in
`crates/product/ironclaw_webui/CONTRACT.md` ("Operator observability").

**Readiness — `GET /api/ready`.** Public and rate-limited per peer. It serves
the same composed readiness the operator status view reports
(`RebornReadiness::operator_status()`, computed once by composition), reduced
to its stable vocabulary: the roll-up plus each check's id, state and severity.
`200` only when the roll-up is `ready`; `503` for `degraded`, `blocked` and
`not_configured`, and when the host supplied no snapshot. Check prose and
remediation never cross this route. `/api/health` remains the unconditional
liveness probe.

This is a **boot-time configuration-readiness** result, not a live dependency
probe. `RebornReadiness` is computed once while the runtime is assembled
(profile validation, which services and workers were wired) and is never
re-evaluated; the operator status port (`OperatorStatusService`) serves the
same snapshot, so re-reading it per request would return identical data. A
database, provider or worker that fails *after* boot does not flip `/api/ready`
to `503`. Development profiles (`DevOnly`, and the mounted-volume previews)
always answer `503`: the dev-only profile carries a blocking
`readiness_composition_profile` check, so its roll-up is `blocked` (observed
on a live `local-dev` serve, 2026-11-13), and preview storage or a missing
worker rolls up to at least `degraded`. An orchestrator should gate traffic on
this route only for production-validated profiles.

**Metrics — `GET /api/operator/metrics`.** Bearer-authenticated and
operator-only, Prometheus text format 0.0.4, rendered through the
`ironclaw_product_contracts::operator_service::OperatorMetricsService` port.
The registry is `ironclaw_operator::metrics::OperatorMetrics`, process-global
in production (`operator_metrics()`), like the operator log ring. It holds no
state of its own: every sample is folded from a fact another owner produced,
observed by a decorator that records only after the inner owner committed it
and never changes the inner result. Composition applies each decorator in one
helper (`observability::operator_metrics::metered_*`), which production calls
with the process-wide registry and the integration harness calls with its own.

| Family | Labels | Folded from |
|---|---|---|
| `ironclaw_turn_runs_total` | `outcome` = completed / failed / cancelled | committed `TurnLifecycleEvent` terminals, via composition's `OperatorMetricsTurnEventSink` in the runtime's turn-event fan-out (`ironclaw_operator` may not name `ironclaw_turns`) |
| `ironclaw_turn_run_duration_seconds` (histogram) | `outcome` | `occurred_at` of the run's first observed lifecycle transition (normally `Submitted`) to its terminal — wall clock, gate waits included; at most 10 000 open runs are tracked and the oldest remembered start is evicted past that (a run whose start was evicted, or never observed, counts without a duration sample) |
| `ironclaw_model_calls_total`, `ironclaw_model_call_duration_seconds` | `outcome` | `ModelCompleted` / `ModelFailed` runtime events **durably committed** to the event log, observed by `MetricsDurableEventLog` beneath the write-behind `CoalescingEventSink`'s drain. Events that sink drops while its bounded queue is full (its `try_emit` still answers `Ok`) and rows the backend rejects are not counted; events lost in a crash before the drain flushes are not counted either |
| `ironclaw_capability_invocations_total` | `capability_id`, `outcome` | `CapabilityActivitySucceeded` / `CapabilityActivityFailed` runtime events, committed as above |
| `ironclaw_capability_failures_total` | `capability_id`, `reason` (sanitized `error_kind`) | `CapabilityActivityFailed` |
| `ironclaw_llm_requests_total`, `ironclaw_llm_errors_total`, `ironclaw_llm_tokens_total` | `provider`, `model`, `outcome` / `kind` | provider responses, observed by `MetricsLlmProvider` outermost on the provider chain (so routed providers and reloads are both covered). `provider` is the provider a routed request (`<provider>::<model>`) named, else the chain's own id (`failover` for a failover set); `model` is read after the call through `effective_model_name`, so a failover that served from a later provider reports that provider's model. A failed request mints no series: it is attributed to `model="unknown"` (and a caller-named provider to `provider="unknown"`) unless that provider/model pair has already served a request — a per-run requested model reaches the provider unvalidated, so this keeps junk model names from claiming series slots |
| `ironclaw_llm_request_duration_seconds` (histogram) | `outcome` = success / error | same observation |
| `ironclaw_approval_gates_opened_total` | — | committed `Blocked` turn transitions whose gate kind is `approval` (same turn-event adapter) |
| `ironclaw_approval_gates_resolved_total` | `resolution` = approved / denied | committed user decisions, observed by `MetricsApprovalRequestStore` around the approval resolver's store (`approve`, `deny`). A capability-side rollback (`discard_pending` when blocking the invocation fails) is not counted: it runs before the turn commits `Blocked`, so the gate was never counted opened. `opened - resolved` is therefore gates still pending plus gates whose run ended (cancelled, failed) while pending |
| `ironclaw_metrics_series_overflow_total` | `family` | samples folded into `__other__` |

Cardinality rules, enforced by the registry: no tenant, user, agent, project,
thread, run, or invocation id is ever a label; label values are reduced to
`[A-Za-z0-9_.:/@-]`, at most 96 chars; `capability_id` families hold at most
256 series and provider/model families 128, beyond which the open-vocabulary
labels (`capability_id`, `reason`, `provider`, `model`) fold into `__other__`
and the sample is counted; closed labels (`outcome`, token/error `kind`) are
kept, so an overflowed failure is still a failure.

The approval-store decorator wraps the store at its consumer (the resolver),
never where it is registered with `HostRuntimeServices`, so the
production-wiring report keeps classifying the concrete store type (an
in-memory store stays `LocalOnly`).

The loop-terminal `RuntimeEvent`s (`LoopCompleted`/`LoopFailed`) are not a run
source: no production path publishes the loop's `Completed`/`Failed`
milestones today, so the registry ignores them and folds runs from the turn
kernel's committed transitions instead.

**Not yet implemented:** OTLP trace/metric export. No exporter dependency is
compiled in; see the follow-up note in the change that introduced this
section.

## Test requirements

Implementation PRs must drive the caller path:

- Service-facade tests for status/logs/service lifecycle mapping.
- Router tests for query/body validation and rate-limit descriptor stability.
- Redaction tests for log messages and lifecycle diagnostics.
- Failure-path tests for unwired, unsupported, malformed, and backend-error cases.
- Backend-specific tests for limit clamping, cursor stability, and command allowlisting.
