//! The command menu (⌘K): one search field over threads, new threads per
//! profile, every settings page and the view actions. (GPUI
//! `workspace/command.rs`.) The scrim fades in and the card rises a few
//! points into place as it mounts, and both go faster than they came when it
//! closes ([`SCRIM`], [`CARD_RISE`]); one wash glides to the selected row. Rows are
//! keyed by what they open ([`Cmd::key`]), never by position.

use rune::{Color, El, Icon, Key, KeyEvent, ScrollSpec, TextEdit, Transition, Tx, col, el, row};
use rune_kit::{Kit, TextInput};
use zlo_client::history::HistoryHit;
use zlo_client::profiles::DEFAULT_PROFILE_ID;

use super::ui::{self, XS, t};
use super::{message_search, row_title};
use crate::icons;
use crate::state::{Page, State};

const ROW_H: f32 = 36.;
/// The list's inset around its rows (each line's sides, the first line's
/// top, the last line's bottom).
const LINE_INSET: f32 = 4.;
/// The "In messages" section header above the message hits.
const HEADER_H: f32 = 28.;
/// Message hits shown: threads, and hits per thread.
const MESSAGE_THREADS: usize = 8;
const HITS_PER_THREAD: usize = 2;
const LIST_MAX_H: f32 = 380.;
const RESULTS: usize = 60;
/// How far below its place the card starts as it opens.
const CARD_RISE: f32 = 8.;

/// The scrim's presence: a plain fade (Rune's default 0.2 s in, 0.15 s out).
fn scrim_transition() -> Transition {
    Transition::opacity()
}

#[derive(Clone, PartialEq)]
enum Cmd {
    Thread(String),
    /// A history-search hit: open the thread at the message.
    Message {
        thread_id: String,
        message_id: String,
    },
    NewThread(String),
    Page(Page),
    Scheme(&'static str),
    StatusBar,
    ToggleScheme,
    Browser,
}

impl Cmd {
    /// The entry's identity, for its row's key: the same entry keeps its
    /// row (and its wash's spring) as the list filters and reorders.
    fn key(&self) -> String {
        match self {
            Cmd::Thread(id) => format!("thread:{id}"),
            Cmd::Message { message_id, .. } => format!("message:{message_id}"),
            Cmd::NewThread(profile) => format!("new:{profile}"),
            Cmd::Page(page) => format!("page:{page:?}"),
            Cmd::Scheme(value) => format!("scheme:{value}"),
            Cmd::StatusBar => "status-bar".into(),
            Cmd::ToggleScheme => "toggle-scheme".into(),
            Cmd::Browser => "browser".into(),
        }
    }
}

/// A list line's key: `cmdk-line-` plus its entry's identity.
fn line_key(e: &Entry) -> String {
    format!("cmdk-line-{}", e.cmd.key())
}

#[derive(Clone)]
struct Entry {
    cmd: Cmd,
    icon: &'static Icon,
    title: String,
    /// Right-hand hint: the kind of entry, or its shortcut.
    hint: String,
    /// Message hits draw their snippet in place of the title.
    hit: Option<HistoryHit>,
}

pub struct CommandMenu {
    pub input: TextEdit,
    pub(super) selected: usize,
    /// Bumped per open, so every open replays the entrance.
    pub(super) opened: u64,
}

/// ⌘K: open the menu, or close it when open.
pub(super) fn toggle(s: &mut State) {
    if s.workspace.command.take().is_some() {
        return;
    }
    let mut input = TextEdit::new("");
    input.focused = true;
    s.workspace.command_opens += 1;
    s.workspace.command = Some(CommandMenu {
        input,
        selected: 0,
        opened: s.workspace.command_opens,
    });
}

fn entries(s: &State) -> Vec<Entry> {
    let mut out = Vec::new();
    let list = &s.workspace.inbox.list;
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
            icon: &icons::PLUS,
            title: if several {
                format!("New thread in {name}")
            } else {
                "New thread".into()
            },
            hint: "New".into(),
            hit: None,
        });
    }
    let mut rows: Vec<_> = list.rows.iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.last_activity_at));
    for r in rows {
        let profile = list
            .profiles
            .iter()
            .find(|p| p.profile_id == r.profile_id)
            .map(|p| p.display_name.clone());
        out.push(Entry {
            cmd: Cmd::Thread(r.thread_id.clone()),
            icon: &icons::INBOX,
            title: row_title(r).to_string(),
            hint: profile.unwrap_or_else(|| "Thread".into()),
            hit: None,
        });
    }
    for (page, icon, label) in super::settings_pages(s) {
        out.push(Entry {
            cmd: Cmd::Page(page),
            icon,
            title: label.into(),
            hint: "Settings".into(),
            hit: None,
        });
    }
    for (value, icon, label) in [
        ("system", &icons::SETTINGS_2, "Theme: System"),
        ("light", &icons::SUN, "Theme: Light"),
        ("dark", &icons::MOON, "Theme: Dark"),
    ] {
        if s.prefs.scheme != value {
            out.push(Entry {
                cmd: Cmd::Scheme(value),
                icon,
                title: label.into(),
                hint: "Appearance".into(),
                hit: None,
            });
        }
    }
    out.push(Entry {
        cmd: Cmd::StatusBar,
        icon: &icons::PANEL_BOTTOM,
        title: "Toggle status bar".into(),
        hint: "⇧⌘S".into(),
        hit: None,
    });
    out.push(Entry {
        cmd: Cmd::ToggleScheme,
        icon: &icons::PALETTE,
        title: "Toggle light and dark".into(),
        hint: "⇧⌘Y".into(),
        hit: None,
    });
    out.push(Entry {
        cmd: Cmd::Browser,
        icon: &icons::GLOBE,
        title: "Open the web UI in the browser".into(),
        hint: "Action".into(),
        hit: None,
    });
    out
}

/// Entries matching the query, best first. An empty query keeps the natural
/// order: new thread, recent threads, settings, actions.
fn matches(s: &State) -> Vec<Entry> {
    let query = s
        .workspace
        .command
        .as_ref()
        .map(|m| m.input.text.trim().to_lowercase())
        .unwrap_or_default();
    let all = entries(s);
    if query.is_empty() {
        return all.into_iter().take(RESULTS).collect();
    }
    let mut scored: Vec<(i32, Entry)> = all
        .into_iter()
        .filter_map(|e| score(&query, &e.title.to_lowercase()).map(|sc| (sc, e)))
        .collect();
    scored.sort_by_key(|(sc, _)| std::cmp::Reverse(*sc));
    let mut out: Vec<Entry> = scored.into_iter().take(RESULTS).map(|(_, e)| e).collect();
    out.extend(message_entries(s, &query));
    out
}

/// The "In messages" section: snippet rows from the message search.
fn message_entries(s: &State, query: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for thread in s
        .workspace
        .message_search
        .results(query)
        .iter()
        .take(MESSAGE_THREADS)
    {
        let title = thread
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "Conversation".into());
        let hint = match message_search::origin_label(thread.origin) {
            Some(origin) => format!("{origin} · {title}"),
            None => title,
        };
        for hit in thread.hits.iter().take(HITS_PER_THREAD) {
            out.push(Entry {
                cmd: Cmd::Message {
                    thread_id: hit.thread_id.clone(),
                    message_id: hit.message_id.clone(),
                },
                icon: &icons::FILE_TEXT,
                title: hit.snippet.clone(),
                hint: hint.clone(),
                hit: Some(hit.clone()),
            });
        }
    }
    out
}

fn move_selection(s: &mut State, delta: isize, tx: &mut Tx) {
    let entries = matches(s);
    let count = entries.len();
    if let Some(menu) = s.workspace.command.as_mut()
        && count > 0
    {
        menu.selected = (menu.selected as isize + delta).rem_euclid(count as isize) as usize;
        if let Some(e) = entries.get(menu.selected) {
            tx.scroll_into_view(line_key(e));
        }
    }
}

fn run_selected(s: &mut State, tx: &mut Tx) {
    let Some(selected) = s.workspace.command.as_ref().map(|m| m.selected) else {
        return;
    };
    if let Some(entry) = matches(s).into_iter().nth(selected) {
        run(s, entry.cmd, tx);
    }
}

fn run(s: &mut State, cmd: Cmd, tx: &mut Tx) {
    s.workspace.command = None;
    match cmd {
        Cmd::Thread(id) => super::open_thread(s, id),
        Cmd::Message {
            thread_id,
            message_id,
        } => super::open_thread_at(s, thread_id, message_id),
        Cmd::NewThread(profile) => super::start_in_profile(s, profile),
        Cmd::Page(page) => super::show(s, page),
        Cmd::Scheme(value) => {
            crate::design::motion::color_morph(tx);
            s.update_prefs(|p| p.scheme = value.into());
            crate::design::look::reload(s);
        }
        Cmd::StatusBar => super::toggle_status_bar(s),
        Cmd::ToggleScheme => super::toggle_scheme(s, tx),
        Cmd::Browser => super::open_in_browser(s, tx),
    }
}

fn on_input_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    match ev.key {
        Key::Escape => s.workspace.command = None,
        Key::Up => move_selection(s, -1, tx),
        Key::Down => move_selection(s, 1, tx),
        _ => return false,
    }
    true
}

fn input_changed(s: &mut State) {
    let Some(menu) = s.workspace.command.as_mut() else {
        return;
    };
    menu.selected = 0;
    let query = menu.input.text.clone();
    message_search::search(s, &query);
}

/// The open menu, pushed into `layer`.
pub(super) fn view(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let Some(menu) = s.workspace.command.as_ref() else {
        return;
    };
    let p = &s.palette;
    let entries = matches(s);
    let typed = menu.input.text.clone();
    let indexing = !typed.trim().is_empty() && s.workspace.message_search.indexing(&typed);
    let selected = menu.selected.min(entries.len().saturating_sub(1));
    let first_message = entries.iter().position(|e| e.hit.is_some());
    let empty = entries.is_empty();

    // Every thread is an entry: the list builds only the rows near its
    // viewport (Rune virtual rows), each keyed by what it opens.
    let count = entries.len();
    let entries = std::rc::Rc::new(entries);
    let keys = entries.clone();
    // The selection is one pill gliding behind the rows (arrowing slides
    // it); virtual rows hold no other children, so it sits beside them.
    let pill = (!empty)
        .then(|| selected_top(selected, first_message))
        .and_then(|top| {
            crate::design::kit::slide_pill_sized(s, "cmdk-pill", Some(top), ROW_H).map(|pill| {
                pill.pin(Some(LINE_INSET), Some(top), Some(LINE_INSET), None)
                    .fill(p.active)
            })
        });
    let list = col()
        .key("cmdk-list")
        .w_full()
        .max_h(LIST_MAX_H)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col().key("cmdk-body").w_full().children(pill).child(
                col()
                    .key("cmdk-rows")
                    .w_full()
                    .growth(rune::GrowthSpec::default())
                    .whole_rows()
                    .virtual_rows(
                        count,
                        ROW_H,
                        move |_: &State, ix| match keys.get(ix) {
                            Some(e) => line_key(e).into(),
                            None => format!("cmdk-line-{ix}").into(),
                        },
                        move |s: &State, ix| {
                            let mut line = col().w_full().px(LINE_INSET);
                            if ix == 0 {
                                line = line.pt(LINE_INSET);
                            }
                            if ix + 1 == count {
                                line = line.pb(LINE_INSET);
                            }
                            let Some(e) = entries.get(ix) else {
                                return line;
                            };
                            if Some(ix) == first_message {
                                line = line.child(messages_header(&s.palette, indexing));
                            }
                            line.child(entry_row(&s.palette, ix, e, ix == selected))
                        },
                    ),
            ),
        );
    let input = TextInput::new(&menu.input)
        .width(None)
        .placeholder("Search threads, messages, settings and actions")
        .plain()
        .on_edit(|s: &mut State| {
            &mut s
                .workspace
                .command
                .get_or_insert_with(|| CommandMenu {
                    input: TextEdit::new(""),
                    selected: 0,
                    opened: 0,
                })
                .input
        })
        .on_change(|s: &mut State, _| input_changed(s))
        .on_key(on_input_key)
        .on_submit(run_selected)
        .build(k);
    let mut footer = row()
        .h(30.)
        .px(16.)
        .gap(16.)
        .center_y()
        .child(t("↑↓ move", XS, p.text_faint))
        .child(t("↵ open", XS, p.text_faint))
        .child(t("esc close", XS, p.text_faint));
    if indexing {
        footer = footer.child(el().grow(1.0)).child(searching_line(p));
    }
    let mut card = col()
        .w(600.)
        .clip_rounded(12.)
        .child(
            row()
                .h(52.)
                .px(16.)
                .gap(12.)
                .center_y()
                .child(ui::glyph(&icons::SEARCH, 16., p.text_muted))
                .child(el().grow(1.0).min_w(0.0).child(input)),
        )
        .child(ui::rule(p))
        .child(list);
    if empty {
        card = card.child(
            el().padding(24.)
                .child(t("Nothing matches", ui::SM, p.text_muted)),
        );
    }
    card = card.child(ui::rule(p)).child(footer);
    // The scrim and the card are separate layer entries: an element that
    // mounts with its parent rides the parent's entrance, so a card inside
    // the scrim would only fade with it, never rise on its own.
    layer.push(
        el().key(format!("cmdk-{}", menu.opened))
            .pin(Some(0.), Some(0.), Some(0.), Some(0.))
            .fill(Color::BLACK.alpha(0.22))
            .transition(scrim_transition()),
    );
    layer.push(
        row()
            .key(format!("cmdk-rise-{}", menu.opened))
            .pin(Some(0.), Some(96.), Some(0.), None)
            .justify_center()
            .transition(crate::design::motion::pop(CARD_RISE))
            .child(
                // A press outside the card (on the scrim) closes it, as Esc.
                ui::floating(p, card, 12.)
                    .key("cmdk-card")
                    .modal(true, |s: &mut State| s.workspace.command = None),
            ),
    );
}

/// Where row `ix` sits in the list's content: the top inset, the rows
/// above it, and the "In messages" header once past the first message hit.
fn selected_top(ix: usize, first_message: Option<usize>) -> f32 {
    let header = if first_message.is_some_and(|f| ix >= f) {
        HEADER_H
    } else {
        0.
    };
    LINE_INSET + ix as f32 * ROW_H + header
}

/// "Searching older chats…" in the footer, breathing on the small loaders'
/// 15 fps clock while the index catches up (still under Reduce Motion).
fn searching_line(p: &crate::design::Palette) -> El {
    t("Searching older chats…", XS, p.text_faint)
        .key("cmdk-searching")
        .cycle(crate::design::kit::loops::breathe(
            1.2,
            0.,
            &crate::design::kit::loops::BREATH_55,
        ))
}

/// "In messages" over the first message hit.
fn messages_header(p: &crate::design::Palette, indexing: bool) -> El {
    row()
        .key("cmdk-messages")
        .h(HEADER_H)
        .px(12.)
        .pb(4.)
        .items_end()
        .child(t(
            if indexing {
                "In messages · indexing older chats…"
            } else {
                "In messages"
            },
            XS,
            p.text_faint,
        ))
}

/// One entry: its glyph, its title (a message hit's snippet), its hint.
fn entry_row(p: &crate::design::Palette, ix: usize, e: &Entry, on: bool) -> El {
    let label = match &e.hit {
        Some(hit) => message_search::snippet_text(p, hit, 13.).truncate(),
        None => t(
            e.title.clone(),
            13.,
            if on { p.text } else { p.text.alpha(0.82) },
        )
        .truncate(),
    };
    let cmd = e.cmd.clone();
    row()
        .key(format!("cmdk-row-{}", e.cmd.key()))
        .role("option")
        .h(ROW_H)
        .px(12.)
        .gap(12.)
        .center_y()
        .radius(8.)
        .cursor(rune::Cursor::Pointer)
        .on_hover(move |s: &mut State| {
            if let Some(m) = s.workspace.command.as_mut()
                && m.selected != ix
            {
                m.selected = ix;
            }
        })
        .on_press_tx(move |s: &mut State, tx: &mut Tx| run(s, cmd.clone(), tx))
        .child(ui::glyph(
            e.icon,
            16.,
            if on { p.text } else { p.text_muted },
        ))
        .child(el().grow(1.0).min_w(0.0).child(label))
        .child(
            el().max_w(220.)
                .child(t(e.hint.clone(), XS, p.text_faint).truncate()),
        )
}

/// Subsequence match: every query character in order. Higher is better: a
/// prefix or word-start hit beats a scattered one, and runs of adjacent
/// characters beat gaps.
pub(super) fn score(query: &str, text: &str) -> Option<i32> {
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
