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
use std::sync::{Arc, Mutex as StdMutex, PoisonError, Weak};

use chrono::{DateTime, Utc};
use ironclaw_host_api::ids::{ActivityId, ThreadId};
use ironclaw_orchestrator::engine::{Engine, TurnCompletedListener};
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::inbox::{InboxFacts, InboxRecord, SavedTitle, classify};
use ironclaw_orchestrator::model::{Millis, ThreadRecord, now_ms};
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
    TaskCompletionResult,
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
                title_pending: ThreadId::new(thread_id.as_str())
                    .is_ok_and(|id| engine.after_turn_busy(&id)),
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
const TITLE_MAX_CHARS: usize = 80;
/// Each message given to the title model, and how many of them.
const TITLE_MESSAGE_CHARS: usize = 600;
const TITLE_MESSAGES: usize = 6;
/// Timeline pages read back to find them past long tool runs.
const TITLE_PAGE_SIZE: u32 = 200;
const TITLE_MAX_PAGES: usize = 5;
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
    content: Option<String>,
}

#[derive(Deserialize, Default)]
struct TitleTimeline {
    #[serde(default)]
    messages: Vec<TitleMessage>,
    #[serde(default)]
    next_cursor: Option<String>,
}

fn clip_chars(text: &str, max: usize) -> &str {
    text.char_indices()
        .nth(max)
        .map_or(text, |(at, _)| &text[..at])
}

/// The text of a user message or reply; tool and system rows have none.
fn exchange_text(m: &TitleMessage) -> Option<&str> {
    if !matches!(m.kind, TitleMessageKind::User | TitleMessageKind::Assistant) {
        return None;
    }
    m.content
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
}

/// The current title, if any, and the latest messages, oldest first.
fn title_prompt(recent: &[TitleMessage], current: Option<&str>) -> String {
    let mut prompt = match current {
        Some(title) => format!("Current title:\n{title}\n\nLatest messages:"),
        None => "Messages:".to_string(),
    };
    for m in recent {
        let who = if m.kind == TitleMessageKind::User {
            "User"
        } else {
            "Assistant"
        };
        let text = exchange_text(m).unwrap_or_default();
        prompt.push_str(&format!(
            "\n\n{who}:\n{}",
            clip_chars(text, TITLE_MESSAGE_CHARS)
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

/// The last [`TITLE_MESSAGES`] user messages and replies, oldest first,
/// paging back through the timeline past tool rows. The read is scoped to
/// the thread's owner.
async fn recent_exchange(
    surface: &dyn ProductSurface,
    caller: &ProductSurfaceCaller,
    thread: &ThreadId,
) -> Result<Vec<TitleMessage>, ProductSurfaceError> {
    let mut recent: Vec<TitleMessage> = Vec::new();
    let mut cursor = None;
    for _ in 0..TITLE_MAX_PAGES {
        let page = surface
            .query(
                caller.clone(),
                ProductSurfaceQueryRequest {
                    view_id: TIMELINE_VIEW.id.to_string(),
                    input: serde_json::json!({
                        "thread_id": thread.as_str(),
                        "limit": TITLE_PAGE_SIZE,
                    }),
                    cursor: cursor.take(),
                    limit: None,
                },
            )
            .await?;
        let next_page = page.next_cursor;
        let timeline = page
            .items
            .into_iter()
            .next()
            .map(serde_json::from_value::<TitleTimeline>)
            .transpose()
            .map_err(ProductSurfaceError::internal_from)?
            .unwrap_or_default(); // silent-ok: no page item is a thread with no messages yet
        // Each page runs oldest first and is older than the one before.
        let mut older: Vec<TitleMessage> = timeline
            .messages
            .into_iter()
            .filter(|m| exchange_text(m).is_some())
            .collect();
        older.append(&mut recent);
        recent = older;
        cursor = timeline.next_cursor.or(next_page);
        if recent.len() >= TITLE_MESSAGES || cursor.is_none() {
            break;
        }
    }
    let keep = recent.len().saturating_sub(TITLE_MESSAGES);
    recent.drain(..keep);
    Ok(recent)
}

/// Titles threads on the "Thread titles" task model as their turns
/// complete: after the first reply, then every [`RETITLE_EVERY_REPLIES`]
/// replies, so the title follows the topic. Replies are counted from the
/// orchestrator's turn log, so the count survives capped CLI histories and
/// provider switches, and titles come with no window open.
pub(super) struct ThreadTitler {
    engine: Weak<Engine>,
    /// The orchestrating surface: its timeline reads stitch CLI and
    /// switched-provider threads.
    surface: Weak<dyn ProductSurface>,
    /// Threads with a title call running, and whether another turn
    /// completed meanwhile (the call then runs again once it ends).
    running: StdMutex<HashMap<ThreadId, bool>>,
}

/// Replies this recent are checked at startup: the journal replays turns
/// that ended during a crash before the titler is wired.
const STARTUP_SCAN_MS: Millis = 24 * 60 * 60 * 1000;

impl ThreadTitler {
    fn new(engine: Weak<Engine>, surface: Weak<dyn ProductSurface>) -> Self {
        Self {
            engine,
            surface,
            running: StdMutex::new(HashMap::new()),
        }
    }

    fn running(&self) -> std::sync::MutexGuard<'_, HashMap<ThreadId, bool>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run the title check for `thread`, or mark it for one more pass when
    /// a check is already running.
    fn schedule(self: &Arc<Self>, thread: ThreadRecord) {
        {
            let mut running = self.running();
            if let Some(again) = running.get_mut(&thread.thread_id) {
                *again = true;
                return;
            }
            running.insert(thread.thread_id.clone(), false);
        }
        let titler = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if let Err(error) = titler.title_if_due(&thread).await {
                    tracing::debug!(?error, "thread title skipped");
                }
                let mut running = titler.running();
                match running.get_mut(&thread.thread_id) {
                    Some(again) if *again => *again = false,
                    _ => {
                        running.remove(&thread.thread_id);
                        break;
                    }
                }
            }
        });
    }

    /// Check every thread with a recent reply once.
    async fn scan_recent(self: Arc<Self>) -> Result<(), OrchestratorError> {
        let Some(engine) = self.engine.upgrade() else {
            return Ok(()); // silent-ok: the backend is shutting down
        };
        let store = engine.store();
        for id in store
            .threads_replied_since(now_ms() - STARTUP_SCAN_MS)
            .await?
        {
            if let Some(thread) = store.thread(&id).await? {
                self.schedule(thread);
            }
        }
        Ok(())
    }

    /// Title the thread now if it is due. `Ok(None)`: nothing was due, no
    /// model is set, or another writer saved first.
    async fn title_if_due(
        &self,
        thread: &ThreadRecord,
    ) -> Result<Option<String>, ProductSurfaceError> {
        let (Some(engine), Some(surface)) = (self.engine.upgrade(), self.surface.upgrade()) else {
            return Ok(None); // silent-ok: the backend is shutting down
        };
        let store = engine.store();
        let id = &thread.thread_id;
        // A delegated child keeps the title its parent gave it.
        if store
            .task_for_child(id)
            .await
            .map_err(ProductSurfaceError::internal_from)?
            .is_some()
        {
            return Ok(None);
        }
        let owner = ProfileOwner::from(&thread.scope);
        let saved = store
            .inbox_title(&owner, id)
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        let replies = store
            .completed_replies(id)
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        let due = match &saved {
            None => replies >= 1,
            // A title saved before the count existed is checked now.
            Some(s) => s
                .replies
                .is_none_or(|at| replies - at >= RETITLE_EVERY_REPLIES as i64),
        };
        if !due {
            return Ok(None);
        }
        let caller = ProductSurfaceCaller::new(
            thread.scope.tenant_id.clone(),
            thread.scope.user_id.clone(),
            thread.scope.agent_id.clone(),
            thread.scope.project_id.clone(),
        );
        let recent = recent_exchange(surface.as_ref(), &caller, id).await?;
        let has = |kind| recent.iter().any(|m| m.kind == kind);
        // A first title needs the person's words, not the reply alone.
        if !has(TitleMessageKind::Assistant) || (saved.is_none() && !has(TitleMessageKind::User)) {
            return Ok(None);
        }
        let completion = TaskCompletionRequest {
            task: BackgroundTask::ThreadTitles,
            system: TITLE_SYSTEM_PROMPT.to_string(),
            prompt: title_prompt(&recent, saved.as_ref().map(|s| s.title.as_str())),
            max_tokens: TITLE_MAX_TOKENS,
        };
        let response = surface
            .invoke(
                caller,
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
        // No answer: keep what is saved; the next reply tries again.
        let Some(title) = result.text.as_deref().and_then(clean_title) else {
            return Ok(None);
        };
        let next = SavedTitle {
            title,
            replies: Some(replies),
        };
        let read = saved.and_then(|s| s.replies);
        let written = store
            .inbox_set_title(&owner, id, &next, read, now_ms())
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        Ok(written.then_some(next.title))
    }
}

/// Title `engine`'s threads as their turns complete, reading through the
/// orchestrating `surface`. Weak handles: the engine keeps the listener for
/// the life of the process.
pub(super) fn title_on_completion(
    engine: &Arc<Engine>,
    surface: &Arc<dyn ProductSurface>,
) -> Arc<ThreadTitler> {
    let titler = Arc::new(ThreadTitler::new(
        Arc::downgrade(engine),
        Arc::downgrade(surface),
    ));
    engine.set_turn_listener(Arc::new(TitleOnCompletion(Arc::clone(&titler))));
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let scan = Arc::clone(&titler);
        runtime.spawn(async move {
            if let Err(error) = scan.scan_recent().await {
                tracing::warn!(%error, "startup title scan failed");
            }
        });
    }
    titler
}

/// Spawns the title work so the journal observer is not held up.
struct TitleOnCompletion(Arc<ThreadTitler>);

impl TurnCompletedListener for TitleOnCompletion {
    fn turn_completed(&self, thread: &ThreadRecord) {
        self.0.schedule(thread.clone());
    }

    fn busy(&self, thread: &ThreadId) -> bool {
        self.0.running().contains_key(thread)
    }
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
    use ironclaw_orchestrator::runtime::{TurnEventSink, TurnOutcome};
    use ironclaw_product_contracts::surface::{
        ProductSurfaceInvokeRequest, ProductSurfaceInvokeResponse, ProductSurfaceQueryPage,
        ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
    };
    use serde_json::json;

    use super::*;
    use crate::orchestration::OrchestratedProductSurface;

    /// The undecorated surface: one session thread and one automation run.
    /// The task model answers a fixed title and counts its calls.
    struct Inner;

    static TASK_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    /// Exchanges about deploys after the first one in the "chat" timeline.
    static LATER_EXCHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    /// The task model takes its time, so turns can complete during a call.
    static SLOW_MODEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
            if SLOW_MODEL.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            let asked: TaskCompletionRequest = serde_json::from_value(request.input).unwrap();
            assert_eq!(asked.task, BackgroundTask::ThreadTitles);
            let text = if asked.prompt.starts_with("Current title:") {
                // Only the latest messages: the first exchange is out of range.
                assert!(!asked.prompt.contains("Plan the inbox for stage 5"));
                // A re-title reads the saved title and the latest messages.
                assert!(
                    asked
                        .prompt
                        .contains("Current title:\nInbox plan for stage 5")
                        || asked
                            .prompt
                            .contains("Current title:\nDeploy pipeline rollout")
                );
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
                    // Newest page: a long tool run, then the later exchanges.
                    // The first exchange is one page older.
                    if request.cursor.as_deref() == Some("older") {
                        json!({"messages": [
                            json!({"kind": "system", "sequence": 1, "content": "setup"}),
                            json!({"kind": "user", "sequence": 2, "content": "Plan the inbox for stage 5"}),
                        ]})
                    } else {
                        let mut messages: Vec<Value> = (0..40)
                            .map(|n| {
                                json!({"kind": "tool_result", "sequence": 3 + n,
                                "content": "{\"ok\":true}"})
                            })
                            .collect();
                        messages.push(json!({"kind": "assistant", "sequence": 50,
                            "content": "Here is a plan"}));
                        for n in 1..=LATER_EXCHANGES.load(std::sync::atomic::Ordering::SeqCst) {
                            messages.push(json!({"kind": "user", "sequence": 50 + 2 * n,
                                "content": format!("Ship the deploy {n}")}));
                            messages.push(json!({"kind": "assistant", "sequence": 51 + 2 * n,
                                "content": "Deploying"}));
                        }
                        json!({"messages": messages, "next_cursor": "older"})
                    }
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
    async fn completed_turns_title_a_thread_and_retitle_it_every_three_replies() {
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
        let chat = ThreadId::new("chat").unwrap();
        engine.ensure_thread(&chat, &scope, None).await.unwrap();
        let orchestrated = Arc::new(OrchestratedProductSurface::new(
            Arc::new(Inner),
            Arc::clone(&engine),
            None,
        ));
        let surface: Arc<dyn ProductSurface> = orchestrated.clone();
        let before = TASK_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        let calls = || TASK_CALLS.load(std::sync::atomic::Ordering::SeqCst) - before;
        let title = || async {
            let list = list_of(&orchestrated, "u1").await;
            let row = list
                .rows
                .into_iter()
                .find(|r| r.thread_id == "chat")
                .unwrap();
            (row.title, row.state)
        };
        let finish = |run: &str, outcome: TurnOutcome| {
            let (engine, chat, run) = (Arc::clone(&engine), chat.clone(), run.to_string());
            async move {
                engine
                    .turn_finished(&chat, Some(run), outcome, now_ms())
                    .await
            }
        };
        let settled = |titler: Arc<ThreadTitler>| async move {
            for _ in 0..300 {
                if titler.running().is_empty() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("title work did not finish");
        };

        // Pinned first: a pinned thread never shows as Working in the
        // sidebar, and is titled all the same.
        invoke(
            &orchestrated,
            INBOX_MUTATE_COMMAND_ID,
            json!({"thread_id": "chat", "action": "pin"}),
        )
        .await
        .unwrap();

        // Replayed at startup before the titler is wired: a failed turn
        // (not a reply) and the first reply.
        finish(
            "r0",
            TurnOutcome::ended(ironclaw_orchestrator::runtime::TurnEndStatus::Failed, None),
        )
        .await;
        finish("r1", TurnOutcome::completed(None)).await;
        assert_eq!(calls(), 0);

        // Wiring scans recent replies: titled from the first exchange,
        // which sits a page behind a long tool run. The pin is kept.
        let titler = title_on_completion(&engine, &surface);
        for _ in 0..300 {
            if calls() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        settled(Arc::clone(&titler)).await;
        // One completed turn, as the journal observer reports it; waits for
        // the spawned title work.
        let reply = |run: &str| {
            let (finish, settled, titler) = (finish, settled, Arc::clone(&titler));
            let run = run.to_string();
            async move {
                finish(&run, TurnOutcome::completed(None)).await;
                settled(titler).await;
            }
        };
        assert_eq!(calls(), 1);
        assert_eq!(
            title().await,
            (
                Some("Inbox plan for stage 5".to_string()),
                InboxState::Pinned
            )
        );

        // Two more replies, one logged three times by a replay: not due,
        // no model call.
        LATER_EXCHANGES.store(2, std::sync::atomic::Ordering::SeqCst);
        reply("r2").await;
        reply("r3").await;
        reply("r3").await;
        reply("r3").await;
        assert_eq!(calls(), 1);

        // The third reply since the title: the model re-reads the latest
        // messages and the inbox follows the new topic.
        LATER_EXCHANGES.store(3, std::sync::atomic::Ordering::SeqCst);
        reply("r4").await;
        assert_eq!(calls(), 2);
        assert_eq!(title().await.0.as_deref(), Some("Deploy pipeline rollout"));

        // A slower request that read the title before that one cannot
        // overwrite it or move its count back.
        let owner = ProfileOwner::from(&scope);
        let stale = SavedTitle {
            title: "Inbox plan, stale".into(),
            replies: Some(2),
        };
        let written = engine
            .store()
            .inbox_set_title(&owner, &chat, &stale, Some(1), now_ms())
            .await
            .unwrap();
        assert!(!written);
        let saved = engine
            .store()
            .inbox_title(&owner, &chat)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.title, "Deploy pipeline rollout");
        assert_eq!(saved.replies, Some(4));

        // The count restarts from that reply.
        reply("r5").await;
        reply("r6").await;
        assert_eq!(calls(), 2);

        // Reply 7 is due and its call is slow. Replies 8 to 10 complete
        // during it; the sidebar sees the title pending; once the call
        // ends, the re-title due at reply 10 still runs.
        SLOW_MODEL.store(true, std::sync::atomic::Ordering::SeqCst);
        finish("r7", TurnOutcome::completed(None)).await;
        for _ in 0..300 {
            if calls() == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        for run in ["r8", "r9", "r10"] {
            finish(run, TurnOutcome::completed(None)).await;
        }
        let pending = list_of(&orchestrated, "u1").await;
        assert!(
            pending
                .rows
                .iter()
                .any(|r| r.thread_id == "chat" && r.title_pending)
        );
        settled(Arc::clone(&titler)).await;
        SLOW_MODEL.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(calls(), 4);
        let saved = engine
            .store()
            .inbox_title(&owner, &chat)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.replies, Some(10));
        let done = list_of(&orchestrated, "u1").await;
        assert!(done.rows.iter().all(|r| !r.title_pending));
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
