//! The seams between the voice session and the machines that hear and
//! speak. On-device Apple engines implement both today (`apple.rs`); a
//! cloud engine (the gateway reaching `ironclaw_llm`'s transcription and
//! `speech` providers) plugs in behind the same traits once the gateway
//! serves voice.

use futures::channel::mpsc::UnboundedSender;

use super::coordinator::Purpose;

/// One recognizer result, tagged with the session it belongs to so events
/// from a stopped session are ignored.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SttEvent {
    /// The whole utterance so far (recognizers resend it every time).
    Partial(String),
    Final(String),
    /// The session ended: after `Final`, on silence, or on an error.
    Ended {
        error: Option<String>,
        fatal: bool,
    },
}

pub(crate) type SttSink = UnboundedSender<(u64, SttEvent)>;

/// How one listening session is set up.
#[derive(Clone, Debug)]
pub(crate) struct SttConfig {
    pub purpose: Purpose,
    /// Refuse to send audio off the Mac.
    pub on_device_only: bool,
    /// Words to bias recognition toward (the wake phrase, "ZLO").
    pub hints: Vec<String>,
}

/// Why listening could not start.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SttFailure {
    pub message: String,
    /// The System Settings privacy pane that fixes it, if any.
    pub pane: Option<PrivacyPane>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrivacyPane {
    Microphone,
    SpeechRecognition,
}

impl PrivacyPane {
    pub fn url(self) -> &'static str {
        match self {
            Self::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
            Self::SpeechRecognition => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition"
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Microphone => "Microphone",
            Self::SpeechRecognition => "Speech Recognition",
        }
    }
}

/// Speech in. One session at a time; `start` replaces a running one.
pub(crate) trait SpeechToText {
    fn start(&mut self, session: u64, config: &SttConfig, sink: SttSink) -> Result<(), SttFailure>;
    /// Stop recording but let the final result arrive.
    fn end_audio(&mut self);
    /// Stop now; no more events for this session.
    fn stop(&mut self);
}

/// Speech out.
pub(crate) trait SpeechSynthesizer {
    /// `rate` is 0..1, 0.5 the voice's normal pace.
    fn speak(&mut self, text: &str, rate: f32) -> Result<(), String>;
    fn stop(&mut self);
    fn is_speaking(&self) -> bool;
}
