//! The composer's approval pill: "Full access" (tool calls run without
//! asking) or "Ask first" (calls that need approval open a card). The mode
//! belongs to the thread's profile, so switching it here changes every
//! thread on that profile, running ones included from their next tool call;
//! the profile editor edits the same value. A profile without a mode follows
//! the global auto-approve setting, and the pill shows what that resolves to.

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::profiles::{ApprovalMode, DEFAULT_PROFILE_ID, ProfileInfo};

use crate::assets::ExtraIcon;
use crate::design::{self, Palette, kit};
use crate::jobs;
use crate::ui::*;

/// The global switch's key in the tool settings list.
const AUTO_APPROVE_KEY: &str = "agent.auto_approve_tools";

/// Menu order, shared with the profile editor.
pub(crate) const CHOICES: [Option<ApprovalMode>; 3] = [
    Some(ApprovalMode::FullAccess),
    Some(ApprovalMode::AskFirst),
    None,
];

pub(crate) fn label(mode: Option<ApprovalMode>) -> &'static str {
    match mode {
        Some(ApprovalMode::FullAccess) => "Full access",
        Some(ApprovalMode::AskFirst) => "Ask first",
        None => "Follow global setting",
    }
}

pub(crate) fn key(mode: Option<ApprovalMode>) -> &'static str {
    match mode {
        Some(ApprovalMode::FullAccess) => "full-access",
        Some(ApprovalMode::AskFirst) => "ask-first",
        None => "global",
    }
}

/// Bumped whenever an approval mode or the global switch is saved, from
/// any page; every pill reloads on it.
#[derive(Default)]
pub(crate) struct ApprovalEpoch(u64);
impl Global for ApprovalEpoch {}

pub(crate) fn changed(cx: &mut App) {
    cx.default_global::<ApprovalEpoch>().0 += 1;
}

pub struct ApprovalPill {
    backend: Backend,
    thread_id: String,
    /// The thread's profile, once loaded.
    profile: Option<ProfileInfo>,
    /// The global auto-approve switch, once loaded.
    global: Option<bool>,
    open: bool,
    busy: bool,
    /// Counts loads and saves; a reply from an older one is dropped.
    seq: u64,
    _epoch: Subscription,
}

impl ApprovalPill {
    pub fn new(backend: Backend, thread_id: String, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            thread_id,
            profile: None,
            global: None,
            open: false,
            busy: false,
            seq: 0,
            _epoch: cx.observe_global::<ApprovalEpoch>(|this, cx| this.load(cx)),
        };
        this.load(cx);
        this
    }

    /// Reads the thread's profile and the global switch (again on each open,
    /// so an edit on the Profiles page shows up here).
    fn load(&mut self, cx: &mut Context<Self>) {
        self.seq += 1;
        let seq = self.seq;
        let thread = self.thread_id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                let inbox = c.inbox().await?;
                let id = inbox
                    .rows
                    .iter()
                    .find(|r| r.thread_id == thread)
                    .map(|r| r.profile_id.clone())
                    .unwrap_or_else(|| DEFAULT_PROFILE_ID.to_string());
                c.profile(&id).await
            },
            move |this, result, cx| {
                // silent-ok: an unreadable profile hides the pill; the gate still applies.
                if let Ok(info) = result
                    && this.seq == seq
                {
                    this.profile = Some(info);
                    cx.notify();
                }
            },
        );
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.tool_settings().await },
            move |this, result, cx| {
                // silent-ok: without the global value the pill shows "Ask first" for an unset mode.
                if let Ok(list) = result
                    && this.seq == seq
                {
                    this.global = list
                        .entries
                        .iter()
                        .find(|e| e.key == AUTO_APPROVE_KEY)
                        .and_then(|e| e.value.as_bool());
                    cx.notify();
                }
            },
        );
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        self.open = !self.open;
        if self.open {
            self.load(cx);
        }
        cx.notify();
    }

    /// Saves `mode` on the profile. The profile is re-read first so a save
    /// here never overwrites an edit made elsewhere; the pill shows the
    /// stored result.
    fn pick(&mut self, mode: Option<ApprovalMode>, cx: &mut Context<Self>) {
        let Some(id) = self.profile.as_ref().map(|p| p.profile_id.clone()) else {
            return;
        };
        self.open = false;
        self.busy = true;
        // A load still in flight must not overwrite what this save stores.
        self.seq += 1;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                let mut info = c.profile(&id).await?;
                info.settings.approval_mode = mode;
                c.update_profile(&id, &info.settings).await
            },
            |this, result, cx| {
                this.busy = false;
                // silent-ok: a failed save keeps the stored mode on the pill (jobs logs it).
                if let Ok(info) = result {
                    this.profile = Some(info);
                    // Other chats on this profile show it too.
                    changed(cx);
                }
                cx.notify();
            },
        );
    }

    /// What the gate does on this profile now.
    fn effective_label(&self, mode: Option<ApprovalMode>) -> &'static str {
        if ApprovalMode::resolve_auto_approve(mode, self.global.unwrap_or(false)) {
            label(Some(ApprovalMode::FullAccess))
        } else {
            label(Some(ApprovalMode::AskFirst))
        }
    }

    fn render_popover(&self, current: Option<ApprovalMode>, cx: &Context<Self>) -> AnyElement {
        // Same shape as the Thinking menu beside it: a faint header, one
        // line per choice, a check on the picked one, a hint at the bottom.
        let p = *Palette::of(cx);
        let row = |mode: Option<ApprovalMode>| {
            let on = current == mode;
            // The global row says what the switch resolves to right now.
            let trailing = mode.is_none().then(|| self.effective_label(None));
            kit::row(
                SharedString::from(format!("approval-{}", key(mode))),
                on,
                cx,
            )
            .role(Role::Button)
            .aria_label(label(mode))
            .gap_2()
            .text_sm()
            .child(
                Icon::new(icon(mode))
                    .size(px(13.))
                    .text_color(tint(mode, self.global, &p)),
            )
            .child(div().flex_1().min_w_0().truncate().child(label(mode)))
            .children(trailing.map(|t| {
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(p.text_faint)
                    .child(t)
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(14.))
                    .when(on, |d| d.child(Icon::new(IconName::Check).size(px(13.)))),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.pick(mode, cx)))
        };
        let hint = match current {
            Some(ApprovalMode::FullAccess) => "Tool calls run without asking.",
            Some(ApprovalMode::AskFirst) => "Calls that need approval wait for you.",
            None => "Follows Settings > Permissions.",
        };
        let pop = kit::popover("approval-pop", cx);
        pop(v_flex()
            .w(px(240.))
            .p_1()
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .pb_1()
                    .text_xs()
                    .font_medium()
                    .text_color(p.text_faint)
                    .child("Access"),
            )
            .children(CHOICES.into_iter().map(row))
            .child(
                div()
                    .mt_1()
                    .px_2()
                    .pt_1p5()
                    .pb_1()
                    .border_t_1()
                    .border_color(p.border)
                    .text_xs()
                    .text_color(p.text_faint)
                    .child(hint),
            ))
    }
}

/// The icon a mode shows on the pill and in the menu.
fn icon(mode: Option<ApprovalMode>) -> ExtraIcon {
    match mode {
        Some(ApprovalMode::FullAccess) => ExtraIcon::Zap,
        Some(ApprovalMode::AskFirst) => ExtraIcon::ShieldCheck,
        None => ExtraIcon::Settings2,
    }
}

/// Full access reads warm (nothing stops a call), ask first calm.
fn tint(mode: Option<ApprovalMode>, global: Option<bool>, p: &Palette) -> gpui_kit::Hsla {
    let full = ApprovalMode::resolve_auto_approve(mode, global.unwrap_or(false));
    match mode {
        None => p.text_muted,
        _ if full => p.warning,
        _ => p.success,
    }
}

impl Render for ApprovalPill {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let Some(profile) = &self.profile else {
            return div().into_any_element();
        };
        let current = profile.settings.approval_mode;
        let text = self.effective_label(current);
        let full = ApprovalMode::resolve_auto_approve(current, self.global.unwrap_or(false));
        let mark = if full {
            Some(ApprovalMode::FullAccess)
        } else {
            Some(ApprovalMode::AskFirst)
        };
        let pill = h_flex()
            .id("approval-pill")
            .role(Role::Button)
            .aria_label(format!("Approvals: {text}"))
            .gap_1p5()
            .px_2()
            .h(px(28.))
            .items_center()
            .rounded(px(design::CONTROL_RADIUS))
            .text_sm()
            .text_color(p.text_muted)
            .cursor_pointer()
            .when(self.open, |d| d.bg(p.active).text_color(p.text))
            .hover(|s| s.bg(p.hover).text_color(p.text))
            .child(
                Icon::new(icon(mark))
                    .size(px(13.))
                    .text_color(tint(mark, self.global, &p)),
            )
            .child(text)
            .when(self.busy, |d| d.child(Spinner::new().xsmall()))
            .when(!self.busy, |d| {
                d.child(Icon::new(IconName::ChevronDown).xsmall())
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)));
        let popover = self.open.then(|| self.render_popover(current, cx));
        div()
            .relative()
            .child(pill)
            .children(popover.map(|pop| {
                gpui_kit::deferred(
                    div()
                        .absolute()
                        .bottom(px(34.))
                        .left_0()
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.open = false;
                            cx.notify();
                        }))
                        .child(pop),
                )
                .with_priority(1)
            }))
            .into_any_element()
    }
}
