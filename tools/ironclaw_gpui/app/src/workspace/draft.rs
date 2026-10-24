//! The new-thread screen (t3code's, with a profile where t3code has a
//! project): "What should we work on?", the profile underlined below it, and
//! the composer card. Nothing exists on the gateway until the first message
//! is sent, so abandoned drafts leave no empty threads behind.

use ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID;

use super::{Page, Workspace};
use crate::chat::composer_card;
use crate::chat::model_picker::{ModelPicker, PickerEvent};
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
                cx.subscribe_in(&input, window, |this, _, event, window, cx| {
                    if let InputEvent::PressEnter { shift: false, .. } = event {
                        this.send_draft(window, cx);
                    }
                }),
                cx.subscribe(&picker, |this, _, event: &PickerEvent, cx| {
                    if let (PickerEvent::Failed(message), Some(draft)) = (event, &mut this.draft) {
                        draft.error = Some(message.clone().into());
                        cx.notify();
                    }
                }),
            ];
            self.draft = Some(Draft {
                profile,
                input,
                picker,
                menu_open: false,
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
        let text = draft.input.read(cx).value().trim().to_string();
        if text.is_empty() || draft.sending {
            return;
        }
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
                    this.draft = None;
                    this.refresh_inbox(cx);
                    this.open_thread(id, window, cx);
                    if let Some((_, chat)) = this.active.clone() {
                        chat.update(cx, |chat, cx| chat.send_text(text, window, cx));
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
        let send = crate::chat::round_button("draft-send", IconName::ArrowUp, true, "Send", cx)
            .when(draft.sending, |d| d.opacity(0.35))
            .on_click(cx.listener(|this, _, window, cx| this.send_draft(window, cx)));
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
                    gpui_kit::div().child(Textarea::new(&draft.input).appearance(false)),
                    draft.picker.clone(),
                    send,
                    cx,
                )
                .max_w(px(720.))
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
        div()
            .size_full()
            .relative()
            .child(div().absolute().top_0().left_0().right_0().h(px(320.)).bg(
                gpui_kit::linear_gradient(
                    180.,
                    gpui_kit::linear_color_stop(p.accent_wash, 0.),
                    gpui_kit::linear_color_stop(p.accent_wash.opacity(0.), 1.),
                ),
            ))
            .child(motion::fade_in(body, "draft", Style::of(cx).enter, 4.))
            .into_any_element()
    }
}
