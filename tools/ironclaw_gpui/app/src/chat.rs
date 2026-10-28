//! One open thread: transcript, live stream, composer, run controls.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ironclaw_gpui_client::attachments::{self, StagedFile, format_bytes};
use ironclaw_gpui_client::commands::{match_command, menu_matches, takes_arguments};
use ironclaw_gpui_client::contract::ProductGateKind;
use ironclaw_gpui_client::inbound_requests::{AnchoredPart, anchored_parts};
use ironclaw_gpui_client::product_wire::RebornProductCommandInfo;
use ironclaw_gpui_client::transcript::{
    self, AttachmentChip, GateState, PREFIX_KEY, Row, RunEnd, ToolStatus, Transcript,
};
use ironclaw_gpui_client::wire::{AttachmentLimits, CommandEffect};
use ironclaw_gpui_client::workspace_views::{ProjectFsEntry, ProjectFsEntryKind};
use ironclaw_gpui_client::{
    Backend, ClientError, EventStream, GateDecision, StreamMessage, StreamStatus,
};

use crate::assets::ExtraIcon;
use crate::design::styles::Style;
use crate::design::{Palette, kit, motion};
use crate::jobs::flatten;
use crate::ui::*;

pub(crate) mod approval_mode;
mod approval_reason;
mod computer;
mod flow;
mod goals;
pub(crate) mod inline_files;
pub mod model_picker;
mod orchestration;
mod outline;
mod pace;
mod side;
mod snapshot;
mod steer;

use flow::Item;
use model_picker::{ModelPicker, PickerEvent};

const TIMELINE_LIMIT: u32 = 200;
/// Zeron's reading column; Appearance → Conversation width moves it.
fn content_width(cx: &App) -> f32 {
    crate::design::prefs::Prefs::of(cx)
        .conversation_width
        .clamp(480., 1200.)
}

/// One folder of the conversation's own workspace.
struct FilesPanel {
    /// `None` is the conversation's root folder.
    path: Option<String>,
    entries: Option<Vec<ProjectFsEntry>>,
    error: Option<SharedString>,
}

pub struct ChatView {
    backend: Backend,
    channel: Option<String>,
    limits: AttachmentLimits,
    /// Files to go with the next message.
    staged: Vec<StagedFile>,
    thread_id: String,
    transcript: Transcript,
    rows: Rc<Vec<Row>>,
    /// What the list shows: `rows` folded, plus the pending and reserve rows.
    items: Rc<Vec<Item>>,
    /// Activity groups the reader opened or closed by hand.
    toggles: HashMap<String, bool>,
    /// When the live run began, for the pending row's seconds. The start
    /// itself lives in `run_clock`, so it outlives this view.
    run_started: Option<Instant>,
    /// Room kept under the last sent message so it can sit at the top.
    reserve: Option<Reserve>,
    /// The list's drawn height, for the prompt rise's travel.
    list_height: Rc<Cell<f32>>,
    /// Drawn height of each row of the newest turn, by item key, so the
    /// reserve only adds what keeps that prompt at the top.
    turn_heights: Rc<RefCell<HashMap<String, f32>>>,
    /// Scroll the newest user message to the top on the next item sync.
    pin_next: bool,
    /// When the last message was sent: MonoCode's prompt rise lifts it from
    /// the composer to its place, then the reply rows slide in.
    prompt_rise: Option<Instant>,
    /// The view opened from the new-thread screen: its composer glides
    /// down from where that screen's composer sat.
    arrive: bool,
    /// Streaming replies revealed at a steady rate.
    pacer: pace::Pacer,
    pacing: bool,
    outline: Entity<outline::Outline>,
    /// Jumps to latest so far; each one keys a fresh settle slide.
    jumps: u32,
    scroller: Entity<MessageScrollerState>,
    /// Whether the transcript ends with the tail row (work, goal, queue).
    has_tail: bool,
    composer: Entity<TextareaState>,
    status: StreamStatus,
    error: Option<SharedString>,
    /// Slash commands from the gateway, loaded once per view.
    commands: Rc<Vec<RebornProductCommandInfo>>,
    menu_index: usize,
    /// The composer's model pill: any provider, switched per thread.
    picker: Entity<ModelPicker>,
    /// The profile's approval-mode pill after the model and thinking pills.
    approval: Entity<approval_mode::ApprovalPill>,
    /// Task ideas for an empty conversation, loaded once it is known empty.
    /// The conversation's files panel, when open.
    files: Option<FilesPanel>,
    /// The draft the `/` menu was closed for with Esc; typing reopens it.
    menu_dismissed: Option<SharedString>,
    /// Delegated tasks, queue and lineage of this thread.
    orch: orchestration::Orchestration,
    /// The computer-use screen share of this thread.
    computer: Entity<computer::ComputerShare>,
    _computer_stop: Subscription,
    /// The right pane: activity, outputs, agents, routines, project tools.
    side: side::Side,
    _stream: Option<EventStream>,
    _pump: Task<()>,
}

impl ChatView {
    /// The composer's model pill, for the ⇧⌘M shortcut.
    pub fn picker(&self) -> Entity<ModelPicker> {
        self.picker.clone()
    }
}

/// Zeron's turn: room kept under the newest user message so it sits high
/// while its answer streams. The room outlives the run, so the view stays
/// put when it ends; the next send replaces it.
struct Reserve {
    /// False only when the send failed: the room then glides away.
    held: bool,
    /// Height drawn last frame, to re-measure the row only while it glides.
    drawn: f32,
}

/// Emitted on every timeline load so the sidebar picks up server-set titles.
pub struct ThreadUpdated;

impl EventEmitter<ThreadUpdated> for ChatView {}

/// A run watched live in this view reached the end on its own (not stopped).
/// The gateway inbox only records background runs, so this is how a chat run
/// becomes a system notification.
pub struct RunEnded {
    pub thread_id: String,
    pub failed: bool,
}

impl EventEmitter<RunEnded> for ChatView {}

/// A command asked to switch conversation (`/new`).
pub struct OpenThread(pub String);

impl EventEmitter<OpenThread> for ChatView {}

/// A failed run's Logs button: show the gateway log lines for this run id.
pub struct OpenRunLogs(pub String);

impl EventEmitter<OpenRunLogs> for ChatView {}

impl ChatView {
    pub fn new(
        backend: Backend,
        channel: Option<String>,
        limits: AttachmentLimits,
        thread_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .placeholder("Do anything…")
                .submit_on_enter(true)
        });
        let subscription =
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => this.send(window, cx),
                    InputEvent::Change => {
                        this.menu_index = 0;
                        inline_files::prune(&this.composer, &mut this.staged, cx);
                        cx.notify();
                    }
                    _ => {}
                },
            );
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let outline = {
            let scroller = scroller.clone();
            cx.new(|cx| outline::Outline::new(scroller, cx))
        };
        let picker = {
            let (backend, id) = (backend.clone(), thread_id.clone());
            cx.new(|cx| ModelPicker::new(backend, Some(id), None, window, cx))
        };
        let picked = cx.subscribe(&picker, |this, _, event: &PickerEvent, cx| match event {
            PickerEvent::Changed(change) => {
                if let Some(fork) = change.fork_thread_id.clone() {
                    cx.emit(OpenThread(fork));
                } else if change.moved == "switched" {
                    // Same thread, new runtime: its live events come from
                    // elsewhere now, and the timeline gains the marker.
                    this.resubscribe(cx);
                    this.load_timeline(cx);
                } else if change.moved == "in_place" {
                    this.load_timeline(cx);
                }
            }
            PickerEvent::Failed(message) => this.set_error(message.clone(), cx),
        });

        let computer = {
            let (backend, id) = (backend.clone(), thread_id.clone());
            cx.new(|cx| computer::ComputerShare::new(backend, id, cx))
        };
        let approval = {
            let (backend, id) = (backend.clone(), thread_id.clone());
            cx.new(|cx| approval_mode::ApprovalPill::new(backend, id, cx))
        };
        // The share's Stop also stops this conversation's turn.
        let computer_stop = cx.subscribe(&computer, |this, _, _: &computer::StopRun, cx| {
            this.stop(cx)
        });
        let (stream, pump) = Self::open_stream(&backend, &thread_id, cx);
        composer.update(cx, |state, cx| state.focus(window, cx));

        let mut this = Self {
            backend,
            channel,
            limits,
            staged: Vec::new(),
            thread_id,
            transcript: Transcript::default(),
            rows: Rc::new(Vec::new()),
            items: Rc::new(Vec::new()),
            toggles: HashMap::new(),
            run_started: None,
            reserve: None,
            list_height: Rc::default(),
            turn_heights: Rc::default(),
            pin_next: false,
            prompt_rise: None,
            arrive: false,
            pacer: pace::Pacer::default(),
            pacing: false,
            outline,
            jumps: 0,
            scroller,
            has_tail: false,
            composer,
            status: StreamStatus::Connecting,
            error: None,
            commands: Rc::new(Vec::new()),
            menu_index: 0,
            picker,
            approval,
            files: None,
            menu_dismissed: None,
            orch: Default::default(),
            computer,
            _computer_stop: computer_stop,
            side: Default::default(),
            _stream: Some(stream),
            _pump: pump,
        };
        cx.on_release(move |_, _| drop((subscription, picked)))
            .detach();
        this.load_timeline(cx);
        this.load_commands(cx);
        this.refresh_orchestration(cx);
        this.side_init(cx);
        this
    }

    fn load_commands(&mut self, cx: &mut Context<Self>) {
        let job = self
            .backend
            .spawn(|c| async move { c.list_commands().await });
        let task = cx.spawn(async move |this, cx| {
            // Without the inventory every `/` line is sent as a normal message.
            if let Ok(list) = flatten(job.await) {
                let _ = this.update(cx, |this, cx| {
                    this.commands = Rc::new(list.commands);
                    cx.notify();
                });
            }
        });
        self.keep(task);
    }

    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update_in(cx, |this, window, cx| this.attach(paths, window, cx));
            }
        });
        self.keep(task);
    }

    /// Read files off the main thread, then stage the ones within limits.
    /// Attach another thread's transcript (its fork source included) as a
    /// Markdown file, so this thread can read it.
    fn attach_thread(
        &mut self,
        dragged: &DraggedThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if dragged.id == self.thread_id {
            return;
        }
        let DraggedThread { id, title } = dragged.clone();
        let limits = self.limits.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.thread_attachment(&id, &title, &limits).await });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                let staged = result.map_err(|e| e.to_string()).and_then(|file| {
                    attachments::check(&file, &this.staged, &this.limits).map(|()| file)
                });
                match staged {
                    Ok(file) => {
                        this.place(vec![file], window, cx);
                        this.error = None;
                    }
                    Err(problem) => this.error = Some(problem.into()),
                }
                cx.notify();
            });
        });
        self.keep(task);
    }

    /// Stage `files` and put their cards at the composer's cursor.
    pub(crate) fn place(
        &mut self,
        files: Vec<StagedFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let from = self.staged.len();
        self.staged.extend(files);
        inline_files::insert(
            &self.composer,
            &self.staged[from..],
            &self.staged,
            window,
            cx,
        );
    }

    /// Files or a picture pasted into the composer.
    fn paste(&mut self, pasted: inline_files::Pasted, window: &mut Window, cx: &mut Context<Self>) {
        match pasted {
            inline_files::Pasted::Paths(paths) => self.attach(paths, window, cx),
            inline_files::Pasted::Image { bytes, mime } => {
                match inline_files::pasted_image(bytes, mime, &self.staged, &self.limits) {
                    Ok(file) => self.place(vec![file], window, cx),
                    Err(problem) => self.error = Some(problem.into()),
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn attach(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let limits = self.limits.clone();
        let job = self.backend.spawn(move |_| async move {
            let mut out = Vec::new();
            for path in paths {
                out.push(attachments::stage(&path, &limits).await);
            }
            Ok::<_, ClientError>(out)
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let Ok(results) = flatten(job.await) else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let mut problems = Vec::new();
                let mut files = Vec::new();
                for result in results {
                    // Path files have no limit; only uploads are checked.
                    match result.and_then(|file| {
                        attachments::check(&file, &this.staged, &this.limits).map(|()| file)
                    }) {
                        Ok(file) => files.push(file),
                        Err(problem) => problems.push(problem),
                    }
                }
                this.place(files, window, cx);
                this.error = (!problems.is_empty()).then(|| problems.join(" ").into());
                cx.notify();
            });
        });
        self.keep(task);
    }

    /// Save an attachment to a temp folder and open it in its default app.
    fn open_attachment(&mut self, chip: AttachmentChip, cx: &mut Context<Self>) {
        let Some(message_id) = chip.message_id.clone() else {
            return;
        };
        let thread_id = self.thread_id.clone();
        let dir = PathBuf::from(&message_id).join(&chip.id);
        let id = chip.id.clone();
        self.open_fetched(dir, &chip.filename, cx, move |c| async move {
            c.attachment_bytes(&thread_id, &message_id, &id).await
        });
    }

    fn browse(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        self.files = Some(FilesPanel {
            path: path.clone(),
            entries: None,
            error: None,
        });
        cx.notify();
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.thread_files(&thread_id, path.as_deref()).await });
        let task = cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update(cx, |this, cx| {
                // Ignore answers for a panel that was closed meanwhile.
                if let Some(panel) = this.files.as_mut() {
                    match result {
                        Ok(list) => {
                            // Dot-files are the gateway's own bookkeeping.
                            let mut entries: Vec<_> = list
                                .entries
                                .into_iter()
                                .filter(|e| !e.name.starts_with('.'))
                                .collect();
                            entries.sort_by_key(|e| {
                                (
                                    !matches!(e.kind, ProjectFsEntryKind::Directory),
                                    e.name.clone(),
                                )
                            });
                            panel.entries = Some(entries);
                        }
                        Err(e) => panel.error = Some(e.to_string().into()),
                    }
                    cx.notify();
                }
            });
        });
        self.keep(task);
    }

    /// Fetch bytes, write them under the app's temp folder and hand the file
    /// to macOS, which opens it in its default app.
    fn open_fetched<Fut>(
        &mut self,
        subdir: PathBuf,
        name: &str,
        cx: &mut Context<Self>,
        fetch: impl FnOnce(ironclaw_gpui_client::Client) -> Fut,
    ) where
        Fut: std::future::Future<Output = Result<Vec<u8>, ClientError>> + Send + 'static,
    {
        // The name comes from the gateway; keep only its last component.
        let name = std::path::Path::new(name)
            .file_name()
            .map(|n| n.to_owned())
            .unwrap_or_else(|| "file".into());
        let dir = std::env::temp_dir().join("numoh").join(subdir);
        let job = self.backend.spawn(move |c| {
            let fetched = fetch(c);
            async move {
                let bytes = fetched.await?;
                let path = dir.join(name);
                let saved = async {
                    tokio::fs::create_dir_all(&dir).await?;
                    tokio::fs::write(&path, bytes).await
                };
                saved
                    .await
                    .map_err(|e| ClientError::Decode(format!("Couldn't save the file: {e}")))?;
                Ok::<_, ClientError>(path)
            }
        });
        let task = cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update(cx, |this, cx| match result {
                Ok(path) => cx.open_with_system(&path),
                Err(e) => this.set_error(e.to_string(), cx),
            });
        });
        self.keep(task);
    }

    /// Menu entries for the current draft, empty when the menu is closed.
    fn menu(&self, cx: &App) -> Vec<RebornProductCommandInfo> {
        let draft = self.composer.read(cx).value();
        if self.menu_dismissed.as_ref() == Some(&draft) {
            return Vec::new();
        }
        menu_matches(&draft, &self.commands)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Arrow keys, Tab, Enter and Esc drive the menu while it is open.
    fn on_composer_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.steer_key(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        // ⌘↵ approves the newest open approval, ⇧⌘↵ always allows it.
        let keys = &event.keystroke;
        if keys.modifiers.platform
            && keys.key == "enter"
            && let Some((run, gate, allow_always)) = self.open_approval()
        {
            let always = keys.modifiers.shift && allow_always;
            self.resolve_gate(run, gate, GateDecision::Approve { always }, cx);
            cx.stop_propagation();
            return;
        }
        let items = self.menu(cx);
        // ↑ in an empty composer brings back the last message to edit and
        // resend (MonoCode's edit-last-turn recall, without the rewind).
        if items.is_empty()
            && keys.key == "up"
            && !keys.modifiers.modified()
            && self.composer.read(cx).value().is_empty()
            && let Some(last) = self.rows.iter().rev().find_map(|r| match r {
                Row::User {
                    text, attachments, ..
                } => Some(transcript::readable(text, attachments)),
                _ => None,
            })
        {
            self.composer
                .update(cx, |state, cx| state.set_value(last, window, cx));
            cx.stop_propagation();
            return;
        }
        if items.is_empty() || event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "up" => self.menu_index = (self.menu_index + items.len() - 1) % items.len(),
            "down" => self.menu_index = (self.menu_index + 1) % items.len(),
            "tab" | "enter" => {
                let pick = items[self.menu_index.min(items.len() - 1)].clone();
                self.pick_command(&pick, window, cx);
            }
            "escape" => self.menu_dismissed = Some(self.composer.read(cx).value()),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// A command without arguments runs at once; others get `/name ` to finish.
    fn pick_command(
        &mut self,
        command: &RebornProductCommandInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if takes_arguments(command) {
            let text = format!("/{} ", command.name);
            let end = text.len();
            self.composer.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                state.set_cursor_position(Position::new(0, end as u32), window, cx);
            });
        } else {
            self.composer.update(cx, |state, cx| {
                state.set_value(format!("/{}", command.name), window, cx)
            });
            self.send(window, cx);
        }
    }

    fn run_command(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |state, cx| state.clean(window, cx));
        self.error = None;
        let thread_id = self.thread_id.clone();
        let line = text.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.execute_command(&thread_id, &line).await });
        let task = cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update(cx, |this, cx| {
                let effect = result.as_ref().ok().and_then(|o| o.effect.clone());
                this.transcript
                    .push_command(&text, result.map_err(|e| e.to_string()));
                this.refresh_rows(cx);
                this.scroller
                    .update(cx, |state, cx| state.scroll_to_end(cx));
                if let Some(CommandEffect::OpenThread { thread_id }) = effect {
                    cx.emit(OpenThread(thread_id));
                }
            });
        });
        self.keep(task);
    }

    /// The thread's live event stream and the task feeding it to `on_stream`.
    fn open_stream(
        backend: &Backend,
        thread_id: &str,
        cx: &mut Context<Self>,
    ) -> (EventStream, Task<()>) {
        let (stream, mut rx) = backend.subscribe(thread_id);
        let pump = cx.spawn(async move |this, cx| {
            while let Some(first) = rx.recv().await {
                // Drain whatever else is queued so a burst renders once.
                let mut batch = vec![first];
                while let Ok(next) = rx.try_recv() {
                    batch.push(next);
                }
                if this
                    .update(cx, |this, cx| this.on_stream(batch, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        (stream, pump)
    }

    /// Reopen the live stream (the thread moved to another runtime).
    fn resubscribe(&mut self, cx: &mut Context<Self>) {
        let (stream, pump) = Self::open_stream(&self.backend, &self.thread_id, cx);
        self._stream = Some(stream);
        self._pump = pump;
    }

    fn load_timeline(&mut self, cx: &mut Context<Self>) {
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.timeline(&thread_id, TIMELINE_LIMIT).await });
        let task = cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update(cx, |this, cx| match result {
                Ok(page) => {
                    cx.emit(ThreadUpdated);
                    this.transcript.set_timeline(page);
                    this.refresh_rows(cx);
                }
                Err(e) => this.set_error(e.to_string(), cx),
            });
        });
        self.keep(task);
    }

    fn on_stream(&mut self, batch: Vec<StreamMessage>, cx: &mut Context<Self>) {
        let mut changed = false;
        let mut ended = None;
        let mut reload = false;
        for message in batch {
            match message {
                StreamMessage::Frame(frame) => {
                    let applied = self.transcript.apply(&frame.event);
                    changed |= applied.changed;
                    reload |= applied.reload_timeline;
                    ended = applied.run_ended.or(ended);
                }
                StreamMessage::Status(status) => {
                    self.status = status;
                    changed = true;
                }
                StreamMessage::Undecodable { event, error } => {
                    tracing::debug!(%event, %error, "skipped an undecodable stream frame");
                }
                StreamMessage::Closed { reason } => {
                    self.set_error(format!("Live updates stopped: {reason}"), cx);
                }
            }
        }
        if reload {
            self.load_timeline(cx);
        }
        if changed {
            self.refresh_rows(cx);
        }
        // A run end or reload can move tasks and the queue; a run start only
        // needs the slow poll armed.
        if reload || ended.is_some() {
            self.refresh_orchestration(cx);
        } else {
            self.ensure_poll(cx);
        }
        if ended.is_some() {
            self.side_turn_ended(cx);
        }
        // Stopping a run is the person's own doing; no alert for that.
        if let Some(end @ (RunEnd::Completed | RunEnd::Failed)) = ended {
            if end == RunEnd::Failed {
                tracing::warn!(thread_id = %self.thread_id, "run failed");
            }
            cx.emit(RunEnded {
                thread_id: self.thread_id.clone(),
                failed: end == RunEnd::Failed,
            });
        }
    }

    fn refresh_rows(&mut self, cx: &mut Context<Self>) {
        self.rows = Rc::new(self.transcript.rows());
        cx.notify();
    }

    pub(super) fn activity_open(&self, key: &str, live: bool) -> bool {
        flow::is_open(key, live, &self.toggles)
    }

    /// Fold rows into list items and tell the virtual list exactly what
    /// moved, so only changed items are re-measured while text streams.
    fn sync_items(&mut self, live: bool, stopping: bool, cx: &mut Context<Self>) {
        if live || stopping {
            let run = self.running_run();
            let sent = self.transcript.last_prompt_sent_ms();
            self.run_started = Some(crate::run_clock::start(
                &self.thread_id,
                run.as_deref(),
                sent,
                cx,
            ));
        } else if self.run_started.take().is_some() {
            crate::run_clock::end(&self.thread_id, cx);
        }
        let rows = self.rows.clone();
        let (rows, pacing) = self.pacer.apply(&rows, Instant::now(), cx.reduce_motion());
        self.pacing = pacing;
        let new_items = flow::fold(&rows, live, stopping, self.has_tail, self.reserve.is_some());
        let old = self.items.clone();
        if *old == new_items && !self.pin_next {
            return;
        }
        let diff = list_diff(&old, &new_items);
        let pin = self.pin_next.then(|| {
            new_items
                .iter()
                .rposition(|i| matches!(i, Item::Row(Row::User { .. })))
        });
        self.pin_next = false;
        self.scroller.update(cx, |state, cx| {
            // Rows replaced in place (a live reply becoming the durable one,
            // a sent message getting its server copy) are re-measured, never
            // spliced: a splice drops the scroll anchor to the top of the
            // range, which jumped the view up when a run finished.
            if let Some((at, old_count, new_count)) = diff.splice {
                state.splice(at..at + old_count, new_count, cx);
            }
            for i in diff.remeasure {
                state.remeasure_items(i..i + 1, cx);
            }
            if let Some(Some(ix)) = pin {
                state.scroll_to_item(ix, cx);
            }
        });
        let prompts = outline_prompts(&new_items);
        self.outline.update(cx, |o, cx| o.set(prompts, cx));
        self.items = Rc::new(new_items);
    }

    /// The reserve's height this frame: while held, exactly what keeps the
    /// newest prompt at the top of the list and no more, so the reader can't
    /// scroll it out of sight. It stays after the run ends (the view keeps
    /// its place, as in Zeron); it glides to nothing only when a send failed.
    fn reserve_height(&mut self, window: &Window, cx: &mut Context<Self>) -> f32 {
        let Some(held) = self.reserve.as_ref().map(|r| r.held) else {
            return 0.;
        };
        let target = if held {
            let heights = self.turn_heights.borrow();
            let turn: f32 = self
                .items
                .iter()
                .rposition(|i| matches!(i, Item::Row(Row::User { .. })))
                .map(|s| {
                    self.items[s..]
                        .iter()
                        .filter_map(|i| heights.get(i.key()))
                        .sum()
                })
                .unwrap_or(0.);
            // Under the reserve: the last row's own bottom padding and the
            // list's 0.5 rem. The reserve runs 3 px past the viewport so the
            // prompt-at-top view never counts as "at the bottom" (GPUI allows
            // 1 px): there, tail following would switch on, the pin below
            // would snap back, and the two would fight the wheel every frame.
            let below = flow::SPACE_LG + 24. + 0.5 * f32::from(window.rem_size()) - 3.;
            (self.list_height.get() - turn - below).max(0.)
        } else {
            0.
        };
        let style = Style::of(cx);
        // Held, it tracks the turn's growth frame by frame; a lagging glide
        // would briefly leave room to scroll the prompt away.
        let h = motion::glide(
            &format!("reserve:{}", self.thread_id),
            target,
            style.enter,
            held || cx.reduce_motion(),
        );
        // A fresh thread's list starts in tail following, and its first
        // frames (before anything is measured) fit the viewport, which keeps
        // following on. Following would stick to the reserve's bottom instead
        // of the prompt, so while there is room the prompt is pinned back.
        if held
            && h > 0.5
            && self.scroller.read(cx).is_following_tail()
            && let Some(ix) = self
                .items
                .iter()
                .rposition(|i| matches!(i, Item::Row(Row::User { .. })))
        {
            self.scroller.update(cx, |s, cx| s.scroll_to_item(ix, cx));
        }
        let Some(r) = self.reserve.as_mut() else {
            return 0.;
        };
        if (h - r.drawn).abs() > 0.25 {
            r.drawn = h;
            let last = self.items.len().saturating_sub(1);
            self.scroller
                .update(cx, |state, cx| state.remeasure_items(last..last + 1, cx));
        }
        if !held && h < 0.5 {
            self.reserve = None;
            cx.notify();
        }
        h
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, files) = inline_files::outgoing(&self.composer, &self.staged, cx);
        if text.is_empty() && files.is_empty() {
            return;
        }
        if files.is_empty()
            && let Some(command) = goals::goal_command(&text)
        {
            return self.goal_command(command, window, cx);
        }
        if files.is_empty() && match_command(&text, &self.commands).is_some() {
            self.run_command(text, window, cx);
            return;
        }
        if self.running_run().is_some() {
            return self.steer(window, cx);
        }
        let Some(channel) = self.channel.clone() else {
            self.set_error("This gateway has no chat channel configured.", cx);
            return;
        };
        let draft = self.composer.read(cx).content();
        self.composer
            .update(cx, |state, cx| state.clean(window, cx));
        self.error = None;
        self.staged.clear();
        let names: Vec<String> = files.iter().map(|f| f.filename.clone()).collect();
        let pending = self.transcript.push_pending_user(&text, &names);
        self.refresh_rows(cx);
        // Zeron's turn: the message moves to the top and the answer streams
        // into the room kept below it.
        self.turn_heights.borrow_mut().clear();
        self.reserve = Some(Reserve {
            held: true,
            drawn: 0.,
        });
        self.prompt_rise = (!cx.reduce_motion()).then(Instant::now);
        self.pin_next = true;
        // A prompt sent here starts its run's clock now; a steer keeps it.
        if self.running_run().is_none() {
            crate::run_clock::end(&self.thread_id, cx);
            crate::run_clock::start(&self.thread_id, None, None, cx);
        }
        // The project's working tree as it stands now, for the turn's card.
        self.side_turn_started(cx);

        let thread_id = self.thread_id.clone();
        let body = text.clone();
        let wire = files.iter().map(StagedFile::to_wire).collect();
        let job = self
            .backend
            .spawn(move |c| async move { c.send_message(&channel, &thread_id, &body, wire).await });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(ack) => {
                    this.transcript.ack_pending_user(pending);
                    if let Some(notice) = ack.notice.filter(|n| !n.trim().is_empty()) {
                        this.error = Some(notice.into());
                    }
                    this.load_timeline(cx);
                    // A CLI-runtime turn has no stream; the poll follows it.
                    this.refresh_orchestration(cx);
                }
                Err(e) => {
                    this.transcript.drop_pending_user(pending);
                    if let Some(r) = this.reserve.as_mut() {
                        r.held = false;
                    }
                    // Put the text and its cards back so a retry needs no
                    // re-picking.
                    this.staged = files;
                    this.composer
                        .update(cx, |state, cx| state.set_value(draft, window, cx));
                    this.set_error(e.to_string(), cx);
                    this.refresh_rows(cx);
                }
            });
        });
        self.keep(task);
    }

    /// The new-thread screen's first message and its files, sent once the
    /// thread exists.
    pub fn send_text(
        &mut self,
        draft: InputContent,
        files: Vec<StagedFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.staged = files;
        self.arrive = true;
        crate::design::dock::take_off(cx.reduce_motion());
        self.composer
            .update(cx, |state, cx| state.set_value(draft, window, cx));
        self.send(window, cx);
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(run) = self.running_run() else {
            return;
        };
        // Flip to "Stopping" on the click itself; the backend confirms later.
        self.transcript.mark_stop_requested(&run);
        self.refresh_rows(cx);
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.cancel_run(&thread_id, &run).await });
        let task = cx.spawn(async move |this, cx| {
            if let Err(e) = flatten(job.await) {
                let _ = this.update(cx, |this, cx| {
                    this.transcript.clear_stop_requested();
                    this.refresh_rows(cx);
                    this.set_error(e.to_string(), cx);
                });
            }
        });
        self.keep(task);
    }

    fn retry(&mut self, run: String, cx: &mut Context<Self>) {
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.retry_run(&thread_id, &run).await });
        self.run_and_report(job, cx);
    }

    fn open_approval(&self) -> Option<(String, String, bool)> {
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

    fn resolve_gate(
        &mut self,
        run: String,
        gate_ref: String,
        decision: GateDecision,
        cx: &mut Context<Self>,
    ) {
        self.transcript.mark_gate_resolving(&gate_ref, decision);
        self.refresh_rows(cx);
        let thread_id = self.thread_id.clone();
        let gate = gate_ref.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.resolve_gate(&thread_id, &run, &gate, decision).await });
        let task = cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.transcript.clear_gate_resolving(&gate_ref);
                    this.set_error(e.to_string(), cx);
                    this.refresh_rows(cx);
                }
            });
        });
        self.keep(task);
    }

    /// Fire a mutation; the stream reports its effect, so only errors matter.
    fn run_and_report(
        &mut self,
        job: tokio::task::JoinHandle<Result<serde_json::Value, ClientError>>,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn(async move |this, cx| {
            if let Err(e) = flatten(job.await) {
                let _ = this.update(cx, |this, cx| this.set_error(e.to_string(), cx));
            }
        });
        self.keep(task);
    }

    fn set_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let message = message.into();
        tracing::warn!(thread_id = %self.thread_id, error = %message, "chat action failed");
        self.error = Some(message);
        cx.notify();
    }

    /// Request tasks hold only a weak handle, so they can outlive the view
    /// safely. Dropping a GPUI task would cancel an in-flight request.
    fn keep(&mut self, task: Task<()>) {
        task.detach();
    }
}

/// MonoCode's prompt rise, in seconds: the sent message's climb, its fade,
/// and the end of the reply rows' slide-in.
const PROMPT_RISE: f32 = 0.56;
const PROMPT_FADE: f32 = 0.48;
const PROMPT_RISE_TOTAL: f32 = 0.88;

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::Focusable as _;
        let focused = self.composer.focus_handle(cx).is_focused(window);
        let weak = cx.weak_entity();
        let menu = self.menu(cx);
        let menu_index = self.menu_index.min(menu.len().saturating_sub(1));
        let running = self.running_run().is_some();
        let stopping = self.transcript.is_stopping();
        self.has_tail = self.render_tail(cx).is_some();
        self.sync_items(running, stopping, cx);
        if self.pacing {
            crate::design::pulse::request(crate::design::pulse::Rate::Smooth, window, cx);
        }
        let reserve_h = self.reserve_height(window, cx);
        let turn_heights = self.turn_heights.clone();
        let items = self.items.clone();
        let turn_start = items
            .iter()
            .rposition(|i| matches!(i, Item::Row(Row::User { .. })))
            .filter(|_| self.reserve.is_some());
        // MonoCode's prompt rise: the sent message starts at the composer and
        // rises into place over 560 ms (fading in over 480 ms); the rows
        // after it slide up 10 px over the next 320 ms.
        let rise = self
            .prompt_rise
            .map(|born| born.elapsed().as_secs_f32())
            .filter(|t| *t < PROMPT_RISE_TOTAL);
        if rise.is_some() {
            window.request_animation_frame();
        } else {
            self.prompt_rise = None;
        }
        let travel = (self.list_height.get() - 160.).max(0.);
        let toggles = self.toggles.clone();
        let since = self.run_started;

        // The run each row belongs to (1-based), for "fork from here".
        let run_of: Vec<u32> = items
            .iter()
            .scan(0u32, |run, item| {
                if let Item::Row(Row::User { key, .. }) = item
                    && !key.starts_with(PREFIX_KEY)
                {
                    *run += 1;
                }
                Some(*run)
            })
            .collect();
        let seed = motion::seed(&self.thread_id);
        let jump_weak = weak.clone();
        let list = MessageScroller::new("transcript", self.scroller.clone(), move |ix, _, cx| {
            let item = &items[ix];
            let top = flow::top_gap(ix.checked_sub(1).map(|i| &items[i]), item);
            let last = ix + 1 == items.len();
            let body = match item {
                Item::Row(
                    row @ Row::Assistant {
                        key,
                        text,
                        streaming: false,
                    },
                ) => with_actions(
                    key,
                    text,
                    false,
                    Some(run_of[ix]).filter(|r| *r > 0 && !key.starts_with(PREFIX_KEY)),
                    &weak,
                    render_row(row, &weak, cx),
                ),
                Item::Row(
                    row @ Row::User {
                        key,
                        text,
                        pending: false,
                        ..
                    },
                ) if !text.is_empty() => {
                    with_actions(key, text, true, None, &weak, render_row(row, &weak, cx))
                }
                Item::Row(Row::Narration { text, .. }) => linked(
                    markdown(
                        SharedString::from(format!("{}-n", item.key())),
                        text.clone(),
                        false,
                    ),
                    &weak,
                )
                .into_any_element(),
                Item::Row(row) => render_row(row, &weak, cx),
                Item::Activity { key, rows, live } => {
                    let open = flow::is_open(key, *live, &toggles);
                    flow::render_activity(key, rows, *live, open, &toggles, &weak, cx)
                }
                Item::Pending { stopping } => flow::render_pending(*stopping, since, seed, cx),
                // Built by the view itself: its rows carry the view's listeners.
                Item::Tail => weak
                    .upgrade()
                    .and_then(|view| view.update(cx, |this, cx| this.render_tail(cx)))
                    .unwrap_or_else(|| div().into_any_element()),
                Item::Reserve => div().h(px(reserve_h)).into_any_element(),
            };
            let (lift, fade) = match (rise, turn_start) {
                (Some(t), Some(s)) if ix == s => (
                    travel * (1. - motion::EASE_OUT_QUINT.eval((t / PROMPT_RISE).min(1.))),
                    motion::EASE_OUT.eval((t / PROMPT_FADE).min(1.)),
                ),
                (Some(t), Some(s)) if ix > s => {
                    let r = motion::EASE_OUT_QUINT.eval(
                        ((t - PROMPT_RISE) / (PROMPT_RISE_TOTAL - PROMPT_RISE)).clamp(0., 1.),
                    );
                    (10. * (1. - r), r)
                }
                _ => (0., 1.),
            };
            // Zeron's reading type: 14 px on a 22 px line.
            h_flex()
                .relative()
                .when(lift > 0.01, |d| d.top(px(lift)))
                .when(fade < 0.999, |d| d.opacity(fade))
                .w_full()
                .justify_center()
                .px_4()
                .text_size(flow::ui(14.))
                .line_height(flow::ui(22.))
                .pt(px(top))
                .when(last, |d| d.pb(px(flow::SPACE_LG + 24.)))
                .child(div().w_full().max_w(px(content_width(cx))).child(body))
                // Rows of the newest turn report their height to size the reserve.
                .when(
                    turn_start.is_some_and(|s| ix >= s) && !matches!(item, Item::Reserve),
                    |d| {
                        let heights = turn_heights.clone();
                        let key = item.key().to_string();
                        d.child(
                            gpui_kit::canvas(
                                move |bounds, window, _| {
                                    // The scroller's row wrapper adds a 2 rem
                                    // gap under every row but the last.
                                    let gap = if last {
                                        0.
                                    } else {
                                        2. * f32::from(window.rem_size())
                                    };
                                    let h = f32::from(bounds.size.height) + gap;
                                    let old = heights.borrow_mut().insert(key, h);
                                    if old.is_none_or(|o| (o - h).abs() > 0.5) {
                                        window.request_animation_frame();
                                    }
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        )
                    },
                )
        })
        // The reserve reads as "scrolled up"; no jump arrow for empty room.
        .jump_button(self.reserve.is_none())
        .with_jump_button_renderer({
            let weak = jump_weak;
            // Name what waits below the fold, not just that there is more.
            let summary = self.tail_summary();
            move |button| {
                button
                    .when_some(summary, |b, text| b.label(text).small())
                    .on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.jumps += 1;
                            this.scroller
                                .update(cx, |state, cx| state.scroll_to_end(cx));
                            cx.notify();
                        });
                    })
            }
        })
        // The list hides its pixel offset, so it cannot glide: it lands on
        // the tail and the rows settle up into place, Zeron's 500 ms ease.
        .when(self.jumps > 0, |list| {
            let t = motion::reveal(
                &format!("jump:{}:{}", self.thread_id, self.jumps),
                motion::SCROLL_GLIDE,
                0,
                cx.reduce_motion(),
            );
            if t >= 1. {
                return list;
            }
            let mut style = gpui_kit::StyleRefinement::default();
            style = style.relative().top(px(motion::JUMP_TRAVEL * (1. - t)));
            list.with_content_style(style)
        });

        let banner = match self.status {
            StreamStatus::Reconnecting { delay_secs, .. } => {
                Some(format!("Connection lost. Reconnecting in {delay_secs}s"))
            }
            _ => None,
        };

        let main = v_flex()
            .id("chat")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .drag_over::<ExternalPaths>(|this, _, _, cx| this.bg(cx.theme().accent))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.attach(paths.paths().to_vec(), window, cx)
            }))
            .drag_over::<DraggedThread>(|this, _, _, cx| this.bg(cx.theme().accent.opacity(0.15)))
            .on_drop(cx.listener(|this, dragged: &DraggedThread, window, cx| {
                this.attach_thread(dragged, window, cx)
            }))
            .when_some(banner, |this, text| {
                this.child(
                    div()
                        .px_4()
                        .py_1()
                        .text_xs()
                        .bg(cx.theme().warning)
                        .text_color(cx.theme().warning_foreground)
                        .child(text),
                )
            })
            // Room for the floating header; rows fade under its lower half.
            .child({
                let list_height = self.list_height.clone();
                let body = div().relative().size_full().child(list.size_full()).child(
                    gpui_kit::canvas(
                        move |bounds, window, _| {
                            let h = f32::from(bounds.size.height);
                            if (list_height.replace(h) - h).abs() > 0.5 {
                                window.request_animation_frame();
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                );
                // The transcript fades in while the composer docks, on the
                // 0.20 → 0.65 slice of Zeron's 420 ms route clock.
                let body = if self.arrive {
                    motion::fade_in_after(
                        body,
                        "arrive-transcript",
                        motion::MotionSpec::new(190, motion::EASE_OUT),
                        6.,
                        84,
                    )
                    .into_any_element()
                } else {
                    body.into_any_element()
                };
                div().flex_1().min_h_0().pt(px(30.)).child(body)
            })
            .child(self.outline.clone())
            .child(self.computer.clone())
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex().justify_center().px_4().child(
                        div()
                            .w_full()
                            .max_w(px(content_width(cx)))
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(error),
                    ),
                )
            })
            .when_some(self.render_orchestration(cx), |this, panel| {
                this.child(panel)
            })
            .when(!menu.is_empty(), |this| {
                this.child(
                    h_flex().justify_center().px_4().child(
                        div()
                            .w_full()
                            .max_w(px(content_width(cx)))
                            .child(render_menu(&menu, menu_index, cx)),
                    ),
                )
            })
            .child(
                h_flex()
                    .justify_center()
                    .px_4()
                    .pb_4()
                    .pt_2()
                    .child(self.render_composer(running, focused, cx)),
            );
        let side = self.render_side(window, cx);
        h_flex()
            .size_full()
            // The pane's edge drag follows the pointer anywhere in the chat.
            .on_mouse_move(
                cx.listener(|this, e: &gpui_kit::MouseMoveEvent, window, cx| {
                    this.side_drag_move(e, window, cx)
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.side_drag_end(cx)),
            )
            .child(main)
            .children(side)
    }
}

/// Links in a reply open inside the app (`ChatView::open_link`).
fn linked(
    view: gpui_kit::component::text::TextView,
    weak: &WeakEntity<ChatView>,
) -> gpui_kit::component::text::TextView {
    let weak = weak.clone();
    view.on_link_click(move |href, event, _, cx| {
        let href = href.to_string();
        let external = event.modifiers().platform;
        let _ = weak.update(cx, |this, cx| this.open_link(&href, external, cx));
    })
}

fn render_row(row: &Row, weak: &WeakEntity<ChatView>, cx: &App) -> AnyElement {
    match row {
        Row::User {
            key,
            text,
            pending,
            attachments,
        } => {
            let parts = anchored_parts(text);
            let placed: std::collections::HashSet<usize> = parts
                .iter()
                .filter_map(|part| match part {
                    AnchoredPart::Attachment(ix) => Some(*ix),
                    AnchoredPart::Text(_) => None,
                })
                .collect();
            let body = if placed.is_empty() {
                selectable(SharedString::from(format!("{key}-text")), text.clone())
                    .into_any_element()
            } else {
                placed_body(key, &parts, attachments, weak, cx)
            };
            v_flex()
                .w_full()
                .items_end()
                .gap_1()
                .when(!text.is_empty(), |this| {
                    this.child(flow::user_pill(key, body, *pending, cx))
                })
                .child(
                    h_flex().flex_wrap().justify_end().gap_1().children(
                        attachments
                            .iter()
                            .enumerate()
                            .filter(|(ix, _)| !placed.contains(ix))
                            .map(|(ix, chip)| {
                                let card = file_chip(
                                    SharedString::from(format!("{key}-file-{ix}")),
                                    &chip.filename,
                                    sent_subtitle(chip),
                                    cx,
                                );
                                openable(card, chip, weak)
                            }),
                    ),
                )
                .into_any_element()
        }
        Row::Assistant {
            key,
            text,
            streaming,
        } => linked(
            markdown(SharedString::from(key.clone()), text.clone(), *streaming),
            weak,
        )
        .into_any_element(),
        Row::System { text, .. } if text.starts_with("Switched to ") => switch_marker(text, cx),
        Row::System { key, text } => div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(selectable(SharedString::from(key.clone()), text.clone()))
            .into_any_element(),
        Row::Thinking { key, text, .. } => activity_line(
            key,
            IconName::Bot,
            "Thinking",
            Some(text.clone().into()),
            cx,
        )
        .into_any_element(),
        Row::Narration { key, text, .. } => {
            activity_line(key, IconName::Ellipsis, text.clone(), None, cx).into_any_element()
        }
        Row::Tool {
            key,
            title,
            detail,
            status,
            ..
        } => {
            let icon = match status {
                ToolStatus::Running => IconName::LoaderCircle,
                ToolStatus::Completed => IconName::CircleCheck,
                ToolStatus::Failed => IconName::CircleX,
            };
            activity_line(key, icon, title.clone(), detail.clone().map(Into::into), cx)
                .into_any_element()
        }
        Row::Skills { key, names, .. } => activity_line(
            key,
            IconName::BookOpen,
            format!("Using skills: {}", names.join(", ")),
            None,
            cx,
        )
        .into_any_element(),
        Row::WorkSummary { key, text, .. } => {
            activity_line(key, IconName::FileText, text.clone(), None, cx).into_any_element()
        }
        Row::Gate {
            key,
            run_id,
            gate_ref,
            kind,
            headline,
            body,
            allow_always,
            state,
            reason,
        } => render_gate(
            key,
            run_id,
            gate_ref,
            *kind,
            headline,
            body,
            *allow_always,
            *state,
            reason.as_deref(),
            weak,
            cx,
        ),
        Row::RunNotice {
            key,
            run_id,
            text,
            failed,
            retryable,
        } => card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .items_start()
                    .when(*failed, |this| this.text_color(cx.theme().danger))
                    .child(
                        Icon::new(if *failed {
                            IconName::TriangleAlert
                        } else {
                            IconName::Info
                        })
                        .flex_none(),
                    )
                    .child(div().flex_1().min_w_0().child(selectable(
                        SharedString::from(format!("{key}-text")),
                        text.clone(),
                    ))),
            )
            .when(*retryable || *failed, |this| {
                let retry = (weak.clone(), run_id.clone());
                let logs = (weak.clone(), run_id.clone());
                this.child(
                    h_flex()
                        .gap_1()
                        .when(*retryable, |row| {
                            row.child(
                                Button::new(SharedString::from(format!("{key}-retry")))
                                    .label("Retry")
                                    .small()
                                    .on_click(move |_, _, cx| {
                                        let (weak, run) = &retry;
                                        let _ =
                                            weak.update(cx, |this, cx| this.retry(run.clone(), cx));
                                    }),
                            )
                        })
                        .when(*failed, |row| {
                            row.child(
                                Button::new(SharedString::from(format!("{key}-logs")))
                                    .label("Logs")
                                    .small()
                                    .ghost()
                                    .tooltip("Gateway log lines for this run")
                                    .on_click(move |_, _, cx| {
                                        let (weak, run) = &logs;
                                        let _ = weak
                                            .update(cx, |_, cx| cx.emit(OpenRunLogs(run.clone())));
                                    }),
                            )
                        }),
                )
            })
            .into_any_element(),
        Row::Command {
            key,
            input,
            title,
            fields,
            lines,
            failed,
            ..
        } => card(cx)
            .text_sm()
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(IconName::SquareTerminal).xsmall())
                    .child(selectable(
                        SharedString::from(format!("{key}-input")),
                        input.clone(),
                    )),
            )
            .when(!title.is_empty(), |this| {
                this.child(div().font_medium().child(title.clone()))
            })
            .children(fields.iter().enumerate().map(|(ix, f)| {
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .w(px(120.))
                            .flex_none()
                            .text_color(cx.theme().muted_foreground)
                            .child(f.label.clone()),
                    )
                    .child(div().flex_1().min_w_0().child(selectable(
                        SharedString::from(format!("{key}-field-{ix}")),
                        f.value.clone(),
                    )))
            }))
            .children(lines.iter().enumerate().map(|(ix, line)| {
                div()
                    .when(*failed, |this| this.text_color(cx.theme().danger))
                    .child(selectable(
                        SharedString::from(format!("{key}-line-{ix}")),
                        line.clone(),
                    ))
            }))
            .into_any_element(),
    }
}

impl ChatView {
    /// The composer card: text on top; model pill, attach and send below.
    fn render_composer(&self, running: bool, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        // Send, plus Stop beside it while a turn runs (chat/steer.rs).
        let send = self.render_send(running, cx);
        let attach = round_button("attach", ExtraIcon::Paperclip, false, "Attach files", cx)
            .tooltip(|w, cx| {
                gpui_kit::component::tooltip::Tooltip::new("Attach files").build(w, cx)
            })
            .on_click(cx.listener(|this, _, window, cx| this.pick_files(window, cx)));
        let card = composer_card(
            "composer",
            focused,
            true,
            Some(self.approval.clone().into_any_element()),
            div()
                .capture_key_down(cx.listener(Self::on_composer_key))
                .child(
                    Textarea::new(&self.composer)
                        .appearance(false)
                        .token(inline_files::token_renderer(&self.staged, cx))
                        .on_paste({
                            let weak = cx.entity().downgrade();
                            move |item, window, cx| {
                                let Some(files) = inline_files::pasted(item) else {
                                    return false;
                                };
                                let _ = weak.update(cx, |this, cx| this.paste(files, window, cx));
                                true
                            }
                        }),
                ),
            self.picker.clone(),
            h_flex().gap(px(6.)).child(attach).child(send),
            cx,
        );
        // From the new-thread screen it glides down from the hero's spot,
        // its width easing from the hero card's.
        use crate::design::dock;
        let card = match dock::width(dock::Dest::Dock, content_width(cx)) {
            Some(w) => card.w(px(w)).max_w(px(w)),
            None => card,
        };
        dock::docked(card).into_any_element()
    }
}

/// Zeron's composer pill. In a thread it docks to one 49 px line: attach,
/// the text, the model and a round send button. It opens into two rows
/// (text on top, controls below) when the text wraps, and the new-thread
/// screen always draws it open. The rim brightens with focus.
#[allow(clippy::too_many_arguments)]
pub(crate) fn composer_card(
    key: &str,
    focused: bool,
    compact: bool,
    access: Option<AnyElement>,
    input: impl IntoElement,
    picker: Entity<ModelPicker>,
    actions: impl IntoElement,
    cx: &App,
) -> gpui_kit::Div {
    let palette = Palette::of(cx);
    let f = motion::state_t(
        &format!("{key}:focus"),
        focused,
        motion::TOOL_FOLD,
        cx.reduce_motion(),
    );
    // One plate, one short shadow (T3's design pass): it sits on the
    // transcript instead of floating over it.
    let ink = |a: f32| {
        if palette.light {
            gpui_kit::hsla(0., 0., 0., a)
        } else {
            gpui_kit::hsla(0., 0., 1., a)
        }
    };
    let fill = if palette.light {
        gpui_kit::hsla(0., 0., 1., 0.8)
    } else {
        motion::mix(palette.surface_card, palette.bg, 0.25).opacity(0.86)
    };
    let rim = motion::mix(ink(0.07), ink(0.14), f);
    let drop = |a: f32, y: f32, blur: f32, spread: f32| gpui_kit::BoxShadow {
        color: gpui_kit::hsla(0., 0., 0., a),
        offset: gpui_kit::point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(spread),
        inset: false,
    };
    let shadows = if palette.light {
        vec![drop(0.06, 4., 14., -4.)]
    } else {
        vec![drop(0.3, 4., 16., -4.)]
    };
    let shell = div()
        .w_full()
        .max_w(px(content_width(cx)))
        .rounded(px(16.))
        .bg(fill)
        .border_1()
        .border_color(rim)
        .shadow(shadows)
        .text_size(flow::ui(14.))
        .line_height(px(22.75));
    // The text on top; below it the model, its thinking level and the
    // access pill on the left, attach and send on the right.
    let input = div().w_full().min_w_0().child(input);
    shell.child(
        v_flex()
            .px(px(if compact { 14. } else { 18. }))
            .pt(px(if compact { 12. } else { 16. }))
            .pb(px(8.))
            .gap(px(6.))
            .child(input.min_h(px(if compact { 24. } else { 44. })))
            .child(
                h_flex()
                    .h(px(32.))
                    .items_center()
                    .gap(px(4.))
                    .child(picker)
                    .children(access)
                    .child(div().flex_1())
                    .child(actions),
            ),
    )
}

/// Zeron's round controls: a 28 px circle, the send one filled with the
/// text color and an up arrow in the background color.
pub(crate) fn round_button(
    id: &'static str,
    icon: impl Into<Icon>,
    filled: bool,
    label: &'static str,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let p = *Palette::of(cx);
    let hover_key = SharedString::from(format!("round-{id}"));
    let t = motion::hover_t(&hover_key);
    let wash = if p.light {
        gpui_kit::hsla(0., 0., 0., 0.08)
    } else {
        gpui_kit::hsla(0., 0., 1., 0.10)
    };
    // Send and stop: a flat accent tile with soft corners. Idle, it sinks
    // into a neutral wash so an empty composer has no bright spot.
    if id.ends_with("send") || id == "stop" {
        let (fill, ink) = if filled {
            (
                crate::design::lift(p.accent, 0.05 * t),
                crate::design::on_accent(p),
            )
        } else {
            (wash, p.text_faint)
        };
        return div()
            .id(id)
            .role(Role::Button)
            .aria_label(label)
            .size(px(30.))
            .flex_none()
            .rounded(px(8.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_hover(motion::hover_listener(
                hover_key,
                motion::MotionSpec::new(150, motion::EASE),
            ))
            .bg(fill)
            .child(Icon::new(icon).size(px(15.)).text_color(ink));
    }
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .size(px(28.))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_hover(motion::hover_listener(
            hover_key,
            motion::MotionSpec::new(150, motion::EASE),
        ))
        .when(filled, |d| d.bg(p.text).opacity(1. - 0.15 * t))
        .when(!filled && id == "send", |d| d.bg(wash))
        .when(!filled && id != "send", |d| {
            d.bg(wash.opacity(wash.a * t)).text_color(p.text_muted)
        })
        .child(
            Icon::new(icon)
                .size(px(14.))
                .text_color(if filled { p.bg } else { p.text_muted }),
        )
}

/// A sidebar thread being dragged. Dropped on a chat, it attaches that
/// thread's transcript to the message being written.
#[derive(Clone)]
pub struct DraggedThread {
    pub id: String,
    pub title: String,
}

impl Render for DraggedThread {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        h_flex()
            .max_w(px(260.))
            .px(px(10.))
            .py(px(6.))
            .gap(px(6.))
            .items_center()
            .rounded(px(8.))
            .bg(p.surface_overlay)
            .border_1()
            .border_color(p.border_strong)
            .shadow_md()
            .text_sm()
            .text_color(p.text)
            .child(
                Icon::new(IconName::File)
                    .size(px(14.))
                    .text_color(p.text_muted),
            )
            .child(div().min_w_0().truncate().child(self.title.clone()))
    }
}

/// A message with actions that show on hover, as in Zeron: copy, and on
/// answers "fork from here" (a new thread holding the history up to this
/// run). `mine`: a sent message, so the bar sits on the right.
fn with_actions(
    key: &str,
    text: &str,
    mine: bool,
    fork_run: Option<u32>,
    weak: &WeakEntity<ChatView>,
    body: AnyElement,
) -> AnyElement {
    let group = SharedString::from(format!("{key}-group"));
    let hover_key = SharedString::from(format!("copy-hover:{key}"));
    let h = motion::hover_t(&hover_key);
    let copied = copied_recently(key);
    let copy = text.to_string();
    let copy_key = key.to_string();
    let weak = weak.clone();
    let menu = {
        let (text, key, weak) = (copy.clone(), copy_key.clone(), weak.clone());
        // Right-click: the selection first when there is one, then the
        // hover bar's actions.
        move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
            let selected = gpui_kit::base::TextSelection::selected_text(window, cx);
            let (text, key, weak) = (text.clone(), key.clone(), weak.clone());
            menu.when(!selected.is_empty(), |m| {
                m.item(
                    PopupMenuItem::new("Copy selection")
                        .icon(Icon::new(IconName::Copy))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(selected.clone()))
                        }),
                )
            })
            .item(
                PopupMenuItem::new("Copy message")
                    .icon(Icon::new(IconName::Copy))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                        mark_copied(&key, cx);
                    }),
            )
            .when_some(fork_run, |m, run| {
                m.item(
                    PopupMenuItem::new("Fork from here")
                        .icon(Icon::new(ExtraIcon::GitFork))
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| this.fork(Some(run), cx));
                        }),
                )
            })
        }
    };
    v_flex()
        .id(group.clone())
        .gap_1()
        .on_hover(motion::hover_listener(hover_key, motion::FADE_QUICK))
        .child(body)
        .child(
            h_flex()
                .h(px(22.))
                .gap_0p5()
                .when(mine, |d| d.justify_end())
                .opacity(if copied { 1. } else { h })
                .child(
                    kit::button(SharedString::from(format!("{key}-copy")))
                        .icon(if copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        })
                        .ghost()
                        .xsmall()
                        .tooltip(if copied { "Copied" } else { "Copy" })
                        .accessibility_label("Copy message")
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            mark_copied(&copy_key, cx);
                        }),
                )
                .when_some(fork_run, |d, run| {
                    d.child(
                        kit::button(SharedString::from(format!("{key}-fork")))
                            .icon(ExtraIcon::GitFork)
                            .ghost()
                            .xsmall()
                            .tooltip("Fork from here")
                            .accessibility_label("Fork from this message")
                            .on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |this, cx| this.fork(Some(run), cx));
                            }),
                    )
                }),
        )
        .context_menu(menu)
        .into_any_element()
}

/// How the virtual list moves from `old` to `new`.
#[derive(Debug, Default, PartialEq)]
struct ListDiff {
    /// `(at, removed, inserted)`, applied before `remeasure`.
    splice: Option<(usize, usize, usize)>,
    /// Indices (in `new`) whose content changed in place.
    remeasure: Vec<usize>,
}

/// Unchanged keys at both ends stay; the middle is re-measured position by
/// position and only the difference in length is spliced, at its end.
fn list_diff(old: &[Item], new: &[Item]) -> ListDiff {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(a, b)| a.key() == b.key())
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a.key() == b.key())
        .count();
    let (old_mid, new_mid) = (old.len() - prefix - suffix, new.len() - prefix - suffix);
    let shared = old_mid.min(new_mid);
    let splice =
        (old_mid != new_mid).then(|| (prefix + shared, old_mid - shared, new_mid - shared));
    let changed = |i: usize, j: usize| old[i] != new[j];
    let mut remeasure: Vec<usize> = (0..prefix).filter(|&i| changed(i, i)).collect();
    remeasure.extend((prefix..prefix + shared).filter(|&i| changed(i, i)));
    let (old_tail, new_tail) = (old.len() - suffix, new.len() - suffix);
    remeasure.extend(
        (0..suffix)
            .filter(|&k| changed(old_tail + k, new_tail + k))
            .map(|k| new_tail + k),
    );
    ListDiff { splice, remeasure }
}

/// Each sent message and the start of the reply under it.
fn outline_prompts(items: &[Item]) -> Vec<outline::Prompt> {
    let mut out: Vec<outline::Prompt> = Vec::new();
    for (ix, item) in items.iter().enumerate() {
        match item {
            Item::Row(Row::User {
                text, attachments, ..
            }) => out.push(outline::Prompt {
                item: ix,
                label: outline::first_line(&transcript::readable(text, attachments), 160),
                reply: None,
            }),
            Item::Row(Row::Assistant { text, .. }) => {
                if let Some(last) = out.last_mut()
                    && last.reply.is_none()
                    && !text.trim().is_empty()
                {
                    last.reply = Some(outline::first_line(text, 120));
                }
            }
            _ => {}
        }
    }
    out
}

thread_local! {
    static COPIED: std::cell::RefCell<Option<(String, Instant)>> = const { std::cell::RefCell::new(None) };
}

/// How long the copy button shows its check.
const COPIED_FOR: Duration = Duration::from_millis(1400);

fn copied_recently(key: &str) -> bool {
    COPIED.with(|c| {
        c.borrow()
            .as_ref()
            .is_some_and(|(k, at)| k == key && at.elapsed() < COPIED_FOR)
    })
}

/// Show the check, then one redraw when it should turn back: a timer, not
/// a frame loop.
fn mark_copied(key: &str, cx: &mut App) {
    COPIED.with(|c| *c.borrow_mut() = Some((key.to_string(), Instant::now())));
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(COPIED_FOR).await;
        cx.update(|cx| cx.refresh_windows());
    })
    .detach();
}

/// An attachment card, the one look for staged and sent files: an icon tile,
/// the name, and a muted second line (folder, size or type).
pub(crate) fn attachment_card(
    id: SharedString,
    icon: AnyElement,
    title: &str,
    subtitle: Option<String>,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let p = Palette::of(cx);
    h_flex()
        .id(id)
        .gap_2()
        .p_1p5()
        .pr_2()
        .max_w(px(300.))
        .rounded(px(8.))
        .border_1()
        .border_color(p.border_strong)
        .bg(p.hover)
        .child(
            div()
                .size(px(32.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .border_1()
                .border_color(p.border_strong)
                .child(icon),
        )
        .child(
            v_flex()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .font_medium()
                        .truncate()
                        .child(title.to_string()),
                )
                .when_some(subtitle, |this, line| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(line),
                    )
                }),
        )
}

/// A sent message whose files sit between its words: each word and each
/// card is one item of a wrapping row, so the cards flow with the text.
fn placed_body(
    key: &str,
    parts: &[AnchoredPart<'_>],
    chips: &[AttachmentChip],
    weak: &WeakEntity<ChatView>,
    cx: &App,
) -> AnyElement {
    let mut items: Vec<AnyElement> = Vec::new();
    for part in parts {
        match part {
            AnchoredPart::Text(run) => {
                for (n, line) in run.split('\n').enumerate() {
                    if n > 0 {
                        // A full-width item ends the row: the line break.
                        items.push(div().w_full().into_any_element());
                    }
                    items.extend(
                        line.split(' ')
                            .filter(|word| !word.is_empty())
                            .map(|word| div().child(word.to_string()).into_any_element()),
                    );
                }
            }
            AnchoredPart::Attachment(ix) => {
                let Some(chip) = chips.get(*ix) else {
                    continue;
                };
                let card = inline_files::inline_card(
                    SharedString::from(format!("{key}-inline-{ix}-{}", items.len())),
                    None,
                    &chip.filename,
                    px(22.),
                    false,
                    cx,
                );
                items.push(openable(card, chip, weak).into_any_element());
            }
        }
    }
    h_flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(4.))
        .children(items)
        .into_any_element()
}

/// A sent file opens on click: the backend's copy from disk, or the upload
/// fetched from the gateway.
fn openable(
    card: gpui_kit::Stateful<gpui_kit::Div>,
    chip: &AttachmentChip,
    weak: &WeakEntity<ChatView>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let open = chip.clone();
    let weak = weak.clone();
    card.when_some(chip.local_path.clone(), |this, path| {
        this.cursor_pointer().on_click(move |_, _, cx| {
            cx.open_with_system(std::path::Path::new(&path));
        })
    })
    .when(chip.message_id.is_some(), |this| {
        this.cursor_pointer().on_click(move |_, _, cx| {
            let chip = open.clone();
            let _ = weak.update(cx, |this, cx| this.open_attachment(chip, cx));
        })
    })
}

/// A file's card with the plain file icon.
pub(crate) fn file_chip(
    id: SharedString,
    name: &str,
    subtitle: Option<String>,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let icon = Icon::new(IconName::File).small().into_any_element();
    attachment_card(id, icon, name, subtitle, cx)
}

/// A sent file's second line: its size, else its type from the extension.
fn sent_subtitle(chip: &AttachmentChip) -> Option<String> {
    chip.size.map(format_bytes).or_else(|| {
        std::path::Path::new(&chip.filename)
            .extension()
            .map(|ext| ext.to_string_lossy().to_uppercase())
    })
}

/// The `/` menu above the composer: name, usage and description per command.
fn render_menu(
    items: &[RebornProductCommandInfo],
    selected: usize,
    cx: &mut Context<ChatView>,
) -> impl IntoElement {
    let pop = kit::popover("command-menu-pop", cx);
    pop(v_flex().w_full().child(
        div()
            .w_full()
            .max_h(px(280.))
            .id("command-menu")
            .overflow_y_scroll()
            .p_1()
            .children(items.iter().enumerate().map(|(ix, command)| {
                let pick = command.clone();
                kit::row(
                    SharedString::from(format!("cmd-{}", command.name)),
                    ix == selected,
                    cx,
                )
                .role(Role::Button)
                .aria_label(command.usage.clone())
                .gap_3()
                .child(div().font_medium().flex_none().child(command.usage.clone()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(command.description.clone()),
                )
                .on_click(
                    cx.listener(move |this, _, window, cx| this.pick_command(&pick, window, cx)),
                )
            })),
    ))
}

#[allow(clippy::too_many_arguments)]
fn render_gate(
    key: &str,
    run_id: &str,
    gate_ref: &str,
    kind: ProductGateKind,
    headline: &str,
    body: &Option<String>,
    allow_always: bool,
    state: GateState,
    reason: Option<&str>,
    weak: &WeakEntity<ChatView>,
    cx: &App,
) -> AnyElement {
    let decide = |label: &'static str, suffix: &str, decision: GateDecision| {
        let weak = weak.clone();
        let (run, gate) = (run_id.to_string(), gate_ref.to_string());
        Button::new(SharedString::from(format!("{key}-{suffix}")))
            .label(label)
            .small()
            .on_click(move |_, _, cx| {
                let (run, gate) = (run.clone(), gate.clone());
                let _ = weak.update(cx, |this, cx| this.resolve_gate(run, gate, decision, cx));
            })
    };

    // A settled gate folds into its activity group (see `flow`); this
    // only draws gates that still ask or are being answered.
    let actions = match (state, kind) {
        (GateState::Closed(_), _) => None,
        (GateState::Resolving(decision), _) => Some(
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(Spinner::new().xsmall())
                .child(match decision {
                    GateDecision::Decline => "Declined, waiting for IronClaw to confirm",
                    GateDecision::Approve { .. } => "Approved, waiting for IronClaw to confirm",
                })
                .into_any_element(),
        ),
        // Sign-in flows run in the browser; the gate clears when they finish.
        (GateState::Open, ProductGateKind::Auth) => Some(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Finish this sign-in from the browser (sidebar, Open in browser).")
                .into_any_element(),
        ),
        (GateState::Open, _) => Some(
            h_flex()
                .gap_2()
                .child(
                    decide(
                        "Approve  ⌘↵",
                        "approve",
                        GateDecision::Approve { always: false },
                    )
                    .primary(),
                )
                .when(allow_always, |this| {
                    this.child(decide(
                        "Always allow  ⇧⌘↵",
                        "always",
                        GateDecision::Approve { always: true },
                    ))
                })
                .child(decide("Decline", "decline", GateDecision::Decline).ghost())
                .into_any_element(),
        ),
    };

    // Zeron's chip language: a hairline card with an icon tile, a quiet
    // label and the request. An open approval is the one thing waiting on
    // the person, so its rim warms toward the warning color.
    let open = matches!(state, GateState::Open);
    let p = *Palette::of(cx);
    let s = *Style::of(cx);
    let ink = |a: f32| {
        if p.light {
            gpui_kit::hsla(0., 0., 0., a)
        } else {
            gpui_kit::hsla(0., 0., 1., a)
        }
    };
    let label = match kind {
        ProductGateKind::Auth => "Sign-in",
        ProductGateKind::Approval => "Approval",
        _ => "Question",
    };
    let card = v_flex()
        .w_full()
        .gap_2()
        .p(px(10.))
        .rounded(px(12.))
        .border_1()
        .border_color(if open {
            p.warning.opacity(0.45)
        } else {
            ink(0.08)
        })
        .bg(ink(0.045))
        .text_size(flow::ui(12.))
        .child(
            h_flex()
                .gap(px(8.))
                .items_center()
                .child(
                    div()
                        .flex_none()
                        .size(px(20.))
                        .rounded(px(6.))
                        .bg(ink(0.09))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .size(px(12.))
                                .text_color(if open { p.warning } else { p.text_muted }),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(p.text_muted)
                        .child(label),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_color(p.text.opacity(0.9))
                        .child(headline.to_string()),
                ),
        )
        .when_some(body.clone(), |this, body| {
            this.child(
                div()
                    .pl(px(28.))
                    .max_h(px(160.))
                    .overflow_hidden()
                    .font_family(crate::design::FONT_MONO)
                    .text_size(flow::ui(11.5))
                    .line_height(px(17.))
                    .text_color(p.text_muted)
                    .whitespace_normal()
                    .child(body),
            )
        })
        .children(approval_reason::gate_line(reason, &p))
        .children(actions.map(|a| div().pl(px(28.)).child(a)));
    motion::pop_in(card, SharedString::from(format!("{key}-gate")), s.pop, 8.).into_any_element()
}

/// The inline marker where a thread switched provider:
/// "Switched to claude-opus-5-5 · Claude CLI", with the model id prettified.
fn switch_marker(text: &str, cx: &App) -> AnyElement {
    let rest = text.trim_start_matches("Switched to ");
    let label = match rest.split_once(" · ") {
        Some((model, provider)) => {
            format!(
                "Switched to {} · {provider}",
                model_picker::pretty_model(model)
            )
        }
        None => text.to_string(),
    };
    let rule = || div().h_px().flex_1().bg(cx.theme().border);
    h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(rule())
        .child(label)
        .child(rule())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(key: &str) -> Item {
        Item::Row(Row::User {
            key: key.into(),
            text: "hi".into(),
            pending: false,
            attachments: Vec::new(),
        })
    }

    fn reply(key: &str, streaming: bool) -> Item {
        Item::Row(Row::Assistant {
            key: key.into(),
            text: "answer".into(),
            streaming,
        })
    }

    #[test]
    fn a_finished_run_remeasures_in_place_instead_of_splicing_the_anchor() {
        let old = vec![
            user("msg:1"),
            reply("msg:2", false),
            user("pending:3"),
            reply("text:live", true),
            Item::Pending { stopping: false },
            Item::Reserve,
        ];
        let new = vec![
            user("msg:1"),
            reply("msg:2", false),
            user("msg:3"),
            reply("msg:4", false),
            Item::Reserve,
        ];
        let diff = list_diff(&old, &new);
        // Only the pending row is dropped, after the two replaced rows.
        assert_eq!(diff.splice, Some((4, 1, 0)));
        assert_eq!(diff.remeasure, vec![2, 3]);
    }

    #[test]
    fn a_row_inserted_before_the_tail_splices_there() {
        let old = vec![
            user("msg:1"),
            Item::Pending { stopping: false },
            Item::Reserve,
        ];
        let new = vec![
            user("msg:1"),
            reply("text:live", true),
            Item::Pending { stopping: false },
            Item::Reserve,
        ];
        let diff = list_diff(&old, &new);
        assert_eq!(diff.splice, Some((1, 0, 1)));
        assert!(diff.remeasure.is_empty());
    }
}
