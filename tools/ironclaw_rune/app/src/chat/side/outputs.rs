//! Outputs: the files this conversation produced (its own workspace on the
//! gateway), browsable by folder and previewed in place (GPUI
//! `chat/side/outputs.rs`). MonoCode's Documents tab, fed by
//! `GET /threads/{id}/files`.
//!
//! The folder listing (GPUI `ChatView::browse` and `FilesPanel` in
//! `chat.rs`) lives here now: Outputs was its only reader.

use std::path::PathBuf;

use rune::{El, col, icon, row};
use rune_kit::Kit;
use zlo_client::ClientError;
use zlo_client::workspace_views::{ProjectFsEntry, ProjectFsEntryKind};

use super::preview::{self, Opened};
use super::ui::{self, sm, xs};
use super::{side, side_mut, side_of, thread_id};
use crate::icons;
use crate::jobs::{self, Generation};
use crate::state::State;

/// One folder of the conversation's own workspace.
pub(super) struct FilesPanel {
    /// `None` is the conversation's root folder.
    pub(super) path: Option<String>,
    entries: Option<Vec<ProjectFsEntry>>,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct OutputsState {
    pub(super) files: Option<FilesPanel>,
    pub(super) opened: Option<Opened>,
    /// Bumped per listing and per opened file; late answers are dropped.
    list_gen: Generation,
    open_gen: Generation,
}

/// List `path` (the root when `None`) of the conversation's workspace.
pub(super) fn browse(s: &mut State, path: Option<String>) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    x.outputs.files = Some(FilesPanel {
        path: path.clone(),
        entries: None,
        error: None,
    });
    let generation = x.outputs.list_gen.bump();
    let asked = thread.clone();
    jobs::run(
        s,
        "outputs",
        move |c| async move { c.thread_files(&asked, path.as_deref()).await },
        move |s, result, _| {
            let Some(x) = side_of(s, &thread).filter(|x| x.outputs.list_gen == generation) else {
                return;
            };
            // Ignore answers for a panel that was closed meanwhile.
            let Some(panel) = x.outputs.files.as_mut() else {
                return;
            };
            match result {
                Ok(list) => {
                    // Dot-files are the gateway's own bookkeeping.
                    let mut entries: Vec<_> = list
                        .entries
                        .into_iter()
                        .filter(|e| !e.name.starts_with('.'))
                        .collect();
                    entries.sort_by_key(|e| {
                        (
                            !matches!(e.kind, ProjectFsEntryKind::Directory),
                            e.name.clone(),
                        )
                    });
                    panel.entries = Some(entries);
                }
                Err(e) => panel.error = Some(e.to_string()),
            }
        },
    );
}

/// Download one of the conversation's files to the temp folder and preview
/// it in the pane.
pub(super) fn open(s: &mut State, path: String) {
    let Some(thread) = thread_id(s) else { return };
    // Thread id and path come from the gateway: one folder per full remote
    // path (a hash, never a joined path), so two `report.md`s in different
    // folders never share a local copy.
    let file = crate::chat::downloads::file_name(&path);
    let name = file.to_string_lossy().into_owned();
    let dir = crate::chat::downloads::dir_for("output", &[&thread, &path]);
    let local = dir.join(&file);
    let Some(x) = side_mut(s) else { return };
    x.outputs.opened = Some(Opened::loading(name, local));
    let generation = x.outputs.open_gen.bump();
    let asked = thread.clone();
    jobs::run(
        s,
        "outputs",
        move |c| async move {
            let bytes = c.thread_file_bytes(&asked, &path).await?;
            crate::chat::downloads::save_async(dir, file, bytes)
                .await
                .map_err(|e| ClientError::Decode(format!("Couldn't save the file: {e}")))
        },
        move |s, result, _| match result {
            Ok(local) => read_into(s, thread, generation, local, true),
            Err(e) => land(
                s,
                &thread,
                generation,
                preview::Preview::Failed(e.to_string()),
            ),
        },
    );
}

/// Preview a file on this Mac in the pane, as Files does.
pub(super) fn open_local(s: &mut State, path: PathBuf) {
    let Some(thread) = thread_id(s) else { return };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let Some(x) = side_mut(s) else { return };
    x.outputs.opened = Some(Opened::loading(name, path.clone()));
    let generation = x.outputs.open_gen.bump();
    read_into(s, thread, generation, path, false);
}

fn read_into(s: &State, thread: String, generation: Generation, local: PathBuf, fetched: bool) {
    jobs::blocking(
        s,
        move || {
            let read = preview::read(&local);
            (read, local)
        },
        move |s, (read, local), _| {
            // A copy already on screen was just rewritten.
            if fetched && matches!(read, preview::Preview::Native) {
                crate::native_view::refresh(&crate::native_view::Source::File(local));
            }
            land(s, &thread, generation, read);
        },
    );
}

fn land(s: &mut State, thread: &str, generation: Generation, read: preview::Preview) {
    if let Some(x) = side_of(s, thread).filter(|x| x.outputs.open_gen == generation)
        && let Some(opened) = x.outputs.opened.as_mut()
    {
        opened.preview = read;
    }
}

fn close_preview(s: &mut State) {
    if let Some(x) = side_mut(s) {
        x.outputs.opened = None;
    }
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let Some(x) = side(s) else {
        return ui::loading(k);
    };
    if let Some(opened) = x.outputs.opened.as_ref() {
        return ui::fill_col()
            .child(preview::toolbar(s, k, opened, "outputs", close_preview))
            .child(preview::body(s, k, opened, "outputs-preview"));
    }
    let p = &s.palette;
    let Some(panel) = x.outputs.files.as_ref() else {
        return ui::loading(k);
    };
    // Back to the root once only the top folder is left.
    let parent = panel.path.as_deref().map(|p| {
        p.trim_end_matches('/')
            .rsplit_once('/')
            .map(|(head, _)| head.to_string())
            .filter(|head| head.matches('/').count() >= 2)
    });
    let folder = panel
        .path
        .as_deref()
        .and_then(|p| p.trim_end_matches('/').rsplit('/').next())
        .unwrap_or("All files")
        .to_string();
    let body = match (&panel.entries, &panel.error) {
        (_, Some(e)) => ui::error_line(s, e.clone()),
        (None, None) => ui::loading(k),
        (Some(entries), None) if entries.is_empty() => ui::empty(
            s,
            "Nothing yet. Files IronClaw writes in this conversation show up here.",
        ),
        (Some(entries), None) => {
            let mut list = col().w_full().px(8.);
            for (ix, entry) in entries.iter().enumerate() {
                let dir = matches!(entry.kind, ProjectFsEntryKind::Directory);
                let path = entry.path.clone();
                let ext = std::path::Path::new(&entry.name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let glyph = if dir {
                    &icons::FOLDER
                } else if preview::is_image(&ext) {
                    &icons::IMAGE
                } else {
                    &icons::FILE
                };
                let mut line = ui::hover_row(s, format!("output-{ix}"))
                    .label(entry.name.clone())
                    .child(icon(glyph, 12.).color(p.text_muted))
                    .child(
                        ui::line(entry.name.clone(), sm(), p.text)
                            .grow(1.)
                            .min_w(0.),
                    );
                if dir {
                    line = line.child(icon(&icons::CHEVRON_RIGHT, 12.).color(p.text_faint));
                }
                list = list.child(line.on_press(move |s: &mut State| {
                    if dir {
                        browse(s, Some(path.clone()))
                    } else {
                        open(s, path.clone())
                    }
                }));
            }
            list
        }
    };
    let current = panel.path.clone();
    let mut bar = row().w_full().px(8.).gap(4.).items_center();
    if let Some(parent) = parent {
        bar = bar.child(
            ui::ibtn(&icons::CHEVRON_LEFT, "Up")
                .label("Up one folder")
                .on_press(move |s: &mut State| browse(s, parent.clone()))
                .build(k)
                .key("outputs-up"),
        );
    }
    bar = bar
        .child(
            ui::line(folder, xs(), p.text_muted)
                .grow(1.)
                .min_w(0.)
                .px(4.),
        )
        .child(
            ui::ibtn(&icons::REFRESH_CW, "Refresh")
                .on_press(move |s: &mut State| browse(s, current.clone()))
                .build(k)
                .key("outputs-refresh"),
        );
    ui::fill_col()
        .child(bar)
        .child(ui::scroll("outputs-list", body))
}

/// A fixture listing for scenes.
pub(super) fn fixture(x: &mut super::SideState, entries: Vec<ProjectFsEntry>) {
    x.outputs.files = Some(FilesPanel {
        path: None,
        entries: Some(entries),
        error: None,
    });
}
