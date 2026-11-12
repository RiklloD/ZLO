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
