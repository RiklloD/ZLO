//! One open thread: transcript, live stream, composer, run controls. The
//! extras (`model_picker`, `goals`, `orchestration`, `computer`) and the
//! docks (`side`) are modules of their own, whose sub-states sit in
//! [`ChatState`].
//!
//! The chat is [`ChatState`] in `State::chat`; its
//! methods are free functions over `&mut State` that find the chat again
//! (and check it is still the same thread) before writing. The split:
//!
//! | File | What |
//! |---|---|
//! | `chat.rs` | state, open, the live stream, timeline loads, folding rows into items, poll |
//! | `chat/actions.rs` | send, stop, retry, gates, commands, attachments, files |
//! | `chat/view.rs` | the main column: transcript list, banner, error, menu, composer |
//! | `chat/rows.rs` | one transcript row: bubbles, gates, notices, command answers |
//! | `chat/composer.rs` | the composer card, its keys and round buttons |
//! | `chat/flow.rs` .. `chat/reject_feedback.rs` | one capability each |

pub mod actions;
pub mod approval_mode;
mod approval_reason;
pub mod composer;
pub mod computer;
mod dispatch;
pub(crate) use dispatch::FOLLOW_UPS_HELP;
pub(crate) mod downloads;
mod file_links;
pub mod flow;
mod glyphs;
pub mod goals;
mod hooks;
pub mod inline_files;
pub mod menu;
pub mod model_picker;
pub mod orchestration;
mod outline;
mod pace;
mod pending;
pub mod plan_strip;
pub mod prewarm;
mod question_form;
pub(crate) mod reject_feedback;
mod rows;
pub mod side;
pub mod snapshot;
pub(crate) mod steer;
mod view;
mod visual;

#[cfg(test)]
mod burst_tests;
pub mod fixtures;
#[cfg(test)]
mod motion_tests;
mod stream_inbox;
#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rune::{Cx, El, TextEdit, Tx};
use rune_kit::Kit;
use zlo_client::attachments::StagedFile;
use zlo_client::transcript::{Row, RunEnd, Transcript};
use zlo_client::wire::AttachmentLimits;
use zlo_client::{EventStream, StreamMessage, StreamStatus};

use crate::jobs::{self, Generation};
use crate::state::State;

pub use flow::Item;
pub use hooks::{
    active_run, approval_mode_changed, embedded, focus_composer, focus_composer_of, open_error,
    reload_timeline, set_composer_text, set_fork_prefix, set_merges, stage_snapshot, stop_run,
    voice_composer, voice_composer_mut, voice_press_enter, with_tx,
};
pub use view::view;

const TIMELINE_LIMIT: u32 = 200;
/// How long the copy button shows its check.
const COPIED_FOR: Duration = Duration::from_millis(1400);
/// How long a fold's glide is carried after the click that started it:
/// the fold spring (`motion::ROW_RESORT`, 0.26 s) and its settle.
const FOLD_HOLD: Duration = Duration::from_millis(600);

/// A person opened or shut a finished activity group (or a step in it):
/// the groups fold on their glide until [`FOLD_HOLD`] has passed, and the
/// poll is woken then to drop it.
pub(crate) fn fold_now(c: &mut ChatState, tx: &mut Tx) {
    c.folding_until = Some(Instant::now() + FOLD_HOLD);
    tx.wake_after(FOLD_HOLD);
}

/// How long past [`COPIED_FOR`] the poll waits before clearing a copy check
/// whose revert timer never came (`rows::mark_copied`).
const COPIED_GRACE: Duration = Duration::from_millis(500);
/// The pacer's step. MonoCode stepped at 30 Hz; each step here rebuilds the
/// view, and Rune's word reveal already staggers a step's words in one after
/// another, so 20 Hz reads the same at two thirds of the work.
const PACE_TICK: Duration = Duration::from_millis(50);

/// Zeron's reading column; Appearance → Conversation width moves it.
pub fn content_width(s: &State) -> f32 {
    s.prefs.conversation_width.clamp(480., 1200.)
}

/// Zeron's turn: room kept under the newest user message so it sits high
/// while its answer streams. The room outlives the run, so the view stays
/// put when it ends; the next send replaces it.
pub(crate) struct Reserve {
    /// False only when the send failed: the room then goes away.
    pub(crate) held: bool,
}

/// One open thread. `side`, `model_picker`, `goals`, `orchestration` and
/// `computer` belong to the modules of those names.
pub struct ChatState {
    pub thread_id: String,
    pub channel: Option<String>,
    pub limits: AttachmentLimits,
    /// Files to go with the next message.
    pub staged: Vec<StagedFile>,
    pub transcript: Transcript,
    pub rows: Rc<Vec<Row>>,
    /// Bumped with `rows`; with the other fold inputs it tells `sync_items`
    /// when there is nothing to redo.
    pub(crate) rows_gen: u64,
    /// The transcript moved since `rows` was built (since then): the next
    /// poll rebuilds them, once per frame however many stream bursts landed
    /// before it.
    rows_stale: Option<Instant>,
    folded: Option<(u64, bool, bool, bool, bool)>,
    /// What the list shows: `rows` folded, plus the pending and reserve rows.
    pub items: Rc<Vec<Item>>,
    /// Activity groups (and tree rows) the reader opened or closed by hand.
    pub(crate) toggles: HashMap<String, bool>,
    /// How long each settled turn worked, by its prompt's row key
    /// (`Transcript::turn_spans`): the "Worked for" of its fold.
    pub(crate) spans: Rc<HashMap<String, i64>>,
    /// The open "Reject with feedback…" field under an approval card.
    pub(crate) reject: Option<reject_feedback::RejectEditor>,
    /// When the live run began, for the pending row's seconds.
    pub(crate) run_started: Option<Instant>,
    /// The whole second the working line last showed, so the poll sets
    /// [`Self::clock_text`] once a second, not on every frame of the run.
    clock_secs: Option<u64>,
    /// The working line's seconds, painted in place: a tick repaints the
    /// label without building the view again (`pending::clock_view` says
    /// when the line itself changes and does need a new view).
    pub(crate) clock_text: rune::LiveText,
    pub(crate) reserve: Option<Reserve>,
    /// The transcript viewport's height, for the reserve.
    pub(crate) list_height: f32,
    /// Drawn height of each row of the newest turn, by item key, so the
    /// reserve only adds what keeps that prompt at the top.
    pub(crate) turn_heights: HashMap<String, f32>,
    /// Scroll the newest user message to the top on the next item sync.
    pin_next: bool,
    /// The prompt just sent (its row key) and when: it rises from the
    /// composer into place (`view::prompt_rise`).
    pub(crate) rise: Option<(String, Instant)>,
    /// The view held the newest prompt in place over a reserve correction
    /// (`view::transcript`), which takes the list off its end.
    pub(crate) held_prompt: bool,
    /// Until then the send's climb runs: the list follows its end (which
    /// the reserve makes the prompt at the top) rather than holding the
    /// prompt where it was first drawn, which stopped the climb before it
    /// began and left the new message under the fold.
    pub(crate) climb_until: Option<Instant>,
    /// A history-search hit to scroll to once its message is in the list.
    reveal: Option<String>,
    /// Keep the transcript loaded back to this message (an older hit was
    /// opened): timeline reloads page back to it instead of the latest page.
    history_floor: Option<String>,
    /// A load back to `history_floor` is in flight.
    floor_pending: bool,
    /// Timeline loads, numbered as asked: the last one asked, and the
    /// newest one shown. An older page never replaces a newer one.
    timeline_asked: u64,
    timeline_shown: u64,
    /// The chat opened from the new-thread screen (its first message).
    pub arrive: bool,
    /// Streaming replies revealed at a steady rate.
    pacer: pace::Pacer,
    pub(crate) pacing: bool,
    /// When the pacer's next step is due. The poll runs on every frame of
    /// any animation; folding the rows (and rebuilding the view) only when a
    /// step is due keeps the pacer at its 30 Hz however fast frames come.
    pace_due: Option<Instant>,
    pub(crate) outline: outline::OutlineState,
    /// Whether the transcript ends with the tail row (work, goal, queue).
    pub(crate) has_tail: bool,
    /// The reader scrolled away from the end (the jump button shows).
    pub(crate) far_from_bottom: bool,
    /// Files dragged from Finder are over the chat.
    pub(crate) drop_over: bool,
    pub composer: TextEdit,
    pub status: StreamStatus,
    pub error: Option<String>,
    /// Commands, skills and extensions for the `/` and `@` menu.
    pub menu: menu::ComposerMenu,
    /// The profile's approval-mode pill after the model pill.
    pub approval: approval_mode::ApprovalPill,
    /// The agent's live checklist above the composer.
    pub plan_strip: plan_strip::PlanStrip,
    /// What the person has filled in on open question forms.
    pub(crate) questions: question_form::Drafts,
    /// A side chat drawn inside its parent's right pane.
    pub embedded: bool,
    /// The live event stream; dropping it closes the subscription.
    stream: Option<EventStream>,
    /// Bumped with each subscription, so a replaced stream's last messages
    /// are dropped.
    stream_gen: Generation,
    /// Talk mode's claim on the next send (the new-thread screen's first
    /// message carries it here).
    pub voice_send: Option<crate::voice::Submission>,
    /// The message whose copy button shows its check, and since when.
    pub(crate) copied: Option<(String, Instant)>,
    /// Until when finished activity groups fold on their own glide: set by a
    /// person's click on a group or step (`fold_now`), cleared by the poll
    /// once it passes. Carried at rest, the glide cost commits on every
    /// scroll through a long group.
    pub(crate) folding_until: Option<Instant>,
    /// Sent files being fetched to open (by attachment id): their cards
    /// breathe meanwhile.
    pub(crate) downloading: HashSet<String>,
    /// Send's and Stop's plate colours (`composer::PlateTint`).
    pub(crate) send_tint: composer::PlateTint,
    pub(crate) stop_tint: composer::PlateTint,
    /// A headless scene's running turn (`fixtures`); `None` in the app.
    pub(crate) fixture_run: Option<String>,
    pub side: side::SideState,
    pub model_picker: model_picker::ModelPickerState,
    pub goals: goals::GoalsState,
    pub orchestration: orchestration::OrchestrationState,
    pub computer: computer::ComputerState,
}

impl ChatState {
    pub fn new(thread_id: String) -> Self {
        let mut composer = TextEdit::new("");
        composer.focused = true;
        Self {
            thread_id,
            channel: None,
            limits: AttachmentLimits::default(),
            staged: Vec::new(),
            transcript: Transcript::default(),
            rows: Rc::new(Vec::new()),
            rows_gen: 0,
            rows_stale: None,
            folded: None,
            items: Rc::new(Vec::new()),
            toggles: HashMap::new(),
            spans: Rc::default(),
            reject: None,
            run_started: None,
            clock_secs: None,
            clock_text: rune::LiveText::new(""),
            reserve: None,
            list_height: 0.,
            turn_heights: HashMap::new(),
            pin_next: false,
            rise: None,
            held_prompt: false,
            climb_until: None,
            reveal: None,
            history_floor: None,
            floor_pending: false,
            timeline_asked: 0,
            timeline_shown: 0,
            arrive: false,
            pacer: pace::Pacer::default(),
            pacing: false,
            pace_due: None,
            outline: Default::default(),
            has_tail: false,
            far_from_bottom: false,
            drop_over: false,
            composer,
            status: StreamStatus::Connecting,
            error: None,
            menu: Default::default(),
            approval: Default::default(),
            plan_strip: Default::default(),
            questions: Default::default(),
            embedded: false,
            stream: None,
            stream_gen: Generation::default(),
            voice_send: None,
            copied: None,
            folding_until: None,
            downloading: Default::default(),
            send_tint: Default::default(),
            stop_tint: Default::default(),
            fixture_run: None,
            side: Default::default(),
            model_picker: Default::default(),
            goals: Default::default(),
            orchestration: Default::default(),
            computer: Default::default(),
        }
    }

    /// The send's climb to the top is still running.
    pub(crate) fn climbing(&self) -> bool {
        self.climb_until.is_some_and(|until| Instant::now() < until)
    }

    /// The running turn, streamed (IronClaw's loop) or polled (CLI runtime).
    pub fn running_run(&self) -> Option<String> {
        self.transcript
            .active_run()
            .or(self.orchestration.queue.external_run())
            .or(self.fixture_run.as_deref())
            .map(str::to_string)
    }

    /// Whether the activity group `key` is drawn open.
    pub(crate) fn activity_open(&self, key: &str) -> bool {
        flow::is_open(key, &self.toggles)
    }

    /// The composer's key for voice and focus: one per thread.
    pub fn composer_key(&self) -> String {
        format!("composer:{}", self.thread_id)
    }

    /// The approval card that still waits on the person, newest first.
    pub(crate) fn open_approval(&self) -> Option<(String, String, bool)> {
        use zlo_client::contract::ProductGateKind;
        use zlo_client::transcript::GateState;
        self.rows.iter().rev().find_map(|row| match row {
            Row::Gate {
                run_id,
                gate_ref,
                kind: ProductGateKind::Approval,
                allow_always,
                state: GateState::Open,
                ..
            } => Some((run_id.clone(), gate_ref.clone(), *allow_always)),
            _ => None,
        })
    }
}

/// The chat showing `thread_id`: the open one, or a side chat inside its
/// right pane. Every handler and async reply finds its chat through this, so
/// a reply for a thread that was closed meanwhile goes nowhere.
pub fn chat_for<'a>(s: &'a mut State, thread_id: &str) -> Option<&'a mut ChatState> {
    if s.chat.as_ref().is_some_and(|c| c.thread_id == thread_id) {
        return s.chat.as_mut();
    }
    side::side_chat_mut(s, thread_id)
}

/// Read-only [`chat_for`].
pub fn chat_ref<'a>(s: &'a State, thread_id: &str) -> Option<&'a ChatState> {
    match s.chat.as_ref() {
        Some(c) if c.thread_id == thread_id => Some(c),
        _ => side::side_chat(s, thread_id),
    }
}

/// A `TextEdit` for an `on_edit` accessor whose owner went away between the
/// event and its handler (the chat closed, a form was answered). It is never
/// shown; the few bytes it takes in that rare race are not worth a slot.
pub(crate) fn orphan_edit() -> &'static mut TextEdit {
    tracing::debug!("chat: edit for a field that is gone");
    Box::leak(Box::new(TextEdit::new("")))
}

/// `thread_id`'s composer, for `on_edit`.
pub(crate) fn composer_edit<'a>(s: &'a mut State, thread_id: &str) -> &'a mut TextEdit {
    match chat_for(s, thread_id) {
        Some(c) => &mut c.composer,
        None => orphan_edit(),
    }
}

/// Open `thread_id` (replacing the open chat). Called by the workspace.
/// Replacing the chat drops the old one's stream, which frees one of the
/// gateway's per-user stream slots.
pub fn open(s: &mut State, thread_id: String) {
    if s.chat.as_ref().is_some_and(|c| c.thread_id == thread_id) {
        return;
    }
    let mut chat = build(s, thread_id.clone(), false);
    // Rows fetched while the pointer rested on its row show at once.
    prewarm::adopt(s, &mut chat);
    s.chat = Some(chat);
    start(s, &thread_id);
}

/// A side chat inside its parent's right pane: no docks of its own. Area S
/// keeps the returned state in `side::SideChat`, then calls [`start_side_chat`]
/// so its stream and timeline load (`chat_for` finds it from then on).
pub fn build_side_chat(s: &State, thread_id: String) -> ChatState {
    let mut chat = build(s, thread_id, true);
    chat.composer.focused = false;
    chat
}

/// The first loads of a side chat the side pane just put in its slot.
pub fn start_side_chat(s: &mut State, thread_id: &str) {
    start(s, thread_id);
}

fn build(s: &State, thread_id: String, embedded: bool) -> ChatState {
    let mut chat = ChatState::new(thread_id);
    chat.embedded = embedded;
    if let Some(session) = &s.session {
        chat.channel = session.session_channel_extension_id.clone();
        chat.limits = session.attachments.clone();
    }
    // An unsent draft comes back.
    if let Some(draft) = crate::drafts::restore(s, &chat.thread_id) {
        chat.staged = draft.files;
        let pills: Vec<_> = draft.tokens.into_iter().map(|t| (t.range, t.id)).collect();
        inline_files::set_content(&mut chat.composer, &draft.text, &pills);
    }
    if embedded {
        chat.model_picker.set_compact(true);
        chat.approval.compact = true;
    }
    chat
}

/// The first loads of a freshly opened chat.
fn start(s: &mut State, id: &str) {
    if s.fixture {
        return;
    }
    open_stream(s, id);
    load_timeline(s, id);
    actions::load_commands(s, id);
    approval_mode::load(s, id);
    // Every chat, main or side, has its own extras (picker, orchestration,
    // screen share); the docks belong to the open chat only.
    orchestration::opened(s, id);
    model_picker::start(s, id);
    computer::start(s, id);
    if is_main(s, id) {
        side::init(s);
    }
}

/// `id` is the open chat (not a side chat in its pane).
pub(crate) fn is_main(s: &State, id: &str) -> bool {
    s.chat.as_ref().is_some_and(|c| c.thread_id == id)
}

/// Re-read `id`'s orchestration state (its tasks, queue, goal and lineage).
pub(crate) fn refresh_extras(s: &mut State, id: &str) {
    orchestration::refresh(s, id);
}

/// The thread's live event stream: a task on the network runtime forwards
/// every burst of messages to the UI thread, where `on_stream` applies it
/// to the chat if that thread is still open.
pub(crate) fn open_stream(s: &mut State, id: &str) {
    let Some(backend) = s.rt.backend.clone() else {
        return;
    };
    let handle = s.rt.handle.clone();
    let Some(c) = chat_for(s, id) else { return };
    let generation = c.stream_gen.bump();
    let thread = c.thread_id.clone();
    // Drop the old subscription first: one stream slot per open chat.
    c.stream = None;
    let (stream, mut rx) = backend.subscribe(&thread);
    c.stream = Some(stream);
    let inbox = std::sync::Arc::new(stream_inbox::StreamInbox::default());
    backend.spawn(move |_| async move {
        while let Some(first) = rx.recv().await {
            // Drain whatever else is queued so a burst lands at once.
            let mut batch = vec![first];
            while let Ok(next) = rx.try_recv() {
                batch.push(next);
            }
            // One update waits per stream at most: bursts that arrive
            // before it runs join it instead of queueing their own.
            if !inbox.push(batch) {
                continue;
            }
            let thread = thread.clone();
            let inbox = inbox.clone();
            // Only a burst that changes what shows rebuilds the view: one
            // the next frame's poll folds in, or that changed nothing,
            // costs no frame here.
            handle.update_if(move |s, tx| {
                crate::energy::hit(crate::energy::C::UpdateStream);
                let batch = inbox.take();
                !s.fixture && !batch.is_empty() && on_stream(s, &thread, generation, batch, tx)
            });
        }
    });
}

/// Reopen the live stream (the thread moved to another runtime).
pub fn resubscribe(s: &mut State, id: &str) {
    open_stream(s, id);
}

pub(crate) fn load_timeline(s: &mut State, id: &str) {
    let Some(c) = chat_for(s, id) else { return };
    c.timeline_asked += 1;
    let asked = c.timeline_asked;
    let floor = c.history_floor.clone();
    let loaded_floor = floor.clone();
    let thread_id = id.to_string();
    let req = thread_id.clone();
    jobs::run(
        s,
        "chat",
        move |c| async move {
            match floor {
                Some(message_id) => c.timeline_through(&req, &message_id).await,
                None => c.timeline(&req, TIMELINE_LIMIT).await,
            }
        },
        move |s, result, tx| {
            let Some(c) = chat_for(s, &thread_id) else {
                return;
            };
            // Only the load that reached the floor settles it.
            if c.history_floor == loaded_floor {
                c.floor_pending = false;
            }
            match result {
                Ok(_) if !accept_timeline(c, asked, loaded_floor.as_deref()) => {
                    tracing::debug!(thread_id = %thread_id, "dropped a superseded timeline page");
                }
                Ok(page) => {
                    c.transcript.set_timeline(page);
                    refresh_rows(s, &thread_id, tx);
                    // Server-set titles reach the sidebar.
                    crate::workspace::thread_updated(s);
                }
                Err(e) if superseded_timeline(c, asked, loaded_floor.as_deref()) => {
                    tracing::debug!(thread_id = %thread_id, error = %e, "dropped a superseded timeline error");
                }
                Err(e) => set_error(s, &thread_id, e.to_string()),
            }
        },
    );
}

/// Whether timeline load `asked` back to `floor` was overtaken: the floor
/// moved since, or a later load was asked. Its failure is then not the
/// thread's to show; the newer load decides.
fn superseded_timeline(c: &ChatState, asked: u64, floor: Option<&str>) -> bool {
    c.history_floor.as_deref() != floor || asked < c.timeline_asked
}

/// Whether a timeline page asked as load `asked` back to `floor` may
/// replace the transcript: not when the floor changed since (a latest-page
/// load must not undo a search hit's history load), nor when a load asked
/// later already landed. Notes it as shown when it may.
fn accept_timeline(c: &mut ChatState, asked: u64, floor: Option<&str>) -> bool {
    if c.history_floor.as_deref() != floor || asked < c.timeline_shown {
        return false;
    }
    c.timeline_shown = asked;
    true
}

/// Apply a burst of stream messages. Returns whether anything the view reads
/// changed now (rows the pacer takes in at its next step do not count).
fn on_stream(
    s: &mut State,
    id: &str,
    generation: Generation,
    batch: Vec<StreamMessage>,
    tx: &mut Tx,
) -> bool {
    let Some(c) = chat_for(s, id) else {
        return false;
    };
    if c.stream_gen != generation {
        return false;
    }
    // The CLI agent's turn diff and review, to tell the Changes tab (area
    // S) when either moves.
    let cli_before = c.transcript.cli_report_rev();
    let mut changed = false;
    let mut visible = false;
    let mut ended = None;
    let mut reload = false;
    let mut closed = None;
    for message in batch {
        match message {
            StreamMessage::Frame(frame) => {
                let applied = c.transcript.apply(&frame.event);
                crate::energy::hit(if applied.changed {
                    crate::energy::C::StreamChanged
                } else {
                    crate::energy::C::StreamIdle
                });
                changed |= applied.changed;
                reload |= applied.reload_timeline;
                ended = applied.run_ended.or(ended);
            }
            StreamMessage::Status(status) => {
                visible |= c.status != status;
                c.status = status;
            }
            StreamMessage::Undecodable { event, error } => {
                tracing::debug!(%event, %error, "skipped an undecodable stream frame");
            }
            StreamMessage::Closed { reason } => closed = Some(reason),
        }
    }
    if let Some(reason) = closed {
        set_error(s, id, format!("Live updates stopped: {reason}"));
        visible = true;
    }
    if reload {
        load_timeline(s, id);
        visible = true;
    }
    if changed {
        if ended.is_some() {
            // The run's end reads the settled rows (voice, alerts) now.
            visible |= !refresh_rows(s, id, tx);
        } else if let Some(c) = chat_for(s, id) {
            // Rows are rebuilt from the whole transcript: leave that to the
            // next frame's poll, so a burst of deltas costs one rebuild per
            // frame, not one per burst that landed before it.
            c.rows_stale.get_or_insert_with(Instant::now);
            tx.wake_after(Duration::ZERO);
        }
    }
    let cli_moved = chat_ref(s, id).is_some_and(|c| c.transcript.cli_report_rev() != cli_before);
    if cli_moved && is_main(s, id) {
        side::on_cli_report(s);
        visible = true;
    }
    // A run end or reload can move tasks and the queue; a run start only
    // needs the slow poll armed.
    if reload || ended.is_some() {
        orchestration::refresh(s, id);
    } else {
        orchestration::ensure_poll(s, id);
    }
    visible |= ended.is_some();
    if ended.is_some() && is_main(s, id) {
        side::on_turn_ended(s);
    }
    // Stopping a run is the person's own doing; no alert for that.
    if let Some(end @ (RunEnd::Completed | RunEnd::Failed)) = ended {
        let Some(c) = chat_ref(s, id) else {
            return visible;
        };
        if end == RunEnd::Failed {
            tracing::warn!(thread_id = %id, "run failed");
        }
        let run = c.transcript.last_settled_run().map(str::to_string);
        let rows = c.rows.clone();
        crate::voice::run_ended(s, id, run.as_deref(), end == RunEnd::Completed, &rows);
        crate::workspace::run_ended(s, id.to_string(), end == RunEnd::Failed);
    }
    visible
}

/// Rebuild the rows from the transcript and fold them into list items.
/// Returns whether the fold waits for the pacer's next step (see
/// [`sync_items`]).
pub(crate) fn refresh_rows(s: &mut State, id: &str, tx: &mut Tx) -> bool {
    let Some(c) = chat_for(s, id) else {
        return false;
    };
    crate::energy::hit(crate::energy::C::RowsRebuilt);
    c.rows_stale = None;
    c.rows = Rc::new(c.transcript.rows());
    c.spans = Rc::new(c.transcript.turn_spans());
    c.rows_gen += 1;
    question_form::sync_drafts(c);
    sync_items(s, id, tx)
}

/// Fold rows into list items: the pacer's view of streaming replies, the
/// pending row while a run is live, the tail and the reserve.
///
/// While the pacer runs and its next step is not due, new rows wait for that
/// step (at most [`PACE_TICK`]) instead of folding now: the reply grows at
/// the pacer's rate either way, and the view rebuilds once, not twice.
/// Returns true when it deferred like that.
pub(crate) fn sync_items(s: &mut State, id: &str, tx: &mut Tx) -> bool {
    // The pacer is motion too: off with "Reduce motion" and while the window
    // is in the background (nobody watches it type; the text lands whole).
    let reduced = s.prefs.reduce_motion == "on" || s.look.battery.background();
    let has_tail = orchestration::has_tail(s, id);
    let Some(c) = chat_for(s, id) else {
        return false;
    };
    c.has_tail = has_tail;
    let run = c.running_run();
    let live = run.is_some();
    let stopping = c.transcript.is_stopping();
    if live || stopping {
        // Every live fold asks again: the
        // backend's turn start may have arrived since, and a run id that
        // arrived after the send is noted against the clock.
        let sent = c.transcript.last_prompt_sent_ms();
        let at = crate::run_clock::start(s, id, run.as_deref(), sent);
        if let Some(c) = chat_for(s, id) {
            c.run_started = Some(at);
        }
    } else if c.run_started.take().is_some() {
        crate::run_clock::end(s, id);
    }
    let Some(c) = chat_for(s, id) else {
        return false;
    };
    let inputs = (c.rows_gen, live, stopping, c.has_tail, c.reserve.is_some());
    if !c.pacing && !c.pin_next && c.folded == Some(inputs) {
        return false;
    }
    let step_pending = c.pace_due.is_some_and(|due| due > Instant::now());
    let same_shape = c
        .folded
        .is_some_and(|f| (f.1, f.2, f.3, f.4) == (inputs.1, inputs.2, inputs.3, inputs.4));
    if c.pacing && step_pending && !c.pin_next && same_shape {
        return true;
    }
    c.folded = Some(inputs);
    crate::energy::hit(crate::energy::C::ItemsFolded);
    let rows = c.rows.clone();
    let (rows, pacing) = c.pacer.apply(&rows, Instant::now(), reduced);
    c.pacing = pacing;
    c.pace_due = pacing.then(|| Instant::now() + PACE_TICK);
    if pacing {
        crate::energy::hit(crate::energy::C::PacerStep);
        tx.wake_after(PACE_TICK);
    }
    let new_items = flow::fold(
        &rows,
        live,
        stopping,
        c.has_tail,
        c.reserve.is_some(),
        &flow::Folding {
            spans: &c.spans,
            toggles: &c.toggles,
            prompt_shown: c.transcript.shows_running_prompt(),
        },
    );
    if *c.items == new_items && !c.pin_next {
        return false;
    }
    let pin = std::mem::take(&mut c.pin_next);
    c.outline.set(outline::prompts(&new_items));
    c.items = Rc::new(new_items);
    if pin {
        // The reserve keeps exactly the room under the newest prompt, so
        // the end of the list is the prompt at the top.
        tx.scroll_to_bottom(view::transcript_key(id));
        c.climb_until = Some(Instant::now() + view::CLIMB);
        // MonoCode's prompt rise, once per send (none under reduced motion).
        c.rise = (!reduced)
            .then(|| {
                c.items.iter().rev().find_map(|i| match i {
                    Item::Row(Row::User { key, .. }) => Some((key.clone(), Instant::now())),
                    _ => None,
                })
            })
            .flatten();
    } else if c.climbing() {
        // Mid-climb: keep heading for the end as rows change under it.
        tx.scroll_to_bottom(view::transcript_key(id));
    } else if let Some(anchor) = view::prompt_anchor(c) {
        // Rows of the held turn changed (the server copy took the bubble's
        // place, the working line gave way to the answer, the run ended):
        // the reserve is resized from their heights only after this frame
        // lays out, and the list follows its end meanwhile, so for one
        // frame everything jumped by the change. Hold the prompt where it is
        // drawn across this commit instead.
        tx.keep_on_screen(anchor);
        // Holding takes the list off its end: once the answer outgrows the
        // room, the layout handler goes back to following it.
        c.held_prompt = true;
    }
    try_reveal(s, id, tx);
    false
}

/// Scroll to `message_id` (a history-search hit) as soon as it is in the
/// loaded transcript of the open chat.
pub fn reveal_message(s: &mut State, message_id: String, tx: &mut Tx) {
    let Some(c) = s.chat.as_mut() else { return };
    c.reveal = Some(message_id);
    // A search hit is somewhere to go: the send's climb gives way.
    c.climb_until = None;
    let id = c.thread_id.clone();
    try_reveal(s, &id, tx);
}

fn try_reveal(s: &mut State, id: &str, tx: &mut Tx) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(target) = c.reveal.clone() else {
        return;
    };
    // A message sent from this view keeps its bubble's key, and a reply
    // keeps the key of the live text it replaced: find that row by its
    // stored text.
    let sent = c.transcript.key_of_message(&target);
    let kept = c.transcript.message_text(&target).and_then(|text| {
        let text = text.trim();
        c.rows.iter().find_map(|r| match r {
            Row::Assistant { key, text: t, .. } | Row::Narration { key, text: t }
                if !text.is_empty() && t.trim() == text =>
            {
                Some(key.clone())
            }
            _ => None,
        })
    });
    let hit = |key: &str| key.ends_with(&target) || key == sent || kept.as_deref() == Some(key);
    // A hit inside a settled turn's shut fold opens the fold first.
    let folded = c.items.iter().find_map(|item| match item {
        Item::TurnFold {
            key,
            hidden,
            open: false,
            ..
        } if hidden.iter().any(|k| hit(k)) => Some(key.clone()),
        _ => None,
    });
    if let Some(fold) = folded {
        c.toggles.insert(fold, true);
        c.folded = None;
        sync_items(s, id, tx);
    }
    let Some(c) = chat_for(s, id) else { return };
    let found = c.items.iter().find_map(|item| match item {
        Item::Activity { rows, key, .. } => rows.iter().any(|r| hit(r.key())).then(|| key.clone()),
        other => hit(other.key()).then(|| other.key().to_string()),
    });
    let Some(key) = found else {
        // Not in the latest page: once that page is in, load back to the
        // message (bounded), then try again; give up if even the older
        // pages do not hold it.
        if c.items.is_empty() {
            return;
        }
        if c.history_floor.as_deref() == Some(target.as_str()) {
            if !c.floor_pending {
                tracing::debug!("search hit is not in the thread's timeline");
                c.reveal = None;
            }
        } else {
            c.history_floor = Some(target);
            c.floor_pending = true;
            load_timeline(s, id);
        }
        return;
    };
    c.reveal = None;
    tx.scroll_into_view(view::row_key(&key));
}

pub(crate) fn set_error(s: &mut State, id: &str, message: impl Into<String>) {
    let message = message.into();
    let Some(c) = chat_for(s, id) else { return };
    tracing::warn!(thread_id = %id, error = %message, "chat action failed");
    c.error = Some(message);
}

/// Called before every frame: the pacer's steps, the working line's
/// seconds and the copy check turning back, for the open chat and its side
/// chats.
pub fn poll(s: &mut State, tx: &mut Tx) -> bool {
    let mut ids: Vec<String> = s.chat.iter().map(|c| c.thread_id.clone()).collect();
    ids.extend(side::side_chat_ids(s));
    let mut changed = false;
    for id in ids {
        changed |= poll_one(s, &id, tx);
    }
    changed
}

fn poll_one(s: &mut State, id: &str, tx: &mut Tx) -> bool {
    poll_chat(s, id, tx.input_waiting(), tx)
}

/// How long the stream's fold and the pacer's step may wait behind the
/// user's input: about one pacer step, so a reply keeps growing (if a
/// little coarser) under a continuous scroll or drag.
const INPUT_YIELD_MAX: Duration = Duration::from_millis(34);

/// Whether work owed since `since` waits a frame for the input queued for
/// this one (`Tx::input_waiting`): the input's frame comes first, the work
/// in the frame after, unless it already waited [`INPUT_YIELD_MAX`].
fn yields_to_input(input_waiting: bool, since: Instant, now: Instant) -> bool {
    input_waiting && now.saturating_duration_since(since) < INPUT_YIELD_MAX
}

/// [`poll_one`] with the frame's `input_waiting` passed in (tests set it;
/// a headless runtime's input carries no timestamp, so it never reads true).
fn poll_chat(s: &mut State, id: &str, input_waiting: bool, tx: &mut Tx) -> bool {
    let front = crate::workspace::window_active(s);
    let Some(c) = chat_for(s, id) else {
        return false;
    };
    let mut changed = false;
    let now = Instant::now();
    // Both of these rebuild from the whole thread: keys, clicks and the
    // wheel go first (they run after this poll, in this frame), and the
    // fold comes on the next frame.
    if let Some(since) = c.rows_stale {
        if yields_to_input(input_waiting, since, now) {
            tx.wake_after(Duration::ZERO);
        } else {
            // Stream bursts since the last frame, folded in once.
            let deferred = refresh_rows(s, id, tx);
            changed = crate::energy::polled(crate::energy::C::PollChatStream, !deferred);
        }
    }
    let Some(c) = chat_for(s, id) else {
        return changed;
    };
    if c.pacing {
        // The pacer reveals more on each step, once a step is due.
        match c.pace_due {
            Some(due) if due > now => tx.wake_after(due - now),
            Some(due) if yields_to_input(input_waiting, due, now) => {
                tx.wake_after(Duration::ZERO);
            }
            _ => {
                sync_items(s, id, tx);
                changed = crate::energy::polled(crate::energy::C::PollChatPace, true);
            }
        }
    }
    // Computer use's cursor glides, ripples and trails.
    changed |= crate::energy::polled(crate::energy::C::PollComputer, computer::poll(s, id, tx));
    let Some(c) = chat_for(s, id) else {
        return changed;
    };
    // A fold's glide is over: drop it from the groups (one view, no motion).
    if c.folding_until.is_some_and(|until| now >= until) {
        c.folding_until = None;
        changed = true;
    }
    // The copy check reverts on its own timer (`rows::mark_copied`), which
    // animates; this only clears one whose timer was lost, with no wake.
    if let Some((_, at)) = &c.copied
        && at.elapsed() >= COPIED_FOR + COPIED_GRACE
    {
        c.copied = None;
        changed = crate::energy::polled(crate::energy::C::PollChatCopied, true);
    }
    // The working line counts seconds while a run is live: the label is
    // repainted in place when the shown second turns (the poll also runs on
    // every frame of any animation), a new view only when the line itself
    // changes (its word rolls), and one wake at the next turn while the
    // window is in front.
    match c.run_started {
        Some(at) => {
            let elapsed = at.elapsed();
            let secs = elapsed.as_secs();
            let was = c.clock_secs.replace(secs);
            if was != Some(secs) {
                c.clock_text.set(pending::format_elapsed(secs));
                // A run's first second builds the line; later ones only
                // when it changes.
                let rebuilt =
                    was.is_none_or(|w| pending::clock_view(w) != pending::clock_view(secs));
                changed |= crate::energy::polled(crate::energy::C::PollChatClock, rebuilt);
            }
            // Nobody watches the seconds of a window in the back; focus
            // asks for a poll (`workspace::window_focus`), which resumes.
            if front {
                tx.wake_after(Duration::from_secs(secs + 1).saturating_sub(elapsed));
            }
        }
        None => c.clock_secs = None,
    }
    changed
}

/// The new-thread screen's first message and its files, sent once the
/// thread exists. The draft screen calls this right after `open`.
pub fn send_text(
    s: &mut State,
    text: String,
    pills: Vec<(std::ops::Range<usize>, String)>,
    files: Vec<StagedFile>,
    voice: Option<crate::voice::Submission>,
    tx: &mut Tx,
) {
    let Some(c) = s.chat.as_mut() else { return };
    // Talk mode's claim from the new-thread or quick composer: only this
    // first message may answer it.
    c.voice_send = voice;
    c.staged = files;
    c.arrive = true;
    inline_files::set_content(&mut c.composer, &text, &pills);
    let id = c.thread_id.clone();
    actions::send(s, &id, tx);
}

/// The model picker changed the thread: a fork opens
/// the new thread, a switch moves the live stream, an in-place change only
/// reloads the timeline for its marker.
pub fn picker_changed(s: &mut State, id: &str, fork_thread_id: Option<String>, moved: &str) {
    if let Some(fork) = fork_thread_id {
        crate::workspace::open_thread(s, fork);
    } else if moved == "switched" {
        resubscribe(s, id);
        load_timeline(s, id);
    } else if moved == "in_place" {
        load_timeline(s, id);
    }
}

/// A failure an extra (model picker, goals, computer use) reports in the
/// chat's error line.
pub fn report_error(s: &mut State, id: &str, message: impl Into<String>) {
    set_error(s, id, message);
}

/// A side chat drawn inside its parent's pane: the side pane renders an
/// embedded `ChatState` with this.
pub fn embedded_view(
    s: &State,
    chat: &ChatState,
    cx: &mut Cx<State>,
    k: &Kit,
    layer: &mut Vec<El>,
) -> El {
    view::column(s, chat, cx, k, layer)
}

/// The open thread's titlebar actions at the breadcrumb's right end: the
/// side pane's toggles (and the screen-share toggle).
pub fn titlebar_actions(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    s.chat.as_ref()?;
    // The capsule fades in as a chat opens from another page, and out as
    // one leaves for it, instead of popping with the page change.
    Some(
        rune::el()
            .key("chat-titlebar-actions")
            .transition(rune::Transition::opacity())
            .child(side::titlebar_actions(s, cx, k, layer)),
    )
}
