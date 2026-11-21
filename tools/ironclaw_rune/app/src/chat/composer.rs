//! The composer card (GPUI `chat.rs`: `render_composer`, `composer_card`,
//! `composer_surface`, `round_button`, `attachment_card`, and the composer's
//! key routing). `composer_card`, `round_button`, `composer_surface` and
//! `attachment_card` are shared with the new-thread and quick composers.
//!
//! Keys (t3code's, `dispatch::key_action`): ⌘↵ with a draft while a turn
//! runs does the other of steer and queue for one message; ⇧⌘↵ sends the
//! first queued message now, and ⌥↑ with the caret at the start edits the
//! last one. The newest open approval has its own chords, ⌥⌘↵ (approve) and
//! ⌥⇧⌘↵ (always allow), so no send chord can grant a tool. A held chord
//! acts once. Then the `/` and `@` menu takes ↑ ↓ Tab
//! Esc ↵ while it is open; ↑ in an empty composer recalls the last message.
//! Plain ↵ sends (`on_submit`): Settings → Follow-ups while a turn runs.

use rune::{Color, Cx, El, Icon, KeyEvent, Tx, auto_grow, col, el, icon, row, text, text_input};
use rune_kit::Kit;
use zlo_client::GateDecision;
use zlo_client::attachments::format_bytes;
use zlo_client::transcript::AttachmentChip;

use super::dispatch::{self, KeyAction};
use super::menu::{MenuKey, MenuKeyName};
use super::{ChatState, actions, chat_for, chat_ref, flow, inline_files, menu, steer};
use crate::design::{self, Palette};
use crate::icons;
use crate::state::State;

/// The composer's fill and rim, `focused` its focus: the theme's card, one
/// opaque step off the panel, so it never reads darker than what is behind
/// it (a see-through fill over a translucent window did). The model
/// picker's panel wears the same surface so it reads as part of the
/// composer.
pub fn composer_surface(p: &Palette, focused: bool) -> (Color, Color) {
    (
        p.surface_card,
        if focused { p.border_strong } else { p.border },
    )
}

/// Zeron's composer pill: the text on top; below it the model, its thinking
/// level and the access pill on the left, attach and send on the right. The
/// new-thread screen draws it open (`compact` false). The rim brightens with
/// focus.
#[allow(clippy::too_many_arguments)]
pub fn composer_card(
    s: &State,
    key: &str,
    focused: bool,
    compact: bool,
    access: Option<El>,
    input: El,
    picker: El,
    actions: El,
) -> El {
    let p = &s.palette;
    // One flat plate. Docked over the transcript it takes one short shadow
    // so text scrolling under it stays behind; the hero card has none.
    let (fill, rim) = composer_surface(p, focused);
    let shadow = if !compact {
        Color::TRANSPARENT
    } else if p.light {
        Color::BLACK.alpha(0.06)
    } else {
        Color::BLACK.alpha(0.28)
    };
    let mut pills = row()
        .key("composer-pills")
        .grow(1.)
        .min_w(0.)
        .center_y()
        .gap(4.)
        .child(picker);
    if let Some(access) = access {
        pills = pills.child(access);
    }
    col()
        .key(key.to_string())
        .w_full()
        .max_w(super::content_width(s))
        .radius(design::COMPOSER_RADIUS)
        .fill(fill)
        .border(1., rim)
        .shadow(4., 16., shadow)
        .px(14.)
        .pt(if compact { 12. } else { 14. })
        .pb(8.)
        .gap(6.)
        .child(
            el().key("composer-text")
                .w_full()
                .min_w(0.)
                .min_h(if compact { 24. } else { 44. })
                .child(input),
        )
        .child(
            row()
                .key("composer-controls")
                .h(32.)
                .center_y()
                .gap(4.)
                // The pills give way first; attach and send never move.
                .child(pills)
                .child(actions),
        )
}

/// The composer's controls, one geometry: 28 px tiles with soft corners.
/// Send and Stop are monochrome, the text colour as a plate when ready and a
/// neutral wash when idle, so the composer holds no accent at all; the
/// others are bare glyphs washed under the pointer.
pub fn round_button(
    s: &State,
    id: &str,
    glyph: &'static Icon,
    filled: bool,
    label: &str,
    _k: &Kit,
) -> El {
    let p = &s.palette;
    let wash = if p.light {
        Color::BLACK.alpha(0.08)
    } else {
        Color::WHITE.alpha(0.10)
    };
    let base = row()
        .key(format!("round-{id}"))
        .role("button")
        .label(label.to_string())
        .center()
        .cursor(rune::Cursor::Pointer);
    if id.ends_with("send") || id == "stop" {
        let (fill, ink) = if filled {
            (p.text, p.bg)
        } else {
            (wash, p.text_faint)
        };
        // The idle wash turns into the solid plate on the slide spring (the
        // first keystroke asks for it, `on_change`); a press sinks it a
        // little. The glyph is keyed by what the button does, so a mode flip
        // (send, steer, queue, update, resume) swaps it with a small blur
        // pop rather than a cut.
        return base
            .size(28., 28.)
            .radius(7.)
            .fill(fill)
            .motion(design::styles::Style::of(&s.prefs).slide)
            .press_scale(0.94)
            .hover(if filled {
                p.bg.alpha(0.15)
            } else {
                Color::TRANSPARENT
            })
            .child(
                icon(glyph, 14.)
                    .key(format!("glyph-{label}"))
                    .color(ink)
                    .transition(glyph_swap()),
            );
    }
    let mut b = base.size(28., 28.).radius(7.);
    b = if filled {
        b.fill(p.text).hover(p.bg.alpha(0.15))
    } else {
        b.hover(wash)
    };
    b.child(icon(glyph, 14.).color(if filled { p.bg } else { p.text_muted }))
}

/// A send-button glyph replacing another in place: a short blur, scale and
/// fade, out faster than in. Under reduced motion Rune makes it a fade.
fn glyph_swap() -> rune::Transition {
    rune::Transition::blur_replace()
        .combined(rune::Transition::scale(0.8))
        .allow_small_scale()
        .springs(rune::SpringPair::with_exit_ratio(GLYPH_SPRING, 0.6))
}

const GLYPH_SPRING: rune::Spring = rune::Spring::new(0.2, 0.);

/// Send or Stop arriving beside the other (typing while a turn runs) or
/// leaving: a small scale pop with a fade, on the pop spring.
pub(crate) fn round_pop() -> rune::Transition {
    rune::Transition::scale(0.75)
        .allow_small_scale()
        .springs(rune::SpringPair::with_exit_ratio(design::styles::POP, 0.6))
}

/// An attachment card, the one look for staged and sent files: an icon
/// tile, the name, and a muted second line (folder, size or type).
pub fn attachment_card(
    s: &State,
    key: String,
    glyph: &'static Icon,
    title: &str,
    subtitle: Option<String>,
) -> El {
    let p = &s.palette;
    let mut words = col().min_w(0.).child(
        text(title.to_string())
            .font(flow::type_at(s, 13., 18., 500.))
            .color(p.text)
            .truncate(),
    );
    if let Some(line) = subtitle {
        words = words.child(
            text(line)
                .font(flow::type_at(s, 11., 15., 400.))
                .color(p.text_muted)
                .truncate(),
        );
    }
    row()
        .key(key)
        .gap(8.)
        .padding(6.)
        .pr(8.)
        .max_w(300.)
        .radius(8.)
        .border(1., p.border_strong)
        .fill(p.hover)
        .center_y()
        .child(
            row()
                .size(32., 32.)
                .center()
                .radius(6.)
                .border(1., p.border_strong)
                .child(icon(glyph, 14.).color(p.text_muted)),
        )
        .child(words)
}

/// A sent file's second line: its size, else its type from the extension.
pub(crate) fn sent_subtitle(chip: &AttachmentChip) -> Option<String> {
    chip.size.map(format_bytes).or_else(|| {
        std::path::Path::new(&chip.filename)
            .extension()
            .map(|ext| ext.to_string_lossy().to_uppercase())
    })
}

/// The composer card for `chat`: text on top; model pill, attach and send
/// below.
pub(crate) fn view(
    s: &State,
    chat: &ChatState,
    running: bool,
    cx: &mut Cx<State>,
    k: &Kit,
    layer: &mut Vec<El>,
) -> El {
    let p = &s.palette;
    let id = chat.thread_id.clone();
    let send = steer::render_send(s, chat, running, k);
    let attach_id = id.clone();
    let attach = round_button(s, "attach", &icons::PAPERCLIP, false, "Attach files", k)
        .help("Attach files")
        .on_press_tx(move |_: &mut State, tx| actions::pick_files(&attach_id, tx));
    let snapshots = inline_files::snapshot_keys(s, &chat.staged);
    let style = flow::type_at(s, 14., 22.75, 400.);
    let (edit_id, key_id, change_id, submit_id, paste_id) =
        (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    // Whether Send was ready when this view was built: the state before the
    // next edit, since every edit builds the view again.
    let was_ready = send_ready(chat);
    let field = text_input(&chat.composer, style)
        .key("composer-input")
        .w_full()
        .color(p.text)
        .placeholder(
            if chat.embedded {
                "Ask anything about this chat…"
            } else {
                "Ask anything…"
            },
            p.text_faint,
        )
        .caret(p.text, k.selection())
        .label("Message")
        .pill_colors(p.hover, Some(p.text))
        .pill_icon(move |pill| inline_files::pill_icon_for(&snapshots, pill))
        .on_edit(move |s: &mut State| super::composer_edit(s, &edit_id))
        .on_input_key(move |s: &mut State, ev: &KeyEvent, tx: &mut Tx| on_key(s, &key_id, ev, tx))
        .on_input_change(move |s: &mut State, tx: &mut Tx| {
            on_change(s, &change_id);
            ready_flip(s, &change_id, was_ready, tx);
        })
        .on_submit(move |s: &mut State, tx: &mut Tx| actions::send(s, &submit_id, tx))
        .on_input_paste(move |s: &mut State, content, _tx| {
            let Some(pasted) = inline_files::pasted(content) else {
                return false;
            };
            actions::paste(s, &paste_id, pasted);
            true
        });
    let input = auto_grow(field, 1, 8);
    let target = crate::voice::Composer::Chat(id.clone());
    let voice = crate::voice::ui::composer_controls(s, k, layer, &target, Some(&chat.thread_id));
    let controls = row()
        .key("composer-actions")
        .gap(6.)
        .center_y()
        .child(attach)
        .child(voice)
        .child(send);
    // Each chat's own model picker (area H); a side chat's is compact.
    let picker = super::model_picker::pills(s, &chat.thread_id, cx, k, layer);
    let access = super::approval_mode::pill(s, chat, k);
    // Keyed by thread: a side chat's composer sits beside this one.
    let card = composer_card(
        s,
        &chat.composer_key(),
        chat.composer.focused,
        true,
        Some(access),
        input,
        picker,
        controls,
    );
    // The new-thread composer flies down to this dock (GPUI `design/dock.rs`)
    // and back on `motion::DOCK` (`dock_matched`).
    match chat.embedded {
        false => dock_matched(s, card),
        true => card,
    }
}

/// The hero and docked composer cards share `motion::COMPOSER_MATCH`, so the
/// new-thread composer flies to the chat's dock and back. Not under a
/// wallpaper: the wallpaper is laid OVER the content (as GPUI and
/// vscode-background do), while a matched flight draws above the whole
/// window, so the flying card would sit on top of the picture, unwashed and
/// dark, then snap back under it as it landed. There it swaps in place.
/// A thread switch gives the new dock a new id (`WorkspaceState::dock_flight`):
/// two docked composers sit at the same rect, and a flight between them
/// would only keep frames coming with nothing moving.
///
/// The card carries `DOCK` as its own spring, so a flight the commit names
/// no spring for (back to the new-thread screen) glides on it too, not on
/// Rune's slower default; its rim and the controls' layout inside it move on
/// it as well. A focus change from a click lands at once (Rune snaps the
/// pointer-down commit), so the rim follows focus without lag.
pub fn dock_matched(s: &State, card: El) -> El {
    if design::wallpaper::covers_composer(&s.prefs) {
        card
    } else {
        card.matched(design::motion::composer_match(s.workspace.dock_flight))
            .motion(design::motion::DOCK)
    }
}

/// Send has something to send.
fn send_ready(chat: &ChatState) -> bool {
    !chat.staged.is_empty() || !chat.composer.text.trim().is_empty()
}

/// The keystroke that gives the draft its first word (or takes its last)
/// animates, though typing lands at once: Send's wash turns into its plate
/// and, while a turn runs, Send pops in beside Stop. Only that keystroke:
/// every other one stays instant, so the caret and the transcript never wait
/// on a spring.
fn ready_flip(s: &State, id: &str, was_ready: bool, tx: &mut Tx) {
    if chat_ref(s, id).is_some_and(|c| send_ready(c) != was_ready) {
        tx.animated();
    }
}

/// The composer's keys, before the field edits with them.
fn on_key(s: &mut State, id: &str, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(c) = chat_ref(s, id) else {
        return false;
    };
    // An IME composition owns its keys (Enter commits it, not a send).
    if c.composer.preedit().is_some() {
        return false;
    }
    let gate = c.open_approval();
    let cx = dispatch::KeyContext {
        running: c.running_run().is_some(),
        draft_empty: c.staged.is_empty() && c.composer.text.trim().is_empty(),
        caret_at_start: c.composer.anchor == 0 && c.composer.focus == 0,
        queued: c
            .orchestration
            .queue
            .items
            .iter()
            .filter(|i| !i.is_sending())
            .count(),
        editing: c.orchestration.editing.is_some(),
        approval: gate.as_ref().map(|(_, _, always)| *always),
        repeat: tx.key_repeat(),
    };
    match dispatch::key_action(ev, cx) {
        Some(KeyAction::Swallow) => return true,
        Some(KeyAction::SendAlternate) => {
            actions::send_as(s, id, true, tx);
            return true;
        }
        Some(KeyAction::Send) => {
            actions::send(s, id, tx);
            return true;
        }
        Some(KeyAction::SendFirstQueued) => return steer::send_first_queued(s, id, tx),
        Some(KeyAction::EditLastQueued) => return steer::edit_last_queued(s, id),
        Some(KeyAction::Approve { always }) => {
            if let Some((run, gate, _)) = gate {
                actions::resolve_gate(s, id, run, gate, GateDecision::Approve { always }, tx);
                return true;
            }
        }
        None => {}
    }
    let Some(name) = MenuKeyName::of(ev) else {
        return false;
    };
    let Some(c) = chat_for(s, id) else {
        return false;
    };
    match c.menu.key(name, &mut c.composer, tx) {
        MenuKey::Handled => {
            on_change(s, id);
            true
        }
        MenuKey::Command(command) => {
            actions::pick_command(s, id, &command, tx);
            true
        }
        // Esc the menu did not take leaves the edit of a queued message.
        MenuKey::Unused if name == MenuKeyName::Escape => cx.editing && steer::escape_edit(s, id),
        MenuKey::Unused => name == MenuKeyName::Up && actions::recall_last(s, id),
    }
}

/// The text changed: the menu's highlight goes back to the top, pills that
/// were deleted drop their files, an `@` query ranks the project's files,
/// and the draft is noted.
pub(crate) fn on_change(s: &mut State, id: &str) {
    // Lines picked in the Changes tab are offered as an `@` selection.
    let selection = super::is_main(s, id)
        .then(|| super::side::changes_selection(s))
        .flatten();
    let Some(c) = chat_for(s, id) else { return };
    c.menu.selection = selection;
    c.menu.reset();
    let composer = c.composer.clone();
    inline_files::prune(&composer, &mut c.staged);
    if let Some(query) = menu::ComposerMenu::at_query(&composer.text) {
        let source = actions::mention_source(s, id);
        let menu_of = menu_of(id);
        menu::sync_index(s, source, menu_of.clone());
        menu::rank_in_background(s, query, menu_of);
    }
    actions::record_draft(s, id);
}

/// `id`'s menu, found again from the root.
pub(crate) fn menu_of(id: &str) -> menu::MenuOf {
    let id = id.to_string();
    std::sync::Arc::new(move |s: &mut State| chat_for(s, &id).map(|c| &mut c.menu))
}
