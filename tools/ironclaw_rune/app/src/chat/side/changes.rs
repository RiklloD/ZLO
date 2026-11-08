//! Changes: MonoCode Native's `changes.rs` + `diff_view.rs` + session review
//! card (MIT) (GPUI `chat/side/changes.rs`), on rune-kit's `DiffView`:
//!
//! - the branch with Pull / Push, a commit box, staged and unstaged files
//!   with Stage / Unstage / Discard;
//! - every change in one virtualized, syntax-coloured diff, unified or split;
//! - comments on a line or a selected range (Zeron's review comments), sent
//!   to the composer as one message for the agent;
//! - the last turn's card: what the agent changed since its turn began (a
//!   `git stash create` checkpoint), with Review, Keep and Undo;
//! - on Codex threads, Codex's own diff, review and revert (`codex.rs`).

use rune::{El, TextEdit, Tx, col, el, icon, row};
use rune_kit::diff::{Annotation, DiffMode, DiffState, DiffView, LineRange, LineRef, Side};
use rune_kit::{ButtonVariant, Kit, TextInput};

use super::ui::{self, xs, xs_med};
use super::{
    SideState, Tab, bridge, git, short_path, side, side_mut, side_of, side_total, thread_id,
};
use crate::icons;
use crate::jobs;
use crate::state::State;

mod codex;
mod list;
mod notes;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) enum Scope {
    #[default]
    Working,
    Turn,
    /// The diff Codex reported for its turn.
    Codex,
}

pub(super) struct Comment {
    id: u64,
    range: LineRange,
    /// The file's path in the diff (repository-relative).
    path: String,
    text: String,
}

pub(super) struct Draft {
    range: LineRange,
    path: String,
    input: TextEdit,
}

#[derive(Default)]
pub(super) struct ChangesState {
    loaded: bool,
    loading: bool,
    stale: bool,
    error: Option<String>,
    branch: git::Branch,
    changes: Vec<git::Change>,
    scope: Scope,
    working_patch: String,
    diff: Option<DiffState>,
    /// The patch the viewer holds, so an unchanged refresh keeps its scroll.
    shown: String,
    split: bool,
    wrap: bool,
    busy: Option<&'static str>,
    note: Option<String>,
    /// The destructive button waiting for its second click.
    confirm: Option<String>,
    commit: TextEdit,
    comments: Vec<Comment>,
    next_comment: u64,
    draft: Option<Draft>,
    checkpoint: Option<git::Checkpoint>,
    turn_patch: String,
    /// Files, additions and deletions of the last turn.
    turn_stats: Option<(usize, usize, usize)>,
    list_hidden: bool,
    codex: codex::CodexState,
}

impl ChangesState {
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

fn stats(files: &[rune_kit::diff::FileDiff]) -> (usize, usize, usize) {
    files.iter().fold((0, 0, 0), |(n, a, d), f| {
        (n + 1, a + f.additions(), d + f.deletions())
    })
}

/// Re-read status, branch and both diffs. Coalesces like the orchestration poll.
pub(super) fn refresh(s: &mut State) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    let Some(repo) = x.repo.clone() else { return };
    let state = &mut x.changes;
    if state.loading {
        state.stale = true;
        return;
    }
    state.loading = true;
    let checkpoint = state.checkpoint.clone();
    let generation = x.folder_gen;
    jobs::blocking(
        s,
        move || {
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
        },
        move |s, loaded, _| {
            let Some(x) = side_of(s, &thread).filter(|x| x.folder_gen == generation) else {
                return;
            };
            let state = &mut x.changes;
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
                    state.turn_stats = Some(stats(&rune_kit::diff::parse(&patch)));
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
            apply(s);
            if stale {
                refresh(s);
            }
        },
    );
}

/// Feed the viewer the patch of the current scope.
fn apply(s: &mut State) {
    let codex = side(s)
        .is_some_and(|x| x.changes.scope == Scope::Codex)
        .then(|| codex::patch(s));
    let Some(x) = side_mut(s) else { return };
    let state = &mut x.changes;
    let patch = match state.scope {
        Scope::Working => state.working_patch.clone(),
        Scope::Turn => state.turn_patch.clone(),
        Scope::Codex => codex.unwrap_or_default(),
    };
    if state.diff.is_some() && patch == state.shown {
        return;
    }
    let files = rune_kit::diff::parse(&patch);
    state.shown = patch;
    let mode = if state.split {
        DiffMode::Split
    } else {
        DiffMode::Unified
    };
    match state.diff.as_mut() {
        Some(diff) => diff.set_files(files),
        None => {
            let mut diff = DiffState::new(files);
            diff.set_mode(mode);
            diff.set_wrap(state.wrap);
            state.diff = Some(diff);
        }
    }
    sync_annotations(s);
}

/// The diff's annotations: comments, the draft, and the latest Codex
/// review's findings on the lines they name.
fn sync_annotations(s: &mut State) {
    let findings = codex::findings(s);
    let Some(x) = side_mut(s) else { return };
    let state = &mut x.changes;
    let Some(diff) = state.diff.as_mut() else {
        return;
    };
    let mut notes: Vec<Annotation> = state
        .comments
        .iter()
        .map(|c| Annotation::new(c.range.file, c.range.end, format!("comment-{}", c.id)))
        .collect();
    for (ix, (path, f)) in findings.iter().enumerate() {
        if let Some(file) = diff.file_index(path) {
            notes.push(Annotation::new(
                file,
                LineRef::new(Side::New, f.end),
                format!("finding-{ix}"),
            ));
        }
    }
    if let Some(d) = &state.draft {
        notes.push(Annotation::new(d.range.file, d.range.end, "draft"));
    }
    diff.set_annotations(notes);
}

/// Record the working tree before a turn, for its card.
pub(super) fn checkpoint(s: &mut State) {
    let Some(thread) = thread_id(s) else { return };
    let Some(repo) = side(s).and_then(|x| x.repo.clone()) else {
        return;
    };
    jobs::blocking(
        s,
        move || git::checkpoint(&repo),
        move |s, result, _| {
            let Some(x) = side_of(s, &thread) else { return };
            match result {
                Ok(cp) => {
                    x.changes.checkpoint = Some(cp);
                    x.changes.turn_stats = None;
                    x.changes.turn_patch.clear();
                }
                Err(e) => tracing::debug!(error = %e, "no turn checkpoint"),
            }
        },
    );
}

/// Another folder: a Codex workspace refusal was about the old one.
pub(super) fn folder_changed(x: &mut SideState) {
    x.changes.codex.mismatch = None;
}

/// Codex reported a new turn diff or review: the Codex scope and the
/// findings follow it. Area A calls this when the transcript's CLI diff
/// or review changes (GPUI did it while drawing).
pub fn on_cli_report(s: &mut State) {
    let follow = side(s)
        .is_some_and(|x| x.changes.scope == Scope::Codex && x.changes.shown != codex::patch(s));
    if follow {
        apply(s);
    } else {
        sync_annotations(s);
    }
}

/// Run a git mutation off the UI thread, then refresh.
fn git_action(
    s: &mut State,
    label: &'static str,
    f: impl FnOnce(&std::path::Path) -> Result<Option<String>, String> + Send + 'static,
) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    let Some(repo) = x.repo.clone() else { return };
    if x.changes.busy.is_some() {
        return;
    }
    x.changes.busy = Some(label);
    x.changes.confirm = None;
    x.changes.note = None;
    jobs::blocking(
        s,
        move || f(&repo),
        move |s, result, _| {
            let Some(x) = side_of(s, &thread) else { return };
            x.changes.busy = None;
            match result {
                Ok(note) => x.changes.note = note,
                Err(e) => x.changes.error = Some(e),
            }
            refresh(s);
            super::history::stale(s);
        },
    );
}

/// The first click arms a destructive action, the second runs it.
fn confirmed(s: &mut State, key: &str) -> bool {
    let Some(x) = side_mut(s) else { return false };
    if x.changes.confirm.as_deref() == Some(key) {
        x.changes.confirm = None;
        return true;
    }
    x.changes.confirm = Some(key.to_string());
    false
}

fn commit_now(s: &mut State, push: bool) {
    let Some(x) = side_mut(s) else { return };
    let message = x.changes.commit.text.trim().to_string();
    if message.is_empty() {
        x.changes.error = Some("Write a commit message first.".into());
        return;
    }
    x.changes.commit.clear();
    git_action(s, "commit", move |repo| {
        let sha = git::commit(repo, &message)?;
        if push {
            git::push(repo)?;
            return Ok(Some(format!("Committed {sha} and pushed.")));
        }
        Ok(Some(format!("Committed {sha}.")))
    });
}

fn set_scope(s: &mut State, scope: Scope) {
    if let Some(x) = side_mut(s) {
        x.changes.scope = scope;
        x.changes.comments.clear();
        x.changes.draft = None;
    }
    apply(s);
    sync_annotations(s);
}

/// The lines picked for a comment (or the latest comment's), new side only,
/// for the composer's `@` selection mention.
pub(super) fn selection(
    x: &SideState,
) -> Option<(
    std::path::PathBuf,
    zlo_client::mentions::LineRange,
)> {
    let state = &x.changes;
    let (range, path) = state
        .draft
        .as_ref()
        .map(|d| (&d.range, &d.path))
        .or_else(|| state.comments.last().map(|c| (&c.range, &c.path)))?;
    if range.start.side != Side::New || range.end.side != Side::New {
        return None;
    }
    let (a, b) = (range.start.line, range.end.line);
    let lines = zlo_client::mentions::LineRange::new(a.min(b), a.max(b))?;
    let repo = x.repo.as_ref()?;
    Some((repo.join(path), lines))
}

fn start_draft(s: &mut State, range: LineRange) {
    let Some(x) = side_mut(s) else { return };
    let path = x
        .changes
        .diff
        .as_ref()
        .and_then(|d| d.files().get(range.file))
        .map(|f| f.path().to_string())
        .unwrap_or_default();
    let mut input = TextEdit::new("");
    input.focused = true;
    x.changes.draft = Some(Draft { range, path, input });
    sync_annotations(s);
}

fn save_draft(s: &mut State) {
    let Some(x) = side_mut(s) else { return };
    let state = &mut x.changes;
    let Some(draft) = state.draft.take() else {
        return;
    };
    let text = draft.input.text.trim().to_string();
    if !text.is_empty() {
        state.next_comment += 1;
        state.comments.push(Comment {
            id: state.next_comment,
            range: draft.range,
            path: draft.path,
            text,
        });
    }
    sync_annotations(s);
}

/// All comments as one message in the composer, ready to send.
fn send_comments(s: &mut State) {
    let Some(x) = side_mut(s) else { return };
    if x.changes.comments.is_empty() {
        return;
    }
    let what = match x.changes.scope {
        Scope::Working => "uncommitted changes",
        Scope::Turn => "changes from your last turn",
        Scope::Codex => "changes Codex made in its last turn",
    };
    let folder = x.repo.as_deref().map(short_path).unwrap_or_default();
    let mut text = format!("Review comments on the {what} in {folder}:\n");
    for (ix, c) in x.changes.comments.iter().enumerate() {
        let side = match c.range.end.side {
            Side::Old => "old",
            Side::New => "new",
        };
        let lines = if c.range.start.line == c.range.end.line {
            format!("line {}", c.range.end.line)
        } else {
            format!("lines {}-{}", c.range.start.line, c.range.end.line)
        };
        text.push_str(&format!(
            "\n{}. {} {lines} ({side}): {}",
            ix + 1,
            c.path,
            c.text
        ));
    }
    x.changes.comments.clear();
    sync_annotations(s);
    // Inserted at the cursor, so file cards already in the draft stay.
    let gap = if bridge::composer_empty(s) {
        ""
    } else {
        "\n\n"
    };
    bridge::composer_insert(s, &format!("{gap}{text}"));
}

fn diff_of(s: &State) -> &DiffState {
    side(s)
        .and_then(|x| x.changes.diff.as_ref())
        .unwrap_or_else(|| ui::empty_diff())
}

fn diff_of_mut(s: &mut State) -> &mut DiffState {
    match side_mut(s).and_then(|x| x.changes.diff.as_mut()) {
        Some(d) => d,
        None => ui::spare_diff(),
    }
}

/// Scroll the diff to `path`'s file.
fn jump(s: &mut State, path: &str, tx: &mut Tx) {
    if let Some(diff) = side_mut(s).and_then(|x| x.changes.diff.as_mut())
        && let Some(file) = diff.file_index(path)
    {
        diff.scroll_to_file(file, tx);
    }
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let Some(x) = side(s) else {
        return ui::loading(k);
    };
    if x.repo.is_none() {
        return ui::empty(
            s,
            "This folder is not a git repository, so there are no changes to review. Pick another folder from the title above.",
        );
    }
    let state = &x.changes;
    if !state.loaded {
        return ui::loading(k);
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
    let mut toolbar = row()
        .w_full()
        .px(8.)
        .gap(4.)
        .items_center()
        .child(icon(&icons::GIT_BRANCH, 12.).color(p.text_faint))
        .child(
            ui::line(b.name.clone(), xs(), p.text_muted)
                .shrink()
                .min_w(0.),
        );
    if let Some(sync) = sync {
        toolbar = toolbar.child(ui::line(sync, xs(), p.accent));
    }
    toolbar = toolbar
        .child(ui::grow())
        .child(
            ui::busy(ui::btn("Pull"), busy == Some("pull"))
                .disabled(busy.is_some() || b.upstream.is_none())
                .on_press(|s: &mut State| {
                    git_action(s, "pull", |repo| {
                        git::pull(repo).map(|()| Some("Pulled.".into()))
                    })
                })
                .build(k)
                .key("changes-pull"),
        )
        .child(
            ui::busy(ui::btn("Push").icon(&icons::UPLOAD), busy == Some("push"))
                .disabled(busy.is_some())
                .on_press(|s: &mut State| {
                    git_action(s, "push", |repo| {
                        git::push(repo).map(|()| Some("Pushed.".into()))
                    })
                })
                .build(k)
                .key("changes-push"),
        )
        .child(ui::selected(
            s,
            ui::ibtn(&icons::GIT_COMPARE, "Side by side")
                .on_press(|s: &mut State| {
                    if let Some(x) = side_mut(s) {
                        x.changes.split = !x.changes.split;
                        let mode = if x.changes.split {
                            DiffMode::Split
                        } else {
                            DiffMode::Unified
                        };
                        if let Some(diff) = x.changes.diff.as_mut() {
                            diff.set_mode(mode);
                        }
                    }
                })
                .build(k)
                .key("changes-split"),
            state.split,
        ))
        .child(ui::selected(
            s,
            ui::ibtn(&icons::REPLACE, "Wrap long lines")
                .on_press(|s: &mut State| {
                    if let Some(x) = side_mut(s) {
                        x.changes.wrap = !x.changes.wrap;
                        let wrap = x.changes.wrap;
                        if let Some(diff) = x.changes.diff.as_mut() {
                            diff.set_wrap(wrap);
                        }
                    }
                })
                .build(k)
                .key("changes-wrap"),
            state.wrap,
        ))
        .child(
            ui::ibtn(&icons::REFRESH_CW, "Refresh")
                .label("Refresh changes")
                .on_press(refresh)
                .build(k)
                .key("changes-refresh"),
        );

    let mut col = ui::fill_col().child(toolbar);

    // The last turn's card.
    if let Some((files, add, del)) = state.turn_stats.filter(|s| s.0 > 0) {
        col = col.child(turn_card(s, k, state, files, add, del));
    }
    if bridge::thread_runtime(s) == Some("codex") {
        col = col.child(codex::card(s, k));
    }
    if let Some(note) = state.note.clone() {
        col = col.child(ui::line(note, xs(), p.success).px(12.).pt(4.));
    }
    if let Some(e) = state.error.clone() {
        col = col.child(ui::error_line(s, e));
    }

    // Commit box and file list (working tree only).
    if scope == Scope::Working {
        let staged = state.changes.iter().any(git::Change::staged);
        let has = !state.changes.is_empty();
        col = col.child(
            col_box()
                .child(
                    TextInput::new(&state.commit)
                        .width(None)
                        .placeholder("Commit message")
                        .label("Commit message")
                        .on_edit(|s: &mut State| &mut side_total(s).changes.commit)
                        .on_submit(|s: &mut State, _| commit_now(s, false))
                        .build(k)
                        .key("changes-commit-message"),
                )
                .child(
                    row()
                        .gap(4.)
                        .child(
                            ui::busy(
                                ui::primary(if staged {
                                    "Commit staged"
                                } else {
                                    "Commit all"
                                })
                                .icon(&icons::CHECK),
                                busy == Some("commit"),
                            )
                            .disabled(busy.is_some() || !has)
                            .on_press(|s: &mut State| commit_now(s, false))
                            .build(k)
                            .key("changes-commit"),
                        )
                        .child(
                            ui::outline("Commit & push")
                                .disabled(busy.is_some() || !has)
                                .on_press(|s: &mut State| commit_now(s, true))
                                .build(k)
                                .key("changes-commit-push"),
                        ),
                ),
        );
        col = col.child(list::view(s, k, state));
    }

    // The diff.
    let diff_empty = state.diff.as_ref().is_none_or(|d| d.files().is_empty());
    let body = match &state.diff {
        Some(_) if diff_empty => ui::empty(
            s,
            match scope {
                Scope::Working => "No uncommitted changes.",
                Scope::Turn => "The last turn changed nothing.",
                Scope::Codex => "Codex has not reported a diff for this turn.",
            },
        ),
        Some(_) => diff_view(s, k),
        None => ui::loading(k),
    };
    col = col
        .child(el().w_full().h(1.).mt(8.).fill(p.border))
        .child(ui::fill_col().child(body));

    // Comments waiting to be sent.
    let count = state.comments.len();
    if count > 0 {
        col = col.child(el().w_full().h(1.).fill(p.border)).child(
            row()
                .w_full()
                .px(8.)
                .py(6.)
                .gap(4.)
                .items_center()
                .child(icon(&icons::MESSAGE_SQUARE_PLUS, 12.).color(p.accent))
                .child(
                    ui::line(
                        format!("{count} comment{}", if count == 1 { "" } else { "s" }),
                        xs(),
                        p.text,
                    )
                    .grow(1.),
                )
                .child(
                    ui::btn("Clear")
                        .on_press(|s: &mut State| {
                            if let Some(x) = side_mut(s) {
                                x.changes.comments.clear();
                            }
                            sync_annotations(s);
                        })
                        .build(k)
                        .key("comments-clear"),
                )
                .child(
                    ui::primary("Add to message")
                        .on_press(send_comments)
                        .build(k)
                        .key("comments-send"),
                ),
        );
    }
    col
}

fn col_box() -> El {
    col().w_full().px(8.).pt(8.).gap(4.)
}

fn turn_card(s: &State, k: &Kit, state: &ChangesState, files: usize, add: usize, del: usize) -> El {
    let p = &s.palette;
    let scope = state.scope;
    let undo_armed = state.confirm.as_deref() == Some("undo-turn");
    col()
        .key("changes-turn-card")
        .mx(8.)
        .mt(4.)
        .padding(8.)
        .gap(6.)
        .radius(8.)
        .fill(p.accent_wash)
        .border(1., p.accent.alpha(0.4))
        .child(
            row()
                .gap(8.)
                .items_center()
                .child(icon(&icons::SPARKLES, 12.).color(p.accent))
                .child(
                    ui::line(
                        format!(
                            "Last turn changed {files} file{}",
                            if files == 1 { "" } else { "s" }
                        ),
                        xs_med(),
                        p.text,
                    )
                    .grow(1.),
                )
                .child(ui::line(format!("+{add}"), xs(), p.success))
                .child(ui::line(format!("−{del}"), xs(), p.danger)),
        )
        .child(
            row()
                .gap(4.)
                .child(
                    ui::outline(if scope == Scope::Turn {
                        "Show all changes"
                    } else {
                        "Review"
                    })
                    .on_press(move |s: &mut State| {
                        set_scope(
                            s,
                            if scope == Scope::Turn {
                                Scope::Working
                            } else {
                                Scope::Turn
                            },
                        )
                    })
                    .build(k)
                    .key("turn-review"),
                )
                .child(
                    ui::btn("Keep")
                        .help("Accept these changes and dismiss the card")
                        .on_press(|s: &mut State| {
                            let Some(x) = side_mut(s) else { return };
                            x.changes.checkpoint = None;
                            x.changes.turn_stats = None;
                            x.changes.turn_patch.clear();
                            if x.changes.scope == Scope::Turn {
                                set_scope(s, Scope::Working);
                            }
                        })
                        .build(k)
                        .key("turn-keep"),
                )
                .child(
                    ui::btn(if undo_armed {
                        "Undo? Click again"
                    } else {
                        "Undo"
                    })
                    .icon(&icons::UNDO_2)
                    .variant(if undo_armed {
                        ButtonVariant::Danger
                    } else {
                        ButtonVariant::Ghost
                    })
                    .disabled(state.busy.is_some())
                    .help("Put every file this turn touched back as it was")
                    .on_press(|s: &mut State| {
                        if !confirmed(s, "undo-turn") {
                            return;
                        }
                        let Some(x) = side_mut(s) else { return };
                        let Some(cp) = x.changes.checkpoint.clone() else {
                            return;
                        };
                        x.changes.scope = Scope::Working;
                        git_action(s, "undo", move |repo| {
                            let n = git::undo_turn(repo, &cp)?;
                            Ok(Some(format!(
                                "Restored {n} file{}.",
                                if n == 1 { "" } else { "s" }
                            )))
                        });
                    })
                    .build(k)
                    .key("turn-undo"),
                ),
        )
}

/// The viewer with comment annotations and a header button to open a file.
fn diff_view(s: &State, k: &Kit) -> El {
    let k2 = k.clone();
    let k3 = k.clone();
    DiffView::new(diff_of, diff_of_mut)
        .key("changes-diff")
        .on_comment(|s: &mut State, range, _| start_draft(s, range))
        .annotation(move |s: &State, a: &Annotation| notes::annotation(s, &k2, a))
        .header_actions(move |s: &State, file| {
            let path = side(s)
                .and_then(|x| {
                    Some((
                        x.repo.clone()?,
                        x.changes
                            .diff
                            .as_ref()?
                            .files()
                            .get(file)?
                            .path()
                            .to_string(),
                    ))
                })
                .map(|(repo, rel)| repo.join(rel));
            ui::ibtn(&icons::FILE, "Show the whole file")
                .on_press(move |s: &mut State| {
                    let Some(path) = path.clone() else { return };
                    super::show(s, Tab::Files);
                    super::project::open(s, path);
                })
                .build(&k3)
                .key(format!("diff-open-{file}"))
        })
        .build(s, k)
}

pub(super) use notes::{fixture, fixture_comment};
