//! Unsent composer drafts, like t3code's: one per thread plus one for the
//! new-thread screen, kept until the message is sent or the box is emptied.
//! They live on this Mac only, one JSON file per draft under
//! `~/Library/Application Support/ZLO/drafts/` (the GPUI app's folder and
//! format, so both apps see the same drafts). Memory updates on every edit;
//! the disk write waits 300 ms after the last one, and quit writes whatever
//! is still pending. File cards come back with their files: a path is re-read
//! on send, a pasted picture is kept inline as base64. (GPUI `drafts.rs`; its
//! global lives in `WorkspaceState::drafts`.)
//!
//! A draft's text carries inline tokens (file cards, `@` mentions) as byte
//! ranges, [`DraftToken`]; the composer (area A) maps them to its pills.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use ironclaw_gpui_client::attachments::{self, Body, StagedFile};
use serde::{Deserialize, Serialize};

use crate::jobs;
use crate::state::State;

/// The new-thread screen's draft key. Thread ids never collide with it.
pub const NEW_THREAD: &str = "new";

const SAVE_DELAY: Duration = Duration::from_millis(300);
/// How long quit waits for queued draft writes.
const QUIT_WAIT: Duration = Duration::from_secs(3);

/// One inline token in a draft's text: a file card or a mention chip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftToken {
    pub range: Range<usize>,
    pub id: String,
    pub text: String,
    pub label: String,
}

#[derive(Clone)]
pub struct Draft {
    pub text: String,
    pub tokens: Vec<DraftToken>,
    pub files: Vec<StagedFile>,
    /// The new-thread screen's chosen profile.
    pub profile: Option<String>,
}

impl Draft {
    /// A plain-text draft without tokens or files.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tokens: Vec::new(),
            files: Vec::new(),
            profile: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.tokens.is_empty()
    }
}

#[derive(Default)]
pub struct Store {
    drafts: HashMap<String, Draft>,
    dirty: HashSet<String>,
    /// Bumped per edit; only the newest delayed save writes.
    generation: u64,
    /// Tests write here instead of the user's drafts folder.
    dir: Option<PathBuf>,
}

impl Store {
    /// A scene's draft: in memory, never written.
    pub fn insert_fixture(&mut self, key: &str, draft: Draft) {
        self.drafts.insert(key.to_string(), draft);
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    key: String,
    text: String,
    #[serde(default)]
    tokens: Vec<SavedToken>,
    #[serde(default)]
    files: Vec<SavedFile>,
    #[serde(default)]
    profile: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SavedToken {
    start: usize,
    end: usize,
    id: String,
    text: String,
    label: String,
}

#[derive(Serialize, Deserialize)]
struct SavedFile {
    key: u64,
    filename: String,
    mime_type: String,
    size: u64,
    #[serde(default)]
    path: Option<PathBuf>,
    /// Base64 bytes for a file with no path (a pasted picture, a SnapShot).
    #[serde(default)]
    data: Option<String>,
}

fn dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("ZLO").join("drafts"))
}

fn file_for(dir: &Path, key: &str) -> PathBuf {
    let name: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{name}.json"))
}

/// Read the saved drafts. Called once at startup, before the first frame.
pub fn load(s: &mut State) {
    let mut store = Store::default();
    let entries = dir().and_then(|d| std::fs::read_dir(d).ok()); // silent-ok: no folder yet means no drafts
    for entry in entries.into_iter().flatten().flatten() {
        let saved = std::fs::read(entry.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Saved>(&b).ok()); // silent-ok: a bad file is skipped
        if let Some((key, draft)) = saved.map(from_saved) {
            store.drafts.insert(key, draft);
        }
    }
    s.workspace.drafts = store;
}

fn from_saved(saved: Saved) -> (String, Draft) {
    let files = saved
        .files
        .into_iter()
        .filter_map(|f| {
            let body = match (f.path, f.data) {
                (Some(path), _) => Body::Path(path),
                (None, Some(data)) => Body::Bytes(
                    base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .ok()?,
                ),
                (None, None) => return None,
            };
            Some(StagedFile {
                key: f.key,
                filename: f.filename,
                mime_type: f.mime_type,
                size: f.size,
                body,
            })
        })
        .collect();
    let len = saved.text.len();
    let tokens = saved
        .tokens
        .into_iter()
        // A token past the text or off a char boundary is dropped, as
        // GPUI's `InputContent::with_token` refuses it.
        .filter(|t| {
            t.start < t.end
                && t.end <= len
                && saved.text.is_char_boundary(t.start)
                && saved.text.is_char_boundary(t.end)
        })
        .map(|t| DraftToken {
            range: t.start..t.end,
            id: t.id,
            text: t.text,
            label: t.label,
        })
        .collect();
    (
        saved.key,
        Draft {
            text: saved.text,
            tokens,
            files,
            profile: saved.profile,
        },
    )
}

fn to_saved(key: &str, draft: &Draft) -> Saved {
    Saved {
        key: key.to_string(),
        text: draft.text.clone(),
        tokens: draft
            .tokens
            .iter()
            .map(|t| SavedToken {
                start: t.range.start,
                end: t.range.end,
                id: t.id.clone(),
                text: t.text.clone(),
                label: t.label.clone(),
            })
            .collect(),
        files: draft
            .files
            .iter()
            .map(|f| {
                let (path, data) = match &f.body {
                    Body::Path(path) => (Some(path.clone()), None),
                    Body::Bytes(bytes) => (
                        None,
                        Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
                    ),
                };
                SavedFile {
                    key: f.key,
                    filename: f.filename.clone(),
                    mime_type: f.mime_type.clone(),
                    size: f.size,
                    path,
                    data,
                }
            })
            .collect(),
        profile: draft.profile.clone(),
    }
}

pub fn has(s: &State, key: &str) -> bool {
    s.workspace.drafts.drafts.contains_key(key)
}

/// The new-thread draft's sidebar card: its first line and profile.
pub fn preview(s: &State, key: &str) -> Option<(String, Option<String>)> {
    let draft = s.workspace.drafts.drafts.get(key)?;
    let line = draft
        .text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("New thread")
        .to_string();
    Some((line, draft.profile.clone()))
}

/// The saved draft, ready for a composer. Files get fresh keys (the cards
/// are re-pointed at them), and a card whose file has left the disk is
/// dropped together with its text.
pub fn restore(s: &State, key: &str) -> Option<Draft> {
    let draft = s.workspace.drafts.drafts.get(key)?.clone();
    Some(rekey(draft))
}

fn rekey(draft: Draft) -> Draft {
    let mut keys = HashMap::new();
    let files: Vec<StagedFile> = draft
        .files
        .into_iter()
        .filter(|f| !matches!(&f.body, Body::Path(p) if !p.exists()))
        .map(|mut f| {
            let fresh = attachments::next_key();
            keys.insert(f.key.to_string(), fresh.to_string());
            f.key = fresh;
            f
        })
        .collect();
    let old = &draft.text;
    let mut text = String::new();
    let mut tokens = Vec::new();
    let mut last = 0;
    let mut spans = draft.tokens.clone();
    spans.sort_by_key(|t| t.range.start);
    for token in spans {
        let range = token.range.clone();
        if range.start < last {
            continue;
        }
        text.push_str(&old[last..range.start]);
        last = range.end;
        // A mention chip names its target, not a staged file: it keeps its id.
        let mention = ironclaw_gpui_client::mentions::token_mention(&token.id)
            .map(|_| token.id.clone());
        if let Some(id) = keys.get(&token.id).cloned().or(mention) {
            let start = text.len();
            text.push_str(&token.text);
            tokens.push(DraftToken {
                range: start..text.len(),
                id,
                text: token.text,
                label: token.label,
            });
        }
    }
    text.push_str(&old[last..]);
    Draft {
        text,
        tokens,
        files,
        profile: draft.profile,
    }
}

/// Note the composer's current state; an empty box deletes the draft.
pub fn record(s: &mut State, key: &str, draft: Draft) {
    let store = &mut s.workspace.drafts;
    let had = store.drafts.contains_key(key);
    if draft.is_empty() {
        if !had {
            return;
        }
        store.drafts.remove(key);
    } else {
        let same = store.drafts.get(key).is_some_and(|d| {
            d.text == draft.text && d.tokens == draft.tokens && d.profile == draft.profile
        });
        if same {
            return;
        }
        store.drafts.insert(key.to_string(), draft);
    }
    store.dirty.insert(key.to_string());
    store.generation += 1;
    let generation = store.generation;
    jobs::after(s, SAVE_DELAY, move |s, _| {
        if s.workspace.drafts.generation == generation {
            save(s, false);
        }
    });
}

pub fn remove(s: &mut State, key: &str) {
    record(s, key, Draft::text(""));
}

/// Hand the changed drafts to the app's one ordered writer (`persist`):
/// writes and deletions of a draft land in the order they were made, each
/// file is replaced atomically, and a failed write stays queued for a
/// retry. `now` (quit) also waits for every queued write, earlier ones too.
pub fn save(s: &mut State, now: bool) {
    save_with(s, crate::persist::global(), now);
}

fn save_with(s: &mut State, writer: &crate::persist::Writer, now: bool) {
    let store = &mut s.workspace.drafts;
    let dir = store.dir.clone().or_else(dir);
    if let Some(dir) = dir {
        for key in store.dirty.drain() {
            let file = file_for(&dir, &key);
            match store.drafts.get(&key).map(|d| serde_json::to_vec(&to_saved(&key, d))) {
                Some(Ok(json)) => writer.write(file, json),
                Some(Err(e)) => tracing::warn!("could not encode a draft: {e}"),
                None => writer.remove(file),
            }
        }
    }
    if now && !writer.flush(QUIT_WAIT) {
        tracing::warn!("some drafts were not saved before quit");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_drops_cards_whose_file_is_gone_and_keeps_mentions() {
        let gone = StagedFile {
            key: 7,
            filename: "gone.txt".into(),
            mime_type: "text/plain".into(),
            size: 1,
            body: Body::Path(PathBuf::from("/definitely/not/here.txt")),
        };
        let text = "see [gone] and @x end".to_string();
        let draft = Draft {
            tokens: vec![
                DraftToken {
                    range: 4..10,
                    id: "7".into(),
                    text: "[gone]".into(),
                    label: "gone.txt".into(),
                },
            ],
            files: vec![gone],
            profile: Some("p".into()),
            text,
        };
        let out = rekey(draft);
        assert_eq!(out.text, "see  and @x end");
        assert!(out.files.is_empty() && out.tokens.is_empty());
        assert_eq!(out.profile.as_deref(), Some("p"));
    }

    #[test]
    fn a_sent_draft_is_not_resurrected_by_an_earlier_save() {
        let dir = std::env::temp_dir().join(format!("zlo-drafts-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let writer = crate::persist::Writer::spawn();
        let mut s = crate::chat::fixtures::test_state();
        s.workspace.drafts.dir = Some(dir.clone());
        // Many edits each saved, then the send clears the draft; the quit
        // save waits for all of them.
        for i in 0..50 {
            s.workspace.drafts.drafts.insert("t1".into(), Draft::text(format!("draft {i}")));
            s.workspace.drafts.dirty.insert("t1".into());
            save_with(&mut s, &writer, false);
        }
        s.workspace.drafts.drafts.remove("t1");
        s.workspace.drafts.dirty.insert("t1".into());
        s.workspace.drafts.drafts.insert("t2".into(), Draft::text("kept"));
        s.workspace.drafts.dirty.insert("t2".into());
        save_with(&mut s, &writer, true);
        assert!(!file_for(&dir, "t1").exists(), "the sent draft came back");
        let kept: Saved = serde_json::from_slice(&std::fs::read(file_for(&dir, "t2")).unwrap()).unwrap();
        assert_eq!(kept.text, "kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_round_trip_keeps_text_tokens_and_inline_bytes() {
        let draft = Draft {
            text: "hi [a]".into(),
            tokens: vec![DraftToken {
                range: 3..6,
                id: "1".into(),
                text: "[a]".into(),
                label: "a.png".into(),
            }],
            files: vec![StagedFile {
                key: 1,
                filename: "a.png".into(),
                mime_type: "image/png".into(),
                size: 3,
                body: Body::Bytes(vec![1, 2, 3]),
            }],
            profile: None,
        };
        let (key, back) = from_saved(to_saved("t1", &draft));
        assert_eq!(key, "t1");
        assert_eq!(back.text, draft.text);
        assert_eq!(back.tokens, draft.tokens);
        assert!(matches!(&back.files[0].body, Body::Bytes(b) if b == &[1, 2, 3]));
    }
}
