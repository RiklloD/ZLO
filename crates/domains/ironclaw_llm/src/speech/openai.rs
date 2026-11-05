//! OpenAI text-to-speech (`/v1/audio/speech`).

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};

use super::{SpeechFormat, SpeechSynthesisError, SpeechSynthesisProvider, SynthesizedSpeech};

/// OpenAI's documented input limit for one speech request.
const MAX_CHARS: usize = 4096;
const DEFAULT_MODEL: &str = "gpt-4o-mini-tts";
const DEFAULT_VOICE: &str = "alloy";

/// OpenAI (or an OpenAI-compatible endpoint) speech provider.
pub struct OpenAiSpeechProvider {
    client: reqwest::Client,
    api_key: SecretString,
    model: String,
    base_url: String,
}

impl OpenAiSpeechProvider {
    /// A provider on `api.openai.com` with the default model.
    pub fn new(api_key: SecretString) -> Result<Self, SpeechSynthesisError> {
        Ok(Self {
            client: super::client()?,
            api_key,
            model: DEFAULT_MODEL.to_string(),
            base_url: "https://api.openai.com".to_string(),
        })
    }

    /// Override the base URL (proxied or compatible endpoints).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = super::normalized_base(base_url);
        self
    }

    /// Override the model name.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

/// The wire name OpenAI uses for `format`.
fn response_format(format: SpeechFormat) -> &'static str {
    match format {
        SpeechFormat::Mp3 => "mp3",
        SpeechFormat::Wav => "wav",
        SpeechFormat::Opus => "opus",
        SpeechFormat::Aac => "aac",
        SpeechFormat::Flac => "flac",
    }
}

/// The JSON body for one request.
fn request_body(model: &str, text: &str, voice: &str, format: SpeechFormat) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "input": text,
        "voice": voice,
        "response_format": response_format(format),
    })
}

#[async_trait]
impl SpeechSynthesisProvider for OpenAiSpeechProvider {
    fn max_chars(&self) -> usize {
        MAX_CHARS
    }

    async fn synthesize(
        &self,
        text: &str,
        voice: Option<&str>,
        format: SpeechFormat,
    ) -> Result<SynthesizedSpeech, SpeechSynthesisError> {
        let text = super::checked_text(text, MAX_CHARS)?;
        let body = request_body(&self.model, text, voice.unwrap_or(DEFAULT_VOICE), format);
        let response = self
            .client
            .post(format!("{}/v1/audio/speech", self.base_url))
            .bearer_auth(self.api_key.expose_secret())
            .json(&body)
            .send()
            .await
            .map_err(|e| SpeechSynthesisError::RequestFailed(e.to_string()))?;
        super::read_audio(response, format, &[self.api_key.expose_secret()]).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_server::serve_once;
    use super::*;

    fn provider(base: &str) -> OpenAiSpeechProvider {
        OpenAiSpeechProvider::new(SecretString::from("sk-test".to_string()))
            .expect("client")
            .with_base_url(format!("{base}/"))
    }

    #[test]
    fn body_names_model_voice_and_format() {
        let body = request_body("m", "Hello", "nova", SpeechFormat::Opus);
        assert_eq!(body["model"], "m");
        assert_eq!(body["input"], "Hello");
        assert_eq!(body["voice"], "nova");
        assert_eq!(body["response_format"], "opus");
    }

    #[tokio::test]
    async fn sends_authorized_json_and_returns_audio() {
        let (base, server) = serve_once(200, b"ID3audio").await;
        let speech = provider(&base)
            .synthesize("  Hello there  ", None, SpeechFormat::Mp3)
            .await
            .expect("speech");
        assert_eq!(speech.audio, b"ID3audio");
        assert_eq!(speech.format, SpeechFormat::Mp3);
        let seen = server.await.expect("server");
        assert!(
            seen.head.starts_with("POST /v1/audio/speech "),
            "{}",
            seen.head
        );
        assert!(
            seen.head
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-test"),
            "{}",
            seen.head
        );
        let body: serde_json::Value = serde_json::from_slice(&seen.body).expect("json");
        assert_eq!(body["input"], "Hello there");
        assert_eq!(body["voice"], DEFAULT_VOICE);
        assert_eq!(body["model"], DEFAULT_MODEL);
    }

    #[tokio::test]
    async fn http_error_keeps_status_and_bounded_body() {
        // Providers echo the key they rejected; it must not reach the error.
        let (base, server) =
            serve_once(401, b"{\"error\":\"Incorrect API key provided: sk-test\"}").await;
        let err = provider(&base)
            .synthesize("Hi", Some("echo"), SpeechFormat::Mp3)
            .await
            .expect_err("401");
        let message = err.to_string();
        assert!(message.contains("401"), "{message}");
        assert!(message.contains("Incorrect API key"), "{message}");
        assert!(
            !message.contains("sk-test"),
            "the key must not leak: {message}"
        );
        server.await.expect("server");
    }

    #[tokio::test]
    async fn redirects_are_not_followed_with_the_key() {
        use super::super::test_server::{never_contacted, serve_once_with};
        let (elsewhere, contacted) = never_contacted().await;
        let (base, server) = serve_once_with(
            307,
            format!("location: {elsewhere}/v1/audio/speech\r\n"),
            Vec::new(),
        )
        .await;
        let err = provider(&base)
            .synthesize("Hi", None, SpeechFormat::Mp3)
            .await
            .expect_err("a redirect is an error");
        assert!(err.to_string().contains("307"), "{err}");
        server.await.expect("server");
        assert!(
            !contacted.await.expect("listener"),
            "the second origin was reached"
        );
    }

    #[tokio::test]
    async fn empty_and_oversized_text_never_reach_the_network() {
        // No server: a request would fail with a connection error instead.
        let p = provider("http://127.0.0.1:9");
        assert!(matches!(
            p.synthesize(" ", None, SpeechFormat::Mp3).await,
            Err(SpeechSynthesisError::EmptyText)
        ));
        let long = "a".repeat(MAX_CHARS + 1);
        assert!(matches!(
            p.synthesize(&long, None, SpeechFormat::Mp3).await,
            Err(SpeechSynthesisError::TextTooLong { .. })
        ));
    }
}
