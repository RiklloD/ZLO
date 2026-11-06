//! ElevenLabs text-to-speech (`/v1/text-to-speech/{voice_id}`).

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};

use super::{SpeechFormat, SpeechSynthesisError, SpeechSynthesisProvider, SynthesizedSpeech};

/// ElevenLabs' per-request character limit for the flash models.
const MAX_CHARS: usize = 5000;
const DEFAULT_MODEL: &str = "eleven_flash_v2_5";
/// "Rachel", ElevenLabs' long-standing default premade voice.
const DEFAULT_VOICE: &str = "21m00Tcm4TlvDq8ikWAM";

/// ElevenLabs speech provider.
pub struct ElevenLabsSpeechProvider {
    client: reqwest::Client,
    api_key: SecretString,
    model: String,
    base_url: String,
}

impl ElevenLabsSpeechProvider {
    /// A provider on `api.elevenlabs.io` with the default model.
    pub fn new(api_key: SecretString) -> Result<Self, SpeechSynthesisError> {
        Ok(Self {
            client: super::client()?,
            api_key,
            model: DEFAULT_MODEL.to_string(),
            base_url: "https://api.elevenlabs.io".to_string(),
        })
    }

    /// Override the base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = super::normalized_base(base_url);
        self
    }

    /// Override the model id.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

/// ElevenLabs' `output_format` for `format`; it serves no WAV/AAC/FLAC.
fn output_format(format: SpeechFormat) -> Result<&'static str, SpeechSynthesisError> {
    match format {
        SpeechFormat::Mp3 => Ok("mp3_44100_128"),
        SpeechFormat::Opus => Ok("opus_48000_64"),
        other => Err(SpeechSynthesisError::UnsupportedFormat {
            provider: "ElevenLabs",
            format: other,
        }),
    }
}

/// A voice id is a path segment: only the id alphabet ElevenLabs issues.
fn valid_voice_id(voice: &str) -> bool {
    !voice.is_empty() && voice.len() <= 64 && voice.chars().all(|c| c.is_ascii_alphanumeric())
}

#[async_trait]
impl SpeechSynthesisProvider for ElevenLabsSpeechProvider {
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
        let wire_format = output_format(format)?;
        let voice = voice.unwrap_or(DEFAULT_VOICE);
        if !valid_voice_id(voice) {
            return Err(SpeechSynthesisError::RequestFailed(format!(
                "invalid ElevenLabs voice id {voice:?}"
            )));
        }
        let response = self
            .client
            // Both path parts are checked above: `voice` is alphanumeric and
            // `wire_format` a constant, so neither needs escaping.
            .post(format!(
                "{}/v1/text-to-speech/{voice}?output_format={wire_format}",
                self.base_url
            ))
            .header("xi-api-key", self.api_key.expose_secret())
            .json(&serde_json::json!({ "text": text, "model_id": self.model }))
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

    fn provider(base: &str) -> ElevenLabsSpeechProvider {
        ElevenLabsSpeechProvider::new(SecretString::from("xi-test".to_string()))
            .expect("client")
            .with_base_url(base)
    }

    #[tokio::test]
    async fn posts_to_the_voice_with_key_header_and_format() {
        let (base, server) = serve_once(200, b"mp3!").await;
        let speech = provider(&base)
            .synthesize("Hello", Some("abc123"), SpeechFormat::Mp3)
            .await
            .expect("speech");
        assert_eq!(speech.audio, b"mp3!");
        let seen = server.await.expect("server");
        assert!(
            seen.head
                .starts_with("POST /v1/text-to-speech/abc123?output_format=mp3_44100_128 "),
            "{}",
            seen.head
        );
        assert!(
            seen.head
                .to_ascii_lowercase()
                .contains("xi-api-key: xi-test")
        );
        let body: serde_json::Value = serde_json::from_slice(&seen.body).expect("json");
        assert_eq!(body["text"], "Hello");
        assert_eq!(body["model_id"], DEFAULT_MODEL);
    }

    #[tokio::test]
    async fn redirects_are_not_followed_with_the_key() {
        use super::super::test_server::{never_contacted, serve_once_with};
        let (elsewhere, contacted) = never_contacted().await;
        let (base, server) =
            serve_once_with(307, format!("location: {elsewhere}/steal\r\n"), Vec::new()).await;
        let err = provider(&base)
            .synthesize("Hi", Some("abc123"), SpeechFormat::Mp3)
            .await
            .expect_err("a redirect is an error");
        assert!(err.to_string().contains("307"), "{err}");
        server.await.expect("server");
        assert!(
            !contacted.await.expect("listener"),
            "xi-api-key went to the second origin"
        );
    }

    #[tokio::test]
    async fn an_echoed_key_never_reaches_the_error() {
        let (base, server) = serve_once(
            401,
            b"{\"detail\":{\"message\":\"Invalid API key: xi-test\"}}",
        )
        .await;
        let err = provider(&base)
            .synthesize("Hi", Some("abc123"), SpeechFormat::Mp3)
            .await
            .expect_err("401");
        let message = err.to_string();
        assert!(message.contains("401"), "{message}");
        assert!(message.contains("Invalid API key"), "{message}");
        assert!(!message.contains("xi-test"), "{message}");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn rejects_formats_and_voice_ids_it_cannot_serve() {
        let p = provider("http://127.0.0.1:9");
        assert!(matches!(
            p.synthesize("Hi", None, SpeechFormat::Wav).await,
            Err(SpeechSynthesisError::UnsupportedFormat { .. })
        ));
        // A path-traversal voice id never becomes a URL.
        assert!(matches!(
            p.synthesize("Hi", Some("../../v1/user"), SpeechFormat::Mp3)
                .await,
            Err(SpeechSynthesisError::RequestFailed(m)) if m.contains("invalid")
        ));
    }

    #[test]
    fn voice_ids_are_alphanumeric() {
        assert!(valid_voice_id(DEFAULT_VOICE));
        assert!(!valid_voice_id(""));
        assert!(!valid_voice_id("a/b"));
        assert!(!valid_voice_id("a b"));
    }
}
