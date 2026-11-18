//! The profile menu: where a new thread starts. One launcher behind both
//! triggers (the sidebar's new-thread button and the new-thread screen's
//! profile switch): a search field that holds the keys, the profiles used
//! last on top with their age, the rest by name, and what each profile is
//! doing now. The selected row takes its profile's own color.
//!
//! Keys, all from the search field: type to filter, ↑↓ move, ↵ starts in
//! the selected profile, ⌘1..⌘9 start in a numbered row (holding ⌘ shows
//! the numbers), Escape closes. The last row always makes a new profile,
//! named after the text typed when no profile has that name.

use std::collections::BTreeMap;

use chrono::Utc;
use rune::{Color, El, Key, KeyEvent, Modifier, TextEdit, Tx, col, el, row};
use rune_kit::{Kit, TextInput, popover};
use zlo_client::inbox::{InboxProfile, InboxRow, InboxStatus};
use zlo_client::profiles::DEFAULT_PROFILE_ID;

use super::short_age;
use super::view::{branch_label, profile_color, profile_mark, pulse_dots};
use crate::design;
use crate::icons;
use crate::state::{Page, State};
use crate::workspace::command::score;
use crate::workspace::draft;
use crate::workspace::ui::{self, MEDIUM, SM, XS, t, tw};

/// The menu's width under the new-thread screen's switch.
pub(in crate::workspace) const WIDTH: f32 = 280.;
/// In the sidebar the menu is as wide as the list, down to this.
pub(in crate::workspace) const MIN_WIDTH: f32 = 224.;
/// Profiles used last, listed first.
const RECENTS: usize = 3;
/// Fewer profiles than this are one short list in the gateway's order
/// (last used first): sections would be noise.
const SECTIONS_FROM: usize = 6;
const ROW_H: f32 = 30.;
const NEW_ROW: &str = "pick-profile-new";

/// The open menu's own state; which trigger opened it lives with the
/// trigger (`Inbox::new_menu_open`, `Draft::menu_open`).
pub struct ProfilePicker {
    pub search: TextEdit,
    /// The row the keys act on: a profile, or the new-profile row after
    /// the last one.
    selected: usize,
    /// ⌘ alone is held: the first nine rows show their number.
    cmd_held: bool,
}

impl ProfilePicker {
    pub(super) fn new() -> Self {
        Self {
            search: TextEdit::new(""),
            selected: 0,
            cmd_held: false,
        }
    }

    /// Scenes: the row an arrow key would have reached.
    pub(in crate::workspace) fn fixture_select(&mut self, ix: usize) {
        self.selected = ix;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Recent,
    All,
    /// A filtered list, best match first; it has no heading.
    Matches,
}

#[derive(Clone, Debug, PartialEq)]
struct Item {
    profile: InboxProfile,
    section: Section,
}

/// What a profile's threads are doing now.
#[derive(Clone, Copy, Default)]
struct Activity {
    working: usize,
    needs: usize,
}

fn by_name(a: &InboxProfile, b: &InboxProfile) -> std::cmp::Ordering {
    a.display_name
        .to_lowercase()
        .cmp(&b.display_name.to_lowercase())
        .then_with(|| a.profile_id.cmp(&b.profile_id))
}

/// The rows in drawing order. `last` is the profile the last thread
/// started in; `query` is the search text.
fn items(profiles: &[InboxProfile], last: Option<&str>, query: &str) -> Vec<Item> {
    let query = query.trim().to_lowercase();
    let item = |profile: &InboxProfile, section| Item {
        profile: profile.clone(),
        section,
    };
    if !query.is_empty() {
        let mut hits: Vec<(i32, &InboxProfile)> = profiles
            .iter()
            .filter_map(|p| {
                let name = score(&query, &p.display_name.to_lowercase());
                // A branch counts when it holds the text as typed, behind
                // any name match: scattered letters there are noise.
                let branch = p
                    .git_branch
                    .as_deref()
                    .is_some_and(|b| b.to_lowercase().contains(&query))
                    .then_some(-1);
                name.or(branch).map(|s| (s, p))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| by_name(a.1, b.1)));
        return hits
            .into_iter()
            .map(|(_, p)| item(p, Section::Matches))
            .collect();
    }
    if profiles.len() < SECTIONS_FROM {
        return profiles.iter().map(|p| item(p, Section::All)).collect();
    }
    let is_last = |p: &InboxProfile| last == Some(p.profile_id.as_str());
    let mut recent: Vec<&InboxProfile> = profiles
        .iter()
        .filter(|p| p.last_used_at.is_some() || is_last(p))
        .collect();
    recent.sort_by(|a, b| {
        is_last(b)
            .cmp(&is_last(a))
            .then_with(|| b.last_used_at.cmp(&a.last_used_at))
            .then_with(|| by_name(a, b))
    });
    recent.truncate(RECENTS);
    let mut rest: Vec<&InboxProfile> = profiles
        .iter()
        .filter(|p| !recent.iter().any(|r| r.profile_id == p.profile_id))
        .collect();
    rest.sort_by(|a, b| by_name(a, b));
    recent
        .into_iter()
        .map(|p| item(p, Section::Recent))
        .chain(rest.into_iter().map(|p| item(p, Section::All)))
        .collect()
}

/// The open menu's rows. An account with no profile still has Default.
fn shown(s: &State) -> Vec<Item> {
    let list = &s.workspace.inbox.list;
    let query = &s.workspace.inbox.picker.search.text;
    if list.profiles.is_empty() {
        let default = [InboxProfile {
            profile_id: DEFAULT_PROFILE_ID.into(),
            display_name: "Default".into(),
            git_branch: None,
            last_used_at: None,
        }];
        return items(&default, None, query);
    }
    items(&list.profiles, list.last_used_profile_id.as_deref(), query)
}

/// The name a new profile gets: the text typed, unless a profile has it.
fn new_name(s: &State) -> Option<String> {
    let typed = s.workspace.inbox.picker.search.text.trim();
    let taken = s
        .workspace
        .inbox
        .list
        .profiles
        .iter()
        .any(|p| p.display_name.eq_ignore_ascii_case(typed));
    (!typed.is_empty() && !taken).then(|| typed.to_string())
}

fn activity(rows: &[InboxRow]) -> BTreeMap<&str, Activity> {
    let mut map: BTreeMap<&str, Activity> = BTreeMap::new();
    // A subagent's run already shows on its parent ("Subagents").
    for r in rows.iter().filter(|r| r.parent_thread_id.is_none()) {
        let a = map.entry(r.profile_id.as_str()).or_default();
        match r.status {
            Some(InboxStatus::Working | InboxStatus::Background) => a.working += 1,
            Some(InboxStatus::NeedsInput) => a.needs += 1,
            _ => {}
        }
    }
    map
}

/// Whether either trigger has the menu open.
pub(in crate::workspace) fn is_open(s: &State) -> bool {
    s.workspace.inbox.new_menu_open || s.workspace.draft.as_ref().is_some_and(|d| d.menu_open)
}

/// The sidebar's new-thread button: open the menu on the last used profile.
pub(in crate::workspace) fn toggle_new_menu(s: &mut State) {
    let open = !s.workspace.inbox.new_menu_open;
    close(s);
    if open {
        s.workspace.inbox.new_menu_open = true;
        opened(s, None);
    }
}

/// The new-thread screen's profile switch: open the menu on its profile.
pub(in crate::workspace) fn toggle_draft_menu(s: &mut State) {
    let Some((open, profile)) = s
        .workspace
        .draft
        .as_ref()
        .map(|d| (!d.menu_open, d.profile.clone()))
    else {
        return;
    };
    close(s);
    if open {
        if let Some(d) = s.workspace.draft.as_mut() {
            d.menu_open = true;
        }
        opened(s, Some(&profile));
    }
}

fn opened(s: &mut State, on: Option<&str>) {
    let picker = &mut s.workspace.inbox.picker;
    picker.search.replace_text("");
    picker.search.focused = true;
    picker.cmd_held = false;
    let at = on.and_then(|id| shown(s).iter().position(|i| i.profile.profile_id == id));
    s.workspace.inbox.picker.selected = at.unwrap_or(0);
}

/// Close the menu, whichever trigger opened it; the composer on screen
/// (the new-thread screen's or the chat's) takes the keys back. False when it was not open.
pub(in crate::workspace) fn close(s: &mut State) -> bool {
    if !is_open(s) {
        return false;
    }
    s.workspace.inbox.new_menu_open = false;
    s.workspace.inbox.picker.cmd_held = false;
    s.workspace.inbox.picker.search.focused = false;
    let on_draft = s.page == Page::Draft;
    if let Some(d) = s.workspace.draft.as_mut() {
        d.menu_open = false;
        if on_draft {
            d.input.focused = true;
        }
    }
    // Over a chat, its composer had the keys before the search took them.
    if s.page == Page::Chat
        && let Some(id) = s.chat.as_ref().map(|c| c.thread_id.clone())
    {
        crate::chat::focus_composer_of(s, &id);
    }
    true
}

/// Start in row `ix`, or make a new profile from the row after the last.
fn activate(s: &mut State, ix: usize) {
    match shown(s).into_iter().nth(ix) {
        Some(item) => draft::start_in_profile(s, item.profile.profile_id),
        None => create(s),
    }
}

fn create(s: &mut State) {
    let name = new_name(s);
    super::new_profile(s);
    if let Some(name) = name {
        crate::pages::profiles::name_new_profile(s, &name);
    }
}

fn select(s: &mut State, ix: usize) {
    let picker = &mut s.workspace.inbox.picker;
    if picker.selected != ix {
        picker.selected = ix;
    }
}

fn step(s: &mut State, delta: isize, tx: &mut Tx) {
    // The new-profile row is one more stop after the profiles.
    let profiles = shown(s).len();
    let picker = &mut s.workspace.inbox.picker;
    let at = picker.selected.min(profiles) as isize;
    picker.selected = (at + delta).rem_euclid(profiles as isize + 1) as usize;
    tx.scroll_into_view(row_key(picker.selected, profiles));
}

fn row_key(ix: usize, profiles: usize) -> String {
    if ix < profiles {
        format!("pick-profile-{ix}")
    } else {
        NEW_ROW.into()
    }
}

fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    match &ev.key {
        Key::Escape => {
            close(s);
        }
        Key::Up => step(s, -1, tx),
        Key::Down => step(s, 1, tx),
        Key::Command(c) if !(ev.mods.ctrl || ev.mods.alt || ev.mods.shift) => {
            let Some(n) = c.to_digit(10).filter(|n| (1..=9).contains(n)) else {
                return false;
            };
            // A number past the list does nothing here, and never falls
            // through to the sidebar's ⌘N under the menu.
            if (n as usize) <= shown(s).len() {
                activate(s, n as usize - 1);
            }
        }
        // Releases pass through, so the sidebar's own ⌘ state clears too.
        Key::Modifier {
            modifier: Modifier::SuperLeft | Modifier::SuperRight,
            ..
        } => {
            s.workspace.inbox.picker.cmd_held = ev.mods.cmd;
            return false;
        }
        _ => return false,
    }
    true
}

fn submit(s: &mut State, _: &mut Tx) {
    let ix = s.workspace.inbox.picker.selected;
    activate(s, ix);
}

/// The menu's card, for either trigger's popover.
pub fn profile_picker(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let picker = &s.workspace.inbox.picker;
    let entries = shown(s);
    let count = entries.len();
    let selected = picker.selected.min(count);
    let chosen = s.workspace.draft.as_ref().map(|d| d.profile.as_str());
    let doing = activity(&s.workspace.inbox.list.rows);
    let now = Utc::now().timestamp_millis();
    let heading = |label: &str, first: bool| {
        el().px(8.)
            .pt(if first { 4. } else { 10. })
            .pb(4.)
            .child(tw(label, XS, MEDIUM, p.text_faint))
    };

    let mut rows = col().key("profile-rows").w_full().padding(4.).gap(1.);
    let mut section = None;
    for (ix, item) in entries.iter().enumerate() {
        if section != Some(item.section) {
            match item.section {
                Section::Recent => rows = rows.child(heading("Recent", ix == 0)),
                // Unsectioned short lists are all `All`: no heading alone.
                Section::All if section.is_some() => {
                    rows = rows.child(heading("All profiles", ix == 0))
                }
                Section::All | Section::Matches => {}
            }
            section = Some(item.section);
        }
        let id = item.profile.profile_id.as_str();
        rows = rows.child(profile_row(
            s,
            ix,
            item,
            ix == selected,
            chosen == Some(id),
            (picker.cmd_held && ix < 9).then_some(ix),
            doing.get(id).copied().unwrap_or_default(),
            now,
        ));
    }
    if count == 0 {
        rows = rows.child(
            el().px(8.)
                .py(10.)
                .child(t("No profile matches", SM, p.text_faint)),
        );
    }

    let search = TextInput::new(&picker.search)
        .placeholder("Start a thread in…")
        .label("Start a thread in profile")
        .width(None)
        .leading_icon(&icons::SEARCH)
        .leading_icon_size(14.)
        .plain()
        .on_edit(|s: &mut State| &mut s.workspace.inbox.picker.search)
        .on_change(|s: &mut State, _| s.workspace.inbox.picker.selected = 0)
        .on_key(on_key)
        .on_submit(submit)
        .build(k)
        .key("profile-search");

    ui::floating(
        p,
        col()
            .key("profile-picker")
            .role("dialog")
            .label("Start in profile")
            // A press on a heading or the card's padding leaves the keys
            // with the search field.
            .preserve_focus_on_press(true)
            .w_full()
            .min_h(0.0)
            .child(el().px(4.).py(4.).child(search))
            .child(ui::rule(p))
            .child(popover::fit_scroll(rows))
            .child(ui::rule(p))
            .child(el().padding(4.).child(new_row(s, selected == count))),
        design::PANEL_RADIUS,
    )
}

#[allow(clippy::too_many_arguments)]
fn profile_row(
    s: &State,
    ix: usize,
    item: &Item,
    on: bool,
    chosen: bool,
    badge: Option<usize>,
    doing: Activity,
    now: i64,
) -> El {
    let p = &s.palette;
    let profile = &item.profile;
    let id = profile.profile_id.clone();
    // The selection wears the profile's color, the mark at full strength.
    let wash = profile_color(p, &profile.profile_id, 1.).alpha(if p.light { 0.1 } else { 0.16 });
    let mut r = row()
        .key(format!("pick-profile-{ix}"))
        .role("button")
        .label(format!("Start in {}", profile.display_name))
        .w_full()
        .h(ROW_H)
        .px(8.)
        .gap(8.)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .cursor(rune::Cursor::Pointer)
        .fill(if on { wash } else { Color::TRANSPARENT })
        .on_hover(move |s: &mut State| select(s, ix))
        .child(profile_mark(
            p,
            &profile.profile_id,
            &profile.display_name,
            if on { 1. } else { 0. },
        ))
        .child(
            el().grow(1.0).min_w(0.0).child(
                t(
                    profile.display_name.clone(),
                    SM,
                    if on { p.text } else { p.text_muted },
                )
                .truncate(),
            ),
        );
    let live = doing.needs > 0 || doing.working > 0;
    // A live status takes the branch's place: the row is one line.
    if let Some(branch) = profile.git_branch.as_deref().filter(|_| !live) {
        r = r.child(branch_label(p, branch));
    }
    if let Some(ix) = badge {
        r = r.child(ui::shortcut_badge(p, ix));
    } else if doing.needs > 0 {
        r = r.child(
            row()
                .gap(4.)
                .center_y()
                .child(ui::dot(p.warning, 6.))
                .child(t("Needs you", XS, p.warning)),
        );
    } else if doing.working > 0 {
        let label = match doing.working {
            1 => "Working".to_string(),
            n => format!("{n} working"),
        };
        r = r.child(
            row()
                .gap(4.)
                .center_y()
                .child(pulse_dots(p.accent))
                .child(t(label, XS, p.accent)),
        );
    } else if let Some(at) = profile
        .last_used_at
        .filter(|_| item.section == Section::Recent)
    {
        r = r.child(t(short_age(at, now), XS, p.text_faint));
    }
    if chosen {
        r = r.child(ui::glyph(&icons::CHECK, 14., p.text_muted));
    }
    r.on_press(move |s: &mut State| draft::start_in_profile(s, id.clone()))
}

/// The last row: a new profile, named after the search text when no
/// profile has that name. It carries the menu's key hints.
fn new_row(s: &State, on: bool) -> El {
    let p = &s.palette;
    let label = match new_name(s) {
        Some(name) => format!("New profile “{name}”"),
        None => "New profile".into(),
    };
    let count = shown(s).len();
    let ink = if on { p.text } else { p.text_muted };
    row()
        .key(NEW_ROW)
        .role("button")
        .label(label.clone())
        .w_full()
        .h(ROW_H)
        .px(8.)
        .gap(8.)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .cursor(rune::Cursor::Pointer)
        .fill(if on {
            ui::selected_wash(p)
        } else {
            Color::TRANSPARENT
        })
        .on_hover(move |s: &mut State| select(s, count))
        .child(
            row()
                .size(16., 16.)
                .radius(4.)
                .border(1., p.border_strong)
                .center()
                .child(ui::glyph(&icons::PLUS, 10., ink)),
        )
        .child(
            el().grow(1.0)
                .min_w(0.0)
                .child(t(label, SM, ink).truncate()),
        )
        .child(t("↑↓  ↵", XS, p.text_faint))
        .on_press(create)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::Mods;

    fn profile(id: &str, name: &str, used: Option<i64>) -> InboxProfile {
        InboxProfile {
            profile_id: id.into(),
            display_name: name.into(),
            git_branch: (id == "verpin").then(|| "release/train".into()),
            last_used_at: used,
        }
    }

    fn many() -> Vec<InboxProfile> {
        vec![
            profile("default", "Default", Some(900)),
            profile("thesis", "Thesis", None),
            profile("avallon", "Avallon Case", Some(100)),
            profile("verpin", "Verpin", Some(500)),
            profile("flat", "Flat Finder", None),
            profile("builder", "IronClaw Builder", Some(700)),
            profile("cyber", "Cyberzen", None),
        ]
    }

    fn ids(items: &[Item]) -> Vec<&str> {
        items
            .iter()
            .map(|i| i.profile.profile_id.as_str())
            .collect()
    }

    fn key(key: Key) -> KeyEvent {
        KeyEvent {
            key,
            mods: Mods::default(),
        }
    }

    fn state() -> State {
        let mut s = crate::chat::fixtures::test_state();
        s.workspace.inbox.list.profiles = many();
        s.workspace.inbox.list.last_used_profile_id = Some("default".into());
        s
    }

    #[test]
    fn recent_profiles_lead_and_the_rest_go_by_name() {
        let list = items(&many(), Some("default"), "");
        assert_eq!(
            ids(&list),
            [
                "default", "builder", "verpin", "avallon", "cyber", "flat", "thesis"
            ]
        );
        assert!(list[..3].iter().all(|i| i.section == Section::Recent));
        assert!(list[3..].iter().all(|i| i.section == Section::All));
        // The last used profile leads even before its time is recorded.
        let list = items(&many(), Some("thesis"), "");
        assert_eq!(ids(&list)[..3], ["thesis", "default", "builder"]);
        // A short list keeps the gateway's order, unsectioned.
        let few = &many()[..3];
        let list = items(few, Some("default"), "");
        assert_eq!(ids(&list), ["default", "thesis", "avallon"]);
        assert!(list.iter().all(|i| i.section == Section::All));
    }

    #[test]
    fn typing_filters_by_name_then_branch() {
        // The closest name leads: "FLat finder" before "deFauLt".
        let list = items(&many(), None, " fl ");
        assert_eq!(ids(&list), ["flat", "default"]);
        // Scattered letters match a name ("Ironclaw BuildeR")...
        assert_eq!(ids(&items(&many(), None, "icbr")), ["builder"]);
        // ...a branch only as typed, and behind a name that matches.
        assert_eq!(ids(&items(&many(), None, "train")), ["verpin"]);
        assert_eq!(ids(&items(&many(), None, "rls")), [""; 0]);
        assert_eq!(
            ids(&items(&many(), None, "re")),
            ["cyber", "builder", "verpin"]
        );
        assert!(items(&many(), None, "zzz").is_empty());
    }

    /// Through the search field's keys: arrows walk the rows and wrap over
    /// the new-profile row, Enter starts in the selected profile.
    #[test]
    fn the_keys_pick_a_profile() {
        let mut s = state();
        let mut tx = Tx::default();
        toggle_new_menu(&mut s);
        assert!(is_open(&s) && s.workspace.inbox.picker.search.focused);
        assert_eq!(s.workspace.inbox.picker.selected, 0);
        assert!(on_key(&mut s, &key(Key::Up), &mut tx));
        assert_eq!(s.workspace.inbox.picker.selected, many().len());
        assert!(on_key(&mut s, &key(Key::Down), &mut tx));
        assert!(on_key(&mut s, &key(Key::Down), &mut tx));
        assert_eq!(s.workspace.inbox.picker.selected, 1);
        submit(&mut s, &mut tx);
        assert!(!is_open(&s));
        let draft = s.workspace.draft.as_ref().expect("draft");
        assert_eq!(draft.profile, "builder");
        assert!(draft.input.focused, "the composer takes the keys back");

        // The screen's own switch opens on its profile; ⌘3 picks row three.
        toggle_draft_menu(&mut s);
        assert_eq!(s.workspace.inbox.picker.selected, 1);
        let cmd = KeyEvent {
            key: Key::Command('3'),
            mods: Mods {
                cmd: true,
                ..Mods::default()
            },
        };
        assert!(on_key(&mut s, &cmd, &mut tx));
        assert_eq!(s.workspace.draft.as_ref().expect("draft").profile, "verpin");

        // Over a chat, closing hands the keys back to its composer.
        let mut c = state();
        c.page = Page::Chat;
        let mut chat = crate::chat::ChatState::new("t1".into());
        // Rune blurs the composer when the search field takes focus.
        chat.composer.focused = false;
        c.chat = Some(chat);
        toggle_new_menu(&mut c);
        assert!(on_key(&mut c, &key(Key::Escape), &mut tx));
        assert!(!c.workspace.inbox.picker.search.focused);
        assert!(c.chat.as_ref().expect("chat").composer.focused);

        // Escape closes without changing the profile.
        toggle_draft_menu(&mut s);
        assert!(on_key(&mut s, &key(Key::Escape), &mut tx));
        assert!(!is_open(&s));
        assert_eq!(s.workspace.draft.as_ref().expect("draft").profile, "verpin");
    }

    /// Typing narrows the list and Enter takes the best match; a name no
    /// profile has is offered as a new profile, carried into the editor.
    #[test]
    fn typing_starts_in_the_match_or_names_a_new_profile() {
        let mut s = state();
        let mut tx = Tx::default();
        toggle_new_menu(&mut s);
        s.workspace.inbox.picker.search.replace_text("thes");
        assert_eq!(new_name(&s).as_deref(), Some("thes"));
        submit(&mut s, &mut tx);
        assert_eq!(s.workspace.draft.as_ref().expect("draft").profile, "thesis");

        toggle_new_menu(&mut s);
        assert_eq!(s.workspace.inbox.picker.search.text, "", "opens clean");
        s.workspace.inbox.picker.search.replace_text("thesis");
        assert_eq!(new_name(&s), None, "the name is taken");
        s.workspace.inbox.picker.search.replace_text("Garden");
        assert!(shown(&s).is_empty());
        // Pages open only with a gateway, or in a scene.
        s.fixture = true;
        submit(&mut s, &mut tx);
        assert!(!is_open(&s));
        assert_eq!(s.page, Page::Profiles);
        assert_eq!(crate::pages::profiles::new_profile_name(&s), "Garden");
    }

    #[test]
    fn a_profile_shows_what_its_threads_are_doing() {
        use crate::workspace::inbox::tests::row;
        use zlo_client::inbox::InboxState;
        let mut working = row("a", InboxState::Working, None);
        working.status = Some(InboxStatus::Working);
        let mut child = row("b", InboxState::Working, None);
        child.status = Some(InboxStatus::Working);
        child.parent_thread_id = Some("a".into());
        let mut needs = row("c", InboxState::Inbox, None);
        needs.status = Some(InboxStatus::NeedsInput);
        needs.profile_id = "work".into();
        let rows = [working, child, needs];
        let doing = activity(&rows);
        assert_eq!(doing["default"].working, 1, "a subagent is not counted");
        assert_eq!(doing["work"].needs, 1);
    }

    /// The sidebar's menu stays inside the sidebar: it hangs under the
    /// header at the list's width instead of spilling over the chat.
    #[test]
    fn the_sidebar_menu_stays_in_the_sidebar() {
        use rune::{Env, Runtime};
        let mut s = crate::chat::fixtures::test_state();
        crate::workspace::fixtures::fixture_profile_menu(&mut s);
        let mut rt = Runtime::new(s, crate::view, (1200., 800.), Env::new(true, 1.));
        let mut t = 0.;
        for _ in 0..30 {
            t += 1. / 60.;
            rt.frame(t);
        }
        let (menu, _) = rt.presented("profile-picker").expect("menu drawn");
        let (header, _) = rt.presented("inbox-header").expect("header drawn");
        assert!(menu[1] >= header[1] + header[3], "below the header");
        assert!(
            menu[0] >= header[0] && menu[0] + menu[2] <= header[0] + header[2],
            "menu {menu:?} leaves the sidebar {header:?}"
        );
        assert!(rt.presented(NEW_ROW).is_some(), "the last row is on screen");
    }
}
