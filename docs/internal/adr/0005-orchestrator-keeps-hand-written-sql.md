# ADR 0005: `ironclaw_orchestrator` stores its state in hand-written libSQL tables

**Status:** Accepted 2026-10-23 (IronClaw v2 plan, stage 2 "Orchestrator
engine": "One store in the existing libSQL file").
**Rows:** PROPOSAL §11.2.6 (persistence idiom), the same exception shape as
ADR 0003 (`ironclaw_triggers`).

## Context

`ironclaw_orchestrator` owns the thread orchestration records: delegated
tasks (a parent/child tree), a per-thread ordered queue, Stop holds,
fork/merge transfers, recovery intents (restart continuation, usage-limit
resume), live-work rows and a per-turn usage log. `crates/domains/AGENTS.md`
sets `ScopedFilesystem` as the persistence floor and asks for an ADR when a
crate needs hand-written SQL instead.

## Decision

The orchestrator keeps `orch_*` tables in the deployment's libSQL file, on
the shared `ironclaw_libsql_runtime` admission runtime (one writer lane per
database, as for triggers). It never opens its own connection.

## Why not the filesystem fabric

The engine's correctness rests on relational, conditional writes:

- **Exactly-one finalization.** A task's terminal state is written by
  whichever arrives first of: the child's terminal turn event, the cancel
  fallback, or restart settlement. `UPDATE … WHERE status NOT IN (terminal)`
  decides that in one statement.
- **Tree walks.** Stop cancels every descendant, grandchildren of finished
  children included: indexed `parent_thread` lookups per level.
- **Ordered queue claims.** Draining claims the current head (`pending →
  sending`) inside one IMMEDIATE transaction so an edit or reorder after the
  claim is refused; reorder renumbers positions atomically.
- **Cross-thread scans every tick** (pending deliveries, armed recoveries,
  pending `now` transfers, queued threads) that a per-document store would
  turn into directory listings plus a projection index per query.
- **Idempotency** through unique indexes (`(parent_thread,
  client_request_id)`, `(thread_id, kind, reset_at)`).

The same rows as JSON documents would need one CAS document per record, a
hand-maintained secondary index for each of the scans above, and
multi-document invariants (task + transfer + hold) the fabric cannot commit
together.

## Consequences

- The orchestrator runs only on libSQL deployments (local-dev and the
  libSQL production profile through the standalone build path). Postgres
  deployments run without orchestration until a Postgres store exists; the
  composition root wires `None` there.
- The one-time import of legacy `ironclaw_threads` thread records reads the
  `session_thread` rows of `root_filesystem_entries` in the same file. It is
  read-only, marker-guarded (`orch_meta.legacy_threads_imported_v1`), and
  transcripts stay in `ironclaw_threads`.
- `reborn_persistence_driver_boundary.rs` lists the crate under this ADR,
  and `reborn_same_layer_edge_inventory.rs` carries its
  `ironclaw_libsql_runtime` edge.

## What would reopen this

A fabric primitive for conditional multi-document commits plus declared
secondary indexes would remove the reasons above.
