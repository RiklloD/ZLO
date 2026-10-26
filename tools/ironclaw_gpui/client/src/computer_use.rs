//! Computer use's live view: which windows the agent drives in each of the
//! caller's threads (with its cursor, status and typing), one window's
//! picture, and Stop / take over / hand back. The gateway answers `Value`
//! documents; the shapes below are the subset the viewer reads.

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{Client, V2, seg};
use crate::error::ClientError;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComputerState {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub now: i64,
    #[serde(default)]
    pub sessions: Vec<ComputerSession>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComputerSession {
    pub thread_id: String,
    #[serde(default)]
    pub apps: Vec<ComputerApp>,
    #[serde(default)]
    pub inflight: u32,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub pointer: Option<ComputerPointer>,
    #[serde(default)]
    pub typing: Option<ComputerTyping>,
    #[serde(default)]
    pub last_activity: i64,
    #[serde(default)]
    pub ended: bool,
    #[serde(default)]
    pub last_failure: Option<ComputerFailure>,
    #[serde(default)]
    pub human_has_control: bool,
    #[serde(default)]
    pub stopping: bool,
    #[serde(default)]
    pub watchable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ComputerApp {
    pub pid: i64,
    pub window_id: u32,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub capture: ComputerCapture,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ComputerCapture {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Seconds since the window's last picture.
    #[serde(default)]
    pub frame_age: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ComputerPointer {
    pub kind: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub to_x: Option<f64>,
    #[serde(default)]
    pub to_y: Option<f64>,
    pub window_id: u32,
    pub seq: u64,
    /// Unix ms.
    pub at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ComputerTyping {
    pub text: String,
    pub at: i64,
    #[serde(default)]
    pub window_id: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ComputerFailure {
    pub label: String,
}

/// One window picture: JPEG bytes and their pixel size.
#[derive(Debug, Clone)]
pub struct ComputerFrame {
    pub seq: u64,
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The frame route's answer.
#[derive(Debug, Clone)]
pub enum FrameAnswer {
    Frame(ComputerFrame),
    /// Nothing newer yet.
    Unchanged,
    /// No picture: `ended`, `human_has_control` or `capture_failed`, plus
    /// the capture error when there is one.
    Refused {
        code: String,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputerControl {
    Stop,
    TakeOver,
    HandBack,
}

impl ComputerControl {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::TakeOver => "take_over",
            Self::HandBack => "hand_back",
        }
    }
}

impl Client {
    /// Every computer-use session of the signed-in user. `viewer` keeps this
    /// viewer's take over alive.
    pub async fn computer_state(&self, viewer: &str) -> Result<ComputerState, ClientError> {
        let value: Value = self
            .get(&format!("{V2}/computer-use?viewer={}", seg(viewer)))
            .await?;
        serde_json::from_value(value).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// The newest picture of `window_id` after `after_seq`; the gateway waits
    /// up to a second for one.
    pub async fn computer_frame(
        &self,
        thread_id: &str,
        window_id: u32,
        width: u32,
        fps: f64,
        after_seq: Option<u64>,
    ) -> Result<FrameAnswer, ClientError> {
        let mut path = format!(
            "{V2}/threads/{}/computer-use/frame?window_id={window_id}&width={width}&fps={fps}",
            seg(thread_id)
        );
        if let Some(after) = after_seq {
            path.push_str(&format!("&after_seq={after}"));
        }
        let value: Value = self.get(&path).await?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
            return Ok(FrameAnswer::Refused {
                code: text("code").unwrap_or_else(|| "unavailable".into()),
                detail: text("detail"),
            });
        }
        let Some(frame) = value.get("frame").filter(|f| !f.is_null()) else {
            return Ok(FrameAnswer::Unchanged);
        };
        let number = |key: &str| frame.get(key).and_then(Value::as_u64).unwrap_or(0);
        let data = frame.get("data").and_then(Value::as_str).unwrap_or("");
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| ClientError::Decode(e.to_string()))?;
        Ok(FrameAnswer::Frame(ComputerFrame {
            seq: number("seq"),
            jpeg,
            width: u32::try_from(number("width")).unwrap_or(0),
            height: u32::try_from(number("height")).unwrap_or(0),
        }))
    }

    pub async fn computer_control(
        &self,
        thread_id: &str,
        action: ComputerControl,
        viewer: &str,
    ) -> Result<Value, ClientError> {
        let body = json!({ "action": action.as_str(), "viewer": viewer });
        self.post(
            &format!("{V2}/threads/{}/computer-use", seg(thread_id)),
            &body,
        )
        .await
    }
}
