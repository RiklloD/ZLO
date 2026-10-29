//! Outputs: the files this conversation produced (its own workspace on the
//! gateway), browsable by folder and previewed in place. MonoCode's
//! Documents tab, fed by `GET /threads/{id}/files`.

use std::path::PathBuf;

use ironclaw_gpui_client::workspace_views::ProjectFsEntryKind;

use super::preview::{self, Opened};
use super::{ChatView, empty, error_line, loading, scroll};
use crate::design::{Palette, kit};
use crate::jobs::flatten;
use crate::ui::*;

#[derive(Default)]
pub(super) struct State {
    opened: Option<Opened>,
}

impl ChatView {
    /// Download one of the conversation's files to the temp folder and
    /// preview it in the pane.
    pub(super) fn outputs_open(&mut self, path: String, cx: &mut Context<Self>) {
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let dir = std::env::temp_dir()
            .join("numoh")
            .join(&self.thread_id)
            .join("files");
        let local = dir.join(&name);
        self.side.outputs.opened = Some(Opened::loading(name, local.clone()));
        cx.notify();
        let thread_id = self.thread_id.clone();
        let job = self.backend.spawn(move |c| async move {
            let bytes = c.thread_file_bytes(&thread_id, &path).await?;
            let saved = async {
                tokio::fs::create_dir_all(&dir).await?;
                tokio::fs::write(&local, bytes).await
            };
            saved.await.map_err(|e| {
                ironclaw_gpui_client::ClientError::Decode(format!("Couldn't save the file: {e}"))
            })?;
            Ok::<PathBuf, ironclaw_gpui_client::ClientError>(local)
        });
        cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let read = match result {
                Ok(local) => {
                    let read = cx
                        .background_spawn({
                            let local = local.clone();
                            async move { preview::read(&local) }
                        })
                        .await;
                    // A copy already on screen was just rewritten.
                    if matches!(read, preview::Preview::Native) {
                        crate::native_view::refresh(&crate::native_view::Source::File(local));
                    }
                    read
                }
                Err(e) => preview::Preview::Failed(e.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(opened) = this.side.outputs.opened.as_mut() {
                    opened.preview = read;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Preview a file on this Mac in the pane, as Files does.
    pub(super) fn outputs_open_local(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        self.side.outputs.opened = Some(Opened::loading(name, path.clone()));
        cx.notify();
        let task = cx.background_spawn(async move { preview::read(&path) });
        cx.spawn(async move |this, cx| {
            let read = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(opened) = this.side.outputs.opened.as_mut() {
                    opened.preview = read;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_outputs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(opened) = self.side.outputs.opened.as_ref() {
            let weak = cx.weak_entity();
            return v_flex()
                .flex_1()
                .min_h_0()
                .child(preview::toolbar(
                    opened,
                    "outputs",
                    move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.side.outputs.opened = None;
                            cx.notify();
                        });
                    },
                    cx,
                ))
                .child(preview::body(opened, "outputs-preview", cx))
                .into_any_element();
        }
        let p = *Palette::of(cx);
        let Some(panel) = self.files.as_ref() else {
            return loading();
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
            (_, Some(e)) => error_line(e.clone(), cx),
            (None, None) => loading(),
            (Some(entries), None) if entries.is_empty() => empty(
                "Nothing yet. Files IronClaw writes in this conversation show up here.",
                cx,
            ),
            (Some(entries), None) => v_flex()
                .px_2()
                .children(entries.iter().enumerate().map(|(ix, entry)| {
                    let dir = matches!(entry.kind, ProjectFsEntryKind::Directory);
                    let path = entry.path.clone();
                    let ext = std::path::Path::new(&entry.name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let icon = if dir {
                        Icon::new(IconName::Folder)
                    } else if preview::is_image(&ext) {
                        Icon::new(crate::assets::ExtraIcon::Image)
                    } else {
                        Icon::new(IconName::File)
                    };
                    kit::row(format!("output-{ix}"), false, cx)
                        .aria_label(entry.name.clone())
                        .child(icon.xsmall().flex_none().text_color(p.text_muted))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(entry.name.clone()),
                        )
                        .when(dir, |d| {
                            d.child(
                                Icon::new(IconName::ChevronRight)
                                    .xsmall()
                                    .text_color(p.text_faint),
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if dir {
                                this.browse(Some(path.clone()), cx)
                            } else {
                                this.outputs_open(path.clone(), cx)
                            }
                        }))
                }))
                .into_any_element(),
        };
        let current = panel.path.clone();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .px_2()
                    .gap_1()
                    .items_center()
                    .when_some(parent, |d, parent| {
                        d.child(
                            Button::new("outputs-up")
                                .icon(IconName::ChevronLeft)
                                .ghost()
                                .xsmall()
                                .tooltip("Up")
                                .accessibility_label("Up one folder")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.browse(parent.clone(), cx)
                                })),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .px_1()
                            .text_xs()
                            .text_color(p.text_muted)
                            .child(folder),
                    )
                    .child(
                        Button::new("outputs-refresh")
                            .icon(IconName::RefreshCw)
                            .ghost()
                            .xsmall()
                            .tooltip("Refresh")
                            .accessibility_label("Refresh")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.browse(current.clone(), cx)),
                            ),
                    ),
            )
            .child(scroll("outputs-list").child(body))
            .into_any_element()
    }
}
