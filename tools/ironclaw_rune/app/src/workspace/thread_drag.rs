//! Dragging a sidebar thread onto a chat or the new-thread composer, to
//! hand it that thread's transcript as a file (GPUI's `DraggedThread`:
//! `inbox.rs` `on_drag`, `chat.rs` and `draft.rs` `on_drop`).
//!
//! Rune drags elements only within themselves, so this is a small state
//! machine in the app:
//!
//! - An inbox row's `on_drag` ([`drag`]) records the thread and the pointer
//!   in window points. Past [`SLOP`] the drag is live: the shell draws
//!   [`ghost`] under the pointer in the overlay layer.
//! - Each drop target reports where it is drawn (`on_drawn_layout`) with
//!   [`saw_chat`] / [`saw_draft`]; the view highlights the one under the
//!   pointer ([`over_chat`], [`over_draft`]).
//! - On release, [`drop_at`] tests the pointer against the targets of the
//!   page on screen and attaches the thread to the one it is over.
//!
//! A release back inside the row would also press it; [`take_press`] eats
//! that one press so a drag never opens the thread it started on.

use std::collections::HashMap;

use rune::{Drag, El, Phase, Tx, icon, row, text};

use crate::icons;
use crate::state::{Page, State};

/// Pointer travel before a press becomes a drag (Rune's click slop).
pub const SLOP: f32 = 4.;

/// The thread being dragged.
#[derive(Clone, Debug, PartialEq)]
pub struct Dragged {
    pub id: String,
    pub title: String,
    /// The pointer in window points.
    pub at: [f32; 2],
    /// Moved past [`SLOP`]: the ghost shows and a release drops.
    pub live: bool,
}

#[derive(Default)]
pub struct ThreadDrag {
    pub dragged: Option<Dragged>,
    /// The drag ended back on its row, whose press follows: skip it.
    swallow_press: bool,
    /// Each chat column's drawn rect, by thread id.
    chats: HashMap<String, [f32; 4]>,
    /// The new-thread composer card's drawn rect.
    draft: Option<[f32; 4]>,
}

/// Where a release lands.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Chat(String),
    Draft,
}

fn contains(r: [f32; 4], [x, y]: [f32; 2]) -> bool {
    x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3]
}

/// An inbox row's `on_drag`: start, follow and drop `id`.
pub fn drag(s: &mut State, id: &str, title: &str, d: &Drag, tx: &mut Tx) {
    let at = d.window_point();
    let state = &mut s.workspace.thread_drag;
    match d.phase {
        Phase::Down => {
            state.swallow_press = false;
            state.dragged = Some(Dragged {
                id: id.to_string(),
                title: title.to_string(),
                at,
                live: false,
            });
            tx.live_only();
        }
        Phase::Move => {
            let Some(dragged) = state.dragged.as_mut() else {
                return;
            };
            let was_live = dragged.live;
            dragged.at = at;
            dragged.live |= d.dx.abs() > SLOP || d.dy.abs() > SLOP;
            if !was_live && !dragged.live {
                tx.live_only();
            }
        }
        Phase::Up => {
            let Some(dragged) = state.dragged.take() else {
                return;
            };
            if !dragged.live {
                return;
            }
            // Rune presses the row when the release is still on it.
            state.swallow_press = d.x >= 0. && d.y >= 0. && d.x <= d.w && d.y <= d.h;
            if d.cancelled() {
                return;
            }
            drop_at(s, Dragged { at, ..dragged });
        }
    }
}

/// The row's press after a drag that ended on it: `true` once, to skip it.
pub fn take_press(s: &mut State) -> bool {
    std::mem::take(&mut s.workspace.thread_drag.swallow_press)
}

/// The target under `at` on the page shown now. The open chat wins over a
/// side chat's rect: a closed pane's last rect may lie under it.
pub fn target_at(s: &State, at: [f32; 2]) -> Option<Target> {
    let state = &s.workspace.thread_drag;
    match s.page {
        Page::Draft => state
            .draft
            .filter(|r| s.workspace.draft.is_some() && contains(*r, at))
            .map(|_| Target::Draft),
        Page::Chat => {
            let main = s.chat.as_ref().map(|c| c.thread_id.as_str());
            let hit = |id: &str| state.chats.get(id).is_some_and(|r| contains(*r, at));
            if let Some(id) = main.filter(|id| hit(id)) {
                return Some(Target::Chat(id.to_string()));
            }
            state
                .chats
                .keys()
                .filter(|id| Some(id.as_str()) != main)
                .find(|id| hit(id) && crate::chat::chat_ref(s, id).is_some())
                .map(|id| Target::Chat(id.clone()))
        }
        _ => None,
    }
}

/// Attach `dragged` to the target under its pointer, if any.
pub fn drop_at(s: &mut State, dragged: Dragged) {
    match target_at(s, dragged.at) {
        Some(Target::Chat(id)) => {
            crate::chat::actions::attach_thread(s, &id, dragged.id, dragged.title);
        }
        Some(Target::Draft) => super::draft::attach_thread(s, dragged.id, dragged.title),
        None => {}
    }
}

/// The live drag's target, for the views' highlight.
fn hovered(s: &State) -> Option<Target> {
    let dragged = s
        .workspace
        .thread_drag
        .dragged
        .as_ref()
        .filter(|d| d.live)?;
    target_at(s, dragged.at)
}

/// A thread is dragged over `id`'s chat column.
pub fn over_chat(s: &State, id: &str) -> bool {
    hovered(s) == Some(Target::Chat(id.to_string()))
}

/// A thread is dragged over the new-thread composer.
pub fn over_draft(s: &State) -> bool {
    hovered(s) == Some(Target::Draft)
}

/// `id`'s chat column was drawn at `r` (its `on_drawn_layout_tx`).
pub fn saw_chat(s: &mut State, id: &str, r: [f32; 4], tx: &mut Tx) {
    if s.workspace.thread_drag.chats.insert(id.to_string(), r) == Some(r) {
        tx.live_only();
    }
}

/// The new-thread composer card was drawn at `r`.
pub fn saw_draft(s: &mut State, r: [f32; 4], tx: &mut Tx) {
    if s.workspace.thread_drag.draft.replace(r) == Some(r) {
        tx.live_only();
    }
}

/// The card under the pointer while a thread is dragged (GPUI
/// `DraggedThread::render`), for the shell's overlay layer.
pub fn ghost(s: &State) -> Option<El> {
    let dragged = s
        .workspace
        .thread_drag
        .dragged
        .as_ref()
        .filter(|d| d.live)?;
    let p = &s.palette;
    let [x, y] = dragged.at;
    Some(
        row()
            .key("thread-drag-ghost")
            .pin(Some(x + 8.), Some(y + 8.), None, None)
            .max_w(260.)
            .px(10.)
            .py(6.)
            .gap(6.)
            .center_y()
            .radius(8.)
            .fill(p.surface_overlay)
            .border(1., p.border_strong)
            .shadow(4., 12., rune::Color::BLACK.alpha(0.2))
            .child(icon(&icons::FILE, 14.).color(p.text_muted))
            .child(
                text(dragged.title.clone())
                    .font(rune::TextStyle::new(13., super::ui::REGULAR).lh(18.))
                    .color(p.text)
                    .truncate()
                    .min_w(0.),
            ),
    )
}

/// A scene's live drag of `id` with the pointer at `at`. `rect` stands in
/// for the page's target (the draft card, or the open chat's column) as
/// first drawn: a one-frame snap has none yet.
pub fn fixture(s: &mut State, id: &str, title: &str, at: [f32; 2], rect: [f32; 4]) {
    let chat = s.chat.as_ref().map(|c| c.thread_id.clone());
    let state = &mut s.workspace.thread_drag;
    match (s.page, chat) {
        (Page::Draft, _) => state.draft = Some(rect),
        (_, Some(id)) => {
            state.chats.insert(id, rect);
        }
        _ => {}
    }
    state.dragged = Some(Dragged {
        id: id.into(),
        title: title.into(),
        at,
        live: true,
    });
}

#[cfg(test)]
mod tests {
    use rune::{App, AppSpec, Cx, Env, Runtime, col, el};

    use super::*;

    fn unused_init() -> State {
        unreachable!("tests build the state themselves")
    }

    fn blank(_: &State, _: &mut Cx<State>) -> El {
        el()
    }

    /// A state with no window or gateway behind it.
    fn state() -> State {
        let handle = App::new(AppSpec::new("test", (400., 300.), unused_init, blank)).handle();
        State::new(crate::jobs::Rt::new(handle))
    }

    fn rects(s: &mut State) {
        let state = &mut s.workspace.thread_drag;
        state.chats.insert("t1".into(), [300., 0., 600., 800.]);
        state.draft = Some([300., 300., 600., 120.]);
    }

    /// The target is the page's own: the draft rect counts only on the
    /// new-thread screen, a chat's only on the chat page and only while
    /// that chat exists.
    #[test]
    fn targets_follow_the_page() {
        let mut s = state();
        rects(&mut s);
        s.page = Page::Draft;
        assert_eq!(target_at(&s, [400., 350.]), None, "no draft open");
        s.workspace.draft = Some(super::super::draft::empty_draft("p"));
        assert_eq!(target_at(&s, [400., 350.]), Some(Target::Draft));
        assert_eq!(target_at(&s, [100., 350.]), None);
        s.page = Page::Chat;
        assert_eq!(target_at(&s, [400., 350.]), None, "no chat open");
        s.chat = Some(crate::chat::ChatState::new("t1".into()));
        assert_eq!(target_at(&s, [400., 350.]), Some(Target::Chat("t1".into())));
        assert_eq!(target_at(&s, [100., 350.]), None);
    }

    /// A row at the top left that drags `t2` and, pressed, opens the
    /// settings page; the draft card's stand-in at (200, 150) 160x100; the
    /// ghost in the overlay layer.
    fn view(s: &State, _: &mut Cx<State>) -> El {
        let source = el()
            .key("source")
            .size(100., 40.)
            .on_press(|s: &mut State| {
                if !take_press(s) {
                    s.page = Page::Settings;
                }
            })
            .on_drag(|s: &mut State, d, tx| drag(s, "t2", "Other thread", d, tx));
        let target = el()
            .key("target")
            .pin(Some(200.), Some(150.), None, None)
            .size(160., 100.)
            .on_drawn_layout_tx(saw_draft);
        let mut root = col().w_full().h_full().child(source).child(target);
        if let Some(ghost) = ghost(s) {
            root = root.child(ghost);
        }
        root
    }

    fn runtime() -> Runtime<State> {
        let mut s = state();
        s.page = Page::Draft;
        s.workspace.draft = Some(super::super::draft::empty_draft("p"));
        let mut rt = Runtime::new(s, view, (400., 300.), Env::new(true, 1.));
        // The second frame runs the target's `on_drawn_layout`.
        rt.frame(0.);
        rt.frame(0.);
        rt
    }

    fn step(rt: &mut Runtime<State>) {
        rt.frame(1. / 60.);
    }

    /// Press, travel past the slop to the target, release: the ghost
    /// follows the pointer, the target lights up, and the drop reaches the
    /// draft (which takes the focus, as GPUI's drop does).
    #[test]
    fn a_drag_onto_the_target_drops_there() {
        let mut rt = runtime();
        assert_eq!(
            rt.state.workspace.thread_drag.draft,
            Some([200., 150., 160., 100.])
        );
        rt.pointer_move(20., 20.);
        rt.pointer_down(20., 20.);
        step(&mut rt);
        rt.pointer_move(22., 21.);
        step(&mut rt);
        assert!(rt.find("thread-drag-ghost").is_none(), "within the slop");
        rt.pointer_move(250., 200.);
        step(&mut rt);
        let dragged = rt.state.workspace.thread_drag.dragged.clone();
        assert_eq!(
            dragged.as_ref().map(|d| (d.id.as_str(), d.live)),
            Some(("t2", true))
        );
        assert!(rt.find("thread-drag-ghost").is_some());
        assert!(over_draft(&rt.state));
        if let Some(d) = rt.state.workspace.draft.as_mut() {
            d.input.focused = false;
        }
        rt.pointer_up(250., 200.);
        step(&mut rt);
        assert!(rt.state.workspace.thread_drag.dragged.is_none());
        assert!(rt.find("thread-drag-ghost").is_none());
        assert!(
            rt.state
                .workspace
                .draft
                .as_ref()
                .is_some_and(|d| d.input.focused)
        );
        assert_eq!(rt.state.page, Page::Draft, "the row was not pressed");
    }

    /// A drag released back on its row drops nothing and does not open the
    /// row; the next plain click does.
    #[test]
    fn a_drag_back_onto_its_row_is_not_a_press() {
        let mut rt = runtime();
        rt.pointer_move(20., 20.);
        rt.pointer_down(20., 20.);
        step(&mut rt);
        rt.pointer_move(250., 200.);
        step(&mut rt);
        rt.pointer_move(30., 20.);
        step(&mut rt);
        rt.pointer_up(30., 20.);
        step(&mut rt);
        assert_eq!(rt.state.page, Page::Draft);
        assert!(rt.state.workspace.thread_drag.dragged.is_none());
        rt.pointer_down(30., 20.);
        step(&mut rt);
        rt.pointer_up(30., 20.);
        step(&mut rt);
        assert_eq!(rt.state.page, Page::Settings);
    }

    /// A release outside every target drops nothing.
    #[test]
    fn a_drag_released_elsewhere_drops_nothing() {
        let mut rt = runtime();
        rt.pointer_move(20., 20.);
        rt.pointer_down(20., 20.);
        step(&mut rt);
        rt.pointer_move(120., 280.);
        step(&mut rt);
        assert!(!over_draft(&rt.state));
        if let Some(d) = rt.state.workspace.draft.as_mut() {
            d.input.focused = false;
        }
        rt.pointer_up(120., 280.);
        step(&mut rt);
        assert!(
            rt.state
                .workspace
                .draft
                .as_ref()
                .is_some_and(|d| !d.input.focused)
        );
        assert_eq!(rt.state.page, Page::Draft);
    }
}
