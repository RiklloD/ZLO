//! The Changes tab on a Codex thread: what Codex itself computed and can do.
//!
//! - **Codex diff**: the turn diff Codex reports while it works
//!   (`turn/diff/updated`, file-change previews before it), as its own scope.
//! - **Review with Codex**: `review/start` on uncommitted changes, against a
//!   base branch, the last commit, or custom instructions. The review runs
//!   as a turn of the thread (its findings are the reply); the findings with
//!   a location also land on the diff as annotations.
//! - **Undo**: on the last turn's card, one coordinated operation
//!   ([`ironclaw_gpui_client::codex_undo`]): Codex reverts the turn the
//!   checkpoint was taken for (by run id, in this folder's repository only),
//!   then the files are restored from the checkpoint. A refusal touches
//!   nothing; a failed restore keeps the checkpoint for "Restore files".
//! - **One repository**: every action names this folder's repository; when
//!   Codex works elsewhere it refuses, and Review and Undo stay disabled
//!   with the reason shown.
//! - **Compact**: `thread/compact/start` now.

use ironclaw_gpui_client::codex_undo::{self, AfterRevert, UndoOutcome};
use ironclaw_gpui_client::review::{self, ReviewFinding};
use ironclaw_gpui_client::{ReviewTarget, ThreadAction, ThreadActionOutcome, ThreadActionRefusal};

use super::Scope;
use crate::chat::ChatView;
use crate::design::Palette;
use crate::ui::*;

/// The review targets offered, in menu order.
const TARGETS: [(&str, &str); 3] = [
    ("uncommitted", "Uncommitted changes"),
    ("base", "Against the base branch"),
    ("commit", "The last commit"),
];

#[derive(Default)]
pub(in crate::chat::side) struct State {
    /// Codex refused an action here: it works in this other root. Review
    /// and Undo stay disabled for this folder until the folder changes.
    pub(in crate::chat::side) mismatch: Option<String>,
    /// The target picker is open.
    menu: bool,
    custom: Option<(Entity<InputState>, Subscription)>,
    note: Option<String>,
    error: Option<String>,
}

impl ChatView {
    /// The thread runs on the Codex CLI.
    pub(in crate::chat::side) fn is_codex(&self) -> bool {
        self.thread_runtime() == Some("codex")
    }

    /// Codex's latest turn diff, empty before it reported one.
    pub(in crate::chat::side) fn codex_patch(&self) -> String {
        self.transcript
            .cli_turn_diff()
            .map(|d| d.body.clone())
            .unwrap_or_default()
    }

    /// The latest review's findings, each with its path relative to the repo
    /// (the diff's file paths).
    pub(in crate::chat::side) fn codex_findings(&self) -> Vec<(String, ReviewFinding)> {
        let Some(text) = self.transcript.cli_review() else {
            return Vec::new();
        };
        let root = self.side.repo.clone().unwrap_or_default();
        review::parse(&text.body)
            .findings
            .into_iter()
            .map(|f| (f.relative_to(&root), f))
            .collect()
    }

    /// This folder's repository as Codex is told it: canonical, absolute.
    fn codex_repo(&self) -> Option<String> {
        let repo = self.side.repo.as_ref()?;
        Some(
            std::fs::canonicalize(repo)
                .unwrap_or_else(|_| repo.clone()) // silent-ok: compared by Codex, never opened here
                .display()
                .to_string(),
        )
    }

    /// Remember a workspace refusal: Codex works in another root.
    fn note_refusal(&mut self, refusal: &ThreadActionRefusal) {
        if let ThreadActionRefusal::WorkspaceMismatch { runtime_root } = refusal {
            self.side.changes.codex.mismatch = Some(runtime_root.clone());
        }
    }

    /// Ask the thread's runtime for `action`; the answer lands as a note.
    /// Shares the tab's one busy slot with git actions and Undo.
    pub(in crate::chat::side) fn codex_action(
        &mut self,
        action: ThreadAction,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        if self.side.changes.busy.is_some() {
            return;
        }
        self.side.changes.busy = Some(label);
        let state = &mut self.side.changes.codex;
        state.menu = false;
        state.error = None;
        state.note = None;
        cx.notify();
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.thread_action(&thread_id, &action).await });
        cx.spawn(async move |this, cx| {
            let result = crate::jobs::flatten(job.await);
            let _ = this.update(cx, |this, cx| {
                this.side.changes.busy = None;
                if let Ok(ThreadActionOutcome::Refused { refusal }) = &result {
                    this.note_refusal(refusal);
                }
                let state = &mut this.side.changes.codex;
                match result {
                    Ok(ThreadActionOutcome::Started { .. }) => {
                        state.note =
                            Some("Codex is reviewing; its findings arrive as the reply.".into());
                        // The review streams like a turn.
                        this.refresh_orchestration(cx);
                    }
                    Ok(ThreadActionOutcome::Done { note }) => {
                        state.note = Some(note);
                        this.load_timeline(cx);
                    }
                    Ok(ThreadActionOutcome::Busy) => {
                        state.error = Some(UndoOutcome::Busy.message());
                    }
                    Ok(ThreadActionOutcome::Refused { refusal }) => {
                        state.error = Some(UndoOutcome::Refused(refusal).message());
                    }
                    Err(e) => state.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn codex_review(&mut self, which: &str, cx: &mut Context<Self>) {
        let target = match which {
            "base" => {
                let current = self.side.changes.branch.name.clone();
                let branch = if current == "main" || current == "master" {
                    self.side
                        .changes
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
        let Some(repo) = self.codex_repo() else {
            return;
        };
        self.codex_action(ThreadAction::Review { target, repo }, "review", cx);
    }

    /// Undo the last turn on a Codex thread as one operation: Codex reverts
    /// the checkpoint's run first; only then are the files restored. With
    /// the revert already done (a restore that failed), only the restore
    /// runs again.
    pub(in crate::chat::side) fn codex_undo(&mut self, cx: &mut Context<Self>) {
        let state = &self.side.changes;
        if state.busy.is_some() {
            return;
        }
        let (Some(repo), Some(cp)) = (self.codex_repo(), state.checkpoint.clone()) else {
            return;
        };
        let restore_only = state.restore_pending;
        let run = state.checkpoint_run.clone();
        if !restore_only && run.is_none() {
            self.side.changes.codex.error =
                Some("This turn has no run to revert in Codex; nothing was undone.".into());
            cx.notify();
            return;
        }
        self.side.changes.busy = Some("undo");
        self.side.changes.codex.error = None;
        self.side.changes.codex.note = None;
        cx.notify();
        let thread_id = self.thread_id.clone();
        let revert = (!restore_only).then(|| {
            let action = ThreadAction::RevertTurn {
                run_id: run.unwrap_or_default(),
                repo: repo.clone(),
            };
            self.backend
                .spawn(move |c| async move { c.thread_action(&thread_id, &action).await })
        });
        let dir = self.side.repo.clone();
        cx.spawn(async move |this, cx| {
            let next = match revert {
                Some(job) => codex_undo::after_revert(crate::jobs::flatten(job.await)),
                None => AfterRevert::Restore {
                    note: "Codex already reverted the turn".into(),
                },
            };
            let outcome = match next {
                AfterRevert::Stop(outcome) => outcome,
                AfterRevert::Restore { note } => {
                    let restored = match dir {
                        Some(dir) => {
                            cx.background_spawn(async move { super::git::undo_turn(&dir, &cp) })
                                .await
                        }
                        None => Err("no repository".to_string()),
                    };
                    codex_undo::after_restore(note, restored)
                }
            };
            let _ = this.update(cx, |this, cx| this.codex_undo_finished(outcome, cx));
        })
        .detach();
    }

    fn codex_undo_finished(&mut self, outcome: UndoOutcome, cx: &mut Context<Self>) {
        if let UndoOutcome::Refused(refusal) = &outcome {
            self.note_refusal(refusal);
        }
        let state = &mut self.side.changes;
        state.busy = None;
        state.restore_pending = outcome.restore_pending();
        if !outcome.keeps_checkpoint() {
            state.checkpoint = None;
            state.checkpoint_run = None;
            state.turn_stats = None;
            state.turn_patch.clear();
            state.scope = super::Scope::Working;
        }
        match &outcome {
            UndoOutcome::Done { .. } => state.codex.note = Some(outcome.message()),
            _ => state.codex.error = Some(outcome.message()),
        }
        self.changes_refresh(cx);
        self.load_timeline(cx);
        cx.notify();
    }

    fn codex_custom_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((input, _)) = self.side.changes.codex.custom.as_ref() else {
            return;
        };
        let instructions = input.read(cx).value().trim().to_string();
        if instructions.is_empty() {
            return;
        }
        let Some(repo) = self.codex_repo() else {
            return;
        };
        input.update(cx, |s, cx| s.set_value("", window, cx));
        self.codex_action(
            ThreadAction::Review {
                target: ReviewTarget::Custom { instructions },
                repo,
            },
            "review",
            cx,
        );
    }

    /// The Codex card: review (with its target picker), the Codex diff
    /// scope, revert and compact, and the latest findings.
    pub(in crate::chat::side) fn render_codex_card(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        if self.side.changes.codex.custom.is_none() {
            let input = cx
                .new(|cx| InputState::new(window, cx).placeholder("Or tell Codex what to review"));
            let sub = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.codex_custom_review(window, cx);
                }
            });
            self.side.changes.codex.custom = Some((input, sub));
        }
        let has_diff = self.transcript.cli_turn_diff().is_some();
        let findings = self.codex_findings();
        let state = &self.side.changes;
        let scope = state.scope;
        let busy = state.busy;
        let mismatch = state.codex.mismatch.clone();
        let menu = state.codex.menu;
        let mut card = v_flex()
            .mx_2()
            .mt_1()
            .p_2()
            .gap_1p5()
            .rounded(px(8.))
            .border_1()
            .border_color(p.border)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child("Codex"),
                    )
                    .when(has_diff, |d| {
                        d.child(
                            Button::new("codex-diff")
                                .label(if scope == Scope::Codex {
                                    "Show all changes"
                                } else {
                                    "Codex diff"
                                })
                                .xsmall()
                                .ghost()
                                .tooltip("The diff Codex computed for its last turn")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let next = if scope == Scope::Codex {
                                        Scope::Working
                                    } else {
                                        Scope::Codex
                                    };
                                    this.set_scope(next, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("codex-compact")
                            .label("Compact")
                            .xsmall()
                            .ghost()
                            .loading(busy == Some("compact"))
                            .disabled(busy.is_some())
                            .tooltip("Summarize the thread's context in Codex now")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.codex_action(ThreadAction::Compact, "compact", cx)
                            })),
                    )
                    .child(
                        Button::new("codex-review")
                            .label("Review with Codex")
                            .icon(IconName::Search)
                            .xsmall()
                            .outline()
                            .selected(menu)
                            .loading(busy == Some("review"))
                            .disabled(busy.is_some() || mismatch.is_some())
                            .on_click(cx.listener(|this, _, _, cx| {
                                let state = &mut this.side.changes.codex;
                                state.menu = !state.menu;
                                cx.notify();
                            })),
                    ),
            );
        if menu {
            let mut picker = v_flex().gap_1();
            for (id, label) in TARGETS {
                picker = picker.child(
                    Button::new(SharedString::from(format!("codex-review-{id}")))
                        .label(label)
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| this.codex_review(id, cx))),
                );
            }
            if let Some((input, _)) = state.codex.custom.as_ref() {
                picker = picker.child(
                    h_flex()
                        .gap_1()
                        .child(div().flex_1().child(Input::new(input).small()))
                        .child(
                            Button::new("codex-review-custom")
                                .label("Review")
                                .xsmall()
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.codex_custom_review(window, cx)
                                })),
                        ),
                );
            }
            card = card.child(picker);
        }
        if let Some(root) = mismatch {
            card = card.child(div().text_xs().text_color(p.text_muted).child(format!(
                "Codex works in {root}, not this folder: Review and Undo are off here."
            )));
        }
        if let Some(note) = state.codex.note.clone() {
            card = card.child(div().text_xs().text_color(p.success).child(note));
        }
        if let Some(error) = state.codex.error.clone() {
            card = card.child(div().text_xs().text_color(p.danger).child(error));
        }
        if !findings.is_empty() {
            let mut list =
                v_flex()
                    .gap_0p5()
                    .child(div().text_xs().text_color(p.text_muted).child(format!(
                        "{} finding{} from the last review (shown on the diff)",
                        findings.len(),
                        if findings.len() == 1 { "" } else { "s" }
                    )));
            for (path, f) in findings.iter().take(8) {
                list = list.child(
                    h_flex()
                        .gap_1()
                        .text_xs()
                        .min_w_0()
                        .when(f.priority.is_some(), |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_color(priority_color(f.priority, &p))
                                    .child(f.priority_label()),
                            )
                        })
                        .child(div().min_w_0().truncate().child(f.title.clone()))
                        .child(
                            div()
                                .flex_none()
                                .text_color(p.text_faint)
                                .child(format!("{path}:{}", f.start)),
                        ),
                );
            }
            card = card.child(list);
        }
        card.into_any_element()
    }
}

/// P0/P1 read as danger, P2 as a warning, the rest muted.
pub(super) fn priority_color(priority: Option<u8>, p: &Palette) -> Hsla {
    match priority {
        Some(0 | 1) => p.danger,
        Some(2) => p.accent,
        _ => p.text_muted,
    }
}
