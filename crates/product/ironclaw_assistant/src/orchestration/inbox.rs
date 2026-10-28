//! The inbox and goal views and commands (v2 stage 5, t3code's inbox).
//!
//! `inbox.list` joins, for the caller, the orchestrator's threads (profile,
//! engines of the whole delegation tree, live and recovery state), the
//! session thread list (titles, activity) and automation runs (which the
//! thread list leaves out), applies the durable pin/snooze/settle/visit
//! marks and returns rows ready to render, so the desktop app does one
//! request. `inbox.mutate` writes one mark for a thread on that list and
//! answers with the list re-read. Goals are one objective per engine thread.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ironclaw_host_api::ids::{ActivityId, ThreadId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::inbox::{InboxFacts, InboxRecord, SavedTitle, classify};
use ironclaw_orchestrator::model::{Millis, now_ms};
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_product_contracts::inbound_requests::{
    ProductListAutomationsRequest, ProductListThreadsRequest,
};
use ironclaw_product_contracts::inbox::*;
use ironclaw_product_contracts::product_wire::{
    RebornAutomationRecentRunInfo, RebornAutomationRecentRunStatus,
};
use ironclaw_product_contracts::profiles::DEFAULT_PROFILE_ID;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceQueryRequest,
};
use serde::Deserialize;
use serde_json::Value;

use super::profiles::{self, ProfileHome};
use super::surface::scope_of;
use ironclaw_product_contracts::task_models::{
    BackgroundTask, RETITLE_EVERY_REPLIES, TASK_COMPLETE_COMMAND, TaskCompletionRequest,
    TaskCompletionResult, ThreadTitleRequest, ThreadTitleResponse,
};

use crate::reborn_services::{
    AUTOMATIONS_VIEW, RebornListThreadsResponse, THREADS_VIEW, TIMELINE_VIEW,
};

/// One page of the session list; older threads stay reachable by search in
/// the Files and Memory pages.
const SESSION_THREADS: u32 = 100;
const AUTOMATIONS: u32 = 50;
const RUNS_PER_AUTOMATION: u32 = 5;
const IRONCLAW_RUNTIME: &str = "ironclaw";

/// What the joins collect per thread before classification.
struct Draft {
    title: Option<String>,
    profile_id: String,
    engines: Vec<InboxEngine>,
    facts: InboxFacts,
    parent_thread_id: Option<String>,
    automation: bool,
}

fn millis(at: Option<DateTime<Utc>>) -> Option<Millis> {
    at.map(|t| t.timestamp_millis())
}

fn ironclaw_engine() -> InboxEngine {
    InboxEngine {
        runtime_id: IRONCLAW_RUNTIME.into(),
        provider: None,
        model: None,
    }
}

async fn query_view(
    inner: &Arc<dyn ProductSurface>,
    caller: &ProductSurfaceCaller,
    view_id: &str,
    input: Value,
) -> Result<Value, ProductSurfaceError> {
    let page = inner
        .query(
            caller.clone(),
            ProductSurfaceQueryRequest {
                view_id: view_id.to_string(),
                input,
                cursor: None,
                limit: None,
            },
        )
        .await?;
    Ok(page.items.into_iter().next().unwrap_or(Value::Null))
}

/// Recent automation runs that reached a thread, newest first per automation.
async fn automation_runs(
    inner: &Arc<dyn ProductSurface>,
    caller: &ProductSurfaceCaller,
) -> Vec<(String, RebornAutomationRecentRunInfo)> {
    let request = ProductListAutomationsRequest {
        limit: Some(AUTOMATIONS),
        run_limit: Some(RUNS_PER_AUTOMATION),
        include_completed: true,
    };
    let input = match serde_json::to_value(request) {
        Ok(input) => input,
        Err(error) => {
            tracing::debug!(%error, "inbox automations request did not encode");
            return Vec::new();
        }
    };
    // Automations are optional: a deployment without them still has an inbox.
    let doc = match query_view(inner, caller, AUTOMATIONS_VIEW.id, input).await {
        Ok(doc) => doc,
        Err(error) => {
            tracing::debug!(?error, "inbox skips automation runs");
            return Vec::new();
        }
    };
    #[derive(Deserialize)]
    struct Automation {
        #[serde(default)]
        name: String,
        #[serde(default)]
        recent_runs: Vec<RebornAutomationRecentRunInfo>,
    }
    #[derive(Deserialize)]
    struct Automations {
        #[serde(default)]
        automations: Vec<Automation>,
    }
    match serde_json::from_value::<Automations>(doc) {
        Ok(list) => list
            .automations
            .into_iter()
            .flat_map(|a| {
                let name = a.name;
                a.recent_runs
                    .into_iter()
                    .map(move |run| (name.clone(), run))
            })
            .collect(),
        Err(error) => {
            tracing::debug!(%error, "inbox could not read automation runs");
            Vec::new()
        }
    }
}

fn group(state: InboxState) -> u8 {
    match state {
        InboxState::Pinned => 0,
        InboxState::Inbox | InboxState::Working => 1,
        InboxState::Snoozed => 2,
        InboxState::Settled => 3,
    }
}

/// Pinned by pin time, the main list by recency, snoozed by wake time,
/// settled newest first.
fn sort_rows(rows: &mut [InboxRow], records: &HashMap<String, InboxRecord>) {
    let pinned_at = |r: &InboxRow| {
        records
            .get(&r.thread_id)
            .and_then(|rec| rec.pinned_at)
            .unwrap_or(0)
    };
    rows.sort_by(|a, b| {
        group(a.state)
            .cmp(&group(b.state))
            .then_with(|| match a.state {
                InboxState::Pinned => pinned_at(a).cmp(&pinned_at(b)),
                InboxState::Snoozed => a.snooze_until.cmp(&b.snooze_until),
                InboxState::Settled => b.settled_at.cmp(&a.settled_at),
                _ => b.last_activity_at.cmp(&a.last_activity_at),
            })
            .then_with(|| a.thread_id.cmp(&b.thread_id))
    });
}

pub(super) async fn list(
    engine: &Engine,
    home: Option<&ProfileHome>,
    inner: &Arc<dyn ProductSurface>,
    caller: &ProductSurfaceCaller,
) -> Result<InboxListResponse, ProductSurfaceError> {
    let store = engine.store();
    let owner = ProfileOwner::from(&scope_of(caller));
    let internal = ProductSurfaceError::internal_from;
    let threads = store.inbox_threads(&owner).await.map_err(internal)?;
    let records = store.inbox_records(&owner).await.map_err(internal)?;
    let epoch = store.inbox_epoch().await.map_err(internal)?;
    let profile_records = store.profiles(&owner).await.map_err(internal)?;

    let request = ProductListThreadsRequest::default().set_limit(SESSION_THREADS);
    let input = serde_json::to_value(request).map_err(ProductSurfaceError::internal_from)?;
    let sessions: RebornListThreadsResponse =
        serde_json::from_value(query_view(inner, caller, THREADS_VIEW.id, input).await?)
            .map_err(ProductSurfaceError::internal_from)?;
    let runs = automation_runs(inner, caller).await;

    let mut drafts: HashMap<String, Draft> = HashMap::new();
    for t in threads {
        drafts.insert(
            t.thread_id.to_string(),
            Draft {
                title: t.title,
                profile_id: t
                    .profile_id
                    .map_or_else(|| DEFAULT_PROFILE_ID.to_string(), |p| p.to_string()),
                engines: t.engines,
                facts: t.facts,
                parent_thread_id: t.parent_thread_id.map(|p| p.to_string()),
                automation: false,
            },
        );
    }
    for s in sessions.threads {
        let activity = millis(s.updated_at.or(s.created_at)).unwrap_or(0);
        let draft = drafts
            .entry(s.thread_id.to_string())
            .or_insert_with(|| Draft {
                title: None,
                profile_id: DEFAULT_PROFILE_ID.into(),
                engines: vec![ironclaw_engine()],
                facts: InboxFacts {
                    last_activity_at: activity,
                    ..InboxFacts::default()
                },
                parent_thread_id: None,
                automation: false,
            });
        // The list route computes titles; the engine's copy may be stale.
        if s.title.as_deref().is_some_and(|t| !t.trim().is_empty()) {
            draft.title = s.title;
        }
        draft.facts.last_activity_at = draft.facts.last_activity_at.max(activity);
    }
    for (name, run) in runs {
        let Some(thread_id) = run.thread_id else {
            continue;
        };
        let submitted = run.submitted_at.timestamp_millis();
        let ended = millis(run.completed_at);
        let draft = drafts
            .entry(thread_id.to_string())
            .or_insert_with(|| Draft {
                title: None,
                profile_id: DEFAULT_PROFILE_ID.into(),
                engines: vec![ironclaw_engine()],
                facts: InboxFacts::default(),
                parent_thread_id: None,
                automation: true,
            });
        draft.automation = true;
        if draft.title.is_none() && !name.trim().is_empty() {
            draft.title = Some(name);
        }
        let f = &mut draft.facts;
        f.last_activity_at = f.last_activity_at.max(ended.unwrap_or(submitted));
        f.last_ended_at = f.last_ended_at.max(ended);
        f.running |= run.status == RebornAutomationRecentRunStatus::Running;
        f.failed |= run.status == RebornAutomationRecentRunStatus::Error;
    }

    let now = now_ms();
    let mut rows: Vec<InboxRow> = drafts
        .into_iter()
        .map(|(thread_id, d)| {
            let record = records.get(&thread_id);
            let (state, status) = classify(&d.facts, record, epoch, now);
            InboxRow {
                // A generated title wins over the first-message one.
                title: record.and_then(|r| r.title.clone()).or(d.title),
                profile_id: d.profile_id,
                state,
                status,
                engines: d.engines,
                last_activity_at: d.facts.last_activity_at,
                snooze_until: (state == InboxState::Snoozed)
                    .then(|| record.and_then(|r| r.snooze_until))
                    .flatten(),
                settled_at: (state == InboxState::Settled)
                    .then(|| record.and_then(|r| r.settled_at))
                    .flatten(),
                limit_reset_at: d.facts.limit_reset_at,
                working_since: d.facts.working_since.filter(|_| d.facts.running),
                parent_thread_id: d.parent_thread_id,
                automation: d.automation,
                thread_id,
            }
        })
        .collect();
    sort_rows(&mut rows, &records);

    let last_used_profile_id = profile_records
        .first()
        .filter(|r| r.last_used_at.is_some())
        .map(|r| r.profile_id.to_string());
    let profiles = profile_records
        .into_iter()
        .map(|record| {
            let last_used_at = record.last_used_at;
            let (git_branch, display_name, profile_id) = match home {
                Some(home) => {
                    let info = profiles::info(home, record);
                    (info.git_branch, info.settings.display_name, info.profile_id)
                }
                None => (
                    None,
                    record.settings.display_name,
                    record.profile_id.to_string(),
                ),
            };
            InboxProfile {
                profile_id,
                display_name,
                git_branch,
                last_used_at,
            }
        })
        .collect();
    Ok(InboxListResponse {
        rows,
        profiles,
        last_used_profile_id,
    })
}

/// Write one mark on a thread of the caller's inbox, then re-read it.
pub(super) async fn mutate(
    engine: &Engine,
    home: Option<&ProfileHome>,
    inner: &Arc<dyn ProductSurface>,
    caller: &ProductSurfaceCaller,
    request: InboxMutationRequest,
) -> Result<Result<InboxListResponse, OrchestratorError>, ProductSurfaceError> {
    let thread = match ThreadId::new(request.thread_id.as_str()) {
        Ok(id) => id,
        Err(error) => return Ok(Err(error.into())),
    };
    // Only a thread the caller can see gets a record.
    let before = list(engine, home, inner, caller).await?;
    if !before.rows.iter().any(|r| r.thread_id == request.thread_id) {
        return Ok(Err(OrchestratorError::not_found(
            "thread_not_found",
            format!("No thread {thread} for this user."),
        )));
    }
    let owner = ProfileOwner::from(&scope_of(caller));
    if let Err(error) = engine
        .store()
        .inbox_mark(&owner, &thread, request.action, request.until, now_ms())
        .await
    {
        return Ok(Err(error));
    }
    list(engine, home, inner, caller).await.map(Ok)
}

const TITLE_SYSTEM_PROMPT: &str = include_str!("../../prompts/thread_title.md");
/// Enough of each side to name the conversation.
const TITLE_SOURCE_CHARS: usize = 1500;
const TITLE_MAX_CHARS: usize = 80;
/// Each recent message given to a re-title, and how many of them.
const RETITLE_MESSAGE_CHARS: usize = 600;
const RETITLE_MESSAGES: usize = 6;
/// Room for reasoning models, which think before they write the title.
const TITLE_MAX_TOKENS: u32 = 400;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TitleMessageKind {
    User,
    Assistant,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct TitleMessage {
    kind: TitleMessageKind,
    #[serde(default)]
    sequence: u64,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize, Default)]
struct TitleTimeline {
    #[serde(default)]
    messages: Vec<TitleMessage>,
}

fn clip_chars(text: &str, max: usize) -> &str {
    text.char_indices()
        .nth(max)
        .map_or(text, |(at, _)| &text[..at])
}

fn message_text(m: &TitleMessage) -> Option<&str> {
    m.content
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
}

fn is_exchange(m: &TitleMessage) -> bool {
    matches!(m.kind, TitleMessageKind::User | TitleMessageKind::Assistant)
        && message_text(m).is_some()
}

/// The first user message and the first reply after it, if both exist.
fn first_exchange(messages: &[TitleMessage]) -> Option<String> {
    let user_at = messages
        .iter()
        .position(|m| m.kind == TitleMessageKind::User && message_text(m).is_some())?;
    let user = message_text(&messages[user_at])?;
    let reply = messages[user_at + 1..]
        .iter()
        .find(|m| m.kind == TitleMessageKind::Assistant && message_text(m).is_some())
        .and_then(message_text)?;
    Some(format!(
        "First message:\n{}\n\nStart of the reply:\n{}",
        clip_chars(user, TITLE_SOURCE_CHARS),
        clip_chars(reply, TITLE_SOURCE_CHARS)
    ))
}

/// The current title and the latest messages, oldest first.
fn recent_exchange(messages: &[TitleMessage], current: &str) -> String {
    let recent: Vec<&TitleMessage> = messages.iter().filter(|m| is_exchange(m)).collect();
    let mut prompt = format!("Current title:\n{current}\n\nLatest messages:");
    for m in &recent[recent.len().saturating_sub(RETITLE_MESSAGES)..] {
        let who = if m.kind == TitleMessageKind::User {
            "User"
        } else {
            "Assistant"
        };
        let text = message_text(m).unwrap_or_default();
        prompt.push_str(&format!(
            "\n\n{who}:\n{}",
            clip_chars(text, RETITLE_MESSAGE_CHARS)
        ));
    }
    prompt
}

/// One line, no wrapping quotes or trailing period, bounded.
fn clean_title(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line
        .trim_start_matches(|c: char| c == '#' || c.is_whitespace())
        .trim_start_matches("Title:")
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '“' | '”'))
        .trim_end_matches('.')
        .trim();
    (!line.is_empty()).then(|| clip_chars(line, TITLE_MAX_CHARS).trim().to_string())
}

/// Title a thread on the "Thread titles" task model: from its first
/// exchange once it has a reply, then again every [`RETITLE_EVERY_REPLIES`]
/// replies from its latest messages, so the title follows the topic. A call
/// before either is due returns the saved title without a model call; no
/// model set or no reply yet answers `title: None` and saves nothing.
pub(super) async fn generate_title(
    engine: &Engine,
    surface: &dyn ProductSurface,
    caller: &ProductSurfaceCaller,
    request: ThreadTitleRequest,
) -> Result<Result<ThreadTitleResponse, OrchestratorError>, ProductSurfaceError> {
    let thread = match ThreadId::new(request.thread_id.as_str()) {
        Ok(id) => id,
        Err(error) => return Ok(Err(error.into())),
    };
    let owner = ProfileOwner::from(&scope_of(caller));
    let saved = match engine.store().inbox_title(&owner, &thread).await {
        Ok(saved) => saved,
        Err(error) => return Ok(Err(error)),
    };
    let unchanged = |saved: Option<SavedTitle>| {
        Ok(Ok(ThreadTitleResponse {
            title: saved.map(|s| s.title),
            changed: false,
        }))
    };
    // The timeline read is caller-scoped: a thread that is not the caller's
    // fails here, before any model call or write. It is the newest page, so
    // the replies since the last title are in it.
    let page = surface
        .query(
            caller.clone(),
            ProductSurfaceQueryRequest {
                view_id: TIMELINE_VIEW.id.to_string(),
                input: serde_json::json!({"thread_id": thread.as_str(), "limit": 50}),
                cursor: None,
                limit: None,
            },
        )
        .await?;
    let mut messages = page
        .items
        .into_iter()
        .next()
        .map(serde_json::from_value::<TitleTimeline>)
        .transpose()
        .map_err(ProductSurfaceError::internal_from)?
        .unwrap_or_default()
        .messages;
    messages.sort_by_key(|m| m.sequence);
    let replies: Vec<i64> = messages
        .iter()
        .filter(|m| m.kind == TitleMessageKind::Assistant && message_text(m).is_some())
        .map(|m| i64::try_from(m.sequence).unwrap_or(i64::MAX))
        .collect();
    let Some(&last_reply) = replies.last() else {
        return unchanged(saved);
    };
    let prompt = match &saved {
        None => match first_exchange(&messages) {
            Some(prompt) => prompt,
            None => return unchanged(saved),
        },
        Some(s) => {
            // A title saved before the marker existed counts every reply.
            let through = s.through_sequence.unwrap_or(i64::MIN);
            if replies.iter().filter(|seq| **seq > through).count() < RETITLE_EVERY_REPLIES {
                return unchanged(saved);
            }
            recent_exchange(&messages, &s.title)
        }
    };
    let completion = TaskCompletionRequest {
        task: BackgroundTask::ThreadTitles,
        system: TITLE_SYSTEM_PROMPT.to_string(),
        prompt,
        max_tokens: TITLE_MAX_TOKENS,
    };
    let response = surface
        .invoke(
            caller.clone(),
            ProductSurfaceInvokeRequest {
                operation_id: TASK_COMPLETE_COMMAND.capability_id()?,
                input: serde_json::to_value(completion)
                    .map_err(ProductSurfaceError::internal_from)?,
                activity_id: ActivityId::new(),
            },
        )
        .await?;
    let result: TaskCompletionResult =
        serde_json::from_value(response.output).map_err(ProductSurfaceError::internal_from)?;
    // No answer: keep what is saved and try again after the next reply.
    let Some(title) = result.text.as_deref().and_then(clean_title) else {
        return unchanged(saved);
    };
    let changed = saved.as_ref().is_none_or(|s| s.title != title);
    let next = SavedTitle {
        title,
        through_sequence: Some(last_reply),
    };
    if let Err(error) = engine
        .store()
        .inbox_set_title(&owner, &thread, &next, now_ms())
        .await
    {
        return Ok(Err(error));
    }
    Ok(Ok(ThreadTitleResponse {
        title: Some(next.title),
        changed,
    }))
}

pub(super) async fn goal(
    engine: &Engine,
    caller: &ProductSurfaceCaller,
    raw_thread: &str,
) -> Result<ThreadGoalResponse, OrchestratorError> {
    let thread = ThreadId::new(raw_thread)?;
    engine.owned_thread(&thread, &scope_of(caller)).await?;
    Ok(ThreadGoalResponse {
        thread_id: thread.to_string(),
        goal: engine.store().goal(&thread).await?,
    })
}

pub(super) async fn set_goal(
    engine: &Engine,
    caller: &ProductSurfaceCaller,
    request: ThreadGoalSetRequest,
) -> Result<ThreadGoalResponse, OrchestratorError> {
    let thread = ThreadId::new(request.thread_id.as_str())?;
    engine.owned_thread(&thread, &scope_of(caller)).await?;
    let goal = engine
        .store()
        .set_goal(
            &thread,
            request.objective.as_deref(),
            request.status,
            now_ms(),
        )
        .await?;
    Ok(ThreadGoalResponse {
        thread_id: thread.to_string(),
        goal,
    })
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use ironclaw_host_api::ids::{ActivityId, CapabilityId, TenantId, UserId};
    use ironclaw_orchestrator::model::ThreadScope;
    use ironclaw_product_contracts::surface::{
        ProductSurfaceInvokeRequest, ProductSurfaceInvokeResponse, ProductSurfaceQueryPage,
        ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
    };
    use ironclaw_product_contracts::task_models::THREAD_TITLE_COMMAND_ID;
    use serde_json::json;

    use super::*;
    use crate::orchestration::OrchestratedProductSurface;

    /// The undecorated surface: one session thread and one automation run.
    /// The task model answers a fixed title and counts its calls.
    struct Inner;

    static TASK_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    /// Exchanges about deploys appended to the "chat" timeline.
    static LATER_EXCHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[async_trait]
    impl ProductSurface for Inner {
        async fn invoke(
            &self,
            _caller: ProductSurfaceCaller,
            request: ProductSurfaceInvokeRequest,
        ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
            if request.operation_id.as_str() != TASK_COMPLETE_COMMAND.id {
                return Err(ProductSurfaceError::not_found());
            }
            TASK_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let asked: TaskCompletionRequest = serde_json::from_value(request.input).unwrap();
            assert_eq!(asked.task, BackgroundTask::ThreadTitles);
            let text = if asked.prompt.starts_with("Current title:") {
                // A re-title reads the saved title and the latest messages.
                assert!(asked.prompt.contains("Inbox plan for stage 5"));
                assert!(asked.prompt.contains("Ship the deploy 3"));
                "Deploy pipeline rollout"
            } else {
                assert!(asked.prompt.contains("Plan the inbox for stage 5"));
                assert!(asked.prompt.contains("Here is a plan"));
                "\"Inbox plan for stage 5.\"\n"
            };
            Ok(ProductSurfaceInvokeResponse {
                output: json!({"text": text}),
            })
        }

        async fn query(
            &self,
            caller: ProductSurfaceCaller,
            request: ProductSurfaceQueryRequest,
        ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
            let scope = json!({"tenant_id": "t1", "agent_id": "a1", "owner_user_id": "u1"});
            let item = match request.view_id.as_str() {
                v if v == TIMELINE_VIEW.id
                    && request.input["thread_id"] == "chat"
                    && caller.user_id.as_str() == "u1" =>
                {
                    let mut messages = vec![
                        json!({"kind": "assistant", "sequence": 3, "content": "Here is a plan"}),
                        json!({"kind": "user", "sequence": 2, "content": "Plan the inbox for stage 5"}),
                        json!({"kind": "system", "sequence": 1, "content": "setup"}),
                    ];
                    for n in 1..=LATER_EXCHANGES.load(std::sync::atomic::Ordering::SeqCst) {
                        messages.push(json!({"kind": "user", "sequence": 2 + 2 * n,
                            "content": format!("Ship the deploy {n}")}));
                        messages.push(json!({"kind": "assistant", "sequence": 3 + 2 * n,
                            "content": "Deploying"}));
                    }
                    json!({ "messages": messages })
                }
                v if v == THREADS_VIEW.id => json!({"threads": [{
                    "scope": scope, "thread_id": "chat", "created_by_actor_id": "u1",
                    "title": "Plan the inbox", "metadata_json": null,
                    "created_at": "2026-01-02T08:00:00Z", "updated_at": "2026-01-02T09:00:00Z"
                }]}),
                v if v == AUTOMATIONS_VIEW.id => json!({"automations": [{
                    "name": "Morning brief",
                    "recent_runs": [{"thread_id": "auto", "status": "ok",
                        "submitted_at": "2026-01-02T07:00:00Z",
                        "completed_at": "2026-01-02T07:01:00Z"}]
                }]}),
                _ => return Err(ProductSurfaceError::not_found()),
            };
            Ok(ProductSurfaceQueryPage {
                items: vec![item],
                next_cursor: None,
            })
        }

        async fn stream_events(
            &self,
            _caller: ProductSurfaceCaller,
            _request: ProductSurfaceStreamRequest,
        ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
            Err(ProductSurfaceError::not_found())
        }
    }

    fn caller(user: &str) -> ProductSurfaceCaller {
        ProductSurfaceCaller::new(
            TenantId::new("t1").unwrap(),
            UserId::new(user).unwrap(),
            None,
            None,
        )
    }

    async fn list_of(surface: &OrchestratedProductSurface, user: &str) -> InboxListResponse {
        let page = surface
            .query(
                caller(user),
                ProductSurfaceQueryRequest {
                    view_id: INBOX_VIEW.id.into(),
                    input: json!({}),
                    cursor: None,
                    limit: None,
                },
            )
            .await
            .unwrap();
        serde_json::from_value(page.items[0].clone()).unwrap()
    }

    async fn invoke(
        surface: &OrchestratedProductSurface,
        operation: &str,
        input: Value,
    ) -> Result<Value, ProductSurfaceError> {
        surface
            .invoke(
                caller("u1"),
                ProductSurfaceInvokeRequest {
                    operation_id: CapabilityId::new(operation).unwrap(),
                    input,
                    activity_id: ActivityId::new(),
                },
            )
            .await
            .map(|r| r.output)
    }

    #[tokio::test]
    async fn inbox_joins_sessions_automations_and_engine_threads_and_persists_marks() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime =
            Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        let scope = ThreadScope {
            tenant_id: TenantId::new("t1").unwrap(),
            user_id: UserId::new("u1").unwrap(),
            agent_id: None,
            project_id: None,
        };
        engine
            .ensure_thread(&ThreadId::new("chat").unwrap(), &scope, None)
            .await
            .unwrap();
        let surface = OrchestratedProductSurface::new(Arc::new(Inner), Arc::clone(&engine), None);

        let list = list_of(&surface, "u1").await;
        let chat = list.rows.iter().find(|r| r.thread_id == "chat").unwrap();
        assert_eq!(chat.title.as_deref(), Some("Plan the inbox"));
        assert_eq!(chat.profile_id, DEFAULT_PROFILE_ID);
        assert_eq!(chat.engines[0].runtime_id, "ironclaw");
        let auto = list.rows.iter().find(|r| r.thread_id == "auto").unwrap();
        assert!(auto.automation, "automation runs join the inbox");
        assert_eq!(auto.title.as_deref(), Some("Morning brief"));
        assert_eq!(list.profiles[0].profile_id, DEFAULT_PROFILE_ID);

        // Settle the automation run and pin the chat; the answer is the new list.
        let out = invoke(
            &surface,
            INBOX_MUTATE_COMMAND_ID,
            json!({"thread_id": "auto", "action": "settle"}),
        )
        .await
        .unwrap();
        let after: InboxListResponse = serde_json::from_value(out).unwrap();
        let auto = after.rows.iter().find(|r| r.thread_id == "auto").unwrap();
        assert_eq!(auto.state, InboxState::Settled);
        assert_eq!(
            after.rows.last().unwrap().thread_id,
            "auto",
            "settled sorts last"
        );
        invoke(
            &surface,
            INBOX_MUTATE_COMMAND_ID,
            json!({"thread_id": "chat", "action": "pin"}),
        )
        .await
        .unwrap();
        let reread = list_of(&surface, "u1").await;
        assert_eq!(reread.rows[0].thread_id, "chat");
        assert_eq!(reread.rows[0].state, InboxState::Pinned);

        // A thread outside the caller's inbox gets no record.
        let refused = invoke(
            &surface,
            INBOX_MUTATE_COMMAND_ID,
            json!({"thread_id": "someone-else", "action": "pin"}),
        )
        .await;
        assert!(refused.is_err(), "unknown thread is a 404");

        // Goals live on engine threads.
        let out = invoke(
            &surface,
            THREAD_GOAL_SET_COMMAND_ID,
            json!({"thread_id": "chat", "objective": "Ship stage 5"}),
        )
        .await
        .unwrap();
        let goal: ThreadGoalResponse = serde_json::from_value(out).unwrap();
        assert_eq!(goal.goal.unwrap().objective, "Ship stage 5");
        let page = surface
            .query(
                caller("u2"),
                ProductSurfaceQueryRequest {
                    view_id: THREAD_GOAL_VIEW.id.into(),
                    input: json!({"thread_id": "chat"}),
                    cursor: None,
                    limit: None,
                },
            )
            .await;
        assert!(page.is_err(), "another user cannot read the goal");
    }

    #[tokio::test]
    async fn a_thread_is_titled_by_the_task_model_and_retitled_every_three_replies() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime =
            Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        let surface = OrchestratedProductSurface::new(Arc::new(Inner), Arc::clone(&engine), None);
        let before = TASK_CALLS.load(std::sync::atomic::Ordering::SeqCst);

        let out = invoke(
            &surface,
            THREAD_TITLE_COMMAND_ID,
            json!({"thread_id": "chat"}),
        )
        .await
        .unwrap();
        let titled: ThreadTitleResponse = serde_json::from_value(out).unwrap();
        assert_eq!(titled.title.as_deref(), Some("Inbox plan for stage 5"));
        assert!(titled.changed);

        // The inbox now shows it over the first-message title.
        let list = list_of(&surface, "u1").await;
        let chat = list.rows.iter().find(|r| r.thread_id == "chat").unwrap();
        assert_eq!(chat.title.as_deref(), Some("Inbox plan for stage 5"));

        // A second request answers the saved title without a model call.
        let again = invoke(
            &surface,
            THREAD_TITLE_COMMAND_ID,
            json!({"thread_id": "chat"}),
        )
        .await
        .unwrap();
        assert_eq!(again["title"], "Inbox plan for stage 5");
        assert_eq!(again["changed"], false);
        let calls = || TASK_CALLS.load(std::sync::atomic::Ordering::SeqCst) - before;
        assert_eq!(calls(), 1, "no model call until a re-title is due");

        // Two more replies: still not due.
        LATER_EXCHANGES.store(2, std::sync::atomic::Ordering::SeqCst);
        let early = invoke(
            &surface,
            THREAD_TITLE_COMMAND_ID,
            json!({"thread_id": "chat"}),
        )
        .await
        .unwrap();
        assert_eq!(early["title"], "Inbox plan for stage 5");
        assert_eq!(calls(), 1);

        // The third reply since the title: the model re-reads the latest
        // messages and the inbox follows the new topic.
        LATER_EXCHANGES.store(3, std::sync::atomic::Ordering::SeqCst);
        let moved = invoke(
            &surface,
            THREAD_TITLE_COMMAND_ID,
            json!({"thread_id": "chat"}),
        )
        .await
        .unwrap();
        assert_eq!(moved["title"], "Deploy pipeline rollout");
        assert_eq!(moved["changed"], true);
        assert_eq!(calls(), 2);
        let list = list_of(&surface, "u1").await;
        let chat = list.rows.iter().find(|r| r.thread_id == "chat").unwrap();
        assert_eq!(chat.title.as_deref(), Some("Deploy pipeline rollout"));

        // The count restarts from that reply.
        let settled = invoke(
            &surface,
            THREAD_TITLE_COMMAND_ID,
            json!({"thread_id": "chat"}),
        )
        .await
        .unwrap();
        assert_eq!(settled["changed"], false);
        assert_eq!(calls(), 2);

        // A thread the caller cannot read is refused before any model call.
        let refused = invoke(
            &surface,
            THREAD_TITLE_COMMAND_ID,
            json!({"thread_id": "auto"}),
        )
        .await;
        assert!(refused.is_err());
        assert_eq!(calls(), 2);
    }

    #[test]
    fn titles_are_cleaned_to_one_bounded_line() {
        assert_eq!(
            clean_title("  **\"Fix login loop.\"**\nextra").as_deref(),
            Some("Fix login loop")
        );
        assert_eq!(
            clean_title("Title: Budget review").as_deref(),
            Some("Budget review")
        );
        assert_eq!(clean_title("  \n "), None);
        assert_eq!(
            clean_title(&"é".repeat(200)).map(|t| t.chars().count()),
            Some(80)
        );
    }
}
