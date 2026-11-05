//! Codex rollouts: `<sessions>/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`,
//! folded into the Codex driver's transcript for the session key (the Codex
//! thread id), with the thread id saved for `thread/resume` and the folder
//! the session ran in.
//!
//! The copy is written in bounded batches. Each transcript line the import
//! writes carries `"import": {"g": generation, "e": source offset}`: the
//! file generation it came from and the offset after the source line that
//! completed it. The ledger moves after each batch; a batch appended but not
//! recorded (a crash in between) is recognized on retry by those offsets and
//! not written twice, and a torn last line is closed off before appending.
//! A replaced or rewritten rollout is folded again from the start into a new
//! generation: what the transcript already holds is kept, and only messages
//! it does not hold yet (by content, time and role) are added.
//!
//! Session keys of imported sessions are the Codex thread id itself, so a
//! saved id file whose name is its own content is an import (possibly one
//! still pending), never a session this app started.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ironclaw_orchestrator::runtime::HistoryMessage;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::*;
use crate::codex::import::Folded;
use crate::codex::{cwd_file_in, thread_file_in, transcript_in};

/// Codex's `originator` for sessions this app started (its `clientInfo`).
const OWN_ORIGINATOR: &str = "ironclaw";

/// Which folded messages a batch still has to write.
enum Dedupe {
    /// Same file, same generation: those after the highest source offset
    /// the transcript already holds for it.
    After(Option<u64>),
    /// A re-read: those the transcript does not hold yet (a multiset, so a
    /// message said twice is kept twice).
    Content(HashMap<String, u32>),
}

impl Dedupe {
    fn keep(&mut self, f: &Folded) -> bool {
        match self {
            Self::After(done) => done.is_none_or(|d| f.end > d),
            Self::Content(held) => match held.get_mut(&message_key(&f.message)) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    false
                }
                _ => true,
            },
        }
    }
}

/// A message's identity across re-reads: role, time, visibility, text and
/// tool calls.
fn message_key(m: &HistoryMessage) -> String {
    let mut h = Sha256::new();
    h.update(serde_json::to_string(&m.role).unwrap_or_default()); // silent-ok: a unit enum always serializes
    h.update([0x1f]);
    h.update(m.at.map(|a| a.to_string()).unwrap_or_default());
    h.update([0x1f, u8::from(m.hidden), 0x1f]);
    h.update(m.text.as_bytes());
    for t in &m.tool_calls {
        h.update([0x1f]);
        h.update(t.as_bytes());
    }
    files::hex(&h.finalize()[..16])
}

/// Where a batch run starts and what it may still write.
struct Cursor {
    from: u64,
    generation: i64,
    dedupe: Dedupe,
}

/// What the transcript already holds from imports.
#[derive(Default)]
struct Held {
    /// Highest source offset per generation.
    ends: HashMap<i64, u64>,
    keys: HashMap<String, u32>,
}

async fn held(path: &Path) -> Result<Held, ImportError> {
    let mut out = Held::default();
    let mut lines = match lines::Lines::open(path, 0).await {
        Ok(lines) => lines,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue; // silent-ok: a torn line from an interrupted append holds nothing
        };
        let mark = &v["import"];
        let (Some(generation), Some(end)) = (mark["g"].as_i64(), mark["e"].as_u64()) else {
            continue;
        };
        let top = out.ends.entry(generation).or_default();
        *top = (*top).max(end);
        if let Ok(m) = serde_json::from_value::<HistoryMessage>(v) {
            *out.keys.entry(message_key(&m)).or_default() += 1;
        }
    }
    Ok(out)
}

/// Close off a torn last line, so the next append starts on a line of its own.
async fn close_torn_line(path: &Path) -> Result<(), ImportError> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata().await?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(std::io::SeekFrom::Start(len - 1)).await?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last).await?;
    if last[0] != b'\n' {
        let mut w = files::private_writer(path, true).await?;
        w.write_all(b"\n").await?;
        w.sync_all().await?;
    }
    Ok(())
}

/// Write `messages` to the transcript, each marked with its source offset.
async fn write_batch(
    path: &Path,
    messages: &[&Folded],
    generation: i64,
    append: bool,
) -> Result<(), ImportError> {
    let mut body = String::new();
    for f in messages {
        let mut v = serde_json::to_value(&f.message)?;
        v["import"] = json!({"g": generation, "e": f.end});
        body.push_str(&serde_json::to_string(&v)?);
        body.push('\n');
    }
    let mut file = files::private_writer(path, append).await?;
    file.write_all(body.as_bytes()).await?;
    file.sync_all().await?;
    Ok(())
}

impl SessionImporter {
    pub(super) async fn codex_candidates(&self) -> Vec<Candidate> {
        let cap = self.limits.files;
        let mut out = Vec::new();
        for year in files::real_dirs(&self.paths.codex_sessions, cap).await {
            for month in files::real_dirs(&year, cap).await {
                for day in files::real_dirs(&month, cap).await {
                    for (path, file) in files::regular_files(&day, cap).await {
                        if out.len() >= cap {
                            return out;
                        }
                        if let Some(c) = codex_candidate(path, file) {
                            out.push(c);
                        }
                    }
                }
            }
        }
        out
    }

    /// Codex thread ids of this app's own Codex sessions: saved ids whose
    /// session key is not the id itself (imports key by the id).
    pub(super) async fn codex_thread_ids(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        let dir = self.paths.codex_state.join("threads");
        for (path, _) in files::regular_files(&dir, self.limits.files).await {
            if path.extension().is_none_or(|e| e != "id") {
                continue;
            }
            let key = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if let Ok(id) = tokio::fs::read_to_string(&path).await {
                let id = id.trim();
                if !id.is_empty() && id != key {
                    out.insert(id.to_string());
                }
            }
        }
        out
    }

    pub(super) async fn codex(
        &self,
        pass: &mut Pass<'_>,
        c: &Candidate,
        budget: &mut Budget,
    ) -> Result<(), ImportError> {
        let journal = transcript_in(&self.paths.codex_state, &c.native_id);
        match self.known(pass, c).await? {
            Known::Done => Ok(()),
            Known::Grew(entry) => {
                let generation = entry.watermark.generation;
                let done = held(&journal).await?.ends.get(&generation).copied();
                let from = entry.watermark.offset;
                let cursor = Cursor {
                    from,
                    generation,
                    dedupe: Dedupe::After(done),
                };
                let read = self.codex_batches(pass, c, &entry, cursor, budget).await?;
                if read > 0 {
                    pass.counts.updated += 1;
                } else {
                    pass.counts.unchanged += 1;
                }
                Ok(())
            }
            Known::Rewritten(entry) => {
                let generation = entry.watermark.generation + 1;
                let keys = held(&journal).await?.keys;
                let cursor = Cursor {
                    from: 0,
                    generation,
                    dedupe: Dedupe::Content(keys),
                };
                self.codex_batches(pass, c, &entry, cursor, budget).await?;
                pass.counts.updated += 1;
                Ok(())
            }
            Known::New => self.codex_new(pass, c, &journal, budget).await,
        }
    }

    async fn codex_new(
        &self,
        pass: &mut Pass<'_>,
        c: &Candidate,
        journal: &Path,
        budget: &mut Budget,
    ) -> Result<(), ImportError> {
        if pass.own.contains(&c.native_id) || self.known_skip(c) {
            pass.counts.skipped += 1;
            return Ok(());
        }
        let scan = codex_import::scan(self.lines(c, 0).await?, 0, self.limits.batch).await?;
        budget.take(scan.end);
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
        let state = &self.paths.codex_state;
        let session = &c.native_id;
        // Nothing here is the app's until the ledger says so: the copy is
        // written whole (a retry replaces it), the id file names itself
        // (never mistaken for the app's own), the folder record is rewritten.
        let batch: Vec<&Folded> = scan.messages.iter().collect();
        write_batch(journal, &batch, 0, false).await?;
        self.step(Step::Transcript)?;
        let thread_file = thread_file_in(state, session);
        if !thread_file.is_file() {
            let id = scan.id.clone().unwrap_or_else(|| session.clone());
            files::write_private(&thread_file, id.as_bytes()).await?;
        }
        self.step(Step::ThreadFile)?;
        if let Some(cwd) = &scan.cwd {
            crate::workspace::record(&cwd_file_in(state, session), cwd).await?;
        }
        self.step(Step::Workspace)?;
        let facts = SessionFacts {
            title: scan.first_prompt.clone(),
            created: scan.started_at,
            last: scan.last_at,
            last_user: scan.last_user_at,
            end: scan.end,
            fingerprint: self.fingerprint(c, scan.end).await?,
            messages: scan.messages.len() as u64,
            partial: !scan.eof,
            ..SessionFacts::default()
        };
        let imported = self.record(pass, c, facts)?;
        pass.store
            .record_imported_thread(&pass.owner, &imported)
            .await?;
        pass.counts.imported += 1;
        if !scan.eof {
            let entry = ImportLedgerEntry {
                thread_id: imported.record.thread_id.clone(),
                watermark: imported.watermark.clone(),
                messages: imported.messages,
                thread_live: true,
                adopted: false,
            };
            let cursor = Cursor {
                from: scan.end,
                generation: 0,
                dedupe: Dedupe::After(scan.messages.last().map(|f| f.end)),
            };
            self.codex_batches(pass, c, &entry, cursor, budget).await?;
        }
        Ok(())
    }

    /// Fold from `from` in batches, write what `dedupe` keeps, and move the
    /// ledger after each batch. Returns how many messages the source added
    /// (written now, or by an interrupted earlier attempt).
    async fn codex_batches(
        &self,
        pass: &mut Pass<'_>,
        c: &Candidate,
        entry: &ImportLedgerEntry,
        cursor: Cursor,
        budget: &mut Budget,
    ) -> Result<u64, ImportError> {
        let Cursor {
            mut from,
            generation,
            mut dedupe,
        } = cursor;
        let journal = transcript_in(&self.paths.codex_state, &c.native_id);
        close_torn_line(&journal).await?;
        let mut read = 0u64;
        loop {
            let scan =
                codex_import::scan(self.lines(c, from).await?, from, self.limits.batch).await?;
            budget.take(scan.end - from);
            let keep: Vec<&Folded> = scan.messages.iter().filter(|f| dedupe.keep(f)).collect();
            if !keep.is_empty() {
                write_batch(&journal, &keep, generation, true).await?;
            }
            self.step(Step::Appended)?;
            let facts = SessionFacts {
                last: scan.last_at,
                last_user: scan.last_user_at,
                end: scan.end,
                fingerprint: self.fingerprint(c, scan.end).await?,
                generation,
                messages: keep.len() as u64,
                partial: !scan.eof,
                ..SessionFacts::default()
            };
            self.advance(pass, c, entry, &facts).await?;
            read += scan.messages.len() as u64;
            from = scan.end;
            if scan.eof {
                break;
            }
            if budget.spent() {
                pass.counts.deferred += 1;
                break;
            }
        }
        Ok(read)
    }
}

fn codex_candidate(path: PathBuf, file: files::FileInfo) -> Option<Candidate> {
    if !jsonl(&path) {
        return None;
    }
    Some(Candidate {
        native_id: codex_import::id_from_file_name(&path)?,
        path,
        file,
    })
}
