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
// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]

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

    #[error("Speech synthesis returned more than {max} bytes of audio")]
    AudioTooLarge { max: usize },
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

/// The most audio one reply may return (a minute of 128 kbit/s MP3 is
/// about 1 MiB; this leaves room for slower formats and longer text).
pub(crate) const MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
/// How much of a provider's error body is read, and shown.
const ERROR_BODY_LIMIT: usize = 4 * 1024;
const ERROR_TEXT_LIMIT: usize = 512;

/// Build the shared hardened client, failing construction instead of
/// degrading to an unhardened default. Redirects are never followed: the
/// requests carry credentials (`xi-api-key` is not a header reqwest strips
/// on a cross-origin hop) and the endpoints never redirect legitimately.
pub(crate) fn client() -> Result<reqwest::Client, SpeechSynthesisError> {
    crate::config::hardened_client_builder(crate::config::SPEECH_REQUEST_TIMEOUT_SECS)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| SpeechSynthesisError::ClientBuild {
            reason: e.to_string(),
        })
}

/// Read a provider response: audio bytes on success (at most
/// [`MAX_AUDIO_BYTES`]), a bounded, redacted error otherwise. `secrets`
/// are the credentials the request carried.
pub(crate) async fn read_audio(
    response: reqwest::Response,
    format: SpeechFormat,
    secrets: &[&str],
) -> Result<SynthesizedSpeech, SpeechSynthesisError> {
    read_audio_capped(response, format, secrets, MAX_AUDIO_BYTES).await
}

pub(crate) async fn read_audio_capped(
    mut response: reqwest::Response,
    format: SpeechFormat,
    secrets: &[&str],
    max: usize,
) -> Result<SynthesizedSpeech, SpeechSynthesisError> {
    let status = response.status();
    if !status.is_success() {
        let body = match read_up_to(&mut response, ERROR_BODY_LIMIT).await {
            Ok((bytes, _)) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => format!("(error body unreadable: {e})"),
        };
        let body: String = redact(&body, secrets)
            .chars()
            .take(ERROR_TEXT_LIMIT)
            .collect();
        return Err(SpeechSynthesisError::RequestFailed(format!(
            "HTTP {status}: {body}"
        )));
    }
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(SpeechSynthesisError::AudioTooLarge { max });
    }
    let (audio, complete) = read_up_to(&mut response, max)
        .await
        .map_err(|e| SpeechSynthesisError::RequestFailed(e.to_string()))?;
    if !complete {
        return Err(SpeechSynthesisError::AudioTooLarge { max });
    }
    if audio.is_empty() {
        return Err(SpeechSynthesisError::EmptyAudio);
    }
    Ok(SynthesizedSpeech { audio, format })
}

/// At most `max` bytes of the body, chunk by chunk; `true` when the body
/// ended within them.
async fn read_up_to(
    response: &mut reqwest::Response,
    max: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = max - out.len();
        if chunk.len() > room {
            out.extend_from_slice(&chunk[..room]);
            return Ok((out, false));
        }
        out.extend_from_slice(&chunk);
    }
    Ok((out, true))
}

/// `body` with the request's credentials replaced (providers echo a
/// rejected key in their error messages), plus anything that is plainly a
/// credential: a documented key format (`sk-…` for OpenAI, `sk_…` for
/// ElevenLabs) or a long mixed letter-and-digit value within two words
/// after a credential word (`key`, `token`, `secret`, `bearer`,
/// `authorization`, `password`). Model, voice and request ids stay, since
/// they are what makes an error actionable.
pub(crate) fn redact(body: &str, secrets: &[&str]) -> String {
    const REDACTED: &str = "[redacted]";
    const CREDENTIAL_WORDS: [&str; 6] = [
        "key",
        "token",
        "secret",
        "bearer",
        "authorization",
        "password",
    ];
    let mut out = body.to_string();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(secret, REDACTED);
    }
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut clean = String::with_capacity(out.len());
    let mut rest = out.as_str();
    // Words left in which a value counts as the credential just named.
    let mut after_credential_word = 0u8;
    while let Some(start) = rest.find(token_char) {
        clean.push_str(&rest[..start]);
        let tail = &rest[start..];
        let len = tail.find(|c: char| !token_char(c)).unwrap_or(tail.len());
        let token = &tail[..len];
        let lower = token.to_ascii_lowercase();
        let documented =
            (lower.starts_with("sk-") || lower.starts_with("sk_")) && token.len() >= 12;
        let in_context = after_credential_word > 0
            && token.len() >= 16
            && token.chars().any(|c| c.is_ascii_digit())
            && token.chars().any(|c| c.is_ascii_alphabetic());
        clean.push_str(if documented || in_context {
            REDACTED
        } else {
            token
        });
        after_credential_word = if CREDENTIAL_WORDS.iter().any(|w| lower.ends_with(w)) {
            2
        } else {
            after_credential_word.saturating_sub(1)
        };
        rest = &tail[len..];
    }
    clean.push_str(rest);
    clean
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
        serve_once_with(status, String::new(), body.to_vec()).await
    }

    /// A base URL whose listener reports whether anyone connected within a
    /// second (redirect tests: the second origin must never be reached).
    pub(crate) async fn never_contacted() -> (String, tokio::task::JoinHandle<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept())
                .await
                .is_ok()
        });
        (format!("http://{addr}"), handle)
    }

    /// Answer one request with `status` and a chunked body (no length) of
    /// `total` bytes, written in `chunk`-byte pieces until the client hangs
    /// up. The handle yields how many body bytes went out, so a test can
    /// tell a capped read (stops early) from one that buffers it all.
    pub(crate) async fn serve_streaming(
        status: u16,
        total: usize,
        chunk: usize,
    ) -> (String, tokio::task::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 4096];
            let mut head = Vec::new();
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.expect("read");
                assert!(n > 0, "client closed before sending headers");
                head.extend_from_slice(&buf[..n]);
            }
            let reply = format!(
                "HTTP/1.1 {status} X\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
            );
            if socket.write_all(reply.as_bytes()).await.is_err() {
                return 0;
            }
            let piece = vec![b'x'; chunk];
            let mut sent = 0;
            while sent < total {
                let frame = format!("{chunk:x}\r\n");
                let ok = socket.write_all(frame.as_bytes()).await.is_ok()
                    && socket.write_all(&piece).await.is_ok()
                    && socket.write_all(b"\r\n").await.is_ok();
                if !ok {
                    return sent;
                }
                sent += chunk;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
            sent
        });
        (format!("http://{addr}"), handle)
    }

    /// [`serve_once`] with extra response header lines (each ending in
    /// `\r\n`) and an owned body. Writes tolerate a client that hangs up
    /// early (a capped read stops reading).
    pub(crate) async fn serve_once_with(
        status: u16,
        headers: String,
        body: Vec<u8>,
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
                "HTTP/1.1 {status} X\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.write_all(&body).await;
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
    fn redaction_removes_the_key_and_credential_shaped_tokens() {
        let body = r#"{"error":"Incorrect API key provided: sk-proj-abc123. Key sk-live-ABCDEFGH1234567890abcdef is bad"}"#;
        let clean = redact(body, &["sk-proj-abc123"]);
        assert!(!clean.contains("sk-proj-abc123"), "{clean}");
        assert!(!clean.contains("ABCDEFGH1234567890abcdef"), "{clean}");
        assert!(clean.contains("Incorrect API key provided"), "{clean}");
        // ElevenLabs keys, bearer tokens, and a long value right after a
        // credential word.
        for (body, secret) in [
            (
                "invalid key sk_9f8e7d6c5b4a3f2e1d0c",
                "sk_9f8e7d6c5b4a3f2e1d0c",
            ),
            (
                "Bearer eyJhbGciOiJIUzI1NiJ9abc123",
                "eyJhbGciOiJIUzI1NiJ9abc123",
            ),
            (
                "api_key: a1b2c3d4e5f6g7h8i9j0 rejected",
                "a1b2c3d4e5f6g7h8i9j0",
            ),
            ("token=Zx9Yw8Vu7Ts6Rq5Po4Nm", "Zx9Yw8Vu7Ts6Rq5Po4Nm"),
        ] {
            let clean = redact(body, &[]);
            assert!(!clean.contains(secret), "{body} -> {clean}");
        }
    }

    #[test]
    fn redaction_keeps_ordinary_ids() {
        for body in [
            "voice_id not found: abc123",
            "model eleven_multilingual_v2 does not support this voice",
            "voice 21m00Tcm4TlvDq8ikWAM is not available on your plan",
            "unknown model gpt-4o-mini-tts-2025-03-20",
            "request req_0123456789abcdef0123 failed, retry later",
            "The key for model eleven_multilingual_v2 is fine",
        ] {
            assert_eq!(redact(body, &[]), body);
        }
    }

    #[tokio::test]
    async fn a_declared_oversized_body_is_refused_before_reading() {
        use super::test_server::serve_once_with;
        let (base, _server) = serve_once_with(200, String::new(), vec![7u8; 64 * 1024]).await;
        let response = reqwest::get(&base).await.expect("response");
        let err = read_audio_capped(response, SpeechFormat::Mp3, &[], 1024)
            .await
            .expect_err("over the cap");
        assert!(
            matches!(err, SpeechSynthesisError::AudioTooLarge { max: 1024 }),
            "{err}"
        );
    }

    /// 64 MiB, far past what socket buffers hold, so a read that buffers it
    /// all makes the server send nearly all of it.
    const STREAM_TOTAL: usize = 64 * 1024 * 1024;

    #[tokio::test]
    async fn streamed_audio_past_the_cap_stops_reading_early() {
        use super::test_server::serve_streaming;
        let (base, server) = serve_streaming(200, STREAM_TOTAL, 16 * 1024).await;
        let response = reqwest::get(&base).await.expect("response");
        assert_eq!(response.content_length(), None, "no declared length");
        let err = read_audio_capped(response, SpeechFormat::Mp3, &[], 64 * 1024)
            .await
            .expect_err("over the cap");
        assert!(
            matches!(err, SpeechSynthesisError::AudioTooLarge { .. }),
            "{err}"
        );
        let sent = server.await.expect("server");
        assert!(
            sent < STREAM_TOTAL / 4,
            "the reader kept going: {sent} of {STREAM_TOTAL} bytes went out"
        );
    }

    #[tokio::test]
    async fn audio_within_the_cap_is_kept_whole() {
        use super::test_server::serve_once_with;
        let (base, _server) = serve_once_with(200, String::new(), vec![7u8; 3000]).await;
        let response = reqwest::get(&base).await.expect("response");
        let speech = read_audio_capped(response, SpeechFormat::Mp3, &[], 4096)
            .await
            .expect("audio");
        assert_eq!(speech.audio.len(), 3000);
    }

    #[tokio::test]
    async fn huge_error_bodies_are_cut_short() {
        use super::test_server::serve_streaming;
        let (base, server) = serve_streaming(500, STREAM_TOTAL, 16 * 1024).await;
        let response = reqwest::get(&base).await.expect("response");
        let err = read_audio_capped(response, SpeechFormat::Mp3, &[], 1024)
            .await
            .expect_err("500");
        let message = err.to_string();
        assert!(message.contains("500"), "{message}");
        assert!(
            message.len() < 1024,
            "error text is bounded: {} bytes",
            message.len()
        );
        let sent = server.await.expect("server");
        assert!(
            sent < STREAM_TOTAL / 4,
            "the error body was read past its limit: {sent} of {STREAM_TOTAL} bytes"
        );
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
