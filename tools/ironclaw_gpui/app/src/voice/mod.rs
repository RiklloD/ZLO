//! Voice: dictation, spoken replies, talk mode and a wake word.
//!
//! - Push-to-talk: hold a lone modifier (Right Option by default, off until
//!   turned on in Settings) or click the composer's mic. Words appear in the
//!   composer as they are heard: settled words as text, the still-changing
//!   tail underlined (`composer.rs`). Release keeps the text; Settings can
//!   make release send it.
//! - Read replies aloud, for every chat or one thread; code blocks and
//!   tables are summarized as "see the screen" (`speakable.rs`).
//! - Talk mode: listen, send after a pause, speak the reply, listen again;
//!   speaking (or the push-to-talk key) interrupts the reply.
//! - Wake word (off by default, labelled with its battery cost): an
//!   on-device recognizer listens for a phrase, then captures the command.
//!
//! One [`coordinator::Coordinator`] owns the mic, so the four never fight.
//! Speech runs on the Mac (`apple.rs`); the engines sit behind the traits
//! in `engine.rs` so a cloud engine can follow. Battery: nothing listens or
//! polls unless the person turned it on or is talking right now.

#[cfg(target_os = "macos")]
mod apple;
mod composer;
mod coordinator;
mod engine;
mod mic;
mod ptt;
mod speakable;
mod transcript;
mod turn;
pub(crate) mod ui;
mod wake;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::channel::mpsc::unbounded;
use gpui_kit::AnyWindowHandle;
use ironclaw_gpui_client::transcript::Row;
use serde::{Deserialize, Serialize};

use self::composer::{Live, Target};
use self::coordinator::{Coordinator, Effect, Input, Mode, Purpose, TalkPhase, Timing};
use self::engine::{PrivacyPane, SpeechSynthesizer, SpeechToText, SttConfig, SttEvent, SttSink};
use self::mic::{Mic, Start};
use self::turn::PendingTurn;
use crate::ui::*;
use crate::workspace::Workspace;

pub(crate) use self::ptt::KEYS as HOTKEYS;

const TICK: Duration = Duration::from_millis(200);
const SPEECH_POLL: Duration = Duration::from_millis(250);
/// The wake recognizer restarts past this much text, keeping it light.
const WAKE_TEXT_LIMIT: usize = 240;

/// Voice preferences, saved as `voice.json` next to `prefs.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct VoicePrefs {
    /// The push-to-talk key is watched (a 10 Hz key-state poll).
    pub hotkey_enabled: bool,
    /// One of [`HOTKEYS`].
    pub hotkey: String,
    /// Releasing the push-to-talk key sends the message.
    pub auto_send: bool,
    /// Never send audio to Apple's servers.
    pub on_device_only: bool,
    /// Read replies aloud in every chat (threads can override).
    pub read_aloud: bool,
    /// Per-thread read-aloud choices, over `read_aloud`.
    pub read_aloud_threads: HashMap<String, bool>,
    /// 0..1, 0.5 the voice's normal pace.
    pub speech_rate: f32,
    /// Quiet after speech that ends a talk-mode turn.
    pub silence_ms: u32,
    pub wake_enabled: bool,
    pub wake_phrase: String,
}

impl Default for VoicePrefs {
    fn default() -> Self {
        Self {
            hotkey_enabled: false,
            hotkey: "right_alt".into(),
            auto_send: false,
            on_device_only: true,
            read_aloud: false,
            read_aloud_threads: HashMap::new(),
            speech_rate: 0.5,
            silence_ms: 2000,
            wake_enabled: false,
            wake_phrase: "Hey ZLO".into(),
        }
    }
}

impl Global for VoicePrefs {}

impl VoicePrefs {
    fn file() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("ZLO").join("voice.json"))
    }

    fn load() -> Self {
        Self::file()
            .and_then(|f| std::fs::read(f).ok()) // silent-ok: no file yet means the defaults
            .and_then(|b| serde_json::from_slice(&b).ok()) // silent-ok: an unreadable file falls back to the defaults
            .unwrap_or_default()
    }

    pub fn of(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    /// Whether replies in `thread` are read aloud.
    pub fn reads(&self, thread: &str) -> bool {
        self.read_aloud_threads
            .get(thread)
            .copied()
            .unwrap_or(self.read_aloud)
    }

    fn timing(&self) -> Timing {
        Timing {
            silence: Duration::from_millis(u64::from(self.silence_ms.clamp(800, 5000))),
            ..Timing::default()
        }
    }

    /// Edit, save off the UI thread, re-arm what changed and redraw.
    pub fn update(cx: &mut App, edit: impl FnOnce(&mut Self)) {
        let old = Self::of(cx);
        let mut prefs = old.clone();
        edit(&mut prefs);
        let saved = prefs.clone();
        cx.set_global(prefs.clone());
        cx.background_spawn(async move {
            let Some(file) = Self::file() else { return };
            let result = file
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| {
                    let json = serde_json::to_vec_pretty(&saved).map_err(std::io::Error::other)?;
                    std::fs::write(&file, json)
                });
            if let Err(e) = result {
                tracing::warn!("could not save voice preferences: {e}");
            }
        })
        .detach();
        // Deferred: a Settings click runs inside its window's update, and
        // stopping the mic may edit a composer in that window.
        cx.defer(move |cx| {
            if !cx.has_global::<Voice>() {
                return;
            }
            if old.hotkey_enabled != prefs.hotkey_enabled || old.hotkey != prefs.hotkey {
                arm_hotkey(cx);
            }
            cx.global_mut::<Voice>().coord.set_timing(prefs.timing());
            send(cx, Input::SetAutoSend(prefs.auto_send));
            let wake = wake_on(&prefs);
            if wake != wake_on(&old) || old.wake_phrase != prefs.wake_phrase {
                send(cx, Input::SetWake(false));
                send(cx, Input::SetWake(wake));
            }
        });
        cx.refresh_windows();
    }
}

fn wake_on(prefs: &VoicePrefs) -> bool {
    prefs.wake_enabled && wake::valid(&prefs.wake_phrase)
}

/// Something to tell the person, with the Settings pane that fixes it.
#[derive(Clone, Debug)]
pub(crate) struct Notice {
    pub text: String,
    pub pane: Option<PrivacyPane>,
}

struct Voice {
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    coord: Coordinator,
    /// The engine behind its session gate.
    mic: Mic,
    tts: Box<dyn SpeechSynthesizer>,
    sink: SttSink,
    _events: Task<()>,
    /// The talk-mode message waiting for its reply.
    turn: PendingTurn,
    /// The recognizer's latest full transcript.
    heard: String,
    live: Option<Live>,
    /// Where dictation goes: set by the mic button or resolved on start.
    target: Option<Target>,
    tick: Option<Task<()>>,
    speech_poll: Option<Task<()>>,
    _hotkey: Option<HotkeyArm>,
    notice: Option<Notice>,
}

impl Global for Voice {}

fn voice_mut(cx: &mut App) -> Option<&mut Voice> {
    cx.has_global::<Voice>().then(|| cx.global_mut::<Voice>())
}

struct HotkeyArm {
    #[cfg(target_os = "macos")]
    _watcher: ptt::Watcher,
    _listen: Task<()>,
}

/// Load the preferences and arm what they turn on. Called once by the
/// workspace.
pub(crate) fn start(workspace: WeakEntity<Workspace>, window: AnyWindowHandle, cx: &mut App) {
    let prefs = VoicePrefs::load();
    cx.set_global(prefs.clone());
    let (sink, mut events) = unbounded::<(u64, SttEvent)>();
    let listen = cx.spawn(async move |cx| {
        use futures::StreamExt as _;
        while let Some((session, event)) = events.next().await {
            cx.update(|cx| on_stt(session, event, cx));
        }
    });
    #[cfg(target_os = "macos")]
    let (stt, tts): (Box<dyn SpeechToText>, Box<dyn SpeechSynthesizer>) = (
        Box::<apple::AppleStt>::default(),
        Box::<apple::AppleTts>::default(),
    );
    #[cfg(not(target_os = "macos"))]
    let (stt, tts): (Box<dyn SpeechToText>, Box<dyn SpeechSynthesizer>) =
        (Box::new(Unsupported), Box::new(Unsupported));
    let now = Instant::now();
    cx.set_global(Voice {
        workspace,
        window,
        coord: Coordinator::new(prefs.timing(), now),
        mic: Mic::new(stt, now),
        tts,
        sink,
        _events: listen,
        turn: PendingTurn::default(),
        heard: String::new(),
        live: None,
        target: None,
        tick: None,
        speech_poll: None,
        _hotkey: None,
        notice: None,
    });
    arm_hotkey(cx);
    send(cx, Input::SetAutoSend(prefs.auto_send));
    if wake_on(&prefs) {
        send(cx, Input::SetWake(true));
    }
}

/// The voice's state, for the composer pill.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Status {
    Off,
    Dictating,
    Listening,
    Thinking,
    Speaking,
    /// Wake word armed, nothing else going on.
    Waiting,
}

pub(crate) fn status(cx: &App) -> Status {
    let Some(v) = cx.try_global::<Voice>() else {
        return Status::Off;
    };
    match v.coord.mode() {
        Mode::Dictating(_) | Mode::Finishing { .. } => Status::Dictating,
        Mode::Talk(TalkPhase::Listening) | Mode::WakeCapture | Mode::WakeArmed => Status::Listening,
        Mode::Talk(TalkPhase::Thinking) => Status::Thinking,
        Mode::Talk(TalkPhase::Speaking) => Status::Speaking,
        Mode::Idle | Mode::WakeListening if v.coord.speaking() => Status::Speaking,
        Mode::WakeListening => Status::Waiting,
        Mode::Idle => Status::Off,
    }
}

pub(crate) fn talk_mode(cx: &App) -> bool {
    cx.try_global::<Voice>()
        .is_some_and(|v| matches!(v.coord.mode(), Mode::Talk(_)))
}

/// Whether `input` is the composer voice is typing into.
pub(crate) fn targets(input: &Entity<TextareaState>, cx: &App) -> bool {
    cx.try_global::<Voice>()
        .and_then(|v| v.target.as_ref())
        .is_some_and(|t| t.is(input))
}

pub(crate) fn notice(cx: &App) -> Option<Notice> {
    cx.try_global::<Voice>().and_then(|v| v.notice.clone())
}

pub(crate) fn dismiss_notice(cx: &mut App) {
    if let Some(v) = cx.try_global::<Voice>()
        && v.notice.is_some()
    {
        cx.global_mut::<Voice>().notice = None;
        cx.refresh_windows();
    }
}

/// The mic button: start or stop dictating into `input`. Deferred, so it
/// runs after the click's window update (it edits that window).
pub(crate) fn toggle_dictation(
    input: Entity<TextareaState>,
    window: AnyWindowHandle,
    thread: Option<String>,
    cx: &mut App,
) {
    cx.defer(move |cx| {
        // A click that starts a dictation (also from wake listening, where
        // the mic is already on) types into the clicked composer.
        if let Some(v) = voice_mut(cx)
            && v.coord.starts_dictation()
        {
            v.target = Some(Target::new(&input, window, thread));
        }
        send(cx, Input::MicClicked);
    });
}

/// Talk mode on or off, aimed at `input` (or the front composer).
pub(crate) fn toggle_talk(
    input: Option<Entity<TextareaState>>,
    window: Option<AnyWindowHandle>,
    thread: Option<String>,
    cx: &mut App,
) {
    cx.defer(move |cx| {
        if let (Some(input), Some(window)) = (input, window)
            && let Some(v) = voice_mut(cx)
            && !matches!(v.coord.mode(), Mode::Talk(_))
        {
            v.target = Some(Target::new(&input, window, thread));
        }
        send(cx, Input::TalkToggled);
    });
}

/// The pill's ✕ and Escape: leave talk mode, end dictation, stop speaking.
pub(crate) fn exit(cx: &mut App) {
    cx.defer(|cx| send(cx, Input::Exit));
}

pub(crate) fn stop_speaking(cx: &mut App) {
    cx.defer(|cx| send(cx, Input::StopSpeaking));
}

/// Flip read-aloud for one thread.
pub(crate) fn toggle_read_aloud(thread: &str, cx: &mut App) {
    let thread = thread.to_string();
    VoicePrefs::update(cx, move |p| {
        let on = !p.reads(&thread);
        if on == p.read_aloud {
            p.read_aloud_threads.remove(&thread);
        } else {
            p.read_aloud_threads.insert(thread, on);
        }
    });
}

/// A chat's run finished with `rows` on screen: speak the reply if it
/// answers talk mode's own message, or if the thread reads aloud.
pub(crate) fn reply_finished(thread: &str, rows: &[Row], cx: &mut App) {
    let Some(v) = cx.try_global::<Voice>() else {
        return;
    };
    let token = talk_answer(v, thread);
    if token.is_none() && !VoicePrefs::of(cx).reads(thread) {
        return;
    }
    let spoken = last_reply(rows)
        .map(speakable::speakable)
        .unwrap_or_default();
    cx.defer(move |cx| match token {
        Some(token) => answer(token, spoken, cx),
        None => send(cx, Input::ReadAloud(spoken)),
    });
}

/// A chat's run failed: if it was talk mode's turn, say so and listen again.
pub(crate) fn reply_failed(thread: &str, cx: &mut App) {
    let Some(token) = cx
        .try_global::<Voice>()
        .and_then(|v| talk_answer(v, thread))
    else {
        return;
    };
    cx.defer(move |cx| answer(token, "That run failed. See the screen.".into(), cx));
}

/// The pending talk turn's token when `thread` answers it.
fn talk_answer(v: &Voice, thread: &str) -> Option<u64> {
    (v.coord.mode() == Mode::Talk(TalkPhase::Thinking))
        .then(|| v.turn.answers(thread))
        .flatten()
}

/// Deliver a talk-mode answer unless the turn moved on since it was taken.
fn answer(token: u64, spoken: String, cx: &mut App) {
    let Some(v) = voice_mut(cx) else {
        return;
    };
    if !v.turn.current(token) || v.coord.mode() != Mode::Talk(TalkPhase::Thinking) {
        return;
    }
    v.turn.clear();
    send(cx, Input::ReplyFinished(spoken));
}

/// A new thread got its first message in `composer` (both the new-thread
/// screen and the quick composer end here). If talk mode sent it, the turn
/// waits for this thread and talk mode listens in its composer next: the
/// composer that sent it is gone.
pub(crate) fn thread_created(
    thread: &str,
    composer: &Entity<TextareaState>,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    if let Some(v) = voice_mut(cx)
        && v.turn.claim_new(thread)
    {
        v.target = Some(Target::new(composer, window, Some(thread.to_string())));
    }
}

/// The assistant's words in the last turn (after the last user message).
fn last_reply(rows: &[Row]) -> Option<&str> {
    let mut texts = Vec::new();
    for row in rows.iter().rev() {
        match row {
            Row::User { .. } => break,
            Row::Assistant { text, .. } if !text.trim().is_empty() => texts.push(text.as_str()),
            _ => {}
        }
    }
    // A turn can answer in several messages between tools; the last one is
    // the answer.
    texts.first().copied()
}

/// Feed the coordinator and carry out what it decides.
fn send(cx: &mut App, input: Input) {
    if !cx.has_global::<Voice>() {
        return;
    }
    let fx = cx.global_mut::<Voice>().coord.handle(input, Instant::now());
    for effect in fx {
        run(effect, cx);
    }
    follow_mode(cx);
    cx.refresh_windows();
}

/// Timers and the composer link follow the mode.
fn follow_mode(cx: &mut App) {
    let v = cx.global::<Voice>();
    if v.coord.needs_tick() && v.tick.is_none() {
        let tick = cx.spawn(async move |cx: &mut gpui_kit::AsyncApp| {
            loop {
                cx.background_executor().timer(TICK).await;
                let more = cx.update(|cx| {
                    send(cx, Input::Tick);
                    cx.try_global::<Voice>()
                        .is_some_and(|v| v.coord.needs_tick())
                });
                if !more {
                    break;
                }
            }
            cx.update(|cx| {
                if let Some(v) = voice_mut(cx) {
                    v.tick = None;
                }
            });
        });
        cx.global_mut::<Voice>().tick = Some(tick);
    }
    let v = cx.global_mut::<Voice>();
    if !matches!(v.coord.mode(), Mode::Talk(_)) {
        v.turn.clear();
    }
    // Words now go to a composer (dictation start, a barge-in, wake
    // capture): link one and type what was heard so far. A composer that
    // went away (sent draft, closed quick composer) is replaced by the one
    // in front.
    let v = cx.global::<Voice>();
    if v.coord.routes_text() && v.live.is_none() {
        let live_target = v.target.clone().filter(Target::alive);
        let Some(target) = live_target.or_else(|| resolve(false, cx)) else {
            return;
        };
        let v = cx.global_mut::<Voice>();
        v.target = Some(target.clone());
        v.live = Some(Live::new(target));
        let text = routed_text(cx);
        if !text.is_empty() {
            with_live(cx, |live, cx| live.partial(&text, cx));
        }
    }
}

fn run(effect: Effect, cx: &mut App) {
    match effect {
        Effect::StartMic(purpose) => start_mic(purpose, cx),
        Effect::StopMic => cx.global_mut::<Voice>().mic.stop(),
        Effect::EndAudio => cx.global_mut::<Voice>().mic.end_audio(),
        Effect::Commit => {
            with_live(cx, |live, cx| live.commit(cx));
            cx.global_mut::<Voice>().live = None;
        }
        Effect::Discard => {
            with_live(cx, |live, cx| live.discard(cx));
            cx.global_mut::<Voice>().live = None;
        }
        Effect::Send => {
            let v = cx.global_mut::<Voice>();
            if let Some(target) = v.target.clone() {
                // Talk mode sends as it starts thinking: wait for this
                // thread (or the one this composer creates).
                if v.coord.mode() == Mode::Talk(TalkPhase::Thinking) {
                    v.turn.sent(target.thread.clone());
                }
                target.send(cx);
            }
        }
        Effect::Speak(text) => speak(&text, cx),
        Effect::StopSpeaking => {
            let v = cx.global_mut::<Voice>();
            v.tts.stop();
            v.speech_poll = None;
        }
        Effect::OpenComposer => {
            let target = resolve(true, cx);
            cx.global_mut::<Voice>().target = target;
        }
        Effect::Notice(text) => {
            cx.global_mut::<Voice>().notice = Some(Notice { text, pane: None });
        }
    }
}

/// Run `f` on the live composer link, if there is one.
fn with_live(cx: &mut App, f: impl FnOnce(&mut Live, &mut App)) {
    if let Some(mut live) = cx.global_mut::<Voice>().live.take() {
        f(&mut live, cx);
        let v = cx.global_mut::<Voice>();
        // `f` may have ended the link (commit); only put back a live one.
        if v.live.is_none() && v.coord.routes_text() {
            v.live = Some(live);
        }
    }
}

/// The composer to type into: the quick composer when open, else the front
/// chat or new-thread screen; `open_quick` opens the quick composer when
/// the main window is not in front.
fn resolve(open_quick: bool, cx: &mut App) -> Option<Target> {
    let v = cx.global::<Voice>();
    let (workspace, window) = (v.workspace.clone(), v.window);
    window
        .update(cx, |_, window, cx| {
            workspace
                .update(cx, |ws, cx| ws.voice_composer(open_quick, window, cx))
                .ok()
                .flatten()
        })
        .ok()
        .flatten()
        .map(|(input, window, thread)| Target::new(&input, window, thread))
}

/// The heard text that belongs in the composer (after the wake phrase in
/// wake capture).
fn routed_text(cx: &App) -> String {
    let v = cx.global::<Voice>();
    match v.coord.mode() {
        Mode::WakeArmed | Mode::WakeCapture => {
            let phrase = VoicePrefs::of(cx).wake_phrase;
            wake::command_after(&phrase, &v.heard).unwrap_or_default()
        }
        _ => v.heard.clone(),
    }
}

fn start_mic(purpose: Purpose, cx: &mut App) {
    let v = cx.global_mut::<Voice>();
    v.live = None;
    v.heard.clear();
    let needs_target = matches!(purpose, Purpose::Dictation | Purpose::Talk)
        && !v.target.as_ref().is_some_and(Target::alive);
    if needs_target {
        let target = resolve(false, cx);
        cx.global_mut::<Voice>().target = target;
    }
    let config = mic_config(purpose, cx);
    let v = cx.global_mut::<Voice>();
    let sink = v.sink.clone();
    match v
        .mic
        .start(purpose, authorized(), &config, sink, Instant::now())
    {
        Start::Opened => tracing::debug!(?purpose, "voice: mic on"),
        Start::Waiting => {}
        Start::Failed(failure) => fail(failure, cx),
        Start::Authorize => {
            // The first use asks macOS. The mic opens on a yes only if that
            // start is still current (nothing stopped or replaced it) and
            // the mode still wants that session.
            #[cfg(target_os = "macos")]
            cx.spawn(async move |cx: &mut gpui_kit::AsyncApp| {
                let result = apple::authorize().await;
                cx.update(|cx| {
                    let Some(v) = voice_mut(cx) else { return };
                    let waiting = v.mic.authorized();
                    match (result, waiting) {
                        (Ok(()), Some(purpose)) if v.coord.mic_purpose() == Some(purpose) => {
                            let config = mic_config(purpose, cx);
                            let v = cx.global_mut::<Voice>();
                            let sink = v.sink.clone();
                            if let Err(failure) = v.mic.open(&config, sink, Instant::now()) {
                                fail(failure, cx);
                            }
                        }
                        (Ok(()), _) => {}
                        (Err(failure), Some(_)) => fail(failure, cx),
                        // Denied for a start nobody waits for any more:
                        // only say why.
                        (Err(failure), None) => {
                            v.notice = Some(Notice {
                                text: failure.message,
                                pane: failure.pane,
                            });
                        }
                    }
                    cx.refresh_windows();
                });
            })
            .detach();
        }
    }
}

fn mic_config(purpose: Purpose, cx: &App) -> SttConfig {
    let prefs = VoicePrefs::of(cx);
    let mut hints = vec!["ZLO".to_string()];
    if purpose == Purpose::Wake {
        hints.push(prefs.wake_phrase.clone());
    }
    SttConfig {
        purpose,
        on_device_only: prefs.on_device_only,
        hints,
    }
}

/// Listening could not start: everything stops and the person hears why.
fn fail(failure: engine::SttFailure, cx: &mut App) {
    let pane = failure.pane;
    send(
        cx,
        Input::RecognizerEnded {
            error: Some(failure.message.clone()),
            fatal: true,
        },
    );
    cx.global_mut::<Voice>().notice = Some(Notice {
        text: failure.message,
        pane,
    });
    cx.refresh_windows();
}

fn on_stt(session: u64, event: SttEvent, cx: &mut App) {
    let Some(v) = voice_mut(cx) else {
        return;
    };
    // Stale sessions are dropped; an end tears the session down here.
    let Some(event) = v.mic.accept(session, event, Instant::now()) else {
        return;
    };
    match event {
        SttEvent::Partial(text) | SttEvent::Final(text)
            if text.is_empty() && v.heard.is_empty() => {}
        SttEvent::Partial(text) => heard(text, false, cx),
        SttEvent::Final(text) => heard(text, true, cx),
        SttEvent::Ended { error, fatal } => {
            send(cx, Input::RecognizerEnded { error, fatal });
        }
    }
}

fn heard(text: String, is_final: bool, cx: &mut App) {
    let v = cx.global_mut::<Voice>();
    v.heard = text;
    let mode = v.coord.mode();
    if mode == Mode::WakeListening {
        let phrase = VoicePrefs::of(cx).wake_phrase;
        let v = cx.global::<Voice>();
        if wake::find(&phrase, &v.heard).is_some() {
            send(cx, Input::WakePhrase);
        } else if v.heard.len() > WAKE_TEXT_LIMIT {
            start_mic(Purpose::Wake, cx);
            return;
        }
    }
    let routed = routed_text(cx);
    let words = !routed.trim().is_empty();
    if cx.global::<Voice>().coord.routes_text() {
        with_live(cx, |live, cx| {
            if is_final {
                live.finish(&routed, cx);
            } else {
                live.partial(&routed, cx);
            }
        });
    }
    send(cx, Input::Heard { words });
}

fn speak(text: &str, cx: &mut App) {
    let rate = VoicePrefs::of(cx).speech_rate;
    let v = cx.global_mut::<Voice>();
    if let Err(e) = v.tts.speak(text, rate) {
        v.notice = Some(Notice {
            text: format!("Could not speak the reply: {e}"),
            pane: None,
        });
        send(cx, Input::SpeechFinished);
        return;
    }
    // Poll for the end only while speaking; a voice that never starts
    // counts as finished after a grace.
    let started = Instant::now();
    let poll = cx.spawn(async move |cx: &mut gpui_kit::AsyncApp| {
        let mut seen = false;
        loop {
            cx.background_executor().timer(SPEECH_POLL).await;
            let done = cx.update(|cx| {
                let Some(v) = voice_mut(cx) else {
                    return true;
                };
                let speaking = v.tts.is_speaking();
                seen |= speaking;
                !speaking && (seen || started.elapsed() > Duration::from_secs(2))
            });
            if done {
                break;
            }
        }
        cx.update(|cx| {
            if let Some(v) = voice_mut(cx) {
                v.tts.stop();
                v.speech_poll = None;
            }
            send(cx, Input::SpeechFinished);
        });
    });
    cx.global_mut::<Voice>().speech_poll = Some(poll);
}

/// (Re)arm the push-to-talk watcher to match the preferences.
fn arm_hotkey(cx: &mut App) {
    let prefs = VoicePrefs::of(cx);
    // The old watcher may be mid-hold: no release will come from it, so
    // end that take before dropping it.
    if cx.global::<Voice>()._hotkey.is_some() {
        send(cx, Input::HotkeyLost);
    }
    let v = cx.global_mut::<Voice>();
    v._hotkey = None;
    if !prefs.hotkey_enabled {
        return;
    }
    #[cfg(target_os = "macos")]
    match ptt::Watcher::new(&prefs.hotkey) {
        Ok((watcher, mut events)) => {
            let listen = cx.spawn(async move |cx| {
                use futures::StreamExt as _;
                while let Some(event) = events.next().await {
                    let input = match event {
                        ptt::PttEvent::Start => Input::HotkeyDown,
                        ptt::PttEvent::Stop => Input::HotkeyUp,
                        ptt::PttEvent::Cancel => Input::HotkeyCancel,
                    };
                    cx.update(|cx| {
                        if input == Input::HotkeyDown
                            && let Some(v) = voice_mut(cx)
                            && v.coord.starts_dictation()
                        {
                            // Aim at whatever is in front now.
                            v.target = None;
                            let target = resolve(true, cx);
                            cx.global_mut::<Voice>().target = target;
                        }
                        send(cx, input)
                    });
                }
                // The watcher thread stopped: a held take has no release.
                cx.update(|cx| send(cx, Input::HotkeyLost));
            });
            cx.global_mut::<Voice>()._hotkey = Some(HotkeyArm {
                _watcher: watcher,
                _listen: listen,
            });
        }
        Err(e) => {
            cx.global_mut::<Voice>().notice = Some(Notice {
                text: e,
                pane: None,
            });
        }
    }
}

#[cfg(not(target_os = "macos"))]
struct Unsupported;

#[cfg(not(target_os = "macos"))]
impl SpeechToText for Unsupported {
    fn start(&mut self, _: u64, _: &SttConfig, _: SttSink) -> Result<(), engine::SttFailure> {
        Err(engine::SttFailure {
            message: "Voice needs macOS.".into(),
            pane: None,
        })
    }
    fn end_audio(&mut self) {}
    fn stop(&mut self) {}
}

#[cfg(not(target_os = "macos"))]
impl SpeechSynthesizer for Unsupported {
    fn speak(&mut self, _: &str, _: f32) -> Result<(), String> {
        Err("Voice needs macOS.".into())
    }
    fn stop(&mut self) {}
    fn is_speaking(&self) -> bool {
        false
    }
}

/// Both voice permissions granted (no prompt needed).
pub(crate) fn authorized() -> bool {
    #[cfg(target_os = "macos")]
    return apple::authorized();
    #[cfg(not(target_os = "macos"))]
    false
}

/// Ask macOS for the microphone and speech recognition now (Settings).
pub(crate) fn request_access(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.spawn(async move |cx| {
        let result = apple::authorize().await;
        cx.update(|cx| {
            if let Err(failure) = result {
                if let Some(pane) = failure.pane {
                    open_settings(pane);
                }
                if let Some(v) = voice_mut(cx) {
                    v.notice = Some(Notice {
                        text: failure.message,
                        pane: failure.pane,
                    });
                }
            }
            cx.refresh_windows();
        });
    })
    .detach();
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

/// Open a privacy pane in System Settings.
pub(crate) fn open_settings(pane: PrivacyPane) {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(pane.url())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: &str) -> Row {
        Row::Assistant {
            key: text.into(),
            text: text.into(),
            streaming: false,
        }
    }

    fn user(text: &str) -> Row {
        Row::User {
            key: text.into(),
            text: text.into(),
            pending: false,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn last_reply_is_the_final_answer_of_the_last_turn() {
        let rows = vec![
            user("q1"),
            assistant("old answer"),
            user("q2"),
            assistant("Let me check."),
            assistant("Here it is."),
        ];
        assert_eq!(last_reply(&rows), Some("Here it is."));
    }

    #[test]
    fn a_turn_without_words_has_no_reply() {
        let rows = vec![user("q1"), assistant("old"), user("q2"), assistant("  ")];
        assert_eq!(last_reply(&rows), None);
        assert_eq!(last_reply(&[]), None);
    }

    #[test]
    fn per_thread_choices_override_the_global_switch() {
        let mut p = VoicePrefs::default();
        assert!(!p.reads("a"));
        p.read_aloud_threads.insert("a".into(), true);
        assert!(p.reads("a"));
        p.read_aloud = true;
        p.read_aloud_threads.insert("b".into(), false);
        assert!(!p.reads("b"));
        assert!(p.reads("c"));
    }

    #[test]
    fn defaults_keep_everything_off() {
        let p = VoicePrefs::default();
        assert!(!p.hotkey_enabled && !p.wake_enabled && !p.read_aloud && !p.auto_send);
        assert!(p.on_device_only);
        assert_eq!(p.hotkey, "right_alt");
        assert!(!wake_on(&p));
    }
}
