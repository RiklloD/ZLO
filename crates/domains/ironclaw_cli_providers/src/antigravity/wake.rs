//! Whether agy has a reply of its own coming (t3code #39989): a waking
//! (high-priority) message in the session's inbox it has not read, or one it
//! already took as the trailing `SYSTEM_MESSAGE` step of its transcript with
//! no reply yet. A second background task can finish while the reply to the
//! first one runs; without this the turn would end before agy answers it.
//!
//! Read-only over agy's own records under the profile
//! (`antigravity-acp/brain/<session>/.system_generated/`), bounded, never
//! through a link; anything missing, linked, oversized or unreadable reads
//! as "nothing pending", so the turn ends as it would have.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::launch::APP_DATA_DIR;
use super::paths::{self, Kind};

const HIGH: &str = "MESSAGE_PRIORITY_HIGH";
const LOW: &str = "MESSAGE_PRIORITY_LOW";
/// At most this many inbox messages are looked at.
const MAX_MESSAGES: usize = 256;
/// The largest inbox file read.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// How much of the transcript's end is read for its last line.
const TAIL_BYTES: u64 = 64 * 1024;

/// `brain/<session>/.system_generated/<parts>` under the profile, reached
/// through plain folders only ([`paths::managed`]).
async fn generated(
    profile: &Path,
    session_id: &str,
    parts: &[&str],
    kind: Kind,
) -> Option<PathBuf> {
    let valid = !session_id.is_empty()
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid {
        return None;
    }
    let mut all = vec![APP_DATA_DIR, "brain", session_id, ".system_generated"];
    all.extend_from_slice(parts);
    paths::managed(profile, &all, kind).await?.found()
}

/// A plain file (never a link) of at most [`MAX_FILE_BYTES`], read whole.
async fn read_plain(path: &Path) -> Option<String> {
    let meta = tokio::fs::symlink_metadata(path).await.ok()?; // silent-ok: a missing record means nothing pending
    if !meta.file_type().is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    tokio::fs::read_to_string(path).await.ok() // silent-ok: an unreadable record means nothing pending
}

/// Whether agy has a waking message it has not answered yet.
pub(super) async fn pending_reply(profile: &Path, session_id: &str) -> bool {
    if let Some(inbox) = generated(profile, session_id, &["messages"], Kind::Dir).await
        && unread_waking_message(&inbox).await
    {
        return true;
    }
    match generated(
        profile,
        session_id,
        &["logs", "transcript.jsonl"],
        Kind::File,
    )
    .await
    {
        Some(transcript) => last_step_waits(&transcript).await,
        None => false,
    }
}

async fn unread_waking_message(inbox: &Path) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(inbox).await else {
        return false; // silent-ok: an unreadable inbox means nothing pending
    };
    let mut names = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".json") && name != "read.json" {
            names.push(name);
            if names.len() > MAX_MESSAGES {
                return false;
            }
        }
    }
    if names.is_empty() {
        return false;
    }
    let receipts: Value = match read_plain(&inbox.join("read.json")).await {
        Some(raw) => match serde_json::from_str(&raw) {
            Ok(receipts) => receipts,
            Err(_) => return false, // silent-ok: torn receipts prove nothing
        },
        None => Value::Null,
    };
    let read = |id: &str| receipts.get(id).and_then(Value::as_bool) == Some(true);
    for name in names {
        let file_id = name.trim_end_matches(".json");
        if read(file_id) {
            continue;
        }
        let Some(raw) = read_plain(&inbox.join(&name)).await else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(&raw) else {
            continue; // silent-ok: a torn message is not a wake
        };
        if message.get("id").and_then(Value::as_str).is_some_and(read) {
            continue;
        }
        if message.get("priority").and_then(Value::as_str) == Some(HIGH) {
            return true;
        }
    }
    false
}

/// The transcript ends on a `SYSTEM_MESSAGE` step (a dequeued wake) that
/// is not a low-priority one: agy has not replied to it yet.
async fn last_step_waits(transcript: &Path) -> bool {
    let Some(line) = last_line(transcript).await else {
        return false;
    };
    let Ok(step) = serde_json::from_str::<Value>(&line) else {
        return false; // silent-ok: a torn line proves nothing
    };
    if step.get("type").and_then(Value::as_str) != Some("SYSTEM_MESSAGE") {
        return false;
    }
    match step.get("content").and_then(Value::as_str) {
        None => true,
        Some(content) => {
            content.contains(&format!("priority={HIGH}"))
                || !content.contains(&format!("priority={LOW}"))
        }
    }
}

/// The last non-empty line within the file's last [`TAIL_BYTES`].
async fn last_line(path: &Path) -> Option<String> {
    let meta = tokio::fs::symlink_metadata(path).await.ok()?; // silent-ok: no transcript means nothing pending
    if !meta.file_type().is_file() || meta.len() == 0 {
        return None;
    }
    let mut file = tokio::fs::File::open(path).await.ok()?; // silent-ok: as above
    let start = meta.len().saturating_sub(TAIL_BYTES);
    file.seek(std::io::SeekFrom::Start(start)).await.ok()?; // silent-ok: as above
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).await.ok()?; // silent-ok: as above
    let text = String::from_utf8_lossy(&bytes);
    let trimmed = text.trim_end_matches(['\n', '\r']);
    match trimmed.rfind('\n') {
        Some(at) => Some(trimmed[at + 1..].to_string()),
        // A tail that starts mid-line holds no whole line.
        None if start > 0 => None,
        None => Some(trimmed.to_string()).filter(|l| !l.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .join(APP_DATA_DIR)
            .join("brain/s-1/.system_generated");
        std::fs::create_dir_all(root.join("messages")).unwrap();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        (dir, root)
    }

    #[tokio::test]
    async fn an_unread_high_priority_message_is_a_pending_reply() {
        let (dir, root) = setup();
        assert!(!pending_reply(dir.path(), "s-1").await);
        std::fs::write(
            root.join("messages/m1.json"),
            r#"{"id":"m1","priority":"MESSAGE_PRIORITY_HIGH"}"#,
        )
        .unwrap();
        assert!(pending_reply(dir.path(), "s-1").await);
        // Read: no longer pending.
        std::fs::write(root.join("messages/read.json"), r#"{"m1":true}"#).unwrap();
        assert!(!pending_reply(dir.path(), "s-1").await);
        // A low-priority message never wakes the agent.
        std::fs::write(
            root.join("messages/m2.json"),
            r#"{"id":"m2","priority":"MESSAGE_PRIORITY_LOW"}"#,
        )
        .unwrap();
        assert!(!pending_reply(dir.path(), "s-1").await);
        assert!(!pending_reply(dir.path(), "../s-1").await);
    }

    #[tokio::test]
    async fn a_trailing_system_message_waits_for_its_reply() {
        let (dir, root) = setup();
        let transcript = root.join("logs/transcript.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"USER_INPUT\"}\n{\"type\":\"SYSTEM_MESSAGE\",\"content\":\"task done priority=MESSAGE_PRIORITY_HIGH\"}\n",
        )
        .unwrap();
        assert!(pending_reply(dir.path(), "s-1").await);
        std::fs::write(
            &transcript,
            "{\"type\":\"SYSTEM_MESSAGE\",\"content\":\"note priority=MESSAGE_PRIORITY_LOW\"}\n",
        )
        .unwrap();
        assert!(!pending_reply(dir.path(), "s-1").await);
        std::fs::write(
            &transcript,
            "{\"type\":\"SYSTEM_MESSAGE\"}\n{\"type\":\"PLANNER_RESPONSE\"}\n",
        )
        .unwrap();
        assert!(!pending_reply(dir.path(), "s-1").await);
    }

    #[tokio::test]
    async fn a_linked_parent_folder_is_never_followed() {
        let (dir, root) = setup();
        let elsewhere = tempfile::tempdir().unwrap();
        let generated = elsewhere.path().join("s-1/.system_generated/messages");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(
            generated.join("m1.json"),
            r#"{"id":"m1","priority":"MESSAGE_PRIORITY_HIGH"}"#,
        )
        .unwrap();
        // `brain` itself is a link out of the profile.
        let brain = root.parent().unwrap().parent().unwrap().to_path_buf();
        std::fs::remove_dir_all(&brain).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &brain).unwrap();
        assert!(!pending_reply(dir.path(), "s-1").await);
    }

    #[tokio::test]
    async fn links_are_never_followed() {
        let (dir, root) = setup();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(
            elsewhere.path().join("m1.json"),
            r#"{"id":"m1","priority":"MESSAGE_PRIORITY_HIGH"}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("m1.json"),
            root.join("messages/m1.json"),
        )
        .unwrap();
        assert!(!pending_reply(dir.path(), "s-1").await);
    }
}
