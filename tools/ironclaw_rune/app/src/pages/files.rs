//! Read-only browser over the gateway's mounts (memory, workspace, skills).

use rune::{Cursor, Cx, El, ScrollSpec, col, el, row, text};
use rune_kit::{Kit, Markdown, Spinner};
use zlo_client::FilePreview;
use zlo_client::workspace_views::{FsMount, ProjectFsEntry, ProjectFsEntryKind, RebornFsMountInfo};

use super::common::{
    SM, XL, XS, choice, empty_state, error_line, font, glyph, label, refresh_button,
};
use crate::design::smooth_scroll::WheelGlide;
use crate::icons;
use crate::jobs;
use crate::state::State;

const LIST_WIDTH: f32 = 300.;
/// Rendering a multi-megabyte string in one text element stalls layout.
const PREVIEW_RENDER_CHARS: usize = 200_000;

pub struct FilesState {
    pub mounts: Vec<RebornFsMountInfo>,
    pub mount: FsMount,
    pub path: String,
    pub entries: Vec<ProjectFsEntry>,
    pub selected: Option<String>,
    pub preview: Option<Result<FilePreview, String>>,
    pub error: Option<String>,
    pub loaded: bool,
    /// The mounts were asked for (once).
    started: bool,
}

impl Default for FilesState {
    fn default() -> Self {
        Self {
            mounts: Vec::new(),
            mount: FsMount::Memory,
            path: String::new(),
            entries: Vec::new(),
            selected: None,
            preview: None,
            error: None,
            loaded: false,
            started: false,
        }
    }
}

/// The page became visible: load the mounts on first visit, refresh the
/// listing after.
pub fn opened(s: &mut State) {
    if s.pages.files.started {
        refresh(s);
        return;
    }
    s.pages.files.started = true;
    jobs::run(
        s,
        "files",
        |c| async move { c.fs_mounts().await },
        |s, result, _| match result {
            Ok(resp) => {
                let this = &mut s.pages.files;
                if let Some(first) = resp.mounts.first() {
                    this.mount = first.mount;
                }
                this.mounts = resp.mounts;
                refresh(s);
            }
            Err(e) => {
                let this = &mut s.pages.files;
                this.error = Some(e.to_string());
                this.loaded = true;
                // Ask again on the next visit.
                this.started = false;
            }
        },
    );
}

pub fn refresh(s: &mut State) {
    let (mount, path) = (s.pages.files.mount, s.pages.files.path.clone());
    let asked = path.clone();
    jobs::run(
        s,
        "files",
        move |c| async move { c.fs_list(mount, &path).await },
        move |s, result, _| {
            // A slow answer for a folder the person already left is dropped.
            if s.pages.files.mount != mount || s.pages.files.path != asked {
                return;
            }
            match result {
                Ok(mut listing) => {
                    listing.entries.sort_by(|a, b| {
                        is_dir(b)
                            .cmp(&is_dir(a))
                            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                    });
                    let threads = listing
                        .entries
                        .iter()
                        .filter_map(|e| is_thread_folder(mount, &e.path))
                        .map(str::to_string)
                        .collect::<Vec<_>>();
                    crate::workspace::titles_ensure(s, threads);
                    let this = &mut s.pages.files;
                    this.entries = listing.entries;
                    this.error = None;
                }
                Err(e) => {
                    let this = &mut s.pages.files;
                    this.entries.clear();
                    this.error = Some(e.to_string());
                }
            }
            s.pages.files.loaded = true;
        },
    );
}

fn navigate(s: &mut State, mount: FsMount, path: String) {
    let this = &mut s.pages.files;
    this.mount = mount;
    this.path = path;
    this.selected = None;
    this.preview = None;
    refresh(s);
}

fn select(s: &mut State, entry: &ProjectFsEntry) {
    let mount = s.pages.files.mount;
    if is_dir(entry) {
        navigate(s, mount, entry.path.clone());
        return;
    }
    let path = entry.path.clone();
    let requested = path.clone();
    let this = &mut s.pages.files;
    this.selected = Some(path.clone());
    this.preview = None;
    jobs::run(
        s,
        "files",
        move |c| async move { c.fs_preview(mount, &path).await },
        move |s, result, _| {
            // Ignore a slow answer for a file the person already left.
            let this = &mut s.pages.files;
            if this.selected.as_deref() == Some(requested.as_str()) {
                this.preview = Some(result.map_err(|e| e.to_string()));
            }
        },
    );
}

fn render_crumbs(s: &State) -> El {
    let p = &s.palette;
    let this = &s.pages.files;
    let mount_label = this
        .mounts
        .iter()
        .find(|m| m.mount == this.mount)
        .map(|m| m.label.clone())
        .unwrap_or_else(|| this.mount.label().to_string());
    let mut crumbs = vec![(mount_label, String::new())];
    let mut acc = String::new();
    for part in this.path.split('/').filter(|p| !p.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        crumbs.push((display_name(s, this.mount, &acc, part), acc.clone()));
    }
    let last = crumbs.len() - 1;
    let mount = this.mount;
    row()
        .key("crumbs")
        .flex_wrap()
        .gap(2.)
        .center_y()
        .children(crumbs.into_iter().enumerate().map(|(ix, (name, path))| {
            let mut crumb = row()
                .key(format!("crumb-{ix}"))
                .role("button")
                .label(name.clone())
                .px(4.)
                .radius(crate::design::CONTROL_RADIUS);
            crumb = if ix == last {
                crumb.child(label(name, XS, 600., p.text))
            } else {
                crumb
                    .cursor(Cursor::Pointer)
                    .hover(p.hover)
                    .on_press(move |s: &mut State| navigate(s, mount, path.clone()))
                    .child(label(name, XS, 400., p.text_muted))
            };
            let mut d = row().gap(2.).center_y();
            if ix > 0 {
                d = d.child(glyph(&icons::CHEVRON_RIGHT, 12., p.text_muted));
            }
            d.child(crumb)
        }))
}

fn render_list(s: &State) -> El {
    let p = &s.palette;
    let this = &s.pages.files;
    let rows = this.entries.iter().map(|entry| {
        let dir = is_dir(entry);
        let selected = this.selected.as_deref() == Some(entry.path.as_str());
        let name = display_name(s, this.mount, &entry.path, &entry.name);
        let pick = entry.clone();
        let mut r = row()
            .key(format!("entry-{}", entry.path))
            .role("button")
            .label(name.clone())
            .gap(8.)
            .px(8.)
            .py(4.)
            .radius(crate::design::CONTROL_RADIUS)
            .center_y()
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| select(s, &pick))
            .child(glyph(
                if dir { &icons::FOLDER } else { &icons::FILE },
                16.,
                p.text_muted,
            ))
            .child(label(name, SM, 400., p.text).truncate().grow(1.0).min_w(0.));
        r = if selected {
            r.fill(p.active)
        } else {
            r.hover(p.hover)
        };
        r
    });
    let mut list = col().w_full().padding(8.).gap(2.);
    if this.loaded && this.entries.is_empty() && this.error.is_none() {
        list = list.child(empty_state(p, "This folder is empty."));
    }
    col()
        .key("file-list")
        .w(LIST_WIDTH)
        .h_full()
        .min_h(0.)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .wheel_glide("file-list")
        .child(list.children(rows))
}

fn render_preview(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let this = &s.pages.files;
    let Some(path) = this.selected.as_ref() else {
        return col()
            .grow(1.0)
            .min_w(0.)
            .child(empty_state(p, "Select a file to preview it."));
    };
    let body: El = match &this.preview {
        None => row()
            .gap(8.)
            .center_y()
            .child(Spinner::new().build(k))
            .child(label("Loading", SM, 400., p.text_muted)),
        Some(Err(e)) => error_line(p, e),
        Some(Ok(FilePreview::TooLarge { size_bytes })) => empty_state(
            p,
            format!(
                "{} is too large to preview here.",
                zlo_client::attachments::format_bytes(*size_bytes)
            ),
        ),
        Some(Ok(FilePreview::Binary {
            mime_type,
            size_bytes,
        })) => empty_state(
            p,
            format!(
                "Binary file ({mime_type}, {}). No preview.",
                zlo_client::attachments::format_bytes(*size_bytes)
            ),
        ),
        Some(Ok(FilePreview::Text {
            text: body,
            markdown,
        })) => {
            let (shown, cut) = clip(body);
            let content = if *markdown {
                Markdown::new(shown).selectable(true).build(k)
            } else {
                text(shown)
                    .font(font(XS, 400.).mono())
                    .color(p.text)
                    .wrap_anywhere()
                    .selectable(true)
            };
            let mut d = col().w_full().gap(8.).child(content);
            if cut {
                d = d.child(label("Preview truncated.", XS, 400., p.text_muted));
            }
            d
        }
    };
    col()
        .key(format!("file-preview-{path}"))
        .grow(1.0)
        .min_w(0.)
        .h_full()
        .min_h(0.)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .wheel_glide(format!("file-preview-{path}"))
        .child(
            col()
                .w_full()
                .padding(16.)
                .gap(12.)
                .child(label(path.clone(), SM, 600., p.text).truncate())
                .child(body),
        )
}

/// Memory keeps one folder per thread under `threads/`, named by thread id.
fn is_thread_folder(mount: FsMount, path: &str) -> Option<&str> {
    let id = path.strip_prefix("threads/")?;
    (mount == FsMount::Memory && !id.is_empty() && !id.contains('/')).then_some(id)
}

/// A thread folder shows its conversation title; anything else its name.
fn display_name(s: &State, mount: FsMount, path: &str, name: &str) -> String {
    is_thread_folder(mount, path)
        .and_then(|id| crate::workspace::title_of(s, id))
        .unwrap_or_else(|| name.to_string())
}

fn is_dir(entry: &ProjectFsEntry) -> bool {
    matches!(entry.kind, ProjectFsEntryKind::Directory)
}

/// Cut on a char boundary so multi-byte text never panics.
fn clip(text: &str) -> (String, bool) {
    match text.char_indices().nth(PREVIEW_RENDER_CHARS) {
        Some((cut, _)) => (text[..cut].to_string(), true),
        None => (text.to_string(), false),
    }
}

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let this = &s.pages.files;
    let tabs = row().gap(4.).children(this.mounts.iter().map(|m| {
        let mount = m.mount;
        choice(&m.label, mount == this.mount)
            .on_press(move |s: &mut State| navigate(s, mount, String::new()))
            .build(k)
    }));
    let mut top = col()
        .key("files-top")
        .w_full()
        .gap(8.)
        .px(16.)
        .pt(16.)
        .pb(12.)
        .child(
            row()
                .w_full()
                .justify_between()
                .center_y()
                .child(label("Files", XL, 600., p.text))
                .child(
                    row()
                        .gap(8.)
                        .center_y()
                        .child(tabs)
                        .child(refresh_button(k, refresh)),
                ),
        )
        .child(render_crumbs(s));
    if let Some(e) = &this.error {
        top = top.child(error_line(p, e));
    }
    col()
        .key("files-page")
        .w_full()
        .h_full()
        .min_h(0.)
        .child(top)
        // Rune has no one-sided borders: hairlines are 1 px elements.
        .child(el().w_full().h(1.).fill(p.border))
        .child(
            row()
                .grow(1.0)
                .min_h(0.)
                .w_full()
                .child(render_list(s))
                .child(el().w(1.).h_full().fill(p.border))
                .child(render_preview(s, k)),
        )
}

pub fn fixture(s: &mut State) {
    let mounts = serde_json::from_value::<Vec<RebornFsMountInfo>>(serde_json::json!([
        {"mount": "memory", "label": "Memory"},
        {"mount": "workspace", "label": "Workspace files"},
        {"mount": "skills", "label": "Skills"}
    ]));
    let entries = serde_json::from_value::<Vec<ProjectFsEntry>>(serde_json::json!([
        {"name": "t1", "path": "threads/t1", "kind": "directory"},
        {"name": "t9", "path": "threads/t9", "kind": "directory"},
        {"name": "MEMORY.md", "path": "threads/MEMORY.md", "kind": "file"},
        {"name": "daily-2026-11-06.md", "path": "threads/daily-2026-11-06.md", "kind": "file"}
    ]));
    let this = &mut s.pages.files;
    match (mounts, entries) {
        (Ok(m), Ok(e)) => {
            this.mounts = m;
            this.entries = e;
        }
        (Err(e), _) | (_, Err(e)) => tracing::warn!(error = %e, "files fixture"),
    }
    this.started = true;
    this.loaded = true;
    this.path = "threads".into();
    this.selected = Some("threads/MEMORY.md".into());
    this.preview = Some(Ok(FilePreview::Text {
        text: "# Memory\n\nThe user ports **ZLO** from GPUI to Rune.\n\n- Prefers short answers\n- Works in Zurich time\n\n```rust\nfn main() {}\n```\n".into(),
        markdown: true,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_cuts_on_char_boundaries_and_thread_folders_resolve() {
        let long = "é".repeat(PREVIEW_RENDER_CHARS + 3);
        let (shown, cut) = clip(&long);
        assert!(cut);
        assert_eq!(shown.chars().count(), PREVIEW_RENDER_CHARS);
        assert_eq!(is_thread_folder(FsMount::Memory, "threads/t1"), Some("t1"));
        assert_eq!(is_thread_folder(FsMount::Memory, "threads/t1/x"), None);
        assert_eq!(is_thread_folder(FsMount::Workspace, "threads/t1"), None);
    }
}
