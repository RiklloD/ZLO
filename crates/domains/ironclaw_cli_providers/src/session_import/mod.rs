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
//! Incremental: the ledger keeps a checkpoint per file: the offset after the
//! last complete line read, the file's identity (device and inode) and a
//! digest of the bytes before the offset. A file that is the same file with
//! the same bytes up to the checkpoint is read from there only; anything
//! else (replaced, truncated, rewritten in place) is read again from the
//! start and reconciled without dropping what was already imported. Codex
//! transcript lines carry the offset of the source line they came from, so
//! an append interrupted before the ledger moved is recognized, not
//! repeated. Once a turn ran on a thread here, the app's own record is
//! authoritative and the import only notes the file as seen.
//!
//! Folders are walked without following symlinks and only regular files are
//! read (`files`), each up to the size it was listed at. A run stops at its
//! [`Limits`] (bytes, time, files listed) and leaves the rest `deferred`.

pub(crate) mod files;
pub(crate) mod lines;
mod projects;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::claude::import as claude_import;
use crate::codex::import as codex_import;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::model::{Millis, RuntimeId, ThreadRecord, ThreadScope, clip, now_ms};
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_orchestrator::session_imports::{
    ImportGrowth, ImportLedgerEntry, ImportWatermark, ImportedThread, SessionImportState,
};
use ironclaw_orchestrator::store::Store;
use ironclaw_product_contracts::session_import::{
    BACKGROUND_RESCAN_MIN_MS, ImportTrigger, ProjectSelection, SessionImportCounts,
    SessionImportRequest, SessionImportSource, SessionImportStatus,
};

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
    /// Imported Claude sessions' folder records (`claude::imports_dir` of
    /// the Claude driver's work dir).
    pub claude_imports: PathBuf,
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
            claude_imports: crate::claude::imports_dir(&home.join(".ironclaw").join("claude-cli")),
            own_roots: own_roots(
                home,
                ["ZLO_HOME", "IRONCLAW_REBORN_HOME"]
                    .into_iter()
                    .filter_map(std::env::var_os)
                    .map(PathBuf::from),
            ),
        }
    }
}

/// This app's own folders: the pre-ZLO `~/.ironclaw`, ZLO's `~/.zlo`, and
/// the home it was started with (`ZLO_HOME`, a custom one included).
fn own_roots(home: &Path, configured: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut roots = vec![home.join(".ironclaw"), home.join(".zlo")];
    for root in configured {
        if root.is_absolute() && !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// A regular file as found on disk.
struct Candidate {
    path: PathBuf,
    native_id: String,
    file: files::FileInfo,
}

/// The ledger's answer for one file, after checking its checkpoint.
enum Known {
    New,
    /// Counted and settled (unchanged, deleted, or continued here).
    Done,
    /// The same file, grown past the checkpoint: read from it.
    Grew(ImportLedgerEntry),
    /// Not the file that was read (replaced, truncated or rewritten in
    /// place): read it again from the start and reconcile.
    Rewritten(ImportLedgerEntry),
}

/// What a session file said, for a new thread or what it added.
#[derive(Debug, Default)]
struct SessionFacts {
    title: Option<String>,
    created: Option<Millis>,
    last: Option<Millis>,
    last_user: Option<Millis>,
    /// Offset after the last complete line read, and the digest before it.
    end: u64,
    fingerprint: String,
    generation: i64,
    messages: u64,
    /// `messages` is the file's whole count (it was read from the start).
    recount: bool,
    /// Reading stopped before the end of the file (a batch or the run's
    /// budget): the file is noted as seen only up to `end`, so the next run
    /// goes on even though the file itself did not change.
    partial: bool,
}

/// How much one run may do. A run that reaches a limit stops; what is left
/// is counted `deferred` and read by the next run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Bytes read from session files.
    pub(crate) bytes: u64,
    pub(crate) time: Duration,
    /// Files listed per source.
    pub(crate) files: usize,
    /// Codex messages folded and written per batch.
    pub(crate) batch: usize,
    /// Entries in the cache of skipped files.
    pub(crate) skip_cache: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: 512 * 1024 * 1024,
            time: Duration::from_secs(120),
            files: 50_000,
            batch: 256,
            skip_cache: 20_000,
        }
    }
}

/// What is left of a run's [`Limits`].
struct Budget {
    bytes: u64,
    deadline: Instant,
}

impl Budget {
    fn spent(&self) -> bool {
        self.bytes == 0 || Instant::now() >= self.deadline
    }

    fn take(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_sub(bytes);
    }
}

/// Points where an import writes; tests fail a run at each one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The Codex transcript copy is written (new import).
    Transcript,
    /// The Codex thread id is saved (new import).
    ThreadFile,
    /// The session's folder record is written (new import).
    Workspace,
    /// A Codex batch was appended, before the ledger moved.
    Appended,
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
    /// The folders new imports are limited to (`SessionImportSettings::projects`).
    projects: Option<BTreeSet<String>>,
    counts: SessionImportCounts,
}

impl Pass<'_> {
    /// A new session started in `cwd` may become a thread.
    fn allowed(&self, cwd: Option<&Path>) -> bool {
        match &self.projects {
            None => true,
            Some(only) => cwd
                .and_then(Path::to_str)
                .is_some_and(|cwd| only.contains(cwd)),
        }
    }
}

pub struct SessionImporter {
    paths: ImportPaths,
    limits: Limits,
    running: AtomicBool,
    /// Files skipped at their current identity, size and mtime (empty, a
    /// subagent's, this app's): not read again until they change. A
    /// bounded cache: a restart, or a full cache, reads them once more.
    skipped: Mutex<HashMap<PathBuf, files::FileInfo>>,
    #[cfg(test)]
    fail_at: Mutex<Option<Step>>,
}

impl SessionImporter {
    pub fn new(paths: ImportPaths) -> Self {
        Self {
            paths,
            limits: Limits::default(),
            running: AtomicBool::new(false),
            skipped: Mutex::new(HashMap::new()),
            #[cfg(test)]
            fail_at: Mutex::new(None),
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
            projects: state.settings.projects.clone(),
            started: false,
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
        let changed = !request.sources.is_empty()
            || request.background.is_some()
            || request.projects.is_some();
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
        if let Some(selection) = &request.projects {
            let projects = match selection {
                ProjectSelection::All => None,
                ProjectSelection::Only(only) => Some(only.clone()),
            };
            if projects != state.settings.projects {
                state.settings.projects = projects;
                // Files skipped for their folder may be wanted now.
                if let Ok(mut skipped) = self.skipped.lock() {
                    skipped.clear();
                }
            }
        }
        if changed {
            store.put_session_import_state(&owner, &state).await?;
        }
        let mut started = false;
        if let Some(trigger) = request.run
            && due(&state, trigger, now_ms(), &scannable(engine, &state))
        {
            started = self.start(Arc::clone(engine), scope.clone(), trigger);
        }
        let mut status = self.status_of(engine, &state);
        status.started = started;
        Ok(status)
    }

    /// Run in the background unless a run is going on already; `true`
    /// when this call started one.
    fn start(
        self: &Arc<Self>,
        engine: Arc<Engine>,
        scope: ThreadScope,
        trigger: ImportTrigger,
    ) -> bool {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            // A run that reached its limits goes on with what it left, as
            // long as each round gets somewhere: a first import finishes
            // now, not one rescan interval at a time.
            let mut total = BTreeMap::new();
            for _ in 0..=FOLLOW_UP_RUNS {
                let round = this.run_held(&engine, &scope, trigger, &total).await;
                total = combined(&total, &round);
                if !continues(&round) {
                    break;
                }
            }
            this.running.store(false, Ordering::SeqCst);
        });
        true
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
        let counts = self
            .run_held(engine, scope, trigger, &BTreeMap::new())
            .await;
        self.running.store(false, Ordering::SeqCst);
        Some(counts)
    }

    /// The run itself, with the running flag held; records the outcome,
    /// added to `earlier` (the rounds of the same request before it), and
    /// answers this round's counts.
    async fn run_held(
        &self,
        engine: &Engine,
        scope: &ThreadScope,
        trigger: ImportTrigger,
        earlier: &BTreeMap<String, SessionImportCounts>,
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
                // Per source: one not scanned this run (its CLI gone, or
                // turned off) keeps its last counts, so it is not "never read".
                state.last_counts.extend(combined(earlier, &counts));
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
            // Each source has a budget of its own: a long Claude Code
            // history does not leave every Codex session deferred.
            let mut budget = Budget {
                bytes: self.limits.bytes,
                deadline: Instant::now() + self.limits.time,
            };
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
                projects: state.settings.projects.clone(),
                counts: SessionImportCounts::default(),
            };
            let candidates = match spec.id {
                CLAUDE_CODE => self.claude_candidates().await,
                _ => self.codex_candidates().await,
            };
            for candidate in candidates {
                pass.counts.found += 1;
                if budget.spent() {
                    pass.counts.deferred += 1;
                    continue;
                }
                let outcome = match spec.id {
                    CLAUDE_CODE => self.claude(&mut pass, &candidate, &mut budget).await,
                    _ => self.codex(&mut pass, &candidate, &mut budget).await,
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

    /// A failure injected at `step` (tests); nothing otherwise.
    fn step(&self, step: Step) -> Result<(), ImportError> {
        #[cfg(test)]
        if self.fail_at.lock().is_ok_and(|f| *f == Some(step)) {
            return Err(ImportError::Injected(step));
        }
        let _ = step;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn fail_at(&self, step: Option<Step>) {
        if let Ok(mut f) = self.fail_at.lock() {
            *f = step;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    fn known_skip(&self, c: &Candidate) -> bool {
        self.skipped
            .lock()
            .map(|m| m.get(&c.path) == Some(&c.file))
            .unwrap_or(false) // silent-ok: a poisoned cache only re-reads files
    }

    fn remember_skip(&self, c: &Candidate) {
        if let Ok(mut m) = self.skipped.lock() {
            if m.len() >= self.limits.skip_cache {
                m.clear();
            }
            m.insert(c.path.clone(), c.file.clone());
        }
    }

    fn own_folder(&self, dir: Option<&Path>) -> bool {
        dir.is_some_and(|d| self.paths.own_roots.iter().any(|r| d.starts_with(r)))
    }

    /// The file, opened for reading only if it is still the one listed.
    async fn open(&self, c: &Candidate) -> Result<tokio::fs::File, ImportError> {
        Ok(files::open_regular(&c.path, &c.file.id).await?)
    }

    /// Lines of the file from `from` up to its listed size. The caller
    /// charges what it consumed to the run's budget.
    async fn lines(&self, c: &Candidate, from: u64) -> Result<lines::Lines, ImportError> {
        let file = self.open(c).await?;
        let limit = c.file.size.saturating_sub(from);
        Ok(lines::Lines::from_file(file, from, limit, lines::MAX_LINE).await?)
    }

    async fn fingerprint(&self, c: &Candidate, offset: u64) -> Result<String, ImportError> {
        let mut file = self.open(c).await?;
        Ok(files::fingerprint(&mut file, offset).await?)
    }

    /// What the ledger says about a file, its checkpoint checked against
    /// the bytes on disk. Settles every case but growth and rewrites.
    async fn known(&self, pass: &mut Pass<'_>, c: &Candidate) -> Result<Known, ImportError> {
        let Some(entry) = pass.ledger.get(&c.native_id).cloned() else {
            return Ok(Known::New);
        };
        let wm = &entry.watermark;
        if !entry.thread_live {
            pass.counts.skipped += 1;
            return Ok(Known::Done);
        }
        let same_meta = wm.size == c.file.size && wm.mtime_ms == c.file.mtime_ms;
        if entry.adopted {
            // Continued here: the app's own record is the thread's; only
            // note the file as seen.
            if !same_meta || wm.file_id != c.file.id {
                let growth = ImportGrowth {
                    watermark: ImportWatermark {
                        size: c.file.size,
                        mtime_ms: c.file.mtime_ms,
                        file_id: c.file.id.clone(),
                        ..wm.clone()
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
            }
            pass.counts.unchanged += 1;
            return Ok(Known::Done);
        }
        if wm.file_id != c.file.id || c.file.size < wm.offset {
            return Ok(Known::Rewritten(entry));
        }
        // A checkpoint check reads one small window; the byte budget is for
        // reading sessions, so it is not charged (time still is).
        if self.fingerprint(c, wm.offset).await? != wm.fingerprint {
            return Ok(Known::Rewritten(entry));
        }
        if same_meta || c.file.size == wm.offset {
            pass.counts.unchanged += 1;
            return Ok(Known::Done);
        }
        Ok(Known::Grew(entry))
    }

    /// Record what was read from a session.
    async fn advance(
        &self,
        pass: &mut Pass<'_>,
        c: &Candidate,
        entry: &ImportLedgerEntry,
        facts: &SessionFacts,
    ) -> Result<(), ImportError> {
        let growth = ImportGrowth {
            watermark: self.watermark(c, facts),
            messages_added: facts.messages,
            recount: facts.recount,
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
        Ok(())
    }

    fn watermark(&self, c: &Candidate, facts: &SessionFacts) -> ImportWatermark {
        ImportWatermark {
            size: if facts.partial {
                facts.end
            } else {
                c.file.size
            },
            mtime_ms: c.file.mtime_ms,
            offset: facts.end,
            file_id: c.file.id.clone(),
            fingerprint: facts.fingerprint.clone(),
            generation: facts.generation,
        }
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
        let created = facts.created.unwrap_or(c.file.mtime_ms);
        record.native_session_id = Some(c.native_id.clone());
        record.provider = Some(pass.runtime.as_str().to_string());
        record.title = facts
            .title
            .as_deref()
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
            watermark: self.watermark(c, &facts),
            messages: facts.messages,
        })
    }
}

/// Rounds a background run may add after one that left work deferred.
const FOLLOW_UP_RUNS: usize = 8;

/// Another round is worth it: work was left, and this round moved.
fn continues(counts: &BTreeMap<String, SessionImportCounts>) -> bool {
    let left = counts.values().any(|c| c.deferred > 0);
    let moved = counts.values().any(|c| c.imported + c.updated > 0);
    left && moved
}

/// A request's counts so far plus one more round: what came in adds up;
/// what was looked at, skipped, failed or is still left is the latest
/// round's.
fn combined(
    earlier: &BTreeMap<String, SessionImportCounts>,
    round: &BTreeMap<String, SessionImportCounts>,
) -> BTreeMap<String, SessionImportCounts> {
    let mut out = earlier.clone();
    for (id, now) in round {
        let before = earlier.get(id).copied().unwrap_or_default();
        out.insert(
            id.clone(),
            SessionImportCounts {
                imported: before.imported + now.imported,
                updated: before.updated + now.updated,
                ..*now
            },
        );
    }
    out
}

/// The sources a run would read now: enabled, their CLI's runtime found.
fn scannable(engine: &Engine, state: &SessionImportState) -> Vec<&'static str> {
    SOURCES
        .iter()
        .filter(|spec| state.settings.enabled(spec.id))
        .filter(|spec| RuntimeId::new(spec.runtime).is_ok_and(|r| engine.has_runtime(&r)))
        .map(|spec| spec.id)
        .collect()
}

/// Whether a run for `trigger` is due. `scannable` names the sources a run
/// would read now (enabled, their CLI found).
///
/// - `Launch`: no run yet, or a scannable source was never read: a first
///   launch that found no CLI is not the launch import, and a CLI found
///   since (installed, or the live rescan) brings its sessions in.
/// - `Background` (with `background` on): the interval passed, a source was
///   never read, or the last run left work deferred.
pub fn due(
    state: &SessionImportState,
    trigger: ImportTrigger,
    now: Millis,
    scannable: &[&str],
) -> bool {
    let unread = scannable
        .iter()
        .any(|id| !state.last_counts.contains_key(*id));
    match trigger {
        ImportTrigger::Manual => true,
        ImportTrigger::Launch => state.last_run_at.is_none() || unread,
        ImportTrigger::Background => {
            state.settings.background
                && (unread
                    || scannable
                        .iter()
                        .any(|id| state.last_counts.get(*id).is_some_and(|c| c.deferred > 0))
                    || state
                        .last_run_at
                        .is_none_or(|at| now - at >= BACKGROUND_RESCAN_MIN_MS))
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
    #[cfg(test)]
    #[error("injected failure at {0:?}")]
    Injected(Step),
}

impl From<ironclaw_host_api::error::HostApiError> for ImportError {
    fn from(e: ironclaw_host_api::error::HostApiError) -> Self {
        Self::Store(e.into())
    }
}

fn jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

mod claude;
mod codex;
#[cfg(test)]
mod tests;
