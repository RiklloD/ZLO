//! The composer card (GPUI `chat.rs`: `render_composer`, `composer_card`,
//! `composer_surface`, `round_button`, `attachment_card`, and the composer's
//! key routing). `composer_card`, `round_button`, `composer_surface` and
//! `attachment_card` are shared with the new-thread and quick composers.
//!
//! Keys, in GPUI's order: ⌥↵ queues after the turn (`steer_key`), ⌘↵
//! approves the newest open approval (⇧⌘↵ always allows it), then the `/`
//! and `@` menu takes ↑ ↓ Tab Esc ↵ while it is open; ↑ in an empty composer
//! recalls the last message. Plain ↵ sends (`on_submit`).

use ironclaw_gpui_client::GateDecision;
use ironclaw_gpui_client::attachments::format_bytes;
use ironclaw_gpui_client::transcript::AttachmentChip;
use rune::{Color, Cx, El, Icon, KeyEvent, Tx, auto_grow, col, el, icon, row, text, text_input};
use rune_kit::Kit;

use super::menu::{MenuKey, MenuKeyName};
use super::{ChatState, actions, chat_for, chat_ref, flow, inline_files, menu, steer};
use crate::design::{self, Palette};
use crate::icons;
use crate::state::State;

/// The composer's fill and rim, `focused` its focus. The model picker's
/// panel wears the same surface so it reads as part of the composer;
/// `solid` lifts the fill's alpha for text drawn over the transcript.
pub fn composer_surface(p: &Palette, focused: bool, solid: bool) -> (Color, Color) {
    let ink = |a: f32| {
        if p.light {
            Color::BLACK.alpha(a)
        } else {
            Color::WHITE.alpha(a)
        }
    };
    let fill = if p.light {
        Color::WHITE.alpha(if solid { 0.94 } else { 0.8 })
    } else {
        p.surface_card
            .mix(p.bg, 0.25)
            .alpha(if solid { 0.96 } else { 0.86 })
    };
    (fill, if focused { ink(0.14) } else { ink(0.07) })
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
    // One plate, one short shadow (T3's design pass): it sits on the
    // transcript instead of floating over it.
    let (fill, rim) = composer_surface(p, focused, false);
    let shadow = if p.light {
        Color::BLACK.alpha(0.06)
    } else {
        Color::BLACK.alpha(0.3)
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
    col().key(key.to_string())
        .w_full()
        .max_w(super::content_width(s))
        .radius(design::BUBBLE_RADIUS)
        .fill(fill)
        .border(1., rim)
        .shadow(4., 16., shadow)
        .px(if compact { 14. } else { 18. })
        .pt(if compact { 12. } else { 16. })
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

/// Zeron's round controls: a 28 px circle; Send and Stop are a flat accent
/// tile with soft corners that sinks into a neutral wash when idle, so an
/// empty composer has no bright spot.
pub fn round_button(s: &State, id: &str, glyph: &'static Icon, filled: bool, label: &str, _k: &Kit) -> El {
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
            (p.accent, p.on_accent())
        } else {
            (wash, p.text_faint)
        };
        return base
            .size(30., 30.)
            .radius(8.)
            .fill(fill)
            .hover(Color::WHITE.alpha(0.06))
            .child(icon(glyph, 15.).color(ink));
    }
    let mut b = base.size(28., 28.).radius(14.);
    b = if filled {
        b.fill(p.text).hover(p.bg.alpha(0.15))
    } else if id == "send" {
        b.fill(wash)
    } else {
        b.hover(wash)
    };
    b.child(icon(glyph, 14.).color(if filled { p.bg } else { p.text_muted }))
}

/// An attachment card, the one look for staged and sent files: an icon
/// tile, the name, and a muted second line (folder, size or type).
pub fn attachment_card(s: &State, key: String, glyph: &'static Icon, title: &str, subtitle: Option<String>) -> El {
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
    let field = text_input(&chat.composer, style)
        .key("composer-input")
        .w_full()
        .color(p.text)
        .placeholder(
            if chat.embedded {
                "Ask anything about this chat…"
            } else {
                "Do anything…"
            },
            p.text_faint,
        )
        .caret(p.text, k.selection())
        .label("Message")
        .pill_colors(p.hover, Some(p.text))
        .pill_icon(move |pill| inline_files::pill_icon_for(&snapshots, pill))
        .on_edit(move |s: &mut State| super::composer_edit(s, &edit_id))
        .on_input_key(move |s: &mut State, ev: &KeyEvent, tx: &mut Tx| on_key(s, &key_id, ev, tx))
        .on_input_change(move |s: &mut State, _tx: &mut Tx| on_change(s, &change_id))
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
    // The model picker (area H) is the open chat's; a side chat runs on
    // the model it was forked with.
    let picker = match chat.embedded {
        false => super::model_picker::pills(s, cx, k, layer),
        true => el(),
    };
    let access = super::approval_mode::pill(s, chat, k);
    // Keyed by thread: a side chat's composer sits beside this one.
    composer_card(
        s,
        &chat.composer_key(),
        chat.composer.focused,
        true,
        Some(access),
        input,
        picker,
        controls,
    )
}

/// The composer's keys, before the field edits with them.
fn on_key(s: &mut State, id: &str, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if steer::steer_key(s, id, ev, tx) {
        return true;
    }
    // ⌘↵ approves the newest open approval, ⇧⌘↵ always allows it.
    if ev.key == rune::Key::Enter
        && ev.mods.cmd
        && let Some((run, gate, allow_always)) = chat_ref(s, id).and_then(|c| c.open_approval())
    {
        let always = ev.mods.shift && allow_always;
        actions::resolve_gate(s, id, run, gate, GateDecision::Approve { always }, tx);
        return true;
    }
    let Some(name) = MenuKeyName::of(ev) else {
        return false;
    };
    let Some(c) = chat_for(s, id) else { return false };
    match c.menu.key(name, &mut c.composer, tx) {
        MenuKey::Handled => {
            on_change(s, id);
            true
        }
        MenuKey::Command(command) => {
            actions::pick_command(s, id, &command, tx);
            true
        }
        MenuKey::Unused => name == MenuKeyName::Up && actions::recall_last(s, id),
    }
}

/// The text changed: the menu's highlight goes back to the top, pills that
/// were deleted drop their files, an `@` query ranks the project's files,
/// and the draft is noted.
pub(crate) fn on_change(s: &mut State, id: &str) {
    // Lines picked in the Changes tab are offered as an `@` selection.
    let selection = super::is_main(s, id).then(|| super::side::changes_selection(s)).flatten();
    let Some(c) = chat_for(s, id) else { return };
    c.menu.selection = selection;
    c.menu.reset();
    let composer = c.composer.clone();
    inline_files::prune(&composer, &mut c.staged);
    if let Some(query) = menu::ComposerMenu::at_query(&composer.text) {
        let root = actions::mention_root(s, id);
        let menu_of = menu_of(id);
        menu::sync_index(s, root.as_deref(), menu_of.clone());
        menu::rank_in_background(s, query, menu_of);
    }
    actions::record_draft(s, id);
}

/// `id`'s menu, found again from the root.
pub(crate) fn menu_of(id: &str) -> menu::MenuOf {
    let id = id.to_string();
    std::sync::Arc::new(move |s: &mut State| chat_for(s, &id).map(|c| &mut c.menu))
}
