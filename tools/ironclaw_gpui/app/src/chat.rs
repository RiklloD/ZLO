//! One open thread: transcript, live stream, composer, run controls.

use std::path::PathBuf;
use std::rc::Rc;

use ironclaw_gpui_client::attachments::{self, StagedFile, format_bytes};
use ironclaw_gpui_client::commands::{match_command, menu_matches, takes_arguments};
use ironclaw_gpui_client::contract::ProductGateKind;
use ironclaw_gpui_client::operator_llm::UserModelCatalog;
use ironclaw_gpui_client::product_wire::RebornProductCommandInfo;
use ironclaw_gpui_client::transcript::{
    AttachmentChip, GateState, Row, RunEnd, ToolStatus, Transcript,
};
use ironclaw_gpui_client::wire::{AttachmentLimits, CommandEffect};
use ironclaw_gpui_client::workspace_views::{ProjectFsEntry, ProjectFsEntryKind};
use ironclaw_gpui_client::{
    Backend, ClientError, EventStream, GateDecision, StreamMessage, StreamStatus,
};

use crate::design::styles::{Material, Style};
use crate::design::{Palette, kit, motion};
use crate::jobs::flatten;
use crate::ui::*;

mod goals;
mod orchestration;

const TIMELINE_LIMIT: u32 = 200;
const CONTENT_MAX_WIDTH: f32 = 780.;

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
    scroller: Entity<MessageScrollerState>,
    composer: Entity<TextareaState>,
    status: StreamStatus,
    error: Option<SharedString>,
    /// Slash commands from the gateway, loaded once per view.
    commands: Rc<Vec<RebornProductCommandInfo>>,
    menu_index: usize,
    /// Quick model switcher: the switchable models and the person's pick.
    model_choice: Option<(UserModelCatalog, Option<String>)>,
    model_menu_open: bool,
    /// Task ideas for an empty conversation, loaded once it is known empty.
    /// The conversation's files panel, when open.
    files: Option<FilesPanel>,
    /// The draft the `/` menu was closed for with Esc; typing reopens it.
    menu_dismissed: Option<SharedString>,
    /// Delegated tasks, queue and lineage of this thread.
    orch: orchestration::Orchestration,
    _stream: Option<EventStream>,
    _pump: Task<()>,
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
                .placeholder("Message IronClaw")
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
                        cx.notify();
                    }
                    _ => {}
                },
            );
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));

        let (stream, mut rx) = backend.subscribe(&thread_id);
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
        composer.update(cx, |state, cx| state.focus(window, cx));

        let mut this = Self {
            backend,
            channel,
            limits,
            staged: Vec::new(),
            thread_id,
            transcript: Transcript::default(),
            rows: Rc::new(Vec::new()),
            scroller,
            composer,
            status: StreamStatus::Connecting,
            error: None,
            commands: Rc::new(Vec::new()),
            menu_index: 0,
            model_choice: None,
            model_menu_open: false,
            files: None,
            menu_dismissed: None,
            orch: Default::default(),
            _stream: Some(stream),
            _pump: pump,
        };
        cx.on_release(move |_, _| drop(subscription)).detach();
        this.load_timeline(cx);
        this.load_commands(cx);
        this.load_model_choice(cx);
        this.refresh_orchestration(cx);
        this
    }

    fn load_model_choice(&mut self, cx: &mut Context<Self>) {
        let job = self.backend.spawn(|c| async move {
            let catalog = c.model_catalog().await?;
            let preference = c.model_preference().await?;
            Ok::<_, ClientError>((catalog, preference.model))
        });
        let task = cx.spawn(async move |this, cx| {
            // silent-ok: without the routes the switcher stays hidden.
            if let Ok(choice) = flatten(job.await) {
                let _ = this.update(cx, |this, cx| {
                    this.model_choice = Some(choice);
                    cx.notify();
                });
            }
        });
        self.keep(task);
    }

    /// `None` follows the workspace default.
    fn choose_model(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        self.model_menu_open = false;
        let job = self
            .backend
            .spawn(move |c| async move { c.set_model_preference(model).await });
        let task = cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            let _ = this.update(cx, |this, cx| match result {
                Ok(preference) => {
                    if let Some((_, pick)) = this.model_choice.as_mut() {
                        *pick = preference.model;
                    }
                    cx.notify();
                }
                Err(e) => this.set_error(e.to_string(), cx),
            });
        });
        self.keep(task);
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
                let _ = this.update(cx, |this, cx| this.attach(paths, cx));
            }
        });
        self.keep(task);
    }

    /// Read files off the main thread, then stage the ones within limits.
    fn attach(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let limits = self.limits.clone();
        let job = self.backend.spawn(move |_| async move {
            let mut out = Vec::new();
            for path in paths {
                out.push(attachments::stage(&path, &limits).await);
            }
            Ok::<_, ClientError>(out)
        });
        let task = cx.spawn(async move |this, cx| {
            let Ok(results) = flatten(job.await) else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let mut problems = Vec::new();
                for result in results {
                    match result.and_then(|file| {
                        attachments::check(&file, &this.staged, &this.limits).map(|()| file)
                    }) {
                        Ok(file) => this.staged.push(file),
                        Err(problem) => problems.push(problem),
                    }
                }
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

    fn toggle_files(&mut self, cx: &mut Context<Self>) {
        if self.files.take().is_none() {
            self.browse(None, cx);
        }
        cx.notify();
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

    fn render_files(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.files.as_ref()?;
        let theme = cx.theme();
        let parent = panel.path.as_deref().map(|p| {
            let trimmed = p.trim_end_matches('/');
            // Back to the root once only the top folder is left.
            trimmed
                .rsplit_once('/')
                .map(|(head, _)| head.to_string())
                .filter(|head| head.matches('/').count() >= 2)
        });
        let title = panel
            .path
            .as_deref()
            .and_then(|p| p.trim_end_matches('/').rsplit('/').next())
            .unwrap_or("Files")
            .to_string();
        let body = match (&panel.entries, &panel.error) {
            (_, Some(e)) => div()
                .text_sm()
                .text_color(theme.danger)
                .whitespace_normal()
                .child(e.clone())
                .into_any_element(),
            (None, None) => h_flex()
                .justify_center()
                .py_4()
                .child(Spinner::new())
                .into_any_element(),
            (Some(entries), None) if entries.is_empty() => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No files yet. Files IronClaw writes in this conversation show up here.")
                .whitespace_normal()
                .into_any_element(),
            (Some(entries), None) => v_flex()
                .children(entries.iter().enumerate().map(|(ix, entry)| {
                    let dir = matches!(entry.kind, ProjectFsEntryKind::Directory);
                    let path = entry.path.clone();
                    h_flex()
                        .id(("file-entry", ix))
                        .role(Role::Button)
                        .aria_label(entry.name.clone())
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(theme.radius)
                        .text_sm()
                        .cursor_pointer()
                        .hover(|this| this.bg(theme.accent))
                        .child(
                            Icon::new(if dir {
                                IconName::Folder
                            } else {
                                IconName::File
                            })
                            .xsmall()
                            .flex_none(),
                        )
                        .child(div().min_w_0().truncate().child(entry.name.clone()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if dir {
                                this.browse(Some(path.clone()), cx)
                            } else {
                                this.open_thread_file(path.clone(), cx)
                            }
                        }))
                }))
                .into_any_element(),
        };
        Some(
            v_flex()
                .w(px(280.))
                .h_full()
                .flex_none()
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.sidebar)
                .child(
                    h_flex()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .items_center()
                        .border_b_1()
                        .border_color(theme.border)
                        .when_some(parent, |this, parent| {
                            this.child(
                                Button::new("files-up")
                                    .icon(IconName::ChevronLeft)
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Back")
                                    .accessibility_label("Back")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.browse(parent.clone(), cx)
                                    })),
                            )
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_medium()
                                .text_sm()
                                .child(title),
                        )
                        .child(
                            Button::new("files-close")
                                .icon(IconName::Close)
                                .ghost()
                                .xsmall()
                                .tooltip("Close")
                                .accessibility_label("Close files")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.files = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .id("files-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_2()
                        .child(body),
                )
                .into_any_element(),
        )
    }

    /// Open one of this conversation's own files.
    fn open_thread_file(&mut self, path: String, cx: &mut Context<Self>) {
        let thread_id = self.thread_id.clone();
        let dir = PathBuf::from(&self.thread_id).join("files");
        let name = path.clone();
        self.open_fetched(dir, &name, cx, move |c| async move {
            c.thread_file_bytes(&thread_id, &path).await
        });
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
        let dir = std::env::temp_dir().join("ironclaw-desktop").join(subdir);
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
        let items = self.menu(cx);
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
        // Stopping a run is the person's own doing; no alert for that.
        if let Some(end @ (RunEnd::Completed | RunEnd::Failed)) = ended {
            cx.emit(RunEnded {
                thread_id: self.thread_id.clone(),
                failed: end == RunEnd::Failed,
            });
        }
    }

    /// Recompute rows and tell the virtual list exactly what moved, so only
    /// changed rows are re-measured while text streams.
    fn refresh_rows(&mut self, cx: &mut Context<Self>) {
        let new_rows = self.transcript.rows();
        let old = self.rows.clone();
        let prefix = old
            .iter()
            .zip(new_rows.iter())
            .take_while(|(a, b)| a.key() == b.key())
            .count();
        let changed: Vec<usize> = (0..prefix).filter(|&i| old[i] != new_rows[i]).collect();
        let (old_len, new_len) = (old.len(), new_rows.len());
        self.scroller.update(cx, |state, cx| {
            if prefix < old_len || prefix < new_len {
                state.splice(prefix..old_len, new_len - prefix, cx);
            }
            for i in changed {
                state.remeasure_items(i..i + 1, cx);
            }
        });
        self.rows = Rc::new(new_rows);
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_string();
        if text.is_empty() && self.staged.is_empty() {
            return;
        }
        if self.staged.is_empty()
            && let Some(command) = goals::goal_command(&text)
        {
            return self.goal_command(command, window, cx);
        }
        if self.staged.is_empty() && match_command(&text, &self.commands).is_some() {
            self.run_command(text, window, cx);
            return;
        }
        let Some(channel) = self.channel.clone() else {
            self.set_error("This gateway has no chat channel configured.", cx);
            return;
        };
        self.composer
            .update(cx, |state, cx| state.clean(window, cx));
        self.error = None;
        let files = std::mem::take(&mut self.staged);
        let names: Vec<String> = files.iter().map(|f| f.filename.clone()).collect();
        let pending = self.transcript.push_pending_user(&text, &names);
        self.refresh_rows(cx);
        self.scroller
            .update(cx, |state, cx| state.scroll_to_end(cx));

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
                    // Put the files back so a retry needs no re-picking.
                    this.staged = files;
                    this.composer
                        .update(cx, |state, cx| state.set_value(text.clone(), window, cx));
                    this.set_error(e.to_string(), cx);
                    this.refresh_rows(cx);
                }
            });
        });
        self.keep(task);
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(run) = self.running_run() else {
            return;
        };
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.cancel_run(&thread_id, &run).await });
        self.run_and_report(job, cx);
    }

    fn retry(&mut self, run: String, cx: &mut Context<Self>) {
        let thread_id = self.thread_id.clone();
        let job = self
            .backend
            .spawn(move |c| async move { c.retry_run(&thread_id, &run).await });
        self.run_and_report(job, cx);
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
        self.error = Some(message.into());
        cx.notify();
    }

    /// Request tasks hold only a weak handle, so they can outlive the view
    /// safely. Dropping a GPUI task would cancel an in-flight request.
    fn keep(&mut self, task: Task<()>) {
        task.detach();
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::Focusable as _;
        let style = *Style::of(cx);
        let palette = *Palette::of(cx);
        let focused = self.composer.focus_handle(cx).is_focused(window);
        let rows = self.rows.clone();
        let weak = cx.weak_entity();
        let menu = self.menu(cx);
        let menu_index = self.menu_index.min(menu.len().saturating_sub(1));
        let (model_button, model_menu) = self.render_model_switcher(cx);
        let running = self.running_run().is_some();
        let stopping = self.transcript.is_stopping();
        // Fill the silence between send and the first streamed token.
        let working = stopping
            || running
                && !matches!(
                    self.rows.last(),
                    Some(Row::Assistant {
                        streaming: true,
                        ..
                    })
                );

        let list = MessageScroller::new("transcript", self.scroller.clone(), move |ix, _, cx| {
            let row = &rows[ix];
            h_flex().w_full().justify_center().px_4().py_1p5().child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .child(render_row(row, &weak, cx)),
            )
        });

        let banner = match self.status {
            StreamStatus::Reconnecting { delay_secs, .. } => {
                Some(format!("Connection lost. Reconnecting in {delay_secs}s"))
            }
            _ => None,
        };

        let files_open = self.files.is_some();
        let main = v_flex()
            .id("chat")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .drag_over::<ExternalPaths>(|this, _, _, cx| this.bg(cx.theme().accent))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.attach(paths.paths().to_vec(), cx)
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
            .child(div().flex_1().min_h_0().child(list.size_full()))
            .child(
                div().absolute().top_2().right_4().child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("fork")
                                .icon(IconName::Copy)
                                .ghost()
                                .small()
                                .tooltip("Fork this conversation")
                                .accessibility_label("Fork this conversation")
                                .on_click(cx.listener(|this, _, _, cx| this.fork(cx))),
                        )
                        .child(
                            Button::new("files-toggle")
                                .icon(IconName::FolderOpen)
                                .ghost()
                                .small()
                                .when(files_open, |b| b.selected(true))
                                .tooltip("Conversation files")
                                .accessibility_label("Conversation files")
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_files(cx))),
                        ),
                ),
            )
            .when(working, |this| {
                this.child(
                    h_flex().justify_center().px_4().child(
                        h_flex()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .child(kit::pulse_dots("working-dots", palette.accent))
                            .child(kit::shimmer_text(
                                "working-label",
                                if stopping { "Stopping…" } else { "Working…" },
                                cx,
                            )),
                    ),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex().justify_center().px_4().child(
                        div()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(error),
                    ),
                )
            })
            .when(!self.staged.is_empty(), |this| {
                this.child(
                    h_flex().justify_center().px_4().pt_2().child(
                        h_flex()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .flex_wrap()
                            .gap_1()
                            .children(self.staged.iter().enumerate().map(|(ix, file)| {
                                file_chip(
                                    SharedString::from(format!("staged-{ix}")),
                                    &file.filename,
                                    Some(file.bytes.len() as u64),
                                    cx,
                                )
                                .child(
                                    Button::new(SharedString::from(format!("unstage-{ix}")))
                                        .icon(IconName::Close)
                                        .xsmall()
                                        .ghost()
                                        .tooltip("Remove")
                                        .accessibility_label("Remove")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if ix < this.staged.len() {
                                                this.staged.remove(ix);
                                            }
                                            cx.notify();
                                        })),
                                )
                            })),
                    ),
                )
            })
            .when_some(self.render_orchestration(cx), |this, panel| {
                this.child(panel)
            })
            .when_some(model_menu, |this, list| {
                this.child(
                    h_flex()
                        .justify_center()
                        .px_4()
                        .child(div().w_full().max_w(px(CONTENT_MAX_WIDTH)).child(list)),
                )
            })
            .when(!menu.is_empty(), |this| {
                this.child(
                    h_flex().justify_center().px_4().child(
                        div()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .child(render_menu(&menu, menu_index, cx)),
                    ),
                )
            })
            .child({
                // The composer dock: one raised plate holding the text and
                // its controls. Focus morphs its rim toward the accent.
                let f = motion::state_t("composer:focus", focused, style.slide, cx.reduce_motion());
                let dock = kit::neutral(&style, &palette, 0.4 * f, 1.);
                let rim = motion::mix(dock.rim, palette.accent.opacity(0.55), f);
                let halo = gpui_kit::BoxShadow {
                    color: palette.accent.opacity(0.16 * f),
                    offset: gpui_kit::point(px(0.), px(0.)),
                    blur_radius: px(0.),
                    spread_radius: px(3. * f),
                    inset: false,
                };
                let mut shadows = dock.shadows.clone();
                shadows.extend(kit::floating(&style, &palette).shadows);
                shadows.push(halo);
                h_flex().justify_center().px_4().pb_4().pt_2().child(
                    v_flex()
                        .w_full()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .p(px(6.))
                        .gap_1()
                        .rounded(px(style.composer_radius))
                        .bg(dock.fill)
                        .border_1()
                        .border_color(rim)
                        .shadow(shadows)
                        .child(
                            div()
                                .w_full()
                                .px_1()
                                .capture_key_down(cx.listener(Self::on_composer_key))
                                .child(Textarea::new(&self.composer).appearance(false)),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .items_center()
                                .child(
                                    kit::button("attach")
                                        .icon(IconName::Plus)
                                        .ghost()
                                        .small()
                                        .tooltip("Attach files")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.pick_files(window, cx)
                                        })),
                                )
                                .when_some(model_button, |this, button| this.child(button))
                                .child(div().flex_1())
                                .when(!running && style.material != Material::Line, |d| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .text_color(palette.text_faint)
                                            .pr_1()
                                            .child("↵ to send"),
                                    )
                                })
                                .child(if running {
                                    kit::button("stop")
                                        .icon(IconName::Square)
                                        .small()
                                        .tooltip("Stop")
                                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                                } else {
                                    kit::button("send")
                                        .icon(IconName::ArrowUp)
                                        .primary()
                                        .small()
                                        .tooltip("Send (Enter)")
                                        .on_click(
                                            cx.listener(|this, _, window, cx| this.send(window, cx)),
                                        )
                                }),
                        ),
                )
            });
        h_flex()
            .size_full()
            .child(main)
            .when_some(self.render_files(cx), |this, panel| this.child(panel))
    }
}

fn render_row(row: &Row, weak: &WeakEntity<ChatView>, cx: &App) -> AnyElement {
    match row {
        Row::User {
            key,
            text,
            pending,
            attachments,
        } => v_flex()
            .w_full()
            .items_end()
            .gap_1()
            .when(!text.is_empty(), |this| {
                this.child(user_bubble(
                    SharedString::from(format!("{key}-bubble")),
                    text.clone(),
                    *pending,
                    cx,
                ))
            })
            .when(!attachments.is_empty(), |this| {
                this.child(h_flex().flex_wrap().justify_end().gap_1().children(
                    attachments.iter().enumerate().map(|(ix, chip)| {
                        let weak = weak.clone();
                        let open = chip.clone();
                        file_chip(
                            SharedString::from(format!("{key}-file-{ix}")),
                            &chip.filename,
                            chip.size,
                            cx,
                        )
                        .when(chip.message_id.is_some(), |this| {
                            this.cursor_pointer().on_click(move |_, _, cx| {
                                let chip = open.clone();
                                let _ = weak.update(cx, |this, cx| this.open_attachment(chip, cx));
                            })
                        })
                    }),
                ))
            })
            .into_any_element(),
        Row::Assistant {
            key,
            text,
            streaming,
        } => markdown(SharedString::from(key.clone()), text.clone(), *streaming).into_any_element(),
        Row::System { text, .. } => div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text.clone())
            .into_any_element(),
        Row::Thinking { text, .. } => {
            activity_line(IconName::Bot, "Thinking", Some(text.clone().into()), cx)
                .into_any_element()
        }
        Row::Narration { text, .. } => {
            activity_line(IconName::Ellipsis, text.clone(), None, cx).into_any_element()
        }
        Row::Tool {
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
            activity_line(icon, title.clone(), detail.clone().map(Into::into), cx)
                .into_any_element()
        }
        Row::Skills { names, .. } => activity_line(
            IconName::BookOpen,
            format!("Using skills: {}", names.join(", ")),
            None,
            cx,
        )
        .into_any_element(),
        Row::WorkSummary { text, .. } => {
            activity_line(IconName::FileText, text.clone(), None, cx).into_any_element()
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
        } => render_gate(
            key,
            run_id,
            gate_ref,
            *kind,
            headline,
            body,
            *allow_always,
            *state,
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
                    .child(div().flex_1().min_w_0().child(text.clone())),
            )
            .when(*retryable, |this| {
                let weak = weak.clone();
                let run = run_id.clone();
                this.child(
                    h_flex().child(
                        Button::new(SharedString::from(format!("{key}-retry")))
                            .label("Retry")
                            .small()
                            .on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |this, cx| this.retry(run.clone(), cx));
                            }),
                    ),
                )
            })
            .into_any_element(),
        Row::Command {
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
                    .child(input.clone()),
            )
            .when(!title.is_empty(), |this| {
                this.child(div().font_medium().child(title.clone()))
            })
            .children(fields.iter().map(|f| {
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .w(px(120.))
                            .flex_none()
                            .text_color(cx.theme().muted_foreground)
                            .child(f.label.clone()),
                    )
                    .child(div().flex_1().min_w_0().child(f.value.clone()))
            }))
            .children(lines.iter().map(|line| {
                div()
                    .when(*failed, |this| this.text_color(cx.theme().danger))
                    .child(line.clone())
            }))
            .into_any_element(),
    }
}

impl ChatView {
    /// The quick model switcher, shown once Settings pinned some models.
    fn render_model_switcher(
        &self,
        cx: &mut Context<Self>,
    ) -> (Option<Button>, Option<AnyElement>) {
        let Some((catalog, pick)) = self
            .model_choice
            .as_ref()
            .filter(|(c, _)| c.selection_enabled && !c.models.is_empty())
        else {
            return (None, None);
        };
        let default = catalog.workspace_default.clone().unwrap_or_default();
        let current = pick.clone().unwrap_or_else(|| default.clone());
        let button = Button::new("model-switcher")
            .label(current.clone())
            .ghost()
            .small()
            .tooltip("Model for your next messages")
            .accessibility_label("Choose model")
            .on_click(cx.listener(|this, _, _, cx| {
                this.model_menu_open = !this.model_menu_open;
                cx.notify();
            }));
        let menu = self.model_menu_open.then(|| {
            let entries = std::iter::once((None, format!("Default ({default})")))
                .chain(catalog.models.iter().map(|m| (Some(m.clone()), m.clone())));
            let pop = kit::popover("model-menu", cx);
            pop(v_flex()
                .w(px(320.))
                .p_1()
                .children(entries.enumerate().map(|(ix, (value, label))| {
                    let selected = value == *pick;
                    kit::row(SharedString::from(format!("model-option-{ix}")), selected, cx)
                        .role(Role::Button)
                        .aria_label(label.clone())
                        .child(
                            div()
                                .w(px(14.))
                                .when(selected, |d| d.child(Icon::new(IconName::Check).xsmall())),
                        )
                        .child(label)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.choose_model(value.clone(), cx)),
                        )
                })))
        });
        (Some(button), menu)
    }
}

/// A file name with its size, used for staged and sent attachments.
fn file_chip(
    id: SharedString,
    name: &str,
    size: Option<u64>,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(id)
        .gap_1p5()
        .px_2()
        .py_1()
        .max_w(px(260.))
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(Palette::of(cx).border_strong)
        .bg(Palette::of(cx).hover)
        .text_xs()
        .child(Icon::new(IconName::File).xsmall().flex_none())
        .child(div().min_w_0().truncate().child(name.to_string()))
        .when_some(size, |this, size| {
            this.child(
                div()
                    .flex_none()
                    .text_color(cx.theme().muted_foreground)
                    .child(format_bytes(size)),
            )
        })
}

/// The `/` menu above the composer: name, usage and description per command.
fn render_menu(
    items: &[RebornProductCommandInfo],
    selected: usize,
    cx: &mut Context<ChatView>,
) -> impl IntoElement {
    let pop = kit::popover("command-menu-pop", cx);
    pop(v_flex().w_full().child(div()
        .w_full()
        .max_h(px(280.))
        .id("command-menu")
        .overflow_y_scroll()
        .p_1()
        .children(items.iter().enumerate().map(|(ix, command)| {
            let pick = command.clone();
            kit::row(SharedString::from(format!("cmd-{}", command.name)), ix == selected, cx)
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
        }))))
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

    // A settled gate collapses to one line; the full card is only for asking.
    if let GateState::Closed(decision) = state {
        let (icon, verdict) = match decision {
            Some(GateDecision::Approve { always: true }) => {
                (IconName::CircleCheck, "Always allowed")
            }
            Some(GateDecision::Approve { always: false }) => (IconName::CircleCheck, "Approved"),
            Some(GateDecision::Decline) => (IconName::CircleX, "Declined"),
            None => (IconName::Check, "Answered"),
        };
        return h_flex()
            .gap_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(icon).xsmall())
            .child(div().font_medium().child(verdict))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(body.clone().unwrap_or_else(|| headline.to_string())),
            )
            .into_any_element();
    }

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
                        "Approve",
                        "approve",
                        GateDecision::Approve { always: false },
                    )
                    .primary(),
                )
                .when(allow_always, |this| {
                    this.child(decide(
                        "Always allow",
                        "always",
                        GateDecision::Approve { always: true },
                    ))
                })
                .child(decide("Decline", "decline", GateDecision::Decline).ghost())
                .into_any_element(),
        ),
    };

    card(cx)
        .child(
            h_flex()
                .gap_2()
                .text_sm()
                .font_medium()
                .items_start()
                .child(Icon::new(IconName::TriangleAlert).flex_none())
                .child(div().flex_1().min_w_0().child(headline.to_string())),
        )
        .when_some(body.clone(), |this, body| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(body),
            )
        })
        .children(actions)
        .into_any_element()
}
