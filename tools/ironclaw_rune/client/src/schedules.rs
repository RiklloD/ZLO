//! Schedules (`t3_schedule` rows, made by an agent or on the Automations
//! page): list, preview, create, update, delete, run now, and a webhook's
//! signing secret — rotated, and revealed only when the user asks.
//!
//! Correctable failures (a bad cron, an unsafe prompt) come back as
//! [`ClientError::Refused`] with the backend's message.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Client, V2, client_action_id, seg};
use crate::error::ClientError;
use crate::orchestration::decode;

/// The header the page's webhook schedules are signed with.
pub const SIGNATURE_HEADER: &str = "x-zlo-signature";
pub const SIGNATURE_PREFIX: &str = "sha256=";

/// When a schedule fires; the backend's canonical `t3_schedule` spec.
/// `timezone` `None` asks for the backend's local zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleSpec {
    Interval {
        #[serde(rename = "everyMs")]
        every_ms: i64,
    },
    FixedTime {
        /// `HH:MM`, 24 h.
        #[serde(rename = "timeOfDay")]
        time_of_day: String,
        /// 0 = Sunday … 6 = Saturday; empty = every day.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        weekdays: Vec<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
    Cron {
        expression: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
    Webhook,
}

impl ScheduleSpec {
    pub fn timezone(&self) -> Option<&str> {
        match self {
            Self::FixedTime { timezone, .. } | Self::Cron { timezone, .. } => timezone.as_deref(),
            _ => None,
        }
    }

    pub fn is_webhook(&self) -> bool {
        matches!(self, Self::Webhook)
    }
}

/// The form's friendly schedule choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Every day at a time.
    Daily,
    /// Monday to Friday at a time.
    Weekdays,
    /// On picked days at a time.
    Weekly,
    /// Every N hours (decimals allowed: 0.5 = 30 min).
    EveryHours,
    /// A raw 5-field cron expression.
    Cron,
}

impl Preset {
    pub const ALL: [Preset; 5] = [
        Self::Daily,
        Self::Weekdays,
        Self::Weekly,
        Self::EveryHours,
        Self::Cron,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Daily => "Every day",
            Self::Weekdays => "Weekdays",
            Self::Weekly => "Weekly",
            Self::EveryHours => "Every N hours",
            Self::Cron => "Cron",
        }
    }
}

/// Which form field a schedule error belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftField {
    Time,
    Days,
    Hours,
    Cron,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftError {
    pub field: DraftField,
    pub message: String,
}

fn draft_error(field: DraftField, message: &str) -> DraftError {
    DraftError {
        field,
        message: message.to_string(),
    }
}

/// The schedule part of the form as typed. `timezone` `None` = the
/// backend's local zone. Checked locally for shape; the backend preview
/// is the authority on cron and zones.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleDraft {
    pub preset: Preset,
    pub time: String,
    /// 0 = Sunday … 6 = Saturday.
    pub days: Vec<u8>,
    pub hours: String,
    pub cron: String,
    pub timezone: Option<String>,
}

impl Default for ScheduleDraft {
    fn default() -> Self {
        Self {
            preset: Preset::Weekdays,
            time: "09:00".into(),
            days: vec![1],
            hours: "4".into(),
            cron: "0 9 * * MON-FRI".into(),
            timezone: None,
        }
    }
}

fn time_of_day(raw: &str) -> Option<String> {
    let (h, m) = raw.trim().split_once(':')?;
    let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
    (h < 24 && m < 60).then(|| format!("{h:02}:{m:02}"))
}

impl ScheduleDraft {
    pub fn to_spec(&self) -> Result<ScheduleSpec, DraftError> {
        let timezone = self
            .timezone
            .as_deref()
            .map(str::trim)
            .filter(|z| !z.is_empty())
            .map(str::to_string);
        let at = || {
            time_of_day(&self.time)
                .ok_or_else(|| draft_error(DraftField::Time, "Use a 24-hour time like 09:00."))
        };
        Ok(match self.preset {
            Preset::Daily | Preset::Weekdays | Preset::Weekly => {
                let weekdays = match self.preset {
                    Preset::Daily => vec![],
                    Preset::Weekdays => vec![1, 2, 3, 4, 5],
                    _ => {
                        let mut days: Vec<u8> =
                            self.days.iter().copied().filter(|d| *d <= 6).collect();
                        days.sort_unstable();
                        days.dedup();
                        if days.is_empty() {
                            return Err(draft_error(DraftField::Days, "Pick at least one day."));
                        }
                        days
                    }
                };
                ScheduleSpec::FixedTime {
                    time_of_day: at()?,
                    weekdays,
                    timezone,
                }
            }
            Preset::EveryHours => {
                let hours = self
                    .hours
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|h| h.is_finite() && *h * 60.0 >= 1.0 && *h <= 24.0 * 366.0)
                    .ok_or_else(|| {
                        draft_error(
                            DraftField::Hours,
                            "Use a number of hours (at least 1 minute).",
                        )
                    })?;
                ScheduleSpec::Interval {
                    every_ms: (hours * 3_600_000.0).round() as i64,
                }
            }
            Preset::Cron => {
                let expression = self.cron.split_whitespace().collect::<Vec<_>>().join(" ");
                if expression.is_empty() {
                    return Err(draft_error(DraftField::Cron, "Enter a cron expression."));
                }
                ScheduleSpec::Cron {
                    expression,
                    timezone,
                }
            }
        })
    }

    /// The form state for an existing clock spec; `None` for a webhook.
    pub fn from_spec(spec: &ScheduleSpec) -> Option<Self> {
        let base = Self::default();
        Some(match spec {
            ScheduleSpec::FixedTime {
                time_of_day,
                weekdays,
                timezone,
            } => {
                let preset = match weekdays.as_slice() {
                    [] => Preset::Daily,
                    [1, 2, 3, 4, 5] => Preset::Weekdays,
                    _ => Preset::Weekly,
                };
                Self {
                    preset,
                    time: time_of_day.clone(),
                    days: if weekdays.is_empty() {
                        base.days
                    } else {
                        weekdays.clone()
                    },
                    timezone: timezone.clone(),
                    ..base
                }
            }
            ScheduleSpec::Interval { every_ms } => {
                let hours = *every_ms as f64 / 3_600_000.0;
                let text = format!("{hours:.4}");
                let text = text.trim_end_matches('0').trim_end_matches('.').to_string();
                Self {
                    preset: Preset::EveryHours,
                    hours: text,
                    ..base
                }
            }
            ScheduleSpec::Cron {
                expression,
                timezone,
            } => Self {
                preset: Preset::Cron,
                cron: expression.clone(),
                timezone: timezone.clone(),
                ..base
            },
            ScheduleSpec::Webhook => return None,
        })
    }
}

/// How a request bearing the webhook's token was last answered.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Accepted,
    Unauthorized,
    Paused,
    Rejected,
    Failed,
    #[serde(other)]
    Unknown,
}

impl DeliveryStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Unauthorized => "bad signature",
            Self::Paused => "refused (paused)",
            Self::Rejected => "refused by the safety scan",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, Self::Accepted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Delivery {
    pub status: DeliveryStatus,
    pub at: Option<i64>,
}

/// A webhook's signature check, without its secret.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureInfo {
    pub header: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub encoding: String,
    #[serde(default)]
    pub has_secret: bool,
}

/// One schedule as the list shows it. Times are unix milliseconds.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleInfo {
    pub id: String,
    pub title: String,
    pub prompt: String,
    /// Kept raw: a spec this client does not know still lists.
    pub schedule: Value,
    #[serde(default)]
    pub describe: String,
    pub enabled: bool,
    pub thread_id: String,
    #[serde(default = "yes")]
    pub bind_to_current_thread: bool,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    #[serde(default)]
    pub run_count: i64,
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_run_thread_id: Option<String>,
    #[serde(default)]
    pub last_delivery: Option<Delivery>,
    #[serde(default)]
    pub webhook_url_masked: Option<String>,
    #[serde(default)]
    pub signature: Option<SignatureInfo>,
    #[serde(default)]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

fn yes() -> bool {
    true
}

impl ScheduleInfo {
    pub fn spec(&self) -> Option<ScheduleSpec> {
        serde_json::from_value(self.schedule.clone()).ok() // silent-ok: an unknown spec lists as raw text
    }

    pub fn is_webhook(&self) -> bool {
        self.schedule.get("type").and_then(Value::as_str) == Some("webhook")
    }
}

/// What the row can honestly say about its latest run. The backend records
/// when a run was *queued* into its thread (and failures to queue one), not
/// how the turn ended — that lives in the run's thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastOutcome {
    Never,
    /// Queued at this unix-ms time.
    Queued(i64),
    /// The schedule could not queue a run (bad spec, thread gone, …).
    Failed(String),
}

impl ScheduleInfo {
    pub fn last_outcome(&self) -> LastOutcome {
        match (&self.last_error, self.last_run_at) {
            (Some(error), _) => LastOutcome::Failed(error.clone()),
            (None, Some(at)) => LastOutcome::Queued(at),
            (None, None) => LastOutcome::Never,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulePreview {
    pub schedule: Value,
    pub describe: String,
    pub next_runs: Vec<i64>,
}

/// A webhook's full URL and signing secret. `Debug` hides both.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleSecret {
    pub webhook_url: Option<String>,
    pub signature: Option<RevealedSignature>,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct RevealedSignature {
    pub header: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub encoding: String,
    pub secret: String,
}

impl fmt::Debug for ScheduleSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScheduleSecret")
            .field(
                "webhook_url",
                &self.webhook_url.as_ref().map(|_| "<redacted>"),
            )
            .field("signature", &self.signature)
            .finish()
    }
}

impl fmt::Debug for RevealedSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RevealedSignature")
            .field("header", &self.header)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// What the form saves. `signature` on a webhook: `Some(true)` signs
/// requests (the backend makes the secret), `Some(false)` drops the check,
/// `None` keeps what is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleWrite {
    pub title: String,
    pub prompt: String,
    pub schedule: ScheduleSpec,
    /// `false`: each run starts a fresh thread on the schedule's profile and model.
    pub bind_to_current_thread: bool,
    pub enabled: bool,
    pub signed: Option<bool>,
}

impl ScheduleWrite {
    fn body(&self) -> Value {
        let mut body = json!({
            "title": self.title,
            "prompt": self.prompt,
            "schedule": self.schedule,
            "bind_to_current_thread": self.bind_to_current_thread,
            "enabled": self.enabled,
        });
        match (self.schedule.is_webhook(), self.signed) {
            (true, Some(true)) => {
                body["signature"] = json!({
                    "header": SIGNATURE_HEADER,
                    "prefix": SIGNATURE_PREFIX,
                    "encoding": "hex",
                    "generate": true,
                });
            }
            (true, Some(false)) | (false, _) => body["signature"] = Value::Null,
            (true, None) => {}
        }
        body
    }
}

#[derive(Deserialize)]
struct ScheduleDoc {
    schedule: ScheduleInfo,
}

#[derive(Deserialize)]
struct ScheduleListDoc {
    schedules: Vec<ScheduleInfo>,
}

/// One POSIX shell word for `value`, whatever it holds: single-quoted, with
/// each `'` written as `'\''`. Header names, prefixes and secrets can come
/// from a model-made schedule, so nothing is pasted raw.
fn sh(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// A copy-paste request that signs its body the way the webhook checks it.
/// `url` `None` and an unrevealed secret read `$ZLO_WEBHOOK_URL` and
/// `$ZLO_WEBHOOK_SECRET`. Every interpolated value is shell-quoted.
pub fn curl_example(
    url: Option<&str>,
    signature: Option<&RevealedSignature>,
    signed: bool,
) -> String {
    let body = sh(r#"{"event":"test","message":"hello from curl"}"#);
    let url = url.map_or_else(|| "\"$ZLO_WEBHOOK_URL\"".to_string(), sh);
    if !signed {
        return format!(
            "curl -X POST {url} \\\n  -H 'Content-Type: application/json' \\\n  -d {body}"
        );
    }
    let (header, secret) = match signature {
        Some(s) => (format!("{}: {}", s.header, s.prefix), sh(&s.secret)),
        None => (
            format!("{SIGNATURE_HEADER}: {SIGNATURE_PREFIX}"),
            "\"$ZLO_WEBHOOK_SECRET\"".to_string(),
        ),
    };
    let base64 = signature.is_some_and(|s| s.encoding == "base64");
    let digest = if base64 {
        "openssl dgst -sha256 -hmac \"$SECRET\" -binary | base64"
    } else {
        "openssl dgst -sha256 -hmac \"$SECRET\" -hex | sed 's/^.* //'"
    };
    let header = sh(&header);
    format!(
        "SECRET={secret}\nBODY={body}\nSIG=$(printf '%s' \"$BODY\" | {digest})\n\
         curl -X POST {url} \\\n  -H 'Content-Type: application/json' \\\n  \
         -H {header}\"$SIG\" \\\n  -d \"$BODY\""
    )
}

impl Client {
    pub async fn list_schedules(&self) -> Result<Vec<ScheduleInfo>, ClientError> {
        let doc: ScheduleListDoc = decode(self.get(&format!("{V2}/schedules")).await?)?;
        Ok(doc.schedules)
    }

    /// Validate `schedule` (and `prompt`) and list its next fire times.
    pub async fn preview_schedule(
        &self,
        schedule: &ScheduleSpec,
        prompt: Option<&str>,
    ) -> Result<SchedulePreview, ClientError> {
        let mut body = json!({ "schedule": schedule });
        if let Some(prompt) = prompt {
            body["prompt"] = json!(prompt);
        }
        decode(
            self.post::<_, Value>(&format!("{V2}/schedules/preview"), &body)
                .await?,
        )
    }

    /// A new schedule on `thread_id` (one of the caller's threads).
    pub async fn create_schedule(
        &self,
        thread_id: &str,
        write: &ScheduleWrite,
    ) -> Result<ScheduleInfo, ClientError> {
        let mut body = write.body();
        body["thread_id"] = json!(thread_id);
        body["client_request_id"] = json!(client_action_id());
        if write.schedule.is_webhook() && write.signed != Some(true) {
            body.as_object_mut().map(|b| b.remove("signature"));
        }
        let doc: ScheduleDoc = decode(
            self.post::<_, Value>(&format!("{V2}/schedules"), &body)
                .await?,
        )?;
        Ok(doc.schedule)
    }

    pub async fn update_schedule(
        &self,
        id: &str,
        write: &ScheduleWrite,
    ) -> Result<ScheduleInfo, ClientError> {
        let path = format!("{V2}/schedules/{}", seg(id));
        let doc: ScheduleDoc = decode(self.post::<_, Value>(&path, &write.body()).await?)?;
        Ok(doc.schedule)
    }

    /// Pause or resume without touching anything else.
    pub async fn set_schedule_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<ScheduleInfo, ClientError> {
        let path = format!("{V2}/schedules/{}", seg(id));
        let body = json!({ "enabled": enabled });
        let doc: ScheduleDoc = decode(self.post::<_, Value>(&path, &body).await?)?;
        Ok(doc.schedule)
    }

    pub async fn delete_schedule(&self, id: &str) -> Result<(), ClientError> {
        let path = format!("{V2}/schedules/{}", seg(id));
        let value: Value = self
            .send(self.request(reqwest::Method::DELETE, &path))
            .await?;
        decode::<Value>(value).map(|_| ())
    }

    /// Run once now; answers the thread the run was posted into.
    pub async fn run_schedule(&self, id: &str) -> Result<Option<String>, ClientError> {
        let path = format!("{V2}/schedules/{}/run", seg(id));
        let value: Value = decode(self.post::<_, Value>(&path, &json!({})).await?)?;
        Ok(value["runThreadId"].as_str().map(str::to_string))
    }

    /// A new signing secret; the URL stays. Reveal to read it.
    pub async fn rotate_schedule_secret(&self, id: &str) -> Result<ScheduleInfo, ClientError> {
        let path = format!("{V2}/schedules/{}/rotate-secret", seg(id));
        let doc: ScheduleDoc = decode(self.post::<_, Value>(&path, &json!({})).await?)?;
        Ok(doc.schedule)
    }

    /// The webhook's full URL and secret. Call only on an explicit Reveal.
    pub async fn reveal_schedule_secret(&self, id: &str) -> Result<ScheduleSecret, ClientError> {
        let path = format!("{V2}/schedules/{}/secret", seg(id));
        decode(self.get::<Value>(&path).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_match_the_backend_wire() {
        let weekdays = ScheduleSpec::FixedTime {
            time_of_day: "09:00".into(),
            weekdays: vec![1, 2, 3, 4, 5],
            timezone: Some("Europe/Zurich".into()),
        };
        assert_eq!(
            serde_json::to_value(&weekdays).unwrap(),
            json!({"type": "fixed_time", "timeOfDay": "09:00", "weekdays": [1, 2, 3, 4, 5],
                   "timezone": "Europe/Zurich"})
        );
        assert_eq!(
            serde_json::to_value(ScheduleSpec::Interval {
                every_ms: 7_200_000
            })
            .unwrap(),
            json!({"type": "interval", "everyMs": 7_200_000})
        );
        assert_eq!(
            serde_json::to_value(ScheduleSpec::Webhook).unwrap(),
            json!({"type": "webhook"})
        );
        let cron: ScheduleSpec = serde_json::from_value(
            json!({"type": "cron", "expression": "0 9 * * 1-5", "timezone": "UTC"}),
        )
        .unwrap();
        assert_eq!(cron.timezone(), Some("UTC"));
    }

    #[test]
    fn presets_build_specs_and_come_back_from_them() {
        let mut draft = ScheduleDraft {
            preset: Preset::Weekly,
            time: "7:5".into(),
            days: vec![5, 1, 5],
            timezone: Some("Europe/Zurich".into()),
            ..ScheduleDraft::default()
        };
        let spec = draft.to_spec().unwrap();
        assert_eq!(
            spec,
            ScheduleSpec::FixedTime {
                time_of_day: "07:05".into(),
                weekdays: vec![1, 5],
                timezone: Some("Europe/Zurich".into()),
            }
        );
        let back = ScheduleDraft::from_spec(&spec).unwrap();
        assert_eq!(back.preset, Preset::Weekly);
        assert_eq!(back.days, vec![1, 5]);

        draft.days.clear();
        assert_eq!(draft.to_spec().unwrap_err().field, DraftField::Days);
        draft.preset = Preset::Daily;
        draft.time = "25:00".into();
        assert_eq!(draft.to_spec().unwrap_err().field, DraftField::Time);
        draft.time = "09:00".into();
        assert!(matches!(
            draft.to_spec().unwrap(),
            ScheduleSpec::FixedTime { weekdays, .. } if weekdays.is_empty()
        ));
        draft.preset = Preset::Weekdays;
        let weekdays = draft.to_spec().unwrap();
        assert_eq!(
            ScheduleDraft::from_spec(&weekdays).unwrap().preset,
            Preset::Weekdays
        );

        draft.preset = Preset::EveryHours;
        draft.hours = "1.5".into();
        let every = draft.to_spec().unwrap();
        assert_eq!(
            every,
            ScheduleSpec::Interval {
                every_ms: 5_400_000
            }
        );
        assert_eq!(ScheduleDraft::from_spec(&every).unwrap().hours, "1.5");
        draft.hours = "0".into();
        assert_eq!(draft.to_spec().unwrap_err().field, DraftField::Hours);

        draft.preset = Preset::Cron;
        draft.cron = "  0  9 * *   1-5 ".into();
        assert_eq!(
            draft.to_spec().unwrap(),
            ScheduleSpec::Cron {
                expression: "0 9 * * 1-5".into(),
                timezone: Some("Europe/Zurich".into()),
            }
        );
        draft.cron = " ".into();
        assert_eq!(draft.to_spec().unwrap_err().field, DraftField::Cron);
        assert!(ScheduleDraft::from_spec(&ScheduleSpec::Webhook).is_none());
    }

    #[test]
    fn list_rows_decode_and_keep_unknown_specs() {
        let doc = json!({"ok": true, "schedules": [
            {"id": "s1", "title": "Hook", "prompt": "p", "schedule": {"type": "webhook"},
             "describe": "on each webhook request", "enabled": true, "threadId": "t1",
             "bindToCurrentThread": true, "nextRunAt": null, "lastRunAt": 5, "runCount": 1,
             "lastError": null, "createdAt": 1, "lastRunThreadId": "t1",
             "lastDelivery": {"status": "unauthorized", "at": 9},
             "webhookUrlMasked": "http://x/api/orchestrator/webhooks/s1.••••",
             "signature": {"header": "x-zlo-signature", "encoding": "hex", "prefix": "sha256=",
                           "hasSecret": true},
             "profileId": "work", "provider": null, "model": null},
            {"id": "s2", "title": "Odd", "prompt": "p", "schedule": {"type": "lunar"},
             "enabled": false, "threadId": "t2", "nextRunAt": null, "lastRunAt": null,
             "lastError": "boom", "createdAt": 1}
        ]});
        let rows: ScheduleListDoc = decode(doc).unwrap();
        let hook = &rows.schedules[0];
        assert!(hook.is_webhook());
        assert_eq!(hook.spec(), Some(ScheduleSpec::Webhook));
        assert_eq!(
            hook.last_delivery.as_ref().map(|d| &d.status),
            Some(&DeliveryStatus::Unauthorized)
        );
        assert!(hook.signature.as_ref().is_some_and(|s| s.has_secret));
        assert_eq!(rows.schedules[1].spec(), None);
        let refused: Result<ScheduleListDoc, _> =
            decode(json!({"ok": false, "error": "invalid", "message": "bad cron"}));
        assert!(
            matches!(refused, Err(ClientError::Refused { message, .. }) if message == "bad cron")
        );
    }

    #[test]
    fn write_bodies_ask_for_a_generated_secret_and_never_carry_one() {
        let write = ScheduleWrite {
            title: "Deploys".into(),
            prompt: "A deploy: {{body}}".into(),
            schedule: ScheduleSpec::Webhook,
            bind_to_current_thread: false,
            enabled: true,
            signed: Some(true),
        };
        let body = write.body();
        assert_eq!(body["signature"]["generate"], true);
        assert_eq!(body["signature"]["header"], SIGNATURE_HEADER);
        assert!(body["signature"].get("secret").is_none());
        let keep = ScheduleWrite {
            signed: None,
            ..write.clone()
        };
        assert!(keep.body().get("signature").is_none());
        let clock = ScheduleWrite {
            schedule: ScheduleSpec::Interval {
                every_ms: 3_600_000,
            },
            ..write
        };
        assert!(clock.body()["signature"].is_null());
    }

    /// Run `script` in `sh` with `curl` and `openssl` stubbed: curl prints
    /// each argument as `<arg>`, openssl reports its key as `KEY<key>` on
    /// stderr. Returns (stdout, stderr).
    fn run_stubbed(script: &str) -> (String, String) {
        let stubs = r#"curl() { for a in "$@"; do printf '<%s>\n' "$a"; done; }
openssl() { cat >/dev/null; printf 'KEY<%s>\n' "$4" >&2; echo '(stdin)= deadbeef'; }
"#;
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{stubs}{script}"))
            .env("ZLO_WEBHOOK_URL", "http://env/url")
            .env("ZLO_WEBHOOK_SECRET", "env-secret")
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    fn curl_examples_quote_hostile_values_so_nothing_runs_on_paste() {
        let sig = RevealedSignature {
            header: "x-sig$(echo PWNED-HEADER >&2)".into(),
            prefix: "`echo PWNED-PREFIX >&2`".into(),
            encoding: "hex".into(),
            secret: "k'; echo PWNED-SECRET >&2; '\n$(echo PWNED-NL >&2)".into(),
        };
        let url = "http://h/x'$(echo PWNED-URL >&2)'";
        let (stdout, stderr) = run_stubbed(&curl_example(Some(url), Some(&sig), true));
        assert!(
            !stderr.lines().any(|l| l.starts_with("PWNED")),
            "payload ran: {stderr}"
        );
        assert!(stdout.contains(&format!("<{url}>")), "{stdout}");
        assert!(
            stdout.contains("<x-sig$(echo PWNED-HEADER >&2): `echo PWNED-PREFIX >&2`deadbeef>"),
            "{stdout}"
        );
        assert!(stderr.contains(&format!("KEY<{}>", sig.secret)), "{stderr}");

        // Unrevealed: the URL and secret come from the environment.
        let (stdout, stderr) = run_stubbed(&curl_example(None, None, true));
        assert!(stdout.contains("<http://env/url>"), "{stdout}");
        assert!(
            stdout.contains("<x-zlo-signature: sha256=deadbeef>"),
            "{stdout}"
        );
        assert!(stderr.contains("KEY<env-secret>"), "{stderr}");
        let (stdout, _) = run_stubbed(&curl_example(Some(url), None, false));
        assert!(stdout.contains(&format!("<{url}>")), "{stdout}");
    }

    #[test]
    fn last_run_reports_queued_never_ok() {
        // The row only knows the run was queued (`post_schedule`); a turn
        // that later fails or is stopped leaves it untouched.
        let mut row: ScheduleInfo = serde_json::from_value(json!({
            "id": "s1", "title": "t", "prompt": "p", "schedule": {"type": "webhook"},
            "enabled": true, "threadId": "t1", "nextRunAt": null, "lastRunAt": 5,
            "lastError": null}))
        .unwrap();
        assert_eq!(row.last_outcome(), LastOutcome::Queued(5));
        row.last_error = Some("thread_not_found".into());
        assert_eq!(
            row.last_outcome(),
            LastOutcome::Failed("thread_not_found".into())
        );
        row.last_run_at = None;
        row.last_error = None;
        assert_eq!(row.last_outcome(), LastOutcome::Never);
    }

    #[test]
    fn curl_examples_sign_the_body_and_secrets_stay_out_of_debug() {
        let sig = RevealedSignature {
            header: "x-zlo-signature".into(),
            prefix: "sha256=".into(),
            encoding: "hex".into(),
            secret: "s3cr3t".into(),
        };
        let shown = curl_example(Some("http://127.0.0.1:1/h"), Some(&sig), true);
        assert!(shown.contains("SECRET='s3cr3t'"), "{shown}");
        assert!(shown.contains("-hmac \"$SECRET\" -hex"));
        assert!(
            shown.contains("-H 'x-zlo-signature: sha256='\"$SIG\""),
            "{shown}"
        );
        let masked = curl_example(Some("http://127.0.0.1:1/h"), None, true);
        assert!(masked.contains("$ZLO_WEBHOOK_SECRET") && !masked.contains("s3cr3t"));
        assert!(!curl_example(Some("u"), None, false).contains("SIG"));
        let secret = ScheduleSecret {
            webhook_url: Some("http://x/hook.token".into()),
            signature: Some(sig),
        };
        let debug = format!("{secret:?}");
        assert!(
            !debug.contains("s3cr3t") && !debug.contains("token"),
            "{debug}"
        );
    }
}
