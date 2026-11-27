//! Failures agy hides behind a clean stop (t3code's
//! `AntigravityTurnOutcome`). agy reports an upstream API failure (a 502, a
//! mid-turn 404, a quota) as a chat message and `end_turn`, or goes silent
//! after logging it; its own session log
//! (`<profile>/antigravity-acp/conversations/<session>.db`) ends that
//! prompt with an error step. The driver marks the log before the turn's
//! first prompt goes out and reads each settled prompt's own steps when it
//! settles, before a later steer's steps can bury them; read-only, from the
//! session's own file reached through plain folders: never a link, never a
//! file that replaced the one seen before.
//!
//! The log is SQLite, read through the system `sqlite3` ([`super::sqlite`]);
//! without the tool, or with an unreadable log, the typed stop reason alone
//! decides, as before.

use std::path::{Path, PathBuf};

use ironclaw_orchestrator::runtime::{TurnEndStatus, TurnOutcome};

use super::fold::{INVALID_THINKING_SIGNATURE, reset_at, reset_stamp, usage_limited};
use super::launch::APP_DATA_DIR;
use super::paths::{self, Kind, Managed};
use super::sqlite;

/// `CORTEX_STEP_TYPE_ERROR_MESSAGE` in agy's trajectory.
const ERROR_STEP: i64 = 17;
/// `CORTEX_STEP_TYPE_PLANNER_RESPONSE`: the model answered within the turn.
const PLANNER_RESPONSE_STEP: i64 = 15;
/// `CORTEX_STEP_STATUS_DONE`.
const DONE_STEP_STATUS: i64 = 3;
/// The harness error text leads the payload; a Go stack trace follows it.
const MAX_ERROR_PAYLOAD_BYTES: usize = 16_384;
/// The prompt a transient mid-turn failure is resumed with, once.
pub(super) const RESUME_PROMPT: &str = "continue";

/// The newest step the turn added, and whether the model answered in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LastStep {
    pub(super) step_type: i64,
    /// The payload's leading bytes, lossily decoded (protobuf framing
    /// bytes around the harness text).
    pub(super) text: String,
    pub(super) reached_model: bool,
    /// The step's `status` (`None` on a log without the column).
    pub(super) status: Option<i64>,
}

/// The prompt's last step is the model's reply, finished: a prompt that
/// then goes silent is done, not hung (t3code 2a7b91ba93).
pub(super) fn finished_reply(step: Option<&LastStep>) -> bool {
    step.is_some_and(|s| s.step_type == PLANNER_RESPONSE_STEP && s.status == Some(DONE_STEP_STATUS))
}

/// A failure a clean stop hid.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HiddenFailure {
    pub(super) outcome: TurnOutcome,
    /// The agent's saved session can no longer be sent to the model.
    pub(super) broken: bool,
    /// The session can carry on: resend this prompt once before failing.
    pub(super) resume_with: Option<&'static str>,
}

/// The failure behind a prompt's stop: an error step the prompt added, or a
/// typed `refusal` (agy's quota stop) without one. `now` anchors a relative
/// reset delay (ms); `None` for a failure read long after it happened (a
/// silent prompt), which keeps only an absolute reset time.
pub(super) fn hidden_failure(
    stop_reason: Option<&str>,
    step: Option<&LastStep>,
    now: Option<i64>,
) -> Option<HiddenFailure> {
    let reset = |text: &str| match now {
        Some(now) => reset_at(text, now),
        None => reset_stamp(text),
    };
    let refusal = stop_reason == Some("refusal");
    let error_text = step
        .filter(|s| s.step_type == ERROR_STEP)
        .map(|s| s.text.as_str());
    if error_text.is_none() && !refusal {
        return None;
    }
    let text = error_text.unwrap_or("");
    let failed = |outcome: TurnOutcome| HiddenFailure {
        outcome,
        broken: false,
        resume_with: None,
    };
    let api = api_error(text).or_else(|| status_error(text));
    if refusal || text.contains("reason:QUOTA_EXHAUSTED") {
        return Some(failed(usage_limited(reset(text))));
    }
    if INVALID_THINKING_SIGNATURE
        .iter()
        .any(|needle| text.contains(needle))
    {
        return Some(HiddenFailure {
            outcome: TurnOutcome::ended(
                TurnEndStatus::Failed,
                Some(
                    "antigravity: the saved session can no longer be sent to the model; \
                     the next message starts a fresh session with this thread's context"
                        .into(),
                ),
            ),
            broken: true,
            resume_with: None,
        });
    }
    let Some(api) = api else {
        return Some(failed(TurnOutcome::ended(
            TurnEndStatus::Failed,
            Some("antigravity stopped this turn because of an error".into()),
        )));
    };
    if api.http == 429 || api.status == "RESOURCE_EXHAUSTED" {
        return Some(failed(usage_limited(reset(text))));
    }
    // A 404 before the model answered is an unknown or unentitled model;
    // after, a transient edge error the session can carry on from.
    let transient_not_found = api.http == 404 && step.is_some_and(|s| s.reached_model);
    // The provider's own words, redacted and bounded to one line.
    let message = crate::approval_reason::bounded(api.message)
        .unwrap_or_else(|| "the provider returned an error".into());
    let retry = if transient_not_found || api.http >= 500 {
        "; send a message to retry"
    } else {
        ""
    };
    Some(HiddenFailure {
        outcome: TurnOutcome::ended(
            TurnEndStatus::Failed,
            Some(format!(
                "antigravity: {message} ({} {}){retry}",
                api.http, api.status
            )),
        ),
        broken: false,
        resume_with: transient_not_found.then_some(RESUME_PROMPT),
    })
}

/// An upstream API error as agy's harness renders it.
#[derive(Debug, PartialEq, Eq)]
struct ApiError<'a> {
    http: u16,
    message: &'a str,
    /// A gRPC code (`NOT_FOUND`) or an HTTP status line (`502 Bad Gateway`).
    status: &'a str,
}

fn three_digits(text: &str) -> Option<u16> {
    let digits = text.get(..3)?;
    digits
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| digits.parse().ok())
        .flatten()
}

/// `Error NNN, Message: <message>, Status: <STATUS or NNN line>`. A message
/// stops at the next error, so each error is read on its own span.
fn api_error(text: &str) -> Option<ApiError<'_>> {
    const ERROR: &str = "Error ";
    const MESSAGE: &str = ", Message: ";
    const STATUS: &str = ", Status: ";
    let starts: Vec<usize> = text
        .match_indices(ERROR)
        .map(|(at, _)| at)
        .filter(|at| {
            let rest = &text[at + ERROR.len()..];
            three_digits(rest).is_some() && rest[3..].starts_with(MESSAGE)
        })
        .collect();
    for (n, &start) in starts.iter().enumerate() {
        let after = start + ERROR.len();
        let http = three_digits(&text[after..])?;
        let message_at = after + 3 + MESSAGE.len();
        let span_end = starts.get(n + 1).copied().unwrap_or(text.len());
        let Some(rel) = text[message_at..span_end].find(STATUS) else {
            continue;
        };
        let message = &text[message_at..message_at + rel];
        let status_at = message_at + rel + STATUS.len();
        let rest = &text[status_at..];
        let upper = rest
            .find(|c: char| !(c.is_ascii_uppercase() || c == '_'))
            .unwrap_or(rest.len());
        let status = if upper > 0 {
            rest.get(..upper)?
        } else if three_digits(rest).is_some() {
            rest.get(..rest.find([',', '\n']).unwrap_or(rest.len()))?
        } else {
            continue;
        };
        if message.trim().is_empty() {
            continue;
        }
        return Some(ApiError {
            http,
            message: message.trim(),
            status: status.trim(),
        });
    }
    None
}

/// agy 1.5 with a personal account: `STATUS (code NNN): message`.
fn status_error(text: &str) -> Option<ApiError<'_>> {
    const CODE: &str = " (code ";
    for (at, _) in text.match_indices(CODE) {
        let name_start = text
            .get(..at)?
            .char_indices()
            .rfind(|(_, c)| !(c.is_ascii_uppercase() || *c == '_'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let status = &text[name_start..at];
        let rest = &text[at + CODE.len()..];
        let (Some(http), false) = (three_digits(rest), status.is_empty()) else {
            continue;
        };
        let Some(message) = rest[3..].strip_prefix("): ") else {
            continue;
        };
        let message = message.get(..message.find(char::is_control).unwrap_or(message.len()))?;
        if message.trim().is_empty() {
            continue;
        }
        return Some(ApiError {
            http,
            message: message.trim(),
            status,
        });
    }
    None
}

/// The session's own log, read one settled prompt at a time.
#[derive(Debug)]
pub(super) struct SessionLog {
    profile: PathBuf,
    /// `<session>.db` under `antigravity-acp/conversations`.
    file: String,
    /// `(dev, ino)` of the file first seen; `None` until the session
    /// creates it.
    identity: Option<(u64, u64)>,
    /// The last step index already attributed to a prompt.
    cursor: i64,
    /// Whether `steps` has a `status` column; probed on the first read.
    has_status: Option<bool>,
}

/// What a prompt added to the log since the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    last: Option<LastStep>,
    max_idx: i64,
}

/// `(dev, ino)` of a plain file.
async fn identity_of(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = tokio::fs::symlink_metadata(path).await.ok()?; // silent-ok: checked just before; gone means no log
    meta.file_type().is_file().then(|| (meta.dev(), meta.ino()))
}

impl SessionLog {
    /// Mark where the turn starts, before its first prompt goes out;
    /// `None` when the log cannot be trusted (a link anywhere on its path,
    /// an unreadable file, no `sqlite3`), and the typed stop reason alone
    /// decides.
    pub(super) async fn open(profile: &Path, session_id: &str) -> Option<Self> {
        if session_id.is_empty()
            || !session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return None;
        }
        sqlite::tool()?;
        let mut log = Self {
            profile: profile.to_path_buf(),
            file: format!("{session_id}.db"),
            identity: None,
            cursor: -1,
            has_status: None,
        };
        match log.checked_path().await? {
            Managed::Missing(_) => {}
            Managed::Found(path) => {
                log.identity = Some(identity_of(&path).await?);
                log.cursor =
                    match sqlite::query(&path, "SELECT coalesce(max(idx), -1) FROM steps;").await {
                        Ok(out) => out.trim().parse().ok()?,
                        Err(error) => {
                            tracing::debug!(%error, "antigravity: session log not marked");
                            return None;
                        }
                    };
            }
        }
        Some(log)
    }

    async fn checked_path(&self) -> Option<Managed> {
        paths::managed(
            &self.profile,
            &[APP_DATA_DIR, "conversations", &self.file],
            Kind::File,
        )
        .await
    }

    /// The newest step added since the last [`Self::take`], which moves the
    /// cursor past it: each settled prompt is judged on its own steps.
    pub(super) async fn take(&mut self) -> Option<LastStep> {
        let span = self.span().await?;
        self.cursor = self.cursor.max(span.max_idx);
        span.last
    }

    /// The newest step added since the last [`Self::take`], cursor kept
    /// (a prompt that has not settled yet).
    pub(super) async fn peek(&mut self) -> Option<LastStep> {
        self.span().await?.last
    }

    async fn span(&mut self) -> Option<Span> {
        let path = self.checked_path().await?.found()?;
        let now = identity_of(&path).await?;
        match self.identity {
            Some(before) if before != now => {
                tracing::debug!("antigravity: session log was replaced during the turn");
                return None;
            }
            Some(_) => {}
            // The file this turn's session created.
            None => self.identity = Some(now),
        }
        let has_status = match self.has_status {
            Some(known) => known,
            None => {
                // A table not created yet reads like one without the
                // column: only a table that exists settles the answer.
                let probe = "SELECT (SELECT count(*) FROM sqlite_master \
                             WHERE type = 'table' AND name = 'steps') || '|' || \
                             (SELECT count(*) FROM pragma_table_info('steps') \
                             WHERE name = 'status');";
                match sqlite::query(&path, probe).await {
                    Ok(out) => {
                        let (table, column) = out.trim().split_once('|').unwrap_or(("0", "0"));
                        let known = column == "1";
                        if table == "1" {
                            self.has_status = Some(known);
                        }
                        known
                    }
                    Err(error) => {
                        tracing::debug!(%error, "antigravity: session log schema not read");
                        return None;
                    }
                }
            }
        };
        let status = if has_status { "status" } else { "NULL" };
        let start = self.cursor;
        let sql = format!(
            "SELECT 'L', step_type, hex(substr(step_payload, 1, {MAX_ERROR_PAYLOAD_BYTES})), {status} \
             FROM steps WHERE idx > {start} ORDER BY idx DESC LIMIT 1; \
             SELECT 'R', EXISTS(SELECT 1 FROM steps WHERE idx > {start} \
             AND step_type = {PLANNER_RESPONSE_STEP}); \
             SELECT 'M', coalesce(max(idx), -1) FROM steps;"
        );
        match sqlite::query(&path, &sql).await {
            Ok(out) => parse_span(&out),
            Err(error) => {
                // Lock contention, a changed schema and corruption read
                // apart here.
                tracing::debug!(%error, "antigravity: session log query failed");
                None
            }
        }
    }
}

/// The tagged rows of [`SessionLog::span`]'s query.
fn parse_span(output: &str) -> Option<Span> {
    let (mut last, mut reached_model, mut max_idx) = (None, false, None);
    for line in output.lines() {
        let mut cells = line.split('|');
        match cells.next() {
            Some("L") => {
                let step_type = cells.next()?.trim().parse().ok()?;
                let hex = cells.next()?.trim();
                let status = cells.next().and_then(|c| c.trim().parse().ok());
                last = Some((
                    step_type,
                    String::from_utf8_lossy(&unhex(hex)?).into_owned(),
                    status,
                ));
            }
            Some("R") => reached_model = cells.next().is_some_and(|c| c.trim() == "1"),
            Some("M") => max_idx = cells.next()?.trim().parse().ok(),
            _ => {}
        }
    }
    Some(Span {
        last: last.map(|(step_type, text, status)| LastStep {
            step_type,
            text,
            reached_model,
            status,
        }),
        max_idx: max_idx?,
    })
}

fn unhex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(step_type: i64, text: &str, reached_model: bool) -> LastStep {
        LastStep {
            step_type,
            text: text.into(),
            reached_model,
            status: None,
        }
    }

    const NOW: i64 = 1_000_000;

    /// t3code #33596: a 502 behind `end_turn` fails the turn with the
    /// provider's words, and says it can be retried.
    #[test]
    fn a_502_fails_with_its_real_error() {
        let payload = "\u{12}7Error 502, Message: Bad Gateway: Failed to connect to backend API, \
                       Status: 502 Bad Gateway, Details: []\ngoroutine 1 [running]";
        let failure =
            hidden_failure(Some("end_turn"), Some(&step(17, payload, true)), Some(NOW)).unwrap();
        assert_eq!(failure.outcome.status, TurnEndStatus::Failed);
        let error = failure.outcome.error.unwrap();
        assert!(
            error.contains("Bad Gateway: Failed to connect to backend API"),
            "{error}"
        );
        assert!(error.contains("502 502 Bad Gateway"), "{error}");
        assert!(error.contains("retry"), "{error}");
        assert_eq!(failure.resume_with, None);
        // Two errors: each message stops at the next one.
        let two = "Error 500, Message: first Error 503, Message: second, Status: UNAVAILABLE";
        let api = api_error(two).unwrap();
        assert_eq!(
            (api.http, api.message, api.status),
            (503, "second", "UNAVAILABLE")
        );
    }

    /// t3code #39869: a 404 after the model answered is transient and the
    /// turn resumes once; before the model answered it is a bad model.
    #[test]
    fn a_mid_turn_404_resumes_and_an_early_one_does_not() {
        // A multi-byte character right before the status name is no panic.
        let api = status_error("\u{80}NOT_FOUND (code 404): gone").unwrap();
        assert_eq!(
            (api.status, api.http, api.message),
            ("NOT_FOUND", 404, "gone")
        );
        let classic =
            "Error 404, Message: Requested entity was not found., Status: NOT_FOUND, Details: []";
        let personal =
            "\u{12}7NOT_FOUND (code 404): Requested entity was not found.\u{1a}\u{80}\u{1}HTTP 404";
        for payload in [classic, personal] {
            let late = hidden_failure(Some("end_turn"), Some(&step(17, payload, true)), Some(NOW))
                .unwrap();
            assert_eq!(late.resume_with, Some(RESUME_PROMPT), "{payload}");
            let error = late.outcome.error.unwrap();
            assert!(error.contains("Requested entity was not found."), "{error}");
            assert!(error.contains("404 NOT_FOUND"), "{error}");
            let early =
                hidden_failure(Some("end_turn"), Some(&step(17, payload, false)), Some(NOW))
                    .unwrap();
            assert_eq!(early.resume_with, None);
            assert!(!early.outcome.error.unwrap().contains("retry"));
        }
    }

    /// t3code #42339: a quota error keeps the reset stated only in its text.
    #[test]
    fn quotas_keep_their_reset_and_clean_stops_pass() {
        let quota =
            "RESOURCE_EXHAUSTED (code 429): You have exhausted your capacity. Resets in 4m42s.";
        let failure =
            hidden_failure(Some("end_turn"), Some(&step(17, quota, true)), Some(NOW)).unwrap();
        assert!(failure.outcome.error.unwrap().starts_with("usage_limit"));
        assert_eq!(failure.outcome.usage_limit_resets_at, Some(NOW + 282_000));
        // Read long after it happened (a silent prompt): a relative delay
        // has no anchor and is dropped; an absolute reset time is kept.
        let late = hidden_failure(Some("end_turn"), Some(&step(17, quota, true)), None).unwrap();
        assert_eq!(late.outcome.usage_limit_resets_at, None);
        let stamped =
            "RESOURCE_EXHAUSTED (code 429): out [quotaResetTimeStamp:2026-11-20T00:00:00Z]";
        let late = hidden_failure(Some("end_turn"), Some(&step(17, stamped, true)), None).unwrap();
        assert!(late.outcome.usage_limit_resets_at.is_some());
        // agy's typed quota stop, with nothing in the log.
        let refusal = hidden_failure(Some("refusal"), None, Some(NOW)).unwrap();
        assert!(refusal.outcome.error.unwrap().starts_with("usage_limit"));
        // A planner reply quoting a notice is no failure; nor is a clean stop.
        assert_eq!(
            hidden_failure(Some("end_turn"), Some(&step(15, quota, true)), Some(NOW)),
            None
        );
        assert_eq!(hidden_failure(Some("end_turn"), None, Some(NOW)), None);
        // An unparsed error step still fails the turn.
        let other =
            hidden_failure(Some("end_turn"), Some(&step(17, "boom", true)), Some(NOW)).unwrap();
        assert_eq!(other.outcome.status, TurnEndStatus::Failed);
        // A lost thinking signature marks the session broken.
        let lost = "Error 400, Message: messages.3.content.0: Invalid `signature` in `thinking` block, Status: INVALID_ARGUMENT";
        assert!(
            hidden_failure(Some("end_turn"), Some(&step(17, lost, true)), Some(NOW))
                .unwrap()
                .broken
        );
    }

    #[test]
    fn span_rows_parse() {
        assert_eq!(
            parse_span("L|17|4F4B|\nR|1\nM|4\n"),
            Some(Span {
                last: Some(step(17, "OK", true)),
                max_idx: 4
            })
        );
        // Nothing added: no step row.
        assert_eq!(
            parse_span("R|0\nM|3\n"),
            Some(Span {
                last: None,
                max_idx: 3
            })
        );
        assert_eq!(parse_span("L|17|4F4\nR|0\nM|1\n"), None);
        let done = parse_span("L|15|4F4B|3\nR|1\nM|2\n").unwrap().last;
        assert!(finished_reply(done.as_ref()));
        let open = parse_span("L|15|4F4B|2\nR|1\nM|2\n").unwrap().last;
        assert!(!finished_reply(open.as_ref()));
        assert_eq!(parse_span("R|0\n"), None);
    }

    /// Review P3: a log whose `steps` table is not created yet is probed
    /// again, not cached as a log without `status`.
    #[tokio::test]
    async fn a_missing_steps_table_is_probed_again() {
        let Some(sqlite) = sqlite::tool() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let conversations = dir.path().join(APP_DATA_DIR).join("conversations");
        std::fs::create_dir_all(&conversations).unwrap();
        let db = conversations.join("s-1.db");
        let sql = |statement: &str| {
            let ok = std::process::Command::new(&sqlite)
                .arg(&db)
                .arg(statement)
                .status()
                .unwrap()
                .success();
            assert!(ok, "{statement}");
        };
        let mut log = SessionLog::open(dir.path(), "s-1").await.unwrap();
        sql("CREATE TABLE other (x integer);");
        assert_eq!(log.peek().await, None, "no steps table yet");
        sql(
            "CREATE TABLE steps (idx integer PRIMARY KEY, step_type integer NOT NULL DEFAULT 0, \
             status integer NOT NULL DEFAULT 0, step_payload blob);\
             INSERT INTO steps VALUES (0, 15, 3, CAST('done' AS BLOB));",
        );
        let step = log.peek().await.unwrap();
        assert_eq!(step.status, Some(3));
        assert!(finished_reply(Some(&step)));
    }

    /// Each settled prompt reads only its own steps, from the session's
    /// own plain file; a link anywhere on the path is never followed.
    #[tokio::test]
    async fn each_prompt_reads_its_own_steps() {
        let Some(sqlite) = sqlite::tool() else {
            return; // no sqlite3 on this host: the typed stop alone decides
        };
        let dir = tempfile::tempdir().unwrap();
        let conversations = dir.path().join(APP_DATA_DIR).join("conversations");
        std::fs::create_dir_all(&conversations).unwrap();
        let db = conversations.join("s-1.db");
        let sql = |statement: &str| {
            let ok = std::process::Command::new(&sqlite)
                .arg(&db)
                .arg(statement)
                .status()
                .unwrap()
                .success();
            assert!(ok, "{statement}");
        };
        sql(
            "CREATE TABLE steps (idx integer PRIMARY KEY, step_type integer NOT NULL DEFAULT 0, step_payload blob);\
             INSERT INTO steps VALUES (0, 14, NULL), (1, 17, CAST('old error' AS BLOB));",
        );
        let mut log = SessionLog::open(dir.path(), "s-1").await.unwrap();
        assert_eq!(log.cursor, 1);
        assert_eq!(log.peek().await, None, "nothing added yet");
        // A first prompt fails; a steer's prompt then succeeds.
        sql(
            "INSERT INTO steps VALUES (2, 15, CAST('hi' AS BLOB)), (3, 17, CAST('Error 502, Message: Bad Gateway, Status: 502 Bad Gateway' AS BLOB));",
        );
        let first = log.take().await.unwrap();
        assert_eq!(first.step_type, 17);
        assert!(first.reached_model);
        assert!(first.text.contains("Bad Gateway"));
        sql("INSERT INTO steps VALUES (4, 15, CAST('fine' AS BLOB));");
        assert_eq!(log.take().await.unwrap().step_type, 15);
        assert_eq!(log.take().await, None, "nothing new since");
        // A session id that could leave the folder is never read.
        assert!(SessionLog::open(dir.path(), "../s-1").await.is_none());
        assert!(SessionLog::open(dir.path(), "").await.is_none());
        // A link in place of the log is never followed.
        std::os::unix::fs::symlink(&db, conversations.join("s-2.db")).unwrap();
        assert!(SessionLog::open(dir.path(), "s-2").await.is_none());
        // Nor a linked `conversations` folder.
        let other = tempfile::tempdir().unwrap();
        let acp = other.path().join(APP_DATA_DIR);
        std::fs::create_dir_all(&acp).unwrap();
        std::os::unix::fs::symlink(&conversations, acp.join("conversations")).unwrap();
        assert!(SessionLog::open(other.path(), "s-1").await.is_none());
    }
}
