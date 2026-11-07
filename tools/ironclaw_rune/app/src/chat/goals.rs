//! The goal strip above the composer (t3code's goal callout): a target,
//! "Pursuing goal" and the objective on one muted line, with edit, done and
//! clear on hover. `/goal <objective>` sets it, `/goal clear` clears it and
//! a bare `/goal` opens the editor. Port of GPUI `chat/goals.rs`.
//!
//! A goal may carry a check: `/goal <objective> --check "<cmd>"` (optionally
//! `--attempts N`, `--timeout SECS`; `/goal --check "<cmd>"` changes only the
//! check, `/goal --check off` removes it). The backend runs it after every
//! completed turn; the strip shows where it stands (not run, running,
//! passed, failed n/N, needs you) with "Run check now", "Stop retrying" and
//! an editor for the command. The goal is read when the thread opens, after
//! every change and whenever the orchestration state refreshes while its
//! check can still move; a queued or running check keeps the thread polling.
//!
//! # Entry points
//!
//! Every chat has its own goal, the open one and each side chat (GPUI: each
//! `ChatView` read its own); functions take the chat's thread id and find
//! it with `chat::chat_for`, so a reply for a closed chat goes nowhere.
//!
//! - `goal_command(text) -> Option<GoalCommand>`: area A's composer asks this
//!   on send; `Some` means the line is a `/goal` command, then it calls
//!   `run_command(s, id, command)` instead of sending.
//! - `load(s, id)`: read the goal (orchestration's refresh calls it).
//! - `view(s, chat, k) -> Option<El>`: the strip (or the editor);
//!   orchestration places it (`orchestration::composer` while editing,
//!   `orchestration::tail` otherwise).

use ironclaw_gpui_client::ClientError;
use ironclaw_gpui_client::inbox::{
    GoalCheckAction, GoalCheckRecord, GoalCheckSpec, GoalCheckStatus, GoalRecord, GoalStatus, ThreadGoalResponse,
};
use rune::{El, Icon, TextEdit, col, el, row};
use rune_kit::{Kit, TextInput};

use crate::chat::ChatState;
use crate::chat::extras_ui::{ICON_XS, XS, ghost_button, ghost_icon, ico, spinner, txt, txt_medium};
use crate::icons;
use crate::jobs;
use crate::state::State;

#[derive(Default)]
pub struct GoalsState {
    pub(crate) goal: Option<GoalRecord>,
    loaded: bool,
    /// A read is in flight.
    reading: bool,
    /// The editor is open (on the objective, or with `editing_check` on the
    /// check command).
    pub(crate) editor: Option<TextEdit>,
    editing_check: bool,
}

impl GoalsState {
    /// The check can still change on its own (a turn ends, a run finishes).
    fn check_can_move(&self) -> bool {
        self.goal.as_ref().is_some_and(|g| {
            g.status == GoalStatus::Active
                && g.check.as_ref().is_some_and(|c| {
                    matches!(
                        c.status,
                        GoalCheckStatus::NotRun | GoalCheckStatus::Pending | GoalCheckStatus::Running | GoalCheckStatus::Failed
                    )
                })
        })
    }

    /// A check is queued or running: worth polling for.
    pub fn check_in_flight(&self) -> bool {
        self.goal
            .as_ref()
            .and_then(|g| g.check.as_ref())
            .is_some_and(|c| matches!(c.status, GoalCheckStatus::Pending | GoalCheckStatus::Running))
    }

    pub fn editing(&self) -> bool {
        self.editor.is_some()
    }
}

/// What a `/goal` line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum GoalCommand {
    Edit,
    Clear,
    Set(String),
    /// Set the check, and the objective when one is given.
    SetChecked { objective: Option<String>, check: GoalCheckSpec },
    ClearCheck,
    /// A malformed `--check`; the message says why.
    Invalid(String),
}

/// `Some` when `text` is a `/goal` command.
pub fn goal_command(text: &str) -> Option<GoalCommand> {
    let rest = text.trim().strip_prefix("/goal")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let rest = rest.trim();
    if let Some((objective, args)) = split_flag(rest, "--check") {
        let objective = objective.strip_prefix("set ").unwrap_or(objective).trim();
        let objective = (!objective.is_empty()).then(|| objective.to_string());
        if args.trim() == "off" && objective.is_none() {
        return Some(GoalCommand::ClearCheck);
        }
        return Some(match parse_check(args) {
            Ok(check) => GoalCommand::SetChecked { objective, check },
            Err(message) => GoalCommand::Invalid(message),
        });
    }
    Some(match rest {
        "" => GoalCommand::Edit,
        "clear" | "off" => GoalCommand::Clear,
        objective => GoalCommand::Set(objective.strip_prefix("set ").unwrap_or(objective).into()),
    })
}

/// Split `text` at a whitespace-delimited `flag`: (before, after).
fn split_flag<'a>(text: &'a str, flag: &str) -> Option<(&'a str, &'a str)> {
    let mut from = 0;
    while let Some(at) = text[from..].find(flag).map(|i| i + from) {
        let before_ok = at == 0 || text[..at].ends_with(char::is_whitespace);
        let end = at + flag.len();
        let after_ok = end == text.len() || text[end..].starts_with(char::is_whitespace);
        if before_ok && after_ok {
        return Some((&text[..at], &text[end..]));
        }
        from = end;
    }
    None
}

/// `"<cmd>" [--attempts N] [--timeout SECS]`; an unquoted command runs to
/// the first flag.
fn parse_check(args: &str) -> Result<GoalCheckSpec, String> {
    let args = args.trim_start();
    let (command, mut rest) = match args.chars().next() {
        Some(quote @ ('"' | '\'')) => {
            let body = &args[1..];
            let close = body.find(quote).ok_or_else(|| format!("--check: missing closing {quote}"))?;
            (body[..close].to_string(), &body[close + 1..])
        }
        _ => {
            let end = [" --attempts", " --timeout"]
                .iter()
                .filter_map(|f| args.find(f))
                .min()
                .unwrap_or(args.len());
            (args[..end].trim().to_string(), &args[end..])
        }
    };
    if command.trim().is_empty() {
        return Err("--check needs a command, e.g. /goal tests pass --check \"cargo test\"".into());
    }
    let mut spec = GoalCheckSpec { command: command.trim().to_string(), max_attempts: None, timeout_secs: None };
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Ok(spec);
        }
        let mut words = rest.splitn(3, char::is_whitespace);
        let (Some(flag), Some(value)) = (words.next(), words.next()) else {
            return Err(format!("{rest}: expected a value"));
        };
        let number = value
            .trim_end_matches('s')
            .parse::<u64>()
            .map_err(|_| format!("{flag}: {value} is not a number"))?;
        match flag {
            "--attempts" => spec.max_attempts = Some(u32::try_from(number).map_err(|e| e.to_string())?),
            "--timeout" => spec.timeout_secs = Some(number),
            other => return Err(format!("unknown option {other}")),
        }
        rest = words.next().unwrap_or("");
    }
}

/// The chip after the objective: label, icon and how loud it is.
enum Tone {
    Muted,
    Good,
    Warn,
}

fn check_chip(check: &GoalCheckRecord) -> (String, Option<&'static Icon>, Tone) {
    let n = check.attempts;
    let max = check.max_attempts;
    match check.status {
        GoalCheckStatus::NotRun => ("Check not run".into(), None, Tone::Muted),
        GoalCheckStatus::Pending => ("Check queued".into(), None, Tone::Muted),
        GoalCheckStatus::AwaitingApproval => ("Check needs approval".into(), Some(&icons::CIRCLE_ALERT), Tone::Warn),
        GoalCheckStatus::Running => ("Checking…".into(), None, Tone::Muted),
        GoalCheckStatus::Passed => ("Check passed".into(), Some(&icons::CIRCLE_CHECK), Tone::Good),
        GoalCheckStatus::Failed => (format!("Check failed {n}/{max}"), Some(&icons::CIRCLE_X), Tone::Warn),
        GoalCheckStatus::NeedsYou => (
            if n >= max { format!("Needs you · failed {n}/{max}") } else { "Needs you".into() },
            Some(&icons::CIRCLE_ALERT),
            Tone::Warn,
        ),
        GoalCheckStatus::Stopped => ("Retries stopped".into(), None, Tone::Muted),
    }
}

/// The goal of the chat showing `thread_id` (main or side), only while that
/// chat is still open.
fn slot<'a>(s: &'a mut State, thread_id: &str) -> Option<&'a mut GoalsState> {
    crate::chat::chat_for(s, thread_id).map(|c| &mut c.goals)
}

/// [`slot`], only while the chat extras `instance` that asked still shows
/// it: a chat closed and reopened on the same thread is a new instance.
fn slot_of<'a>(s: &'a mut State, thread_id: &str, instance: jobs::Instance) -> Option<&'a mut GoalsState> {
    if !crate::chat::orchestration::is_current(s, thread_id, instance) {
        return None;
    }
    slot(s, thread_id)
}

/// Read the goal once per thread open, and again while its check can still
/// move (each orchestration refresh asks).
pub fn load(s: &mut State, id: &str) {
    let Some(instance) = crate::chat::orchestration::instance_of(s, id) else { return };
    let Some(g) = slot(s, id) else { return };
    let thread_id = id.to_string();
    if g.reading || (g.loaded && !g.check_can_move()) {
        return;
    }
    g.loaded = true;
    g.reading = true;
    let asked = thread_id.clone();
    jobs::run(
        s,
        "goal",
        move |c| async move { c.thread_goal(&thread_id).await },
        move |s, result, _| {
            let Some(g) = slot_of(s, &asked, instance) else { return };
            g.reading = false;
            match result {
                Ok(doc) => g.goal = doc.goal,
                // silent-ok: no goal route (older gateway) or a thread the
                // orchestrator has not seen yet both mean "no goal".
                Err(e) => tracing::debug!(error = %e, "thread goal read failed"),
            }
            crate::chat::orchestration::ensure_poll(s, &asked);
        },
    );
}

fn apply_goal(s: &mut State, asked: &str, instance: jobs::Instance, result: Result<ThreadGoalResponse, ClientError>) {
    let Some(g) = slot_of(s, asked, instance) else { return };
    match result {
        Ok(doc) => {
            g.goal = doc.goal;
            g.editor = None;
            g.editing_check = false;
        }
        Err(e) => crate::chat::report_error(s, asked, e.to_string()),
    }
    crate::chat::orchestration::ensure_poll(s, asked);
}

fn write_goal(s: &mut State, id: &str, objective: Option<String>, status: Option<GoalStatus>) {
    let Some(instance) = crate::chat::orchestration::instance_of(s, id) else { return };
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    jobs::run(
        s,
        "goal",
        move |c| async move { c.set_thread_goal(&thread_id, objective.as_deref(), status).await },
        move |s, result, _| apply_goal(s, &asked, instance, result),
    );
}

/// Set (or with `None` remove) the check, and the objective if given.
fn write_goal_check(s: &mut State, id: &str, objective: Option<String>, check: Option<GoalCheckSpec>) {
    let Some(instance) = crate::chat::orchestration::instance_of(s, id) else { return };
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    jobs::run(
        s,
        "goal",
        move |c| async move { c.set_thread_goal_check(&thread_id, objective.as_deref(), check.as_ref()).await },
        move |s, result, _| apply_goal(s, &asked, instance, result),
    );
}

/// `revision` is the check the strip showed when it was clicked.
fn goal_check_action(s: &mut State, id: &str, action: GoalCheckAction, revision: u64) {
    let Some(instance) = crate::chat::orchestration::instance_of(s, id) else { return };
    let thread_id = id.to_string();
    let asked = thread_id.clone();
    jobs::run(
        s,
        "goal",
        move |c| async move { c.goal_check_action(&thread_id, action, revision).await },
        move |s, result, _| apply_goal(s, &asked, instance, result),
    );
}

/// Act on a `/goal` line from the composer. A malformed one stays in the
/// composer so it can be fixed; the rest clear it.
pub fn run_command(s: &mut State, id: &str, command: GoalCommand) {
    if let GoalCommand::Invalid(message) = &command {
        crate::chat::report_error(s, id, message.clone());
        return;
    }
    crate::chat::set_composer_text(s, id, String::new());
    match command {
        GoalCommand::Edit => edit_goal(s, id, false),
        GoalCommand::Clear => write_goal(s, id, None, None),
        GoalCommand::Set(objective) => write_goal(s, id, Some(objective), None),
        GoalCommand::SetChecked { objective, check } => write_goal_check(s, id, objective, Some(check)),
        GoalCommand::ClearCheck => write_goal_check(s, id, None, None),
        GoalCommand::Invalid(_) => {}
    }
}

/// Open the editor on the objective, or on the check command.
fn edit_goal(s: &mut State, id: &str, check: bool) {
    let Some(g) = slot(s, id) else { return };
    let goal = g.goal.as_ref();
    let current = if check {
        goal.and_then(|g| g.check.as_ref()).map(|c| c.command.clone())
    } else {
        goal.map(|g| g.objective.clone())
    }
    .unwrap_or_default();
    let mut edit = TextEdit::new(current);
    edit.focused = true;
    g.editor = Some(edit);
    g.editing_check = check;
}

/// Enter in the editor: save the objective, or the check command (empty
/// removes the check, keeping its attempts and timeout otherwise).
fn submit_editor(s: &mut State, id: &str) {
    let Some(g) = slot(s, id) else { return };
    let Some(edit) = &g.editor else { return };
    let text = edit.text.trim().to_string();
    if g.editing_check {
        let keep = g.goal.as_ref().and_then(|g| g.check.as_ref());
        let spec = (!text.is_empty()).then(|| GoalCheckSpec {
            command: text,
            max_attempts: keep.map(|c| c.max_attempts),
            timeout_secs: keep.map(|c| c.timeout_secs),
        });
        write_goal_check(s, id, None, spec);
    } else if !text.is_empty() {
        write_goal(s, id, Some(text), None);
    }
}

fn editor_edit<'a>(s: &'a mut State, id: &str) -> &'a mut TextEdit {
    match slot(s, id).and_then(|g| g.editor.as_mut()) {
        Some(edit) => edit,
        // A late edit after the editor closed lands in a scratch field.
        None => crate::chat::orphan_edit(),
    }
}

/// The goal strip, or its editor while open (GPUI `render_goal`).
pub fn view(s: &State, chat: &ChatState, k: &Kit) -> Option<El> {
    let g = &chat.goals;
    let id = chat.thread_id.clone();
    let p = &s.palette;
    let muted = p.text_muted;
    if let Some(edit) = &g.editor {
        let i = if g.editing_check { &icons::SQUARE_TERMINAL } else { &icons::TARGET };
        let placeholder = if g.editing_check {
            "Command that verifies the goal, e.g. cargo test (empty removes the check)"
        } else {
            "What should this thread achieve?"
        };
        let (edit_id, submit_id, key_id, cancel_id) = (id.clone(), id.clone(), id.clone(), id);
        return Some(
            row()
                .key("goal-editor")
                .gap(8.)
                .center_y()
                .child(ico(i, ICON_XS, muted))
                .child(
                    el().grow(1.).min_w(0.).child(
                        TextInput::new(edit)
                            .placeholder(placeholder)
                            .width(None)
                            .on_edit(move |s: &mut State| editor_edit(s, &edit_id))
                            .on_submit(move |s: &mut State, _| submit_editor(s, &submit_id))
                            .on_key(move |s: &mut State, ev, _| {
                                if ev.key == rune::Key::Escape {
                                    close_editor(s, &key_id);
                                    return true;
                                }
                                false
                            })
                            .build(k)
                            .key("goal-input"),
                    ),
                )
                .child(ghost_button(k, "goal-cancel", "Cancel", None, false, move |s| close_editor(s, &cancel_id))),
        );
    }
    let goal = g.goal.as_ref()?;
    let check = goal.check.as_ref();
    let passed = check.is_some_and(|c| c.status == GoalCheckStatus::Passed);
    let title = match goal.status {
        GoalStatus::Active => "Pursuing goal",
        GoalStatus::Paused => "Goal paused",
        GoalStatus::Done if passed => "Goal met",
        GoalStatus::Done => "Goal complete",
    };
    let done = goal.status == GoalStatus::Done;
    let accent = if passed { p.success } else { muted };
    let chip = check.map(|c| {
        let (label, icon, tone) = check_chip(c);
        let color = match tone {
            Tone::Muted => muted,
            Tone::Good => p.success,
            Tone::Warn => p.warning,
        };
        let mut chip = row().key("goal-check-chip").gap(4.).center_y();
        if c.status == GoalCheckStatus::Running {
            chip = chip.child(spinner(k, ICON_XS, color));
        }
        if let Some(i) = icon {
            chip = chip.child(ico(i, ICON_XS, color));
        }
        chip.child(txt(label, XS, color))
    });
    let check_label = check.map(|c| format!(", check `{}`: {}", c.command, check_chip(c).0)).unwrap_or_default();
    // The last line of a failed run, under the strip.
    let tail = check
        .filter(|c| matches!(c.status, GoalCheckStatus::Failed | GoalCheckStatus::NeedsYou))
        .and_then(|c| c.output_tail.as_deref())
        .and_then(|t| t.lines().rev().find(|l| !l.trim().is_empty()))
        .map(|line| format!("`{}` → {}", check.map(|c| c.command.as_str()).unwrap_or(""), line.trim()));
    let can_run = check.is_some_and(|c| !matches!(c.status, GoalCheckStatus::Pending | GoalCheckStatus::Running));
    let can_stop = check.is_some_and(|c| {
        !matches!(c.status, GoalCheckStatus::Passed | GoalCheckStatus::Running | GoalCheckStatus::Stopped)
    }) && !done;
    let has_check = check.is_some();
    // What "Run check now" approves: the check drawn here, not whatever the
    // state holds when the click lands.
    let shown_revision = check.map(|c| c.revision).unwrap_or_default();

    let mut actions = row().gap(1.).center_y().group_opacity(0., 1.);
    if can_run {
        let id = id.clone();
        actions = actions.child(ghost_icon(k, "goal-check-run", &icons::PLAY, "Run check now", move |s| {
            goal_check_action(s, &id, GoalCheckAction::RunNow, shown_revision)
        }));
    }
    if can_stop {
        let id = id.clone();
        actions = actions.child(ghost_icon(k, "goal-check-stop", &icons::SQUARE, "Stop retrying", move |s| {
            goal_check_action(s, &id, GoalCheckAction::StopRetrying, shown_revision)
        }));
    }
    let edit_check = if has_check { "Edit check" } else { "Add a check" };
    let (check_id, edit_id, done_id, clear_id) = (id.clone(), id.clone(), id.clone(), id);
    actions = actions
        .child(ghost_icon(k, "goal-check-edit", &icons::SQUARE_TERMINAL, edit_check, move |s| {
            edit_goal(s, &check_id, true)
        }))
        .child(ghost_icon(k, "goal-edit", &icons::PENCIL, "Edit goal", move |s| edit_goal(s, &edit_id, false)))
        .child(ghost_icon(
            k,
            "goal-done",
            if done { &icons::UNDO_2 } else { &icons::CHECK },
            if done { "Pursue again" } else { "Mark done" },
            move |s| write_goal(s, &done_id, None, Some(if done { GoalStatus::Active } else { GoalStatus::Done })),
        ))
        .child(ghost_icon(k, "goal-clear", &icons::CLOSE, "Clear goal", move |s| {
            write_goal(s, &clear_id, None, None)
        }));

    let mut strip = row()
        .key("goal-strip")
        .hover_group()
        .role("status")
        .label(format!("{title}: {}{check_label}", goal.objective))
        .h(28.)
        .gap(4.)
        .center_y()
        .child(el().w(24.).center().child(ico(&icons::TARGET, ICON_XS, accent)))
        .child(txt_medium(title, XS, if passed { p.success } else { p.text }))
        .child(txt(goal.objective.clone(), XS, muted).truncate().grow(1.).min_w(0.));
    // A side-pane chat is too narrow for title, objective and chip on one
    // line: the chip drops below, so the objective keeps its room.
    let narrow = chat.embedded;
    let mut below = None;
    match chip {
        Some(chip) if narrow => below = Some(el().key("goal-check-line").pl(28.).child(chip)),
        Some(chip) => strip = strip.child(chip),
        None => {}
    }
    strip = strip.child(actions);
    let mut out = col().key("goal").child(strip);
    if let Some(line) = below {
        out = out.child(line);
    }
    if let Some(line) = tail {
        out = out.child(
            el().pl(28.).child(
                rune::text(line)
                    .font(rune::TextStyle::new(XS, 400.).mono())
                    .color(muted)
                    .truncate(),
            ),
        );
    }
    Some(out)
}

fn close_editor(s: &mut State, id: &str) {
    if let Some(g) = slot(s, id) {
        g.editor = None;
        g.editing_check = false;
    }
}

/// A goal with a failed check, for the headless scene.
pub fn fixture() -> GoalsState {
    GoalsState {
        goal: Some(GoalRecord {
            objective: "Port the chat extras to Rune with full parity".into(),
            status: GoalStatus::Active,
            updated_at: 0,
            check: Some(GoalCheckRecord {
                revision: 3,
                command: "cargo test -p ironclaw_rune".into(),
                max_attempts: 5,
                timeout_secs: 300,
                status: GoalCheckStatus::Failed,
                attempts: 2,
                exit_code: Some(101),
                output_tail: Some("running 12 tests\ntest result: FAILED. 11 passed; 1 failed\n".into()),
                ran_at: None,
            }),
        }),
        loaded: true,
        ..Default::default()
    }
}

/// Scene `goals`: a goal with a failed check at the transcript's end.
pub fn scene(s: &mut State) {
    crate::scenes::chat(s);
    if let Some(c) = s.chat.as_mut() {
        c.goals = fixture();
    }
    crate::chat::fixtures::refold(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::chat_ref;
    use crate::chat::fixtures::{close_side_chat, test_state, with_side_chat};

    #[test]
    fn a_goal_reply_lands_in_its_own_chat_and_is_dropped_once_that_chat_closed() {
        let mut s = test_state();
        with_side_chat(&mut s, "sc2");
        let doc = || ThreadGoalResponse { thread_id: "sc2".into(), goal: fixture().goal };
        let inst = crate::chat::orchestration::instance_of(&mut s, "sc2").expect("side chat extras");
        apply_goal(&mut s, "sc2", inst, Ok(doc()));
        assert!(chat_ref(&s, "sc2").is_some_and(|c| c.goals.goal.is_some()));
        assert!(chat_ref(&s, "t1").is_some_and(|c| c.goals.goal.is_none()), "the open chat's goal is its own");
        close_side_chat(&mut s);
        apply_goal(&mut s, "sc2", inst, Ok(doc()));
        apply_goal(&mut s, "sc2", inst, Err(ClientError::Config("gone".into())));
        let main = chat_ref(&s, "t1");
        assert!(main.is_some_and(|c| c.goals.goal.is_none() && c.error.is_none()));
    }

    #[test]
    fn a_goal_reply_of_a_closed_chat_skips_the_reopened_one() {
        // The same thread, closed and reopened: a new extras instance.
        let mut s = test_state();
        with_side_chat(&mut s, "sc2");
        let old = crate::chat::orchestration::instance_of(&mut s, "sc2").expect("extras");
        close_side_chat(&mut s);
        with_side_chat(&mut s, "sc2");
        let doc = ThreadGoalResponse { thread_id: "sc2".into(), goal: fixture().goal };
        apply_goal(&mut s, "sc2", old, Ok(doc));
        apply_goal(&mut s, "sc2", old, Err(ClientError::Config("late".into())));
        let side = chat_ref(&s, "sc2").expect("reopened side chat");
        assert!(side.goals.goal.is_none(), "the old chat's goal never lands");
        assert!(side.error.is_none(), "nor its error");
        let new = crate::chat::orchestration::instance_of(&mut s, "sc2").expect("extras");
        apply_goal(&mut s, "sc2", new, Ok(ThreadGoalResponse { thread_id: "sc2".into(), goal: fixture().goal }));
        assert!(chat_ref(&s, "sc2").is_some_and(|c| c.goals.goal.is_some()));
    }

    #[test]
    fn goal_commands_parse_like_t3() {
        assert_eq!(goal_command("/goal"), Some(GoalCommand::Edit));
        assert_eq!(goal_command(" /goal clear "), Some(GoalCommand::Clear));
        assert_eq!(goal_command("/goal set ship the inbox"), Some(GoalCommand::Set("ship the inbox".into())));
        assert_eq!(goal_command("/goal ship it"), Some(GoalCommand::Set("ship it".into())));
        assert_eq!(goal_command("/goals"), None);
        assert_eq!(goal_command("set a /goal"), None);
    }

    fn checked(objective: Option<&str>, command: &str) -> GoalCommand {
        GoalCommand::SetChecked {
            objective: objective.map(str::to_string),
            check: GoalCheckSpec { command: command.into(), max_attempts: None, timeout_secs: None },
        }
    }

    #[test]
    fn check_flags_parse_quoted_bare_and_with_options() {
        assert_eq!(
            goal_command(r#"/goal tests pass --check "cargo test -p foo""#),
            Some(checked(Some("tests pass"), "cargo test -p foo"))
        );
        assert_eq!(goal_command("/goal lint clean --check 'npm run lint'"), Some(checked(Some("lint clean"), "npm run lint")));
        assert_eq!(goal_command("/goal green --check make check"), Some(checked(Some("green"), "make check")));
        assert_eq!(goal_command(r#"/goal --check "cargo test""#), Some(checked(None, "cargo test")));
        assert_eq!(goal_command("/goal --check off"), Some(GoalCommand::ClearCheck));
        assert_eq!(
            goal_command(r#"/goal ok --check "cargo test" --attempts 3 --timeout 300s"#),
            Some(GoalCommand::SetChecked {
                objective: Some("ok".into()),
                check: GoalCheckSpec { command: "cargo test".into(), max_attempts: Some(3), timeout_secs: Some(300) },
            })
        );
        assert_eq!(
            goal_command("/goal x --check cargo test --attempts 2"),
            Some(GoalCommand::SetChecked {
                objective: Some("x".into()),
                check: GoalCheckSpec { command: "cargo test".into(), max_attempts: Some(2), timeout_secs: None },
            })
        );
        // `--checks` inside the objective is not the flag.
        assert_eq!(goal_command("/goal add --checks to CI"), Some(GoalCommand::Set("add --checks to CI".into())));
        for bad in [
            r#"/goal x --check "cargo test"#,
            "/goal x --check",
            r#"/goal x --check "a" --attempts many"#,
            r#"/goal x --check "a" --verbose 1"#,
        ] {
            assert!(matches!(goal_command(bad), Some(GoalCommand::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn chips_say_where_the_check_stands() {
        let mut c = GoalCheckRecord {
            revision: 1,
            command: "cargo test".into(),
            max_attempts: 5,
            timeout_secs: 120,
            status: GoalCheckStatus::Failed,
            attempts: 2,
            exit_code: Some(1),
            output_tail: None,
            ran_at: None,
        };
        assert_eq!(check_chip(&c).0, "Check failed 2/5");
        c.status = GoalCheckStatus::NeedsYou;
        c.attempts = 5;
        assert_eq!(check_chip(&c).0, "Needs you · failed 5/5");
        c.status = GoalCheckStatus::Passed;
        assert!(matches!(check_chip(&c).2, Tone::Good));
    }
}
