//! The new-thread screen (t3code's, with a profile where t3code has a
//! project): "What should we work on?", the profile underlined below it, and
//! the composer card. Nothing exists on the gateway until the first message
//! is sent, so abandoned drafts leave no empty threads behind.

mod snapshot;

use std::path::PathBuf;

use ironclaw_gpui_client::attachments::{self, StagedFile};
use ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID;

use super::{Page, Workspace};
use crate::assets::ExtraIcon;
use crate::chat::model_picker::{ModelPicker, PickerEvent};
use crate::chat::{composer_card, inline_files, round_button};
use crate::design::styles::Style;
use crate::design::{Palette, motion};
use crate::jobs;
use crate::ui::*;
use gpui_kit::Focusable as _;

pub(super) struct Draft {
    pub(super) profile: String,
    input: Entity<TextareaState>,
    picker: Entity<ModelPicker>,
    pub(super) menu_open: bool,
    /// Files for the first message, handed to the thread on send.
    staged: Vec<StagedFile>,
    sending: bool,
    error: Option<SharedString>,
    _subs: Vec<Subscription>,
}

impl Workspace {
    /// Show the new-thread screen; an unsent draft keeps its text.
    pub(super) fn open_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Draft;
        if self.draft.is_none()
            && let Some(backend) = self.backend.clone()
        {
            let list = &self.inbox.list;
            let profile = list
                .last_used_profile_id
                .clone()
                .or_else(|| list.profiles.first().map(|p| p.profile_id.clone()))
                .unwrap_or_else(|| DEFAULT_PROFILE_ID.into());
            // The model catalog is per user; any thread can read it.
            let catalog = list.rows.first().map(|r| r.thread_id.clone());
            let input = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 10)
                    .placeholder("Do anything…")
                    .submit_on_enter(true)
            });
            let picker = cx.new(|cx| ModelPicker::new(backend, None, catalog, window, cx));
            let subs = vec![
                cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => this.send_draft(window, cx),
                    InputEvent::Change => {
                        if let Some(draft) = this.draft.as_mut() {
                            inline_files::prune(&draft.input, &mut draft.staged, cx);
                        }
                    }
                    _ => {}
                }),
                cx.subscribe(&picker, |this, _, event: &PickerEvent, cx| {
                    if let (PickerEvent::Failed(message), Some(draft)) = (event, &mut this.draft) {
                        draft.error = Some(message.clone().into());
                        cx.notify();
                    }
                }),
            ];
            let id = profile.clone();
            picker.update(cx, |picker, cx| picker.set_profile(id, cx));
            crate::design::wallpaper::reshuffle(cx);
            self.draft = Some(Draft {
                profile,
                input,
                picker,
                menu_open: false,
                staged: Vec::new(),
                sending: false,
                error: None,
                _subs: subs,
            });
        }
        if let Some(draft) = &self.draft {
            draft.input.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    /// A prompt from the quick composer: the new-thread screen's send, with
    /// its profile and model choice.
    pub(super) fn quick_send(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.open_draft(window, cx);
        if let Some(draft) = self.draft.as_ref() {
            draft
                .input
                .update(cx, |s, cx| s.set_value(text, window, cx));
        }
        self.send_draft(window, cx);
    }

    /// A profile picked in either menu: open the draft screen in it.
    pub(super) fn start_in_profile(
        &mut self,
        profile: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.inbox.new_menu_open = false;
        self.open_draft(window, cx);
        if let Some(draft) = self.draft.as_mut() {
            draft
                .picker
                .update(cx, |picker, cx| picker.set_profile(profile.clone(), cx));
            draft.profile = profile;
            draft.menu_open = false;
        }
        cx.notify();
    }

    /// Create the thread in the chosen profile, apply the model pick, open
    /// it and send the first message there.
    fn send_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(backend), Some(draft)) = (self.backend.clone(), self.draft.as_mut()) else {
            return;
        };
        let (text, files) = inline_files::outgoing(&draft.input, &draft.staged, cx);
        if (text.is_empty() && files.is_empty()) || draft.sending {
            return;
        }
        let content = draft.input.read(cx).content();
        draft.sending = true;
        draft.error = None;
        let profile = draft.profile.clone();
        let pick = draft.picker.read(cx).pending();
        let effort = draft.picker.read(cx).pending_effort();
        cx.notify();
        jobs::run_in(
            &backend,
            window,
            cx,
            move |c| async move {
                let id = match c.create_profile_thread(&profile).await {
                    Ok(created) => created.thread_id,
                    // A gateway without profiles creates a plain thread.
                    Err(e) if e.is_not_found() => c.create_thread().await?.thread.thread_id,
                    Err(e) => return Err(e),
                };
                match (pick, effort) {
                    (Some((provider, model)), effort) => {
                        c.set_thread_route(&id, &provider, &model, effort.as_deref())
                            .await?;
                    }
                    (None, Some(effort)) => {
                        c.set_thread_effort(&id, &effort).await?;
                    }
                    (None, None) => {}
                }
                Ok(id)
            },
            move |this, result, window, cx| match result {
                Ok(id) => {
                    let files = this.draft.take().map(|d| d.staged).unwrap_or_default();
                    this.refresh_inbox(cx);
                    this.open_thread(id, window, cx);
                    if let Some((_, chat)) = this.active.clone() {
                        chat.update(cx, |chat, cx| chat.send_text(content, files, window, cx));
                    }
                }
                Err(e) => {
                    if let Some(draft) = this.draft.as_mut() {
                        draft.sending = false;
                        draft.error = Some(e.to_string().into());
                    }
                    cx.notify();
                }
            },
        );
    }

    fn pick_draft_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.attach_to_draft(paths, window, cx)
                });
            }
        })
        .detach();
    }

    fn attach_thread_to_draft(
        &mut self,
        dragged: &crate::chat::DraggedThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(backend), Some(session), Some(draft)) = (
            self.backend.clone(),
            self.session.as_ref(),
            self.draft.as_ref(),
        ) else {
            return;
        };
        let input = draft.input.clone();
        input.update(cx, |s, cx| s.focus(window, cx));
        let limits = session.attachments.clone();
        let dragged = dragged.clone();
        jobs::run_in(
            &backend,
            window,
            cx,
            move |c| async move {
                c.thread_attachment(&dragged.id, &dragged.title, &limits)
                    .await
            },
            move |this, result, window, cx| {
                let (Some(draft), Some(session)) = (this.draft.as_mut(), this.session.as_ref())
                else {
                    return;
                };
                // A reset or send must not attach this result to a different draft.
                if draft.input != input {
                    return;
                }
                let result = result.map_err(|e| e.to_string()).and_then(|file| {
                    attachments::check(&file, &draft.staged, &session.attachments).map(|()| file)
                });
                match result {
                    Ok(file) => {
                        draft.error = None;
                        this.place_in_draft(vec![file], window, cx);
                    }
                    Err(error) => draft.error = Some(error.into()),
                }
                cx.notify();
            },
        );
    }

    /// Stage `files` in the draft and put their cards at its cursor.
    pub(super) fn place_in_draft(
        &mut self,
        files: Vec<StagedFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let from = draft.staged.len();
        draft.staged.extend(files);
        inline_files::insert(
            &draft.input,
            &draft.staged[from..],
            &draft.staged,
            window,
            cx,
        );
    }

    /// Files or a picture pasted into the draft's composer.
    fn paste_into_draft(
        &mut self,
        pasted: inline_files::Pasted,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match pasted {
            inline_files::Pasted::Paths(paths) => self.attach_to_draft(paths, window, cx),
            inline_files::Pasted::Image { bytes, mime } => {
                let (Some(draft), Some(session)) = (self.draft.as_mut(), self.session.as_ref())
                else {
                    return;
                };
                match inline_files::pasted_image(bytes, mime, &draft.staged, &session.attachments) {
                    Ok(file) => self.place_in_draft(vec![file], window, cx),
                    Err(problem) => draft.error = Some(problem.into()),
                }
                cx.notify();
            }
        }
    }

    /// Read files off the main thread, then stage the ones within limits.
    fn attach_to_draft(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(backend), Some(session)) = (self.backend.clone(), self.session.as_ref()) else {
            return;
        };
        let limits = session.attachments.clone();
        let check = limits.clone();
        jobs::run_in(
            &backend,
            window,
            cx,
            move |_| async move {
                let mut out = Vec::new();
                for path in paths {
                    out.push(attachments::stage(&path, &limits).await);
                }
                Ok(out)
            },
            move |this, result, window, cx| {
                let (Ok(results), Some(draft)) = (result, this.draft.as_mut()) else {
                    return;
                };
                let mut problems = Vec::new();
                let mut files = Vec::new();
                for result in results {
                    match result.and_then(|file| {
                        attachments::check(&file, &draft.staged, &check).map(|()| file)
                    }) {
                        Ok(file) => files.push(file),
                        Err(problem) => problems.push(problem),
                    }
                }
                draft.error = (!problems.is_empty()).then(|| problems.join(" ").into());
                this.place_in_draft(files, window, cx);
                cx.notify();
            },
        );
    }

    pub(super) fn draft_profile_name(&self) -> String {
        let id = self.draft.as_ref().map(|d| d.profile.as_str());
        id.and_then(|id| {
            self.inbox
                .list
                .profiles
                .iter()
                .find(|p| p.profile_id == id)
                .map(|p| p.display_name.clone())
        })
        .unwrap_or_else(|| "Default".into())
    }

    pub(super) fn render_draft(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let Some(draft) = self.draft.as_ref() else {
            return div().into_any_element();
        };
        let name = self.draft_profile_name();
        let ready = !draft.staged.is_empty() || !draft.input.read(cx).value().trim().is_empty();
        let send = round_button("draft-send", IconName::ArrowUp, ready, "Send", cx)
            .when(draft.sending, |d| d.opacity(0.35))
            .on_click(cx.listener(|this, _, window, cx| this.send_draft(window, cx)));
        let attach = round_button(
            "draft-attach",
            ExtraIcon::Paperclip,
            false,
            "Attach files",
            cx,
        )
        .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new("Attach files").build(w, cx))
        .on_click(cx.listener(|this, _, window, cx| this.pick_draft_files(window, cx)));
        let profile_switch = div()
            .relative()
            .child(
                div()
                    .id("draft-profile")
                    .role(Role::Button)
                    .aria_label(format!("Profile: {name}"))
                    .text_color(p.text_muted)
                    .border_b_1()
                    .border_color(p.text_faint)
                    .cursor_pointer()
                    .hover(|s| s.text_color(p.text).border_color(p.text_muted))
                    .child(name)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(d) = this.draft.as_mut() {
                            d.menu_open = !d.menu_open;
                        }
                        cx.notify();
                    })),
            )
            .when(draft.menu_open, |d| {
                d.child(gpui_kit::deferred(
                    div()
                        .absolute()
                        .top(px(26.))
                        .left(px(-110.))
                        .w(px(260.))
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            if let Some(d) = this.draft.as_mut() {
                                d.menu_open = false;
                            }
                            cx.notify();
                        }))
                        .child(self.render_picker(cx)),
                ))
            });
        let body = v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .px_6()
            .gap_6()
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(32.))
                            .font_semibold()
                            .child("What should we work on?"),
                    )
                    .child(profile_switch),
            )
            .child(
                composer_card(
                    "draft-composer",
                    draft.input.focus_handle(cx).is_focused(window),
                    false,
                    None,
                    gpui_kit::div().child(
                        Textarea::new(&draft.input)
                            .appearance(false)
                            .token(inline_files::token_renderer(&draft.staged, cx))
                            .on_paste({
                                let weak = cx.entity().downgrade();
                                move |item, window, cx| {
                                    let Some(files) = inline_files::pasted(item) else {
                                        return false;
                                    };
                                    let _ = weak.update(cx, |this, cx| {
                                        this.paste_into_draft(files, window, cx)
                                    });
                                    true
                                }
                            }),
                    ),
                    draft.picker.clone(),
                    h_flex().gap(px(6.)).child(attach).child(send),
                    cx,
                )
                .max_w(px(720.))
                .id("draft-thread-drop")
                .drag_over::<crate::chat::DraggedThread>(|s, _, _, cx| {
                    s.border_color(cx.theme().accent)
                })
                .on_drop(
                    cx.listener(|this, dragged: &crate::chat::DraggedThread, window, cx| {
                        this.attach_thread_to_draft(dragged, window, cx);
                    }),
                )
                .relative()
                .child(crate::design::dock::hero_probe()),
            )
            .when_some(draft.error.clone(), |d, error| {
                d.child(
                    div()
                        .max_w(px(720.))
                        .text_sm()
                        .text_color(p.danger)
                        .whitespace_normal()
                        .child(error),
                )
            })
            // Room under the card so it sits just above the middle, like t3code.
            .child(div().h(px(80.)));
        // Without a wallpaper, a soft accent glow from the top stands in.
        let bare =
            crate::design::prefs::Prefs::of(cx).wallpaper == crate::design::prefs::Wallpaper::None;
        div()
            .id("draft")
            .size_full()
            .relative()
            .drag_over::<ExternalPaths>(|this, _, _, cx| this.bg(cx.theme().accent.opacity(0.15)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.attach_to_draft(paths.paths().to_vec(), window, cx)
            }))
            .when(bare, |d| {
                d.child(div().absolute().top_0().left_0().right_0().h(px(320.)).bg(
                    gpui_kit::linear_gradient(
                        180.,
                        gpui_kit::linear_color_stop(p.accent_wash, 0.),
                        gpui_kit::linear_color_stop(p.accent_wash.opacity(0.), 1.),
                    ),
                ))
            })
            .child(motion::fade_in(body, "draft", Style::of(cx).enter, 4.))
            .into_any_element()
    }
}
