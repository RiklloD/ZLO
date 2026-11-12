//! The operator console of a thread for a client on another computer
//! (`ironclaw_product_contracts::thread_repo`): one `sh -c` command at a
//! time in the thread's folder, its output kept in a bounded ring of lines
//! the client polls with a cursor, Stop to kill it.
//!
//! Commands run through the host's [`HostCommandRunner`] — composition
//! plugs one in only where the agent itself gets a host shell (a
//! single-user local deployment), with the host process port's scrubbed
//! environment; without it the console reports `available: false` and
//! refuses to start. Stop, the timeout, and shutdown kill the command's
//! whole process tree. Output is not persisted, logged or shown to a model:
//! it goes back only to the operator who typed the command.
// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::thread_repo::*;
use tokio::sync::oneshot;

use crate::engine::Engine;
use crate::error::OrchestratorError;
use crate::model::ThreadRecord;
use crate::worktree::{HostCommand, HostCommandEvent, HostCommandProcess, HostCommandRunner};

type Result<T> = std::result::Result<T, OrchestratorError>;

/// A console command runs at most this long.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Commands running at once across all threads.
const RUNNING_MAX: usize = 8;

#[derive(Default)]
struct Session {
    lines: VecDeque<ConsoleLine>,
    last_seq: u64,
    command: Option<String>,
    exit_code: Option<i32>,
    /// The running command, until its pump has recorded the exit.
    run: Option<Run>,
    /// Counts starts, so a late pump never ends a newer command's run.
    generation: u64,
    folder: Option<PathBuf>,
}

struct Run {
    generation: u64,
    /// Ends the command; taken by the first Stop.
    stop: Option<oneshot::Sender<()>>,
}

impl Session {
    fn push(&mut self, kind: ConsoleLineKind, mut text: String) {
        if text.len() > CONSOLE_LINE_MAX_BYTES {
            let mut cut = CONSOLE_LINE_MAX_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str(" …");
        }
        self.last_seq += 1;
        self.lines.push_back(ConsoleLine {
            seq: self.last_seq,
            kind,
            text,
        });
        while self.lines.len() > CONSOLE_LINES_MAX {
            self.lines.pop_front();
        }
    }

    fn view(&self, after: Option<u64>, available: bool) -> ThreadConsoleView {
        let after = after.unwrap_or(0);
        let first_kept = self.lines.front().map_or(self.last_seq + 1, |l| l.seq);
        let dropped = first_kept.saturating_sub(after + 1);
        let lines: Vec<ConsoleLine> = self
            .lines
            .iter()
            .filter(|l| l.seq > after)
            .take(CONSOLE_PAGE_LINES)
            .cloned()
            .collect();
        ThreadConsoleView {
            available,
            folder: self
                .folder
                .as_ref()
                .map(|f| f.to_string_lossy().into_owned()),
            running: self.run.is_some(),
            command: self.command.clone(),
            exit_code: self.exit_code,
            next: lines.last().map_or(after.max(self.last_seq), |l| l.seq),
            lines,
            dropped,
        }
    }
}

/// Every thread's console, plus the runner that executes them.
#[derive(Default)]
pub(crate) struct Consoles {
    runner: OnceLock<Arc<dyn HostCommandRunner>>,
    sessions: Arc<Mutex<HashMap<ThreadId, Session>>>,
}

fn poisoned(e: impl std::fmt::Display) -> OrchestratorError {
    OrchestratorError::Store(format!("console sessions: {e}"))
}

/// Splits a byte stream into lines for one session.
struct LineSplitter {
    kind: ConsoleLineKind,
    partial: Vec<u8>,
}

impl LineSplitter {
    fn new(kind: ConsoleLineKind) -> Self {
        Self {
            kind,
            partial: Vec::new(),
        }
    }

    /// Whole lines in `bytes` (and an over-long partial one, cut).
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                out.push(self.take());
            } else {
                self.partial.push(b);
                if self.partial.len() >= CONSOLE_LINE_MAX_BYTES {
                    out.push(self.take());
                }
            }
        }
        out
    }

    fn take(&mut self) -> String {
        let line = String::from_utf8_lossy(&self.partial)
            .trim_end_matches('\r')
            .to_string();
        self.partial.clear();
        line
    }

    fn finish(&mut self) -> Option<String> {
        (!self.partial.is_empty()).then(|| self.take())
    }
}

async fn pump(
    sessions: Arc<Mutex<HashMap<ThreadId, Session>>>,
    thread: ThreadId,
    generation: u64,
    mut process: Box<dyn HostCommandProcess>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut out = LineSplitter::new(ConsoleLineKind::Out);
    let mut err = LineSplitter::new(ConsoleLineKind::Err);
    let mut stopped = false;
    let append = |kind: ConsoleLineKind, lines: Vec<String>| {
        if lines.is_empty() {
            return;
        }
        match sessions.lock() {
            Ok(mut map) => {
                if let Some(session) = map.get_mut(&thread) {
                    for line in lines {
                        session.push(kind, line);
                    }
                }
            }
            Err(error) => tracing::debug!(%error, "console sessions poisoned"),
        }
    };
    let ended = loop {
        let event = tokio::select! {
            event = process.next() => event,
            _ = &mut stop, if !stopped => {
                stopped = true;
                process.kill();
                continue;
            }
        };
        match event {
            Some(HostCommandEvent::Stdout(bytes)) => append(out.kind, out.feed(&bytes)),
            Some(HostCommandEvent::Stderr(bytes)) => append(err.kind, err.feed(&bytes)),
            Some(HostCommandEvent::Exited { code, timed_out }) => break Some((code, timed_out)),
            None => break None,
        }
    };
    let rest: Vec<(ConsoleLineKind, String)> = [(out.kind, out.finish()), (err.kind, err.finish())]
        .into_iter()
        .filter_map(|(k, l)| l.map(|l| (k, l)))
        .collect();
    let note = match ended {
        _ if stopped => "Stopped.".to_string(),
        Some((_, true)) => "Stopped: it ran too long.".to_string(),
        Some((Some(0), _)) => String::new(),
        Some((Some(code), _)) => format!("Exited with code {code}."),
        Some((None, _)) | None => "Ended by a signal.".to_string(),
    };
    match sessions.lock() {
        Ok(mut map) => {
            if let Some(session) = map
                .get_mut(&thread)
                .filter(|s| s.run.as_ref().is_some_and(|r| r.generation == generation))
            {
                for (kind, line) in rest {
                    session.push(kind, line);
                }
                if !note.is_empty() {
                    session.push(ConsoleLineKind::Info, note);
                }
                session.exit_code = ended.and_then(|(code, _)| code).filter(|_| !stopped);
                session.run = None;
            }
        }
        Err(error) => tracing::debug!(%error, "console sessions poisoned"),
    }
}

impl Consoles {
    fn available(&self) -> bool {
        self.runner.get().is_some()
    }

    /// Stop every running command (backend shutdown). Dropping a command's
    /// process also kills its tree, should the pump never run again.
    pub(crate) fn stop_all(&self) {
        match self.sessions.lock() {
            Ok(mut map) => {
                for session in map.values_mut() {
                    if let Some(stop) = session.run.as_mut().and_then(|r| r.stop.take()) {
                        let _ = stop.send(()); // silent-ok: the command already ended
                    }
                }
            }
            Err(error) => tracing::debug!(%error, "console sessions poisoned"),
        }
    }
}

impl Engine {
    /// Turn on the operator console with `runner` (first call wins). Only
    /// where the agent itself gets a host shell.
    pub fn set_console_runner(&self, runner: Arc<dyn HostCommandRunner>) {
        let _ = self.consoles.runner.set(runner); // silent-ok: already set by an earlier wiring
    }

    /// `thread`'s console after `after` (the caller checked ownership).
    pub async fn thread_console(
        &self,
        thread: &ThreadRecord,
        after: Option<u64>,
    ) -> Result<ThreadConsoleView> {
        let available = self.consoles.available();
        let folder = if available {
            self.thread_cwd(thread).await?
        } else {
            None
        };
        let mut map = self.consoles.sessions.lock().map_err(poisoned)?;
        let view = match map.get_mut(&thread.thread_id) {
            Some(session) => {
                if session.run.is_none() {
                    session.folder = folder;
                }
                session.view(after, available)
            }
            None => Session {
                folder,
                ..Session::default()
            }
            .view(after, available),
        };
        Ok(view)
    }

    /// Start, stop or clear `thread`'s console; answers its view after.
    pub async fn thread_console_control(
        &self,
        thread: &ThreadRecord,
        action: ThreadConsoleAction,
    ) -> Result<ThreadConsoleView> {
        let id = &thread.thread_id;
        match action {
            ThreadConsoleAction::Start { command } => {
                let Some(runner) = self.consoles.runner.get().cloned() else {
                    return Err(OrchestratorError::request(
                        "console_unavailable",
                        "This computer does not run console commands for clients.",
                    ));
                };
                let command = command.trim().to_string();
                if command.is_empty() || command.len() > CONSOLE_COMMAND_MAX_BYTES {
                    return Err(OrchestratorError::invalid(format!(
                        "a command is 1 to {CONSOLE_COMMAND_MAX_BYTES} bytes"
                    )));
                }
                let Some(folder) = self.thread_cwd(thread).await? else {
                    return Err(OrchestratorError::request(
                        "no_repository",
                        "This thread has no folder on its computer.",
                    ));
                };
                let (stop, stop_rx) = oneshot::channel();
                let generation = {
                    let mut map = self.consoles.sessions.lock().map_err(poisoned)?;
                    if map.get(id).is_some_and(|s| s.run.is_some()) {
                        return Err(OrchestratorError::request(
                            "console_busy",
                            "A command is still running; stop it first.",
                        ));
                    }
                    if map.values().filter(|s| s.run.is_some()).count() >= RUNNING_MAX {
                        return Err(OrchestratorError::request(
                            "console_busy",
                            "Too many console commands are running on this computer.",
                        ));
                    }
                    let session = map.entry(id.clone()).or_default();
                    session.push(ConsoleLineKind::Command, command.clone());
                    session.command = Some(command.clone());
                    session.exit_code = None;
                    session.folder = Some(folder.clone());
                    session.generation += 1;
                    session.run = Some(Run {
                        generation: session.generation,
                        stop: Some(stop),
                    });
                    session.generation
                };
                let line = format!(
                    "TERM=dumb NO_COLOR=1 CLICOLOR=0\nexport TERM NO_COLOR CLICOLOR\n{command}"
                );
                let spawned = runner
                    .spawn(HostCommand {
                        line,
                        workdir: folder,
                        timeout: COMMAND_TIMEOUT,
                    })
                    .await;
                match spawned {
                    Ok(process) => {
                        let sessions = Arc::clone(&self.consoles.sessions);
                        tokio::spawn(pump(sessions, id.clone(), generation, process, stop_rx));
                    }
                    Err(error) => {
                        let mut map = self.consoles.sessions.lock().map_err(poisoned)?;
                        if let Some(session) = map.get_mut(id) {
                            session.run = None;
                            session.push(
                                ConsoleLineKind::Info,
                                format!("Couldn't start the command: {error}"),
                            );
                        }
                    }
                }
            }
            ThreadConsoleAction::Cancel {} => {
                let mut map = self.consoles.sessions.lock().map_err(poisoned)?;
                if let Some(stop) = map
                    .get_mut(id)
                    .and_then(|s| s.run.as_mut())
                    .and_then(|r| r.stop.take())
                {
                    let _ = stop.send(()); // silent-ok: the command ended on its own meanwhile
                }
            }
            ThreadConsoleAction::Clear {} => {
                let mut map = self.consoles.sessions.lock().map_err(poisoned)?;
                if let Some(session) = map.get_mut(id) {
                    session.lines.clear();
                }
            }
        }
        self.thread_console(thread, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_drops_oldest_lines_and_reports_them() {
        let mut s = Session::default();
        for n in 0..(CONSOLE_LINES_MAX + 10) {
            s.push(ConsoleLineKind::Out, format!("{n}"));
        }
        let view = s.view(Some(0), true);
        assert_eq!(view.dropped, 10);
        assert_eq!(view.lines.len(), CONSOLE_PAGE_LINES);
        assert_eq!(view.lines[0].text, "10");
        let tail = s.view(Some(s.last_seq), true);
        assert!(tail.lines.is_empty());
        assert_eq!(tail.next, s.last_seq);
        assert_eq!(tail.dropped, 0);
    }

    #[test]
    fn lines_are_split_and_long_ones_cut() {
        let mut split = LineSplitter::new(ConsoleLineKind::Out);
        assert_eq!(split.feed(b"a\r\nb"), vec!["a".to_string()]);
        assert_eq!(split.feed(b"c\n"), vec!["bc".to_string()]);
        let long = vec![b'x'; CONSOLE_LINE_MAX_BYTES + 5];
        assert_eq!(split.feed(&long).len(), 1);
        assert_eq!(split.finish().as_deref(), Some("xxxxx"));
        let mut s = Session::default();
        s.push(ConsoleLineKind::Out, "é".repeat(CONSOLE_LINE_MAX_BYTES));
        assert!(s.lines[0].text.len() <= CONSOLE_LINE_MAX_BYTES + 4);
    }
}
