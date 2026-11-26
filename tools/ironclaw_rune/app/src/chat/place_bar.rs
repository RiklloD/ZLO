//! The thin line under the composer card (t3code's): where the thread
//! works on the left (its worktree, or Local), its branch on the right.
//! Faint text, no plate, so it takes one short line and no attention.
//!
//! The chat draws it from [`crate::chat::side::place`]; the new-thread
//! screen puts its Local / New worktree menu on the left
//! (`workspace::draft`).

use rune::{El, el, row};

use crate::chat::side::{self, Place};
use crate::icons;
use crate::state::State;
use crate::workspace::ui::{self, XS, t};
use rune::Icon;

/// The line's height.
pub const HEIGHT: f32 = 24.;

/// The bar: `left` at the start, `right` at the end, under a card
/// `max_w` wide.
pub fn bar(key: &str, max_w: f32, left: Option<El>, right: Option<El>) -> El {
    row()
        .key(key.to_string())
        .w_full()
        .max_w(max_w)
        .h(HEIGHT)
        .px(6.)
        .gap(12.)
        .center_y()
        .child(left.unwrap_or_else(el).min_w(0.).shrink())
        .child(el().grow(1.))
        .child(right.unwrap_or_else(el).min_w(0.).shrink())
}

/// One faint label with its glyph; long names truncate.
pub fn item(s: &State, key: &str, glyph: &'static Icon, text: impl Into<String>) -> El {
    let p = &s.palette;
    row()
        .key(key.to_string())
        .min_w(0.)
        .gap(6.)
        .center_y()
        .child(ui::glyph(glyph, 13., p.text_faint))
        .child(t(text, XS, p.text_faint).min_w(0.).truncate())
}

/// The open chat's bar, or `None` when it works outside git.
pub fn chat_bar(s: &State, max_w: f32) -> Option<El> {
    let place = side::place(s)?;
    Some(bar(
        "composer-place",
        max_w,
        Some(place_item(s, &place)),
        place
            .branch
            .as_ref()
            .map(|b| branch_item(s, "composer-branch", b)),
    ))
}

/// The left end: the worktree's folder name, else Local.
fn place_item(s: &State, place: &Place) -> El {
    match &place.worktree {
        Some((name, path)) => {
            let mut item = item(s, "composer-worktree", &icons::FOLDER_GIT, name.clone());
            if !path.is_empty() {
                item = item.help(format!("Works in its own git worktree, {path}"));
            }
            item
        }
        None => item(s, "composer-local", &icons::MONITOR, "Local")
            .help("Works in the project folder itself"),
    }
}

/// The right end: the branch checked out.
pub fn branch_item(s: &State, key: &str, branch: &str) -> El {
    item(s, key, &icons::GIT_BRANCH, branch.to_string()).help(format!("On branch {branch}"))
}
