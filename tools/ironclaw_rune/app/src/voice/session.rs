//! The voice engine behind [`super::VoiceState`]: the coordinator, the mic
//! session gate, the synthesizer, and the code that carries the
//! coordinator's effects out against them and the composers.
//!
//! Rune port of the effect runner in the GPUI `voice/mod.rs` (`send`,
//! `follow_mode`, `run`, `start_mic`, `on_stt`, `heard`, `speak`,
//! `arm_hotkey`). GPUI's timer tasks became [`poll`] with `Tx::wake_after`;
//! its async listeners became threads that land through
//! `State::rt.handle.update`.

use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::mpsc::unbounded;
use rune::Tx;

use super::composer::{Composer, Live, Target};
use super::coordinator::{Coordinator, Effect, Input, Mode, Purpose, TalkPhase};
use super::engine::{
    PrivacyPane, SpeechSynthesizer, SpeechToText, SttConfig, SttEvent, SttFailure, SttSink,
};
use super::mic::{Mic, Start};
use super::turn::{Answer, PendingTurn, Submission};
use super::{Notice, wake};
use crate::state::State;

const TICK: Duration = Duration::from_millis(200);
const SPEECH_POLL: Duration = Duration::from_millis(250);
/// The wake recognizer restarts past this much text, keeping it light.
const WAKE_TEXT_LIMIT: usize = 240;
/// A voice that never starts speaking counts as finished after this.
const SPEECH_GRACE: Duration = Duration::from_secs(2);

pub(super) struct Engine {
    pub coord: Coordinator,
    /// The engine behind its session gate.
    mic: Mic,
    tts: Box<dyn SpeechSynthesizer>,
    sink: SttSink,
    /// The talk-mode message waiting for its reply.
    pub turn: PendingTurn,
    /// The composer talk mode just pressed Enter in, and the claim it must
    /// take (`take_submission`).
    pub submitting: Option<(Composer, Submission)>,
    /// The recognizer's latest full transcript.
    heard: String,
    live: Option<Live>,
    /// Where dictation goes: set by the mic button or resolved on start.
    pub target: Option<Target>,
    /// The next coordinator tick, while the mode needs one.
    next_tick: Option<Instant>,
    /// The end-of-speech poll, while a reply is spoken.
    speech: Option<SpeechPoll>,
    hotkey: Option<HotkeyArm>,
    /// Bumped on every (re)arm: events from an older watcher are dropped.
    hotkey_gen: u64,
}

struct SpeechPoll {
    started: Instant,
    seen: bool,
    next: Instant,
}

struct HotkeyArm {
    #[cfg(target_os = "macos")]
    _watcher: super::ptt::Watcher,
}

impl Engine {
    /// The real engines, with a thread that lands recognizer events.
    pub fn new(s: &State) -> Self {
        let (sink, mut events) = unbounded::<(u64, SttEvent)>();
        let handle = s.rt.handle.clone();
        let spawned = std::thread::Builder::new()
            .name("voice-stt-events".into())
            .spawn(move || {
                while let Some((session, event)) = futures::executor::block_on(events.next()) {
                    handle.update(move |s, _| on_stt(s, session, event));
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "voice: the recognizer event thread did not start");
        }
        #[cfg(target_os = "macos")]
        let (stt, tts): (Box<dyn SpeechToText>, Box<dyn SpeechSynthesizer>) = (
            Box::<super::apple::AppleStt>::default(),
            Box::<super::apple::AppleTts>::default(),
        );
        #[cfg(not(target_os = "macos"))]
        let (stt, tts): (Box<dyn SpeechToText>, Box<dyn SpeechSynthesizer>) =
            (Box::new(Unsupported), Box::new(Unsupported));
        Self::with(s, stt, tts, sink)
    }

    /// No microphone and no speaker: fixture scenes.
    pub fn silent(s: &State) -> Self {
        let (sink, _events) = unbounded::<(u64, SttEvent)>();
        Self::with(s, Box::new(Unsupported), Box::new(Unsupported), sink)
    }

    fn with(
        s: &State,
        stt: Box<dyn SpeechToText>,
        tts: Box<dyn SpeechSynthesizer>,
        sink: SttSink,
    ) -> Self {
        let now = Instant::now();
        Self {
            coord: Coordinator::new(s.voice.prefs.timing(), now),
            mic: Mic::new(stt, now),
            tts,
            sink,
            turn: PendingTurn::default(),
            submitting: None,
            heard: String::new(),
            live: None,
            target: None,
            next_tick: None,
            speech: None,
            hotkey: None,
            hotkey_gen: 0,
        }
    }
}

fn engine(s: &mut State) -> Option<&mut Engine> {
    s.voice.engine.as_mut()
}

/// Feed the coordinator and carry out what it decides.
pub(super) fn send(s: &mut State, input: Input) {
    let Some(e) = engine(s) else { return };
    let fx = e.coord.handle(input, Instant::now());
    for effect in fx {
        run(s, effect);
    }
    follow_mode(s);
}

/// The tick and the speech poll (GPUI's timer tasks).
pub(super) fn poll(s: &mut State, tx: &mut Tx) -> bool {
    let Some(e) = engine(s) else { return false };
    let now = Instant::now();
    let mut changed = false;
    if e.coord.needs_tick() {
        let due = *e.next_tick.get_or_insert(now + TICK);
        if now >= due {
            e.next_tick = Some(now + TICK);
            send(s, Input::Tick);
            changed = true;
        }
    } else {
        e.next_tick = None;
    }
    let Some(e) = engine(s) else { return changed };
    if !e.coord.needs_tick() {
        e.next_tick = None;
    }
    if let Some(poll) = e.speech.as_mut()
        && now >= poll.next
    {
        let speaking = e.tts.is_speaking();
        poll.seen |= speaking;
        let done = !speaking && (poll.seen || poll.started.elapsed() > SPEECH_GRACE);
        if done {
            e.tts.stop();
            e.speech = None;
            send(s, Input::SpeechFinished);
            changed = true;
        } else {
            poll.next = now + SPEECH_POLL;
        }
    }
    let Some(e) = engine(s) else { return changed };
    let next = [e.next_tick, e.speech.as_ref().map(|p| p.next)]
        .into_iter()
        .flatten()
        .min();
    if let Some(next) = next {
        tx.wake_after(next.saturating_duration_since(now));
    }
    changed
}

/// Timers and the composer link follow the mode.
fn follow_mode(s: &mut State) {
    let Some(e) = engine(s) else { return };
    if !matches!(e.coord.mode(), Mode::Talk(_)) {
        e.turn.clear();
        e.submitting = None;
    }
    // Words now go to a composer (dictation start, a barge-in, wake
    // capture): link one and type what was heard so far. A composer that
    // went away (sent draft, closed quick composer) is replaced by the one
    // in front.
    if !(e.coord.routes_text() && e.live.is_none()) {
        return;
    }
    let live_target = e.target.clone().filter(|t| t.alive(s));
    let Some(target) = live_target.or_else(|| resolve(s, false)) else {
        return;
    };
    let text = routed_text(s);
    let Some(e) = engine(s) else { return };
    e.target = Some(target.clone());
    e.live = Some(Live::new(target));
    if !text.is_empty() {
        with_live(s, |live, s| live.partial(&text, s));
    }
}

fn run(s: &mut State, effect: Effect) {
    let Some(e) = engine(s) else { return };
    match effect {
        Effect::StartMic(purpose) => start_mic(s, purpose),
        Effect::StopMic => e.mic.stop(),
        Effect::EndAudio => e.mic.end_audio(),
        Effect::Commit => {
            with_live(s, |live, s| live.commit(s));
            if let Some(e) = engine(s) {
                e.live = None;
            }
        }
        Effect::Discard => {
            with_live(s, |live, s| live.discard(s));
            if let Some(e) = engine(s) {
                e.live = None;
            }
        }
        Effect::Send => send_message(s),
        Effect::Speak(text) => speak(s, &text),
        Effect::StopSpeaking => {
            e.tts.stop();
            e.speech = None;
        }
        Effect::OpenComposer => {
            let target = resolve(s, true);
            if let Some(e) = engine(s) {
                e.target = target;
            }
        }
        Effect::Notice(text) => {
            let pane = PrivacyPane::for_message(&text);
            s.voice.notice = Some(Notice { text, pane });
        }
    }
}

/// Press Enter in the target composer. Runs on the next update, as GPUI's
/// emitted Enter did, so the commit before it has landed and the
/// composer's send handler gets a `Tx`.
fn send_message(s: &mut State) {
    let alive = |s: &State, t: &Target| t.alive(s);
    let target = engine(s)
        .and_then(|e| e.target.clone())
        .filter(|t| alive(s, t));
    let Some(e) = engine(s) else { return };
    if e.coord.mode() != Mode::Talk(TalkPhase::Thinking) {
        if let Some(target) = target {
            s.rt.handle.update(move |s, tx| target.send(s, tx));
        }
        return;
    }
    // Talk mode: the composer handling this Enter takes the claim and
    // reports what the gateway made of the message.
    let sub = e.turn.submit();
    e.submitting = target.as_ref().map(|t| (t.composer.clone(), sub));
    if let Some(target) = target {
        s.rt.handle.update(move |s, tx| target.send(s, tx));
    }
    // Queued after the Enter: an untaken claim means nothing was sent.
    s.rt.handle.update(move |s, _| {
        let Some(e) = engine(s) else { return };
        if e.submitting.as_ref().is_none_or(|(_, x)| *x == sub) {
            e.submitting = None;
            super::submission_refused(s, sub, "The message wasn't sent.");
        }
    });
}

/// Run `f` on the live composer link, if there is one.
fn with_live(s: &mut State, f: impl FnOnce(&mut Live, &mut State)) {
    let Some(mut live) = engine(s).and_then(|e| e.live.take()) else {
        return;
    };
    f(&mut live, s);
    if let Some(e) = engine(s)
        && e.live.is_none()
        && e.coord.routes_text()
    {
        // `f` may have ended the link (commit); only put back a live one.
        e.live = Some(live);
    }
}

/// The composer to type into: the open quick composer, else the open chat
/// or the new-thread screen while the main window is in front; for a key
/// press from another app (`open_quick`) the quick composer, opened for it
/// (GPUI `Workspace::voice_composer`; the choice is
/// `workspace::voice_composer`'s).
fn resolve(s: &mut State, open_quick: bool) -> Option<Target> {
    let composer = crate::workspace::voice_composer(s, open_quick)?;
    let thread = match &composer {
        Composer::Chat(id) => Some(id.clone()),
        Composer::Draft | Composer::Quick => None,
    };
    Some(Target::new(composer, thread)).filter(|t| t.alive(s))
}

/// The heard text that belongs in the composer (after the wake phrase in
/// wake capture).
fn routed_text(s: &State) -> String {
    let Some(e) = s.voice.engine.as_ref() else {
        return String::new();
    };
    match e.coord.mode() {
        Mode::WakeArmed | Mode::WakeCapture => {
            wake::command_after(&s.voice.prefs.wake_phrase, &e.heard).unwrap_or_default()
        }
        _ => e.heard.clone(),
    }
}

fn start_mic(s: &mut State, purpose: Purpose) {
    let alive = s
        .voice
        .engine
        .as_ref()
        .and_then(|e| e.target.as_ref())
        .is_some_and(|t| t.alive(s));
    let Some(e) = engine(s) else { return };
    e.live = None;
    e.heard.clear();
    if matches!(purpose, Purpose::Dictation | Purpose::Talk) && !alive {
        let target = resolve(s, false);
        if let Some(e) = engine(s) {
            e.target = target;
        }
    }
    let config = mic_config(s, purpose);
    let Some(e) = engine(s) else { return };
    let sink = e.sink.clone();
    match e
        .mic
        .start(purpose, super::authorized(), &config, sink, Instant::now())
    {
        Start::Opened => tracing::debug!(?purpose, "voice: mic on"),
        Start::Waiting => {}
        Start::Failed(failure) => fail(s, failure),
        Start::Authorize => authorize_then_open(s),
    }
}

/// The first use asks macOS. The mic opens on a yes only if that start is
/// still current (nothing stopped or replaced it) and the mode still wants
/// that session.
fn authorize_then_open(s: &mut State) {
    #[cfg(target_os = "macos")]
    {
        let handle = s.rt.handle.clone();
        std::thread::spawn(move || {
            let result = futures::executor::block_on(super::apple::authorize());
            handle.update(move |s, _| {
                s.voice.authorized = super::authorized();
                let Some(e) = engine(s) else { return };
                let waiting = e.mic.authorized();
                match (result, waiting) {
                    (Ok(()), Some(purpose)) if e.coord.mic_purpose() == Some(purpose) => {
                        let config = mic_config(s, purpose);
                        let Some(e) = engine(s) else { return };
                        let sink = e.sink.clone();
                        if let Err(failure) = e.mic.open(&config, sink, Instant::now()) {
                            fail(s, failure);
                        }
                    }
                    (Ok(()), _) => {}
                    (Err(failure), Some(_)) => fail(s, failure),
                    // Denied for a start nobody waits for any more: only
                    // say why.
                    (Err(failure), None) => {
                        s.voice.notice = Some(Notice {
                            text: failure.message,
                            pane: failure.pane,
                        });
                    }
                }
            });
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = s;
}

fn mic_config(s: &State, purpose: Purpose) -> SttConfig {
    let prefs = &s.voice.prefs;
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
fn fail(s: &mut State, failure: SttFailure) {
    let pane = failure.pane;
    send(
        s,
        Input::RecognizerEnded {
            error: Some(failure.message.clone()),
            fatal: true,
        },
    );
    s.voice.notice = Some(Notice {
        text: failure.message,
        pane,
    });
}

fn on_stt(s: &mut State, session: u64, event: SttEvent) {
    let Some(e) = engine(s) else { return };
    // Stale sessions are dropped; an end tears the session down here.
    let Some(event) = e.mic.accept(session, event, Instant::now()) else {
        return;
    };
    match event {
        SttEvent::Partial(text) | SttEvent::Final(text)
            if text.is_empty() && e.heard.is_empty() => {}
        SttEvent::Partial(text) => heard(s, text, false),
        SttEvent::Final(text) => heard(s, text, true),
        SttEvent::Ended { error, fatal } => send(s, Input::RecognizerEnded { error, fatal }),
    }
}

fn heard(s: &mut State, text: String, is_final: bool) {
    let phrase = s.voice.prefs.wake_phrase.clone();
    let Some(e) = engine(s) else { return };
    e.heard = text;
    if e.coord.mode() == Mode::WakeListening {
        if wake::find(&phrase, &e.heard).is_some() {
            send(s, Input::WakePhrase);
        } else if e.heard.len() > WAKE_TEXT_LIMIT {
            start_mic(s, Purpose::Wake);
            return;
        }
    }
    let routed = routed_text(s);
    let words = !routed.trim().is_empty();
    if engine(s).is_some_and(|e| e.coord.routes_text()) {
        with_live(s, |live, s| {
            if is_final {
                live.finish(&routed, s);
            } else {
                live.partial(&routed, s);
            }
        });
    }
    send(s, Input::Heard { words });
}

fn speak(s: &mut State, text: &str) {
    let rate = s.voice.prefs.speech_rate;
    let Some(e) = engine(s) else { return };
    if let Err(err) = e.tts.speak(text, rate) {
        s.voice.notice = Some(Notice {
            text: format!("Could not speak the reply: {err}"),
            pane: None,
        });
        send(s, Input::SpeechFinished);
        return;
    }
    // Poll for the end only while speaking (`poll`).
    let now = Instant::now();
    e.speech = Some(SpeechPoll {
        started: now,
        seen: false,
        next: now + SPEECH_POLL,
    });
}

/// Speak a talk-mode answer unless the turn moved on since it was found.
pub(super) fn deliver(s: &mut State, answer: Answer) {
    let Some(e) = engine(s) else { return };
    if !e.turn.current(answer.token) || e.coord.mode() != Mode::Talk(TalkPhase::Thinking) {
        return;
    }
    e.turn.clear();
    send(s, Input::ReplyFinished(answer.spoken));
}

/// (Re)arm the push-to-talk watcher to match the preferences.
pub(super) fn arm_hotkey(s: &mut State) {
    let prefs = s.voice.prefs.clone();
    #[cfg(target_os = "macos")]
    let handle = s.rt.handle.clone();
    // The old watcher may be mid-hold: no release will come from it, so
    // end that take before dropping it.
    if engine(s).is_some_and(|e| e.hotkey.is_some()) {
        send(s, Input::HotkeyLost);
    }
    let Some(e) = engine(s) else { return };
    e.hotkey = None;
    e.hotkey_gen += 1;
    if !prefs.hotkey_enabled {
        return;
    }
    #[cfg(target_os = "macos")]
    match super::ptt::Watcher::new(&prefs.hotkey) {
        Ok((watcher, mut events)) => {
            let generation = e.hotkey_gen;
            let spawned = std::thread::Builder::new()
                .name("voice-ptt-events".into())
                .spawn(move || {
                    while let Some(event) = futures::executor::block_on(events.next()) {
                        handle.update(move |s, _| on_hotkey(s, generation, event));
                    }
                    // The watcher thread stopped: a held take has no release.
                    handle.update(move |s, _| {
                        if engine(s).is_some_and(|e| e.hotkey_gen == generation) {
                            send(s, Input::HotkeyLost);
                        }
                    });
                });
            match spawned {
                Ok(_) => {
                    e.hotkey = Some(HotkeyArm { _watcher: watcher });
                }
                Err(err) => {
                    s.voice.notice = Some(Notice {
                        text: format!("The push-to-talk watcher did not start ({err})."),
                        pane: None,
                    });
                }
            }
        }
        Err(text) => {
            s.voice.notice = Some(Notice { text, pane: None });
        }
    }
}

#[cfg(target_os = "macos")]
fn on_hotkey(s: &mut State, generation: u64, event: super::ptt::PttEvent) {
    use super::ptt::PttEvent;
    if !engine(s).is_some_and(|e| e.hotkey_gen == generation) {
        return;
    }
    let input = match event {
        PttEvent::Start => Input::HotkeyDown,
        PttEvent::Stop => Input::HotkeyUp,
        PttEvent::Cancel => Input::HotkeyCancel,
    };
    if input == Input::HotkeyDown
        && let Some(e) = engine(s)
        && e.coord.starts_dictation()
    {
        // Aim at whatever is in front now.
        e.target = None;
        let target = resolve(s, true);
        if let Some(e) = engine(s) {
            e.target = target;
        }
    }
    send(s, input);
}

/// Ask macOS for the microphone and speech recognition now (Settings).
pub(super) fn request_access(s: &mut State) {
    #[cfg(target_os = "macos")]
    {
        let handle = s.rt.handle.clone();
        std::thread::spawn(move || {
            let result = futures::executor::block_on(super::apple::authorize());
            handle.update(move |s, _| {
                s.voice.authorized = super::authorized();
                if let Err(failure) = result {
                    if let Some(pane) = failure.pane {
                        super::open_settings(pane);
                    }
                    s.voice.notice = Some(Notice {
                        text: failure.message,
                        pane: failure.pane,
                    });
                }
            });
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = s;
}

/// The engine where there is none: other platforms, and fixture scenes.
struct Unsupported;

impl SpeechToText for Unsupported {
    fn start(&mut self, _: u64, _: &SttConfig, _: SttSink) -> Result<(), SttFailure> {
        Err(SttFailure {
            message: "Voice needs macOS.".into(),
            pane: None,
        })
    }
    fn end_audio(&mut self) {}
    fn stop(&mut self) {}
}

impl SpeechSynthesizer for Unsupported {
    fn speak(&mut self, _: &str, _: f32) -> Result<(), String> {
        Err("Voice needs macOS.".into())
    }
    fn stop(&mut self) {}
    fn is_speaking(&self) -> bool {
        false
    }
}
