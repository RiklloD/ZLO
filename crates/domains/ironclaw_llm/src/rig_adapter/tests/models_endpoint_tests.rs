// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]
use super::*;

#[test]
fn parse_models_response_openai_extracts_ids() {
    let body =
        br#"{"object":"list","data":[{"id":"gpt-4o","object":"model"},{"id":"gpt-4o-mini"}]}"#;
    let models = parse_models_response("openai", ModelsShape::OpenAiData, body).expect("parses");
    assert_eq!(models, vec!["gpt-4o", "gpt-4o-mini"]);
}

#[test]
fn parse_models_response_ollama_extracts_names() {
    let body = br#"{"models":[{"name":"llama3.2:latest"},{"name":"qwen2.5"}]}"#;
    let models = parse_models_response("ollama", ModelsShape::OllamaTags, body).expect("parses");
    assert_eq!(models, vec!["llama3.2:latest", "qwen2.5"]);
}

#[test]
fn parse_models_response_drops_blank_ids_and_tolerates_missing_data() {
    let with_blank = br#"{"data":[{"id":"a"},{"id":"  "},{"id":""}]}"#;
    assert_eq!(
        parse_models_response("openai", ModelsShape::OpenAiData, with_blank).expect("parses"),
        vec!["a"]
    );
    // A successful response with no `data` key yields an empty list, not an error.
    let no_data = br#"{"object":"list"}"#;
    assert!(
        parse_models_response("openai", ModelsShape::OpenAiData, no_data)
            .expect("parses")
            .is_empty()
    );
}

#[test]
fn parse_models_response_rejects_malformed_json() {
    let err =
        parse_models_response("openai", ModelsShape::OpenAiData, b"not json").expect_err("rejects");
    assert!(matches!(err, LlmError::InvalidResponse { .. }));
}

// Serve one canned HTTP response on a loopback port and return the endpoint
// pointed at it. The response is written verbatim, so callers control the
// status line and headers.
async fn endpoint_against_canned_response(raw_response: &'static str) -> ModelsEndpoint {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(raw_response.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });
    ModelsEndpoint {
        provider_id: "p".to_string(),
        url: format!("http://{addr}/models"),
        auth: ModelsAuth::Bearer("k".to_string()),
        shape: ModelsShape::OpenAiData,
        extra_headers: reqwest::header::HeaderMap::new(),
    }
}

#[tokio::test]
async fn fetch_models_maps_401_to_auth_failed() {
    let endpoint =
        endpoint_against_canned_response("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            .await;
    let err = endpoint.fetch_models().await.expect_err("401 is an error");
    assert!(matches!(err, LlmError::AuthFailed { .. }), "got {err:?}");
}

#[tokio::test]
async fn fetch_models_maps_403_to_auth_failed() {
    let endpoint =
        endpoint_against_canned_response("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await;
    let err = endpoint.fetch_models().await.expect_err("403 is an error");
    assert!(matches!(err, LlmError::AuthFailed { .. }), "got {err:?}");
}

#[tokio::test]
async fn fetch_models_does_not_follow_redirects() {
    // A host that passed the SSRF guard must not be able to 3xx-redirect the
    // request elsewhere (e.g. the metadata IP). With redirects disabled the
    // 301 surfaces as a non-success status, not a followed hop.
    let endpoint = endpoint_against_canned_response(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: http://169.254.169.254/\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
    let err = endpoint
        .fetch_models()
        .await
        .expect_err("redirect is not followed");
    assert!(matches!(err, LlmError::RequestFailed { .. }), "got {err:?}");
}

#[test]
fn parse_models_response_gemini_strips_prefix_and_keeps_chat_models() {
    // Gemini `models.list`: ids carry a `models/` prefix, and the catalogue
    // mixes chat models with embedding/video models that cannot serve
    // `generateContent`.
    let body = br#"{"models":[
        {"name":"models/gemini-4-hydrogen","supportedGenerationMethods":["generateContent","countTokens"]},
        {"name":"models/gemini-embedding-2","supportedGenerationMethods":["embedContent"]},
        {"name":"models/gemini-4-carbon","supportedGenerationMethods":["generateContent"]},
        {"name":"models/veo-4","supportedGenerationMethods":["predictLongRunning"]},
        {"name":"models/gemini-3.8-flash"}
    ],"nextPageToken":"abc"}"#;
    let models = parse_models_response("gemini", ModelsShape::GeminiModels, body).expect("parses");
    assert_eq!(
        models,
        vec!["gemini-4-hydrogen", "gemini-4-carbon", "gemini-3.8-flash"]
    );
}

/// Caller-level: the `gemini` (API key) factory must wire live discovery, so
/// new Google models reach the picker without an app update. Drives the real
/// factory against a loopback server and checks the request it sends.
#[tokio::test]
async fn gemini_factory_lists_models_from_models_endpoint() {
    use secrecy::SecretString;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let (req_tx, req_rx) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let _ = req_tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
            let body = r#"{"models":[{"name":"models/gemini-4-hydrogen","supportedGenerationMethods":["generateContent"]},{"name":"models/text-embedding-5","supportedGenerationMethods":["embedContent"]}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });

    let config = crate::config::RegistryProviderConfig::generic(
        crate::registry::ProviderProtocol::Gemini,
        "gemini",
        Some(SecretString::from("test-gemini-key".to_string())),
        format!("http://{addr}"),
        "gemini-2.5-flash",
    );
    let provider = crate::create_gemini_from_registry(&config, 30).expect("provider builds");
    let models = provider.list_models().await.expect("lists models");
    assert_eq!(models, vec!["gemini-4-hydrogen"]);

    let request = req_rx.await.expect("request captured");
    let request_line = request.lines().next().unwrap_or_default();
    assert!(
        request_line.starts_with("GET /v1beta/models?pageSize=1000 "),
        "unexpected request line: {request_line}"
    );
    let lower = request.to_ascii_lowercase();
    assert!(
        lower.contains("x-goog-api-key: test-gemini-key"),
        "API key must travel in the x-goog-api-key header: {request}"
    );
    assert!(
        !request_line.contains("test-gemini-key"),
        "API key must never be put in the URL"
    );
}
