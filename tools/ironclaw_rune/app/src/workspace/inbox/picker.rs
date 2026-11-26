//! Picking where a new thread starts, as t3code picks a project.
//!
//! - The sidebar's new-thread button (and ⌘N, ⇧⌘O) starts at once in the
//!   current profile when there is nothing to choose: Shift held (⇧⌘N), the
//!   sidebar filtered to one profile, or a single profile. Otherwise it
//!   opens ⌘K on "New thread in…" (`command::open_new_thread_in`).
//! - The new-thread screen's heading names the profile ("What should we work
//!   on in IronClaw?"); the name opens this module's combobox: a search
//!   field, the profiles, and "Add profile" last.
//!
//! One order everywhere ([`ordered`]): the current profile, then pinned
//! ones in pin order, then the rest by last use. It is frozen while a list
//! is open, so pinning a row never moves it under the pointer; the new order
//! shows on the next open. Search filters in place and keeps that order.
//! A pinned profile always shows its pin; an unpinned one only on the
//! highlighted row. ⇧⌘P pins the highlighted row from the keyboard.
//!
//! "Add profile" asks for a folder and makes a profile working in it, named
//! after the folder (t3code's Add project); its persona stays editable in
//! Settings → Profiles.

use std::path::Path;

use rune::{
    Color, El, FileDialogOptions, Key, KeyEvent, Span, TextEdit, TextStyle, Tx, col, el, rich, row,
};
use rune_kit::{Kit, popover};
use zlo_client::inbox::InboxProfile;
use zlo_client::profiles::{DEFAULT_PROFILE_ID, ProfileSettings};

use super::view::{profile_color, profile_mark};
use crate::design;
use crate::icons;
use crate::state::{Page, State};
use crate::workspace::draft;
use crate::workspace::ui::{self, REGULAR, SEMIBOLD, SM, t};

/// The combobox's width (t3code's `w-72`).
pub(in crate::workspace) const WIDTH: f32 = 288.;
const ROW_H: f32 = 30.;
/// The list's padding and the gap between its rows.
const ROWS_PAD: f32 = 4.;
const ROWS_GAP: f32 = 1.;
/// The list scrolls past this (about nine rows).
const LIST_MAX_H: f32 = 296.;
const ADD_ROW: &str = "pick-profile-add";

/// The open combobox's own state; the draft says whether it is open
/// (`Draft::menu_open`).
pub struct ProfilePicker {
    pub search: TextEdit,
    /// The highlighted row: a profile, or "Add profile" after the last.
    selected: usize,
    /// The order frozen when the list opened (profile ids).
    order: Vec<String>,
}

impl ProfilePicker {
    pub(super) fn new() -> Self {
        Self {
            search: TextEdit::new(""),
            selected: 0,
            order: Vec::new(),
        }
    }

    /// Scenes: the row an arrow key would have reached.
    pub(in crate::workspace) fn fixture_select(&mut self, ix: usize) {
        self.selected = ix;
    }
}

// ---------------------------------------------------------------------------
// The one order, the current profile, pins.

/// The focused computer's profiles; before the first read, a stand-in
/// Default so the pickers are never empty.
fn profiles(s: &State) -> Vec<InboxProfile> {
    let list = &s.workspace.inbox.list.profiles;
    if list.is_empty() {
        return vec![InboxProfile {
            profile_id: DEFAULT_PROFILE_ID.into(),
            display_name: "Default".into(),
            git_branch: None,
            last_used_at: None,
            folder: None,
        }];
    }
    list.clone()
}

/// The profile "here": the new-thread screen's, the open thread's, the
/// sidebar's profile filter, then the profile last used.
pub(in crate::workspace) fn current_profile(s: &State) -> String {
    let known = |id: &str| profiles(s).iter().any(|p| p.profile_id == id);
    let on_draft = (s.page == Page::Draft)
        .then(|| s.workspace.draft.as_ref().map(|d| d.profile.clone()))
        .flatten();
    let in_chat = (s.page == Page::Chat)
        .then(|| {
            let id = &s.chat.as_ref()?.thread_id;
            s.workspace
                .inbox
                .row(id)
                .map(|r| r.profile_id.clone())
                // Just started here: the inbox may not list it yet.
                .or_else(|| {
                    let (started, profile) = s.workspace.started.as_ref()?;
                    (started == id).then(|| profile.clone())
                })
        })
        .flatten();
    let list = &s.workspace.inbox.list;
    [
        on_draft,
        in_chat,
        s.workspace.inbox.profile.clone(),
        list.last_used_profile_id.clone(),
    ]
    .into_iter()
    .flatten()
    .find(|id| known(id))
    .or_else(|| profiles(s).first().map(|p| p.profile_id.clone()))
    .unwrap_or_else(|| DEFAULT_PROFILE_ID.into())
}

/// A pin names its computer too (`this-mac/work`): two computers can each
/// have an unrelated `work` profile.
fn pin_key(s: &State, id: &str) -> String {
    format!("{}/{id}", s.onboarding.computer.key())
}

/// The focused computer's pinned profile ids, in pin order.
fn pinned_here(s: &State) -> Vec<String> {
    let prefix = format!("{}/", s.onboarding.computer.key());
    s.prefs
        .pinned_profiles
        .iter()
        .filter_map(|p| p.strip_prefix(&prefix).map(str::to_string))
        .collect()
}

pub(in crate::workspace) fn is_pinned(s: &State, id: &str) -> bool {
    let key = pin_key(s, id);
    s.prefs.pinned_profiles.contains(&key)
}

/// Pin `id`, or unpin it when pinned (Prefs, saved with them).
pub(in crate::workspace) fn toggle_pin(s: &mut State, id: &str) {
    let id = pin_key(s, id);
    s.update_prefs(move |p| {
        if let Some(at) = p.pinned_profiles.iter().position(|x| *x == id) {
            p.pinned_profiles.remove(at);
        } else {
            p.pinned_profiles.push(id);
        }
    });
}

/// t3code's order: `current` first, then pinned profiles in pin order, then
/// the rest by last use (never used last, by name).
pub(in crate::workspace) fn rank(
    mut all: Vec<InboxProfile>,
    current: &str,
    pinned: &[String],
) -> Vec<InboxProfile> {
    let pin_at = |id: &str| pinned.iter().position(|p| p == id);
    all.sort_by(|a, b| {
        let lead = |p: &InboxProfile| p.profile_id != current;
        lead(a)
            .cmp(&lead(b))
            .then_with(|| match (pin_at(&a.profile_id), pin_at(&b.profile_id)) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| b.last_used_at.cmp(&a.last_used_at))
            .then_with(|| {
                a.display_name
                    .to_lowercase()
                    .cmp(&b.display_name.to_lowercase())
            })
            .then_with(|| a.profile_id.cmp(&b.profile_id))
    });
    all
}

/// The live order (for opening a list).
pub(in crate::workspace) fn ordered(s: &State) -> Vec<InboxProfile> {
    rank(profiles(s), &current_profile(s), &pinned_here(s))
}

/// `all` in a frozen `order`; profiles that appeared since go last, in
/// their live order.
pub(in crate::workspace) fn in_order(
    all: Vec<InboxProfile>,
    order: &[String],
) -> Vec<InboxProfile> {
    let at = |p: &InboxProfile| {
        order
            .iter()
            .position(|id| *id == p.profile_id)
            .unwrap_or(usize::MAX)
    };
    let mut all = all;
    all.sort_by_key(at);
    all
}

/// Whether `query` matches a profile: held in its name, its folder (as
/// shown) or its branch. Plain containment, as t3code's combobox filter: a
/// scattered subsequence through a long path matches far too much.
pub(in crate::workspace) fn matches(p: &InboxProfile, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let hit = |text: &str| text.to_lowercase().contains(&query);
    hit(&p.display_name)
        || p.folder.as_deref().is_some_and(|f| hit(&short_folder(f)))
        || p.git_branch.as_deref().is_some_and(hit)
}

/// `~/ironclaw` rather than the full home path.
pub(in crate::workspace) fn short_folder(path: &str) -> String {
    let p = Path::new(path);
    match dirs::home_dir().and_then(|h| p.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Starting a thread.

/// The sidebar's new-thread button, ⌘N and ⇧⌘O; `here` is Shift (⇧⌘N):
/// start in the current profile. Without it the thread starts at once when
/// there is nothing to choose (the sidebar filtered to one profile, or a
/// single profile), else ⌘K opens on "New thread in…".
pub(in crate::workspace) fn new_thread(s: &mut State, here: bool) {
    // New threads start on another computer: go there first; its draft
    // screen offers that computer's profiles. Not for Shift (⇧⌘N): "the
    // current profile" is this computer's.
    if !here && draft::runs_on::default_elsewhere(s) {
        close(s);
        draft::open(s);
        return;
    }
    if let Some(filtered) = s.workspace.inbox.profile.clone()
        && !here
        && profiles(s).iter().any(|p| p.profile_id == filtered)
    {
        draft::start_in_profile(s, filtered);
        return;
    }
    if here || profiles(s).len() <= 1 {
        let id = current_profile(s);
        draft::start_in_profile(s, id);
        return;
    }
    close(s);
    crate::workspace::command::open_new_thread_in(s);
}

/// "Add profile" picks a folder with this Mac's dialog, so it is offered
/// only while the backend in focus runs on this Mac (focused and on
/// loopback: an `IRONCLAW_DESKTOP_URL` elsewhere is labelled This Mac too).
/// A remote computer would get a path that exists only here; its profiles
/// are made in Settings → Profiles there.
pub(in crate::workspace) fn can_add_profile(s: &State) -> bool {
    s.onboarding.computer.key() == zlo_client::environments::THIS_MAC
        && crate::cli_install::backend_on_this_mac(s)
}

/// Pick a folder, then make a profile working in it (named after the
/// folder) and start a thread there.
pub(in crate::workspace) fn add_profile(s: &mut State, tx: &mut Tx) {
    close(s);
    s.workspace.command = None;
    if !can_add_profile(s) {
        return;
    }
    let options = FileDialogOptions {
        title: "Add a profile for this folder".into(),
        ..Default::default()
    };
    tx.open_folder(options, |s: &mut State, result, _| match result {
        // The dialog is modal, but the focus can still move while it is
        // open (a peer handover): the folder belongs to This Mac only.
        Ok(Some(_)) if !can_add_profile(s) => {
            add_failed(s, "the focused computer changed".into());
        }
        Ok(Some(folder)) => create_for_folder(s, &folder),
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "folder dialog failed");
            add_failed(s, e.to_string());
        }
    });
}

/// How many ids "Add profile" tries before giving up.
const ID_ATTEMPTS: usize = 50;

/// The `n`th id to try for `base` (`base`, `base-2`, `base-3`, …).
fn nth_id(base: &str, n: usize) -> String {
    if n == 0 {
        base.to_string()
    } else {
        format!("{base}-{}", n + 1)
    }
}

/// A profile id from a folder name: lowercase letters, digits, `-` and `_`,
/// starting with a letter or digit, unique among `taken`.
pub(in crate::workspace) fn profile_id_for(name: &str, taken: &[String]) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches(|c| c == '-' || c == '_');
    let base: String = if slug.is_empty() {
        "project".into()
    } else {
        slug.chars().take(56).collect()
    };
    let free = |id: &str| !taken.iter().any(|t| t == id);
    if free(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| free(id))
        .unwrap_or(base)
}

fn create_for_folder(s: &mut State, folder: &Path) {
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "Project".into());
    // An existing profile on this folder is the one meant.
    let path = folder.to_string_lossy().into_owned();
    if let Some(existing) = s
        .workspace
        .inbox
        .list
        .profiles
        .iter()
        .find(|p| p.folder.as_deref() == Some(path.as_str()))
    {
        let id = existing.profile_id.clone();
        draft::start_in_profile(s, id);
        return;
    }
    let mut taken: Vec<String> = s
        .workspace
        .inbox
        .list
        .profiles
        .iter()
        .map(|p| p.profile_id.clone())
        .collect();
    taken.extend(crate::pages::profiles::known_ids(s));
    let base = profile_id_for(&name, &taken);
    let settings = ProfileSettings {
        display_name: name,
        working_folder: Some(path),
        ..Default::default()
    };
    crate::jobs::run(
        s,
        "profiles",
        // Ids this app has not seen can still be taken (archived profiles,
        // another client): a fresh create answers `profile_exists` for
        // those, and the next suffix is tried.
        move |c| async move {
            let mut last = None;
            for n in 0..ID_ATTEMPTS {
                let id = nth_id(&base, n);
                if n > 0 && taken.contains(&id) {
                    continue;
                }
                match c.create_new_profile(&id, settings.clone()).await {
                    Err(e) if e.code() == Some("profile_exists") => last = Some(e),
                    other => return other,
                }
            }
            Err(last
                .unwrap_or_else(|| zlo_client::ClientError::Config("no free profile id".into())))
        },
        |s, result, tx| {
            tx.animated();
            match result {
                Ok(saved) => {
                    crate::workspace::profiles_changed(
                        s,
                        crate::workspace::ProfileChange::Saved(&saved),
                    );
                    if !s
                        .workspace
                        .inbox
                        .list
                        .profiles
                        .iter()
                        .any(|p| p.profile_id == saved.profile_id)
                    {
                        // Before the first inbox read the list holds no
                        // stand-in to replace: the new profile joins it.
                        s.workspace.inbox.list.profiles.push(InboxProfile {
                            profile_id: saved.profile_id.clone(),
                            display_name: saved.settings.display_name.clone(),
                            git_branch: saved.git_branch.clone(),
                            last_used_at: saved.last_used_at,
                            folder: Some(saved.effective_folder.clone()),
                        });
                    }
                    crate::pages::profiles::remember_saved(s, &saved);
                    draft::start_in_profile(s, saved.profile_id.clone());
                }
                Err(e) => add_failed(s, e.to_string()),
            }
        },
    );
}

/// The new-thread screen says why no profile was added.
fn add_failed(s: &mut State, error: String) {
    draft::open(s);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.error = Some(format!("Could not add the profile: {error}"));
    }
}

// ---------------------------------------------------------------------------
// The combobox (the new-thread screen's heading).

/// Whether the combobox is open.
pub(in crate::workspace) fn is_open(s: &State) -> bool {
    s.workspace.draft.as_ref().is_some_and(|d| d.menu_open)
}

/// The heading's profile name: open the combobox, or close it.
pub(in crate::workspace) fn toggle_draft_menu(s: &mut State) {
    let Some(open) = s.workspace.draft.as_ref().map(|d| !d.menu_open) else {
        return;
    };
    close(s);
    if open {
        let order = ordered(s).into_iter().map(|p| p.profile_id).collect();
        if let Some(d) = s.workspace.draft.as_mut() {
            d.menu_open = true;
        }
        let picker = &mut s.workspace.inbox.picker;
        picker.search.replace_text("");
        picker.search.focused = true;
        picker.selected = 0;
        picker.order = order;
    }
}

/// Close the combobox; the new-thread composer takes the keys back. False
/// when it was not open.
pub(in crate::workspace) fn close(s: &mut State) -> bool {
    if !is_open(s) {
        return false;
    }
    s.workspace.inbox.picker.search.focused = false;
    let on_draft = s.page == Page::Draft;
    if let Some(d) = s.workspace.draft.as_mut() {
        d.menu_open = false;
        if on_draft {
            d.input.focused = true;
        }
    }
    true
}

/// The rows on screen: the frozen order, filtered by the search.
fn shown(s: &State) -> Vec<InboxProfile> {
    let picker = &s.workspace.inbox.picker;
    in_order(profiles(s), &picker.order)
        .into_iter()
        .filter(|p| matches(p, &picker.search.text))
        .collect()
}

/// "Add profile" is offered while nothing is typed (t3code filters it out).
fn offers_add(s: &State) -> bool {
    s.workspace.inbox.picker.search.text.trim().is_empty() && can_add_profile(s)
}

/// Pick row `ix`: a profile (moving the draft there), or "Add profile".
/// `ix` is clamped as the highlight is drawn, so a list that shrank under
/// it acts on the row shown highlighted.
fn activate(s: &mut State, ix: usize, tx: &mut Tx) {
    let ix = ix.min(row_count(s).saturating_sub(1));
    match shown(s).into_iter().nth(ix) {
        Some(profile) => {
            let same = s
                .workspace
                .draft
                .as_ref()
                .is_some_and(|d| d.profile == profile.profile_id);
            if same {
                close(s);
            } else {
                draft::start_in_profile(s, profile.profile_id);
            }
        }
        None if offers_add(s) && ix == shown(s).len() => add_profile(s, tx),
        None => {}
    }
}

fn select(s: &mut State, ix: usize) {
    let picker = &mut s.workspace.inbox.picker;
    if picker.selected != ix {
        picker.selected = ix;
    }
}

fn row_count(s: &State) -> usize {
    shown(s).len() + usize::from(offers_add(s))
}

fn step(s: &mut State, delta: isize, tx: &mut Tx) {
    let count = row_count(s);
    if count == 0 {
        return;
    }
    let picker = &mut s.workspace.inbox.picker;
    let at = picker.selected.min(count - 1) as isize;
    picker.selected = (at + delta).rem_euclid(count as isize) as usize;
    let key = row_key(&shown(s), s.workspace.inbox.picker.selected);
    tx.scroll_into_view(key);
}

fn row_key(rows: &[InboxProfile], ix: usize) -> String {
    match rows.get(ix) {
        Some(p) => profile_key(&p.profile_id),
        None => ADD_ROW.into(),
    }
}

fn profile_key(profile_id: &str) -> String {
    format!("pick-profile-id-{profile_id}")
}

/// ⇧⌘P: pin or unpin the highlighted profile.
pub(in crate::workspace) fn is_pin_chord(ev: &KeyEvent) -> bool {
    ev.mods.shift
        && !ev.mods.alt
        && !crate::system::extra_ctrl(ev.mods)
        && matches!(&ev.key, Key::Command(c) if c.eq_ignore_ascii_case(&'p'))
}

fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    match &ev.key {
        Key::Escape => {
            close(s);
        }
        Key::Up => step(s, -1, tx),
        Key::Down => step(s, 1, tx),
        _ if is_pin_chord(ev) => {
            let ix = s
                .workspace
                .inbox
                .picker
                .selected
                .min(row_count(s).saturating_sub(1));
            if let Some(p) = shown(s).get(ix) {
                toggle_pin(s, &p.profile_id.clone());
            }
        }
        _ => return false,
    }
    true
}

fn submit(s: &mut State, tx: &mut Tx) {
    let ix = s.workspace.inbox.picker.selected;
    activate(s, ix, tx);
}

/// Byte ranges of `name` to bold for `query`: where it holds the query.
fn match_ranges(name: &str, query: &str) -> Vec<(usize, usize)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = name.char_indices().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|(_, c)| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let q: Vec<char> = q.chars().collect();
    let Some(start) = lower.windows(q.len()).position(|w| w == q.as_slice()) else {
        return Vec::new();
    };
    let last = start + q.len() - 1;
    vec![(chars[start].0, chars[last].0 + chars[last].1.len_utf8())]
}

fn highlighted_name(p: &design::Palette, name: &str, query: &str, on: bool) -> El {
    let base = if on { p.text } else { p.text.alpha(0.82) };
    let ranges = match_ranges(name, query);
    if ranges.is_empty() {
        return t(name.to_string(), SM, base).truncate();
    }
    let spans: Vec<Span> = ranges
        .into_iter()
        .map(|(a, b)| Span::new(a, b).weight(SEMIBOLD).color(p.text))
        .collect();
    rich(name.to_string(), spans)
        .font(TextStyle::new(crate::design::styles::rem(SM), REGULAR))
        .color(base)
        .truncate()
}

/// The pin toggle on a row: always there when pinned, on the highlighted
/// row otherwise.
pub(in crate::workspace) fn pin_button(
    p: &design::Palette,
    key: String,
    id: &str,
    name: &str,
    pinned: bool,
    on: bool,
) -> El {
    let id = id.to_string();
    let help = if pinned {
        format!("Unpin {name} (⇧⌘P)")
    } else {
        format!("Pin {name} (⇧⌘P)")
    };
    row()
        .key(key)
        .role("button")
        .label(help.clone())
        .help(help)
        .size(22., 22.)
        .center()
        .radius(design::CONTROL_RADIUS)
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .opacity(if pinned || on { 1. } else { 0. })
        .child(ui::glyph(
            &icons::PIN,
            12.,
            if pinned { p.text_muted } else { p.text_faint },
        ))
        .on_press(move |s: &mut State| toggle_pin(s, &id))
}

/// The combobox card.
pub fn profile_picker(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let picker = &s.workspace.inbox.picker;
    let entries = shown(s);
    let add = offers_add(s);
    let count = entries.len() + usize::from(add);
    let selected = picker.selected.min(count.saturating_sub(1));
    let query = picker.search.text.as_str();

    let mut rows = col()
        .key("profile-rows")
        .w_full()
        .padding(ROWS_PAD)
        .gap(ROWS_GAP);
    // The highlight is one pill gliding behind the rows in the row's
    // profile colour (it morphs as it moves); on "Add profile" it is plain.
    if count > 0 {
        let wash = match entries.get(selected) {
            Some(profile) => profile_wash(p, &profile.profile_id),
            None => ui::selected_wash(p),
        };
        let top = ROWS_PAD + selected as f32 * (ROW_H + ROWS_GAP);
        rows = rows.children(
            design::kit::slide_pill_sized(s, "profile-pill", Some(top), ROW_H).map(|pill| {
                pill.pin(Some(ROWS_PAD), Some(top), Some(ROWS_PAD), None)
                    .radius(design::CONTROL_RADIUS)
                    .fill(wash)
            }),
        );
    }
    for (ix, profile) in entries.iter().enumerate() {
        rows = rows.child(profile_row(s, ix, profile, query, ix == selected));
    }
    if add {
        rows = rows.child(add_row(s, entries.len(), selected == entries.len()));
    }
    if count == 0 {
        rows = rows.child(
            el().px(8.)
                .py(10.)
                .child(t("No matching profiles.", SM, p.text_faint)),
        );
    }

    let card = col()
        .key("profile-picker")
        .role("dialog")
        .label("Choose a profile")
        // A press on padding leaves the keys with the search field.
        .preserve_focus_on_press(true)
        .w_full()
        .min_h(0.0)
        .child(search_field(s, k))
        .child(ui::rule(p))
        .child(popover::fit_scroll(rows).max_h(LIST_MAX_H));
    ui::floating(p, card, design::PANEL_RADIUS)
}

/// The highlight's wash: the profile's own colour, lightly.
fn profile_wash(p: &design::Palette, profile_id: &str) -> Color {
    profile_color(p, profile_id, 1.).alpha(if p.light { 0.1 } else { 0.16 })
}

fn profile_row(s: &State, ix: usize, profile: &InboxProfile, query: &str, on: bool) -> El {
    let p = &s.palette;
    let id = profile.profile_id.clone();
    let pinned = is_pinned(s, &id);
    let mut r = row()
        .key(profile_key(&id))
        .role("option")
        .label(profile.display_name.clone())
        .w_full()
        .h(ROW_H)
        .pl(8.)
        .pr(4.)
        .gap(8.)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .cursor(rune::Cursor::Pointer)
        .on_hover(move |s: &mut State| select(s, ix))
        .child(profile_mark(
            p,
            &id,
            &profile.display_name,
            if on { 1. } else { 0. },
        ))
        .child(el().grow(1.0).min_w(0.0).child(highlighted_name(
            p,
            &profile.display_name,
            query,
            on,
        )))
        .child(pin_button(
            p,
            format!("pick-profile-pin-{id}"),
            &id,
            &profile.display_name,
            pinned,
            on,
        ));
    if let Some(folder) = profile.folder.as_deref() {
        r = r.help(short_folder(folder));
    }
    r.on_press_tx(move |s: &mut State, tx: &mut Tx| activate(s, ix, tx))
}

fn add_row(s: &State, ix: usize, on: bool) -> El {
    let p = &s.palette;
    let ink = if on { p.text } else { p.text.alpha(0.82) };
    row()
        .key(ADD_ROW)
        .role("option")
        .label("Add profile")
        .w_full()
        .h(ROW_H)
        .px(8.)
        .gap(8.)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .cursor(rune::Cursor::Pointer)
        .on_hover(move |s: &mut State| select(s, ix))
        .child(el().size(16., 16.).center().child(ui::glyph(
            &icons::FOLDER_PLUS,
            14.,
            if on { p.text } else { p.text_muted },
        )))
        .child(
            el().grow(1.0)
                .min_w(0.0)
                .child(t("Add profile", SM, ink).truncate()),
        )
        .on_press_tx(move |s: &mut State, tx: &mut Tx| activate(s, ix, tx))
}

/// The search field on top, as t3code's `ComboboxSearchInput`.
fn search_field(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let picker = &s.workspace.inbox.picker;
    let style = k.ty.ui_compact;
    let field = rune::text_input(&picker.search, style)
        .single_line()
        .key("input")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .color(p.text)
        .placeholder("Search profiles…", p.text_faint)
        .caret(p.text, k.selection())
        .label("Search profiles")
        .on_edit(|s: &mut State| &mut s.workspace.inbox.picker.search)
        .on_input_change(|s: &mut State, _| s.workspace.inbox.picker.selected = 0)
        .on_input_key(on_key)
        .on_submit(submit);
    row()
        .key("profile-search")
        .w_full()
        .h(36.0_f32.max(style.line_px() + 16.0))
        .px(12.)
        .gap(8.)
        .center_y()
        .child(ui::glyph(&icons::SEARCH, 14., k.c.icon_muted).a11y_hidden())
        .child(field)
}

/// The combobox as a layer entry under the element keyed `anchor`, `gap`
/// points below it, centred on it. It pops in, falling into place, and out
/// faster ([`motion::pop`](crate::design::motion::pop)); its highlight glides.
pub(in crate::workspace) fn anchored(k: &Kit, menu: El, anchor: &str, gap: f32) -> El {
    menu.key("profile-menu")
        .anchored_fit(anchor, rune::Side::Bottom, rune::Align::Center, gap)
        .transition(crate::design::motion::pop(-6.))
        .token_motion(k.motion.overlay)
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
            folder: Some(format!("/work/{id}")),
        }
    }

    fn many() -> Vec<InboxProfile> {
        vec![
            profile("default", "Default", Some(900)),
            profile("thesis", "Thesis", None),
            profile("avallon", "Avallon Case", Some(100)),
            profile("verpin", "Verpin", Some(500)),
            profile("builder", "IronClaw Builder", Some(700)),
        ]
    }

    fn ids(items: &[InboxProfile]) -> Vec<&str> {
        items.iter().map(|i| i.profile_id.as_str()).collect()
    }

    fn key(key: Key, mods: Mods) -> KeyEvent {
        KeyEvent { key, mods }
    }

    fn state() -> State {
        let mut s = crate::chat::fixtures::test_state();
        // A fixture's backend counts as This Mac's (Add profile offered).
        s.fixture = true;
        s.workspace.inbox.list.profiles = many();
        s.workspace.inbox.list.last_used_profile_id = Some("default".into());
        s
    }

    fn on_draft(profile: &str) -> State {
        let mut s = state();
        s.chat = None;
        draft::start_in_profile(&mut s, profile.into());
        s.page = Page::Draft;
        s
    }

    /// The current profile leads, pinned ones follow in pin order, the rest
    /// go by last use and then by name.
    #[test]
    fn current_then_pinned_then_recent() {
        let pinned = vec!["thesis".to_string(), "avallon".to_string()];
        let ranked = rank(many(), "verpin", &pinned);
        assert_eq!(
            ids(&ranked),
            ["verpin", "thesis", "avallon", "default", "builder"]
        );
        // The current profile leads even when pinned further down.
        let ranked = rank(many(), "avallon", &pinned);
        assert_eq!(ids(&ranked)[..3], ["avallon", "thesis", "default"]);
    }

    /// The current profile is the draft's on the new-thread screen, the
    /// open thread's in a chat, then the sidebar filter, then the last used.
    #[test]
    fn the_current_profile_follows_the_screen() {
        let mut s = state();
        assert_eq!(current_profile(&s), "default", "last used");
        s.workspace.inbox.profile = Some("thesis".into());
        assert_eq!(current_profile(&s), "thesis", "sidebar filter");
        let s = on_draft("verpin");
        assert_eq!(current_profile(&s), "verpin", "the draft's");
    }

    /// Pinning while the list is open keeps every row in place; the new
    /// order shows on the next open.
    #[test]
    fn the_order_is_frozen_while_open() {
        let mut s = on_draft("default");
        toggle_draft_menu(&mut s);
        let before = ids(&shown(&s)).join(",");
        assert_eq!(before, "default,builder,verpin,avallon,thesis");
        let mut tx = Tx::default();
        // Highlight Thesis (the last row) and pin it from the keyboard.
        s.workspace.inbox.picker.selected = 4;
        let pin = Mods {
            shift: true,
            cmd: true,
            ..Mods::default()
        };
        assert!(on_key(&mut s, &key(Key::Command('p'), pin), &mut tx));
        assert!(is_pinned(&s, "thesis"));
        assert_eq!(ids(&shown(&s)).join(","), before, "no row moved");
        close(&mut s);
        toggle_draft_menu(&mut s);
        assert_eq!(
            ids(&shown(&s)).join(","),
            "default,thesis,builder,verpin,avallon"
        );
    }

    /// Search filters in place (name, folder or branch), Enter moves the
    /// draft, and "Add profile" is only offered while nothing is typed.
    #[test]
    fn search_filters_and_enter_moves_the_draft() {
        let mut s = on_draft("default");
        let mut tx = Tx::default();
        toggle_draft_menu(&mut s);
        assert!(offers_add(&s));
        assert_eq!(row_count(&s), 6);
        s.workspace.inbox.picker.search.replace_text("train");
        assert_eq!(ids(&shown(&s)), ["verpin"], "by branch");
        assert!(!offers_add(&s));
        s.workspace.inbox.picker.search.replace_text("work/thes");
        assert_eq!(ids(&shown(&s)), ["thesis"], "by folder");
        s.workspace.inbox.picker.search.replace_text("claw b");
        assert_eq!(ids(&shown(&s)), ["builder"], "by name");
        s.workspace.inbox.picker.search.replace_text("ic");
        assert!(shown(&s).is_empty(), "no scattered matches");
        s.workspace.inbox.picker.search.replace_text("claw");
        submit(&mut s, &mut tx);
        assert!(!is_open(&s));
        assert_eq!(
            s.workspace.draft.as_ref().expect("draft").profile,
            "builder"
        );
    }

    /// Escape closes without changing the profile and gives the keys back
    /// to the composer.
    #[test]
    fn escape_closes_and_keeps_the_profile() {
        let mut s = on_draft("verpin");
        let mut tx = Tx::default();
        toggle_draft_menu(&mut s);
        assert!(s.workspace.inbox.picker.search.focused);
        assert!(on_key(&mut s, &key(Key::Escape, Mods::default()), &mut tx));
        assert!(!is_open(&s));
        let d = s.workspace.draft.as_ref().expect("draft");
        assert_eq!(d.profile, "verpin");
        assert!(d.input.focused);
    }

    /// The new-thread button starts at once when there is nothing to choose,
    /// else opens ⌘K on "New thread in…"; Shift always starts here.
    #[test]
    fn new_thread_skips_the_picker_when_the_answer_is_known() {
        // Several profiles: ⌘K opens on the profiles.
        let mut s = state();
        new_thread(&mut s, false);
        assert!(crate::workspace::command::is_new_thread_in(&s));

        // Shift: the current profile at once.
        let mut s = state();
        s.workspace.inbox.profile = Some("thesis".into());
        new_thread(&mut s, true);
        assert_eq!(s.workspace.draft.as_ref().expect("draft").profile, "thesis");

        // The sidebar filtered to one profile: that one.
        let mut s = state();
        s.workspace.inbox.profile = Some("verpin".into());
        new_thread(&mut s, false);
        assert!(!crate::workspace::command::is_new_thread_in(&s));
        assert_eq!(s.workspace.draft.as_ref().expect("draft").profile, "verpin");

        // One profile: no question to ask.
        let mut s = state();
        s.workspace.inbox.list.profiles.truncate(1);
        new_thread(&mut s, false);
        assert_eq!(
            s.workspace.draft.as_ref().expect("draft").profile,
            "default"
        );
    }

    /// "Add profile" creates fresh and, when the server says an id is
    /// taken (an archived profile this app never saw), tries the next
    /// suffix instead of reviving the old profile; the draft lands in it.
    #[test]
    fn add_profile_skips_ids_the_server_says_are_taken() {
        use crate::jobs::harness::Pump;
        use crate::test_gateway::{Answer, Gateway};
        use zlo_client::profiles::ProfileInfo;

        let folder = "/tmp/zlo-picker-test/site";
        let saved = ProfileInfo {
            profile_id: "site-2".into(),
            settings: ProfileSettings {
                display_name: "site".into(),
                working_folder: Some(folder.into()),
                ..ProfileSettings::default()
            },
            is_default: false,
            effective_folder: folder.into(),
            git_branch: None,
            thread_count: 0,
            last_used_at: None,
            created_at: 1,
            updated_at: 1,
        };
        let inbox = serde_json::to_string(&zlo_client::inbox::InboxListResponse {
            rows: vec![],
            profiles: many(),
            last_used_profile_id: None,
        })
        .expect("json");
        let gateway = Gateway::start(vec![
            Answer::ok(r#"{"ok":false,"error":"profile_exists","message":"taken"}"#),
            Answer::ok(serde_json::to_string(&saved).expect("json")),
            Answer::ok(inbox.clone()),
            Answer::ok(inbox),
        ]);
        let mut s = state();
        s.fixture = false;
        s.rt.backend = Some(gateway.backend());
        let mut pump = Pump::install(&mut s);

        create_for_folder(&mut s, Path::new(folder));
        assert!(pump.until(&mut s, std::time::Duration::from_secs(10), |s| {
            s.workspace
                .draft
                .as_ref()
                .is_some_and(|d| d.profile == "site-2")
        }));
        assert_eq!(
            gateway.requests_to("/profiles/create/fresh"),
            2,
            "{:?}",
            gateway.paths()
        );
        assert!(
            s.workspace
                .draft
                .as_ref()
                .is_some_and(|d| d.error.is_none())
        );
    }

    /// A pin belongs to its computer: another computer's `work` is not
    /// pinned by this one's, and unpinning there leaves this one's pin.
    /// A gateway from before the create-only route answers 404: Add profile
    /// says the engine needs updating and never falls back to the plain
    /// create, which would revive an archived profile of that id.
    #[test]
    fn add_profile_never_falls_back_on_an_old_gateway() {
        use crate::jobs::harness::Pump;
        use crate::test_gateway::{Answer, Gateway};

        let gateway = Gateway::start(vec![Answer {
            status: 404,
            body: "Not Found".into(),
            delay: std::time::Duration::ZERO,
            drop_conn: false,
        }]);
        let mut s = state();
        s.fixture = false;
        s.rt.backend = Some(gateway.backend());
        let mut pump = Pump::install(&mut s);

        create_for_folder(&mut s, Path::new("/tmp/zlo-picker-test/site"));
        assert!(pump.until(&mut s, std::time::Duration::from_secs(10), |s| {
            s.workspace
                .draft
                .as_ref()
                .is_some_and(|d| d.error.is_some())
        }));
        let error = s.workspace.draft.as_ref().and_then(|d| d.error.clone());
        assert!(
            error.as_deref().is_some_and(|e| e.contains("too old")),
            "{error:?}"
        );
        assert_eq!(gateway.paths().len(), 1, "one try: {:?}", gateway.paths());
    }

    /// Add profile names a folder on this Mac: only offered while the
    /// backend in focus is this Mac's own (a loopback address), never a
    /// remote gateway that happens to be labelled This Mac.
    #[test]
    fn add_profile_is_only_offered_for_this_macs_backend() {
        let mut s = state();
        assert!(can_add_profile(&s), "a fixture is This Mac's");
        s.fixture = false;
        assert!(!can_add_profile(&s), "no local backend in focus");
        s.fixture = true;
        s.onboarding.computer = crate::onboarding::gateway::Computer::remote("studio", "Studio");
        assert!(!can_add_profile(&s), "another computer");
    }

    #[test]
    fn pins_belong_to_their_computer() {
        let mut s = state();
        toggle_pin(&mut s, "verpin");
        assert!(is_pinned(&s, "verpin"));
        let here = s.onboarding.computer.clone();
        s.onboarding.computer = crate::onboarding::gateway::Computer::remote("studio", "Studio");
        assert!(!is_pinned(&s, "verpin"));
        assert_eq!(pinned_here(&s), Vec::<String>::new());
        toggle_pin(&mut s, "verpin");
        toggle_pin(&mut s, "verpin");
        s.onboarding.computer = here;
        assert!(is_pinned(&s, "verpin"), "this computer's pin stays");
    }

    #[test]
    fn profile_ids_come_from_folder_names() {
        let taken = vec!["ironclaw".to_string(), "ironclaw-2".to_string()];
        assert_eq!(profile_id_for("IronClaw", &taken), "ironclaw-3");
        assert_eq!(profile_id_for("My Site (v2)", &[]), "my-site-v2");
        assert_eq!(profile_id_for("--Ünï--", &[]), "n");
        assert_eq!(profile_id_for("日本", &[]), "project");
        assert_eq!(profile_id_for("_x", &[]), "x");
    }

    /// The combobox hangs under the heading's name, t3code-wide, with
    /// "Add profile" on screen as its last row.
    #[test]
    fn the_combobox_hangs_under_the_heading() {
        use rune::{Env, Runtime};
        let mut s = crate::chat::fixtures::test_state();
        crate::workspace::fixtures::fixture_profile_menu_search(&mut s);
        s.workspace.inbox.picker.search.replace_text("");
        let mut rt = Runtime::new(s, crate::view, (1200., 800.), Env::new(true, 1.));
        let mut t = 0.;
        for _ in 0..30 {
            t += 1. / 60.;
            rt.frame(t);
        }
        let (menu, _) = rt.presented("profile-picker").expect("combobox drawn");
        let (name, _) = rt.presented("draft-profile").expect("heading name drawn");
        assert!(menu[1] >= name[1] + name[3], "below the name");
        assert!(
            (menu[2] - WIDTH).abs() < 1.,
            "t3code's width, got {}",
            menu[2]
        );
        assert!(menu[3] <= LIST_MAX_H + 60., "it scrolls instead of growing");
        assert!(
            rt.presented(ADD_ROW).is_some() || menu[3] > 200.,
            "add row reachable"
        );
    }
}
