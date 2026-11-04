//! Folding a Codex rollout (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`)
//! into the transcript entries this driver journals for its own threads:
//! each prompt as a user row, and each turn's reply as one assistant row
//! with the turn's tool calls as `name()` labels, like [`super::fold`] does
//! live. Instructions and environment blocks Codex adds as user messages are
//! kept, hidden. Developer/system messages, reasoning and tool output are
//! not conversation and are left in the rollout.
//!
//! Two layouts: the current one wraps each record (`{"timestamp", "type":
//! "session_meta"|"response_item"|"turn_context"|"event_msg"|…, "payload"}`);
//! older rollouts start with a bare `{id, timestamp, instructions}` line and
//! hold bare response items after it.

use std::path::{Path, PathBuf};

use ironclaw_orchestrator::model::Millis;
use ironclaw_orchestrator::runtime::{HistoryMessage, HistoryRole};
use serde_json::Value;

use crate::journal;
use crate::session_import::lines::Lines;

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct CodexScan {
    /// The Codex thread id (`session_meta.id`).
    pub(crate) id: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
    /// The client that started the session (`ironclaw` for this app's own).
    pub(crate) originator: Option<String>,
    /// Spawned by another session's agent.
    pub(crate) subagent: bool,
    pub(crate) started_at: Option<Millis>,
    pub(crate) first_prompt: Option<String>,
    pub(crate) messages: Vec<HistoryMessage>,
    pub(crate) last_at: Option<Millis>,
    pub(crate) last_user_at: Option<Millis>,
    pub(crate) end: u64,
    pub(crate) bad_lines: u32,
}

#[derive(Default)]
struct Reply {
    text: String,
    tools: Vec<String>,
    at: Option<Millis>,
}

fn timestamp(v: &Value) -> Option<Millis> {
    let raw = v.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok() // silent-ok: a record without a readable time just has none
        .map(|t| t.timestamp_millis())
}

/// Blocks Codex sends as user messages that nobody typed:
/// `<environment_context>…`, `<user_instructions>…`, AGENTS.md text.
fn injected(text: &str) -> bool {
    let t = text.trim();
    if t.starts_with("# AGENTS.md instructions") {
        return true;
    }
    let Some(rest) = t.strip_prefix('<') else {
        return false;
    };
    let tag: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    !tag.is_empty() && t.ends_with('>') && t.contains(&format!("</{tag}>"))
}

/// The request itself when the desktop app put attached-file notes ahead
/// of it.
fn request_text(text: &str) -> &str {
    const MARK: &str = "## My request for Codex:";
    match text.split_once(MARK) {
        Some((_, request)) => request.trim(),
        None => text.trim(),
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn tool_label(item: &Value) -> Option<String> {
    let kind = item.get("type").and_then(Value::as_str)?;
    let name = match kind {
        "function_call" | "custom_tool_call" => item.get("name").and_then(Value::as_str)?,
        "local_shell_call" => "shell",
        "web_search_call" => "web_search",
        "image_generation_call" => "image_generation",
        _ => return None,
    };
    Some(format!("{name}()"))
}

struct Fold {
    out: CodexScan,
    reply: Reply,
}

impl Fold {
    fn flush(&mut self) {
        let reply = std::mem::take(&mut self.reply);
        if reply.text.is_empty() && reply.tools.is_empty() {
            return;
        }
        let mut m = journal::message(HistoryRole::Assistant, reply.text, reply.tools);
        m.at = reply.at;
        self.out.messages.push(m);
    }

    fn meta(&mut self, meta: &Value, at: Option<Millis>) {
        let o = &mut self.out;
        if o.id.is_none() {
            o.id = meta.get("id").and_then(Value::as_str).map(str::to_string);
        }
        if o.cwd.is_none() {
            o.cwd = meta.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        }
        if o.originator.is_none() {
            o.originator = meta
                .get("originator")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        // `source` is a string for sessions a person started and an object
        // (`{"subagent": …}`) for spawned ones; `thread_source` is `user`
        // for the person's own.
        if meta
            .get("source")
            .is_some_and(|s| s.get("subagent").is_some())
            || meta
                .get("thread_source")
                .and_then(Value::as_str)
                .is_some_and(|s| s != "user")
        {
            o.subagent = true;
        }
        o.started_at = o.started_at.or_else(|| timestamp(meta)).or(at);
    }

    fn item(&mut self, item: &Value, at: Option<Millis>) {
        if let Some(label) = tool_label(item) {
            self.reply.tools.push(label);
            self.reply.at = at.or(self.reply.at);
            return;
        }
        if item.get("type").and_then(Value::as_str) != Some("message") {
            return;
        }
        let text = content_text(item.get("content").unwrap_or(&Value::Null));
        match item.get("role").and_then(Value::as_str) {
            Some("user") => {
                if text.trim().is_empty() {
                    return;
                }
                self.flush();
                let hidden = injected(&text);
                if !hidden {
                    self.out.last_user_at = at.or(self.out.last_user_at);
                    if self.out.first_prompt.is_none() {
                        self.out.first_prompt = Some(request_text(&text).to_string());
                    }
                }
                let mut m = journal::message(HistoryRole::User, text, Vec::new());
                m.hidden = hidden;
                m.at = at;
                self.out.messages.push(m);
            }
            Some("assistant") if !text.trim().is_empty() => {
                if !self.reply.text.is_empty() {
                    self.reply.text.push_str("\n\n");
                }
                self.reply.text.push_str(text.trim());
                self.reply.at = at.or(self.reply.at);
            }
            _ => {}
        }
    }

    fn line(&mut self, v: &Value) {
        let at = timestamp(v);
        if let Some(at) = at {
            self.out.last_at = Some(self.out.last_at.map_or(at, |l| l.max(at)));
        }
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => self.meta(&v["payload"], at),
            Some("turn_context") => {
                if let Some(cwd) = v["payload"].get("cwd").and_then(Value::as_str) {
                    self.out.cwd = Some(PathBuf::from(cwd));
                }
            }
            Some("response_item") => self.item(&v["payload"], at),
            // Older layout: bare items, and a bare meta line first.
            Some(_) if v.get("payload").is_none() => self.item(v, at),
            None if v.get("id").is_some() && v.get("record_type").is_none() => self.meta(v, at),
            _ => {}
        }
    }
}

/// Read `path` from `from` (a line boundary).
pub(crate) async fn scan(path: &Path, from: u64) -> std::io::Result<CodexScan> {
    let mut lines = Lines::open(path, from).await?;
    let mut fold = Fold {
        out: CodexScan::default(),
        reply: Reply::default(),
    };
    while let Some(line) = lines.next_line().await? {
        match serde_json::from_slice::<Value>(&line) {
            Ok(v) => fold.line(&v),
            Err(_) => fold.out.bad_lines += 1, // silent-ok: a malformed record is counted and skipped
        }
    }
    fold.flush();
    let mut out = fold.out;
    out.bad_lines += lines.skipped;
    out.end = lines.end();
    Ok(out)
}

/// The Codex thread id a rollout file name ends with
/// (`rollout-<time>-<uuid>.jsonl`).
pub(crate) fn id_from_file_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let start = stem.len().checked_sub(36)?;
    let id = stem.get(start..)?;
    uuid::Uuid::parse_str(id).ok().map(|_| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLLOUT: &[&str] = &[
        r#"{"timestamp":"2026-10-01T10:00:00.000Z","type":"session_meta","payload":{"id":"01a19dc0-9936-7d50-8808-84fea8a23fa6","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/work/app","originator":"codex_cli_rs","source":"cli","thread_source":"user","base_instructions":{"text":"You are Codex."}}}"#,
        r#"{"timestamp":"2026-10-01T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>sandboxed</permissions instructions>"}]}}"#,
        r#"{"timestamp":"2026-10-01T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/work/app</cwd>\n</environment_context>"}]}}"#,
        r#"{"timestamp":"2026-10-01T10:00:02.000Z","type":"turn_context","payload":{"cwd":"/work/app","model":"gpt-x"}}"#,
        r#"{"timestamp":"2026-10-01T10:00:02.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Why is CI red?"}]}}"#,
        r#"{"timestamp":"2026-10-01T10:00:02.500Z","type":"event_msg","payload":{"type":"user_message","message":"Why is CI red?"}}"#,
        r#"{"timestamp":"2026-10-01T10:00:03.000Z","type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
        r#"{"timestamp":"2026-10-01T10:00:04.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking the logs."}]}}"#,
        r#"{"timestamp":"2026-10-01T10:00:05.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#,
        r#"{"timestamp":"2026-10-01T10:00:06.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"FAIL"}}"#,
        r#"{broken"#,
        r#"{"timestamp":"2026-10-01T10:00:07.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch"}}"#,
        r#"{"timestamp":"2026-10-01T10:00:08.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"A snapshot test drifted; fixed."}]}}"#,
    ];

    #[tokio::test]
    async fn a_rollout_folds_into_prompts_and_one_reply_per_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-2026-10-01T10-00-00-01a19dc0-9936-7d50-8808-84fea8a23fa6.jsonl");
        tokio::fs::write(&path, format!("{}\n", ROLLOUT.join("\n")))
            .await
            .unwrap();
        assert_eq!(
            id_from_file_name(&path).as_deref(),
            Some("01a19dc0-9936-7d50-8808-84fea8a23fa6")
        );
        let scan = scan(&path, 0).await.unwrap();
        assert_eq!(
            scan.id.as_deref(),
            Some("01a19dc0-9936-7d50-8808-84fea8a23fa6")
        );
        assert_eq!(scan.cwd.as_deref(), Some(Path::new("/work/app")));
        assert_eq!(scan.originator.as_deref(), Some("codex_cli_rs"));
        assert!(!scan.subagent);
        assert_eq!(scan.bad_lines, 1);
        assert_eq!(scan.first_prompt.as_deref(), Some("Why is CI red?"));
        let rows: Vec<_> = scan
            .messages
            .iter()
            .map(|m| (m.role, m.hidden, m.text.as_str(), m.tool_calls.clone()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    HistoryRole::User,
                    true,
                    "<environment_context>\n  <cwd>/work/app</cwd>\n</environment_context>",
                    vec![]
                ),
                (HistoryRole::User, false, "Why is CI red?", vec![]),
                (
                    HistoryRole::Assistant,
                    false,
                    "Checking the logs.\n\nA snapshot test drifted; fixed.",
                    vec!["shell()".to_string(), "apply_patch()".to_string()]
                ),
            ]
        );
        assert_eq!(scan.messages[1].at, Some(1_790_848_802_000));
        assert_eq!(scan.last_user_at, Some(1_790_848_802_000));
        assert_eq!(scan.end as usize, ROLLOUT.join("\n").len() + 1);
    }

    #[tokio::test]
    async fn legacy_rollouts_and_spawned_sessions_are_recognized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-old.jsonl");
        let legacy = [
            r#"{"id":"0b5e","timestamp":"2025-05-01T08:00:00Z","instructions":null}"#,
            r#"{"record_type":"state"}"#,
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}"#,
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}"#,
        ];
        tokio::fs::write(&path, format!("{}\n", legacy.join("\n")))
            .await
            .unwrap();
        let scan = scan(&path, 0).await.unwrap();
        assert_eq!(scan.id.as_deref(), Some("0b5e"));
        assert_eq!(scan.messages.len(), 2);
        assert_eq!(scan.started_at, Some(1_746_086_400_000));

        let spawned = dir.path().join("rollout-sub.jsonl");
        let meta = r#"{"timestamp":"2026-10-01T10:00:00Z","type":"session_meta","payload":{"id":"s","source":{"subagent":{"thread_spawn":{}}}}}"#;
        tokio::fs::write(&spawned, format!("{meta}\n"))
            .await
            .unwrap();
        assert!(super::scan(&spawned, 0).await.unwrap().subagent);
    }
}
