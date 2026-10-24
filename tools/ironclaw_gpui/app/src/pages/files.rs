//! Read-only browser over the gateway's mounts (memory, workspace, skills).

use ironclaw_gpui_client::workspace_views::{
    FsMount, ProjectFsEntry, ProjectFsEntryKind, RebornFsMountInfo,
};
use ironclaw_gpui_client::{Backend, FilePreview};

use super::{PageEvent, empty_state, error_line, refresh_button};
use crate::jobs;
use crate::ui::*;

const LIST_WIDTH: f32 = 300.;
/// Rendering a multi-megabyte string in one text element stalls layout.
const PREVIEW_RENDER_CHARS: usize = 200_000;

pub struct FilesView {
    backend: Backend,
    mounts: Vec<RebornFsMountInfo>,
    mount: FsMount,
    path: String,
    entries: Vec<ProjectFsEntry>,
    selected: Option<String>,
    preview: Option<Result<FilePreview, SharedString>>,
    error: Option<SharedString>,
    loaded: bool,
}

impl EventEmitter<PageEvent> for FilesView {}

impl FilesView {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let this = Self {
            backend,
            mounts: Vec::new(),
            mount: FsMount::Memory,
            path: String::new(),
            entries: Vec::new(),
            selected: None,
            preview: None,
            error: None,
            loaded: false,
        };
        jobs::run(
            &this.backend,
            cx,
            |c| async move { c.fs_mounts().await },
            |this, result, cx| match result {
                Ok(resp) => {
                    if let Some(first) = resp.mounts.first() {
                        this.mount = first.mount;
                    }
                    this.mounts = resp.mounts;
                    this.refresh(cx);
                }
                Err(e) => {
                    this.error = Some(e.to_string().into());
                    this.loaded = true;
                    cx.notify();
                }
            },
        );
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let (mount, path) = (self.mount, self.path.clone());
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.fs_list(mount, &path).await },
            |this, result, cx| {
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
                            .filter_map(|e| is_thread_folder(this.mount, &e.path))
                            .map(str::to_string)
                            .collect::<Vec<_>>();
                        crate::titles::ensure(threads, &this.backend, cx);
                        this.entries = listing.entries;
                        this.error = None;
                    }
                    Err(e) => {
                        this.entries.clear();
                        this.error = Some(e.to_string().into());
                    }
                }
                this.loaded = true;
                cx.notify();
            },
        );
    }

    fn navigate(&mut self, mount: FsMount, path: String, cx: &mut Context<Self>) {
        self.mount = mount;
        self.path = path;
        self.selected = None;
        self.preview = None;
        self.refresh(cx);
    }

    fn select(&mut self, entry: &ProjectFsEntry, cx: &mut Context<Self>) {
        if is_dir(entry) {
            self.navigate(self.mount, entry.path.clone(), cx);
            return;
        }
        let (mount, path) = (self.mount, entry.path.clone());
        let requested = path.clone();
        self.selected = Some(path.clone());
        self.preview = None;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.fs_preview(mount, &path).await },
            move |this, result, cx| {
                // Ignore a slow answer for a file the person already left.
                if this.selected.as_deref() == Some(requested.as_str()) {
                    this.preview = Some(result.map_err(|e| e.to_string().into()));
                    cx.notify();
                }
            },
        );
    }

    fn render_crumbs(&self, cx: &Context<Self>) -> impl IntoElement {
        let label = self
            .mounts
            .iter()
            .find(|m| m.mount == self.mount)
            .map(|m| m.label.clone())
            .unwrap_or_else(|| self.mount.label().to_string());
        let mut crumbs = vec![(label, String::new())];
        let mut acc = String::new();
        for part in self.path.split('/').filter(|p| !p.is_empty()) {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            crumbs.push((display_name(self.mount, &acc, part, cx), acc.clone()));
        }
        let last = crumbs.len() - 1;
        let mount = self.mount;
        h_flex()
            .flex_wrap()
            .gap_0p5()
            .text_xs()
            .children(crumbs.into_iter().enumerate().map(|(ix, (name, path))| {
                h_flex()
                    .gap_0p5()
                    .when(ix > 0, |this| {
                        this.child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(Icon::new(IconName::ChevronRight).xsmall()),
                        )
                    })
                    .child(
                        div()
                            .id(("crumb", ix))
                            .role(Role::Button)
                            .aria_label(name.clone())
                            .px_1()
                            .rounded(cx.theme().radius)
                            .when(ix == last, |d| d.font_semibold())
                            .when(ix != last, |d| {
                                d.cursor_pointer()
                                    .text_color(cx.theme().muted_foreground)
                                    .map(|d| {
                                        crate::design::kit::smooth_bg(d, format!("crumb-{ix}"), cx)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.navigate(mount, path.clone(), cx)
                                    }))
                            })
                            .child(name),
                    )
            }))
    }

    fn render_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = self.entries.iter().enumerate().map(|(ix, entry)| {
            let dir = is_dir(entry);
            let selected = self.selected.as_deref() == Some(entry.path.as_str());
            let name = display_name(self.mount, &entry.path, &entry.name, cx);
            let entry = entry.clone();
            h_flex()
                .id(("entry", ix))
                .role(Role::Button)
                .aria_label(name.clone())
                .gap_2()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .text_sm()
                .cursor_pointer()
                .when(selected, |d| d.bg(theme.accent))
                .when(!selected, |d| {
                    crate::design::kit::smooth_bg(d, format!("entry-{ix}"), cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| this.select(&entry, cx)))
                .child(
                    div().text_color(theme.muted_foreground).child(
                        Icon::new(if dir {
                            IconName::Folder
                        } else {
                            IconName::File
                        })
                        .small(),
                    ),
                )
                .child(div().flex_1().min_w_0().truncate().child(name))
        });
        v_flex()
            .id("file-list")
            .w(px(LIST_WIDTH))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(theme.border)
            .overflow_y_scroll()
            .p_2()
            .gap_0p5()
            .when(
                self.loaded && self.entries.is_empty() && self.error.is_none(),
                |d| d.child(empty_state("This folder is empty.", cx)),
            )
            .children(rows)
    }

    fn render_preview(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(path) = self.selected.as_ref() else {
            return empty_state("Select a file to preview it.", cx).into_any_element();
        };
        let body: AnyElement = match &self.preview {
            None => h_flex()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Spinner::new())
                .child("Loading")
                .into_any_element(),
            Some(Err(e)) => error_line(e, cx).into_any_element(),
            Some(Ok(FilePreview::TooLarge { size_bytes })) => empty_state(
                format!("{} is too large to preview here.", jobs::size(*size_bytes)),
                cx,
            )
            .into_any_element(),
            Some(Ok(FilePreview::Binary {
                mime_type,
                size_bytes,
            })) => empty_state(
                format!(
                    "Binary file ({mime_type}, {}). No preview.",
                    jobs::size(*size_bytes)
                ),
                cx,
            )
            .into_any_element(),
            Some(Ok(FilePreview::Text { text, markdown })) => {
                let (shown, cut) = clip(text);
                let content: AnyElement = if *markdown {
                    crate::ui::markdown(SharedString::from(format!("preview:{path}")), shown, false)
                        .into_any_element()
                } else {
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .whitespace_normal()
                        .child(shown)
                        .into_any_element()
                };
                v_flex()
                    .gap_2()
                    .child(content)
                    .when(cut, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Preview truncated."),
                        )
                    })
                    .into_any_element()
            }
        };
        v_flex()
            .id("file-preview")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .p_4()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .truncate()
                    .child(path.clone()),
            )
            .child(body)
            .into_any_element()
    }
}

/// Memory keeps one folder per thread under `threads/`, named by thread id.
fn is_thread_folder(mount: FsMount, path: &str) -> Option<&str> {
    let id = path.strip_prefix("threads/")?;
    (mount == FsMount::Memory && !id.is_empty() && !id.contains('/')).then_some(id)
}

/// A thread folder shows its conversation title; anything else its name.
fn display_name(mount: FsMount, path: &str, name: &str, cx: &App) -> String {
    is_thread_folder(mount, path)
        .and_then(|id| crate::titles::get(cx, id))
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

impl Render for FilesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = h_flex()
            .gap_1()
            .children(self.mounts.iter().enumerate().map(|(ix, m)| {
                let mount = m.mount;
                let active = mount == self.mount;
                Button::new(("mount", ix))
                    .label(m.label.clone())
                    .small()
                    .when(active, |b| b.primary())
                    .when(!active, |b| b.ghost())
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.navigate(mount, String::new(), cx)),
                    )
            }));
        let top = v_flex()
            .gap_2()
            .px_4()
            .pt_4()
            .pb_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_xl().font_semibold().child("Files"))
                    .child(
                        h_flex().gap_2().child(tabs).child(
                            refresh_button("refresh-files")
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                        ),
                    ),
            )
            .child(self.render_crumbs(cx))
            .when_some(self.error.as_ref(), |this, e| this.child(error_line(e, cx)));
        v_flex().size_full().child(top).child(
            h_flex()
                .flex_1()
                .min_h_0()
                .child(self.render_list(cx))
                .child(self.render_preview(cx)),
        )
    }
}
