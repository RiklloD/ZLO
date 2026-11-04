//! Terminal session import: Claude Code and Codex sessions the person ran in
//! a terminal become ordinary threads on the matching CLI runtime, so they
//! can be searched and continued here. Ported in spirit from Goose's
//! `session/import_formats/{claude_code,codex}.rs`.
//!
//! - **Claude Code** (`~/.claude/projects/<dir>/<session>.jsonl`): the
//!   thread's native session id is the transcript's session id. The
//!   transcript stays where it is and is the thread's history, as for every
//!   Claude CLI thread; a message sent here runs `claude --resume <id>` from
//!   the folder the session was started in.
//! - **Codex** (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`): the thread's
//!   session key is the Codex thread id; the rollout is folded into the
//!   driver's own transcript (the history of every Codex thread), and the
//!   saved thread id makes the next turn `thread/resume` it, in its folder.
//!
//! Sessions this app started are skipped (their native ids are already on a
//! thread, or Codex names this app as their originator), as are subagent
//! sessions, empty ones and anything under the app's own home. Every new
//! thread lands in the inbox's Settled shelf with nothing unseen.
//!
//! Incremental: the ledger keeps each file's size, mtime and the offset of
//! the last complete line read. An unchanged file is not opened; a grown one
//! is read from the offset only. Once a turn ran on a thread here, the app's
//! own record is authoritative and the import only moves the watermark.

pub(crate) mod lines;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::model::{Millis, RuntimeId, ThreadRecord, ThreadScope, clip, now_ms};
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_orchestrator::runtime::HistoryMessage;
use ironclaw_orchestrator::session_imports::{
    ImportGrowth, ImportLedgerEntry, ImportWatermark, ImportedThread, SessionImportState,
};
use ironclaw_orchestrator::store::Store;
use ironclaw_product_contracts::session_import::{
    BACKGROUND_RESCAN_MIN_MS, ImportTrigger, SessionImportCounts, SessionImportRequest,
    SessionImportSource, SessionImportStatus,
};
use tokio::io::AsyncWriteExt;

use crate::claude::import as claude_import;
use crate::codex::import as codex_import;

/// Source ids (persisted in the ledger and settings, and on inbox rows).
pub const CLAUDE_CODE: &str = "claude_code";
pub const CODEX: &str = "codex";

const TITLE_CHARS: usize = 80;

/// One importable source: id, label, and the runtime its threads run on.
struct SourceSpec {
    id: &'static str,
    label: &'static str,
    runtime: &'static str,
}

const SOURCES: [SourceSpec; 2] = [
    SourceSpec {
        id: CLAUDE_CODE,
        label: "Claude Code",
        runtime: "claude-cli",
    },
    SourceSpec {
        id: CODEX,
        label: "Codex",
        runtime: "codex",
    },
];

/// Where the sessions and the drivers' state live.
#[derive(Debug, Clone)]
pub struct ImportPaths {
    /// `~/.claude/projects`.
    pub claude_projects: PathBuf,
    /// `$CODEX_HOME/sessions` (`~/.codex/sessions`).
    pub codex_sessions: PathBuf,
    /// The Codex driver's state folder (`codex::state_dir`).
    pub codex_state: PathBuf,
    /// Sessions that ran under these folders are this app's own.
    pub own_roots: Vec<PathBuf>,
}

impl ImportPaths {
    pub fn from_home(home: &Path) -> Self {
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Self {
            claude_projects: home.join(".claude").join("projects"),
            codex_sessions: codex_home.join("sessions"),
            codex_state: crate::codex::state_dir(home),
            own_roots: vec![home.join(".ironclaw")],
        }
    }
}

/// A file as found on disk.
struct Candidate {
    path: PathBuf,
    native_id: String,
    size: u64,
    mtime_ms: Millis,
}

/// The ledger's answer for one file.
enum Known {
    New,
    /// Counted and settled (unchanged, deleted, or only the mark moved).
    Done,
    /// Grew since the watermark: read from it.
    Grew(ImportLedgerEntry),
}

/// What a session file said, for a new thread or its growth.
#[derive(Debug, Default)]
struct SessionFacts {
    title: Option<String>,
    created: Option<Millis>,
    last: Option<Millis>,
    last_user: Option<Millis>,
    /// Offset after the last complete line read.
    end: u64,
    messages: u64,
}

/// What a run needs for one source.
struct Pass<'a> {
    store: &'a Store,
    scope: &'a ThreadScope,
    owner: ProfileOwner,
    source: &'static str,
    runtime: RuntimeId,
    ledger: HashMap<String, ImportLedgerEntry>,
    /// Native ids on threads this app started.
    own: HashSet<String>,
    counts: SessionImportCounts,
}

pub struct SessionImporter {
    paths: ImportPaths,
    running: AtomicBool,
    /// Files skipped at their current size and mtime (empty, a subagent's,
    /// this app's): not read again until they change. A cache: a restart
    /// reads them once more.
    skipped: Mutex<HashMap<PathBuf, (u64, Millis)>>,
}

impl SessionImporter {
    pub fn new(paths: ImportPaths) -> Self {
        Self {
            paths,
            running: AtomicBool::new(false),
            skipped: Mutex::new(HashMap::new()),
        }
    }

    /// For the process's `HOME`; `None` without one.
    pub fn from_env() -> Option<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        Some(Self::new(ImportPaths::from_home(&home)))
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn status(
        &self,
        engine: &Engine,
        owner: &ProfileOwner,
    ) -> Result<SessionImportStatus, OrchestratorError> {
        let state = engine.store().session_import_state(owner).await?;
        Ok(self.status_of(engine, &state))
    }

    fn status_of(&self, engine: &Engine, state: &SessionImportState) -> SessionImportStatus {
        let sources = SOURCES
            .iter()
            .map(|spec| SessionImportSource {
                source: spec.id.to_string(),
                label: spec.label.to_string(),
                enabled: state.settings.enabled(spec.id),
                available: RuntimeId::new(spec.runtime).is_ok_and(|r| engine.has_runtime(&r)),
                counts: state.last_counts.get(spec.id).copied(),
            })
            .collect();
        SessionImportStatus {
            sources,
            background: state.settings.background,
            running: self.is_running(),
            last_run_at: state.last_run_at,
            last_trigger: state.last_trigger,
            last_error: state.last_error.clone(),
        }
    }

    /// Apply the settings in `request`, then start a run in the background
    /// when it asks for one and the run is due. Answers the status after.
    pub async fn request(
        self: &Arc<Self>,
        engine: &Arc<Engine>,
        scope: &ThreadScope,
        request: &SessionImportRequest,
    ) -> Result<SessionImportStatus, OrchestratorError> {
        let owner = ProfileOwner::from(scope);
        let store = engine.store();
        let mut state = store.session_import_state(&owner).await?;
        let changed = !request.sources.is_empty() || request.background.is_some();
        for (source, on) in &request.sources {
            if !SOURCES.iter().any(|s| s.id == source) {
                return Err(OrchestratorError::invalid(format!(
                    "unknown import source {source:?}"
                )));
            }
            state.settings.sources.insert(source.clone(), *on);
        }
        if let Some(background) = request.background {
            state.settings.background = background;
        }
        if changed {
            store.put_session_import_state(&owner, &state).await?;
        }
        if let Some(trigger) = request.run
            && due(&state, trigger, now_ms())
        {
            self.start(Arc::clone(engine), scope.clone(), trigger);
        }
        Ok(self.status_of(engine, &state))
    }

    /// Run in the background unless a run is going on already.
    fn start(self: &Arc<Self>, engine: Arc<Engine>, scope: ThreadScope, trigger: ImportTrigger) {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_held(&engine, &scope, trigger).await;
            this.running.store(false, Ordering::SeqCst);
        });
    }

    /// Run now and wait for it; `None` when another run is going on.
    pub async fn run(
        &self,
        engine: &Engine,
        scope: &ThreadScope,
        trigger: ImportTrigger,
    ) -> Option<BTreeMap<String, SessionImportCounts>> {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        let counts = self.run_held(engine, scope, trigger).await;
        self.running.store(false, Ordering::SeqCst);
        Some(counts)
    }

    /// The run itself, with the running flag held; records the outcome.
    async fn run_held(
        &self,
        engine: &Engine,
        scope: &ThreadScope,
        trigger: ImportTrigger,
    ) -> BTreeMap<String, SessionImportCounts> {
        let owner = ProfileOwner::from(scope);
        let store = engine.store();
        let started = now_ms();
        let result = self.scan_all(engine, scope).await;
        let (counts, error) = match result {
            Ok(counts) => (counts, None),
            Err(error) => {
                tracing::debug!(%error, "session import failed");
                (
                    BTreeMap::new(),
                    Some("The import could not finish.".to_string()),
                )
            }
        };
        // Settings may have changed while it ran: re-read before writing.
        match store.session_import_state(&owner).await {
            Ok(mut state) => {
                state.last_run_at = Some(started);
                state.last_trigger = Some(trigger);
                state.last_error = error;
                if !counts.is_empty() {
                    state.last_counts = counts.clone();
                }
                if let Err(error) = store.put_session_import_state(&owner, &state).await {
                    tracing::debug!(%error, "session import: run not recorded");
                }
            }
            Err(error) => tracing::debug!(%error, "session import: state unreadable"),
        }
        tracing::debug!(?counts, "session import finished");
        counts
    }

    async fn scan_all(
        &self,
        engine: &Engine,
        scope: &ThreadScope,
    ) -> Result<BTreeMap<String, SessionImportCounts>, OrchestratorError> {
        let owner = ProfileOwner::from(scope);
        let store = engine.store();
        let state = store.session_import_state(&owner).await?;
        let mut out = BTreeMap::new();
        for spec in &SOURCES {
            let runtime = RuntimeId::new(spec.runtime)?;
            if !state.settings.enabled(spec.id) || !engine.has_runtime(&runtime) {
                continue;
            }
            let mut own = store
                .native_sessions(std::slice::from_ref(&runtime))
                .await?;
            if spec.id == CODEX {
                own.extend(self.codex_thread_ids().await);
            }
            let mut pass = Pass {
                store,
                scope,
                owner: owner.clone(),
                source: spec.id,
                runtime,
                ledger: store.import_ledger(&owner, spec.id).await?,
                own,
                counts: SessionImportCounts::default(),
            };
            let candidates = match spec.id {
                CLAUDE_CODE => self.claude_candidates().await,
                _ => self.codex_candidates().await,
            };
            for candidate in candidates {
                pass.counts.found += 1;
                let outcome = match spec.id {
                    CLAUDE_CODE => self.claude(&mut pass, &candidate).await,
                    _ => self.codex(&mut pass, &candidate).await,
                };
                if let Err(error) = outcome {
                    tracing::debug!(%error, source = spec.id, "session import: file not imported");
                    pass.counts.failed += 1;
                }
                tokio::task::yield_now().await;
            }
            out.insert(spec.id.to_string(), pass.counts);
        }
        Ok(out)
    }

    // ── shared steps ────────────────────────────────────────────────────

    fn known_skip(&self, c: &Candidate) -> bool {
        self.skipped
            .lock()
            .map(|m| m.get(&c.path) == Some(&(c.size, c.mtime_ms)))
            .unwrap_or(false) // silent-ok: a poisoned cache only re-reads files
    }

    fn remember_skip(&self, c: &Candidate) {
        if let Ok(mut m) = self.skipped.lock() {
            m.insert(c.path.clone(), (c.size, c.mtime_ms));
        }
    }

    fn own_folder(&self, dir: Option<&Path>) -> bool {
        dir.is_some_and(|d| self.paths.own_roots.iter().any(|r| d.starts_with(r)))
    }

    /// What the ledger says about a file. Settles every case but growth.
    async fn known(&self, pass: &mut Pass<'_>, c: &Candidate) -> Result<Known, ImportError> {
        let Some(entry) = pass.ledger.get(&c.native_id).cloned() else {
            return Ok(Known::New);
        };
        let wm = entry.watermark;
        if !entry.thread_live {
            pass.counts.skipped += 1;
        } else if wm.size == c.size && wm.mtime_ms == c.mtime_ms {
            pass.counts.unchanged += 1;
        } else if entry.adopted || c.size < wm.offset {
            // Continued here (the app's record wins), or rewritten in place
            // (re-reading could duplicate messages): only move the mark.
            let growth = ImportGrowth {
                watermark: ImportWatermark {
                    size: c.size,
                    mtime_ms: c.mtime_ms,
                    offset: c.size,
                },
                ..ImportGrowth::default()
            };
            pass.store
                .advance_import(
                    &pass.owner,
                    pass.source,
                    &c.native_id,
                    &entry.thread_id,
                    &growth,
                )
                .await?;
            pass.counts.unchanged += 1;
        } else {
            return Ok(Known::Grew(entry));
        }
        Ok(Known::Done)
    }

    /// Record growth read from a session (`added` new messages).
    async fn advance(
        &self,
        pass: &mut Pass<'_>,
        c: &Candidate,
        entry: &ImportLedgerEntry,
        facts: &SessionFacts,
    ) -> Result<(), ImportError> {
        let growth = ImportGrowth {
            watermark: ImportWatermark {
                size: c.size,
                mtime_ms: c.mtime_ms,
                offset: facts.end,
            },
            messages_added: facts.messages,
            last_user_at: facts.last_user,
            last_at: facts.last,
        };
        pass.store
            .advance_import(
                &pass.owner,
                pass.source,
                &c.native_id,
                &entry.thread_id,
                &growth,
            )
            .await?;
        if facts.messages > 0 {
            pass.counts.updated += 1;
        } else {
            pass.counts.unchanged += 1;
        }
        Ok(())
    }

    fn thread_id(&self, pass: &Pass<'_>, native_id: &str) -> Result<ThreadId, ImportError> {
        let key = format!(
            "session-import:{}:{}:{}:{native_id}",
            pass.owner.tenant_id.as_str(),
            pass.owner.user_id.as_str(),
            pass.source
        );
        let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, key.as_bytes());
        Ok(ThreadId::new(id.to_string())?)
    }

    fn record(
        &self,
        pass: &Pass<'_>,
        c: &Candidate,
        facts: SessionFacts,
    ) -> Result<ImportedThread, ImportError> {
        let mut record = ThreadRecord::new(
            self.thread_id(pass, &c.native_id)?,
            pass.scope.clone(),
            pass.runtime.clone(),
        );
        let created = facts.created.unwrap_or(c.mtime_ms);
        record.native_session_id = Some(c.native_id.clone());
        record.provider = Some(pass.runtime.as_str().to_string());
        record.title = facts
            .title
            .map(|t| t.lines().next().unwrap_or_default().trim().to_string())
            .filter(|t| !t.is_empty())
            .map(|t| clip(&t, TITLE_CHARS));
        record.created_at = created;
        record.updated_at = facts.last.unwrap_or(created).max(created);
        record.last_user_at = facts.last_user;
        Ok(ImportedThread {
            record,
            source: pass.source.to_string(),
            native_id: c.native_id.clone(),
            path: c.path.display().to_string(),
            watermark: ImportWatermark {
                size: c.size,
                mtime_ms: c.mtime_ms,
                offset: facts.end,
            },
            messages: facts.messages,
        })
    }
}

/// Whether a run for `trigger` is due.
pub fn due(state: &SessionImportState, trigger: ImportTrigger, now: Millis) -> bool {
    match trigger {
        ImportTrigger::Manual => true,
        ImportTrigger::Launch => state.last_run_at.is_none(),
        ImportTrigger::Background => {
            state.settings.background
                && state
                    .last_run_at
                    .is_none_or(|at| now - at >= BACKGROUND_RESCAN_MIN_MS)
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ImportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] OrchestratorError),
    #[error("encode: {0}")]
    Encode(#[from] serde_json::Error),
}

impl From<ironclaw_host_api::error::HostApiError> for ImportError {
    fn from(e: ironclaw_host_api::error::HostApiError) -> Self {
        Self::Store(e.into())
    }
}

async fn stat(path: &Path) -> Option<(u64, Millis)> {
    let meta = tokio::fs::metadata(path).await.ok()?; // silent-ok: a file that vanished mid-scan is simply not found
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some((meta.len(), i64::try_from(mtime).unwrap_or(i64::MAX)))
}

async fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return out; // silent-ok: a CLI that never ran has no sessions folder
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        out.push(entry.path());
    }
    out.sort();
    out
}

fn jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

/// Write `messages` as journal lines, replacing the file or appending.
async fn write_journal(
    path: &Path,
    messages: &[HistoryMessage],
    append: bool,
) -> Result<(), ImportError> {
    let mut body = String::new();
    for m in messages {
        body.push_str(&serde_json::to_string(m)?);
        body.push('\n');
    }
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .await?;
    file.write_all(body.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

mod claude;
mod codex;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_runs_once_and_background_waits_its_interval() {
        let mut state = SessionImportState::default();
        assert!(due(&state, ImportTrigger::Launch, 0));
        assert!(due(&state, ImportTrigger::Background, 0));
        state.last_run_at = Some(1_000);
        assert!(!due(&state, ImportTrigger::Launch, 2_000));
        assert!(!due(&state, ImportTrigger::Background, 2_000));
        assert!(due(
            &state,
            ImportTrigger::Background,
            1_000 + BACKGROUND_RESCAN_MIN_MS
        ));
        assert!(due(&state, ImportTrigger::Manual, 2_000));
        state.settings.background = false;
        assert!(!due(
            &state,
            ImportTrigger::Background,
            1_000 + BACKGROUND_RESCAN_MIN_MS
        ));
    }
}
