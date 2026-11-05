//! What a session import needs from a Claude Code transcript the person ran
//! in a terminal: id, working directory, title, times and message count.
//! The messages themselves stay in the transcript, which is the thread's
//! history for every Claude CLI thread (`transcript::read`), so an imported
//! thread shows, searches and resumes from the very file the terminal wrote.

use std::path::PathBuf;

use ironclaw_orchestrator::model::Millis;
use ironclaw_orchestrator::runtime::HistoryRole;
use serde_json::Value;

use super::transcript;
use crate::session_import::lines::Lines;

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ClaudeScan {
    pub(crate) session_id: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
    /// The CLI's own title for the session (latest), if it wrote one.
    pub(crate) title: Option<String>,
    pub(crate) first_prompt: Option<String>,
    pub(crate) first_at: Option<Millis>,
    pub(crate) last_at: Option<Millis>,
    pub(crate) last_user_at: Option<Millis>,
    /// Conversation rows (prompts, replies, tool results).
    pub(crate) messages: u64,
    /// A subagent's own transcript.
    pub(crate) sidechain: bool,
    /// Offset after the last complete line read.
    pub(crate) end: u64,
    /// Lines that were not JSON or were too long to read.
    pub(crate) bad_lines: u32,
}

/// Text that is CLI scaffolding rather than something the person typed.
fn scaffolding(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty() || t.starts_with('<') || t.starts_with("Caveat:")
}

/// Read `lines`, which start at `from` (a line boundary).
pub(crate) async fn scan(mut lines: Lines, from: u64) -> std::io::Result<ClaudeScan> {
    let mut out = ClaudeScan {
        end: from,
        ..ClaudeScan::default()
    };
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            out.bad_lines += 1;
            continue;
        };
        if out.session_id.is_none() {
            out.session_id = v["sessionId"].as_str().map(str::to_string);
        }
        if out.cwd.is_none() {
            out.cwd = v["cwd"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(PathBuf::from);
        }
        let title = match v["type"].as_str() {
            Some("ai-title") => v["aiTitle"].as_str(),
            Some("custom-title") => v["customTitle"].as_str(),
            Some("summary") => v["summary"].as_str(),
            _ => None,
        };
        if let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) {
            out.title = Some(title.to_string());
            continue;
        }
        let Some(mut m) = transcript::message(&v) else {
            continue;
        };
        m.hidden = transcript::compact_scaffolding(&v, &m);
        if v["isSidechain"].as_bool() == Some(true) {
            out.sidechain = true;
        }
        let at = transcript::timestamp(&v);
        if m.role != HistoryRole::System {
            out.messages += 1;
        }
        if let Some(at) = at {
            out.first_at = Some(out.first_at.map_or(at, |f| f.min(at)));
            out.last_at = Some(out.last_at.map_or(at, |l| l.max(at)));
        }
        if m.role == HistoryRole::User && !m.hidden && !scaffolding(&m.text) {
            out.last_user_at = at.or(out.last_user_at);
            if out.first_prompt.is_none() {
                out.first_prompt = Some(m.text.trim().to_string());
            }
        }
    }
    out.bad_lines += lines.skipped;
    out.end = lines.end();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal session as Claude Code writes it, bookkeeping rows and a
    /// torn line included.
    pub(crate) const FIXTURE: &[&str] = &[
        r#"{"type":"queue-operation","operation":"enqueue","sessionId":"7f0c","timestamp":"2026-10-01T09:59:59.000Z"}"#,
        r#"{"type":"user","uuid":"u1","sessionId":"7f0c","cwd":"/work/app","isSidechain":false,"timestamp":"2026-10-01T10:00:00.000Z","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
        r#"{"type":"user","uuid":"u2","sessionId":"7f0c","cwd":"/work/app","timestamp":"2026-10-01T10:00:05.000Z","message":{"role":"user","content":"Fix the flaky login test"}}"#,
        r#"not json at all"#,
        r#"{"type":"assistant","uuid":"a1","sessionId":"7f0c","timestamp":"2026-10-01T10:00:09.000Z","message":{"model":"claude-x","content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Bash"}]}}"#,
        r#"{"type":"user","uuid":"t1","sessionId":"7f0c","timestamp":"2026-10-01T10:00:10.000Z","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
        r#"{"type":"ai-title","aiTitle":"Flaky login test","sessionId":"7f0c"}"#,
        r#"{"type":"assistant","uuid":"a2","sessionId":"7f0c","timestamp":"2026-10-01T10:01:00.000Z","message":{"content":[{"type":"text","text":"Fixed."}]}}"#,
    ];

    #[tokio::test]
    async fn a_terminal_transcript_yields_its_session_facts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("7f0c.jsonl");
        // The last line is still being written.
        let body = format!("{}\n{{\"type\":\"user\"", FIXTURE.join("\n"));
        tokio::fs::write(&path, &body).await.unwrap();
        let scan = scan(Lines::open(&path, 0).await.unwrap(), 0).await.unwrap();
        assert_eq!(scan.session_id.as_deref(), Some("7f0c"));
        assert_eq!(scan.cwd.as_deref(), Some(std::path::Path::new("/work/app")));
        assert_eq!(scan.title.as_deref(), Some("Flaky login test"));
        assert_eq!(
            scan.first_prompt.as_deref(),
            Some("Fix the flaky login test")
        );
        assert_eq!(scan.messages, 5);
        assert_eq!(scan.bad_lines, 1);
        assert!(!scan.sidechain);
        assert_eq!(scan.first_at, Some(1_790_848_800_000));
        assert_eq!(scan.last_at, Some(1_790_848_860_000));
        assert_eq!(scan.last_user_at, Some(1_790_848_805_000));
        assert_eq!(scan.end as usize, FIXTURE.join("\n").len() + 1);

        // Growth: only the rows after the watermark are read.
        let more = r#"{"type":"user","uuid":"u3","timestamp":"2026-10-01T11:00:00.000Z","message":{"role":"user","content":"and the signup one"}}"#;
        tokio::fs::write(&path, format!("{}\n{more}\n", FIXTURE.join("\n")))
            .await
            .unwrap();
        let lines = Lines::open(&path, scan.end).await.unwrap();
        let grown = super::scan(lines, scan.end).await.unwrap();
        assert_eq!(grown.messages, 1);
        assert_eq!(grown.last_user_at, Some(1_790_852_400_000));
    }
}
