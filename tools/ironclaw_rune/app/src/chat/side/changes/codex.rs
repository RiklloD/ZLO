//! The Changes tab on a Codex thread: what Codex itself computed and can do
//! (GPUI `chat/side/changes/codex.rs`).
//!
//! - **Codex diff**: the turn diff Codex reports while it works
//!   (`turn/diff/updated`, file-change previews before it), as its own scope.
//! - **Review with Codex**: `review/start` on uncommitted changes, against a
//!   base branch, the last commit, or custom instructions. The review runs
//!   as a turn of the thread (its findings are the reply); the findings with
//!   a location also land on the diff as annotations.
//! - **Undo** stays the tab's own checkpoint restore (files only), on Codex
//!   threads too: reverting the turn in Codex's conversation is a follow-up
//!   (it needs a crash-safe pairing with the file restore).
//! - **One repository**: Review names this folder's repository; when Codex
//!   works elsewhere it refuses, and Review stays disabled for the folder
//!   with the reason shown.
//! - **Compact**: `thread/compact/start` now.

use ironclaw_gpui_client::review::{self, ReviewFinding};
use ironclaw_gpui_client::{ReviewTarget, ThreadAction, ThreadActionOutcome, ThreadActionRefusal};
use rune::{Color, El, TextEdit, col, row};
use rune_kit::{Kit, TextInput};

use super::super::ui::{self, XS, XS_MED};
use super::{Scope, set_scope};
use crate::chat::side::{bridge, side, side_mut, side_of, side_total, thread_id};
use crate::design::Palette;
use crate::icons;
use crate::jobs;
use crate::state::State;

/// The review targets offered, in menu order.
const TARGETS: [(&str, &str); 3] = [
    ("uncommitted", "Uncommitted changes"),
    ("base", "Against the base branch"),
    ("commit", "The last commit"),
];

#[derive(Default)]
pub(in crate::chat::side) struct CodexState {
    /// Codex refused a review here: it works in this other root. Review
    /// stays disabled for this folder until the folder changes.
    pub(in crate::chat::side) mismatch: Option<String>,
    /// The target picker is open.
    menu: bool,
    custom: TextEdit,
    note: Option<String>,
    error: Option<String>,
}

/// Codex's latest turn diff, empty before it reported one.
pub(super) fn patch(s: &State) -> String {
    bridge::cli_turn_diff(s)
        .map(|d| d.body.clone())
        .unwrap_or_default()
}

/// The latest review's findings, each with its path relative to the repo
/// (the diff's file paths).
pub(super) fn findings(s: &State) -> Vec<(String, ReviewFinding)> {
    let Some(text) = bridge::cli_review(s) else {
        return Vec::new();
    };
    let root = side(s).and_then(|x| x.repo.clone()).unwrap_or_default();
    review::parse(&text.body)
        .findings
        .into_iter()
        .map(|f| (f.relative_to(&root), f))
        .collect()
}

/// This folder's repository as Codex is told it: canonical, absolute.
fn repo(s: &State) -> Option<String> {
    let repo = side(s)?.repo.as_ref()?;
    Some(
        std::fs::canonicalize(repo)
            .unwrap_or_else(|_| repo.clone()) // silent-ok: compared by Codex, never opened here
            .display()
            .to_string(),
    )
}

/// Ask the thread's runtime for `action`; the answer lands as a note.
/// Shares the tab's one busy slot with the git actions.
fn action(s: &mut State, action: ThreadAction, label: &'static str) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    if x.changes.busy.is_some() {
        return;
    }
    x.changes.busy = Some(label);
    let state = &mut x.changes.codex;
    state.menu = false;
    state.error = None;
    state.note = None;
    let asked = thread.clone();
    jobs::run(
        s,
        "codex",
        move |c| async move { c.thread_action(&asked, &action).await },
        move |s, result, _| {
            let Some(x) = side_of(s, &thread) else { return };
            x.changes.busy = None;
            if let Ok(ThreadActionOutcome::Refused {
                refusal: ThreadActionRefusal::WorkspaceMismatch { runtime_root },
            }) = &result
            {
                x.changes.codex.mismatch = Some(runtime_root.clone());
            }
            let state = &mut x.changes.codex;
            match result {
                Ok(ThreadActionOutcome::Started { .. }) => {
                    state.note =
                        Some("Codex is reviewing; its findings arrive as the reply.".into());
                    // The review streams like a turn.
                    bridge::refresh_orchestration(s);
                }
                Ok(ThreadActionOutcome::Done { note }) => {
                    state.note = Some(note);
                    bridge::load_timeline(s);
                }
                Ok(ThreadActionOutcome::Busy) => {
                    state.error =
                        Some("Codex is busy with this thread; try again when it is done.".into());
                }
                Ok(ThreadActionOutcome::Refused {
                    refusal: ThreadActionRefusal::WorkspaceMismatch { runtime_root },
                }) => {
                    state.error = Some(format!(
                        "Codex works in {runtime_root}, not this folder: nothing was reviewed."
                    ));
                }
                Err(e) => state.error = Some(e.to_string()),
            }
        },
    );
}

fn review_target(s: &mut State, which: &str) {
    let target = match which {
        "base" => {
            let Some(x) = side(s) else { return };
            let current = x.changes.branch.name.clone();
            let branch = if current == "main" || current == "master" {
                x.changes
                    .branch
                    .upstream
                    .clone()
                    .unwrap_or_else(|| format!("origin/{current}"))
            } else {
                "main".to_string()
            };
            ReviewTarget::BaseBranch { branch }
        }
        "commit" => ReviewTarget::Commit {
            sha: "HEAD".into(),
            title: Some("the last commit".into()),
        },
        _ => ReviewTarget::UncommittedChanges,
    };
    let Some(repo) = repo(s) else { return };
    action(s, ThreadAction::Review { target, repo }, "review");
}

fn custom_review(s: &mut State) {
    let Some(x) = side_mut(s) else { return };
    let instructions = x.changes.codex.custom.text.trim().to_string();
    if instructions.is_empty() {
        return;
    }
    let Some(repo) = repo(s) else { return };
    if let Some(x) = side_mut(s) {
        x.changes.codex.custom.clear();
    }
    action(
        s,
        ThreadAction::Review {
            target: ReviewTarget::Custom { instructions },
            repo,
        },
        "review",
    );
}

/// The Codex card: review (with its target picker), the Codex diff scope,
/// compact, and the latest findings.
pub(super) fn card(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let Some(x) = side(s) else { return col() };
    let has_diff = bridge::cli_turn_diff(s).is_some();
    let findings = findings(s);
    let state = &x.changes;
    let scope = state.scope;
    let busy = state.busy;
    let mismatch = state.codex.mismatch.clone();
    let menu = state.codex.menu;
    let mut head = row()
        .gap(4.)
        .items_center()
        .child(ui::line("Codex", XS_MED, p.text).grow(1.));
    if has_diff {
        head = head.child(
            ui::btn(if scope == Scope::Codex {
                "Show all changes"
            } else {
                "Codex diff"
            })
            .help("The diff Codex computed for its last turn")
            .on_press(move |s: &mut State| {
                set_scope(
                    s,
                    if scope == Scope::Codex {
                        Scope::Working
                    } else {
                        Scope::Codex
                    },
                )
            })
            .build(k)
            .key("codex-diff"),
        );
    }
    head = head
        .child(
            ui::busy(ui::btn("Compact"), busy == Some("compact"))
                .disabled(busy.is_some())
                .help("Summarize the thread's context in Codex now")
                .on_press(|s: &mut State| action(s, ThreadAction::Compact, "compact"))
                .build(k)
                .key("codex-compact"),
        )
        .child(ui::selected(
            s,
            ui::busy(
                ui::outline("Review with Codex").icon(&icons::SEARCH),
                busy == Some("review"),
            )
            .disabled(busy.is_some() || mismatch.is_some())
            .expanded(menu)
            .on_press(|s: &mut State| {
                if let Some(x) = side_mut(s) {
                    x.changes.codex.menu = !x.changes.codex.menu;
                }
            })
            .build(k)
            .key("codex-review"),
            menu,
        ));
    let mut card = col()
        .key("codex-card")
        .mx(8.)
        .mt(4.)
        .padding(8.)
        .gap(6.)
        .radius(8.)
        .border(1., p.border)
        .child(head);
    if menu {
        let mut picker = col().gap(4.);
        for (id, label) in TARGETS {
            picker = picker.child(
                ui::btn(label)
                    .on_press(move |s: &mut State| review_target(s, id))
                    .build(k)
                    .key(format!("codex-review-{id}")),
            );
        }
        picker = picker.child(
            row()
                .gap(4.)
                .child(
                    row().grow(1.).min_w(0.).child(
                        TextInput::new(&state.codex.custom)
                            .placeholder("Or tell Codex what to review")
                            .label("Review instructions")
                            .on_edit(|s: &mut State| &mut side_total(s).changes.codex.custom)
                            .on_submit(|s: &mut State, _| custom_review(s))
                            .build(k)
                            .key("codex-review-input"),
                    ),
                )
                .child(
                    ui::primary("Review")
                        .on_press(custom_review)
                        .build(k)
                        .key("codex-review-custom"),
                ),
        );
        card = card.child(picker);
    }
    if let Some(root) = mismatch {
        card = card.child(ui::para(
            format!("Codex works in {root}, not this folder: Review is off here."),
            XS,
            p.text_muted,
        ));
    }
    if let Some(note) = state.codex.note.clone() {
        card = card.child(ui::para(note, XS, p.success));
    }
    if let Some(error) = state.codex.error.clone() {
        card = card.child(ui::para(error, XS, p.danger));
    }
    if !findings.is_empty() {
        let mut list = col().gap(2.).child(ui::line(
            format!(
                "{} finding{} from the last review (shown on the diff)",
                findings.len(),
                if findings.len() == 1 { "" } else { "s" }
            ),
            XS,
            p.text_muted,
        ));
        for (path, f) in findings.iter().take(8) {
            let mut line = row().gap(4.).min_w(0.);
            if f.priority.is_some() {
                line = line.child(ui::line(
                    f.priority_label(),
                    XS,
                    priority_color(f.priority, p),
                ));
            }
            list = list.child(
                line.child(ui::line(f.title.clone(), XS, p.text).shrink().min_w(0.))
                    .child(ui::line(format!("{path}:{}", f.start), XS, p.text_faint)),
            );
        }
        card = card.child(list);
    }
    card
}

/// P0/P1 read as danger, P2 as the accent, the rest muted.
pub(super) fn priority_color(priority: Option<u8>, p: &Palette) -> Color {
    match priority {
        Some(0 | 1) => p.danger,
        Some(2) => p.accent,
        _ => p.text_muted,
    }
}
