//! An activity group's tree of steps. A group of a few dozen steps lays
//! every row out; a turn with hundreds or thousands of tool calls windows
//! its rows as a virtual list nested in the transcript's row (Rune #956):
//! it windows against the transcript's own scroll viewport, so only the
//! steps in view are built, while the group's header and the rows around
//! it stay as they are.

use std::borrow::Cow;
use std::rc::Rc;

use rune::{El, GrowthSpec, col};

use super::{Leaf, TREE_ROW_HEIGHT, tree_row};
use crate::state::State;

/// Above this many steps the tree is a nested virtual list. Below it, every
/// row is built (a viewport shows about 25 steps; the window mounts a few
/// viewports), so ordinary groups are unchanged.
pub(super) const NEST_AFTER: usize = 64;

/// One step of the tree, worked out once per view.
#[derive(Clone)]
pub(super) struct Line {
    pub key: String,
    pub leaf: Leaf,
    pub expanded: bool,
    pub open_key: String,
}

/// The steps under an open group's header, in `tree` (the keyed column).
pub(super) fn tree(s: &State, thread: &str, tree: El, lines: Vec<Line>) -> El {
    if lines.len() <= NEST_AFTER {
        // Each step sits in the same `{key}-step` column the virtual list
        // gives its rows, so a live group crossing the threshold keeps every
        // step's keyed path: a text selection inside a step still resolves.
        return tree.children(lines.into_iter().map(|l| {
            col()
                .w_full()
                .key(format!("{}-step", l.key))
                .children(tree_row(s, thread, &l.key, l.leaf, l.expanded, l.open_key))
        }));
    }
    let count = lines.len();
    let lines = Rc::new(lines);
    let keys = lines.clone();
    let thread = thread.to_string();
    tree.growth(GrowthSpec::default())
        .whole_rows()
        .virtual_rows(
            count,
            TREE_ROW_HEIGHT,
            move |_: &State, i| {
                Cow::Owned(
                    keys.get(i)
                        .map_or_else(String::new, |l| format!("{}-step", l.key)),
                )
            },
            // A virtual row takes the list's key for its own, so each step
            // sits in a plain column and keeps `{key}-leaf`, which its
            // press holds on screen.
            move |s: &State, i| match lines.get(i) {
                Some(l) => col().w_full().children(tree_row(
                    s,
                    &thread,
                    &l.key,
                    l.leaf.clone(),
                    l.expanded,
                    l.open_key.clone(),
                )),
                None => rune::el(),
            },
        )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rune::{Cx, El, Env, GrowthSpec, Runtime, ScrollSpec, col};
    use zlo_client::transcript::{Row, ToolStatus};

    use super::NEST_AFTER;
    use crate::state::State;

    fn tools(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| Row::Tool {
                key: format!("t{i}"),
                title: "Read".into(),
                detail: Some(format!("file{i}.rs")),
                status: ToolStatus::Completed,
                reason: None,
            })
            .collect()
    }

    thread_local! {
        static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// The open activity group, as the transcript builds it.
    fn group(s: &State) -> El {
        let k = crate::design::kit(&s.palette);
        let rows = tools(STEPS.with(|n| n.get()));
        super::super::render_activity(
            s,
            "thread",
            "t0",
            &rows,
            false,
            true,
            &HashMap::new(),
            &HashMap::new(),
            &k,
        )
    }

    /// A transcript-like viewport: a virtual list whose second row is the
    /// group, following its end.
    fn view(_: &State, _: &mut Cx<State>) -> El {
        col()
            .key("viewport")
            .w_full()
            .h(600.)
            .scroll(ScrollSpec { stick_bottom: true })
            .child(
                col()
                    .key("rows")
                    .w_full()
                    .growth(GrowthSpec::default())
                    .virtual_rows(
                        2,
                        72.,
                        |_: &State, i| {
                            if i == 0 {
                                "before".into()
                            } else {
                                "turn".into()
                            }
                        },
                        |s: &State, i| {
                            if i == 0 {
                                col().w_full().h(200.)
                            } else {
                                group(s)
                            }
                        },
                    ),
            )
    }

    fn leaves(rt: &Runtime<State>) -> usize {
        rt.elements()
            .iter()
            .filter(|e| e.key.as_deref().is_some_and(|k| k.ends_with("-leaf")))
            .count()
    }

    fn runtime(steps: usize) -> Runtime<State> {
        STEPS.with(|n| n.set(steps));
        let s = crate::chat::fixtures::test_state();
        let mut rt = Runtime::new(s, view, (600., 600.), Env::new(true, 1.));
        for _ in 0..30 {
            rt.frame(1. / 60.);
        }
        rt
    }

    #[test]
    fn a_short_group_builds_every_step() {
        let rt = runtime(NEST_AFTER);
        assert_eq!(leaves(&rt), NEST_AFTER);
        assert!(rt.presented("t0-head").is_some());
    }

    #[test]
    fn a_step_keeps_its_keyed_wrapper_on_both_sides_of_the_threshold() {
        // A live group crossing NEST_AFTER must not re-parent its steps under
        // a new keyed node, or a selection inside a step stops resolving.
        let steps = |rt: &Runtime<State>| {
            rt.elements()
                .iter()
                .filter(|e| e.key.as_deref().is_some_and(|k| k.ends_with("-step")))
                .count()
        };
        let flat = runtime(NEST_AFTER);
        assert_eq!(
            steps(&flat),
            NEST_AFTER,
            "every flat step has its -step key"
        );
        let nested = runtime(NEST_AFTER + 1);
        assert!(nested.presented("t64-step").is_some());
        assert!(nested.presented("t64-leaf").is_some());
    }

    #[test]
    fn a_thousand_tool_calls_mount_only_the_steps_near_the_viewport() {
        let rt = runtime(1000);
        let mounted = leaves(&rt);
        assert!(
            mounted > 0 && mounted < 200,
            "the nested list windows the steps: {mounted} of 1000 built"
        );
        // Pinned to the end, the window sits on the last steps.
        assert!(rt.presented("t999-leaf").is_some(), "the end is in view");
        assert!(rt.presented("t0-leaf").is_none(), "the start is not built");
    }
}
