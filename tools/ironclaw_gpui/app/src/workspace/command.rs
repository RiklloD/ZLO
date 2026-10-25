//! The command menu (Cmd+K): one search field over threads, new threads per
//! profile, every settings page and the view actions. Entrance after
//! MonoCode's `modal.rs` (MIT, see `THIRD_PARTY_NOTICES.md`): the scrim
//! fades in and the card rises 6 px over 200 ms; the selection pill glides
//! between rows.

use gpui_kit::ScrollHandle;
use gpui_kit::component::IconName;
use ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID;

use super::{Page, Workspace, row_title};
use crate::design::{self, Palette, Style, kit, motion};
use crate::ui::*;

const ROW_H: f32 = 36.;
const LIST_MAX_H: f32 = 380.;
const RESULTS: usize = 60;
/// MonoCode's modal: 200 ms ease-out, 6 px rise.
const ENTER: motion::MotionSpec = motion::MotionSpec::new(200, motion::EASE_OUT);
const RISE: f32 = 6.;

#[derive(Clone, PartialEq)]
enum Cmd {
    Thread(String),
    NewThread(String),
    Page(Page),
    Scheme(&'static str),
    StatusBar,
    ToggleScheme,
    Browser,
}

#[derive(Clone)]
struct Entry {
    cmd: Cmd,
    icon: IconName,
    title: SharedString,
    /// Right-hand hint: the kind of entry, or its shortcut.
    hint: SharedString,
}

pub(super) struct CommandMenu {
    input: Entity<InputState>,
    selected: usize,
    /// Bumped per open, so every open replays the entrance.
    opened: u64,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl Workspace {
    pub(super) fn toggle_command_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command.take().is_some() {
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search threads, settings and actions")
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        let subs =
            vec![
                cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                    InputEvent::Change => {
                        if let Some(menu) = this.command.as_mut() {
                            menu.selected = 0;
                            menu.scroll.scroll_to_item(0);
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.run_selected(window, cx),
                    _ => {}
                }),
            ];
        self.command_opens += 1;
        self.command = Some(CommandMenu {
            input,
            selected: 0,
            opened: self.command_opens,
            scroll: ScrollHandle::new(),
            _subs: subs,
        });
        cx.notify();
    }

    fn entries(&self, cx: &App) -> Vec<Entry> {
        let mut out = Vec::new();
        let list = &self.inbox.list;
        let mut profiles: Vec<(String, String)> = list
            .profiles
            .iter()
            .map(|p| (p.profile_id.clone(), p.display_name.clone()))
            .collect();
        if profiles.is_empty() {
            profiles.push((DEFAULT_PROFILE_ID.into(), "Default".into()));
        }
        let several = profiles.len() > 1;
        for (id, name) in profiles {
            out.push(Entry {
                cmd: Cmd::NewThread(id),
                icon: IconName::Plus,
                title: if several {
                    format!("New thread in {name}").into()
                } else {
                    "New thread".into()
                },
                hint: "New".into(),
            });
        }
        let mut rows: Vec<_> = list.rows.iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.last_activity_at));
        for row in rows {
            let profile = list
                .profiles
                .iter()
                .find(|p| p.profile_id == row.profile_id)
                .map(|p| p.display_name.clone());
            out.push(Entry {
                cmd: Cmd::Thread(row.thread_id.clone()),
                icon: IconName::Inbox,
                title: row_title(row).to_string().into(),
                hint: profile.unwrap_or_else(|| "Thread".into()).into(),
            });
        }
        for (page, icon, label) in self.settings_pages() {
            out.push(Entry {
                cmd: Cmd::Page(page),
                icon,
                title: label.into(),
                hint: "Settings".into(),
            });
        }
        let scheme = design::prefs::Prefs::of(cx).scheme.clone();
        for (value, icon, label) in [
            ("system", IconName::Settings2, "Theme: System"),
            ("light", IconName::Sun, "Theme: Light"),
            ("dark", IconName::Moon, "Theme: Dark"),
        ] {
            if scheme != value {
                out.push(Entry {
                    cmd: Cmd::Scheme(value),
                    icon,
                    title: label.into(),
                    hint: "Appearance".into(),
                });
            }
        }
        out.push(Entry {
            cmd: Cmd::StatusBar,
            icon: IconName::PanelBottom,
            title: "Toggle status bar".into(),
            hint: "⇧⌘S".into(),
        });
        out.push(Entry {
            cmd: Cmd::ToggleScheme,
            icon: IconName::Palette,
            title: "Toggle light and dark".into(),
            hint: "⇧⌘Y".into(),
        });
        out.push(Entry {
            cmd: Cmd::Browser,
            icon: IconName::Globe,
            title: "Open the web UI in the browser".into(),
            hint: "Action".into(),
        });
        out
    }

    /// Entries matching the query, best first. An empty query keeps the
    /// natural order: new thread, recent threads, settings, actions.
    fn matches(&self, cx: &App) -> Vec<Entry> {
        let query = self
            .command
            .as_ref()
            .map(|m| m.input.read(cx).value().trim().to_lowercase())
            .unwrap_or_default();
        let all = self.entries(cx);
        if query.is_empty() {
            return all.into_iter().take(RESULTS).collect();
        }
        let mut scored: Vec<(i32, Entry)> = all
            .into_iter()
            .filter_map(|e| score(&query, &e.title.to_lowercase()).map(|s| (s, e)))
            .collect();
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        scored.into_iter().take(RESULTS).map(|(_, e)| e).collect()
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.matches(cx).len();
        if let Some(menu) = self.command.as_mut()
            && count > 0
        {
            menu.selected = (menu.selected as isize + delta).rem_euclid(count as isize) as usize;
            // The pill is the scroll content's first child.
            menu.scroll.scroll_to_item(menu.selected + 1);
            cx.notify();
        }
    }

    fn run_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.command.as_ref().map(|m| m.selected) else {
            return;
        };
        if let Some(entry) = self.matches(cx).into_iter().nth(selected) {
            self.run(entry.cmd, window, cx);
        }
    }

    fn run(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        self.command = None;
        match cmd {
            Cmd::Thread(id) => self.open_thread(id, window, cx),
            Cmd::NewThread(profile) => self.start_in_profile(profile, window, cx),
            Cmd::Page(page) => self.show(page, window, cx),
            Cmd::Scheme(value) => design::update_appearance(cx, |p| p.scheme = value.into()),
            Cmd::StatusBar => super::toggle_status_bar(cx),
            Cmd::ToggleScheme => design::toggle_scheme(cx),
            Cmd::Browser => self.open_in_browser(cx),
        }
        cx.notify();
    }

    pub(super) fn render_command_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.command.as_ref()?;
        let p = *Palette::of(cx);
        let s = *Style::of(cx);
        let reduced = cx.reduce_motion();
        let t = motion::reveal(&format!("cmdk:{}", menu.opened), ENTER, 0, reduced);
        let selected = menu.selected;
        let input = menu.input.clone();
        let scroll = menu.scroll.clone();
        let entries = self.matches(cx);
        let selected = selected.min(entries.len().saturating_sub(1));
        let pill_top = (!entries.is_empty()).then_some(selected as f32 * ROW_H);

        let rows = entries.into_iter().enumerate().map(|(ix, e)| {
            let on = ix == selected;
            let cmd = e.cmd.clone();
            h_flex()
                .id(("cmdk-row", ix))
                .relative()
                .h(px(ROW_H))
                .flex_none()
                .px_3()
                .gap_3()
                .items_center()
                .rounded(px(8.))
                .cursor_pointer()
                .text_size(gpui_kit::rems(13. / 16.))
                .text_color(if on { p.text } else { p.text.opacity(0.82) })
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if let Some(m) = this.command.as_mut()
                        && m.selected != ix
                    {
                        m.selected = ix;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, window, cx| this.run(cmd.clone(), window, cx)))
                .child(
                    Icon::new(e.icon)
                        .small()
                        .text_color(if on { p.text } else { p.text_muted }),
                )
                .child(div().flex_1().min_w_0().truncate().child(e.title))
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(p.text_faint)
                        .child(e.hint),
                )
        });
        let list = v_flex()
            .id("cmdk-list")
            .relative()
            .max_h(px(LIST_MAX_H))
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .p_1()
            .child(kit::slide_pill("cmdk-pill", pill_top, cx))
            .children(rows);
        let empty = pill_top.is_none();

        let card = kit::floating(&s, &p).apply(
            v_flex()
                .w(px(600.))
                .rounded(px(s.panel_radius.clamp(8., 16.)))
                .overflow_hidden()
                .child(
                    h_flex()
                        .h(px(52.))
                        .px_4()
                        .gap_3()
                        .items_center()
                        .border_b_1()
                        .border_color(p.border)
                        .child(Icon::new(IconName::Search).text_color(p.text_muted))
                        .child(div().flex_1().child(Input::new(&input).appearance(false))),
                )
                .child(list)
                .when(empty, |d| {
                    d.child(
                        div()
                            .p_6()
                            .text_sm()
                            .text_color(p.text_muted)
                            .child("Nothing matches"),
                    )
                })
                .child(
                    h_flex()
                        .h(px(30.))
                        .px_4()
                        .gap_4()
                        .items_center()
                        .border_t_1()
                        .border_color(p.border)
                        .text_xs()
                        .text_color(p.text_faint)
                        .child("↑↓ move")
                        .child("↵ open")
                        .child("esc close"),
                ),
        );

        Some(
            div()
                .id("cmdk-scrim")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(gpui_kit::black().opacity(0.22 * t))
                .flex()
                .flex_col()
                .items_center()
                .pt(px(96.))
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.command = None;
                        cx.notify();
                    }),
                )
                .capture_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                    match e.keystroke.key.as_str() {
                        "escape" => {
                            this.command = None;
                            cx.notify();
                        }
                        "up" => this.move_selection(-1, cx),
                        "down" => this.move_selection(1, cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }))
                .child(
                    div()
                        .id("cmdk-card")
                        .relative()
                        .top(px(RISE * (1. - t)))
                        .opacity(t)
                        // Clicks inside the card never reach the scrim.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(card),
                )
                .into_any_element(),
        )
    }
}

/// Subsequence match: every query character in order. Higher is better:
/// a prefix or word-start hit beats a scattered one, and runs of adjacent
/// characters beat gaps.
fn score(query: &str, text: &str) -> Option<i32> {
    if let Some(at) = text.find(query) {
        let word = at == 0 || !text[..at].ends_with(char::is_alphanumeric);
        return Some(1_000 - at as i32 + if word { 500 } else { 0 });
    }
    let mut score = 0;
    let mut last: Option<usize> = None;
    let mut chars = text.char_indices();
    for q in query.chars() {
        let (at, _) = chars.by_ref().find(|(_, c)| *c == q)?;
        score += match last {
            Some(prev) if at == prev + 1 => 8,
            _ if at == 0 || !text[..at].ends_with(char::is_alphanumeric) => 6,
            _ => 1,
        };
        last = Some(at);
    }
    Some(score - text.len() as i32 / 8)
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn substring_and_word_starts_rank_first() {
        let q = "pro";
        let a = score(q, "profiles").unwrap_or_default();
        let b = score(q, "new thread in pro").unwrap_or_default();
        let c = score(q, "permissions route order").unwrap_or_default();
        assert!(a > b && b > c, "{a} {b} {c}");
        assert!(score("xyz", "profiles").is_none());
    }
}
