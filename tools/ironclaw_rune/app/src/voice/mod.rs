//! Voice: dictation, spoken replies, talk mode and a wake word.
//!
//! - Push-to-talk: hold a lone modifier (Right Option by default) or click
//!   the composer's mic. Words appear in the composer as they are heard:
//!   settled words as text, the still-changing tail as the field's IME
//!   preedit (`composer.rs`). Release keeps the text; Settings can make
//!   release send it.
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
//!
//! Rune port of the GPUI app's `voice/mod.rs`. The GPUI globals became
//! [`VoiceState`] in `State::voice`; the effect runner and the engine glue
//! live in `session.rs`, the composer controls in `ui.rs` and Settings →
//! Voice in `settings.rs`. Background threads (recognizer events, the
//! push-to-talk watcher, permission prompts) land through
//! `State::rt.handle.update`; timers run from [`poll`] with `Tx::wake_after`.

#[cfg(target_os = "macos")]
mod apple;
mod composer;
mod coordinator;
mod engine;
mod mic;
mod ptt;
mod session;
mod settings;
mod speakable;
mod transcript;
mod turn;
pub mod ui;
mod wake;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use rune::{Cx, El, TextEdit, Tx};
use rune_kit::Kit;
use serde::{Deserialize, Serialize};
use zlo_client::transcript::Row;

pub use self::composer::Composer;
use self::composer::Target;
use self::coordinator::{Input, Mode, TalkPhase, Timing};
pub(crate) use self::engine::PrivacyPane;
pub(crate) use self::ptt::KEYS as HOTKEYS;
use self::session::Engine;
pub(crate) use self::turn::Submission;
use crate::state::State;

/// Voice preferences, saved as `voice.json` next to `prefs.json` (the same
/// file and format as the GPUI app).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoicePrefs {
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
            hotkey_enabled: true,
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

    /// Save off the UI thread.
    fn save(&self) {
        let Some(file) = Self::file() else { return };
        match serde_json::to_vec_pretty(self) {
            Ok(json) => crate::persist::global().write(file, json),
            Err(e) => tracing::warn!("could not encode voice preferences: {e}"),
        }
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
}

fn wake_on(prefs: &VoicePrefs) -> bool {
    prefs.wake_enabled && wake::valid(&prefs.wake_phrase)
}

/// Something to tell the person, with the Settings pane that fixes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub text: String,
    pub pane: Option<PrivacyPane>,
}

/// The `State::voice` slot.
pub struct VoiceState {
    pub prefs: VoicePrefs,
    /// The mic, speaker and coordinator; `None` before [`start`] (and in
    /// headless scenes that do not build one).
    engine: Option<Engine>,
    pub notice: Option<Notice>,
    /// Settings → Voice: the wake phrase field (Enter saves).
    pub wake_phrase: TextEdit,
    /// Where the mic's right-click menu opened (read aloud per thread).
    pub mic_menu: Option<rune::PointerPress>,
    /// Both permissions granted, as last checked (Settings shows it).
    pub authorized: bool,
    /// Headless scenes only (`s.fixture`): Settings → Voice draws the
    /// composer controls and these SnapShot chips above the page, since the
    /// composers themselves belong to other areas.
    pub fixture_preview: Option<FixturePreview>,
}

/// What [`VoiceState::fixture_preview`] shows.
pub struct FixturePreview {
    pub composer: Composer,
    pub staged: Vec<zlo_client::attachments::StagedFile>,
}

impl Default for VoiceState {
    fn default() -> Self {
        let prefs = VoicePrefs::default();
        Self {
            wake_phrase: TextEdit::new(prefs.wake_phrase.clone()),
            prefs,
            engine: None,
            notice: None,
            mic_menu: None,
            authorized: false,
            fixture_preview: None,
        }
    }
}

/// Load the preferences and arm what they turn on. Called once by the
/// workspace when it starts (area B: `workspace::start`).
pub fn start(s: &mut State) {
    let prefs = VoicePrefs::load();
    s.voice.wake_phrase = TextEdit::new(prefs.wake_phrase.clone());
    s.voice.prefs = prefs;
    s.voice.authorized = authorized();
    if rune::is_headless() || s.voice.engine.is_some() {
        // Scenes and scripts have no microphone, key watcher or speaker.
        return;
    }
    s.voice.engine = Some(session::Engine::new(s));
    session::arm_hotkey(s);
    let auto_send = s.voice.prefs.auto_send;
    session::send(s, Input::SetAutoSend(auto_send));
    if wake_on(&s.voice.prefs) {
        session::send(s, Input::SetWake(true));
    }
}

/// Timers: the coordinator's tick while a mode needs it, and the speech
/// poll while a reply is spoken. Nothing runs (or wakes) otherwise.
pub fn poll(s: &mut State, tx: &mut Tx) -> bool {
    session::poll(s, tx)
}

/// Edit the preferences, save them off the UI thread and re-arm what
/// changed (GPUI `VoicePrefs::update`).
pub fn update_prefs(s: &mut State, edit: impl FnOnce(&mut VoicePrefs)) {
    let old = s.voice.prefs.clone();
    edit(&mut s.voice.prefs);
    if s.voice.prefs == old {
        return;
    }
    s.voice.prefs.save();
    let prefs = s.voice.prefs.clone();
    let Some(engine) = s.voice.engine.as_mut() else {
        return;
    };
    engine.coord.set_timing(prefs.timing());
    if old.hotkey_enabled != prefs.hotkey_enabled || old.hotkey != prefs.hotkey {
        session::arm_hotkey(s);
    }
    session::send(s, Input::SetAutoSend(prefs.auto_send));
    let wake = wake_on(&prefs);
    if wake != wake_on(&old) || old.wake_phrase != prefs.wake_phrase {
        session::send(s, Input::SetWake(false));
        session::send(s, Input::SetWake(wake));
    }
}

/// The voice's state, for the composer pill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Off,
    Dictating,
    Listening,
    Thinking,
    Speaking,
    /// Wake word armed, nothing else going on.
    Waiting,
}

pub fn status(s: &State) -> Status {
    let Some(e) = s.voice.engine.as_ref() else {
        return Status::Off;
    };
    match e.coord.mode() {
        Mode::Dictating(_) | Mode::Finishing { .. } => Status::Dictating,
        Mode::Talk(TalkPhase::Listening) | Mode::WakeCapture | Mode::WakeArmed => Status::Listening,
        Mode::Talk(TalkPhase::Thinking) => Status::Thinking,
        Mode::Talk(TalkPhase::Speaking) => Status::Speaking,
        Mode::Idle | Mode::WakeListening if e.coord.speaking() => Status::Speaking,
        Mode::WakeListening => Status::Waiting,
        Mode::Idle => Status::Off,
    }
}

pub fn talk_mode(s: &State) -> bool {
    s.voice
        .engine
        .as_ref()
        .is_some_and(|e| matches!(e.coord.mode(), Mode::Talk(_)))
}

/// Whether `composer` is the one voice is typing into.
pub fn targets(s: &State, composer: &Composer) -> bool {
    s.voice
        .engine
        .as_ref()
        .and_then(|e| e.target.as_ref())
        .is_some_and(|t| t.composer == *composer)
}

pub fn dismiss_notice(s: &mut State) {
    s.voice.notice = None;
}

/// The mic button: start or stop dictating into `composer`.
pub fn toggle_dictation(s: &mut State, composer: Composer, thread: Option<String>) {
    // A click that starts a dictation (also from wake listening, where the
    // mic is already on) types into the clicked composer.
    if let Some(e) = s.voice.engine.as_mut() {
        let starts = e.coord.starts_dictation();
        aim(&mut e.target, Target::new(composer, thread), starts);
    }
    session::send(s, Input::MicClicked);
}

/// Talk mode on or off, aimed at `composer` (or the front composer).
pub fn toggle_talk(s: &mut State, composer: Option<Composer>, thread: Option<String>) {
    if let (Some(composer), Some(e)) = (composer, s.voice.engine.as_mut())
        && !matches!(e.coord.mode(), Mode::Talk(_))
    {
        e.target = Some(Target::new(composer, thread));
    }
    session::send(s, Input::TalkToggled);
}

/// The pill's ✕ and Escape: leave talk mode, end dictation, stop speaking.
pub fn exit(s: &mut State) {
    session::send(s, Input::Exit);
}

pub fn stop_speaking(s: &mut State) {
    session::send(s, Input::StopSpeaking);
}

/// Flip read-aloud for one thread.
pub fn toggle_read_aloud(s: &mut State, thread: &str) {
    let thread = thread.to_string();
    update_prefs(s, move |p| {
        let on = !p.reads(&thread);
        if on == p.read_aloud {
            p.read_aloud_threads.remove(&thread);
        } else {
            p.read_aloud_threads.insert(thread, on);
        }
    });
}

/// Point dictation at `chosen` when the click starts one (from idle or
/// any wake state, where the mic may already be on); a click that stops a
/// dictation leaves the target alone.
fn aim<T>(slot: &mut Option<T>, chosen: T, starts_dictation: bool) {
    if starts_dictation {
        *slot = Some(chosen);
    }
}

/// The composer that handles an Enter takes talk mode's claim on it, if
/// talk mode pressed it there, and carries it through the send. Area A
/// (chat), and area B (new thread, quick composer) call this from their
/// send handlers.
pub fn take_submission(s: &mut State, composer: &Composer) -> Option<Submission> {
    let e = s.voice.engine.as_mut()?;
    let (id, sub) = e.submitting.as_ref()?;
    if id != composer {
        return None;
    }
    let sub = *sub;
    e.submitting = None;
    Some(sub)
}

/// The gateway took talk mode's message into `thread` as `run` (when the
/// ack names it). Talk mode listens in this thread's composer next: a
/// new-thread or quick composer is gone after the send.
pub fn submission_accepted(
    s: &mut State,
    sub: Submission,
    thread: &str,
    run: Option<String>,
    composer: Composer,
) {
    bound(s, sub, thread, composer, |turn| {
        turn.accepted(sub, thread, run)
    });
}

/// Talk mode's message waits in `thread`'s queue behind `behind`.
pub fn submission_queued(
    s: &mut State,
    sub: Submission,
    thread: &str,
    behind: Option<String>,
    composer: Composer,
) {
    bound(s, sub, thread, composer, |turn| {
        turn.queued(sub, thread, behind)
    });
}

fn bound(
    s: &mut State,
    sub: Submission,
    thread: &str,
    composer: Composer,
    bind: impl FnOnce(&mut turn::PendingTurn) -> Option<turn::Answer>,
) {
    let alive = |s: &State, t: &Target| t.thread.is_some() && t.alive(s);
    let Some(e) = s.voice.engine.as_mut() else {
        return;
    };
    if !e.turn.waits_for(sub) {
        return;
    }
    let early = bind(&mut e.turn);
    let keep = s
        .voice
        .engine
        .as_ref()
        .and_then(|e| e.target.as_ref())
        .is_some_and(|t| alive(s, t));
    if !keep && let Some(e) = s.voice.engine.as_mut() {
        e.target = Some(Target::new(composer, Some(thread.to_string())));
    }
    if let Some(answer) = early {
        // Deferred like the GPUI app: the composer finishes its send first.
        s.rt.handle.update(move |s, _| session::deliver(s, answer));
    }
}

/// Talk mode's message was not sent: say why and leave talk mode.
pub fn submission_refused(s: &mut State, sub: Submission, why: &str) {
    if let Some(e) = s.voice.engine.as_mut()
        && e.turn.refused(sub)
    {
        let why = why.to_string();
        s.rt.handle
            .update(move |s, _| session::send(s, Input::SendRefused(why)));
    }
}

/// A chat's run `run` ended in `thread` with `rows` on screen. Talk mode
/// speaks it only when it answers its own message; otherwise the thread's
/// read-aloud choice decides. Area A calls this when a run ends.
pub fn run_ended(s: &mut State, thread: &str, run: Option<&str>, completed: bool, rows: &[Row]) {
    let Some(e) = s.voice.engine.as_ref() else {
        return;
    };
    let thinking = e.coord.mode() == Mode::Talk(TalkPhase::Thinking);
    let reads = completed && s.voice.prefs.reads(thread);
    if !thinking && !reads {
        return;
    }
    let spoken = if completed {
        last_reply(rows)
            .map(speakable::speakable)
            .unwrap_or_default()
    } else {
        "That run failed. See the screen.".to_string()
    };
    if thinking
        && let Some(e) = s.voice.engine.as_mut()
        && let Some(answer) = e.turn.run_ended(thread, run, spoken.clone())
    {
        s.rt.handle.update(move |s, _| session::deliver(s, answer));
        return;
    }
    if reads {
        s.rt.handle
            .update(move |s, _| session::send(s, Input::ReadAloud(spoken)));
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

/// Both voice permissions granted (no prompt needed).
pub fn authorized() -> bool {
    #[cfg(target_os = "macos")]
    return apple::authorized();
    #[cfg(not(target_os = "macos"))]
    false
}

/// Ask macOS for the microphone and speech recognition now (Settings).
pub fn request_access(s: &mut State) {
    session::request_access(s);
}

/// Open a privacy pane in System Settings.
pub fn open_settings(pane: PrivacyPane) {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(pane.url())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Settings → Voice became visible: show the current permission state and
/// the saved wake phrase.
pub fn settings_opened(s: &mut State) {
    s.voice.authorized = authorized();
    if !s.voice.wake_phrase.focused {
        s.voice.wake_phrase = TextEdit::new(s.voice.prefs.wake_phrase.clone());
    }
}

/// Settings → Voice (GPUI `pages/settings/voice.rs`).
pub fn settings_view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    settings::view(s, cx, k, layer)
}

/// Fixture voice states for headless scenes (`scenes.rs`).
pub mod fixtures {
    use super::*;

    /// A voice with a coordinator but no microphone or speaker, put into
    /// the mode `inputs` lead to (their effects are not carried out).
    pub fn with_status(s: &mut State, inputs: &[&str], target: Option<Composer>) {
        let mut engine = Engine::silent(s);
        let mut at = std::time::Instant::now();
        for input in inputs {
            let input = match *input {
                "talk" => Input::TalkToggled,
                "click" => Input::MicClicked,
                "words" => Input::Heard { words: true },
                "tick" => Input::Tick,
                "reply" => Input::ReplyFinished("Here it is.".into()),
                "read" => Input::ReadAloud("Here it is.".into()),
                "wake" => Input::SetWake(true),
                _ => continue,
            };
            // Spaced so ticks pass their timers.
            at += Duration::from_secs(3);
            engine.coord.handle(input, at);
        }
        engine.target = target.map(|c| {
            let thread = match &c {
                Composer::Chat(id) => Some(id.clone()),
                _ => None,
            };
            Target::new(c, thread)
        });
        s.voice.engine = Some(engine);
    }

    /// Show the composer controls (and `staged` chips) above Settings → Voice.
    pub fn preview(
        s: &mut State,
        composer: Composer,
        staged: Vec<zlo_client::attachments::StagedFile>,
    ) {
        s.voice.fixture_preview = Some(FixturePreview { composer, staged });
    }

    pub fn notice(s: &mut State, text: &str, pane: Option<PrivacyPane>) {
        s.voice.notice = Some(Notice {
            text: text.into(),
            pane,
        });
    }

    /// Talk mode pressed Enter in `composer`: the claim its send handler
    /// takes (`take_submission`).
    #[cfg(test)]
    pub fn claim(s: &mut State, composer: Composer) -> Submission {
        let mut e = match s.voice.engine.take() {
            Some(e) => e,
            None => Engine::silent(s),
        };
        let sub = e.turn.submit();
        e.submitting = Some((composer, sub));
        s.voice.engine = Some(e);
        sub
    }

    /// Talk mode still waits on `sub` (it was neither refused nor answered).
    #[cfg(test)]
    pub fn waits_for(s: &State, sub: Submission) -> bool {
        s.voice
            .engine
            .as_ref()
            .is_some_and(|e| e.turn.waits_for(sub))
    }

    pub fn dictation_off() -> (&'static str, Option<PrivacyPane>) {
        (engine::DICTATION_OFF, Some(PrivacyPane::Dictation))
    }
}

#[cfg(test)]
mod tests {
    use super::coordinator::Coordinator;
    use super::*;
    use std::time::Instant;

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
    fn a_click_from_wake_listening_aims_at_the_clicked_composer() {
        let now = Instant::now();
        let mut c = Coordinator::new(Timing::default(), now);
        let mut target = Some("old composer");
        c.handle(Input::SetWake(true), now);
        assert!(c.mic_on(), "the wake word holds the mic");
        aim(&mut target, "clicked", c.starts_dictation());
        assert_eq!(target, Some("clicked"));
        c.handle(Input::MicClicked, now);
        // The second click stops that dictation; the target stays put.
        aim(&mut target, "another", c.starts_dictation());
        assert_eq!(target, Some("clicked"));
    }

    #[test]
    fn dictation_off_points_at_the_keyboard_pane() {
        assert_eq!(
            PrivacyPane::for_message(engine::DICTATION_OFF),
            Some(PrivacyPane::Dictation)
        );
        assert_eq!(PrivacyPane::for_message("Microphone access is off."), None);
    }

    #[test]
    fn defaults_turn_on_only_push_to_talk() {
        let p = VoicePrefs::default();
        assert!(p.hotkey_enabled && !p.wake_enabled && !p.read_aloud && !p.auto_send);
        assert!(p.on_device_only);
        assert_eq!(p.hotkey, "right_alt");
        assert!(!wake_on(&p));
    }
}
