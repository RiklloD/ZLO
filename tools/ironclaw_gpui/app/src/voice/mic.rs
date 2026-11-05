//! The one gate in front of the speech engine.
//!
//! Every start, stop and engine event goes through [`Mic`], which keeps a
//! session number: stopping (or starting again) bumps it, so a start that
//! was waiting on macOS's permission prompts, or a late event from a
//! finished session, can never reopen or feed the mic. A session that ends
//! on its own is torn down here too, because Apple's recognizer finishing
//! does not stop the audio engine's tap (the mic would stay open).

use std::time::{Duration, Instant};

use super::coordinator::Purpose;
use super::engine::{SpeechToText, SttConfig, SttEvent, SttFailure, SttSink};

/// A session that ends this fast without a word is a failure, so a
/// recognizer that refuses to run cannot spin the mic in a loop.
pub(crate) const MIN_SESSION: Duration = Duration::from_secs(1);

/// What a start request did.
#[derive(Debug, PartialEq)]
pub(crate) enum Start {
    Opened,
    /// Ask macOS first; call [`Mic::authorized`] with the answer.
    Authorize,
    /// A permission prompt is already up; its answer covers this start.
    Waiting,
    Failed(SttFailure),
}

pub(crate) struct Mic {
    engine: Box<dyn SpeechToText>,
    session: u64,
    /// The start waiting on the permission prompt, by session.
    pending: Option<(u64, Purpose)>,
    authorizing: bool,
    /// The engine holds an open session.
    open: bool,
    started: Instant,
    words: bool,
}

impl Mic {
    pub fn new(engine: Box<dyn SpeechToText>, now: Instant) -> Self {
        Self {
            engine,
            session: 0,
            pending: None,
            authorizing: false,
            open: false,
            started: now,
            words: false,
        }
    }

    #[cfg(test)]
    pub fn session(&self) -> u64 {
        self.session
    }

    /// Start a fresh session for `purpose`. `authorized`: both permissions
    /// are granted already.
    pub fn start(
        &mut self,
        purpose: Purpose,
        authorized: bool,
        config: &SttConfig,
        sink: SttSink,
        now: Instant,
    ) -> Start {
        self.stop();
        if !authorized {
            self.pending = Some((self.session, purpose));
            if self.authorizing {
                return Start::Waiting;
            }
            self.authorizing = true;
            return Start::Authorize;
        }
        match self.open(config, sink, now) {
            Ok(()) => Start::Opened,
            Err(failure) => Start::Failed(failure),
        }
    }

    /// The permission prompt was answered: the start still waiting for it,
    /// if nothing stopped or replaced it meanwhile.
    pub fn authorized(&mut self) -> Option<Purpose> {
        self.authorizing = false;
        let (session, purpose) = self.pending.take()?;
        (session == self.session).then_some(purpose)
    }

    /// Open the engine for the current session (after authorization).
    pub fn open(
        &mut self,
        config: &SttConfig,
        sink: SttSink,
        now: Instant,
    ) -> Result<(), SttFailure> {
        self.started = now;
        self.words = false;
        self.engine.start(self.session, config, sink)?;
        self.open = true;
        Ok(())
    }

    /// End the session: the engine stops, a pending start is dropped, and
    /// nothing from this session gets through any more.
    pub fn stop(&mut self) {
        self.session += 1;
        self.pending = None;
        if self.open {
            self.open = false;
            self.engine.stop();
        }
    }

    /// Stop recording but let the final result arrive.
    pub fn end_audio(&mut self) {
        if self.open {
            self.engine.end_audio();
        }
    }

    /// An engine event, if it belongs to the open session. An end tears the
    /// session down; one that came too fast without a word is an error.
    pub fn accept(&mut self, session: u64, event: SttEvent, now: Instant) -> Option<SttEvent> {
        if session != self.session || !self.open {
            return None;
        }
        match event {
            SttEvent::Partial(ref text) | SttEvent::Final(ref text) => {
                self.words |= !text.trim().is_empty();
                Some(event)
            }
            SttEvent::Ended { error, fatal } => {
                let error = match error {
                    None if !self.words
                        && now.saturating_duration_since(self.started) < MIN_SESSION =>
                    {
                        Some("the recognizer stopped right away".into())
                    }
                    other => other,
                };
                self.stop();
                Some(SttEvent::Ended { error, fatal })
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use futures::channel::mpsc::unbounded;

    use super::super::coordinator::{Coordinator, Effect, Input, Mode, Timing};
    use super::*;

    #[derive(Default, Debug)]
    pub(crate) struct Calls {
        pub starts: Vec<u64>,
        pub stops: u32,
        pub end_audio: u32,
        pub recording: bool,
    }

    /// Records what the mic asked of the engine.
    pub(crate) struct FakeStt(pub Rc<RefCell<Calls>>);

    impl SpeechToText for FakeStt {
        fn start(&mut self, session: u64, _: &SttConfig, _: SttSink) -> Result<(), SttFailure> {
            let mut c = self.0.borrow_mut();
            c.starts.push(session);
            c.recording = true;
            Ok(())
        }
        fn end_audio(&mut self) {
            let mut c = self.0.borrow_mut();
            c.end_audio += 1;
            c.recording = false;
        }
        fn stop(&mut self) {
            let mut c = self.0.borrow_mut();
            c.stops += 1;
            c.recording = false;
        }
    }

    fn config(purpose: Purpose) -> SttConfig {
        SttConfig {
            purpose,
            on_device_only: true,
            hints: Vec::new(),
        }
    }

    fn mic() -> (Mic, Rc<RefCell<Calls>>, Instant) {
        let calls = Rc::new(RefCell::new(Calls::default()));
        let now = Instant::now();
        (Mic::new(Box::new(FakeStt(calls.clone())), now), calls, now)
    }

    fn sink() -> SttSink {
        unbounded().0
    }

    #[test]
    fn a_start_cancelled_during_the_permission_prompt_never_opens() {
        let (mut m, calls, now) = mic();
        let start = m.start(
            Purpose::Dictation,
            false,
            &config(Purpose::Dictation),
            sink(),
            now,
        );
        assert_eq!(start, Start::Authorize);
        // The person lets go (or presses ✕) while macOS asks.
        m.stop();
        // Then grants access: nothing is waiting any more.
        assert_eq!(m.authorized(), None);
        assert!(calls.borrow().starts.is_empty());
        assert!(!calls.borrow().recording);
    }

    #[test]
    fn the_latest_start_during_the_prompt_is_the_one_that_opens() {
        let (mut m, _, now) = mic();
        m.start(
            Purpose::Dictation,
            false,
            &config(Purpose::Dictation),
            sink(),
            now,
        );
        m.stop();
        let again = m.start(Purpose::Wake, false, &config(Purpose::Wake), sink(), now);
        assert_eq!(again, Start::Waiting, "one prompt at a time");
        assert_eq!(m.authorized(), Some(Purpose::Wake));
    }

    #[test]
    fn an_ended_session_stops_the_engine_and_drops_late_events() {
        let (mut m, calls, now) = mic();
        m.start(
            Purpose::Dictation,
            true,
            &config(Purpose::Dictation),
            sink(),
            now,
        );
        let s = m.session();
        assert!(calls.borrow().recording);
        let later = now + Duration::from_secs(3);
        m.accept(s, SttEvent::Partial("hi".into()), later);
        let ended = m.accept(
            s,
            SttEvent::Ended {
                error: None,
                fatal: false,
            },
            later,
        );
        assert_eq!(
            ended,
            Some(SttEvent::Ended {
                error: None,
                fatal: false
            })
        );
        assert!(
            !calls.borrow().recording,
            "the tap must not keep the mic open"
        );
        assert_eq!(calls.borrow().stops, 1);
        assert_eq!(m.accept(s, SttEvent::Partial("late".into()), later), None);
    }

    #[test]
    fn events_from_a_stopped_session_are_ignored() {
        let (mut m, _, now) = mic();
        m.start(Purpose::Talk, true, &config(Purpose::Talk), sink(), now);
        let old = m.session();
        m.start(Purpose::Talk, true, &config(Purpose::Talk), sink(), now);
        assert_eq!(m.accept(old, SttEvent::Final("old".into()), now), None);
        assert!(
            m.accept(m.session(), SttEvent::Partial("new".into()), now)
                .is_some()
        );
    }

    #[test]
    fn an_instant_wordless_end_is_a_failure() {
        let (mut m, _, now) = mic();
        m.start(Purpose::Wake, true, &config(Purpose::Wake), sink(), now);
        let s = m.session();
        let ended = m.accept(
            s,
            SttEvent::Ended {
                error: None,
                fatal: false,
            },
            now,
        );
        assert!(matches!(
            ended,
            Some(SttEvent::Ended { error: Some(_), .. })
        ));
    }

    /// Drive the coordinator with a real [`Mic`] in front of a fake engine,
    /// the way `voice/mod.rs` carries out mic effects.
    struct Rig {
        c: Coordinator,
        m: Mic,
        calls: Rc<RefCell<Calls>>,
        now: Instant,
    }

    impl Rig {
        fn new() -> Self {
            let (m, calls, now) = mic();
            Self {
                c: Coordinator::new(Timing::default(), now),
                m,
                calls,
                now,
            }
        }

        fn send(&mut self, ms: u64, input: Input) {
            self.now += Duration::from_millis(ms);
            for effect in self.c.handle(input, self.now) {
                match effect {
                    Effect::StartMic(p) => {
                        self.m.start(p, true, &config(p), sink(), self.now);
                    }
                    Effect::StopMic => self.m.stop(),
                    Effect::EndAudio => self.m.end_audio(),
                    _ => {}
                }
            }
        }

        /// An engine event the way `on_stt` feeds it.
        fn engine(&mut self, ms: u64, event: SttEvent) {
            self.now += Duration::from_millis(ms);
            let session = self.m.session();
            match self.m.accept(session, event, self.now) {
                Some(SttEvent::Ended { error, fatal }) => {
                    self.send(0, Input::RecognizerEnded { error, fatal })
                }
                Some(SttEvent::Partial(t) | SttEvent::Final(t)) => self.send(
                    0,
                    Input::Heard {
                        words: !t.trim().is_empty(),
                    },
                ),
                None => {}
            }
        }
    }

    #[test]
    fn a_dictation_the_recognizer_ends_leaves_the_mic_off() {
        let mut r = Rig::new();
        r.send(0, Input::HotkeyDown);
        r.engine(1500, SttEvent::Final("done".into()));
        r.engine(
            0,
            SttEvent::Ended {
                error: None,
                fatal: false,
            },
        );
        assert_eq!(r.c.mode(), Mode::Idle);
        assert!(!r.calls.borrow().recording);
    }

    #[test]
    fn talk_mode_stops_the_mic_while_thinking_after_a_natural_end() {
        let mut r = Rig::new();
        r.send(0, Input::TalkToggled);
        r.engine(1500, SttEvent::Partial("what time is it".into()));
        r.engine(
            100,
            SttEvent::Ended {
                error: None,
                fatal: false,
            },
        );
        assert_eq!(
            r.c.mode(),
            Mode::Talk(super::super::coordinator::TalkPhase::Thinking)
        );
        assert!(!r.calls.borrow().recording);
    }

    #[test]
    fn the_wake_word_giving_up_leaves_the_mic_off() {
        let mut r = Rig::new();
        r.send(0, Input::SetWake(true));
        for _ in 0..4 {
            r.engine(
                2000,
                SttEvent::Ended {
                    error: Some("glitch".into()),
                    fatal: false,
                },
            );
        }
        assert_eq!(r.c.mode(), Mode::Idle);
        assert!(!r.calls.borrow().recording);
    }
}
