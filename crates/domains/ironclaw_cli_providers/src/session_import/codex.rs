//! Codex rollouts: `<sessions>/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`,
//! folded into the Codex driver's transcript for the session key (the Codex
//! thread id), with the thread id saved for `thread/resume` and the folder
//! the session ran in.

use std::path::PathBuf;

use super::*;
use crate::codex::{cwd_file_in, thread_file_in, transcript_in};

/// Codex's `originator` for sessions this app started (its `clientInfo`).
const OWN_ORIGINATOR: &str = "ironclaw";

impl SessionImporter {
    pub(super) async fn codex_candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for year in dir_entries(&self.paths.codex_sessions).await {
            for month in dir_entries(&year).await {
                for day in dir_entries(&month).await {
                    for path in dir_entries(&day).await {
                        if let Some(c) = codex_candidate(path).await {
                            out.push(c);
                        }
                    }
                }
            }
        }
        out
    }

    /// Codex thread ids of this app's own Codex sessions.
    pub(super) async fn codex_thread_ids(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for path in dir_entries(&self.paths.codex_state.join("threads")).await {
            if path.extension().is_some_and(|e| e == "id")
                && let Ok(id) = tokio::fs::read_to_string(&path).await
            {
                out.insert(id.trim().to_string());
            }
        }
        out
    }

    pub(super) async fn codex(
        &self,
        pass: &mut Pass<'_>,
        c: &Candidate,
    ) -> Result<(), ImportError> {
        let state = &self.paths.codex_state;
        match self.known(pass, c).await? {
            Known::Done => Ok(()),
            Known::Grew(entry) => {
                let scan = codex_import::scan(&c.path, entry.watermark.offset).await?;
                // Append before the mark moves: a failure in between reads
                // the growth again rather than losing it.
                if !scan.messages.is_empty() {
                    write_journal(&transcript_in(state, &c.native_id), &scan.messages, true)
                        .await?;
                }
                let facts = SessionFacts {
                    last: scan.last_at,
                    last_user: scan.last_user_at,
                    end: scan.end,
                    messages: scan.messages.len() as u64,
                    ..SessionFacts::default()
                };
                self.advance(pass, c, &entry, &facts).await
            }
            Known::New => {
                if pass.own.contains(&c.native_id) || self.known_skip(c) {
                    pass.counts.skipped += 1;
                    return Ok(());
                }
                let scan = codex_import::scan(&c.path, 0).await?;
                let own = scan
                    .originator
                    .as_deref()
                    .is_some_and(|o| o.eq_ignore_ascii_case(OWN_ORIGINATOR));
                if own
                    || scan.subagent
                    || scan.first_prompt.is_none()
                    || self.own_folder(scan.cwd.as_deref())
                    || scan.id.as_ref().is_some_and(|id| pass.own.contains(id))
                {
                    self.remember_skip(c);
                    pass.counts.skipped += 1;
                    return Ok(());
                }
                let session = &c.native_id;
                // A fresh session key: the transcript is written whole, so
                // a retry after a failure below never doubles it.
                write_journal(&transcript_in(state, session), &scan.messages, false).await?;
                let thread_file = thread_file_in(state, session);
                if let Some(dir) = thread_file.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                if !thread_file.is_file() {
                    let id = scan.id.clone().unwrap_or_else(|| session.clone());
                    tokio::fs::write(&thread_file, id).await?;
                }
                if let Some(cwd) = &scan.cwd {
                    tokio::fs::write(cwd_file_in(state, session), cwd.display().to_string())
                        .await?;
                }
                let facts = SessionFacts {
                    title: scan.first_prompt.clone(),
                    created: scan.started_at,
                    last: scan.last_at,
                    last_user: scan.last_user_at,
                    end: scan.end,
                    messages: scan.messages.len() as u64,
                };
                let imported = self.record(pass, c, facts)?;
                pass.store
                    .record_imported_thread(&pass.owner, &imported)
                    .await?;
                pass.counts.imported += 1;
                Ok(())
            }
        }
    }
}

async fn codex_candidate(path: PathBuf) -> Option<Candidate> {
    if !jsonl(&path) {
        return None;
    }
    let native_id = codex_import::id_from_file_name(&path)?;
    let (size, mtime_ms) = stat(&path).await?;
    Some(Candidate {
        path,
        native_id,
        size,
        mtime_ms,
    })
}
