//! The latest screenshot per session. Frames come from the server's own
//! action results (the agent's view of the window it drives); nothing here
//! captures the screen on its own.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    /// Increases by one per frame within a session.
    pub seq: u64,
    pub captured_at_ms: i64,
    pub mime_type: String,
    /// Base64 image bytes as the server sent them.
    pub data: String,
    /// The tool call that produced it.
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
}

/// Target app and window from a result's header lines
/// (`App=<bundle> (pid N)`, `Window: "<title>", App: <name>.`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Target {
    pub app: Option<String>,
    pub window: Option<String>,
}

pub(crate) fn parse_target(text: &str) -> Target {
    let mut target = Target::default();
    for line in text.lines() {
        if target.app.is_none()
            && let Some(rest) = line.strip_prefix("App=")
        {
            let bundle = rest.split(" (pid").next().unwrap_or(rest).trim();
            if !bundle.is_empty() {
                target.app = Some(bundle.to_string());
            }
        } else if target.window.is_none()
            && let Some(rest) = line.strip_prefix("Window: \"")
            && let Some(end) = rest.rfind("\", App:")
        {
            // `end` comes from `rfind`, so it sits on a char boundary.
            target.window = Some(rest[..end].to_string());
        }
    }
    target
}

/// Text parts joined, plus the first image part (`(mime, base64)`).
pub(crate) fn split_content(result: &Value) -> (String, Option<(String, String)>) {
    let mut texts = Vec::new();
    let mut image = None;
    for item in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    texts.push(text);
                }
            }
            Some("image") if image.is_none() => {
                if let Some(data) = item.get("data").and_then(Value::as_str)
                    && !data.is_empty()
                {
                    let mime = item
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png");
                    image = Some((mime.to_string(), data.to_string()));
                }
            }
            _ => {}
        }
    }
    (texts.join("\n"), image)
}
