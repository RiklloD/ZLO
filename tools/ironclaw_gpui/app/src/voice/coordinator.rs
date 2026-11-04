//! One owner for the microphone and the speaker.
//!
//! Push-to-talk, the mic button, talk mode and the wake word all want the
//! same microphone, and spoken replies must stop the moment the person
//! talks. [`Coordinator`] is the single state machine that decides: inputs
//! (key held, words heard, reply finished, a timer tick) go in, effects
//! (start the mic, commit the text, send, speak) come out. It does no I/O,
//! so every transition is unit-tested; `voice/mod.rs` carries the effects
//! out against the real recognizer, synthesizer and composer.
//!
//! Battery: the mic runs only in a mode the person started (or the wake
//! word they turned on), talk mode ends itself after [`Timing::talk_idle`]
//! of quiet, and the tick only runs while a mode needs it
//! ([`Coordinator::needs_tick`]).

use std::time::{Duration, Instant};

/// What started a dictation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// Held push-to-talk key; release ends it.
    Hotkey,
    /// The composer's mic button; a second click ends it.
    Button,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TalkPhase {
    Listening,
    /// Sent; waiting for the reply.
    Thinking,
    Speaking,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Idle,
    Dictating(Trigger),
    /// Released: the recognizer finalizes, then the text is committed.
    Finishing {
        send: bool,
    },
    Talk(TalkPhase),
    /// The wake recognizer runs, listening only for the phrase.
    WakeListening,
    /// The phrase was heard; capture starts after a short pause.
    WakeArmed,
    /// Capturing the command that followed the phrase.
    WakeCapture,
}

/// Why the mic is on, which decides how the recognizer is set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Dictation,
    Talk,
    /// Listening while a reply is spoken, so speech interrupts it (echo
    /// cancellation on).
    BargeIn,
    Wake,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Effect {
    /// (Re)start the mic with a fresh transcript.
    StartMic(Purpose),
    StopMic,
    /// Stop recording but let the recognizer deliver its final result.
    EndAudio,
    /// Keep the volatile text in the composer as typed text.
    Commit,
    /// Remove the volatile text from the composer.
    Discard,
    /// Send the composer's message.
    Send,
    Speak(String),
    StopSpeaking,
    /// Wake word: bring up the quick composer before text lands in it.
    OpenComposer,
    /// Tell the person something (shown in the voice pill).
    Notice(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Input {
    HotkeyDown,
    HotkeyUp,
    /// The key turned out to be part of a chord (Option+E): drop the take.
    HotkeyCancel,
    MicClicked,
    TalkToggled,
    /// Escape, the pill's ✕, or the Stop button.
    Exit,
    SetWake(bool),
    SetAutoSend(bool),
    /// The transcript changed; `words` when it has any.
    Heard {
        words: bool,
    },
    /// The wake phrase showed up in the wake transcript.
    WakePhrase,
    /// The recognizer stopped on its own: final result, silence or error.
    /// `fatal` errors (permission, recognizer missing) stop every mode.
    RecognizerEnded {
        error: Option<String>,
        fatal: bool,
    },
    /// A reply finished; its spoken form (may be empty).
    ReplyFinished(String),
    /// Read a reply aloud outside talk mode.
    ReadAloud(String),
    SpeechFinished,
    /// Stop the voice without leaving the mode.
    StopSpeaking,
    Tick,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// Quiet after words that ends an utterance (talk mode, wake capture).
    pub silence: Duration,
    /// How long a released dictation waits for the final result.
    pub finish_grace: Duration,
    /// Quiet after the wake phrase before capture starts.
    pub wake_pause: Duration,
    /// Wake capture with no command words gives up after this.
    pub capture_idle: Duration,
    /// Hard stop for one wake capture.
    pub capture_max: Duration,
    /// Talk mode with nothing said ends itself after this.
    pub talk_idle: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            silence: Duration::from_millis(2000),
            finish_grace: Duration::from_millis(700),
            wake_pause: Duration::from_millis(600),
            capture_idle: Duration::from_secs(6),
            capture_max: Duration::from_secs(20),
            talk_idle: Duration::from_secs(120),
        }
    }
}

/// Restarts allowed in a row before a continuous mode gives up.
const MAX_FAILURES: u32 = 3;

pub(crate) struct Coordinator {
    mode: Mode,
    /// When the current mode (or mic session) began.
    since: Instant,
    last_heard: Instant,
    /// The current utterance has words.
    words: bool,
    speaking: bool,
    wake: bool,
    auto_send: bool,
    failures: u32,
    timing: Timing,
}

impl Coordinator {
    pub fn new(timing: Timing, now: Instant) -> Self {
        Self {
            mode: Mode::Idle,
            since: now,
            last_heard: now,
            words: false,
            speaking: false,
            wake: false,
            auto_send: false,
            failures: 0,
            timing,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn speaking(&self) -> bool {
        self.speaking
    }

    pub fn set_timing(&mut self, timing: Timing) {
        self.timing = timing;
    }

    /// Whether transcripts go into the composer now.
    pub fn routes_text(&self) -> bool {
        matches!(
            self.mode,
            Mode::Dictating(_) | Mode::Finishing { .. } | Mode::Talk(TalkPhase::Listening)
        ) || self.mode == Mode::WakeCapture
    }

    /// Whether the mode needs [`Input::Tick`] (a timer only runs then).
    pub fn needs_tick(&self) -> bool {
        matches!(
            self.mode,
            Mode::Finishing { .. }
                | Mode::Talk(TalkPhase::Listening)
                | Mode::WakeArmed
                | Mode::WakeCapture
        )
    }

    /// Whether the mic is on in this mode.
    pub fn mic_on(&self) -> bool {
        !matches!(self.mode, Mode::Idle | Mode::Talk(TalkPhase::Thinking))
    }

    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        use Effect as E;
        use Mode as M;
        let mut fx = Vec::new();
        match input {
            Input::HotkeyDown => match self.mode {
                M::Idle | M::WakeListening | M::WakeArmed => {
                    self.dictate(Trigger::Hotkey, &mut fx, now)
                }
                M::WakeCapture => {
                    fx.push(E::Commit);
                    self.dictate(Trigger::Hotkey, &mut fx, now);
                }
                // A key press interrupts a spoken reply in talk mode.
                M::Talk(TalkPhase::Speaking) => {
                    self.hush(&mut fx);
                    self.enter(M::Talk(TalkPhase::Listening), now);
                    self.start_mic(Purpose::Talk, &mut fx, now);
                }
                _ => {}
            },
            Input::HotkeyUp => {
                if self.mode == M::Dictating(Trigger::Hotkey) {
                    fx.push(E::EndAudio);
                    let send = self.auto_send;
                    self.enter(M::Finishing { send }, now);
                }
            }
            Input::HotkeyCancel => {
                if self.mode == M::Dictating(Trigger::Hotkey) {
                    fx.extend([E::StopMic, E::Discard]);
                    self.rest(&mut fx, now);
                }
            }
            Input::MicClicked => match self.mode {
                M::Idle | M::WakeListening | M::WakeArmed => {
                    self.dictate(Trigger::Button, &mut fx, now)
                }
                M::WakeCapture => {
                    fx.push(E::Commit);
                    self.dictate(Trigger::Button, &mut fx, now);
                }
                // Clicking stops; the text waits for Enter.
                M::Dictating(_) => {
                    fx.push(E::EndAudio);
                    self.enter(M::Finishing { send: false }, now);
                }
                M::Talk(_) => self.exit_talk(&mut fx, now),
                M::Finishing { .. } => {}
            },
            Input::TalkToggled => match self.mode {
                M::Talk(_) => self.exit_talk(&mut fx, now),
                _ => {
                    self.hush(&mut fx);
                    if self.routes_text() {
                        fx.push(E::Commit);
                    }
                    self.failures = 0;
                    self.enter(M::Talk(TalkPhase::Listening), now);
                    self.start_mic(Purpose::Talk, &mut fx, now);
                }
            },
            Input::Exit => match self.mode {
                M::Talk(_) => self.exit_talk(&mut fx, now),
                M::Dictating(_) | M::Finishing { .. } => {
                    fx.extend([E::StopMic, E::Commit]);
                    self.rest(&mut fx, now);
                }
                M::WakeArmed | M::WakeCapture => {
                    fx.extend([E::StopMic, E::Discard]);
                    self.rest(&mut fx, now);
                }
                M::Idle | M::WakeListening => self.hush(&mut fx),
            },
            Input::SetWake(on) => {
                self.wake = on;
                match self.mode {
                    M::Idle if on => {
                        self.failures = 0;
                        self.rest(&mut fx, now);
                    }
                    M::WakeListening | M::WakeArmed | M::WakeCapture if !on => {
                        fx.extend([E::StopMic, E::Discard]);
                        self.enter(M::Idle, now);
                    }
                    _ => {}
                }
            }
            Input::SetAutoSend(on) => self.auto_send = on,
            Input::Heard { words } => {
                self.last_heard = now;
                self.words = words;
                if words {
                    self.failures = 0;
                }
                // Speaking over a reply interrupts it; the words already
                // heard start the next message.
                if words && self.mode == M::Talk(TalkPhase::Speaking) {
                    self.hush(&mut fx);
                    self.enter(M::Talk(TalkPhase::Listening), now);
                }
            }
            Input::WakePhrase => {
                if self.mode == M::WakeListening {
                    self.last_heard = now;
                    self.enter(M::WakeArmed, now);
                }
            }
            Input::RecognizerEnded { error, fatal } => {
                if fatal {
                    if self.mic_on() || matches!(self.mode, M::Talk(_)) {
                        fx.extend([E::StopMic, E::Commit]);
                    }
                    self.hush(&mut fx);
                    self.wake = false;
                    self.enter(M::Idle, now);
                    if let Some(e) = error {
                        fx.push(E::Notice(e));
                    }
                    return fx;
                }
                self.recognizer_ended(error, &mut fx, now);
            }
            Input::ReplyFinished(text) => {
                if self.mode == M::Talk(TalkPhase::Thinking) {
                    if text.trim().is_empty() {
                        self.enter(M::Talk(TalkPhase::Listening), now);
                        self.start_mic(Purpose::Talk, &mut fx, now);
                    } else {
                        fx.push(E::Speak(text));
                        self.speaking = true;
                        self.enter(M::Talk(TalkPhase::Speaking), now);
                        self.start_mic(Purpose::BargeIn, &mut fx, now);
                    }
                }
            }
            Input::ReadAloud(text) => {
                // Never over the person's own voice.
                if matches!(self.mode, M::Idle | M::WakeListening) && !text.trim().is_empty() {
                    if self.speaking {
                        fx.push(E::StopSpeaking);
                    }
                    fx.push(E::Speak(text));
                    self.speaking = true;
                }
            }
            Input::SpeechFinished => {
                self.speaking = false;
                if self.mode == M::Talk(TalkPhase::Speaking) {
                    self.enter(M::Talk(TalkPhase::Listening), now);
                    self.start_mic(Purpose::Talk, &mut fx, now);
                }
            }
            Input::StopSpeaking => {
                if self.speaking {
                    self.hush(&mut fx);
                    if self.mode == M::Talk(TalkPhase::Speaking) {
                        self.enter(M::Talk(TalkPhase::Listening), now);
                        self.start_mic(Purpose::Talk, &mut fx, now);
                    }
                }
            }
            Input::Tick => self.tick(&mut fx, now),
        }
        fx
    }

    fn tick(&mut self, fx: &mut Vec<Effect>, now: Instant) {
        use Effect as E;
        let in_mode = now.saturating_duration_since(self.since);
        let quiet = now.saturating_duration_since(self.last_heard);
        let t = self.timing;
        match self.mode {
            Mode::Finishing { send } if in_mode >= t.finish_grace => {
                self.finalize(send, fx, now);
            }
            Mode::Talk(TalkPhase::Listening) => {
                if self.words && quiet >= t.silence {
                    fx.extend([E::StopMic, E::Commit, E::Send]);
                    self.enter(Mode::Talk(TalkPhase::Thinking), now);
                } else if !self.words && in_mode >= t.talk_idle {
                    self.exit_talk(fx, now);
                    fx.push(E::Notice("Talk mode ended after a quiet spell.".into()));
                }
            }
            Mode::WakeArmed => {
                if quiet >= t.wake_pause {
                    fx.push(E::OpenComposer);
                    self.enter(Mode::WakeCapture, now);
                    // Words after the phrase may already be in.
                    self.last_heard = now;
                }
            }
            Mode::WakeCapture => {
                if self.words && quiet >= t.silence {
                    fx.extend([E::StopMic, E::Commit, E::Send]);
                    self.rest(fx, now);
                } else if !self.words && in_mode >= t.capture_idle {
                    fx.extend([E::StopMic, E::Discard]);
                    self.rest(fx, now);
                } else if in_mode >= t.capture_max {
                    fx.extend([E::StopMic, E::Commit]);
                    if self.words {
                        fx.push(E::Send);
                    }
                    self.rest(fx, now);
                }
            }
            _ => {}
        }
    }

    fn recognizer_ended(&mut self, error: Option<String>, fx: &mut Vec<Effect>, now: Instant) {
        use Effect as E;
        match self.mode {
            Mode::Dictating(_) => {
                fx.push(E::Commit);
                self.rest(fx, now);
                fx.extend(error.map(E::Notice));
            }
            Mode::Finishing { send } => self.finalize(send, fx, now),
            Mode::Talk(TalkPhase::Listening) if self.words => {
                fx.extend([E::Commit, E::Send]);
                self.enter(Mode::Talk(TalkPhase::Thinking), now);
            }
            Mode::Talk(TalkPhase::Listening) => match self.give_up(error) {
                None => self.start_mic(Purpose::Talk, fx, now),
                Some(why) => {
                    self.exit_talk(fx, now);
                    fx.push(E::Notice(why));
                }
            },
            // The barge-in mic stopped: keep speaking, listen again.
            Mode::Talk(TalkPhase::Speaking) => match self.give_up(error) {
                None => self.start_mic(Purpose::BargeIn, fx, now),
                Some(why) => fx.push(E::Notice(why)),
            },
            Mode::WakeListening | Mode::WakeArmed => match self.give_up(error) {
                None => {
                    self.enter(Mode::WakeListening, now);
                    self.start_mic(Purpose::Wake, fx, now);
                }
                Some(why) => {
                    self.wake = false;
                    self.enter(Mode::Idle, now);
                    fx.push(E::Notice(why));
                }
            },
            Mode::WakeCapture => {
                if self.words {
                    fx.extend([E::Commit, E::Send]);
                } else {
                    fx.push(E::Discard);
                }
                self.rest(fx, now);
            }
            Mode::Idle | Mode::Talk(TalkPhase::Thinking) => {}
        }
    }

    /// Count a failed session: the notice to show once there were too many
    /// in a row, `None` to try again. An end without an error (the
    /// recognizer's own pause limit) never counts.
    fn give_up(&mut self, error: Option<String>) -> Option<String> {
        let why = error?;
        self.failures += 1;
        (self.failures > MAX_FAILURES).then(|| format!("Voice stopped: {why}"))
    }

    fn dictate(&mut self, trigger: Trigger, fx: &mut Vec<Effect>, now: Instant) {
        self.hush(fx);
        self.enter(Mode::Dictating(trigger), now);
        self.start_mic(Purpose::Dictation, fx, now);
    }

    fn finalize(&mut self, send: bool, fx: &mut Vec<Effect>, now: Instant) {
        fx.extend([Effect::StopMic, Effect::Commit]);
        if send && self.words {
            fx.push(Effect::Send);
        }
        self.rest(fx, now);
    }

    fn exit_talk(&mut self, fx: &mut Vec<Effect>, now: Instant) {
        if self.mode != Mode::Talk(TalkPhase::Thinking) {
            fx.push(Effect::StopMic);
        }
        fx.push(Effect::Discard);
        self.hush(fx);
        self.rest(fx, now);
    }

    /// Stop the voice if it is speaking.
    fn hush(&mut self, fx: &mut Vec<Effect>) {
        if self.speaking {
            fx.push(Effect::StopSpeaking);
            self.speaking = false;
        }
    }

    /// Back to resting: wake listening when it is on, else idle.
    fn rest(&mut self, fx: &mut Vec<Effect>, now: Instant) {
        if self.wake {
            self.enter(Mode::WakeListening, now);
            self.start_mic(Purpose::Wake, fx, now);
        } else {
            self.enter(Mode::Idle, now);
        }
    }

    fn start_mic(&mut self, purpose: Purpose, fx: &mut Vec<Effect>, now: Instant) {
        self.words = false;
        self.last_heard = now;
        self.since = now;
        fx.push(Effect::StartMic(purpose));
    }

    fn enter(&mut self, mode: Mode, now: Instant) {
        self.mode = mode;
        self.since = now;
    }
}

#[cfg(test)]
mod tests {
    use super::Effect::*;
    use super::*;

    struct Clock {
        c: Coordinator,
        now: Instant,
    }

    impl Clock {
        fn new() -> Self {
            let now = Instant::now();
            Self {
                c: Coordinator::new(Timing::default(), now),
                now,
            }
        }
        fn at(&mut self, ms: u64, input: Input) -> Vec<Effect> {
            self.now += Duration::from_millis(ms);
            self.c.handle(input, self.now)
        }
        fn send(&mut self, input: Input) -> Vec<Effect> {
            self.at(0, input)
        }
    }

    fn heard() -> Input {
        Input::Heard { words: true }
    }

    #[test]
    fn push_to_talk_records_while_held_and_commits_after_release() {
        let mut t = Clock::new();
        assert_eq!(
            t.send(Input::HotkeyDown),
            vec![StartMic(Purpose::Dictation)]
        );
        assert!(t.c.routes_text());
        t.at(300, heard());
        assert_eq!(t.at(500, Input::HotkeyUp), vec![EndAudio]);
        assert!(t.c.needs_tick());
        // The final result lands before the grace runs out.
        assert_eq!(
            t.at(
                200,
                Input::RecognizerEnded {
                    error: None,
                    fatal: false
                }
            ),
            vec![StopMic, Commit]
        );
        assert_eq!(t.c.mode(), Mode::Idle);
        assert!(!t.c.needs_tick());
    }

    #[test]
    fn auto_send_sends_on_release_only_when_words_were_heard() {
        let mut t = Clock::new();
        t.send(Input::SetAutoSend(true));
        t.send(Input::HotkeyDown);
        t.at(100, heard());
        t.at(100, Input::HotkeyUp);
        // No final result: the grace period ends it.
        assert_eq!(t.at(100, Input::Tick), vec![]);
        assert_eq!(t.at(700, Input::Tick), vec![StopMic, Commit, Send]);

        t.send(Input::HotkeyDown);
        t.at(100, Input::HotkeyUp);
        assert_eq!(t.at(800, Input::Tick), vec![StopMic, Commit]);
    }

    #[test]
    fn a_chord_drops_the_take() {
        let mut t = Clock::new();
        t.send(Input::HotkeyDown);
        assert_eq!(t.send(Input::HotkeyCancel), vec![StopMic, Discard]);
        assert_eq!(t.c.mode(), Mode::Idle);
    }

    #[test]
    fn mic_button_toggles_and_never_auto_sends() {
        let mut t = Clock::new();
        t.send(Input::SetAutoSend(true));
        assert_eq!(
            t.send(Input::MicClicked),
            vec![StartMic(Purpose::Dictation)]
        );
        t.at(100, heard());
        assert_eq!(t.send(Input::MicClicked), vec![EndAudio]);
        assert_eq!(t.at(800, Input::Tick), vec![StopMic, Commit]);
    }

    #[test]
    fn talking_stops_a_reply_read_aloud() {
        let mut t = Clock::new();
        assert_eq!(
            t.send(Input::ReadAloud("Hi.".into())),
            vec![Speak("Hi.".into())]
        );
        assert!(t.c.speaking());
        assert_eq!(
            t.send(Input::HotkeyDown),
            vec![StopSpeaking, StartMic(Purpose::Dictation)]
        );
        assert!(!t.c.speaking());
    }

    #[test]
    fn read_aloud_waits_for_the_person() {
        let mut t = Clock::new();
        t.send(Input::HotkeyDown);
        assert_eq!(t.send(Input::ReadAloud("Hi.".into())), vec![]);
        assert_eq!(t.send(Input::ReadAloud("".into())), vec![]);
    }

    #[test]
    fn talk_mode_loops_listen_send_speak_listen() {
        let mut t = Clock::new();
        assert_eq!(t.send(Input::TalkToggled), vec![StartMic(Purpose::Talk)]);
        t.at(500, heard());
        t.at(900, heard());
        // 1.9 s of quiet is not yet the end.
        assert_eq!(t.at(1900, Input::Tick), vec![]);
        assert_eq!(t.at(100, Input::Tick), vec![StopMic, Commit, Send]);
        assert_eq!(t.c.mode(), Mode::Talk(TalkPhase::Thinking));
        assert!(!t.c.mic_on());
        assert!(!t.c.needs_tick());
        assert_eq!(
            t.at(3000, Input::ReplyFinished("Sure.".into())),
            vec![Speak("Sure.".into()), StartMic(Purpose::BargeIn)]
        );
        assert_eq!(t.c.mode(), Mode::Talk(TalkPhase::Speaking));
        assert!(!t.c.routes_text());
        assert_eq!(
            t.at(1000, Input::SpeechFinished),
            vec![StartMic(Purpose::Talk)]
        );
        assert_eq!(t.c.mode(), Mode::Talk(TalkPhase::Listening));
    }

    #[test]
    fn speaking_over_a_reply_interrupts_it() {
        let mut t = Clock::new();
        t.send(Input::TalkToggled);
        t.at(100, heard());
        t.at(2000, Input::Tick);
        t.send(Input::ReplyFinished("A long answer.".into()));
        // Silence (no words) while speaking changes nothing.
        assert_eq!(t.at(100, Input::Heard { words: false }), vec![]);
        assert_eq!(t.at(100, heard()), vec![StopSpeaking]);
        assert_eq!(t.c.mode(), Mode::Talk(TalkPhase::Listening));
        assert!(t.c.routes_text());
        // The interruption is the next message.
        assert_eq!(t.at(2000, Input::Tick), vec![StopMic, Commit, Send]);
    }

    #[test]
    fn an_empty_reply_goes_straight_back_to_listening() {
        let mut t = Clock::new();
        t.send(Input::TalkToggled);
        t.at(100, heard());
        t.at(2000, Input::Tick);
        assert_eq!(
            t.send(Input::ReplyFinished(" ".into())),
            vec![StartMic(Purpose::Talk)]
        );
    }

    #[test]
    fn talk_mode_exits_in_one_click_from_any_phase() {
        for phase_inputs in [
            vec![],
            vec![heard(), Input::Tick],
            vec![heard(), Input::Tick, Input::ReplyFinished("x".into())],
        ] {
            let mut t = Clock::new();
            t.send(Input::TalkToggled);
            for input in phase_inputs {
                t.at(2500, input);
            }
            let fx = t.send(Input::Exit);
            assert_eq!(t.c.mode(), Mode::Idle, "{fx:?}");
            assert!(!t.c.speaking());
            assert!(fx.contains(&Discard));
        }
    }

    #[test]
    fn quiet_talk_mode_ends_itself() {
        let mut t = Clock::new();
        t.send(Input::TalkToggled);
        assert_eq!(t.at(60_000, Input::Tick), vec![]);
        let fx = t.at(60_000, Input::Tick);
        assert_eq!(fx[..2], [StopMic, Discard]);
        assert!(matches!(fx.last(), Some(Notice(_))));
        assert_eq!(t.c.mode(), Mode::Idle);
    }

    #[test]
    fn a_dropped_recognizer_restarts_then_gives_up() {
        let mut t = Clock::new();
        t.send(Input::TalkToggled);
        let fail = || Input::RecognizerEnded {
            error: Some("glitch".into()),
            fatal: false,
        };
        for _ in 0..MAX_FAILURES {
            assert_eq!(t.send(fail()), vec![StartMic(Purpose::Talk)]);
        }
        let fx = t.send(fail());
        assert_eq!(t.c.mode(), Mode::Idle);
        assert!(matches!(fx.last(), Some(Notice(m)) if m.contains("glitch")));
    }

    #[test]
    fn a_recognizer_that_stops_after_words_sends_them() {
        let mut t = Clock::new();
        t.send(Input::TalkToggled);
        t.at(100, heard());
        assert_eq!(
            t.send(Input::RecognizerEnded {
                error: None,
                fatal: false
            }),
            vec![Commit, Send]
        );
        assert_eq!(t.c.mode(), Mode::Talk(TalkPhase::Thinking));
    }

    #[test]
    fn fatal_errors_stop_everything_including_the_wake_word() {
        let mut t = Clock::new();
        t.send(Input::SetWake(true));
        let fx = t.send(Input::RecognizerEnded {
            error: Some("Microphone access is off.".into()),
            fatal: true,
        });
        assert_eq!(
            fx,
            vec![StopMic, Commit, Notice("Microphone access is off.".into())]
        );
        assert_eq!(t.c.mode(), Mode::Idle);
        // Nothing restarts the wake word afterwards.
        t.send(Input::HotkeyDown);
        assert_eq!(t.send(Input::Exit), vec![StopMic, Commit]);
        assert_eq!(t.c.mode(), Mode::Idle);
    }

    #[test]
    fn wake_word_waits_for_a_pause_then_captures_and_sends() {
        let mut t = Clock::new();
        assert_eq!(t.send(Input::SetWake(true)), vec![StartMic(Purpose::Wake)]);
        assert!(!t.c.routes_text());
        assert!(!t.c.needs_tick(), "wake listening needs no timer");
        t.at(1000, Input::WakePhrase);
        assert_eq!(t.c.mode(), Mode::WakeArmed);
        assert_eq!(t.at(300, Input::Tick), vec![]);
        assert_eq!(t.at(300, Input::Tick), vec![OpenComposer]);
        assert!(t.c.routes_text());
        t.at(500, heard());
        assert_eq!(
            t.at(2000, Input::Tick),
            vec![StopMic, Commit, Send, StartMic(Purpose::Wake)]
        );
        assert_eq!(t.c.mode(), Mode::WakeListening);
    }

    #[test]
    fn wake_capture_without_a_command_gives_up() {
        let mut t = Clock::new();
        t.send(Input::SetWake(true));
        t.send(Input::WakePhrase);
        t.at(600, Input::Tick);
        assert_eq!(
            t.at(6000, Input::Tick),
            vec![StopMic, Discard, StartMic(Purpose::Wake)]
        );
    }

    #[test]
    fn wake_capture_has_a_hard_stop() {
        let mut t = Clock::new();
        t.send(Input::SetWake(true));
        t.send(Input::WakePhrase);
        t.at(600, Input::Tick);
        // Someone keeps talking: words every second for 20 s.
        for _ in 0..19 {
            t.at(1000, heard());
            assert!(t.at(0, Input::Tick).is_empty());
        }
        t.at(1000, heard());
        assert_eq!(
            t.at(0, Input::Tick),
            vec![StopMic, Commit, Send, StartMic(Purpose::Wake)]
        );
    }

    #[test]
    fn push_to_talk_borrows_the_mic_from_the_wake_word_and_gives_it_back() {
        let mut t = Clock::new();
        t.send(Input::SetWake(true));
        assert_eq!(
            t.send(Input::HotkeyDown),
            vec![StartMic(Purpose::Dictation)]
        );
        t.at(100, Input::HotkeyUp);
        assert_eq!(
            t.at(800, Input::Tick),
            vec![StopMic, Commit, StartMic(Purpose::Wake)]
        );
    }

    #[test]
    fn talk_mode_returns_to_the_wake_word() {
        let mut t = Clock::new();
        t.send(Input::SetWake(true));
        t.send(Input::TalkToggled);
        assert_eq!(t.c.mode(), Mode::Talk(TalkPhase::Listening));
        let fx = t.send(Input::TalkToggled);
        assert_eq!(fx.last(), Some(&StartMic(Purpose::Wake)));
    }

    #[test]
    fn turning_the_wake_word_off_frees_the_mic() {
        let mut t = Clock::new();
        t.send(Input::SetWake(true));
        assert_eq!(t.send(Input::SetWake(false)), vec![StopMic, Discard]);
        assert!(!t.c.mic_on());
        // Off while dictating changes nothing now, and the rest is idle.
        t.send(Input::SetWake(true));
        t.send(Input::HotkeyDown);
        assert_eq!(t.send(Input::SetWake(false)), vec![]);
        t.send(Input::HotkeyUp);
        assert_eq!(t.at(800, Input::Tick), vec![StopMic, Commit]);
        assert_eq!(t.c.mode(), Mode::Idle);
    }

    #[test]
    fn stop_button_silences_without_leaving_talk_mode() {
        let mut t = Clock::new();
        t.send(Input::TalkToggled);
        t.at(100, heard());
        t.at(2000, Input::Tick);
        t.send(Input::ReplyFinished("Words.".into()));
        assert_eq!(
            t.send(Input::StopSpeaking),
            vec![StopSpeaking, StartMic(Purpose::Talk)]
        );
        assert_eq!(t.c.mode(), Mode::Talk(TalkPhase::Listening));
    }
}
