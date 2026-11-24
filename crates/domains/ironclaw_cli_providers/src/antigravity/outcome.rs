//! Failures agy hides behind a clean stop (t3code's
//! `AntigravityTurnOutcome`). agy reports an upstream API failure (a 502, a
//! mid-turn 404, a quota) as a chat message and `end_turn`; its own session
//! log (`<profile>/antigravity-acp/conversations/<session>.db`) ends that
//! turn with an error step. The driver reads only the steps this turn added,
//! read-only, from the session's own file: never a link, never a file that
//! replaced the one seen before the turn.
//!
//! The log is SQLite. It is read through the system `sqlite3` tool rather
//! than a linked driver (the persistence driver cone stays where
//! `reborn_persistence_driver_boundary.rs` allows it); without the tool, or
//! with an unreadable log, the typed stop reason alone decides, as before.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ironclaw_orchestrator::runtime::{TurnEndStatus, TurnOutcome};
use tokio::process::Command;

use super::fold::{INVALID_THINKING_SIGNATURE, reset_at, usage_limited};
use super::launch::APP_DATA_DIR;

/// `CORTEX_STEP_TYPE_ERROR_MESSAGE` in agy's trajectory.
const ERROR_STEP: i64 = 17;
/// `CORTEX_STEP_TYPE_PLANNER_RESPONSE`: the model answered within the turn.
const PLANNER_RESPONSE_STEP: i64 = 15;
/// The harness error text leads the payload; a Go stack trace follows it.
const MAX_ERROR_PAYLOAD_BYTES: usize = 16_384;
/// One `sqlite3` read, output included.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
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

/// The failure behind a prompt's stop: an error step this turn added, or a
/// typed `refusal` (agy's quota stop) without one. `now` anchors a relative
/// reset delay (ms).
pub(super) fn hidden_failure(
    stop_reason: Option<&str>,
    step: Option<&LastStep>,
    now: i64,
) -> Option<HiddenFailure> {
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
        return Some(failed(usage_limited(reset_at(text, now))));
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
        return Some(failed(usage_limited(reset_at(text, now))));
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

/// The session's own log, as it stood before the turn's first prompt.
#[derive(Debug)]
pub(super) struct SessionLog {
    path: PathBuf,
    /// `(dev, ino)` of the file seen before the turn; `None` when the turn
    /// creates it.
    identity: Option<(u64, u64)>,
    /// The last step index before the turn.
    start: i64,
}

/// `(dev, ino)` of a plain file, never through a link.
async fn plain_file(path: &Path) -> Result<Option<(u64, u64)>, ()> {
    use std::os::unix::fs::MetadataExt;
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.file_type().is_file() => Ok(Some((meta.dev(), meta.ino()))),
        Ok(_) => Err(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            tracing::debug!(%error, "antigravity: session log unreadable");
            Err(())
        }
    }
}

impl SessionLog {
    /// Mark where the turn starts; `None` when the log cannot be trusted
    /// (a link, an unreadable file, no `sqlite3`), and the typed stop
    /// reason alone decides.
    pub(super) async fn open(profile: &Path, session_id: &str) -> Option<Self> {
        let valid = !session_id.is_empty()
            && session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid {
            return None;
        }
        let path = profile
            .join(APP_DATA_DIR)
            .join("conversations")
            .join(format!("{session_id}.db"));
        let identity = plain_file(&path).await.ok()?;
        let start = match identity {
            None => -1,
            Some(_) => query(&path, "SELECT coalesce(max(idx), -1) FROM steps;")
                .await?
                .trim()
                .parse()
                .ok()?,
        };
        Some(Self {
            path,
            identity,
            start,
        })
    }

    /// The newest step the turn added: from the same file as before the
    /// turn, or the plain file the turn created.
    pub(super) async fn last_step(&self) -> Option<LastStep> {
        let now = plain_file(&self.path).await.ok()??;
        if self.identity.is_some_and(|before| before != now) {
            tracing::debug!("antigravity: session log was replaced during the turn");
            return None;
        }
        let start = self.start;
        let sql = format!(
            "SELECT step_type, hex(substr(step_payload, 1, {MAX_ERROR_PAYLOAD_BYTES})) \
             FROM steps WHERE idx > {start} ORDER BY idx DESC LIMIT 1; \
             SELECT EXISTS(SELECT 1 FROM steps WHERE idx > {start} \
             AND step_type = {PLANNER_RESPONSE_STEP});"
        );
        parse_last_step(&query(&self.path, &sql).await?)
    }
}

/// The two result rows of [`SessionLog::last_step`]'s query.
fn parse_last_step(output: &str) -> Option<LastStep> {
    let mut lines = output.lines();
    let (step_type, hex) = lines.next()?.split_once('|')?;
    let reached_model = lines.next().is_some_and(|l| l.trim() == "1");
    Some(LastStep {
        step_type: step_type.trim().parse().ok()?,
        text: String::from_utf8_lossy(&unhex(hex.trim())?).into_owned(),
        reached_model,
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

/// The system `sqlite3`, if any.
fn sqlite3() -> Option<PathBuf> {
    let system = PathBuf::from("/usr/bin/sqlite3");
    if system.is_file() {
        return Some(system);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    crate::rpc::find_on_path(&home, "sqlite3")
}

/// Run one read-only query, bounded; `None` on any failure.
async fn query(db: &Path, sql: &str) -> Option<String> {
    let tool = sqlite3()?;
    let run = Command::new(tool)
        .args(["-readonly", "-batch", "-noheader", "-separator", "|"])
        .args(["-cmd", ".timeout 100"])
        .arg(db)
        .arg(sql)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(QUERY_TIMEOUT, run).await {
        Ok(Ok(out)) if out.status.success() => String::from_utf8(out.stdout).ok(),
        Ok(Ok(out)) => {
            tracing::debug!(status = ?out.status, "antigravity: session log query failed");
            None
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "antigravity: sqlite3 did not start");
            None
        }
        Err(_elapsed) => {
            tracing::debug!("antigravity: session log query timed out");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(step_type: i64, text: &str, reached_model: bool) -> LastStep {
        LastStep {
            step_type,
            text: text.into(),
            reached_model,
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
            hidden_failure(Some("end_turn"), Some(&step(17, payload, true)), NOW).unwrap();
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
            let late =
                hidden_failure(Some("end_turn"), Some(&step(17, payload, true)), NOW).unwrap();
            assert_eq!(late.resume_with, Some(RESUME_PROMPT), "{payload}");
            let error = late.outcome.error.unwrap();
            assert!(error.contains("Requested entity was not found."), "{error}");
            assert!(error.contains("404 NOT_FOUND"), "{error}");
            let early =
                hidden_failure(Some("end_turn"), Some(&step(17, payload, false)), NOW).unwrap();
            assert_eq!(early.resume_with, None);
            assert!(!early.outcome.error.unwrap().contains("retry"));
        }
    }

    /// t3code #42339: a quota error keeps the reset stated only in its text.
    #[test]
    fn quotas_keep_their_reset_and_clean_stops_pass() {
        let quota =
            "RESOURCE_EXHAUSTED (code 429): You have exhausted your capacity. Resets in 4m42s.";
        let failure = hidden_failure(Some("end_turn"), Some(&step(17, quota, true)), NOW).unwrap();
        assert!(failure.outcome.error.unwrap().starts_with("usage_limit"));
        assert_eq!(failure.outcome.usage_limit_resets_at, Some(NOW + 282_000));
        // agy's typed quota stop, with nothing in the log.
        let refusal = hidden_failure(Some("refusal"), None, NOW).unwrap();
        assert!(refusal.outcome.error.unwrap().starts_with("usage_limit"));
        // A planner reply quoting a notice is no failure; nor is a clean stop.
        assert_eq!(
            hidden_failure(Some("end_turn"), Some(&step(15, quota, true)), NOW),
            None
        );
        assert_eq!(hidden_failure(Some("end_turn"), None, NOW), None);
        // An unparsed error step still fails the turn.
        let other = hidden_failure(Some("end_turn"), Some(&step(17, "boom", true)), NOW).unwrap();
        assert_eq!(other.outcome.status, TurnEndStatus::Failed);
        // A lost thinking signature marks the session broken.
        let lost = "Error 400, Message: messages.3.content.0: Invalid `signature` in `thinking` block, Status: INVALID_ARGUMENT";
        assert!(
            hidden_failure(Some("end_turn"), Some(&step(17, lost, true)), NOW)
                .unwrap()
                .broken
        );
    }

    #[test]
    fn last_step_rows_parse() {
        assert_eq!(parse_last_step("17|4F4B\n1\n"), Some(step(17, "OK", true)));
        assert_eq!(parse_last_step("0\n"), None);
        assert_eq!(parse_last_step("17|4F4\n0\n"), None);
    }

    /// Reads only what the turn added, from the session's own plain file.
    #[tokio::test]
    async fn the_log_is_read_from_the_turns_own_steps() {
        if sqlite3().is_none() {
            return; // no sqlite3 on this host: the typed stop alone decides
        }
        let dir = tempfile::tempdir().unwrap();
        let conversations = dir.path().join(APP_DATA_DIR).join("conversations");
        std::fs::create_dir_all(&conversations).unwrap();
        let db = conversations.join("s-1.db");
        let sql = |statement: &str| {
            let ok = std::process::Command::new(sqlite3().unwrap())
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
        let log = SessionLog::open(dir.path(), "s-1").await.unwrap();
        assert_eq!(log.start, 1);
        assert_eq!(log.last_step().await, None, "nothing added yet");
        sql(
            "INSERT INTO steps VALUES (2, 15, CAST('hi' AS BLOB)), (3, 17, CAST('Error 502, Message: Bad Gateway, Status: 502 Bad Gateway' AS BLOB));",
        );
        let last = log.last_step().await.unwrap();
        assert_eq!(last.step_type, 17);
        assert!(last.reached_model);
        assert!(last.text.contains("Bad Gateway"));
        // A session id that could leave the folder is never read.
        assert!(SessionLog::open(dir.path(), "../s-1").await.is_none());
        // A link in place of the log is never followed.
        std::os::unix::fs::symlink(&db, conversations.join("s-2.db")).unwrap();
        assert!(SessionLog::open(dir.path(), "s-2").await.is_none());
    }
}
