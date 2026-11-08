//! Apple's on-device engines: `SFSpeechRecognizer` fed by `AVAudioEngine`
//! for speech in, `AVSpeechSynthesizer` for speech out. No model download,
//! no audio leaves the Mac unless the person allows Apple's servers.
//!
//! Kept light like `native_view.rs`: Speech and AVFoundation are not
//! linked but `dlopen`ed the first time voice is used, so a session that
//! never talks never maps them. All calls run on the main thread; the
//! recognizer delivers results on the main queue and the audio tap runs on
//! Core Audio's thread, touching only the request it feeds.
//!
//! macOS kills a process that opens the microphone or the recognizer
//! without the usage strings in its Info.plist, so a plain `cargo run`
//! binary is refused up front (`tools/ironclaw_rune/bundle.sh` writes the strings).

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use block2::RcBlock;
use futures::channel::oneshot;
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::sel;

use super::coordinator::Purpose;
use super::engine::{
    DICTATION_OFF, PrivacyPane, SpeechSynthesizer, SpeechToText, SttConfig, SttEvent, SttFailure,
    SttSink,
};

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_LAZY: i32 = 1;
const MIC_USAGE: &str = "NSMicrophoneUsageDescription";
const SPEECH_USAGE: &str = "NSSpeechRecognitionUsageDescription";

/// The AVFoundation handle, for its exported constants.
struct Frameworks {
    avf: usize,
}

fn frameworks() -> Result<&'static Frameworks, String> {
    static LOADED: OnceLock<Result<Frameworks, String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            // SAFETY: dlopen of system framework paths; handles are kept for
            // the life of the process.
            unsafe {
                let speech = dlopen(
                    c"/System/Library/Frameworks/Speech.framework/Speech".as_ptr(),
                    RTLD_LAZY,
                );
                let avf = dlopen(
                    c"/System/Library/Frameworks/AVFoundation.framework/AVFoundation".as_ptr(),
                    RTLD_LAZY,
                );
                if speech.is_null() || avf.is_null() {
                    return Err("This Mac is missing the Speech or AVFoundation framework.".into());
                }
                Ok(Frameworks { avf: avf as usize })
            }
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn class(name: &CStr) -> Result<&'static AnyClass, String> {
    AnyClass::get(name).ok_or_else(|| format!("{} is not available.", name.to_string_lossy()))
}

/// Whether the running bundle declares `key` (a privacy usage string).
fn declares(key: &str) -> bool {
    let Ok(cls) = class(c"NSBundle") else {
        return false;
    };
    // SAFETY: NSBundle class methods; `key` is a live NSString.
    unsafe {
        let bundle: *mut AnyObject = msg_send![cls, mainBundle];
        let Some(key) = ns_string(key) else {
            return false;
        };
        !bundle.is_null() && {
            let value: *mut AnyObject = msg_send![bundle, objectForInfoDictionaryKey: &*key];
            !value.is_null()
        }
    }
}

/// Voice can start: usage strings present, frameworks loaded.
fn ready() -> Result<(), SttFailure> {
    if !declares(MIC_USAGE) || !declares(SPEECH_USAGE) {
        return Err(SttFailure {
            message:
                "Voice works in ZLO.app (tools/ironclaw_rune/bundle.sh); this build has no \
                      microphone permission strings."
                    .into(),
            pane: None,
        });
    }
    frameworks().map(|_| ()).map_err(|message| SttFailure {
        message,
        pane: None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Granted,
    Undetermined,
    Denied,
}

fn audio_media_type() -> Option<*mut AnyObject> {
    let f = frameworks().ok()?;
    // SAFETY: AVMediaTypeAudio is an exported `NSString *const`.
    unsafe {
        let sym =
            dlsym(f.avf as *mut c_void, c"AVMediaTypeAudio".as_ptr()) as *const *mut AnyObject;
        (!sym.is_null() && !(*sym).is_null()).then(|| *sym)
    }
}

fn mic_access() -> Access {
    let (Ok(cls), Some(media)) = (class(c"AVCaptureDevice"), audio_media_type()) else {
        return Access::Denied;
    };
    // SAFETY: class method taking the audio media type constant.
    let status: isize = unsafe { msg_send![cls, authorizationStatusForMediaType: media] };
    match status {
        0 => Access::Undetermined,
        3 => Access::Granted,
        _ => Access::Denied,
    }
}

fn speech_access() -> Access {
    let Ok(cls) = class(c"SFSpeechRecognizer") else {
        return Access::Denied;
    };
    // SAFETY: class method with no arguments.
    let status: isize = unsafe { msg_send![cls, authorizationStatus] };
    match status {
        0 => Access::Undetermined,
        3 => Access::Granted,
        _ => Access::Denied,
    }
}

/// A one-shot reply slot a block (called once, on any queue) can fill.
type Slot<T> = Arc<Mutex<Option<oneshot::Sender<T>>>>;

fn reply<T: Send + 'static>() -> (Slot<T>, oneshot::Receiver<T>) {
    let (tx, rx) = oneshot::channel();
    (Arc::new(Mutex::new(Some(tx))), rx)
}

fn fill<T>(slot: &Mutex<Option<oneshot::Sender<T>>>, value: T) {
    if let Ok(mut slot) = slot.lock()
        && let Some(tx) = slot.take()
    {
        let _ = tx.send(value);
    }
}

async fn request_mic() -> bool {
    let (Ok(cls), Some(media)) = (class(c"AVCaptureDevice"), audio_media_type()) else {
        return false;
    };
    let (slot, rx) = reply::<bool>();
    let block = RcBlock::new(move |granted: Bool| fill(&slot, granted.as_bool()));
    // SAFETY: the block matches `void (^)(BOOL)`; AVFoundation copies it.
    unsafe {
        let _: () = msg_send![cls, requestAccessForMediaType: media, completionHandler: &*block];
    }
    // silent-ok: a dropped reply means macOS never answered, which is a "no".
    rx.await.unwrap_or(false)
}

async fn request_speech() -> bool {
    let Ok(cls) = class(c"SFSpeechRecognizer") else {
        return false;
    };
    let (slot, rx) = reply::<bool>();
    let block = RcBlock::new(move |status: isize| fill(&slot, status == 3));
    // SAFETY: the block matches `void (^)(SFSpeechRecognizerAuthorizationStatus)`.
    unsafe {
        let _: () = msg_send![cls, requestAuthorization: &*block];
    }
    // silent-ok: a dropped reply means macOS never answered, which is a "no".
    rx.await.unwrap_or(false)
}

/// Ask for the microphone and speech recognition, prompting the first
/// time. A refusal names the System Settings pane that undoes it.
pub(crate) async fn authorize() -> Result<(), SttFailure> {
    ready()?;
    let denied = |pane: PrivacyPane| SttFailure {
        message: format!(
            "ZLO has no {} access. Turn it on in System Settings → Privacy & Security.",
            pane.label()
        ),
        pane: Some(pane),
    };
    let mic = match mic_access() {
        Access::Granted => true,
        Access::Undetermined => request_mic().await,
        Access::Denied => false,
    };
    if !mic {
        return Err(denied(PrivacyPane::Microphone));
    }
    let speech = match speech_access() {
        Access::Granted => true,
        Access::Undetermined => request_speech().await,
        Access::Denied => false,
    };
    if !speech {
        return Err(denied(PrivacyPane::SpeechRecognition));
    }
    Ok(())
}

/// Whether both permissions are granted already (no prompt).
pub(crate) fn authorized() -> bool {
    ready().is_ok() && mic_access() == Access::Granted && speech_access() == Access::Granted
}

type ResultHandler = RcBlock<dyn Fn(*mut AnyObject, *mut AnyObject)>;
type TapBlock = RcBlock<dyn Fn(*mut AnyObject, *mut AnyObject)>;

struct Session {
    engine: Retained<AnyObject>,
    input: Retained<AnyObject>,
    request: Retained<AnyObject>,
    task: Option<Retained<AnyObject>>,
    _recognizer: Retained<AnyObject>,
    _tap: TapBlock,
    _handler: ResultHandler,
    /// Set once the session reported its end (or was stopped).
    ended: Arc<AtomicBool>,
    recording: bool,
}

impl Session {
    fn stop_recording(&mut self) {
        if !self.recording {
            return;
        }
        self.recording = false;
        // SAFETY: live engine, its input node and the request.
        unsafe {
            let _: () = msg_send![&*self.engine, stop];
            let _: () = msg_send![&*self.input, removeTapOnBus: 0usize];
            let _: () = msg_send![&*self.request, endAudio];
        }
    }
}

/// `SFSpeechRecognizer` on the default input device.
#[derive(Default)]
pub(crate) struct AppleStt {
    session: Option<Session>,
}

impl Drop for AppleStt {
    fn drop(&mut self) {
        self.stop();
    }
}

impl SpeechToText for AppleStt {
    fn start(&mut self, id: u64, config: &SttConfig, sink: SttSink) -> Result<(), SttFailure> {
        self.stop();
        ready()?;
        let fail = |message: String| SttFailure {
            message,
            pane: None,
        };
        // SAFETY: every message below goes to a live object of the class
        // named, with arguments of the documented types. Blocks match their
        // documented signatures and are kept alive by the session.
        unsafe {
            let recognizer: Option<Retained<AnyObject>> =
                msg_send![class(c"SFSpeechRecognizer").map_err(fail)?, new];
            let recognizer = recognizer
                .ok_or_else(|| fail("Speech recognition does not support this language.".into()))?;
            let available: bool = msg_send![&*recognizer, isAvailable];
            if !available {
                return Err(fail(
                    "Speech recognition is not available right now. Check Siri & Dictation in \
                     System Settings."
                        .into(),
                ));
            }
            let on_device: bool = msg_send![&*recognizer, supportsOnDeviceRecognition];
            if !on_device && config.on_device_only {
                return Err(fail(
                    "On-device recognition is not installed for your language. Turn on \
                     Dictation in System Settings → Keyboard to download it, or allow Apple's \
                     servers in ZLO's Voice settings."
                        .into(),
                ));
            }
            let request: Option<Retained<AnyObject>> = msg_send![
                class(c"SFSpeechAudioBufferRecognitionRequest").map_err(fail)?,
                new
            ];
            let request = request.ok_or_else(|| fail("Could not create a request.".into()))?;
            let _: () = msg_send![&*request, setShouldReportPartialResults: true];
            if on_device {
                let _: () = msg_send![&*request, setRequiresOnDeviceRecognition: true];
            }
            let punctuates: bool =
                msg_send![&*request, respondsToSelector: sel!(setAddsPunctuation:)];
            if punctuates {
                let _: () = msg_send![&*request, setAddsPunctuation: true];
            }
            // SFSpeechRecognitionTaskHint: 1 dictation, 2 search (short).
            let hint: isize = if config.purpose == Purpose::Wake {
                2
            } else {
                1
            };
            let _: () = msg_send![&*request, setTaskHint: hint];
            if let Some(hints) = ns_array(&config.hints) {
                let _: () = msg_send![&*request, setContextualStrings: &*hints];
            }

            let engine: Option<Retained<AnyObject>> =
                msg_send![class(c"AVAudioEngine").map_err(fail)?, new];
            let engine = engine.ok_or_else(|| fail("Could not open the audio engine.".into()))?;
            let input: *mut AnyObject = msg_send![&*engine, inputNode];
            let input = Retained::retain(input)
                .ok_or_else(|| fail("No microphone is connected.".into()))?;
            // Echo cancellation, so a reply being spoken does not hear itself.
            if matches!(config.purpose, Purpose::Talk | Purpose::BargeIn) {
                let mut error: *mut AnyObject = std::ptr::null_mut();
                let ok: bool =
                    msg_send![&*input, setVoiceProcessingEnabled: true, error: &mut error];
                if !ok {
                    tracing::debug!(error = %error_text(error), "voice: no echo cancellation");
                }
            }
            let format: *mut AnyObject = msg_send![&*input, outputFormatForBus: 0usize];
            if format.is_null() {
                return Err(fail("No microphone input is available.".into()));
            }
            let rate: f64 = msg_send![format, sampleRate];
            let channels: u32 = msg_send![format, channelCount];
            if rate <= 0. || channels == 0 {
                return Err(fail("No microphone input is available.".into()));
            }
            let feed = request.clone();
            let tap: TapBlock =
                RcBlock::new(move |buffer: *mut AnyObject, _when: *mut AnyObject| {
                    if !buffer.is_null() {
                        // SAFETY: appending a PCM buffer is safe from the tap thread.
                        let _: () = msg_send![&*feed, appendAudioPCMBuffer: buffer];
                    }
                });
            let _: () = msg_send![
                &*input,
                installTapOnBus: 0usize,
                bufferSize: 1024u32,
                format: format,
                block: &*tap
            ];
            let _: () = msg_send![&*engine, prepare];
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let started: bool = msg_send![&*engine, startAndReturnError: &mut error];
            if !started {
                let _: () = msg_send![&*input, removeTapOnBus: 0usize];
                return Err(fail(format!(
                    "The microphone did not start: {}",
                    error_text(error)
                )));
            }

            let ended = Arc::new(AtomicBool::new(false));
            let done = ended.clone();
            let handler: ResultHandler =
                RcBlock::new(move |result: *mut AnyObject, error: *mut AnyObject| {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    let mut finished = false;
                    if !result.is_null() {
                        // SAFETY: an SFSpeechRecognitionResult from the recognizer.
                        let (text, is_final) = {
                            let best: *mut AnyObject = msg_send![result, bestTranscription];
                            let text = if best.is_null() {
                                String::new()
                            } else {
                                let s: *mut AnyObject = msg_send![best, formattedString];
                                rust_string(s)
                            };
                            let is_final: bool = msg_send![result, isFinal];
                            (text, is_final)
                        };
                        let event = if is_final {
                            SttEvent::Final(text)
                        } else {
                            SttEvent::Partial(text)
                        };
                        let _ = sink.unbounded_send((id, event));
                        finished = is_final;
                    }
                    if !error.is_null() || finished {
                        done.store(true, Ordering::Relaxed);
                        let (error, fatal) = classify(error);
                        let _ = sink.unbounded_send((id, SttEvent::Ended { error, fatal }));
                    }
                });
            let task: Option<Retained<AnyObject>> = msg_send![
                &*recognizer,
                recognitionTaskWithRequest: &*request,
                resultHandler: &*handler
            ];
            self.session = Some(Session {
                engine,
                input,
                request,
                task,
                _recognizer: recognizer,
                _tap: tap,
                _handler: handler,
                ended,
                recording: true,
            });
        }
        Ok(())
    }

    fn end_audio(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.stop_recording();
        }
    }

    fn stop(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.ended.store(true, Ordering::Relaxed);
            session.stop_recording();
            if let Some(task) = session.task.as_ref() {
                // SAFETY: a live SFSpeechRecognitionTask.
                unsafe {
                    let _: () = msg_send![&**task, cancel];
                }
            }
        }
    }
}

/// (message to show, fatal) for a recognizer error; no message for the
/// ordinary ends (no speech, canceled).
fn classify(error: *mut AnyObject) -> (Option<String>, bool) {
    if error.is_null() {
        return (None, false);
    }
    // SAFETY: an NSError from the recognizer.
    let (code, domain) = unsafe {
        let code: isize = msg_send![error, code];
        let domain: *mut AnyObject = msg_send![error, domain];
        (code, rust_string(domain))
    };
    let benign = matches!(code, 203 | 209 | 216 | 301 | 1110);
    if benign && (domain.contains("Assistant") || domain.contains("Speech")) {
        return (None, false);
    }
    // Siri & Dictation off (1700, LSR 201): fatal, and say how to fix it.
    if code == 1700 || (domain.contains("LSR") && code == 201) {
        return (Some(DICTATION_OFF.into()), true);
    }
    (Some(error_text(error)), false)
}

fn error_text(error: *mut AnyObject) -> String {
    if error.is_null() {
        return "unknown error".into();
    }
    // SAFETY: an NSError.
    unsafe {
        let text: *mut AnyObject = msg_send![error, localizedDescription];
        rust_string(text)
    }
}

/// `AVSpeechSynthesizer`, made per reply and freed when it stops.
#[derive(Default)]
pub(crate) struct AppleTts {
    synth: Option<Retained<AnyObject>>,
}

impl SpeechSynthesizer for AppleTts {
    fn speak(&mut self, text: &str, rate: f32) -> Result<(), String> {
        frameworks()?;
        // SAFETY: AVSpeechSynthesizer/AVSpeechUtterance with live NSStrings.
        unsafe {
            if self.synth.is_none() {
                let synth: Option<Retained<AnyObject>> =
                    msg_send![class(c"AVSpeechSynthesizer")?, new];
                self.synth = synth;
            }
            let synth = self
                .synth
                .as_ref()
                .ok_or("The speech synthesizer did not start.")?;
            let text = ns_string(text).ok_or("Could not read the reply.")?;
            let utterance: *mut AnyObject = msg_send![
                class(c"AVSpeechUtterance")?,
                speechUtteranceWithString: &*text
            ];
            if utterance.is_null() {
                return Err("Could not prepare the reply for speaking.".into());
            }
            let _: () = msg_send![utterance, setRate: rate.clamp(0., 1.)];
            let _: () = msg_send![&**synth, speakUtterance: utterance];
        }
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(synth) = self.synth.take() {
            // SAFETY: a live synthesizer; 0 is AVSpeechBoundaryImmediate.
            unsafe {
                let _: bool = msg_send![&*synth, stopSpeakingAtBoundary: 0isize];
            }
        }
    }

    fn is_speaking(&self) -> bool {
        self.synth.as_ref().is_some_and(|synth| {
            // SAFETY: a live synthesizer.
            unsafe { msg_send![&**synth, isSpeaking] }
        })
    }
}

/// An NSString copy of `text`.
unsafe fn ns_string(text: &str) -> Option<Retained<AnyObject>> {
    let cls = AnyClass::get(c"NSString")?;
    // SAFETY: initWithBytes copies `text`; NSUTF8StringEncoding = 4.
    unsafe {
        let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
        msg_send![
            alloc,
            initWithBytes: text.as_ptr().cast::<c_void>(),
            length: text.len(),
            encoding: 4usize
        ]
    }
}

/// An NSArray of NSStrings, or `None` for an empty list.
unsafe fn ns_array(items: &[String]) -> Option<Retained<AnyObject>> {
    if items.is_empty() {
        return None;
    }
    // SAFETY: the strings outlive the call; arrayWithObjects retains them.
    unsafe {
        let strings: Vec<Retained<AnyObject>> = items.iter().filter_map(|s| ns_string(s)).collect();
        let ptrs: Vec<*mut AnyObject> = strings
            .iter()
            .map(|s| Retained::as_ptr(s).cast_mut())
            .collect();
        let cls = AnyClass::get(c"NSArray")?;
        msg_send![cls, arrayWithObjects: ptrs.as_ptr(), count: ptrs.len()]
    }
}

fn rust_string(ns: *mut AnyObject) -> String {
    if ns.is_null() {
        return String::new();
    }
    // SAFETY: an NSString; UTF8String lives as long as the string.
    unsafe {
        let utf8: *const c_char = msg_send![ns, UTF8String];
        if utf8.is_null() {
            return String::new();
        }
        CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }
}
