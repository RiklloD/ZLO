//! The one owner of a Claude turn's stdin.
//!
//! Everything that goes to the CLI passes this task's single ordered
//! mailbox: the hook registration, the prompt and the steers held for it,
//! every hook and question answer, each steer, and the close. Nothing else
//! holds the pipe, so the stdout reader never waits on a write.
//!
//! Each steer's ledger entry (`steer.rs`) moves only here or from the CLI's
//! own frames: held → written (a write that completed) → read (`started` or
//! an echo); a write that fails leaves it unread (held ones back to held,
//! in place). A steer is refused, never written, once the ledger says the
//! turn may close: from then on nothing new is asked of the CLI.
//!
//! Every write is bounded by [`DEFAULT_WRITE_TIMEOUT`]. One that fails or
//! times out closes the pipe for good (it may hold half a frame) and flips
//! the turn's [`InputState`] to `Failed`, which wakes the reader: it then
//! gives the CLI a bounded exit grace and ends the turn, result or not.

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot, watch};

use super::control::{SharedSteers, with_ledger};
use super::steer::HeldSteer;

/// How long one frame may take to go onto the CLI's stdin.
pub(super) const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether the CLI can still be written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InputState {
    Open,
    /// Closed on purpose (the turn is settled, or ended).
    Closed,
    /// A write failed or timed out: the turn must end.
    Failed,
}

/// What became of a steer handed to the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SteerAdmit {
    /// On the wire (the write completed), recorded in the ledger.
    Written,
    /// The prompt is not out yet: held, written right after it.
    Held,
    /// Never written: input is closed, the turn is settling, or the write
    /// failed. The caller queues it.
    Refused,
}

enum Command {
    /// A frame (hook registration, hook or question answer, any control
    /// response); `done` hears whether it was written.
    Frame {
        line: String,
        done: Option<oneshot::Sender<bool>>,
    },
    /// The prompt, then every steer held for it, in order.
    Prompt { run_id: String, line: String },
    Steer {
        steer: HeldSteer,
        admit: oneshot::Sender<SteerAdmit>,
    },
    /// EOF once the ledger says nothing is open any more (or `force`).
    Close { force: bool },
}

/// The turn's handle on its input owner. Cloneable; sending never waits.
#[derive(Clone)]
pub(super) struct Input {
    tx: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<InputState>,
}

impl Input {
    /// Start the owner of `pipe`. `steers` is the turn's ledger.
    pub(super) fn spawn(pipe: ChildStdin, steers: SharedSteers, write_timeout: Duration) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(InputState::Open);
        let owner = Owner {
            pipe: Some(pipe),
            ready: false,
            steers,
            state: state_tx,
            write_timeout,
        };
        tokio::spawn(owner.run(rx));
        Self { tx, state }
    }

    fn send(&self, command: Command) -> bool {
        self.tx.send(command).is_ok()
    }

    /// Queue a frame; resolves `true` once it is written.
    pub(super) async fn frame(&self, line: String) -> bool {
        let (done, written) = oneshot::channel();
        if !self.send(Command::Frame {
            line,
            done: Some(done),
        }) {
            return false;
        }
        written.await.unwrap_or(false) // silent-ok: the owner is gone, so not written
    }

    /// Queue a frame nobody waits on.
    pub(super) fn frame_unawaited(&self, line: String) {
        if !self.send(Command::Frame { line, done: None }) {
            tracing::debug!("claude cli: input owner gone; frame dropped");
        }
    }

    /// Queue the prompt (and so the held steers after it).
    pub(super) fn prompt(&self, run_id: &str, line: String) {
        if !self.send(Command::Prompt {
            run_id: run_id.to_string(),
            line,
        }) {
            tracing::debug!("claude cli: input owner gone; prompt dropped");
        }
    }

    /// Hand a steer over; resolves with what became of it.
    pub(super) async fn steer(&self, steer: HeldSteer) -> SteerAdmit {
        let (admit, verdict) = oneshot::channel();
        if !self.send(Command::Steer { steer, admit }) {
            return SteerAdmit::Refused;
        }
        verdict.await.unwrap_or(SteerAdmit::Refused) // silent-ok: the owner is gone, so never written
    }

    /// Close once nothing is open (`force`: now).
    pub(super) fn close(&self, force: bool) {
        if !self.send(Command::Close { force }) {
            tracing::debug!("claude cli: input owner already gone");
        }
    }

    /// Close now and wait until the owner has: no write happens after.
    pub(super) async fn close_now(&self) {
        self.close(true);
        let mut state = self.state.clone();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a gone owner has dropped the pipe too"
        )]
        let _ = state.wait_for(|s| *s != InputState::Open).await;
    }

    pub(super) fn state(&self) -> InputState {
        *self.state.borrow()
    }

    /// A receiver that wakes on every change (for the read loop's select).
    pub(super) fn watch(&self) -> watch::Receiver<InputState> {
        self.state.clone()
    }
}

struct Owner {
    pipe: Option<ChildStdin>,
    /// The prompt is out: steers go straight to the wire.
    ready: bool,
    steers: SharedSteers,
    state: watch::Sender<InputState>,
    write_timeout: Duration,
}

impl Owner {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Command>) {
        while let Some(command) = rx.recv().await {
            match command {
                Command::Frame { line, done } => {
                    let written = self.write(&line).await;
                    if let Some(done) = done
                        && done.send(written).is_err()
                    {
                        tracing::debug!("claude cli: nobody waits on this frame");
                    }
                }
                Command::Prompt { run_id, line } => self.prompt(&run_id, &line).await,
                Command::Steer { steer, admit } => {
                    let verdict = self.steer(steer).await;
                    if admit.send(verdict).is_err() {
                        tracing::debug!("claude cli: the steerer stopped waiting");
                    }
                }
                Command::Close { force } => {
                    if force || with_ledger(&self.steers, |l| l.may_close()) {
                        self.shut(InputState::Closed);
                    }
                }
            }
        }
        // Every handle is gone: the turn is over.
        self.shut(InputState::Closed);
    }

    /// Write one frame; a failure closes the pipe for good and fails input.
    async fn write(&mut self, line: &str) -> bool {
        let Some(pipe) = self.pipe.as_mut() else {
            return false;
        };
        let write = async {
            pipe.write_all(format!("{line}\n").as_bytes()).await?;
            pipe.flush().await
        };
        let written = matches!(
            tokio::time::timeout(self.write_timeout, write).await,
            Ok(Ok(()))
        );
        if !written {
            tracing::debug!("claude cli: a write to the CLI failed or timed out; input closed");
            self.shut(InputState::Failed);
        }
        written
    }

    async fn prompt(&mut self, run_id: &str, line: &str) {
        with_ledger(&self.steers, |l| l.prompt(run_id));
        if !self.write(line).await {
            return;
        }
        self.ready = true;
        // Held steers, in the order they were accepted. One whose write
        // fails goes back to held (input is failed now; the rest stay held).
        for steer in with_ledger(&self.steers, |l| l.held()) {
            with_ledger(&self.steers, |l| l.sent(&steer));
            if !self.write(&steer.line).await {
                with_ledger(&self.steers, |l| l.unsent(&steer.uuid, true));
                return;
            }
        }
    }

    async fn steer(&mut self, steer: HeldSteer) -> SteerAdmit {
        if self.pipe.is_none() {
            return SteerAdmit::Refused;
        }
        // Decided with the ledger locked, against the reader folding a
        // result: a turn that may close asks nothing new of the CLI.
        let admitted = with_ledger(&self.steers, |l| {
            if l.may_close() {
                None
            } else if !self.ready {
                l.hold(steer.clone());
                Some(SteerAdmit::Held)
            } else {
                l.sent(&steer);
                Some(SteerAdmit::Written)
            }
        });
        match admitted {
            None => SteerAdmit::Refused,
            Some(SteerAdmit::Written) => {
                if self.write(&steer.line).await {
                    SteerAdmit::Written
                } else {
                    with_ledger(&self.steers, |l| l.unsent(&steer.uuid, false));
                    SteerAdmit::Refused
                }
            }
            Some(other) => other,
        }
    }

    fn shut(&mut self, to: InputState) {
        // Dropping the pipe closes it at once; nothing waits on the CLI.
        drop(self.pipe.take());
        self.state.send_if_modified(|state| {
            if *state == InputState::Open {
                *state = to;
                true
            } else {
                false
            }
        });
    }
}
