//! The session card (Zeron's first titlebar toggle) and side chats.
//!
//! The card shows where the conversation runs: the profile, its model and
//! folder, the project the coding tabs read, and the side chats.
//!
//! A side chat is a by-the-way question about this conversation. It is a
//! fork (the conversation so far, the running turn's prompt included) that
//! opens as a full chat in the right pane, so the main turn keeps going in
//! view while the side one answers. Side chats are listed under their
//! parent (in `Prefs::side_chats`) and stay out of the inbox. One open at a
//! time; closing it drops its stream, reopening reads it back.

use ironclaw_gpui_client::orchestration::ForkOptions;

use gpui_kit::{Div, Focusable as _, Stateful};

use super::{ChatView, Tab, short_path};
use crate::assets::ExtraIcon;
use crate::chat::{OpenRunLogs, OpenThread, RunEnded, SideLink, ThreadUpdated};
use crate::design::prefs::Prefs;
use crate::design::{Palette, Style, kit, motion, panels};
use crate::jobs::flatten;
use crate::ui::*;

/// The side chat open in the right pane. Its view is built on first show.
pub(in crate::chat) struct SideChat {
    pub(in crate::chat) thread_id: String,
    view: Option<Entity<ChatView>>,
    _events: Vec<Subscription>,
}

impl ChatView {
    /// Fork this conversation into a side chat and show it.
    pub(crate) fn new_side_chat(&mut self, cx: &mut Context<Self>) {
        // Zeron's rule: only top-level chats have side chats.
        if self.embedded || self.side.chat_busy {
            return;
        }
        if self.rows.is_empty() {
            self.set_error(
                "Send a message first: a side chat starts from this conversation.",
                cx,
            );
            return;
        }
        self.side.chat_busy = true;
        self.side.session_open = false;
        cx.notify();
        let parent = self.thread_id.clone();
        let job = self.backend.spawn({
            let parent = parent.clone();
            move |c| async move {
                let options = ForkOptions {
                    title: Some("Side chat".into()),
                    ..ForkOptions::default()
                };
                c.fork_thread(&parent, &options).await
            }
        });
        // The link to the parent is saved even if this view is gone by the
        // time the fork lands (the user switched threads), so the child
        // never shows up in the inbox as a stray thread.
        cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            if let Ok(forked) = &result {
                let child = forked.fork_thread_id.clone();
                cx.update(|cx| {
                    Prefs::update(cx, |p| p.side_chats.entry(parent).or_default().push(child))
                });
            }
            let _ = this.update(cx, |this, cx| {
                this.side.chat_busy = false;
                match result {
                    Ok(forked) => this.open_side_chat(forked.fork_thread_id, cx),
                    Err(e) => {
                        tracing::warn!(error = %e, "side chat fork failed");
                        this.set_error(e.to_string(), cx)
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Show side chat `thread_id` in the right pane.
    pub(super) fn open_side_chat(&mut self, thread_id: String, cx: &mut Context<Self>) {
        self.side.session_open = false;
        if self.side.chat.as_ref().map(|c| c.thread_id.as_str()) != Some(thread_id.as_str()) {
            self.side.chat = Some(SideChat {
                thread_id,
                view: None,
                _events: Vec::new(),
            });
        }
        self.show_side(Tab::SideChat, cx);
    }

    /// Close the side chat; the pane goes back to Activity.
    fn close_side_chat(&mut self, cx: &mut Context<Self>) {
        self.side.chat = None;
        Prefs::update(cx, |p| p.side_tab = Tab::Activity.id().into());
        cx.notify();
    }

    /// The side chats of this thread, newest first.
    fn side_chats(&self, cx: &App) -> Vec<String> {
        Prefs::of(cx)
            .side_chats
            .get(&self.thread_id)
            .map(|ids| ids.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    /// The pane was left on a side chat: the newest one comes back.
    pub(super) fn side_chat_restore(&mut self, cx: &mut Context<Self>) {
        if self.embedded || Prefs::of(cx).side_tab != Tab::SideChat.id() {
            return;
        }
        if let Some(id) = self.side_chats(cx).into_iter().next() {
            self.side.chat = Some(SideChat {
                thread_id: id,
                view: None,
                _events: Vec::new(),
            });
        }
    }

    /// A hidden, idle side chat lets go of its view: its stream, poll and
    /// transcript. A running one stays so the dot and the answer arrive; it
    /// lets go once it ends. Showing it again reads the thread back.
    pub(super) fn side_chat_release(&mut self, cx: &App) {
        let shown = self.dock_tab(super::Dock::Side, cx) == Tab::SideChat
            && panels::reveal("side-reveal", super::dock_open(super::Dock::Side, cx), cx) > 0.;
        if shown || self.side_chat_running(cx) {
            return;
        }
        if let Some(chat) = &mut self.side.chat
            && chat.view.is_some()
        {
            chat.view = None;
            chat._events.clear();
        }
    }

    /// The model picker ⇧⌘M and Esc mean: the side chat's while its
    /// composer has focus or its picker is open, else this chat's.
    pub fn focused_picker(
        &self,
        window: &Window,
        cx: &App,
    ) -> Entity<crate::chat::model_picker::ModelPicker> {
        if let Some(view) = self.side.chat.as_ref().and_then(|c| c.view.as_ref()) {
            let side = view.read(cx);
            if side.composer.focus_handle(cx).contains_focused(window, cx)
                || side.picker.read(cx).is_open()
            {
                return side.picker.clone();
            }
        }
        self.picker.clone()
    }

    pub(super) fn side_chat_running(&self, cx: &App) -> bool {
        self.side
            .chat
            .as_ref()
            .and_then(|c| c.view.as_ref())
            .is_some_and(|v| v.read(cx).running_run().is_some())
    }

    /// The open side chat's view, built on first show.
    fn side_chat_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ChatView>> {
        let chat = self.side.chat.as_mut()?;
        if let Some(view) = &chat.view {
            return Some(view.clone());
        }
        let (backend, channel, limits) = (
            self.backend.clone(),
            self.channel.clone(),
            self.limits.clone(),
        );
        let id = chat.thread_id.clone();
        let view =
            cx.new(|cx| ChatView::new_side_chat_view(backend, channel, limits, id, window, cx));
        // What the side chat asks of the window goes through this view.
        chat._events = vec![
            cx.subscribe(&view, |_, _, e: &OpenThread, cx| {
                cx.emit(OpenThread(e.0.clone()))
            }),
            cx.subscribe(&view, |_, _, e: &OpenRunLogs, cx| {
                cx.emit(OpenRunLogs(e.0.clone()))
            }),
            cx.subscribe(&view, |this, _, e: &SideLink, cx| {
                this.open_link(&e.href, e.external, cx)
            }),
            cx.subscribe(&view, |_, _, _: &RunEnded, cx| cx.notify()),
            cx.subscribe(&view, |_, _, _: &ThreadUpdated, cx| cx.notify()),
        ];
        chat.view = Some(view.clone());
        crate::titles::ensure([chat.thread_id.clone()], &self.backend, cx);
        Some(view)
    }

    pub(super) fn render_side_chat(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.side_chat_view(window, cx) {
            Some(view) => div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(view)
                .into_any_element(),
            None => super::empty("No side chat open.", cx),
        }
    }

    /// The side chat's title and its buttons, in the pane's title row.
    pub(super) fn render_side_chat_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(chat) = &self.side.chat else {
            return div().into_any_element();
        };
        let id = chat.thread_id.clone();
        let title = crate::titles::get(cx, &id).filter(|t| t != "Side chat");
        h_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .items_center()
            .child(div().flex_1().min_w_0().truncate().children(title))
            .child(
                Button::new("side-chat-new")
                    .icon(IconName::Plus)
                    .ghost()
                    .xsmall()
                    .tooltip("New side chat")
                    .accessibility_label("New side chat")
                    .on_click(cx.listener(|this, _, _, cx| this.new_side_chat(cx))),
            )
            .child(
                Button::new("side-chat-full")
                    .icon(IconName::Maximize)
                    .ghost()
                    .xsmall()
                    .tooltip("Open as the main chat")
                    .accessibility_label("Open side chat as the main chat")
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(OpenThread(id.clone())))),
            )
            .child(
                Button::new("side-chat-close")
                    .icon(IconName::Close)
                    .ghost()
                    .xsmall()
                    .tooltip("Close side chat")
                    .accessibility_label("Close side chat")
                    .on_click(cx.listener(|this, _, _, cx| this.close_side_chat(cx))),
            )
            .into_any_element()
    }

    /// Zeron's first titlebar toggle: the session card, with a dot while a
    /// side chat works.
    pub(super) fn render_session_toggle(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let open = self.side.session_open;
        let running = self.side_chat_running(cx);
        let toggle = panels::toggle_with(
            "session-toggle",
            open,
            "Session and side chats",
            cx,
            |m, ink| panels::card_glyph(m, panels::GLYPH_PX, ink).into_any_element(),
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.side.session_open = !this.side.session_open;
            if this.side.session_open {
                let ids = this.side_chats(cx);
                crate::titles::ensure(ids, &this.backend, cx);
            }
            cx.notify();
        }));
        let card = open.then(|| self.render_session_card(cx));
        div()
            .id("session-wrap")
            .relative()
            .on_hover(cx.listener(|this, h: &bool, _, _| this.side.session_hover = *h))
            .child(toggle)
            .when(running || self.side.chat_busy, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(3.))
                        .right(px(3.))
                        .size(px(6.))
                        .rounded_full()
                        .bg(p.accent),
                )
            })
            .children(card.map(|card| {
                gpui_kit::deferred(
                    div().absolute().top(px(34.)).right_0().child(
                        gpui_kit::anchored()
                            .anchor(gpui_kit::Anchor::TopRight)
                            .snap_to_window_with_margin(px(8.))
                            .child(
                                div()
                                    .w(px(280.))
                                    .occlude()
                                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                        if this.side.session_hover {
                                            return;
                                        }
                                        this.side.session_open = false;
                                        cx.notify();
                                    }))
                                    .child(card),
                            ),
                    ),
                )
                .with_priority(1)
            }))
            .into_any_element()
    }

    /// Zeron's session card: two glass cards, where the chat runs (profile,
    /// model, folder, project) and its side chats, as plain rows.
    fn render_session_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let st = *Style::of(cx);
        let card = |content: Div| {
            kit::glass(&st, &p).apply(
                content
                    .w_full()
                    .p_1()
                    .rounded(px(st.panel_radius.clamp(10., 14.))),
            )
        };
        let row = |id: SharedString, icon: Icon, text: SharedString, live: bool| {
            h_flex()
                .id(id)
                .h(px(32.))
                .px_2()
                .gap_2p5()
                .items_center()
                .rounded(px(8.))
                .text_sm()
                .text_color(if live { p.text } else { p.text_faint })
                .child(icon.size(px(15.)).text_color(if live {
                    p.text_muted
                } else {
                    p.text_faint
                }))
                .child(div().flex_1().min_w_0().truncate().child(text))
        };
        let action = |el: Stateful<Div>, enabled: bool| {
            el.when(enabled, |d| {
                d.role(Role::Button)
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
            })
        };
        let hint = |text: SharedString| {
            div()
                .flex_none()
                .text_xs()
                .text_color(p.text_faint)
                .child(text)
        };

        // Where it runs.
        let profile = self.approval.read(cx).profile().cloned();
        let model = self.picker.read(cx).model_label();
        let who: SharedString = match &profile {
            Some(info) => format!("{} · {model}", info.settings.display_name).into(),
            None => model.into(),
        };
        let mut place = v_flex().child(row(
            "session-profile".into(),
            Icon::new(ExtraIcon::Laptop),
            who,
            true,
        ));
        if let Some(info) = &profile {
            place = place.child(
                row(
                    "session-folder".into(),
                    Icon::new(IconName::Folder),
                    short_path(std::path::Path::new(&info.effective_folder)).into(),
                    true,
                )
                .when_some(info.git_branch.clone(), |d, branch| {
                    d.child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .text_color(p.text_faint)
                            .child(Icon::new(ExtraIcon::GitBranch).size(px(12.)))
                            .child(branch),
                    )
                }),
            );
        }
        let has_project = self.side.folder.is_some();
        let project = action(
            row(
                "session-project".into(),
                Icon::new(ExtraIcon::House),
                self.side
                    .folder
                    .as_deref()
                    .map_or("No project".into(), |f| short_path(f).into()),
                has_project,
            ),
            true,
        )
        .aria_label("Choose the project folder")
        .text_color(if has_project { p.text } else { p.text_muted })
        .child(hint(if has_project {
            "Change".into()
        } else {
            "Choose…".into()
        }))
        .on_click(cx.listener(|this, _, window, cx| {
            this.side.session_open = false;
            this.pick_project_folder(window, cx);
        }));
        let place = place.child(project);

        // Side chats.
        let open_id = self.side.chat.as_ref().map(|c| c.thread_id.clone());
        let running = self.side_chat_running(cx);
        let chats = self.side_chats(cx);
        let listed = chats.into_iter().enumerate().map(|(ix, id)| {
            let shown = open_id.as_deref() == Some(id.as_str());
            let name = crate::titles::get(cx, &id).unwrap_or_else(|| "Side chat".into());
            action(
                row(
                    format!("session-side-chat-{ix}").into(),
                    Icon::new(ExtraIcon::MessagesSquare),
                    name.clone().into(),
                    true,
                ),
                true,
            )
            .aria_label(format!("Open side chat {name}"))
            .when(shown, |d| d.bg(p.active))
            .when(shown && running, |d| {
                d.child(kit::pulse_dots(("session-side-run", ix), p.accent))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.open_side_chat(id.clone(), cx)))
        });
        let can_fork = !self.rows.is_empty();
        let busy = self.side.chat_busy;
        let new_chat = action(
            row(
                "session-new-side-chat".into(),
                Icon::new(IconName::Plus),
                "New side chat".into(),
                can_fork && !busy,
            ),
            can_fork && !busy,
        )
        .aria_label("New side chat")
        .when(busy, |d| {
            d.child(kit::pulse_dots("session-side-new", p.accent))
        })
        .when(!busy, |d| d.child(hint("⌥⌘N".into())))
        .when(can_fork && !busy, |d| {
            d.on_click(cx.listener(|this, _, _, cx| this.new_side_chat(cx)))
        });
        let fork = action(
            row(
                "session-fork".into(),
                Icon::new(ExtraIcon::GitFork),
                "Fork chat".into(),
                can_fork,
            ),
            can_fork,
        )
        .aria_label("Fork chat")
        .when(can_fork, |d| {
            d.on_click(cx.listener(|this, _, _, cx| {
                this.side.session_open = false;
                this.fork(None, cx)
            }))
        });
        let side = v_flex()
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(p.text_faint)
                    .child("SIDE CHATS"),
            )
            .children(listed)
            .child(new_chat)
            .child(fork)
            .when(!can_fork, |d| {
                d.child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_xs()
                        .text_color(p.text_faint)
                        .child("Send a message first."),
                )
            });

        motion::pop_in(
            v_flex().gap_2().child(card(place)).child(card(side)),
            "session-card",
            st.pop,
            -6.,
        )
        .into_any_element()
    }
}
