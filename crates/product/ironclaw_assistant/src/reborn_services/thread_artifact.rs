//! Caller-owned, redacted evidence bundle for a complete thread.

use super::*;
use chrono::{DateTime, Utc};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode,
};
use ironclaw_trace_commons::contribution::DeterministicTraceRedactor;

use ironclaw_threads::{BoundedThreadMessages, BoundedThreadMessagesRequest};
use ironclaw_turns::TurnRunId;
use serde::{Deserialize, Serialize};

use ironclaw_product_contracts::views::{RebornViewDescriptor, RebornViewProvider};

use super::{
    ProductCapabilityInvoker, RebornServices, RunArtifactLogs, RunArtifactMessage,
    RunArtifactRedaction, map_timeline_probe_error, parse_thread_id_field,
    run_artifact::{
        ARTIFACT_REDACTION_PIPELINE, artifact_messages, context_messages_by_id,
        timings::RunArtifactTimings,
    },
    thread_scope_from_turn_scope,
};

pub const THREAD_ARTIFACT_SCHEMA: &str = "ironclaw.thread_artifact.v1";
pub use ironclaw_product_contracts::product_wire::RebornThreadArtifactRequest;

/// A tool-heavy run persists several rows and can retain substantial tool
/// arguments and results per invocation. Keep the row and byte guards high
/// enough for those trajectories while bounding memory and response size.
pub const THREAD_ARTIFACT_MAX_MESSAGES: usize = 10_000;
const THREAD_ARTIFACT_MAX_STORED_BYTES: usize = 64 * 1024 * 1024;
const THREAD_ARTIFACT_MAX_SERIALIZED_BYTES: usize = 80 * 1024 * 1024;
pub const THREAD_ARTIFACT_VIEW: RebornViewDescriptor = RebornViewDescriptor {
    id: "thread_artifact",
    paginated: false,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebornThreadArtifact {
    pub schema: String,
    pub generated_at: DateTime<Utc>,
    pub thread_id: String,
    pub messages: Vec<RunArtifactMessage>,
    pub logs: RunArtifactLogs,
    /// One timing block per run with a durable message timestamp. Runs absent
    /// from the process-local store retain an explicit unavailable block.
    #[serde(default)]
    pub timings_by_run: Vec<RunArtifactRunTimings>,
    pub redaction: RunArtifactRedaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunArtifactRunTimings {
    pub run_id: String,
    pub timings: RunArtifactTimings,
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub(super) async fn build_thread_artifact(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornThreadArtifactRequest,
    ) -> Result<RebornThreadArtifact, ProductSurfaceError> {
        let thread_id = parse_thread_id_field("thread_id", request.thread_id)?;
        let scope = caller.turn_scope(thread_id.clone());
        let actor = caller.actor();
        let access = self
            .resolve_thread_access_for_caller(caller.clone(), scope, &actor)
            .await?;
        let thread_scope =
            thread_scope_from_turn_scope(&access.scope, Some(access.run_actor.user_id.clone()))?;
        let snapshot = match self
            .thread_service
            .list_thread_messages_bounded(BoundedThreadMessagesRequest {
                scope: thread_scope.clone(),
                thread_id: thread_id.clone(),
                max_messages: THREAD_ARTIFACT_MAX_MESSAGES,
                max_bytes: THREAD_ARTIFACT_MAX_STORED_BYTES,
            })
            .await
            .map_err(map_timeline_probe_error)?
        {
            BoundedThreadMessages::Complete(snapshot) => snapshot,
            BoundedThreadMessages::LimitExceeded => return Err(thread_artifact_too_large()),
        };

        let redactor = DeterministicTraceRedactor::new(Vec::new());
        let context_by_id = context_messages_by_id(snapshot.context.messages);
        let (messages, message_redaction_applied) =
            artifact_messages(snapshot.history.messages, &context_by_id, &redactor);

        // Compute per-run timings before `artifact_logs` takes `caller` by
        // value. Single pass: bucket messages by run first, so each run's
        // timing computation (`derive_wall_clock_ms`'s `updated_at` scan in
        // particular) sees only its own messages instead of the whole
        // thread's — passing the full `messages` list per run would let one
        // run's wall-clock span reach into a later run's activity, and would
        // also re-scan the whole list once per distinct run.
        let (runs_in_order, messages_by_run) = group_messages_by_run(&messages);

        let mut timings_by_run = Vec::new();
        for run_id in &runs_in_order {
            let run_messages = &messages_by_run[run_id];
            // No per-run `received_at` here; the earliest message creation in
            // the run is the closest durable origin available.
            // silent-ok: no message in the run carries `created_at` (only
            // possible for pre-timestamp records); wall-clock timing has no
            // origin to measure from, so this run is left out rather than
            // reported with a fabricated span.
            let Some(origin) = run_messages.iter().filter_map(|m| m.created_at).min() else {
                continue;
            };
            let timings = self.artifact_timings(
                &caller,
                &thread_id,
                run_id,
                origin,
                run_messages.iter().copied(),
            );
            timings_by_run.push(RunArtifactRunTimings {
                run_id: run_id.to_string(),
                timings,
            });
        }

        let (logs, log_redaction_applied) = self
            .artifact_logs(caller, &thread_id, None, &redactor)
            .await;

        let artifact = RebornThreadArtifact {
            schema: THREAD_ARTIFACT_SCHEMA.to_string(),
            generated_at: Utc::now(),
            thread_id: thread_id.to_string(),
            messages,
            logs,
            timings_by_run,
            redaction: RunArtifactRedaction {
                pipeline: ARTIFACT_REDACTION_PIPELINE.to_string(),
                applied: message_redaction_applied || log_redaction_applied,
            },
        };
        let serialized_bytes = serde_json::to_vec(&artifact)
            .map_err(ProductSurfaceError::internal_from)?
            .len();
        if serialized_bytes > THREAD_ARTIFACT_MAX_SERIALIZED_BYTES {
            return Err(thread_artifact_too_large());
        }
        Ok(artifact)
    }
}

fn thread_artifact_too_large() -> ProductSurfaceError {
    ProductSurfaceError::from_status(ProductSurfaceErrorCode::InvalidRequest, 413, false)
}

fn group_messages_by_run(
    messages: &[RunArtifactMessage],
) -> (
    Vec<TurnRunId>,
    std::collections::HashMap<TurnRunId, Vec<&RunArtifactMessage>>,
) {
    let mut runs_in_order = Vec::new();
    let mut messages_by_run = std::collections::HashMap::new();
    for message in messages {
        // silent-ok: a message with no run_id (pre-turn history, or a
        // non-run message kind) simply contributes no timings entry.
        let Some(run_id_text) = message.run_id.as_deref() else {
            continue;
        };
        let Ok(run_id) = TurnRunId::parse(run_id_text) else {
            continue;
        };
        messages_by_run
            .entry(run_id)
            .or_insert_with(|| {
                runs_in_order.push(run_id);
                Vec::new()
            })
            .push(message);
    }
    (runs_in_order, messages_by_run)
}

pub const ADMIN_THREAD_SCRAPE_THREADS_VIEW: ProductView<
    RebornAdminThreadScrapeListRequest,
    RebornListThreadsResponse,
> = ProductView::paginated("admin_thread_scrape_threads");

pub const ADMIN_THREAD_SCRAPE_ARTIFACT_VIEW: ProductView<
    RebornAdminThreadScrapeArtifactRequest,
    RebornThreadArtifact,
> = ProductView::unpaginated("admin_thread_scrape_artifact");

pub const ADMIN_THREAD_SCRAPE_RUN_ARTIFACT_VIEW: ProductView<
    RebornAdminThreadScrapeRunArtifactRequest,
    RebornRunArtifact,
> = ProductView::unpaginated("admin_thread_scrape_run_artifact");

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    /// Mint the admin->target scope transition for one thread-scrape request.
    ///
    /// Revalidates the caller as an active admin and the target as an
    /// existing same-tenant user (absence maps to a sanitized 404), then
    /// returns a `ProductSurfaceCaller` scoped to the *target* user so the
    /// downstream artifact builders read that user's threads through the
    /// caller-owned redaction and ownership gates. Never caches the admin
    /// decision: the revalidation runs per request.
    pub(super) async fn thread_scrape_subject(
        &self,
        caller: &ProductSurfaceCaller,
        user_id: UserId,
    ) -> Result<ProductSurfaceCaller, ProductSurfaceError> {
        self.authorize_admin(caller).await?;
        self.require_admin_target(&caller.tenant_id, &user_id)
            .await?;
        Ok(ProductSurfaceCaller::new(
            caller.tenant_id.clone(),
            user_id,
            caller.agent_id.clone(),
            caller.project_id.clone(),
        ))
    }

    pub async fn list_admin_thread_scrape_threads(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornAdminThreadScrapeListRequest,
    ) -> Result<RebornListThreadsResponse, ProductSurfaceError> {
        let admin_user_id = caller.user_id.clone();
        let target_user_id = request.user_id.clone();
        let result = async {
            let subject = self.thread_scrape_subject(&caller, request.user_id).await?;
            self.build_threads_view(
                subject,
                ProductListThreadsRequest {
                    limit: request.limit,
                    cursor: request.cursor,
                    candidate_thread_id: None,
                    needs_approval: false,
                },
            )
            .await
        }
        .await;
        let outcome = if result.is_ok() { "success" } else { "failure" };
        // debug! + the audit target, not info!: the operator-log buffer
        // captures INFO+ and would otherwise re-embed these audit fields
        // (including the admin's identity) into the scraped user's own
        // artifact export. Security-boundary diagnostics are debug-level per
        // TracingSecurityAuditSink; a durable audit sink is composition wiring.
        tracing::debug!(
            target: "ironclaw::thread_scrape_audit",
            action = "threads_listed",
            outcome,
            admin_user_id = %admin_user_id,
            target_user_id = %target_user_id,
            "thread scraping"
        );
        result
    }

    pub async fn build_admin_thread_scrape_artifact(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornAdminThreadScrapeArtifactRequest,
    ) -> Result<RebornThreadArtifact, ProductSurfaceError> {
        let admin_user_id = caller.user_id.clone();
        let target_user_id = request.user_id.clone();
        // Validate the id before it can reach any audit emission: a raw path
        // segment Display-formatted into the audit line would let a caller
        // forge audit entries (e.g. embedded newlines). Only validated
        // ThreadIds (newline-free by construction) are ever logged.
        let thread_id = parse_thread_id_field("thread_id", request.thread_id.clone())?;
        let result = async {
            let subject = self.thread_scrape_subject(&caller, request.user_id).await?;
            self.build_thread_artifact(
                subject,
                RebornThreadArtifactRequest {
                    thread_id: thread_id.to_string(),
                },
            )
            .await
        }
        .await;
        let outcome = if result.is_ok() { "success" } else { "failure" };
        // debug!, not info!: see threads_listed — the operator-log buffer
        // captures INFO+ and would leak these audit fields (admin identity,
        // target thread) into the scraped user's artifact export.
        tracing::debug!(
            target: "ironclaw::thread_scrape_audit",
            action = "thread_artifact_exported",
            outcome,
            admin_user_id = %admin_user_id,
            target_user_id = %target_user_id,
            thread_id = %thread_id,
            "thread scraping"
        );
        result
    }

    pub async fn build_admin_thread_scrape_run_artifact(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornAdminThreadScrapeRunArtifactRequest,
    ) -> Result<RebornRunArtifact, ProductSurfaceError> {
        let admin_user_id = caller.user_id.clone();
        let target_user_id = request.user_id.clone();
        // Validate ids before any audit emission (see thread_artifact_exported):
        // only validated ThreadId/TurnRunId values are ever Display-formatted
        // into the audit trail, so path-segment injection cannot forge lines.
        let thread_id = parse_thread_id_field("thread_id", request.thread_id.clone())?;
        let run_id = parse_run_id_field("run_id", request.run_id.clone())?;
        let result = async {
            let subject = self.thread_scrape_subject(&caller, request.user_id).await?;
            self.build_run_artifact(
                subject,
                RebornRunArtifactRequest {
                    thread_id: thread_id.to_string(),
                    run_id: run_id.to_string(),
                },
            )
            .await
        }
        .await;
        let outcome = if result.is_ok() { "success" } else { "failure" };
        // debug!, not info!: see threads_listed — the operator-log buffer
        // captures INFO+ and would leak these audit fields into the scraped
        // user's artifact export.
        tracing::debug!(
            target: "ironclaw::thread_scrape_audit",
            action = "run_artifact_exported",
            outcome,
            admin_user_id = %admin_user_id,
            target_user_id = %target_user_id,
            thread_id = %thread_id,
            run_id = %run_id,
            "thread scraping"
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use ironclaw_threads::{MessageKind, MessageStatus};

    fn message(
        run_id: &str,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> RunArtifactMessage {
        RunArtifactMessage {
            message_id: format!("message-{run_id}"),
            sequence: 1,
            run_id: Some(run_id.to_string()),
            created_at: Some(created_at),
            updated_at: Some(updated_at),
            kind: MessageKind::Assistant,
            status: MessageStatus::Finalized,
            content: "hello".to_string(),
            tool_call: None,
        }
    }

    #[test]
    fn per_run_message_groups_keep_exact_wall_clock_inputs_without_cloning() {
        let origin = Utc::now();
        let run_a = TurnRunId::new();
        let run_b = TurnRunId::new();
        let messages = vec![
            message(
                &run_a.to_string(),
                origin,
                origin + Duration::milliseconds(12),
            ),
            message(
                &run_b.to_string(),
                origin + Duration::seconds(2),
                origin + Duration::seconds(2) + Duration::milliseconds(34),
            ),
        ];

        let (runs, messages_by_run) = group_messages_by_run(&messages);
        assert_eq!(runs, vec![run_a, run_b]);
        assert_eq!(messages_by_run[&run_a].len(), 1);
        assert_eq!(messages_by_run[&run_b].len(), 1);
        assert_eq!(
            crate::reborn_services::timings_source::derive_wall_clock_ms(
                origin,
                messages_by_run[&run_a].iter().copied(),
            ),
            Some(12),
        );
        assert_eq!(
            crate::reborn_services::timings_source::derive_wall_clock_ms(
                origin + Duration::seconds(2),
                messages_by_run[&run_b].iter().copied(),
            ),
            Some(34),
        );
    }
}
