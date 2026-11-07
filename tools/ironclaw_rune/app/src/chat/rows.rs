//! One transcript row (GPUI `chat.rs`: `render_row`, `with_actions`,
//! `placed_body`, `openable`, `render_gate`, `switch_marker`): the person's
//! bubble with its files and mentions in place, replies, system lines,
//! gates, run notices and slash-command answers.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use ironclaw_gpui_client::GateDecision;
use ironclaw_gpui_client::contract::ProductGateKind;
use ironclaw_gpui_client::inbound_requests::{AnchoredPart, anchored_parts};
use ironclaw_gpui_client::mentions::{self as mention, MentionPart, MentionSpan, MentionUri};
use ironclaw_gpui_client::transcript::{AttachmentChip, GateState, Row, ToolStatus};
use rune::{Color, El, Icon, PopupItem, PopupMenu, Tx, col, el, icon, row, text};
use rune_kit::{Button, ControlSize, ButtonVariant, IconButton, IconButtonVariant, Kit};

use super::composer::{attachment_card, sent_subtitle};
use super::{ChatState, actions, approval_reason, chat_for, flow, inline_files, menu, reject_feedback, visual};
use crate::icons;
use crate::state::State;

/// A message with actions that show on hover, as in Zeron: copy, and on
/// answers "fork from here" (a new thread holding the history up to this
/// run). `mine`: a sent message, so the bar sits on the right. Right-click
/// opens the same actions, the selection's copy first when there is one.
pub(crate) fn with_actions(
    chat: &ChatState,
    key: &str,
    copy_text: &str,
    mine: bool,
    fork_run: Option<u32>,
    body: El,
    k: &Kit,
) -> El {
    let copied = chat
        .copied
        .as_ref()
        .is_some_and(|(k, at)| k == key && at.elapsed() < super::COPIED_FOR);
    let thread = chat.thread_id.clone();
    let copy = {
        let (text, copy_key, thread) = (copy_text.to_string(), key.to_string(), thread.clone());
        IconButton::new(if copied { &icons::CHECK } else { &icons::COPY })
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label("Copy message")
            .help(if copied { "Copied" } else { "Copy" })
            .on_press_tx(move |s: &mut State, tx| {
                tx.copy(text.clone());
                mark_copied(s, &thread, &copy_key, tx);
            })
            .build(k)
            .key(format!("{key}-copy"))
    };
    let mut bar = row().key(format!("{key}-bar")).h(22.).gap(2.).child(copy);
    if mine {
        bar = bar.justify_end();
    }
    if let Some(run) = fork_run {
        let thread = thread.clone();
        bar = bar.child(
            IconButton::new(&icons::GIT_FORK)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label("Fork from this message")
                .help("Fork from here")
                .on_press(move |s: &mut State| fork(s, &thread, Some(run)))
                .build(k)
                .key(format!("{key}-fork")),
        );
    }
    // The bar shows on hover (and while its check is up).
    let bar = bar.group_opacity(if copied { 1. } else { 0. }, 1.);
    let (menu_text, menu_key) = (copy_text.to_string(), key.to_string());
    col().key(format!("{key}-group"))
        .w_full()
        .gap(4.)
        .hover_group()
        .child(body)
        .child(bar)
        .on_secondary_press(move |_: &mut State, press, tx: &mut Tx| {
            let selected = tx.text_selection().map(|sel| sel.text().to_string()).unwrap_or_default();
            let mut m = PopupMenu::new();
            if !selected.is_empty() {
                m = m.item(PopupItem::new("selection", "Copy selection"));
            }
            m = m.item(PopupItem::new("message", "Copy message"));
            if fork_run.is_some() {
                m = m.item(PopupItem::new("fork", "Fork from here"));
            }
            let (text, key, thread) = (menu_text.clone(), menu_key.clone(), thread.clone());
            tx.popup_menu(m, press, move |s: &mut State, picked, tx| match picked {
                Some("selection") => tx.copy(selected),
                Some("message") => {
                    tx.copy(text);
                    mark_copied(s, &thread, &key, tx);
                }
                Some("fork") => fork(s, &thread, fork_run),
                _ => {}
            });
        })
}

/// Show the check, then one wake when it should turn back.
fn mark_copied(s: &mut State, thread: &str, key: &str, tx: &mut Tx) {
    if let Some(c) = chat_for(s, thread) {
        c.copied = Some((key.to_string(), Instant::now()));
    }
    tx.wake_after(super::COPIED_FOR);
}

pub(crate) fn render_row(s: &State, chat: &ChatState, row_: &Row, k: &Kit) -> El {
    let p = &s.palette;
    let thread = chat.thread_id.as_str();
    let small = flow::type_at(s, 12., 16., 400.);
    match row_ {
        Row::User {
            key,
            text: words,
            pending,
            attachments,
        } => {
            let parts = placed_parts(words);
            let placed: HashSet<usize> = parts
                .iter()
                .filter_map(|part| match part {
                    Placed::File(ix) => Some(*ix),
                    Placed::Text(_) | Placed::Mention(_) => None,
                })
                .collect();
            let chips = parts.iter().any(|p| !matches!(p, Placed::Text(_)));
            let body = if !chips {
                text(words.clone())
                    .key(format!("{key}-text"))
                    .font(flow::type_at(s, 14., 22., 400.))
                    .color(p.text)
                    .wrap()
                    .selectable(true)
            } else {
                placed_body(s, thread, key, &parts, attachments)
            };
            let mut out = col().key(format!("{key}-user")).w_full().items_end().gap(4.);
            if !words.is_empty() {
                out = out.child(flow::user_pill(s, key, body, *pending, super::content_width(s)));
            }
            let mut files = row().flex_wrap().justify_end().gap(4.);
            for (ix, chip) in attachments.iter().enumerate().filter(|(ix, _)| !placed.contains(ix)) {
                let card = attachment_card(
                    s,
                    format!("{key}-file-{ix}"),
                    &icons::FILE,
                    &chip.filename,
                    sent_subtitle(chip),
                );
                files = files.child(openable(card, thread, chip));
            }
            out.child(files)
        }
        Row::Assistant {
            key,
            text: words,
            streaming,
        } => visual::reply(s, thread, key, words, *streaming, k),
        Row::System { text: words, .. } if words.starts_with("Switched to ") => switch_marker(s, words),
        Row::System { key, text: words } => text(words.clone())
            .key(format!("{key}-system"))
            .font(small)
            .color(p.text_muted)
            .wrap()
            .selectable(true),
        Row::Thinking { key, text: words, .. } => {
            activity_line(s, key, &icons::BOT, "Thinking", Some(words.clone()))
        }
        Row::Narration { key, text: words, .. } => {
            activity_line(s, key, &icons::ELLIPSIS, words, None)
        }
        Row::Tool {
            key,
            title,
            detail,
            status,
            ..
        } => {
            let glyph = match status {
                ToolStatus::Running => &icons::LOADER_CIRCLE,
                ToolStatus::Completed => &icons::CIRCLE_CHECK,
                ToolStatus::Failed => &icons::CIRCLE_X,
            };
            activity_line(s, key, glyph, title, detail.clone())
        }
        Row::Skills { key, names, .. } => activity_line(
            s,
            key,
            &icons::BOOK_OPEN,
            &format!("Using skills: {}", names.join(", ")),
            None,
        ),
        Row::WorkSummary { key, text: words, .. } => {
            activity_line(s, key, &icons::FILE_TEXT, words, None)
        }
        Row::Plan { key, plan } => activity_line(
            s,
            key,
            &icons::CIRCLE_CHECK,
            &format!("Updated plan · {}", super::plan_strip::plan_summary(plan)),
            None,
        ),
        // Drawn by the transcript list with the chat's drafts; never reached.
        Row::Question { .. } => rune::el(),
        Row::Delegated { key, title, body } => card(s, format!("{key}-delegated"))
            .gap(8.)
            .child(activity_line(s, key, &icons::BOT, title, None))
            .child(visual::reply(s, thread, &format!("{key}-body"), body, false, k)),
        Row::Gate {
            key,
            run_id,
            gate_ref,
            kind,
            headline,
            body,
            allow_always,
            state,
            reason,
        } => {
            let card = render_gate(
                s,
                chat,
                Gate {
                    key,
                    run_id,
                    gate_ref,
                    kind: *kind,
                    headline,
                    body,
                    allow_always: *allow_always,
                    state: *state,
                    reason: reason.as_deref(),
                },
                k,
            );
            // An approval card with its "Reject with feedback…" field open.
            match reject_feedback::render_reject_editor(chat, gate_ref, k) {
                Some(editor) => col().w_full().child(card).child(editor),
                None => card,
            }
        }
        Row::RunNotice {
            key,
            run_id,
            text: words,
            failed,
            retryable,
        } => {
            let ink = if *failed { p.danger } else { p.text };
            let mut out = card(s, format!("{key}-notice")).child(
                row()
                    .gap(8.)
                    .items_start()
                    .child(
                        icon(
                            if *failed {
                                &icons::TRIANGLE_ALERT
                            } else {
                                &icons::INFO
                            },
                            14.,
                        )
                        .color(ink)
                        .mt(2.),
                    )
                    .child(
                        text(words.clone())
                            .key(format!("{key}-text"))
                            .font(flow::type_at(s, 13., 19., 400.))
                            .color(ink)
                            .wrap()
                            .selectable(true)
                            .grow(1.)
                            .min_w(0.),
                    ),
            );
            if *retryable || *failed {
                let mut buttons = row().gap(4.);
                if *retryable {
                    let (thread, run) = (thread.to_string(), run_id.clone());
                    buttons = buttons.child(
                        Button::new("Retry")
                            .control_size(ControlSize::Regular)
                            .on_press(move |s: &mut State| actions::retry(s, &thread, run.clone()))
                            .build(k)
                            .key(format!("{key}-retry")),
                    );
                }
                if *failed {
                    let run = run_id.clone();
                    buttons = buttons.child(
                        Button::new("Logs")
                            .variant(ButtonVariant::Ghost)
                            .control_size(ControlSize::Regular)
                            .help("Gateway log lines for this run")
                            .on_press(move |s: &mut State| crate::workspace::open_run_logs(s, run.clone()))
                            .build(k)
                            .key(format!("{key}-logs")),
                    );
                }
                out = out.child(buttons);
            }
            out
        }
        Row::Command {
            key,
            input,
            title,
            fields,
            lines,
            failed,
            ..
        } => {
            let body = flow::type_at(s, 13., 19., 400.);
            let mut out = card(s, format!("{key}-command")).child(
                row()
                    .gap(8.)
                    .center_y()
                    .child(icon(&icons::SQUARE_TERMINAL, 12.).color(p.text_muted))
                    .child(
                        text(input.clone())
                            .key(format!("{key}-input"))
                            .font(small)
                            .color(p.text_muted)
                            .selectable(true),
                    ),
            );
            if !title.is_empty() {
                out = out.child(text(title.clone()).font(flow::type_at(s, 13., 19., 500.)).color(p.text));
            }
            for (ix, f) in fields.iter().enumerate() {
                out = out.child(
                    row()
                        .key(format!("{key}-field-{ix}"))
                        .gap(12.)
                        .child(text(f.label.clone()).font(body).color(p.text_muted).w(120.))
                        .child(
                            text(f.value.clone())
                                .font(body)
                                .color(p.text)
                                .wrap()
                                .selectable(true)
                                .grow(1.)
                                .min_w(0.),
                        ),
                );
            }
            for (ix, line) in lines.iter().enumerate() {
                out = out.child(
                    text(line.clone())
                        .key(format!("{key}-line-{ix}"))
                        .font(body)
                        .color(if *failed { p.danger } else { p.text })
                        .wrap()
                        .selectable(true),
                );
            }
            out
        }
    }
}

/// A plain card: the hairline frame cards share.
fn card(s: &State, key: String) -> El {
    let p = &s.palette;
    col().key(key)
        .w_full()
        .gap(8.)
        .padding(12.)
        .radius(12.)
        .border(1., p.border)
        .fill(p.surface_card)
}

/// An icon, a label and an optional muted detail on one line.
fn activity_line(s: &State, key: &str, glyph: &'static Icon, label: &str, detail: Option<String>) -> El {
    let p = &s.palette;
    let small = flow::type_at(s, 12., 18., 400.);
    let mut out = row()
        .key(format!("{key}-line"))
        .gap(8.)
        .center_y()
        .child(icon(glyph, 12.).color(p.text_muted))
        .child(text(label.to_string()).font(small).color(p.text_muted).truncate().min_w(0.));
    if let Some(detail) = detail.filter(|d| !d.trim().is_empty()) {
        out = out.child(
            text(detail.split_whitespace().collect::<Vec<_>>().join(" "))
                .font(small)
                .color(p.text_faint)
                .truncate()
                .min_w(0.)
                .shrink(),
        );
    }
    out
}

/// One run of a sent message: words, a placed file, or a mention.
enum Placed<'a> {
    Text(&'a str),
    File(usize),
    Mention(MentionSpan),
}

/// `text` split at its attachment anchors and its mention links.
fn placed_parts(text: &str) -> Vec<Placed<'_>> {
    let mut out = Vec::new();
    for part in anchored_parts(text) {
        match part {
            AnchoredPart::Attachment(ix) => out.push(Placed::File(ix)),
            AnchoredPart::Text(run) => {
                out.extend(mention::mention_parts(run).into_iter().map(|p| match p {
                    MentionPart::Text(t) => Placed::Text(t),
                    MentionPart::Mention(span) => Placed::Mention(span),
                }))
            }
        }
    }
    out
}

/// A sent message whose files and mentions sit between its words: each
/// word and each card is one item of a wrapping row, so the cards flow with
/// the text.
fn placed_body(s: &State, thread: &str, key: &str, parts: &[Placed<'_>], chips: &[AttachmentChip]) -> El {
    let p = &s.palette;
    let font = flow::type_at(s, 14., 22., 400.);
    let line = flow::ui(s, 22.);
    let mut items: Vec<El> = Vec::new();
    for part in parts {
        match part {
            Placed::Mention(span) => {
                let glyph = menu::kind_icon(menu::uri_kind(&span.uri));
                let card = inline_files::inline_card(
                    s,
                    format!("{key}-mention-{}", items.len()),
                    Some(glyph),
                    &mention::chip_text(&span.label),
                    line,
                );
                let card = match &span.uri {
                    MentionUri::File { path, .. }
                    | MentionUri::Directory { path }
                    | MentionUri::Selection { path, .. } => {
                        let path = PathBuf::from(path);
                        card.cursor(rune::Cursor::Pointer)
                            .on_press(move |_: &mut State| actions::open_with_system(&path))
                    }
                    _ => card,
                };
                items.push(card);
            }
            Placed::Text(run) => {
                for (n, words) in run.split('\n').enumerate() {
                    if n > 0 {
                        // A full-width item ends the row: the line break.
                        items.push(el().key(format!("{key}-br-{}", items.len())).w_full());
                    }
                    for word in words.split(' ').filter(|w| !w.is_empty()) {
                        items.push(
                            text(word.to_string())
                                .key(format!("{key}-w-{}", items.len()))
                                .font(font)
                                .color(p.text)
                                .selectable(true),
                        );
                    }
                }
            }
            Placed::File(ix) => {
                let Some(chip) = chips.get(*ix) else {
                    continue;
                };
                let card = inline_files::inline_card(
                    s,
                    format!("{key}-inline-{ix}-{}", items.len()),
                    None,
                    &chip.filename,
                    line,
                );
                items.push(openable(card, thread, chip));
            }
        }
    }
    row().key(format!("{key}-placed"))
        .flex_wrap()
        .center_y()
        .gap(4.)
        .selection_group(true)
        .children(items)
}

/// A sent file opens on click: the backend's copy from disk, or the upload
/// fetched from the gateway.
fn openable(card: El, thread: &str, chip: &AttachmentChip) -> El {
    if let Some(path) = chip.local_path.clone() {
        return card
            .cursor(rune::Cursor::Pointer)
            .on_press(move |_: &mut State| actions::open_with_system(std::path::Path::new(&path)));
    }
    if chip.message_id.is_some() {
        let (chip, thread) = (chip.clone(), thread.to_string());
        return card
            .cursor(rune::Cursor::Pointer)
            .on_press(move |s: &mut State| actions::open_attachment(s, &thread, chip.clone()));
    }
    card
}

/// A gate row's fields.
struct Gate<'a> {
    key: &'a str,
    run_id: &'a str,
    gate_ref: &'a str,
    kind: ProductGateKind,
    headline: &'a str,
    body: &'a Option<String>,
    allow_always: bool,
    state: GateState,
    reason: Option<&'a str>,
}

/// Zeron's chip language: a hairline card with an icon tile, a quiet label
/// and the request. An open approval is the one thing waiting on the person,
/// so its rim warms toward the warning color. A settled gate folds into its
/// activity group (see `flow`); this only draws gates that still ask or are
/// being answered.
fn render_gate(s: &State, chat: &ChatState, g: Gate<'_>, k: &Kit) -> El {
    let p = &s.palette;
    let thread = chat.thread_id.clone();
    let decide = |label: &str, suffix: &str, decision: GateDecision, variant: ButtonVariant| {
        let (run, gate, thread) = (g.run_id.to_string(), g.gate_ref.to_string(), thread.clone());
        Button::new(label.to_string())
            .variant(variant)
            .control_size(ControlSize::Regular)
            .on_press_tx(move |s: &mut State, tx| {
                actions::resolve_gate(s, &thread, run.clone(), gate.clone(), decision, tx)
            })
            .build(k)
            .key(format!("{}-{suffix}", g.key))
    };
    let small = flow::type_at(s, 12., 16., 400.);
    let actions_row = match (g.state, g.kind) {
        (GateState::Closed(_), _) => None,
        (GateState::Resolving(decision), _) => Some(
            row()
                .gap(8.)
                .center_y()
                .child(rune_kit::Spinner::new().size(12.).build(k))
                .child(
                    text(match decision {
                        GateDecision::Decline => "Declined, waiting for IronClaw to confirm",
                        GateDecision::Approve { .. } => "Approved, waiting for IronClaw to confirm",
                    })
                    .font(small)
                    .color(p.text_muted),
                ),
        ),
        // Sign-in flows run in the browser; the gate clears when they finish.
        (GateState::Open, ProductGateKind::Auth) => Some(
            row().child(
                text("Finish this sign-in from the browser (sidebar, Open in browser).")
                    .font(small)
                    .color(p.text_muted)
                    .wrap(),
            ),
        ),
        (GateState::Open, kind) => {
            let mut buttons = row().gap(8.).flex_wrap().child(decide(
                "Approve  ⌘↵",
                "approve",
                GateDecision::Approve { always: false },
                ButtonVariant::Contrast,
            ));
            if g.allow_always {
                buttons = buttons.child(decide(
                    "Always allow  ⇧⌘↵",
                    "always",
                    GateDecision::Approve { always: true },
                    ButtonVariant::Neutral,
                ));
            }
            buttons = buttons.child(decide(
                "Decline",
                "decline",
                GateDecision::Decline,
                ButtonVariant::Ghost,
            ));
            // "No, do X instead" explains a rejected tool call; other gate
            // kinds have no call to explain.
            if matches!(kind, ProductGateKind::Approval) {
                buttons = buttons.child(reject_feedback::reject_button(chat, g.key, g.run_id, g.gate_ref, k));
            }
            Some(buttons)
        }
    };
    let open = matches!(g.state, GateState::Open);
    let ink = |a: f32| {
        if p.light {
            Color::BLACK.alpha(a)
        } else {
            Color::WHITE.alpha(a)
        }
    };
    let label = match g.kind {
        ProductGateKind::Auth => "Sign-in",
        ProductGateKind::Approval => "Approval",
        _ => "Question",
    };
    let line12 = flow::type_at(s, 12., 18., 400.);
    let mut card = col()
        .key(format!("{}-gate", g.key))
        .w_full()
        .gap(8.)
        .padding(10.)
        .radius(12.)
        .border(1., if open { p.warning.alpha(0.45) } else { ink(0.08) })
        .fill(ink(0.045))
        .transition(flow::rise(8.))
        .child(
            row()
                .gap(8.)
                .center_y()
                .child(
                    row()
                        .size(20., 20.)
                        .radius(6.)
                        .fill(ink(0.09))
                        .center()
                        .child(
                            icon(&icons::TRIANGLE_ALERT, 12.)
                                .color(if open { p.warning } else { p.text_muted }),
                        ),
                )
                .child(
                    text(label)
                        .font(flow::type_at(s, 12., 18., 500.))
                        .color(p.text_muted),
                )
                .child(
                    text(g.headline.to_string())
                        .font(line12)
                        .color(p.text.alpha(0.9))
                        .wrap()
                        .selectable(true)
                        .grow(1.)
                        .min_w(0.),
                ),
        );
    if let Some(body) = g.body {
        card = card.child(
            text(body.clone())
                .font(flow::type_at(s, 11.5, 17., 400.).mono())
                .color(p.text_muted)
                .wrap()
                .max_lines(9)
                .selectable(true)
                .pl(28.),
        );
    }
    if let Some(line) = approval_reason::gate_line(s, g.reason) {
        card = card.child(line);
    }
    if let Some(actions_row) = actions_row {
        card = card.child(el().pl(28.).child(actions_row));
    }
    card
}

/// The inline marker where a thread switched provider:
/// "Switched to claude-opus-5-5 · Claude CLI", with the model id prettified.
fn switch_marker(s: &State, words: &str) -> El {
    let p = &s.palette;
    let rest = words.trim_start_matches("Switched to ");
    let label = match rest.split_once(" · ") {
        Some((model, provider)) => format!(
            "Switched to {} · {provider}",
            super::model_picker::pretty_model(model)
        ),
        None => words.to_string(),
    };
    let rule = || el().h(1.).grow(1.).fill(p.border);
    row().w_full()
        .gap(8.)
        .center_y()
        .child(rule())
        .child(
            text(label)
                .font(flow::type_at(s, 12., 16., 400.))
                .color(p.text_muted),
        )
        .child(rule())
}

/// Fork the conversation from run `at_run` (area H's orchestration, which
/// works on the open chat; a side chat's rows offer no fork).
fn fork(s: &mut State, thread: &str, at_run: Option<u32>) {
    if super::is_main(s, thread) {
        super::orchestration::fork(s, at_run);
    }
}
