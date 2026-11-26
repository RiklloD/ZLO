//! The profile menu: a compact, self-adaptive Ghost Capsule behind both
//! triggers (the sidebar's new-thread button and the new-thread screen's
//! profile switch).
//!
//! With 30+ profiles it stays small (~4 dynamic rows) instead of stretching
//! across the sidebar. Profiles rank automatically by frecency (decayed
//! thread activity + recency) and live urgency (`Needs you` / `Working`),
//! with zero manual pinning. Typing searches all profiles immediately (with
//! acronym matching, inline ghost autocomplete, and matched-character
//! highlights); scrolling down, arrowing past row 4, or clicking the reveal
//! strip expands into a capped scrollable list.
//!
//! Keys, all from the search field: type to filter, `Tab` accepts the inline
//! ghost completion, `↑↓` move (arrowing past the last collapsed row expands),
//! `↵` starts in the selected profile, `⌘1..⌘5` start in a numbered row
//! (holding `⌘` shows the numbers), `Escape` closes. The last row always
//! makes a new profile, named after the text typed when no profile has that
//! name.

use std::collections::BTreeMap;

use chrono::Utc;
use rune::{
    Color, El, Key, KeyEvent, Modifier, Span, TextEdit, TextStyle, Tx, WheelEvent, col, el, rich,
    row,
};
use rune_kit::{Kit, popover};
use zlo_client::inbox::{InboxProfile, InboxRow, InboxStatus};
use zlo_client::profiles::DEFAULT_PROFILE_ID;

use super::short_age;
use super::view::{profile_color, profile_mark, pulse_dots};
use crate::design;
use crate::icons;
use crate::state::{Page, State};
use crate::workspace::command::score;
use crate::workspace::draft;
use crate::workspace::ui::{self, REGULAR, SEMIBOLD, SM, XS, t};

/// The menu's width under the new-thread screen's switch.
pub(in crate::workspace) const WIDTH: f32 = 280.;
/// In the sidebar the menu is as wide as the list, down to this.
pub(in crate::workspace) const MIN_WIDTH: f32 = 224.;
/// Dynamic rows shown on open before scrolling or typing.
const CAPSULE_LIMIT: usize = 4;
/// Fast dynamic `⌘1..⌘N` shortcuts (only visible while holding `⌘`).
const CMD_SHORTCUTS: usize = 5;
const ROW_H: f32 = 30.;
/// The profile list's padding and the gap between its rows.
const ROWS_PAD: f32 = 4.;
const ROWS_GAP: f32 = 1.;
/// Hard vertical ceiling for the expanded or filtered list (~8 rows).
const EXPANDED_MAX_H: f32 = 248.;
/// Exponential half-life for profile frecency (in days).
const FRECENCY_HALF_LIFE_DAYS: f64 = 5.0;
const MS_PER_MIN: f64 = 60_000.0;
const MS_PER_DAY: f64 = 86_400_000.0;
const NEW_ROW: &str = "pick-profile-new";
const MORE_ROW: &str = "pick-profile-more";

/// The open menu's own state; which trigger opened it lives with the
/// trigger (`Inbox::new_menu_open`, `Draft::menu_open`).
pub struct ProfilePicker {
    pub search: TextEdit,
    /// The row the keys act on: a profile, or the new-profile row after
    /// the last visible one.
    selected: usize,
    /// Expanded beyond [`CAPSULE_LIMIT`] via wheel scroll, arrow-down, or click.
    expanded: bool,
    /// `⌘` alone is held: the first five rows show their shortcut number.
    cmd_held: bool,
}

impl ProfilePicker {
    pub(super) fn new() -> Self {
        Self {
            search: TextEdit::new(""),
            selected: 0,
            expanded: false,
            cmd_held: false,
        }
    }

    /// Scenes: the row an arrow key would have reached.
    pub(in crate::workspace) fn fixture_select(&mut self, ix: usize) {
        self.selected = ix;
    }

    /// Tests: ⌘ held (or let go) while the menu is open.
    #[cfg(test)]
    pub(in crate::workspace) fn fixture_cmd(&mut self, held: bool) {
        self.cmd_held = held;
    }
}

/// Live activity and decayed thread-usage weight for one profile.
#[derive(Clone, Copy, Default)]
struct ProfileSignal {
    working: usize,
    needs: usize,
    frecency: f64,
}

fn by_name(a: &InboxProfile, b: &InboxProfile) -> std::cmp::Ordering {
    a.display_name
        .to_lowercase()
        .cmp(&b.display_name.to_lowercase())
        .then_with(|| a.profile_id.cmp(&b.profile_id))
}

fn signals(rows: &[InboxRow], now: i64) -> BTreeMap<&str, ProfileSignal> {
    let mut map: BTreeMap<&str, ProfileSignal> = BTreeMap::new();
    // A subagent's run already shows on its parent ("Subagents").
    for r in rows.iter().filter(|r| r.parent_thread_id.is_none()) {
        let sig = map.entry(r.profile_id.as_str()).or_default();
        let age_days = ((now - r.last_activity_at).max(0) as f64) / MS_PER_DAY;
        sig.frecency += 0.5_f64.powf(age_days / FRECENCY_HALF_LIFE_DAYS);
        match r.status {
            Some(InboxStatus::Working | InboxStatus::Background) => sig.working += 1,
            Some(InboxStatus::NeedsInput) => sig.needs += 1,
            _ => {}
        }
    }
    map
}

fn dynamic_rank(p: &InboxProfile, last: Option<&str>, sig: ProfileSignal, now: i64) -> f64 {
    let is_last = last == Some(p.profile_id.as_str());
    let mut rank = sig.frecency;
    if let Some(at) = p.last_used_at {
        let age_mins = ((now - at).max(0) as f64) / MS_PER_MIN;
        rank += 1.5 * 0.5_f64.powf(age_mins / (FRECENCY_HALF_LIFE_DAYS * 1440.0));
        // Short-term recency lift keeps a just-touched profile handy while in
        // context without permanently knocking out daily drivers.
        if age_mins <= 30.0 {
            rank += 4.0;
        } else if age_mins <= 180.0 {
            rank += 1.5;
        }
    }
    if is_last {
        rank += if p.last_used_at.is_none() { 4.5 } else { 0.5 };
    }
    if sig.needs > 0 {
        rank += 6.5;
    } else if sig.working > 0 {
        rank += 2.5;
    }
    rank
}

/// Word initials in lowercase, e.g. "Financial Advisor" -> "fa".
fn initials(name: &str) -> String {
    let mut out = String::new();
    let mut new_word = true;
    for c in name.chars() {
        if c.is_alphanumeric() {
            if new_word {
                for lc in c.to_lowercase() {
                    out.push(lc);
                }
                new_word = false;
            }
        } else {
            new_word = true;
        }
    }
    out
}

/// Acronym prefix match for 2+ char queries (`fa` -> `Financial Advisor`,
/// `icb` -> `IronClaw Builder`). Ranks right behind substring/prefix hits.
fn acronym_score(query: &str, name: &str) -> Option<i32> {
    if query.chars().count() < 2 {
        return None;
    }
    let init = initials(name);
    init.starts_with(query)
        .then_some(900 - (init.chars().count().saturating_sub(query.chars().count())) as i32)
}

/// All profiles in dynamic or filtered order. `last` is the profile the last
/// thread started in; `rows` provide frecency and live urgency; `query` is
/// the search text.
fn ranked_profiles(
    profiles: &[InboxProfile],
    rows: &[InboxRow],
    last: Option<&str>,
    query: &str,
    now: i64,
) -> Vec<InboxProfile> {
    let sigs = signals(rows, now);
    let query = query.trim().to_lowercase();
    if !query.is_empty() {
        let mut hits: Vec<(i32, f64, &InboxProfile)> = profiles
            .iter()
            .filter_map(|p| {
                let lower = p.display_name.to_lowercase();
                let name = score(&query, &lower);
                let acro = acronym_score(&query, &p.display_name);
                let name_hit = match (name, acro) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
                // A branch counts when it holds the text as typed, behind
                // any name match: scattered letters there are noise.
                let branch = p
                    .git_branch
                    .as_deref()
                    .is_some_and(|b| b.to_lowercase().contains(&query))
                    .then_some(-1);
                let base = name_hit.or(branch)?;
                let sig = sigs.get(p.profile_id.as_str()).copied().unwrap_or_default();
                let frec = dynamic_rank(p, last, sig, now);
                // Bounded frecency boost breaks ties within a match tier
                // without letting a weak subsequence beat a stronger hit.
                let total = if base >= 0 {
                    base + (frec.min(10.0) * 2.0).round() as i32
                } else {
                    base
                };
                Some((total, frec, p))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.total_cmp(&a.1))
                .then_with(|| by_name(a.2, b.2))
        });
        return hits.into_iter().map(|(_, _, p)| p.clone()).collect();
    }

    let is_last = |p: &InboxProfile| last == Some(p.profile_id.as_str());
    let mut ranked: Vec<(f64, &InboxProfile)> = profiles
        .iter()
        .map(|p| {
            let sig = sigs.get(p.profile_id.as_str()).copied().unwrap_or_default();
            (dynamic_rank(p, last, sig, now), p)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| is_last(b.1).cmp(&is_last(a.1)))
            .then_with(|| b.1.last_used_at.cmp(&a.1.last_used_at))
            .then_with(|| by_name(a.1, b.1))
    });
    ranked.into_iter().map(|(_, p)| p.clone()).collect()
}

fn all_entries(s: &State, now: i64) -> Vec<InboxProfile> {
    let list = &s.workspace.inbox.list;
    let query = &s.workspace.inbox.picker.search.text;
    if list.profiles.is_empty() {
        let default = [InboxProfile {
            profile_id: DEFAULT_PROFILE_ID.into(),
            display_name: "Default".into(),
            git_branch: None,
            last_used_at: None,
        }];
        return ranked_profiles(&default, &list.rows, None, query, now);
    }
    ranked_profiles(
        &list.profiles,
        &list.rows,
        list.last_used_profile_id.as_deref(),
        query,
        now,
    )
}

/// The visible rows and how many more remain hidden in the collapsed capsule.
fn visible(s: &State, now: i64) -> (Vec<InboxProfile>, usize) {
    let mut all = all_entries(s, now);
    let picker = &s.workspace.inbox.picker;
    let collapsed = !picker.expanded && picker.search.text.trim().is_empty();
    if collapsed && all.len() > CAPSULE_LIMIT {
        let hidden = all.len() - CAPSULE_LIMIT;
        all.truncate(CAPSULE_LIMIT);
        (all, hidden)
    } else {
        (all, 0)
    }
}

fn shown(s: &State) -> Vec<InboxProfile> {
    visible(s, Utc::now().timestamp_millis()).0
}

/// Inline ghost completion suffix for `typed` against `candidate` when
/// `candidate` starts with `typed` (case-insensitively).
fn ghost_suffix<'a>(typed: &str, candidate: &'a str) -> Option<&'a str> {
    if typed.is_empty() || typed.starts_with(char::is_whitespace) {
        return None;
    }
    let mut t_chars = typed.chars();
    let mut c_indices = candidate.char_indices();
    let mut end_byte = 0;
    loop {
        match t_chars.next() {
            None => break,
            Some(tc) => {
                let (idx, cc) = c_indices.next()?;
                if tc.to_lowercase().ne(cc.to_lowercase()) {
                    return None;
                }
                end_byte = idx + cc.len_utf8();
            }
        }
    }
    let rest = &candidate[end_byte..];
    (!rest.is_empty()).then_some(rest)
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

/// Whether either trigger has the menu open.
pub(in crate::workspace) fn is_open(s: &State) -> bool {
    s.workspace.inbox.new_menu_open || s.workspace.draft.as_ref().is_some_and(|d| d.menu_open)
}

/// The sidebar's new-thread button: open the capsule on the top dynamic profile.
pub(in crate::workspace) fn toggle_new_menu(s: &mut State) {
    let open = !s.workspace.inbox.new_menu_open;
    close(s);
    if open {
        s.workspace.inbox.new_menu_open = true;
        opened(s, None);
    }
}

/// The new-thread screen's profile switch: open on its current profile.
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
    picker.expanded = false;
    picker.cmd_held = false;
    let now = Utc::now().timestamp_millis();
    let at = on.and_then(|id| all_entries(s, now).iter().position(|p| p.profile_id == id));
    if let Some(ix) = at
        && ix >= CAPSULE_LIMIT
    {
        s.workspace.inbox.picker.expanded = true;
    }
    s.workspace.inbox.picker.selected = at.unwrap_or(0);
}

/// Close the menu, whichever trigger opened it; the composer on screen
/// (the new-thread screen's or the chat's) takes the keys back. False when it was not open.
pub(in crate::workspace) fn close(s: &mut State) -> bool {
    if !is_open(s) {
        return false;
    }
    s.workspace.inbox.new_menu_open = false;
    s.workspace.inbox.picker.expanded = false;
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
        Some(profile) => draft::start_in_profile(s, profile.profile_id),
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

fn expand(s: &mut State) {
    s.workspace.inbox.picker.expanded = true;
}

fn step(s: &mut State, delta: isize, tx: &mut Tx) {
    let now = Utc::now().timestamp_millis();
    let (vis, hidden) = visible(s, now);
    let profiles = vis.len();
    let picker = &mut s.workspace.inbox.picker;
    let at = picker.selected.min(profiles) as isize;
    // Arrowing down past the last collapsed row smoothly unlocks the rest.
    if delta > 0 && hidden > 0 && at == profiles as isize - 1 {
        picker.expanded = true;
        picker.selected = profiles;
        // The first row the expansion unlocks.
        let all = all_entries(s, now);
        tx.scroll_into_view(row_key(&all, profiles));
        return;
    }
    picker.selected = (at + delta).rem_euclid(profiles as isize + 1) as usize;
    tx.scroll_into_view(row_key(&vis, picker.selected));
}

/// Row `ix` of `profiles`' key: the profile's id, so a row keeps its place
/// (and its wash) as the list filters; past the end, the "New profile" row.
fn row_key(profiles: &[InboxProfile], ix: usize) -> String {
    match profiles.get(ix) {
        Some(profile) => profile_key(&profile.profile_id),
        None => NEW_ROW.into(),
    }
}

/// The selection's wash: the profile's own colour, lightly (its mark goes
/// to full strength with it).
fn profile_wash(p: &design::Palette, profile_id: &str) -> Color {
    profile_color(p, profile_id, 1.).alpha(if p.light { 0.1 } else { 0.16 })
}

fn profile_key(profile_id: &str) -> String {
    format!("pick-profile-id-{profile_id}")
}

fn accept_ghost(s: &mut State) -> bool {
    let now = Utc::now().timestamp_millis();
    let (entries, _) = visible(s, now);
    let picker = &mut s.workspace.inbox.picker;
    let Some(candidate) = entries.get(picker.selected) else {
        return false;
    };
    let Some(suffix) = ghost_suffix(&picker.search.text, &candidate.display_name) else {
        return false;
    };
    let completed = format!("{}{suffix}", picker.search.text);
    picker.search.replace_text(&completed);
    picker.selected = 0;
    true
}

fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    match &ev.key {
        Key::Escape => {
            close(s);
        }
        Key::Up => step(s, -1, tx),
        Key::Down => step(s, 1, tx),
        Key::Tab if !(ev.mods.cmd || ev.mods.ctrl || ev.mods.alt) => {
            return accept_ghost(s);
        }
        Key::Command(c)
            if !(crate::system::extra_ctrl(ev.mods) || ev.mods.alt || ev.mods.shift) =>
        {
            let Some(n) = c.to_digit(10).filter(|n| (1..=9).contains(n)) else {
                return false;
            };
            // Only the top dynamic slots carry shortcuts; numbers past them do
            // nothing here and never fall through to the sidebar's ⌘N.
            let limit = shown(s).len().min(CMD_SHORTCUTS);
            if (n as usize) <= limit {
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

fn on_wheel_expand(s: &mut State, ev: WheelEvent, _: &mut Tx) -> bool {
    if ev.dy < 0.0
        && !s.workspace.inbox.picker.expanded
        && s.workspace.inbox.picker.search.text.trim().is_empty()
    {
        s.workspace.inbox.picker.expanded = true;
        return true;
    }
    false
}

fn submit(s: &mut State, _: &mut Tx) {
    let ix = s.workspace.inbox.picker.selected;
    activate(s, ix);
}

/// Short leaf segment of a git branch (`feat/spawn-fleet` -> `spawn-fleet`).
fn leaf_branch(branch: &str) -> &str {
    branch
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(branch)
}

/// Byte ranges in `name` to bold when `query` matches it (substring, initials,
/// or fuzzy subsequence).
fn match_ranges(name: &str, query: &str) -> Vec<(usize, usize)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = name.char_indices().collect();
    let lower_chars: Vec<char> = chars
        .iter()
        .map(|(_, c)| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let q_chars: Vec<char> = q.chars().collect();

    // 1. Contiguous substring match.
    if q_chars.len() <= lower_chars.len() {
        for start in 0..=(lower_chars.len() - q_chars.len()) {
            if lower_chars[start..start + q_chars.len()] == q_chars[..] {
                let b0 = chars[start].0;
                let last = start + q_chars.len() - 1;
                let b1 = chars[last].0 + chars[last].1.len_utf8();
                return vec![(b0, b1)];
            }
        }
    }

    // 2. Acronym / word-initial prefix match.
    if q_chars.len() >= 2 {
        let mut word_starts = Vec::new();
        let mut new_word = true;
        for (ix, (_, c)) in chars.iter().enumerate() {
            if c.is_alphanumeric() {
                if new_word {
                    word_starts.push(ix);
                    new_word = false;
                }
            } else {
                new_word = true;
            }
        }
        if word_starts.len() >= q_chars.len()
            && word_starts
                .iter()
                .zip(&q_chars)
                .all(|(&ix, &qc)| lower_chars[ix] == qc)
        {
            return word_starts[..q_chars.len()]
                .iter()
                .map(|&ix| (chars[ix].0, chars[ix].0 + chars[ix].1.len_utf8()))
                .collect();
        }
    }

    // 3. Fuzzy subsequence match.
    let mut qi = 0;
    let mut out = Vec::new();
    for (ix, &lc) in lower_chars.iter().enumerate() {
        if qi < q_chars.len() && lc == q_chars[qi] {
            out.push((chars[ix].0, chars[ix].0 + chars[ix].1.len_utf8()));
            qi += 1;
        }
    }
    if qi == q_chars.len() { out } else { Vec::new() }
}

fn highlighted_name(p: &design::Palette, name: &str, query: &str, on: bool) -> El {
    let base = if on { p.text } else { p.text_muted };
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

/// The search field with inline ghost autocomplete behind the live input.
fn search_field(s: &State, k: &Kit, placeholder: String, ghost: Option<&str>) -> El {
    let p = &s.palette;
    let picker = &s.workspace.inbox.picker;
    let style = k.ty.ui_compact;
    let height = 28.0_f32.max(style.line_px() + 12.0);

    let mut stack = row()
        .key("profile-search-stack")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .center_y();
    if let Some(suffix) = ghost {
        let typed = &picker.search.text;
        let full = format!("{typed}{suffix}");
        let ghost_el = row()
            .key("profile-search-ghost")
            .pin(Some(0.), Some(0.), Some(0.), Some(0.))
            .center_y()
            .a11y_hidden()
            .child(
                rich(
                    full,
                    vec![Span::new(0, typed.len()).color(Color::TRANSPARENT)],
                )
                .font(style)
                .color(p.text_faint)
                .truncate(),
            );
        stack = stack.child(ghost_el);
    }
    let field = rune::text_input(&picker.search, style)
        .single_line()
        .key("input")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .color(p.text)
        .placeholder(placeholder, p.text_faint)
        .caret(p.text, k.selection())
        .label("Start a thread in profile")
        .on_edit(|s: &mut State| &mut s.workspace.inbox.picker.search)
        .on_input_change(|s: &mut State, _| s.workspace.inbox.picker.selected = 0)
        .on_input_key(on_key)
        .on_submit(submit);
    stack = stack.child(field);

    let value = row()
        .key("value")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(6.0)
        .center_y()
        .child(
            el().key("leading")
                .pl(8.0)
                .center()
                .child(ui::glyph(&icons::SEARCH, 14., k.c.icon_muted))
                .a11y_hidden(),
        )
        .child(stack);

    row()
        .key("profile-search")
        .role("textbox")
        .w_full()
        .h(height)
        .pr(8.0)
        .gap(8.0)
        .child(value)
}

/// The menu as a layer entry under the element keyed `anchor`, `gap`
/// points below it, fitted to the window as `rune_kit::popover::anchored`
/// fits it. Not that helper itself: it lands every change inside the menu
/// at once (`El::instant_changes`), and this menu moves. It pops in, falling
/// into place, and out faster ([`motion::pop`](crate::design::motion::pop));
/// its selection glides, its height follows the rows as it expands, and
/// the ⌘N keycaps pop.
pub(in crate::workspace) fn anchored(k: &Kit, menu: El, anchor: &str, gap: f32) -> El {
    menu.key("profile-menu")
        .anchored_fit(anchor, rune::Side::Bottom, rune::Align::Center, gap)
        .transition(crate::design::motion::pop(-6.))
        .token_motion(k.motion.overlay)
}

/// Where the selected profile row sits in its list: the list's padding,
/// then a row and a gap per row above it.
fn row_top(ix: usize) -> f32 {
    ROWS_PAD + ix as f32 * (ROW_H + ROWS_GAP)
}

/// The menu's card, for either trigger's popover.
pub fn profile_picker(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let picker = &s.workspace.inbox.picker;
    let now = Utc::now().timestamp_millis();
    let (entries, hidden) = visible(s, now);
    let count = entries.len();
    let selected = picker.selected.min(count);
    let chosen = s.workspace.draft.as_ref().map(|d| d.profile.as_str());
    let sigs = signals(&s.workspace.inbox.list.rows, now);
    let query = picker.search.text.as_str();

    let placeholder = entries
        .first()
        .map(|first| format!("Start in {}…", first.display_name))
        .unwrap_or_else(|| "Start a thread in…".into());
    let ghost = entries
        .get(selected)
        .and_then(|item| ghost_suffix(query, &item.display_name));

    let mut rows = col()
        .key("profile-rows")
        .w_full()
        .padding(ROWS_PAD)
        .gap(ROWS_GAP);
    // The selection is one pill gliding behind the rows, in the selected
    // profile's colour (it morphs as it moves); on the New row it fades.
    let pill = entries.get(selected).and_then(|profile| {
        let top = row_top(selected);
        design::kit::slide_pill_sized(s, "profile-pill", Some(top), ROW_H).map(|pill| {
            pill.pin(Some(ROWS_PAD), Some(top), Some(ROWS_PAD), None)
                .radius(design::CONTROL_RADIUS)
                .fill(profile_wash(p, &profile.profile_id))
        })
    });
    rows = rows.children(pill);
    for (ix, profile) in entries.iter().enumerate() {
        let id = profile.profile_id.as_str();
        rows = rows.child(profile_row(
            s,
            ix,
            profile,
            query,
            ix == selected,
            chosen == Some(id),
            (picker.cmd_held && ix < CMD_SHORTCUTS).then_some(ix),
            sigs.get(id).copied().unwrap_or_default(),
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

    let search = search_field(s, k, placeholder, ghost);

    let mut card = col()
        .key("profile-picker")
        .role("dialog")
        .label("Start in profile")
        // A press on padding leaves the keys with the search field.
        .preserve_focus_on_press(true)
        .w_full()
        .min_h(0.0)
        .child(el().px(4.).py(4.).child(search))
        .child(ui::rule(p))
        .child(popover::fit_scroll(rows).max_h(EXPANDED_MAX_H));

    if hidden > 0 {
        card = card
            .on_wheel(on_wheel_expand)
            .child(ui::rule(p))
            .child(more_row(s, hidden));
    }

    card = card
        .child(ui::rule(p))
        .child(el().padding(4.).child(new_row(s, count, selected == count)));

    ui::floating(p, card, design::PANEL_RADIUS)
}

fn more_row(s: &State, hidden: usize) -> El {
    let p = &s.palette;
    let label = match hidden {
        1 => "1 more profile".to_string(),
        n => format!("{n} more profiles"),
    };
    row()
        .key(MORE_ROW)
        .role("button")
        .label(format!("Show {label}"))
        .w_full()
        .h(26.)
        .px(12.)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .hover(p.hover)
        .child(el().grow(1.0).child(t(label, XS, p.text_faint)))
        .child(t("type or scroll ↓", XS, p.text_faint))
        .on_press(expand)
}

#[allow(clippy::too_many_arguments)]
fn profile_row(
    s: &State,
    ix: usize,
    profile: &InboxProfile,
    query: &str,
    on: bool,
    chosen: bool,
    badge: Option<usize>,
    sig: ProfileSignal,
    now: i64,
) -> El {
    let p = &s.palette;
    let id = profile.profile_id.clone();
    let mut r = row()
        .key(profile_key(&profile.profile_id))
        .role("button")
        .label(format!("Start in {}", profile.display_name))
        .w_full()
        .h(ROW_H)
        .px(8.)
        .gap(8.)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .cursor(rune::Cursor::Pointer)
        .on_hover(move |s: &mut State| select(s, ix))
        .child(profile_mark(
            p,
            &profile.profile_id,
            &profile.display_name,
            if on { 1. } else { 0. },
        ))
        .child(el().grow(1.0).min_w(0.0).child(highlighted_name(
            p,
            &profile.display_name,
            query,
            on,
        )));

    // Strict 1-slot right metadata so names keep generous horizontal room:
    // ⌘N (while holding ⌘) > live urgency > compact leaf branch > short age.
    if let Some(ix) = badge {
        r = r.child(ui::shortcut_badge(p, ix));
    } else if sig.needs > 0 {
        r = r.child(
            row()
                .gap(4.)
                .center_y()
                .child(ui::dot(p.warning, 6.))
                .child(t("Needs you", XS, p.warning)),
        );
    } else if sig.working > 0 {
        let label = match sig.working {
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
    } else if let Some(branch) = profile.git_branch.as_deref() {
        r = r.child(
            row()
                .gap(2.)
                .center_y()
                .max_w(82.)
                .min_w(0.0)
                .shrink()
                .child(ui::glyph(&icons::GIT_BRANCH, 11., p.text_faint))
                .child(
                    el().min_w(0.0)
                        .child(t(leaf_branch(branch), XS, p.text_faint).truncate()),
                ),
        );
    } else if let Some(at) = profile.last_used_at {
        r = r.child(t(short_age(at, now), XS, p.text_faint));
    }
    if chosen {
        r = r.child(ui::glyph(&icons::CHECK, 14., p.text_muted));
    }
    r.on_press(move |s: &mut State| draft::start_in_profile(s, id.clone()))
}

/// The last row: a new profile, named after the search text when no
/// profile has that name. It carries the menu's key hints.
fn new_row(s: &State, count: usize, on: bool) -> El {
    let p = &s.palette;
    let label = match new_name(s) {
        Some(name) => format!("New profile “{name}”"),
        None => "New profile".into(),
    };
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

    fn ids(items: &[InboxProfile]) -> Vec<&str> {
        items.iter().map(|i| i.profile_id.as_str()).collect()
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
    fn dynamic_ranking_combines_frecency_recency_and_live_urgency() {
        use crate::workspace::inbox::tests::row;
        use zlo_client::inbox::InboxState;

        let now = 1_700_000_000_000_i64;
        let min = 60_000_i64;
        let profiles = vec![
            profile("daily", "Daily Driver", Some(now - 90 * min)),
            profile("cold", "Tax Prep", Some(now - 5 * min)),
            profile("urgent", "Needs Attention", None),
            profile("idle", "Idle Profile", None),
        ];
        let mut rows = Vec::new();
        for i in 0..8 {
            let mut r = row(&format!("d{i}"), InboxState::Inbox, None);
            r.profile_id = "daily".into();
            r.last_activity_at = now - (i + 1) * 60 * min;
            rows.push(r);
        }
        let mut need = row("u1", InboxState::Inbox, None);
        need.profile_id = "urgent".into();
        need.status = Some(InboxStatus::NeedsInput);
        need.last_activity_at = now - 10 * min;
        rows.push(need);

        let ranked = ranked_profiles(&profiles, &rows, Some("cold"), "", now);
        // Daily driver's frecency beats a one-off recent touch; urgent surfaces too.
        assert_eq!(ids(&ranked), ["daily", "urgent", "cold", "idle"]);
    }

    #[test]
    fn capsule_caps_idle_view_and_expands_on_arrow_or_wheel() {
        let mut s = state();
        let mut tx = Tx::default();
        toggle_new_menu(&mut s);
        let (vis, hidden) = visible(&s, 1_000);
        assert_eq!(vis.len(), CAPSULE_LIMIT);
        assert_eq!(hidden, many().len() - CAPSULE_LIMIT);

        // Arrowing down past row 4 expands seamlessly to row 5.
        for _ in 0..CAPSULE_LIMIT {
            assert!(on_key(&mut s, &key(Key::Down), &mut tx));
        }
        assert!(s.workspace.inbox.picker.expanded);
        assert_eq!(s.workspace.inbox.picker.selected, CAPSULE_LIMIT);
        assert_eq!(visible(&s, 1_000).0.len(), many().len());

        // Reopening resets back to the compact capsule; downward wheel unlocks it.
        toggle_new_menu(&mut s);
        toggle_new_menu(&mut s);
        assert!(!s.workspace.inbox.picker.expanded);
        assert!(on_wheel_expand(
            &mut s,
            WheelEvent::new(0., -20.).precise(false),
            &mut tx
        ));
        assert!(s.workspace.inbox.picker.expanded);
    }

    #[test]
    fn typing_filters_by_name_acronym_then_branch() {
        // The closest name leads: "FLat finder" before "deFauLt".
        let list = ranked_profiles(&many(), &[], None, " fl ", 1_000);
        assert_eq!(ids(&list), ["flat", "default"]);
        // Acronym match snaps to the top ("Flat Finder" -> "ff", "IronClaw Builder" -> "ib").
        assert_eq!(
            ids(&ranked_profiles(&many(), &[], None, "ff", 1_000)),
            ["flat"]
        );
        assert_eq!(
            ids(&ranked_profiles(&many(), &[], None, "ib", 1_000)),
            ["builder"]
        );
        // Scattered letters match a name ("Ironclaw BuildeR")...
        assert_eq!(
            ids(&ranked_profiles(&many(), &[], None, "icbr", 1_000)),
            ["builder"]
        );
        // ...a branch only as typed, and behind any name that matches; frecency
        // lifts the recently used match ("builder") ahead of a dormant one ("cyber").
        assert_eq!(
            ids(&ranked_profiles(&many(), &[], None, "train", 1_000)),
            ["verpin"]
        );
        assert_eq!(
            ids(&ranked_profiles(&many(), &[], None, "rls", 1_000)),
            [""; 0]
        );
        assert_eq!(
            ids(&ranked_profiles(&many(), &[], None, "re", 1_000)),
            ["builder", "cyber", "verpin"]
        );
        assert!(ranked_profiles(&many(), &[], None, "zzz", 1_000).is_empty());
    }

    #[test]
    fn ghost_completion_accepts_on_tab() {
        assert_eq!(ghost_suffix("v", "Verpin"), Some("erpin"));
        assert_eq!(ghost_suffix("VER", "Verpin"), Some("pin"));
        assert_eq!(ghost_suffix("verpin", "Verpin"), None);
        assert_eq!(ghost_suffix("", "Verpin"), None);

        let mut s = state();
        let mut tx = Tx::default();
        toggle_new_menu(&mut s);
        s.workspace.inbox.picker.search.replace_text("ver");
        assert!(on_key(&mut s, &key(Key::Tab), &mut tx));
        assert_eq!(s.workspace.inbox.picker.search.text, "verpin");
        submit(&mut s, &mut tx);
        assert_eq!(s.workspace.draft.as_ref().expect("draft").profile, "verpin");
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
        // Up from 0 lands straight on the new-profile row.
        assert!(on_key(&mut s, &key(Key::Up), &mut tx));
        assert_eq!(s.workspace.inbox.picker.selected, CAPSULE_LIMIT);
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
        let doing = signals(&rows, 1_000);
        assert_eq!(doing["default"].working, 1, "a subagent is not counted");
        assert_eq!(doing["work"].needs, 1);
    }

    /// The sidebar's menu stays compact and inside the sidebar: it hangs under
    /// the header at the list's width instead of spilling over the chat or
    /// growing into a tall wall with 30 profiles.
    #[test]
    fn the_sidebar_menu_stays_compact_and_in_the_sidebar() {
        use rune::{Env, Runtime};
        let mut s = crate::chat::fixtures::test_state();
        crate::workspace::fixtures::fixture_profile_menu(&mut s);
        for i in 0..20 {
            s.workspace.inbox.list.profiles.push(InboxProfile {
                profile_id: format!("extra-{i}"),
                display_name: format!("Workspace {i:02}"),
                git_branch: None,
                last_used_at: None,
            });
        }
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
        assert!(
            menu[3] < 260.,
            "collapsed capsule stays compact at 30+ profiles (got {:.1}px)",
            menu[3]
        );
        assert!(rt.presented(MORE_ROW).is_some(), "reveal strip is visible");
        assert!(rt.presented(NEW_ROW).is_some(), "the last row is on screen");
    }
}
