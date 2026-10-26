//! Changes: MonoCode Native's `changes.rs` + `diff_view.rs` + session
//! review card (MIT), rebuilt on gpui-component's `Diff` viewer:
//!
//! - the branch with Pull / Push, a commit box, staged and unstaged files
//!   with Stage / Unstage / Discard;
//! - every change in one virtualized, syntax-colored diff, unified or split;
//! - comments on a line or a selected range (Zeron's review comments), sent
//!   to the composer as one message for the agent;
//! - the last turn's card: what the agent changed since its turn began
//!   (a `git stash create` checkpoint), with Review, Keep and Undo.

use std::rc::Rc;

use gpui_kit::component::diff::{
    Diff, DiffAnnotation, DiffFile, DiffLinePosition, DiffLineRange, DiffMode, DiffSide, DiffState,
};

use super::{ChatView, Tab, empty, error_line, git, loading, short_path};
use crate::assets::ExtraIcon;
use crate::design::{Palette, kit};
use crate::ui::*;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Scope {
    #[default]
    Working,
    Turn,
}

struct Comment {
    id: u64,
    range: DiffLineRange,
    text: String,
}

struct Draft {
    range: DiffLineRange,
    input: Entity<InputState>,
    _sub: Subscription,
}

#[derive(Default)]
pub(super) struct State {
    loaded: bool,
    loading: bool,
    stale: bool,
    error: Option<String>,
    branch: git::Branch,
    changes: Vec<git::Change>,
    scope: Scope,
    working_patch: String,
    diff: Option<Entity<DiffState>>,
    /// The patch the viewer holds, so an unchanged refresh keeps its scroll.
    shown: String,
    split: bool,
    wrap: bool,
    busy: Option<&'static str>,
    note: Option<String>,
    /// The destructive button waiting for its second click.
    confirm: Option<String>,
    commit: Option<(Entity<InputState>, Subscription)>,
    comments: Vec<Comment>,
    next_comment: u64,
    draft: Option<Draft>,
    checkpoint: Option<git::Checkpoint>,
    turn_patch: String,
    /// Files, additions and deletions of the last turn.
    turn_stats: Option<(usize, usize, usize)>,
    list_hidden: bool,
}

impl State {
    pub(super) fn has_turn(&self) -> bool {
        self.turn_stats.is_some_and(|(files, _, _)| files > 0)
    }
}

struct Loaded {
    branch: git::Branch,
    changes: Vec<git::Change>,
    working: Result<String, String>,
    turn: Option<Result<String, String>>,
}

fn stats(files: &[DiffFile]) -> (usize, usize, usize) {
    files.iter().fold((0, 0, 0), |(n, a, d), f| {
        (n + 1, a + f.additions(), d + f.deletions())
    })
}

impl ChatView {
    /// Re-read status, branch and both diffs. Coalesces like the orchestration poll.
    pub(super) fn changes_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.side.repo.clone() else {
            return;
        };
        let state = &mut self.side.changes;
        if state.loading {
            state.stale = true;
            return;
        }
        state.loading = true;
        let checkpoint = state.checkpoint.clone();
        let task = cx.background_spawn(async move {
            let changes = git::status(&repo);
            let branch = git::branch(&repo);
            let (changes, working) = match changes {
                Ok(changes) => {
                    let working = git::working_diff(&repo, &changes);
                    (changes, working)
                }
                Err(e) => (Vec::new(), Err(e)),
            };
            let turn = checkpoint.map(|cp| git::turn_diff(&repo, &cp));
            Loaded {
                branch,
                changes,
                working,
                turn,
            }
        });
        cx.spawn(async move |this, cx| {
            let loaded = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.side.changes;
                state.loading = false;
                state.loaded = true;
                state.branch = loaded.branch;
                state.changes = loaded.changes;
                match loaded.working {
                    Ok(patch) => {
                        state.error = None;
                        state.working_patch = patch;
                    }
                    Err(e) => state.error = Some(e),
                }
                match loaded.turn {
                    Some(Ok(patch)) => {
                        let files = DiffFile::parse(&patch).unwrap_or_default();
                        state.turn_stats = Some(stats(&files));
                        state.turn_patch = patch;
                    }
                    Some(Err(e)) => {
                        tracing::debug!(error = %e, "turn diff failed");
                        state.turn_stats = None;
                    }
                    None => state.turn_stats = None,
                }
                if state.scope == Scope::Turn && !state.has_turn() {
                    state.scope = Scope::Working;
                }
                let stale = std::mem::take(&mut state.stale);
                this.changes_apply(cx);
                if stale {
                    this.changes_refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Feed the viewer the patch of the current scope.
    fn changes_apply(&mut self, cx: &mut Context<Self>) {
        let state = &mut self.side.changes;
        let patch = match state.scope {
            Scope::Working => state.working_patch.clone(),
            Scope::Turn => state.turn_patch.clone(),
        };
        if state.diff.is_some() && patch == state.shown {
            return;
        }
        let files = match DiffFile::parse(&patch) {
            Ok(files) => files,
            Err(e) => {
                state.error = Some(format!("Couldn't read the diff: {e}"));
                Vec::new()
            }
        };
        state.shown = patch;
        let mode = if state.split {
            DiffMode::Split
        } else {
            DiffMode::Unified
        };
        match state.diff.clone() {
            Some(diff) => diff.update(cx, |d, cx| d.set_files(files, cx)),
            None => {
                let diff = cx.new(|cx| DiffState::new(files, cx).with_mode(mode));
                self.side.changes.diff = Some(diff);
            }
        }
    }

    /// Record the working tree before a turn, for its card.
    pub(super) fn changes_checkpoint(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.side.repo.clone() else {
            return;
        };
        let task = cx.background_spawn(async move { git::checkpoint(&repo) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.side.changes;
                match result {
                    Ok(cp) => {
                        state.checkpoint = Some(cp);
                        state.turn_stats = None;
                        state.turn_patch.clear();
                    }
                    Err(e) => tracing::debug!(error = %e, "no turn checkpoint"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run a git mutation off the UI thread, then refresh.
    fn git_action(
        &mut self,
        label: &'static str,
        cx: &mut Context<Self>,
        f: impl FnOnce(&std::path::Path) -> Result<Option<String>, String> + Send + 'static,
    ) {
        let Some(repo) = self.side.repo.clone() else {
            return;
        };
        if self.side.changes.busy.is_some() {
            return;
        }
        self.side.changes.busy = Some(label);
        self.side.changes.confirm = None;
        self.side.changes.note = None;
        cx.notify();
        let task = cx.background_spawn(async move { f(&repo) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.side.changes;
                state.busy = None;
                match result {
                    Ok(note) => state.note = note,
                    Err(e) => state.error = Some(e),
                }
                this.changes_refresh(cx);
                this.history_stale();
                cx.notify();
            });
        })
        .detach();
    }

    /// The first click arms a destructive action, the second runs it.
    fn confirmed(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let state = &mut self.side.changes;
        if state.confirm.as_deref() == Some(key) {
            state.confirm = None;
            return true;
        }
        state.confirm = Some(key.to_string());
        cx.notify();
        false
    }

    fn commit_now(&mut self, push: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((input, _)) = self.side.changes.commit.as_ref() else {
            return;
        };
        let message = input.read(cx).value().trim().to_string();
        if message.is_empty() {
            self.side.changes.error = Some("Write a commit message first.".into());
            cx.notify();
            return;
        }
        input.update(cx, |s, cx| s.set_value("", window, cx));
        self.git_action("commit", cx, move |repo| {
            let sha = git::commit(repo, &message)?;
            if push {
                git::push(repo)?;
                return Ok(Some(format!("Committed {sha} and pushed.")));
            }
            Ok(Some(format!("Committed {sha}.")))
        });
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.side.changes.scope = scope;
        self.side.changes.comments.clear();
        self.side.changes.draft = None;
        self.changes_apply(cx);
        cx.notify();
    }

    fn start_draft(&mut self, range: DiffLineRange, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Comment for the agent"));
        let sub = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.save_draft(cx);
            }
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        self.side.changes.draft = Some(Draft {
            range,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    fn save_draft(&mut self, cx: &mut Context<Self>) {
        let state = &mut self.side.changes;
        let Some(draft) = state.draft.take() else {
            return;
        };
        let text = draft.input.read(cx).value().trim().to_string();
        if !text.is_empty() {
            state.next_comment += 1;
            state.comments.push(Comment {
                id: state.next_comment,
                range: draft.range,
                text,
            });
        }
        cx.notify();
    }

    /// All comments as one message in the composer, ready to send.
    fn send_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = &mut self.side.changes;
        if state.comments.is_empty() {
            return;
        }
        let what = match state.scope {
            Scope::Working => "uncommitted changes",
            Scope::Turn => "changes from your last turn",
        };
        let folder = self
            .side
            .repo
            .as_deref()
            .map(short_path)
            .unwrap_or_default();
        let mut text = format!("Review comments on the {what} in {folder}:\n");
        for (ix, c) in state.comments.iter().enumerate() {
            let side = match c.range.end_side() {
                DiffSide::Original => "old",
                DiffSide::Modified => "new",
            };
            let lines = if c.range.start() == c.range.end() {
                format!("line {}", c.range.end())
            } else {
                format!("lines {}-{}", c.range.start(), c.range.end())
            };
            text.push_str(&format!(
                "\n{}. {} {lines} ({side}): {}",
                ix + 1,
                c.range.path(),
                c.text
            ));
        }
        state.comments.clear();
        // Inserted at the cursor, so file cards already in the draft stay.
        let gap = if self.composer.read(cx).value().trim().is_empty() {
            ""
        } else {
            "\n\n"
        };
        self.composer.update(cx, |s, cx| {
            s.focus(window, cx);
            s.insert(format!("{gap}{text}"), window, cx);
        });
        cx.notify();
    }

    pub(super) fn render_changes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        if self.side.repo.is_none() {
            return empty(
                "This folder is not a git repository, so there are no changes to review. Pick another folder from the title above.",
                cx,
            );
        }
        if self.side.changes.commit.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Commit message"));
            let sub = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.commit_now(false, window, cx);
                }
            });
            self.side.changes.commit = Some((input, sub));
        }
        let state = &self.side.changes;
        if !state.loaded {
            return loading();
        }
        let busy = state.busy;
        let scope = state.scope;

        // Branch strip.
        let b = &state.branch;
        let sync = match (b.ahead, b.behind) {
            (0, 0) => None,
            (a, 0) => Some(format!("↑{a}")),
            (0, d) => Some(format!("↓{d}")),
            (a, d) => Some(format!("↑{a} ↓{d}")),
        };
        let toolbar = h_flex()
            .px_2()
            .gap_1()
            .items_center()
            .child(
                Icon::new(ExtraIcon::GitBranch)
                    .xsmall()
                    .text_color(p.text_faint),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(p.text_muted)
                    .child(b.name.clone()),
            )
            .when_some(sync, |d, s| {
                d.child(div().text_xs().text_color(p.accent).child(s))
            })
            .child(div().flex_1())
            .child(
                Button::new("changes-pull")
                    .label("Pull")
                    .ghost()
                    .xsmall()
                    .loading(busy == Some("pull"))
                    .disabled(busy.is_some() || b.upstream.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.git_action("pull", cx, |repo| {
                            git::pull(repo).map(|()| Some("Pulled.".into()))
                        })
                    })),
            )
            .child(
                Button::new("changes-push")
                    .label("Push")
                    .icon(ExtraIcon::Upload)
                    .ghost()
                    .xsmall()
                    .loading(busy == Some("push"))
                    .disabled(busy.is_some())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.git_action("push", cx, |repo| {
                            git::push(repo).map(|()| Some("Pushed.".into()))
                        })
                    })),
            )
            .child(
                Button::new("changes-split")
                    .icon(ExtraIcon::GitCompare)
                    .ghost()
                    .xsmall()
                    .selected(state.split)
                    .tooltip("Side by side")
                    .accessibility_label("Side by side")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let state = &mut this.side.changes;
                        state.split = !state.split;
                        let mode = if state.split {
                            DiffMode::Split
                        } else {
                            DiffMode::Unified
                        };
                        if let Some(diff) = state.diff.clone() {
                            diff.update(cx, |d, cx| d.set_mode(mode, cx));
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("changes-wrap")
                    .icon(IconName::Replace)
                    .ghost()
                    .xsmall()
                    .selected(state.wrap)
                    .tooltip("Wrap long lines")
                    .accessibility_label("Wrap long lines")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.side.changes.wrap = !this.side.changes.wrap;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("changes-refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .xsmall()
                    .tooltip("Refresh")
                    .accessibility_label("Refresh changes")
                    .on_click(cx.listener(|this, _, _, cx| this.changes_refresh(cx))),
            );

        let mut col = v_flex().flex_1().min_h_0().child(toolbar);

        // The last turn's card.
        if let Some((files, add, del)) = state.turn_stats.filter(|s| s.0 > 0) {
            let undo_armed = state.confirm.as_deref() == Some("undo-turn");
            col = col.child(
                v_flex()
                    .mx_2()
                    .mt_1()
                    .p_2()
                    .gap_1p5()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(p.accent.opacity(0.4))
                    .bg(p.accent_wash)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .child(Icon::new(ExtraIcon::Sparkles).xsmall().text_color(p.accent))
                            .child(
                                div()
                                    .flex_1()
                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                    .child(format!(
                                        "Last turn changed {files} file{}",
                                        if files == 1 { "" } else { "s" }
                                    )),
                            )
                            .child(div().text_color(p.success).child(format!("+{add}")))
                            .child(div().text_color(p.danger).child(format!("−{del}"))),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("turn-review")
                                    .label(if scope == Scope::Turn {
                                        "Show all changes"
                                    } else {
                                        "Review"
                                    })
                                    .xsmall()
                                    .outline()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let next = if scope == Scope::Turn {
                                            Scope::Working
                                        } else {
                                            Scope::Turn
                                        };
                                        this.set_scope(next, cx)
                                    })),
                            )
                            .child(
                                Button::new("turn-keep")
                                    .label("Keep")
                                    .xsmall()
                                    .ghost()
                                    .tooltip("Accept these changes and dismiss the card")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let state = &mut this.side.changes;
                                        state.checkpoint = None;
                                        state.turn_stats = None;
                                        state.turn_patch.clear();
                                        if state.scope == Scope::Turn {
                                            this.set_scope(Scope::Working, cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("turn-undo")
                                    .label(if undo_armed {
                                        "Undo? Click again"
                                    } else {
                                        "Undo"
                                    })
                                    .icon(IconName::Undo2)
                                    .xsmall()
                                    .when(undo_armed, |b| b.danger())
                                    .when(!undo_armed, |b| b.ghost())
                                    .disabled(busy.is_some())
                                    .tooltip("Put every file this turn touched back as it was")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if !this.confirmed("undo-turn", cx) {
                                            return;
                                        }
                                        let Some(cp) = this.side.changes.checkpoint.clone() else {
                                            return;
                                        };
                                        this.side.changes.scope = Scope::Working;
                                        this.git_action("undo", cx, move |repo| {
                                            let n = git::undo_turn(repo, &cp)?;
                                            Ok(Some(format!(
                                                "Restored {n} file{}.",
                                                if n == 1 { "" } else { "s" }
                                            )))
                                        });
                                    })),
                            ),
                    ),
            );
        }

        if let Some(note) = state.note.clone() {
            col = col.child(
                div()
                    .px_3()
                    .pt_1()
                    .text_xs()
                    .text_color(p.success)
                    .child(note),
            );
        }
        if let Some(e) = state.error.clone() {
            col = col.child(error_line(e, cx));
        }

        // Commit box and file list (working tree only).
        if scope == Scope::Working {
            if let Some((input, _)) = state.commit.as_ref() {
                let staged = state.changes.iter().any(git::Change::staged);
                let has = !state.changes.is_empty();
                col = col.child(
                    v_flex()
                        .px_2()
                        .pt_2()
                        .gap_1()
                        .child(Input::new(input).small())
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("changes-commit")
                                        .label(if staged {
                                            "Commit staged"
                                        } else {
                                            "Commit all"
                                        })
                                        .icon(IconName::Check)
                                        .primary()
                                        .xsmall()
                                        .loading(busy == Some("commit"))
                                        .disabled(busy.is_some() || !has)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.commit_now(false, window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("changes-commit-push")
                                        .label("Commit & push")
                                        .outline()
                                        .xsmall()
                                        .disabled(busy.is_some() || !has)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.commit_now(true, window, cx)
                                        })),
                                ),
                        ),
                );
            }
            col = col.child(self.render_change_list(cx));
        }

        // The diff.
        let state = &self.side.changes;
        let diff_empty = state
            .diff
            .as_ref()
            .is_none_or(|d| d.read(cx).files().is_empty());
        let body: AnyElement = match state.diff.clone() {
            Some(_) if diff_empty => empty(
                match scope {
                    Scope::Working => "No uncommitted changes.",
                    Scope::Turn => "The last turn changed nothing.",
                },
                cx,
            ),
            Some(diff) => self.render_diff(diff, cx),
            None => loading(),
        };
        col = col.child(
            div()
                .flex_1()
                .min_h_0()
                .mt_2()
                .border_t_1()
                .border_color(p.border)
                .child(body),
        );

        // Comments waiting to be sent.
        let count = self.side.changes.comments.len();
        if count > 0 {
            col = col.child(
                h_flex()
                    .px_2()
                    .py_1p5()
                    .gap_1()
                    .items_center()
                    .border_t_1()
                    .border_color(p.border)
                    .child(
                        Icon::new(ExtraIcon::MessageSquarePlus)
                            .xsmall()
                            .text_color(p.accent),
                    )
                    .child(div().flex_1().text_xs().child(format!(
                        "{count} comment{}",
                        if count == 1 { "" } else { "s" }
                    )))
                    .child(
                        Button::new("comments-clear")
                            .label("Clear")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.side.changes.comments.clear();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("comments-send")
                            .label("Add to message")
                            .primary()
                            .xsmall()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.send_comments(window, cx)),
                            ),
                    ),
            );
        }
        col.into_any_element()
    }

    /// Staged and unstaged files, each with its actions.
    fn render_change_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let state = &self.side.changes;
        let staged: Vec<&git::Change> = state.changes.iter().filter(|c| c.staged()).collect();
        let unstaged: Vec<&git::Change> = state.changes.iter().filter(|c| c.unstaged()).collect();
        if staged.is_empty() && unstaged.is_empty() {
            return div().into_any_element();
        }
        let hidden = state.list_hidden;
        let busy = state.busy.is_some();
        let mut list = v_flex().px_1();
        let section = |label: String,
                       id: &'static str,
                       all: Vec<String>,
                       stage: bool,
                       cx: &mut Context<Self>| {
            h_flex()
                .px_2()
                .pt_2()
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(p.text_faint)
                .child(div().flex_1().child(label))
                .child(
                    Button::new(id)
                        .label(if stage { "Stage all" } else { "Unstage all" })
                        .ghost()
                        .xsmall()
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let paths = all.clone();
                            this.git_action("stage", cx, move |repo| {
                                if stage {
                                    git::stage(repo, &paths)?;
                                } else {
                                    git::unstage(repo, &paths)?;
                                }
                                Ok(None)
                            })
                        })),
                )
        };
        let row = |change: &git::Change, staged_row: bool, ix: usize, cx: &mut Context<Self>| {
            let letter = change.letter();
            let color = match letter {
                'M' => p.warning,
                'A' | 'U' => p.success,
                'D' => p.danger,
                _ => p.accent,
            };
            let (dir, name) = match change.path.rsplit_once('/') {
                Some((d, n)) => (Some(d.to_string()), n.to_string()),
                None => (None, change.path.clone()),
            };
            let path = change.path.clone();
            let jump = change.path.clone();
            let c = change.clone();
            let discard_key = format!("discard:{}", change.path);
            let armed = state.confirm.as_deref() == Some(discard_key.as_str());
            let id = if staged_row { "s" } else { "u" };
            kit::row(format!("change-{id}-{ix}"), false, cx)
                .min_h(px(24.))
                .gap_1p5()
                .text_xs()
                .child(
                    div()
                        .w(px(12.))
                        .flex_none()
                        .text_color(color)
                        .child(letter.to_string()),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .flex_shrink_0()
                        .max_w(px(180.))
                        .child(name),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(p.text_faint)
                        .child(dir.unwrap_or_default()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(diff) = this.side.changes.diff.clone() {
                        diff.update(cx, |d, cx| d.scroll_to_file(&jump, cx));
                    }
                }))
                .child(if staged_row {
                    Button::new(SharedString::from(format!("unstage-{ix}")))
                        .icon(IconName::Minus)
                        .ghost()
                        .xsmall()
                        .tooltip("Unstage")
                        .accessibility_label("Unstage")
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let paths = vec![path.clone()];
                            this.git_action("stage", cx, move |repo| {
                                git::unstage(repo, &paths).map(|()| None)
                            })
                        }))
                        .into_any_element()
                } else {
                    h_flex()
                        .child(
                            Button::new(SharedString::from(format!("discard-{ix}")))
                                .icon(if armed {
                                    IconName::TriangleAlert
                                } else {
                                    IconName::Undo2
                                })
                                .ghost()
                                .xsmall()
                                .when(armed, |b| b.danger())
                                .tooltip(if armed {
                                    "Click again to discard"
                                } else {
                                    "Discard changes"
                                })
                                .accessibility_label("Discard changes")
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.confirmed(&discard_key, cx) {
                                        return;
                                    }
                                    let change = c.clone();
                                    this.git_action("discard", cx, move |repo| {
                                        git::discard(repo, &change).map(|()| None)
                                    })
                                })),
                        )
                        .child({
                            let path = path.clone();
                            Button::new(SharedString::from(format!("stage-{ix}")))
                                .icon(IconName::Plus)
                                .ghost()
                                .xsmall()
                                .tooltip("Stage")
                                .accessibility_label("Stage")
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let paths = vec![path.clone()];
                                    this.git_action("stage", cx, move |repo| {
                                        git::stage(repo, &paths).map(|()| None)
                                    })
                                }))
                        })
                        .into_any_element()
                })
        };
        let header = h_flex()
            .id("changes-list-toggle")
            .px_2()
            .pt_2()
            .gap_1()
            .items_center()
            .cursor_pointer()
            .text_xs()
            .text_color(p.text_muted)
            .child(
                Icon::new(if hidden {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .xsmall(),
            )
            .child(format!(
                "{} changed file{}",
                state.changes.len(),
                if state.changes.len() == 1 { "" } else { "s" }
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.side.changes.list_hidden = !this.side.changes.list_hidden;
                cx.notify();
            }));
        if hidden {
            return header.into_any_element();
        }
        if !staged.is_empty() {
            let all = staged.iter().map(|c| c.path.clone()).collect();
            list = list.child(section(
                format!("Staged · {}", staged.len()),
                "unstage-all",
                all,
                false,
                cx,
            ));
            for (ix, c) in staged.iter().enumerate() {
                list = list.child(row(c, true, ix, cx));
            }
        }
        if !unstaged.is_empty() {
            let all = unstaged.iter().map(|c| c.path.clone()).collect();
            list = list.child(section(
                format!("Changes · {}", unstaged.len()),
                "stage-all",
                all,
                true,
                cx,
            ));
            for (ix, c) in unstaged.iter().enumerate() {
                list = list.child(row(c, false, ix, cx));
            }
        }
        v_flex()
            .child(header)
            .child(
                div()
                    .id("changes-list")
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    /// The viewer with comment annotations and a header button to open a file.
    fn render_diff(&self, diff: Entity<DiffState>, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let state = &self.side.changes;
        let weak = cx.weak_entity();
        let comments: Rc<Vec<(u64, String)>> = Rc::new(
            state
                .comments
                .iter()
                .map(|c| (c.id, c.text.clone()))
                .collect(),
        );
        let mut annotations: Vec<DiffAnnotation> = state
            .comments
            .iter()
            .map(|c| {
                DiffAnnotation::line(
                    ElementId::NamedInteger("comment".into(), c.id),
                    DiffLinePosition::new(
                        c.range.path().clone(),
                        c.range.end_side(),
                        c.range.end(),
                    ),
                )
            })
            .collect();
        let draft_input = state.draft.as_ref().map(|d| d.input.clone());
        if let Some(d) = state.draft.as_ref() {
            annotations.push(DiffAnnotation::line(
                ElementId::Name("comment-draft".into()),
                DiffLinePosition::new(d.range.path().clone(), d.range.end_side(), d.range.end()),
            ));
        }
        let repo = self.side.repo.clone();
        let add_weak = weak.clone();
        let note_weak = weak.clone();
        let open_weak = weak.clone();
        Diff::new(&diff)
            .size_full()
            .soft_wrap(state.wrap)
            .annotations(annotations)
            .on_add_annotation(move |range, window, cx| {
                let range = range.clone();
                let _ = add_weak.update(cx, |this, cx| this.start_draft(range, window, cx));
            })
            .render_annotation(move |annotation, _, _cx| {
                let base = div()
                    .mx_2()
                    .my_1()
                    .p_2()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(p.accent.opacity(0.4))
                    .bg(p.surface_raised)
                    .text_xs();
                if let ElementId::NamedInteger(_, id) = annotation.id() {
                    let id = *id;
                    let text = comments
                        .iter()
                        .find(|(cid, _)| *cid == id)
                        .map(|(_, t)| t.clone())
                        .unwrap_or_default();
                    let weak = note_weak.clone();
                    return base
                        .child(
                            h_flex()
                                .gap_2()
                                .items_start()
                                .child(
                                    Icon::new(ExtraIcon::MessageSquarePlus)
                                        .xsmall()
                                        .text_color(p.accent),
                                )
                                .child(div().flex_1().whitespace_normal().child(text))
                                .child(
                                    Button::new(SharedString::from(format!("comment-remove-{id}")))
                                        .icon(IconName::Close)
                                        .ghost()
                                        .xsmall()
                                        .accessibility_label("Remove comment")
                                        .on_click(move |_, _, cx| {
                                            let _ = weak.update(cx, |this, cx| {
                                                this.side.changes.comments.retain(|c| c.id != id);
                                                cx.notify();
                                            });
                                        }),
                                ),
                        )
                        .into_any_element();
                }
                let Some(input) = draft_input.clone() else {
                    return div().into_any_element();
                };
                let save = note_weak.clone();
                let cancel = note_weak.clone();
                base.child(
                    v_flex().gap_1().child(Input::new(&input).small()).child(
                        h_flex()
                            .gap_1()
                            .justify_end()
                            .child(
                                Button::new("comment-cancel")
                                    .label("Cancel")
                                    .ghost()
                                    .xsmall()
                                    .on_click(move |_, _, cx| {
                                        let _ = cancel.update(cx, |this, cx| {
                                            this.side.changes.draft = None;
                                            cx.notify();
                                        });
                                    }),
                            )
                            .child(
                                Button::new("comment-save")
                                    .label("Comment")
                                    .primary()
                                    .xsmall()
                                    .on_click(move |_, _, cx| {
                                        let _ = save.update(cx, |this, cx| this.save_draft(cx));
                                    }),
                            ),
                    ),
                )
                .into_any_element()
            })
            .render_header_suffix(move |file, _, _| {
                let path = repo.as_ref().map(|r| r.join(file.path().as_ref()));
                let weak = open_weak.clone();
                let id = SharedString::from(format!("diff-open-{}", file.path()));
                Button::new(id)
                    .icon(IconName::File)
                    .ghost()
                    .xsmall()
                    .tooltip("Show the whole file")
                    .accessibility_label("Show the whole file")
                    .on_click(move |_, _, cx| {
                        let Some(path) = path.clone() else { return };
                        let _ = weak.update(cx, |this, cx| {
                            this.show_side(Tab::Files, cx);
                            this.project_open(path, cx);
                        });
                    })
            })
            .into_any_element()
    }
}
