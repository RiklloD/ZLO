//! Whether the account is signed in, read without starting the agent and
//! without anything that could ask the user (t3code #38957 / #39485).
//!
//! agy keeps an account sign-in in one of three places:
//! - the legacy runtime's `antigravity-acp/acp_token.json` (refreshable);
//! - the 1.5 CLI's file sign-in `antigravity-acp/antigravity-oauth-token`
//!   (`{"token": {"access_token", "refresh_token", "expiry"}}`), written
//!   when it cannot use the OS keyring;
//! - otherwise, and by default on macOS, one OS keyring entry that every
//!   profile shares: go-keyring's service `gemini`, account `antigravity`
//!   (macOS login keychain, generic password).
//!
//! The keyring entry is only looked up, never read: `security
//! find-generic-password -s gemini -a antigravity` without `-g`/`-w`
//! matches on attributes, which needs no access to the secret and so never
//! shows the keychain's access prompt. The secret stays agy's.
//!
//! Which of them counts depends on the agent that runs: the legacy
//! runtime's token is evidence only for [`AgentLaunch::Runtime`]; native
//! agy never reads it (a shared Hermes profile keeps an old one long after
//! `agy logout`), so for [`AgentLaunch::Cli`] only agy's own file sign-in
//! or keyring entry counts, and a keyring checked and absent outweighs
//! every file.
//!
//! Only this positive evidence lets a passive read (the catalog's model
//! refresh) send `authenticate`: native agy ignores `BROWSER` for its ACP
//! sign-in and opens the system browser itself, so an `authenticate` on a
//! signed-out profile pops a sign-in page whenever the picker, Settings or
//! onboarding is opened. A turn the user started may still sign in.
//!
//! Evidence is not proof: a keyring entry whose refresh token was revoked
//! (`invalid_grant`) still sends agy to the browser. So once an
//! `authenticate` failed after the last success, passive reads stop
//! probing until a turn the user started signs in again; and every agent
//! ZLO starts on its own runs with failing `open`/`xdg-open` stubs first on
//! its PATH ([`super::launch::browser_stubs`]).
//!
//! The driver's own note of how the last `authenticate` went is a
//! fallback for display where no credential store can be checked (no
//! keyring tool on this OS). Each note carries when its attempt started and
//! a later note never loses to an earlier attempt that finished late; a note
//! older than [`NOTE_TTL_MS`] proves nothing.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ironclaw_orchestrator::model::now_ms;
use serde_json::{Value, json};
use tokio::process::Command;

use super::launch::{APP_DATA_DIR, AgentLaunch};
use super::paths::{self, Kind};
use super::{AntigravityConfig, locked};

/// agy's go-keyring entry, shared by every profile.
pub const AGY_KEYRING_SERVICE: &str = "gemini";
pub const AGY_KEYRING_ACCOUNT: &str = "antigravity";
/// How long the driver's own sign-in note is trusted for display.
pub(super) const NOTE_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// A token whose access token expires sooner reads as expired.
const EXPIRY_MARGIN_MS: i64 = 60_000;
/// The largest token file read.
const MAX_TOKEN_BYTES: u64 = 64 * 1024;
/// One keyring lookup.
const KEYRING_TIMEOUT: Duration = Duration::from_secs(3);
/// How long one status is reused, so a polling screen costs one lookup.
const STATUS_TTL: Duration = Duration::from_secs(5);

/// Where agy keeps a sign-in outside the profile, and the tool that can
/// look the entry up without reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyringEntry {
    /// `/usr/bin/security` on macOS.
    pub tool: PathBuf,
    pub service: String,
    pub account: String,
}

impl KeyringEntry {
    /// agy's entry on this OS; `None` where no lookup tool that leaves the
    /// secret alone is known (Linux's Secret Service may ask to unlock).
    pub fn agy() -> Option<Self> {
        let tool = PathBuf::from("/usr/bin/security");
        (cfg!(target_os = "macos") && tool.is_file()).then(|| Self {
            tool,
            service: AGY_KEYRING_SERVICE.into(),
            account: AGY_KEYRING_ACCOUNT.into(),
        })
    }
}

/// What the keyring lookup found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Keyring {
    Present,
    Absent,
    /// No lookup tool, or it failed or timed out.
    Unknown,
}

/// The account's sign-in as a passive read sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SignIn {
    /// The catalog's `authenticated`.
    pub(super) signed_in: bool,
    /// Credentials are on hand, so `authenticate` cannot need a browser:
    /// a passive model-list refresh may start the agent.
    pub(super) may_probe: bool,
}

/// A stored token's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Valid,
    Expired,
    /// Missing, linked, oversized or malformed.
    None,
}

/// Look the keyring entry up by its attributes; never reads the secret.
pub(super) async fn keyring(entry: Option<&KeyringEntry>) -> Keyring {
    let Some(entry) = entry else {
        return Keyring::Unknown;
    };
    let mut command = Command::new(&entry.tool);
    command
        .args([
            "find-generic-password",
            "-s",
            &entry.service,
            "-a",
            &entry.account,
        ])
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    for key in ["HOME", "USER", "LOGNAME", "PATH"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    match tokio::time::timeout(KEYRING_TIMEOUT, command.status()).await {
        Ok(Ok(status)) if status.success() => Keyring::Present,
        // errSecItemNotFound
        Ok(Ok(status)) if status.code() == Some(44) => Keyring::Absent,
        Ok(Ok(status)) => {
            tracing::debug!(?status, "antigravity: keyring lookup failed");
            Keyring::Unknown
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "antigravity: keyring lookup did not start");
            Keyring::Unknown
        }
        Err(_elapsed) => {
            tracing::debug!("antigravity: keyring lookup timed out");
            Keyring::Unknown
        }
    }
}

/// A plain token file under the profile's app data folder, read whole.
async fn read_token(profile: &Path, name: &str) -> Option<Value> {
    let path = paths::managed(profile, &[APP_DATA_DIR, name], Kind::File)
        .await?
        .found()?;
    let meta = tokio::fs::symlink_metadata(&path).await.ok()?; // silent-ok: gone since the check: no token
    if meta.len() > MAX_TOKEN_BYTES {
        tracing::debug!(name, "antigravity: token file too large");
        return None;
    }
    let raw = tokio::fs::read(&path).await.ok()?; // silent-ok: an unreadable token is no sign-in
    serde_json::from_slice(&raw)
        .inspect_err(|_| tracing::debug!(name, "antigravity: token file is not JSON"))
        .ok() // silent-ok: a torn token is no sign-in, as logged above
}

fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// The legacy runtime's token: refreshable when it holds a refresh token.
fn legacy_token(doc: Option<&Value>) -> Token {
    match doc {
        Some(doc) if non_empty(doc, "refresh_token").is_some() => Token::Valid,
        _ => Token::None,
    }
}

/// agy 1.5's file sign-in: valid while its access token lasts, or when
/// agy can renew it on its own (a refresh token, no browser).
fn cli_token(doc: Option<&Value>, now: i64) -> Token {
    let Some(token) = doc.map(|d| &d["token"]) else {
        return Token::None;
    };
    if non_empty(token, "access_token").is_none() {
        return Token::None;
    }
    if non_empty(token, "refresh_token").is_some() {
        return Token::Valid;
    }
    let expires = non_empty(token, "expiry")
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(e).ok()) // silent-ok: no expiry reads as expired
        .map(|at| at.timestamp_millis());
    match expires {
        Some(at) if at > now.saturating_add(EXPIRY_MARGIN_MS) => Token::Valid,
        _ => Token::Expired,
    }
}

/// The driver's note of the last `authenticate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Note {
    /// When that attempt started (ms).
    attempt: i64,
    signed_in: bool,
    /// When `signed_in` was learned (ms).
    at: i64,
    /// The latest attempt failed: passive reads may not probe until a
    /// later one succeeds.
    failed: bool,
}

/// How one `authenticate` went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Attempt {
    SignedIn,
    /// It failed because the account is signed out.
    SignedOut,
    /// It failed for another reason (timeout, crash, a revoked token the
    /// agent did not call a sign-out): what was known stays, but passive
    /// probes stop.
    Failed,
}

fn note_file(state_dir: &Path) -> PathBuf {
    state_dir.join("signed-in")
}

fn parse_note(raw: &str) -> Option<Note> {
    // Before notes were sequenced: a bare timestamp of a success.
    if let Ok(at) = raw.trim().parse::<i64>() {
        return Some(Note {
            attempt: at,
            signed_in: true,
            at,
            failed: false,
        });
    }
    let doc: Value = serde_json::from_str(raw).ok()?; // silent-ok: a torn note proves nothing
    Some(Note {
        attempt: doc["attempt"].as_i64()?,
        signed_in: doc["signed_in"].as_bool()?,
        at: doc["at"].as_i64()?,
        failed: doc["failed"].as_bool().unwrap_or(false),
    })
}

async fn read_note(state_dir: &Path) -> Option<Note> {
    let raw = tokio::fs::read_to_string(note_file(state_dir)).await.ok()?; // silent-ok: never noted
    parse_note(&raw)
}

/// Note how the `authenticate` started at `attempt` (ms) went. An attempt
/// that started before the noted one finished late and is dropped, so an
/// overlapping sign-in or sign-out cannot be overwritten by a stale one.
pub(super) async fn note(state_dir: &Path, attempt: i64, outcome: Attempt) {
    let noted = locked::update(note_file(state_dir), move |current| {
        let before = current.and_then(parse_note);
        if before.is_some_and(|before| before.attempt > attempt) {
            return None;
        }
        let now = now_ms();
        let (signed_in, at) = match outcome {
            Attempt::SignedIn => (true, now),
            Attempt::SignedOut => (false, now),
            // Nothing new learned about the account.
            Attempt::Failed => before.map_or((false, now), |b| (b.signed_in, b.at)),
        };
        let failed = outcome != Attempt::SignedIn;
        Some(
            json!({"attempt": attempt, "signed_in": signed_in, "at": at, "failed": failed})
                .to_string(),
        )
    })
    .await;
    match noted {
        Ok(true) => {}
        Ok(false) => tracing::debug!("antigravity: a later sign-in attempt was already noted"),
        Err(error) => tracing::debug!(%error, "antigravity: sign-in state not noted"),
    }
}

/// The sign-in a passive read sees, at `now` (ms).
pub(super) async fn status(config: &AntigravityConfig, now: i64) -> SignIn {
    let legacy = legacy_token(
        read_token(&config.profile_dir, "acp_token.json")
            .await
            .as_ref(),
    );
    let cli = cli_token(
        read_token(&config.profile_dir, "antigravity-oauth-token")
            .await
            .as_ref(),
        now,
    );
    let note = read_note(&config.state_dir).await;
    // A failed attempt newer than the last success: credentials on hand
    // did not sign in without asking, so a passive read must not try.
    let unproven = config.uses_browser() && note.is_some_and(|n| n.failed);
    let evidence = match config.launch {
        // The legacy runtime reads its own token (and the file sign-in).
        AgentLaunch::Runtime { .. } => legacy == Token::Valid || cli == Token::Valid,
        // Native agy never reads `acp_token.json`.
        AgentLaunch::Cli { .. } => {
            let keyring = if config.uses_browser() {
                keyring(config.keyring.as_ref()).await
            } else {
                Keyring::Unknown
            };
            match keyring {
                // Checked and gone (a logout elsewhere): no file and no
                // older note of a success outweighs that.
                Keyring::Absent => {
                    return SignIn {
                        signed_in: false,
                        may_probe: false,
                    };
                }
                Keyring::Present => true,
                Keyring::Unknown => cli == Token::Valid,
            }
        }
    };
    if evidence {
        return SignIn {
            signed_in: true,
            may_probe: !unproven,
        };
    }
    let noted = note.is_some_and(|n| n.signed_in && now.saturating_sub(n.at) < NOTE_TTL_MS);
    SignIn {
        // An expired file sign-in is not outweighed by an older note.
        signed_in: noted && cli != Token::Expired,
        // Without a browser method, `authenticate` cannot open one.
        may_probe: !config.uses_browser(),
    }
}

/// One status reused for [`STATUS_TTL`], dropped when a sign-in is noted.
#[derive(Debug, Default)]
pub(super) struct StatusCache(Mutex<Option<(Instant, SignIn)>>);

impl StatusCache {
    pub(super) async fn get(&self, config: &AntigravityConfig) -> SignIn {
        let cached = self
            .0
            .lock()
            .ok()
            .and_then(|slot| *slot)
            .filter(|(at, _)| at.elapsed() < STATUS_TTL);
        if let Some((_, status)) = cached {
            return status;
        }
        let fresh = status(config, now_ms()).await;
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some((Instant::now(), fresh));
        }
        fresh
    }

    pub(super) fn invalidate(&self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn token_files_count_only_while_usable() {
        let doc = |v: Value| Some(v);
        assert_eq!(legacy_token(doc(json!({})).as_ref()), Token::None);
        assert_eq!(
            legacy_token(doc(json!({"refresh_token": "r"})).as_ref()),
            Token::Valid
        );
        let at = |ms: i64| {
            chrono::DateTime::from_timestamp_millis(ms)
                .unwrap()
                .to_rfc3339()
        };
        let cli = |token: Value| cli_token(Some(&json!({"token": token})), NOW);
        assert_eq!(cli_token(None, NOW), Token::None);
        assert_eq!(cli_token(Some(&json!({})), NOW), Token::None);
        assert_eq!(cli(json!({"access_token": ""})), Token::None);
        assert_eq!(
            cli(json!({"access_token": "a", "expiry": at(NOW + 3_600_000)})),
            Token::Valid
        );
        assert_eq!(
            cli(json!({"access_token": "a", "expiry": at(NOW - 1)})),
            Token::Expired
        );
        assert_eq!(cli(json!({"access_token": "a"})), Token::Expired);
        assert_eq!(
            cli(json!({"access_token": "a", "expiry": "soon"})),
            Token::Expired
        );
        // agy renews an expired access token itself with its refresh token.
        assert_eq!(
            cli(json!({"access_token": "a", "refresh_token": "r", "expiry": at(NOW - 1)})),
            Token::Valid
        );
    }

    /// A slow attempt that started first cannot overwrite a later one.
    #[tokio::test]
    async fn a_stale_attempt_never_overwrites_a_later_one() {
        let dir = tempfile::tempdir().unwrap();
        note(dir.path(), 200, Attempt::SignedOut).await;
        note(dir.path(), 100, Attempt::SignedIn).await;
        let noted = read_note(dir.path()).await.unwrap();
        assert_eq!((noted.attempt, noted.signed_in), (200, false));
        note(dir.path(), 300, Attempt::SignedIn).await;
        let noted = read_note(dir.path()).await.unwrap();
        assert!(noted.signed_in && !noted.failed);
        // A failure that proves no sign-out keeps what was known, but
        // marks the latest attempt failed until a later success.
        note(dir.path(), 400, Attempt::Failed).await;
        let failed = read_note(dir.path()).await.unwrap();
        assert_eq!(
            (failed.signed_in, failed.at, failed.failed),
            (true, noted.at, true)
        );
        note(dir.path(), 500, Attempt::SignedIn).await;
        assert!(!read_note(dir.path()).await.unwrap().failed);
        // The unsequenced note of older builds still reads.
        assert_eq!(
            parse_note("12345"),
            Some(Note {
                attempt: 12345,
                signed_in: true,
                at: 12345,
                failed: false,
            })
        );
        assert_eq!(parse_note("{}"), None);
    }

    #[tokio::test]
    async fn the_keyring_lookup_never_asks_for_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args");
        let tool = dir.path().join("security");
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n[ \"$3\" = present ] && exit 0\nexit 44\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let entry = |service: &str| KeyringEntry {
            tool: tool.clone(),
            service: service.into(),
            account: "antigravity".into(),
        };
        assert_eq!(keyring(Some(&entry("present"))).await, Keyring::Present);
        assert_eq!(keyring(Some(&entry("gone"))).await, Keyring::Absent);
        assert_eq!(keyring(None).await, Keyring::Unknown);
        let args = std::fs::read_to_string(&log).unwrap();
        assert_eq!(args.lines().count(), 2, "{args}");
        assert!(
            args.lines()
                .all(|l| l.starts_with("find-generic-password -s ")
                    && l.split_whitespace().all(|a| a != "-w" && a != "-g")),
            "{args}"
        );
    }

    /// Review P3: `ironclaw_usage` looks agy's entry up on its own (the
    /// crates are peers); both tests pin the exact same lookup literal.
    #[tokio::test]
    async fn production_lookup_matches_the_usage_monitor() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args");
        let tool = dir.path().join("security");
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let entry = KeyringEntry {
            tool,
            ..KeyringEntry::agy().unwrap_or(KeyringEntry {
                tool: PathBuf::new(),
                service: AGY_KEYRING_SERVICE.into(),
                account: AGY_KEYRING_ACCOUNT.into(),
            })
        };
        assert_eq!(keyring(Some(&entry)).await, Keyring::Present);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "find-generic-password -s gemini -a antigravity\n"
        );
    }
}
