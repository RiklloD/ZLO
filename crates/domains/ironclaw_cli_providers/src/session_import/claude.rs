//! Claude Code transcripts: `<projects>/<encoded cwd>/<session>.jsonl`.
//! Subagent transcripts (`<session>/subagents/…`, `agent-*.jsonl`) are not
//! sessions of their own. The transcript itself is the thread's history, so
//! growth and rewrites only update what the ledger knows about it.

use std::path::PathBuf;

use super::*;

impl SessionImporter {
    pub(super) async fn claude_candidates(&self) -> Vec<Candidate> {
        let own: Vec<String> = self
            .paths
            .own_roots
            .iter()
            .map(|r| claude_codes::transcript::encode_project_dir(r))
            .collect();
        let cap = self.limits.files;
        let mut out = Vec::new();
        for dir in files::real_dirs(&self.paths.claude_projects, cap).await {
            let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // This app's own folders (the encoding is lossy: `/` and `.`
            // both become `-`, so a sibling folder may be skipped too).
            if own
                .iter()
                .any(|o| name == o || name.starts_with(&format!("{o}-")))
            {
                continue;
            }
            for (path, file) in files::regular_files(&dir, cap).await {
                if out.len() >= cap {
                    return out;
                }
                if let Some(c) = claude_candidate(path, file) {
                    out.push(c);
                }
            }
        }
        out
    }

    pub(super) async fn claude(
        &self,
        pass: &mut Pass<'_>,
        c: &Candidate,
        budget: &mut Budget,
    ) -> Result<(), ImportError> {
        match self.known(pass, c, budget).await? {
            Known::Done => Ok(()),
            Known::Grew(entry) => {
                let from = entry.watermark.offset;
                let scan = claude_import::scan(self.lines(c, from).await?, from).await?;
                budget.take(scan.end - from);
                let facts = SessionFacts {
                    last: scan.last_at,
                    last_user: scan.last_user_at,
                    end: scan.end,
                    fingerprint: self.fingerprint(c, scan.end).await?,
                    generation: entry.watermark.generation,
                    messages: scan.messages,
                    ..SessionFacts::default()
                };
                self.advance(pass, c, &entry, &facts).await?;
                if scan.messages > 0 {
                    pass.counts.updated += 1;
                } else {
                    pass.counts.unchanged += 1;
                }
                Ok(())
            }
            Known::Rewritten(entry) => {
                // The new file is the history now: count it from the start.
                let scan = claude_import::scan(self.lines(c, 0).await?, 0).await?;
                budget.take(scan.end);
                let facts = SessionFacts {
                    last: scan.last_at,
                    last_user: scan.last_user_at,
                    end: scan.end,
                    fingerprint: self.fingerprint(c, scan.end).await?,
                    generation: entry.watermark.generation + 1,
                    messages: scan.messages,
                    recount: true,
                    ..SessionFacts::default()
                };
                self.advance(pass, c, &entry, &facts).await?;
                pass.counts.updated += 1;
                Ok(())
            }
            Known::New => {
                if pass.own.contains(&c.native_id) || self.known_skip(c) {
                    pass.counts.skipped += 1;
                    return Ok(());
                }
                let scan = claude_import::scan(self.lines(c, 0).await?, 0).await?;
                budget.take(scan.end);
                if scan.sidechain
                    || scan.first_prompt.is_none()
                    || self.own_folder(scan.cwd.as_deref())
                    || scan
                        .session_id
                        .as_deref()
                        .is_some_and(|id| id != c.native_id)
                {
                    self.remember_skip(c);
                    pass.counts.skipped += 1;
                    return Ok(());
                }
                if let Some(cwd) = &scan.cwd {
                    let file = self.paths.claude_imports.join(format!("{}.json", c.native_id));
                    crate::workspace::record(&file, cwd).await?;
                    self.step(Step::Workspace)?;
                }
                let facts = SessionFacts {
                    title: scan.title.or(scan.first_prompt),
                    created: scan.first_at,
                    last: scan.last_at,
                    last_user: scan.last_user_at,
                    end: scan.end,
                    fingerprint: self.fingerprint(c, scan.end).await?,
                    messages: scan.messages,
                    ..SessionFacts::default()
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

fn claude_candidate(path: PathBuf, file: files::FileInfo) -> Option<Candidate> {
    if !jsonl(&path) {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() || stem.starts_with("agent-") || stem.len() > 200 {
        return None;
    }
    Some(Candidate {
        native_id: stem.to_string(),
        path,
        file,
    })
}
