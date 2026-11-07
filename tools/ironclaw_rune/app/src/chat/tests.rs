//! Tests for the chat's folding (GPUI `chat/flow.rs` tests). GPUI's
//! `list_diff` tests have no port: Rune diffs the transcript by key itself.

use ironclaw_gpui_client::GateDecision;
use ironclaw_gpui_client::transcript::{GateState, Row, ToolStatus};

use super::flow::{Item, fold, summary};

fn tool(key: &str, title: &str) -> Row {
    Row::Tool {
        key: key.into(),
        title: title.into(),
        detail: None,
        status: ToolStatus::Completed,
        reason: None,
    }
}

#[test]
fn summary_reads_like_zerons() {
    let rows = vec![
        Row::Thinking {
            key: "t".into(),
            text: "hmm".into(),
        },
        tool("a", "ironclaw.shell"),
        tool("b", "ironclaw.shell"),
        tool("c", "mcp.yab-browser.open"),
    ];
    assert_eq!(
        summary(&rows),
        "Thought process · ran 2 commands · called 1 tool"
    );
}

#[test]
fn settled_gates_fold_into_the_work_and_open_ones_do_not() {
    let gate = |state| Row::Gate {
        key: "g".into(),
        run_id: "r".into(),
        gate_ref: "g".into(),
        kind: ironclaw_gpui_client::contract::ProductGateKind::Approval,
        headline: "Run zsh".into(),
        body: None,
        allow_always: true,
        state,
        reason: None,
    };
    let rows = vec![
        tool("a", "ironclaw.shell"),
        gate(GateState::Closed(Some(GateDecision::Approve { always: false }))),
        tool("b", "ironclaw.shell"),
    ];
    assert_eq!(fold(&rows, false, false, false, false).len(), 1);
    let rows = vec![tool("a", "ironclaw.shell"), gate(GateState::Open)];
    assert_eq!(fold(&rows, true, false, false, false).len(), 3);
    // The tail follows the working line and stays above the reserve.
    let items = fold(&rows, true, false, true, true);
    assert!(matches!(
        items[items.len() - 3..],
        [Item::Pending { .. }, Item::Tail, Item::Reserve]
    ));
}

#[test]
fn the_working_word_rotates_every_seven_seconds() {
    use super::pending::{flavour_word, format_elapsed};
    assert_eq!(flavour_word(0, 0), flavour_word(0, 6));
    assert_ne!(flavour_word(0, 0), flavour_word(0, 7));
    assert_eq!(format_elapsed(59), "59s");
    assert_eq!(format_elapsed(125), "2m 5s");
    assert_eq!(format_elapsed(3_700), "1h 1m");
}

#[test]
fn a_late_latest_page_never_replaces_a_history_load() {
    use super::{ChatState, accept_timeline};
    let mut c = ChatState::new("t".into());
    // Load 1 asks for the latest page; a search hit then sets the floor and
    // load 2 pages back to it.
    c.history_floor = Some("msg:old".into());
    assert!(accept_timeline(&mut c, 2, Some("msg:old")), "the history load lands");
    assert!(!accept_timeline(&mut c, 1, None), "the delayed latest page is dropped");
    // Same floor: a page asked before one already shown is dropped too.
    assert!(accept_timeline(&mut c, 4, Some("msg:old")));
    assert!(!accept_timeline(&mut c, 3, Some("msg:old")));
    assert!(accept_timeline(&mut c, 5, Some("msg:old")));
}
