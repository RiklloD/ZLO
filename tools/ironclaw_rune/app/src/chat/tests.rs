//! Tests for the chat's folding. (Rune diffs the transcript by key itself,
//! so there is no list-diff of the app's own to test.)

use zlo_client::GateDecision;
use zlo_client::transcript::{GateState, Row, ToolStatus};

use std::collections::HashMap;

use zlo_client::transcript::SteerMark;

use super::flow::{Item, summary};

fn tool(key: &str, title: &str) -> Row {
    Row::Tool {
        key: key.into(),
        title: title.into(),
        detail: None,
        status: ToolStatus::Completed,
        reason: None,
    }
}

fn fold(rows: &[Row], live: bool, stopping: bool, tail: bool, reserve: bool) -> Vec<Item> {
    let none = HashMap::new();
    super::flow::fold(
        rows,
        live,
        stopping,
        tail,
        reserve,
        &ctx(&none, &HashMap::new()),
    )
}

fn ctx<'a>(
    spans: &'a HashMap<String, i64>,
    toggles: &'a HashMap<String, bool>,
) -> super::flow::Folding<'a> {
    super::flow::Folding {
        spans,
        toggles,
        prompt_shown: true,
    }
}

fn user(key: &str) -> Row {
    Row::User {
        key: key.into(),
        text: key.into(),
        pending: false,
        attachments: Vec::new(),
        steer: None,
    }
}

fn said(key: &str) -> Row {
    Row::Narration {
        key: key.into(),
        text: key.into(),
    }
}

fn answer(key: &str) -> Row {
    Row::Assistant {
        key: key.into(),
        text: key.into(),
        streaming: false,
    }
}

/// The items as short tags: `fold:<label>`, `act:<first row>`, a row's key.
fn shape(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            Item::TurnFold { label, open, .. } => {
                format!("fold:{label}{}", if *open { ":open" } else { "" })
            }
            Item::Activity { rows, live, .. } => {
                format!("act:{}{}", rows[0].key(), if *live { ":live" } else { "" })
            }
            Item::Working => "working".into(),
            Item::Pending { .. } => "pending".into(),
            other => other.key().to_string(),
        })
        .collect()
}

#[test]
fn summary_reads_like_t3codes() {
    let rows = vec![
        Row::Thinking {
            key: "t".into(),
            text: "hmm".into(),
        },
        tool("a", "ironclaw.shell"),
        tool("b", "ironclaw.shell"),
        tool("c", "mcp.yab-browser.open"),
    ];
    assert_eq!(summary(&rows), "Ran 2 commands and called 1 tool");
    // Commands and edits lead; the rest is counted.
    let rows = vec![
        tool("r", "cli.read"),
        tool("w", "cli.write"),
        tool("g", "cli.grep"),
        tool("x", "cli.bash"),
        tool("y", "cli.bash"),
    ];
    assert_eq!(
        summary(&rows),
        "Edited 1 file, ran 2 commands and performed 2 other actions"
    );
    let thoughts = |n: usize| -> Vec<Row> {
        (0..n)
            .map(|i| Row::Thinking {
                key: format!("t{i}"),
                text: "hmm".into(),
            })
            .collect()
    };
    assert_eq!(summary(&thoughts(1)), "Thought");
    assert_eq!(summary(&thoughts(3)), "Thought (×3)");
}

#[test]
fn settled_gates_fold_into_the_work_and_open_ones_do_not() {
    let gate = |state| Row::Gate {
        key: "g".into(),
        run_id: "r".into(),
        gate_ref: "g".into(),
        kind: zlo_client::contract::ProductGateKind::Approval,
        headline: "Run zsh".into(),
        body: None,
        allow_always: true,
        state,
        reason: None,
    };
    let rows = vec![
        tool("a", "ironclaw.shell"),
        gate(GateState::Closed(Some(GateDecision::Approve {
            always: false,
        }))),
        tool("b", "ironclaw.shell"),
    ];
    assert_eq!(fold(&rows, false, false, false, false).len(), 1);
    let rows = vec![tool("a", "ironclaw.shell"), gate(GateState::Open)];
    // The group, the open card, then (no prompt in view) the working and
    // thinking lines.
    assert_eq!(
        shape(&fold(&rows, true, false, false, false)),
        ["act:a", "g", "working", "pending"]
    );
    // The tail follows the working line and stays above the reserve.
    let items = fold(&rows, true, false, true, true);
    assert!(matches!(
        items[items.len() - 3..],
        [Item::Pending { .. }, Item::Tail, Item::Reserve]
    ));
}

/// The auto-battler turn that drew every commentary line twice: settled,
/// it is the prompt, one "Worked for" line and the answer; opened, the
/// words and tool groups come back in arrival order, each once.
#[test]
fn a_settled_turn_folds_its_work_above_the_answer() {
    let rows = vec![
        user("p"),
        tool("t1", "cli.bash"),
        tool("t2", "cli.read"),
        said("s1"),
        tool("t3", "cli.write"),
        said("s2"),
        tool("t4", "cli.bash"),
        answer("a"),
    ];
    let spans = HashMap::from([("p".to_string(), 1_872_000)]);
    let shut = super::flow::fold(
        &rows,
        false,
        false,
        false,
        false,
        &ctx(&spans, &HashMap::new()),
    );
    assert_eq!(shape(&shut), ["p", "fold:Worked for 31m 12s", "a"]);
    let Item::TurnFold { hidden, .. } = &shut[1] else {
        panic!("a fold");
    };
    assert!(hidden.iter().any(|k| k == "s1") && hidden.iter().any(|k| k == "t3"));
    let toggles = HashMap::from([("fold-p".to_string(), true)]);
    let open = super::flow::fold(&rows, false, false, false, false, &ctx(&spans, &toggles));
    assert_eq!(
        shape(&open),
        [
            "p",
            "fold:Worked for 31m 12s:open",
            "act:t1",
            "s1",
            "act:t3",
            "s2",
            "act:t4",
            "a"
        ]
    );
}

/// The live turn never folds: the working line sits under its prompt, the
/// trailing group follows the newest step and stands in for the thinking
/// line. Older turns fold all the same.
#[test]
fn the_live_turn_works_in_the_open() {
    let rows = vec![
        user("p1"),
        tool("t1", "cli.bash"),
        answer("a1"),
        user("p2"),
        said("s1"),
        tool("t2", "cli.bash"),
    ];
    let items = fold(&rows, true, false, false, false);
    assert_eq!(
        shape(&items),
        [
            "p1",
            "fold:Worked",
            "a1",
            "p2",
            "working",
            "s1",
            "act:t2:live"
        ]
    );
    // Text last: the thinking line closes the turn.
    let mut rows = rows;
    rows.push(said("s2"));
    let items = fold(&rows, true, false, false, false);
    assert_eq!(shape(&items).last().map(String::as_str), Some("pending"));
}

/// A run started elsewhere, its prompt not loaded yet: the turn before it
/// stays folded, and the working line waits at the end.
#[test]
fn a_run_whose_prompt_has_not_loaded_leaves_the_last_turn_folded() {
    let rows = vec![user("p1"), tool("t1", "cli.bash"), answer("a1")];
    let (none, toggles) = (HashMap::new(), HashMap::new());
    let early = super::flow::Folding {
        prompt_shown: false,
        ..ctx(&none, &toggles)
    };
    let items = super::flow::fold(&rows, true, false, false, false, &early);
    assert_eq!(
        shape(&items),
        ["p1", "fold:Worked", "a1", "working", "pending"]
    );
}

/// A plain answer has nothing to fold; a failed or stopped run keeps its
/// work in view; finished work after the answer folds, running work not.
#[test]
fn only_settled_work_folds() {
    let plain = vec![user("p"), answer("a")];
    assert_eq!(shape(&fold(&plain, false, false, false, false)), ["p", "a"]);
    let failed = vec![
        user("p"),
        tool("t1", "cli.bash"),
        Row::RunNotice {
            key: "n".into(),
            run_id: "r".into(),
            text: "Run failed".into(),
            failed: true,
            retryable: true,
        },
    ];
    assert_eq!(
        shape(&fold(&failed, false, false, false, false)),
        ["p", "act:t1", "n"]
    );
    let running = Row::Tool {
        key: "t9".into(),
        title: "cli.bash".into(),
        detail: None,
        status: ToolStatus::Running,
        reason: None,
    };
    let after = vec![
        user("p"),
        tool("t1", "cli.bash"),
        answer("a"),
        tool("t2", "cli.bash"),
        user("q"),
        tool("t3", "cli.bash"),
        answer("b"),
        running,
    ];
    assert_eq!(
        shape(&fold(&after, false, false, false, false)),
        ["p", "fold:Worked", "a", "q", "fold:Worked", "b", "act:t9"]
    );
}

/// The work after a steer folds on its own under "Continued for", so the
/// steer stays between the work it split; the turn's answer stays out.
#[test]
fn a_steer_splits_the_fold() {
    let steer = Row::User {
        key: "s".into(),
        text: "s".into(),
        pending: false,
        attachments: Vec::new(),
        steer: Some(SteerMark {
            continued_ms: Some(90_000),
            ..SteerMark::default()
        }),
    };
    let rows = vec![
        user("p"),
        tool("t1", "cli.bash"),
        said("w"),
        steer,
        tool("t2", "cli.bash"),
        answer("a"),
    ];
    assert_eq!(
        shape(&fold(&rows, false, false, false, false)),
        ["p", "fold:Worked", "s", "fold:Continued for 1m 30s", "a"]
    );
}

#[test]
fn a_late_latest_page_never_replaces_a_history_load() {
    use super::{ChatState, accept_timeline};
    let mut c = ChatState::new("t".into());
    // Load 1 asks for the latest page; a search hit then sets the floor and
    // load 2 pages back to it.
    c.history_floor = Some("msg:old".into());
    assert!(
        accept_timeline(&mut c, 2, Some("msg:old")),
        "the history load lands"
    );
    assert!(
        !accept_timeline(&mut c, 1, None),
        "the delayed latest page is dropped"
    );
    // Same floor: a page asked before one already shown is dropped too.
    assert!(accept_timeline(&mut c, 4, Some("msg:old")));
    assert!(!accept_timeline(&mut c, 3, Some("msg:old")));
    assert!(accept_timeline(&mut c, 5, Some("msg:old")));
}

#[test]
fn a_superseded_timeline_load_keeps_its_error_to_itself() {
    use super::{ChatState, superseded_timeline};
    let mut c = ChatState::new("t".into());
    // Loads 1 and 2 of the latest page: only the newer one may show a failure.
    c.timeline_asked = 2;
    assert!(superseded_timeline(&c, 1, None));
    assert!(!superseded_timeline(&c, 2, None));
    // A search hit moved the floor while load 2 was out.
    c.history_floor = Some("msg:old".into());
    assert!(superseded_timeline(&c, 2, None));
    c.timeline_asked = 3;
    assert!(!superseded_timeline(&c, 3, Some("msg:old")));
}

#[test]
fn every_live_fold_asks_the_run_clock_again() {
    use rune::{Cx, El, Env, Runtime};
    use zlo_client::inbox::InboxState;

    use crate::state::State;

    fn blank(_: &State, _: &mut Cx<State>) -> El {
        rune::el()
    }
    let mut s = super::fixtures::test_state();
    super::fixtures::running(&mut s);
    let id = s
        .chat
        .as_ref()
        .map(|c| c.thread_id.clone())
        .expect("open chat");
    let mut rt = Runtime::new(s, blank, (100., 100.), Env::new(true, 1.));
    let fold = |rt: &mut Runtime<State>| {
        let id = id.clone();
        rt.update(move |s, tx| {
            super::sync_items(s, &id, tx);
        });
        rt.state
            .chat
            .as_ref()
            .and_then(|c| c.run_started)
            .expect("a live run has a start")
    };
    assert!(fold(&mut rt).elapsed().as_secs() < 30, "first seen now");
    // The inbox then reports the backend's turn start, 90 s ago: the next
    // fold takes it, not only the first.
    let mut row = crate::workspace::fixtures::row(&id, "t", "p", InboxState::Working, 0);
    row.working_since = Some(chrono::Utc::now().timestamp_millis() - 90_000);
    rt.update(move |s, _| crate::run_clock::set_rows(s, &[row]));
    assert!(fold(&mut rt).elapsed().as_secs() >= 89);
}

/// A live chat in a runtime, for the poll and fold tests below.
fn live_runtime() -> (rune::Runtime<crate::state::State>, String) {
    use rune::{Cx, El, Env, Runtime};

    use crate::state::State;

    fn blank(_: &State, _: &mut Cx<State>) -> El {
        rune::el()
    }
    let mut s = super::fixtures::test_state();
    super::fixtures::running(&mut s);
    let id = s
        .chat
        .as_ref()
        .map(|c| c.thread_id.clone())
        .expect("open chat");
    (Runtime::new(s, blank, (100., 100.), Env::new(true, 1.)), id)
}

#[test]
fn the_working_clock_repaints_its_seconds_without_a_new_view() {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    let (mut rt, id) = live_runtime();
    let poll = |rt: &mut rune::Runtime<crate::state::State>| {
        let out = Rc::new(Cell::new(false));
        let seen = out.clone();
        let id = id.clone();
        rt.update(move |s, tx| {
            let c = super::chat_for(s, &id).expect("chat");
            // The pacer and the copy check are not under test.
            c.pacing = false;
            c.copied = None;
            if c.run_started.is_none() {
                c.run_started = Some(Instant::now());
            }
            seen.set(super::poll(s, tx));
        });
        out.get()
    };
    assert!(poll(&mut rt), "the first poll shows the seconds");
    // The poll runs on every frame of any animation; within the same second
    // it must not ask for a new view.
    for _ in 0..5 {
        assert!(!poll(&mut rt), "same second, no new view");
    }
    let back = id.clone();
    let earlier = move |rt: &mut rune::Runtime<crate::state::State>, secs: u64| {
        let back = back.clone();
        rt.update(move |s, _| {
            let c = super::chat_for(s, &back).expect("chat");
            c.run_started = c.run_started.map(|t| t - Duration::from_secs(secs));
        });
    };
    let label = |rt: &rune::Runtime<crate::state::State>| {
        super::chat_ref(&rt.state, &id)
            .map(|c| c.clock_text.get().to_string())
            .unwrap_or_default()
    };
    let shown = |rt: &rune::Runtime<crate::state::State>| {
        super::chat_ref(&rt.state, &id)
            .and_then(|c| c.clock_secs)
            .expect("a second shown")
    };
    let at = shown(&rt);
    // The next second turns: the live label repaints, the view stays.
    earlier(&mut rt, 1);
    assert!(!poll(&mut rt), "a tick built a new view");
    assert_eq!(label(&rt), super::pending::format_elapsed(at + 1));
    // Past the hour the label's box widens: the line itself changes.
    let hour = 3_600u64.saturating_sub(at + 1).max(1);
    earlier(&mut rt, hour);
    assert!(poll(&mut rt), "the widened label needs a new view");
    assert_eq!(label(&rt), super::pending::format_elapsed(at + 1 + hour));
}

#[test]
fn the_working_clock_asks_for_no_wake_while_the_window_is_in_the_background() {
    let (mut rt, id) = live_runtime();
    rt.poll = Some(|s, _, tx| super::poll(s, tx));
    rt.update(move |s, _| {
        let c = super::chat_for(s, &id).expect("chat");
        c.pacing = false;
        c.copied = None;
    });
    rt.frame(0.);
    rt.frame(1. / 60.);
    assert!(rt.wake_deadline().is_some(), "in front: the next second");

    rt.update(|s, tx| crate::window_focus(s, false, tx));
    // Past the wake asked for while it was in front.
    rt.frame(1.5);
    rt.frame(1. / 60.);
    assert_eq!(rt.wake_deadline(), None, "in the background: no wake");

    rt.update(|s, tx| crate::window_focus(s, true, tx));
    rt.frame(1. / 60.);
    rt.frame(1. / 60.);
    assert!(rt.wake_deadline().is_some(), "in front again: it counts on");
}

#[test]
fn stream_rows_wait_for_the_pacers_next_step() {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    let (mut rt, id) = live_runtime();
    let fold = |rt: &mut rune::Runtime<crate::state::State>, due_in: Option<Duration>| {
        let out = Rc::new(Cell::new(false));
        let seen = out.clone();
        let id = id.clone();
        rt.update(move |s, tx| {
            super::sync_items(s, &id, tx);
            let c = super::chat_for(s, &id).expect("chat");
            // New rows from the stream, with the pacer between steps.
            c.rows_gen += 1;
            c.pacing = due_in.is_some();
            c.pace_due = due_in.map(|d| Instant::now() + d);
            seen.set(super::sync_items(s, &id, tx));
        });
        out.get()
    };
    assert!(
        fold(&mut rt, Some(Duration::from_secs(5))),
        "a step is pending: the rows wait for it"
    );
    assert!(
        !fold(&mut rt, Some(Duration::ZERO)),
        "the step is due: the rows fold now"
    );
    assert!(!fold(&mut rt, None), "no pacer: the rows fold now");
}

/// A search hit on commentary folded into a settled turn opens the fold,
/// found by its stored text when the row kept the live text's key.
#[test]
fn a_search_hit_inside_a_shut_fold_opens_it() {
    let mut s = super::fixtures::test_state();
    super::fixtures::cli_turn_settled(&mut s);
    let said = "First, the decision record. It's the contract that everything else follows.";
    if let Some(c) = s.chat.as_mut() {
        let page = serde_json::from_value(serde_json::json!({
            "thread": { "thread_id": "t1" },
            "messages": [{ "message_id": "m-said", "sequence": 3, "kind": "assistant",
                           "status": "finalized", "content": said }]
        }))
        .expect("timeline decodes");
        c.transcript.set_timeline(page);
        assert!(
            !c.items.iter().any(|i| i.key() == "say:c1"),
            "the commentary starts folded"
        );
    }
    let mut tx = rune::Tx::default();
    super::reveal_message(&mut s, "m-said".into(), &mut tx);
    let c = s.chat.as_ref().expect("chat");
    assert_eq!(c.toggles.get("fold-msg:c1"), Some(&true), "the fold opened");
    assert!(
        c.items.iter().any(|i| i.key() == "say:c1"),
        "the hit is shown"
    );
    assert!(c.reveal.is_none(), "the hit was found");
}
