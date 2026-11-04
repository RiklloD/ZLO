//! Claude Code transcripts: `<projects>/<encoded cwd>/<session>.jsonl`.
//! Subagent transcripts (`<session>/subagents/…`, `agent-*.jsonl`) are not
//! sessions of their own.

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
        let mut out = Vec::new();
        for dir in dir_entries(&self.paths.claude_projects).await {
            let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // This app's own folders (the encoding is lossy: `/` and `.`
            // both become `-`, so a sibling folder may be skipped too).
            if own
                .iter()
                .any(|o| name == o || name.starts_with(&format!("{o}-")))
                || !dir.is_dir()
            {
                continue;
            }
            for path in dir_entries(&dir).await {
                if let Some(c) = claude_candidate(path).await {
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
    ) -> Result<(), ImportError> {
        match self.known(pass, c).await? {
            Known::Done => Ok(()),
            Known::Grew(entry) => {
                let scan = claude_import::scan(&c.path, entry.watermark.offset).await?;
                let facts = SessionFacts {
                    last: scan.last_at,
                    last_user: scan.last_user_at,
                    end: scan.end,
                    messages: scan.messages,
                    ..SessionFacts::default()
                };
                self.advance(pass, c, &entry, &facts).await
            }
            Known::New => {
                if pass.own.contains(&c.native_id) || self.known_skip(c) {
                    pass.counts.skipped += 1;
                    return Ok(());
                }
                let scan = claude_import::scan(&c.path, 0).await?;
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
                let facts = SessionFacts {
                    title: scan.title.or(scan.first_prompt),
                    created: scan.first_at,
                    last: scan.last_at,
                    last_user: scan.last_user_at,
                    end: scan.end,
                    messages: scan.messages,
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

async fn claude_candidate(path: PathBuf) -> Option<Candidate> {
    if !jsonl(&path) {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() || stem.starts_with("agent-") || stem.len() > 200 {
        return None;
    }
    let native_id = stem.to_string();
    let (size, mtime_ms) = stat(&path).await?;
    Some(Candidate {
        path,
        native_id,
        size,
        mtime_ms,
    })
}
