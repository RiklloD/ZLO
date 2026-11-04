//! Text-to-speech: the spoken-output counterpart of [`crate::transcription`].
//!
//! A [`SpeechSynthesisProvider`] turns reply text into audio bytes. The
//! implementations here are the cloud voices (OpenAI's `/v1/audio/speech`,
//! ElevenLabs' `/v1/text-to-speech`); an on-device voice (macOS
//! `AVSpeechSynthesizer`) lives in the client that plays it. Callers decide
//! *what* is spoken (code blocks and tables are summarized before they get
//! here); providers only enforce their own input limits.
//!
//! Like the transcription providers, these are a different trait from
//! `LlmProvider` and share only the hardened HTTP transport in `config.rs`.
//! Credentials arrive as [`secrecy::SecretString`] resolved by the caller's
//! secrets path; they are never logged or echoed in errors.

mod elevenlabs;
mod openai;

pub use self::elevenlabs::ElevenLabsSpeechProvider;
pub use self::openai::OpenAiSpeechProvider;

use async_trait::async_trait;

/// Audio encodings a provider can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechFormat {
    Mp3,
    Wav,
    Opus,
    Aac,
    Flac,
}

impl SpeechFormat {
    /// The MIME type of audio in this format.
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Mp3 => "audio/mpeg",
            Self::Wav => "audio/wav",
            Self::Opus => "audio/ogg",
            Self::Aac => "audio/aac",
            Self::Flac => "audio/flac",
        }
    }
}

/// Synthesized audio, ready to play or store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizedSpeech {
    pub audio: Vec<u8>,
    pub format: SpeechFormat,
}

/// Errors from a speech synthesis provider.
#[derive(Debug, thiserror::Error)]
pub enum SpeechSynthesisError {
    #[error("Speech synthesis request failed: {0}")]
    RequestFailed(String),

    #[error("Speech synthesis HTTP client could not be built: {reason}")]
    ClientBuild { reason: String },

    #[error("Nothing to speak: the text is empty")]
    EmptyText,

    #[error("Text is {len} characters; this voice takes at most {max}")]
    TextTooLong { len: usize, max: usize },

    #[error("{provider} cannot return {format:?} audio")]
    UnsupportedFormat {
        provider: &'static str,
        format: SpeechFormat,
    },

    #[error("Speech synthesis returned no audio")]
    EmptyAudio,
}

/// Trait for text-to-speech providers.
#[async_trait]
pub trait SpeechSynthesisProvider: Send + Sync {
    /// The longest text (in characters) one request accepts.
    fn max_chars(&self) -> usize;

    /// Speak `text` with `voice` (provider-specific id; `None` takes the
    /// provider's default) and return the audio in `format`.
    async fn synthesize(
        &self,
        text: &str,
        voice: Option<&str>,
        format: SpeechFormat,
    ) -> Result<SynthesizedSpeech, SpeechSynthesisError>;
}

/// Shared input check: trimmed, non-empty, within the provider's limit.
pub(crate) fn checked_text(text: &str, max: usize) -> Result<&str, SpeechSynthesisError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(SpeechSynthesisError::EmptyText);
    }
    let len = text.chars().count();
    if len > max {
        return Err(SpeechSynthesisError::TextTooLong { len, max });
    }
    Ok(text)
}

/// A base URL without trailing slashes, so joins never double them.
pub(crate) fn normalized_base(url: impl Into<String>) -> String {
    let mut url = url.into();
    while url.ends_with('/') {
        url.pop();
    }
    url
}

/// Build the shared hardened client, failing construction instead of
/// degrading to an unhardened default.
pub(crate) fn client() -> Result<reqwest::Client, SpeechSynthesisError> {
    crate::config::hardened_client_builder(crate::config::SPEECH_REQUEST_TIMEOUT_SECS)
        .build()
        .map_err(|e| SpeechSynthesisError::ClientBuild {
            reason: e.to_string(),
        })
}

/// Read a provider response: audio bytes on success, a bounded error body
/// otherwise (provider error bodies carry no secrets, but can be large).
pub(crate) async fn read_audio(
    response: reqwest::Response,
    format: SpeechFormat,
) -> Result<SynthesizedSpeech, SpeechSynthesisError> {
    const ERROR_BODY_LIMIT: usize = 512;
    let status = response.status();
    if !status.is_success() {
        let body = match response.text().await {
            Ok(body) => body,
            Err(e) => format!("(error body unreadable: {e})"),
        };
        let body: String = body.chars().take(ERROR_BODY_LIMIT).collect();
        return Err(SpeechSynthesisError::RequestFailed(format!(
            "HTTP {status}: {body}"
        )));
    }
    let audio = response
        .bytes()
        .await
        .map_err(|e| SpeechSynthesisError::RequestFailed(e.to_string()))?;
    if audio.is_empty() {
        return Err(SpeechSynthesisError::EmptyAudio);
    }
    Ok(SynthesizedSpeech {
        audio: audio.to_vec(),
        format,
    })
}

#[cfg(test)]
pub(crate) mod test_server {
    //! A one-shot HTTP server that records the request it gets and answers
    //! with a canned response, so provider tests check the real wire bytes
    //! the production client sends.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    pub(crate) struct Recorded {
        pub head: String,
        pub body: Vec<u8>,
    }

    /// Serve one request at the returned base URL.
    pub(crate) async fn serve_once(
        status: u16,
        body: &'static [u8],
    ) -> (String, tokio::task::JoinHandle<Recorded>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, body_start) = loop {
                let n = socket.read(&mut chunk).await.expect("read");
                assert!(n > 0, "client closed before sending headers");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break (String::from_utf8_lossy(&buf[..pos]).to_string(), pos + 4);
                }
            };
            let length = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while buf.len() < body_start + length {
                let n = socket.read(&mut chunk).await.expect("read body");
                assert!(n > 0, "client closed mid-body");
                buf.extend_from_slice(&chunk[..n]);
            }
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.expect("write");
            socket.write_all(body).await.expect("write body");
            Recorded {
                head,
                body: buf[body_start..body_start + length].to_vec(),
            }
        });
        (format!("http://{addr}"), handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_text_trims_and_bounds() {
        assert_eq!(checked_text("  hi  ", 10).ok(), Some("hi"));
        assert!(matches!(
            checked_text(" \n ", 10),
            Err(SpeechSynthesisError::EmptyText)
        ));
        // Counted in characters, not bytes.
        assert!(checked_text("ééé", 3).is_ok());
        assert!(matches!(
            checked_text("éééé", 3),
            Err(SpeechSynthesisError::TextTooLong { len: 4, max: 3 })
        ));
    }

    #[test]
    fn normalized_base_strips_trailing_slashes() {
        assert_eq!(normalized_base("https://x.test//"), "https://x.test");
        assert_eq!(normalized_base("https://x.test"), "https://x.test");
    }

    #[test]
    fn mime_types_are_audio() {
        for f in [
            SpeechFormat::Mp3,
            SpeechFormat::Wav,
            SpeechFormat::Opus,
            SpeechFormat::Aac,
            SpeechFormat::Flac,
        ] {
            assert!(f.mime_type().starts_with("audio/"));
        }
    }
}
