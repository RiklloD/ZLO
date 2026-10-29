//! Unsent composer drafts, like t3code's: one per thread plus one for the
//! new-thread screen, kept until the message is sent or the box is emptied.
//! They live on this Mac only, one JSON file per draft under
//! `~/Library/Application Support/Numoh/drafts/`. Memory updates on every
//! edit; the disk write waits 300 ms after the last one, and quit writes
//! whatever is still pending. File cards come back with their files: a path
//! is re-read on send, a pasted picture is kept inline as base64.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use ironclaw_gpui_client::attachments::{self, Body, StagedFile};
use serde::{Deserialize, Serialize};

use crate::ui::*;

/// The new-thread screen's draft key. Thread ids never collide with it.
pub const NEW_THREAD: &str = "new";

const SAVE_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone)]
pub struct Draft {
    pub content: InputContent,
    pub files: Vec<StagedFile>,
    /// The new-thread screen's chosen profile.
    pub profile: Option<String>,
}

impl Draft {
    fn is_empty(&self) -> bool {
        self.content.text().trim().is_empty() && self.content.tokens().is_empty()
    }
}

#[derive(Default)]
struct Store {
    drafts: HashMap<String, Draft>,
    dirty: HashSet<String>,
    /// Bumped per edit; only the newest delayed save writes.
    generation: u64,
}

impl Global for Store {}

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
    Some(dirs::config_dir()?.join("Numoh").join("drafts"))
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

/// Read the saved drafts. Called once at startup.
pub fn load(cx: &mut App) {
    let mut store = Store::default();
    let entries = dir().and_then(|d| std::fs::read_dir(d).ok()); // silent-ok: no folder yet means no drafts
    for entry in entries.into_iter().flatten().flatten() {
        let saved = std::fs::read(entry.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Saved>(&b).ok()); // silent-ok: a bad file is skipped
        if let Some(draft) = saved.map(from_saved) {
            store.drafts.insert(draft.0, draft.1);
        }
    }
    cx.set_global(store);
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
    let content =
        saved
            .tokens
            .into_iter()
            .fold(InputContent::new(saved.text.clone()), |content, t| {
                let token = InlineToken::new(t.id, t.text).with_label(t.label);
                content
                    .clone()
                    .with_token(t.start..t.end, token)
                    .unwrap_or(content)
            });
    (
        saved.key,
        Draft {
            content,
            files,
            profile: saved.profile,
        },
    )
}

fn to_saved(key: &str, draft: &Draft) -> Saved {
    let content = &draft.content;
    Saved {
        key: key.to_string(),
        text: content.text().to_string(),
        tokens: content
            .tokens()
            .iter()
            .map(|span| SavedToken {
                start: span.range().start,
                end: span.range().end,
                id: span.token().id().to_string(),
                text: span.token().text().to_string(),
                label: span.token().label().to_string(),
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

pub fn has(cx: &App, key: &str) -> bool {
    cx.try_global::<Store>()
        .is_some_and(|s| s.drafts.contains_key(key))
}

/// The new-thread draft's sidebar card: its first line and profile.
pub fn preview(cx: &App, key: &str) -> Option<(String, Option<String>)> {
    let draft = cx.try_global::<Store>()?.drafts.get(key)?;
    let line = draft
        .content
        .text()
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
pub fn restore(cx: &App, key: &str) -> Option<Draft> {
    let draft = cx.try_global::<Store>()?.drafts.get(key)?.clone();
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
    let old = &draft.content;
    let mut text = String::new();
    let mut spans = Vec::new();
    let mut last = 0;
    for span in old.tokens() {
        let range = span.range();
        text.push_str(&old.text()[last..range.start]);
        last = range.end;
        if let Some(id) = keys.get(span.token().id().as_ref()) {
            let start = text.len();
            text.push_str(span.token().text());
            let token = InlineToken::new(id.clone(), span.token().text().clone())
                .with_label(span.token().label().clone());
            spans.push((start..text.len(), token));
        }
    }
    text.push_str(&old.text()[last..]);
    let content = spans
        .into_iter()
        .fold(InputContent::new(text), |content, (range, token)| {
            content.clone().with_token(range, token).unwrap_or(content)
        });
    Some(Draft {
        content,
        files,
        profile: draft.profile,
    })
}

/// Note the composer's current state; an empty box deletes the draft.
pub fn record(key: &str, draft: Draft, cx: &mut App) {
    let store = cx.default_global::<Store>();
    let had = store.drafts.contains_key(key);
    if draft.is_empty() {
        if !had {
            return;
        }
        store.drafts.remove(key);
    } else {
        let same = store
            .drafts
            .get(key)
            .is_some_and(|d| d.content == draft.content && d.profile == draft.profile);
        if same {
            return;
        }
        store.drafts.insert(key.to_string(), draft);
    }
    store.dirty.insert(key.to_string());
    store.generation += 1;
    let generation = store.generation;
    // The sidebar's Draft mark appears or goes.
    if had != store.drafts.contains_key(key) {
        cx.refresh_windows();
    }
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_DELAY).await;
        cx.update(|cx| {
            if cx.global::<Store>().generation == generation {
                save(cx, false);
            }
        });
    })
    .detach();
}

pub fn remove(key: &str, cx: &mut App) {
    record(
        key,
        Draft {
            content: InputContent::new(""),
            files: Vec::new(),
            profile: None,
        },
        cx,
    );
}

/// Write the changed drafts: off the UI thread, or right here on quit.
pub fn save(cx: &mut App, now: bool) {
    let Some(store) = cx.try_global::<Store>() else {
        return;
    };
    if store.dirty.is_empty() {
        return;
    }
    let jobs: Vec<(String, Option<Draft>)> = store
        .dirty
        .iter()
        .map(|key| (key.clone(), store.drafts.get(key).cloned()))
        .collect();
    cx.global_mut::<Store>().dirty.clear();
    let write = move || {
        let Some(dir) = dir() else { return };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("could not create the drafts folder: {e}");
            return;
        }
        for (key, draft) in jobs {
            let file = file_for(&dir, &key);
            let result = match draft {
                Some(draft) => serde_json::to_vec(&to_saved(&key, &draft))
                    .map_err(std::io::Error::other)
                    .and_then(|json| std::fs::write(&file, json)),
                None => match std::fs::remove_file(&file) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    other => other,
                },
            };
            if let Err(e) = result {
                tracing::warn!("could not save a draft: {e}");
            }
        }
    };
    if now {
        write();
    } else {
        cx.background_spawn(async move { write() }).detach();
    }
}
