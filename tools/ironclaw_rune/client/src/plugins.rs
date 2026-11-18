//! Plugins: consent, per-plugin settings and the UI slot protocol.
//!
//! A plugin is an extension package; the backend does everything on its
//! behalf and the app only draws what comes back. These shapes are additive
//! to the extension lifecycle responses (`installation_state`, `consent`)
//! plus three route families:
//!
//! - `GET  /extensions/{id}/settings`, `PUT /extensions/{id}/settings/{setting}`
//! - `POST /extensions/{id}/consent` (lifecycle command `grant_consent`)
//! - `GET  /plugin-ui/slots`, `POST /plugin-ui/render`, `POST /plugin-ui/event`
//!
//! An older backend answers 404 on all of them; callers treat
//! [`ClientError::is_not_found`] as "no plugins here" and show nothing.
//! The node tree a plugin returns (`reborn.plugin_ui.v1`) is untrusted and
//! decoded by [`crate::plugin_ui`] under hard limits.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{Client, V2, seg};
use crate::error::{ApiErrorBody, ClientError};
use crate::plugin_ui::{self, Node};

// --- lifecycle state and consent ---

/// An installed extension's lifecycle state. Mirrors
/// `LifecyclePublicState` plus the plugin states; a state this app does
/// not know yet decodes as [`PluginState::Other`] so the list still loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginState {
    Uninstalled,
    SetupNeeded,
    Active,
    /// Installed, but nothing runs until the person allows what it asks for.
    ConsentRequired,
    /// The package changed after it was allowed (its digest no longer
    /// matches the one consent was pinned to).
    Tampered,
    #[serde(other)]
    Other,
}

/// What a plugin asks for, pinned to the package digest it was read from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Consent {
    pub digest: String,
    #[serde(default)]
    pub requests: Vec<ConsentRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ConsentRequest {
    pub kind: RequestKind,
    pub title: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Tool,
    Network,
    Credential,
    Hook,
    UiSlot,
    Setting,
    Service,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize)]
struct GrantConsent<'a> {
    extension_id: &'a str,
    digest: &'a str,
}

/// The reply to a lifecycle command; only the message is shown.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LifecycleReply {
    #[serde(default)]
    pub message: Option<String>,
}

// --- settings ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingKind {
    Switch,
    Choice,
    Text,
    Number,
    /// A kind this app cannot draw: the setting is left out.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SettingOption {
    pub value: Value,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// One setting as the plugin's schema declares it, with its current value.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PluginSetting {
    pub id: String,
    pub kind: SettingKind,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub options: Vec<SettingOption>,
    #[serde(default)]
    pub default: Value,
    #[serde(default)]
    pub value: Value,
}

impl PluginSetting {
    /// The value in effect: the saved one, else the default.
    pub fn effective(&self) -> &Value {
        if self.value.is_null() {
            &self.default
        } else {
            &self.value
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PluginSettings {
    #[serde(default)]
    pub settings: Vec<PluginSetting>,
}

#[derive(Debug, Clone, Serialize)]
struct SettingValue<'a> {
    value: &'a Value,
}

// --- UI slots ---

/// Where a slot mounts in the app. Unknown mount points are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum SlotKind {
    #[serde(rename = "panel.tab")]
    PanelTab,
    #[serde(rename = "sidebar.session_row.badge")]
    SessionBadge,
    #[serde(rename = "settings.page")]
    SettingsPage,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Slot {
    pub extension_id: String,
    pub slot_id: String,
    pub slot: SlotKind,
    pub title: String,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SlotList {
    #[serde(default)]
    pub slots: Vec<Slot>,
}

/// What a render or event is about (`context` on the wire).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub struct SlotContext {
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RenderRequest<'a> {
    extension_id: &'a str,
    slot_id: &'a str,
    context: &'a SlotContext,
}

#[derive(Debug, Clone, Serialize)]
struct EventRequest<'a> {
    extension_id: &'a str,
    slot_id: &'a str,
    context: &'a SlotContext,
    action: &'a str,
    payload: &'a Value,
}

/// A render or event reply: a decoded tree, or the plugin's plain reason
/// (also used when the tree breaks the client-side limits).
#[derive(Debug, Clone, PartialEq)]
pub enum UiReply {
    Tree(Node),
    Error(String),
}

#[derive(Debug, Deserialize)]
struct RawReply {
    #[serde(default)]
    tree: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

impl UiReply {
    /// Decode `{ "tree": node }` or `{ "error": reason }`.
    pub fn from_json(text: &str) -> Result<Self, ClientError> {
        let raw: RawReply =
            serde_json::from_str(text).map_err(|e| ClientError::Decode(e.to_string()))?;
        Ok(match (raw.error, raw.tree) {
            (Some(reason), _) => Self::Error(reason),
            (None, Some(tree)) => match plugin_ui::decode(&tree) {
                Ok(node) => Self::Tree(node),
                Err(e) => Self::Error(e.to_string()),
            },
            (None, None) => Self::Error("The plugin sent nothing to show.".into()),
        })
    }
}

// --- the calls ---

impl Client {
    /// The plugin's settings schema with current values.
    pub async fn plugin_settings(&self, extension_id: &str) -> Result<PluginSettings, ClientError> {
        self.get(&format!("{V2}/extensions/{}/settings", seg(extension_id)))
            .await
    }

    /// Save one setting; a refusal comes back as [`ClientError::Api`] whose
    /// [`plain_reason`] is the plugin's own sentence.
    pub async fn put_plugin_setting(
        &self,
        extension_id: &str,
        setting_id: &str,
        value: &Value,
    ) -> Result<PluginSetting, ClientError> {
        let path = format!(
            "{V2}/extensions/{}/settings/{}",
            seg(extension_id),
            seg(setting_id)
        );
        let text = self
            .send_plain(
                self.request(Method::PUT, &path)
                    .json(&SettingValue { value }),
            )
            .await?;
        serde_json::from_str(&text).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Allow what the plugin asks for, at the digest the person reviewed.
    pub async fn grant_consent(
        &self,
        extension_id: &str,
        digest: &str,
    ) -> Result<LifecycleReply, ClientError> {
        let body = GrantConsent {
            extension_id,
            digest,
        };
        self.post(
            &format!("{V2}/extensions/{}/consent", seg(extension_id)),
            &body,
        )
        .await
    }

    pub async fn plugin_ui_slots(&self) -> Result<SlotList, ClientError> {
        self.get(&format!("{V2}/plugin-ui/slots")).await
    }

    pub async fn plugin_ui_render(
        &self,
        extension_id: &str,
        slot_id: &str,
        context: &SlotContext,
    ) -> Result<UiReply, ClientError> {
        let body = RenderRequest {
            extension_id,
            slot_id,
            context,
        };
        let text = self
            .send_plain(
                self.request(Method::POST, &format!("{V2}/plugin-ui/render"))
                    .json(&body),
            )
            .await?;
        UiReply::from_json(&text)
    }

    pub async fn plugin_ui_event(
        &self,
        extension_id: &str,
        slot_id: &str,
        context: &SlotContext,
        action: &str,
        payload: &Value,
    ) -> Result<UiReply, ClientError> {
        let body = EventRequest {
            extension_id,
            slot_id,
            context,
            action,
            payload,
        };
        let text = self
            .send_plain(
                self.request(Method::POST, &format!("{V2}/plugin-ui/event"))
                    .json(&body),
            )
            .await?;
        UiReply::from_json(&text)
    }

    /// Like `send`, but a refusal keeps the plugin's sentence as it is
    /// (the shared envelope humanizes snake_case codes, which would mangle
    /// a plain reason).
    async fn send_plain(&self, req: reqwest::RequestBuilder) -> Result<String, ClientError> {
        let resp = req
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if status.is_success() {
            return Ok(text);
        }
        Err(match prose_reason(&text) {
            Some(message) => ClientError::Api {
                status: status.as_u16(),
                message,
                body: Box::new(ApiErrorBody::default()),
            },
            None => ClientError::from_status(status.as_u16(), &text),
        })
    }
}

/// A sentence in an error body (`error`, `message` or `reason`), as the
/// plugin wrote it. Codes (no spaces) are left to the shared humanizer.
fn prose_reason(text: &str) -> Option<String> {
    let body: Value = serde_json::from_str(text).ok()?;
    ["error", "message", "reason"].iter().find_map(|k| {
        body.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| s.contains(' ') && s.len() <= 300)
            .map(str::to_string)
    })
}

/// What to show inline for a failed plugin call: the reason alone, without
/// the HTTP status the log keeps.
pub fn plain_reason(e: &ClientError) -> String {
    match e {
        ClientError::Api { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::connection::{Connection, Token};
    use crate::test_http::{serve, serve_routes};

    fn client(base: &str) -> Client {
        Client::new(Connection {
            base_url: base.to_string(),
            token: Token::new("t").unwrap(),
            token_source: "test".into(),
        })
        .unwrap()
    }

    #[test]
    fn plugin_states_decode_and_unknown_ones_do_not_break_the_list() {
        for (wire, want) in [
            ("consent_required", PluginState::ConsentRequired),
            ("tampered", PluginState::Tampered),
            ("active", PluginState::Active),
            ("setup_needed", PluginState::SetupNeeded),
            ("quarantined_by_a_newer_backend", PluginState::Other),
        ] {
            let got: PluginState = serde_json::from_value(json!(wire)).unwrap();
            assert_eq!(got, want, "{wire}");
        }
    }

    #[test]
    fn consent_decodes_every_request_kind() {
        let consent: Consent = serde_json::from_value(json!({
            "digest": "9f2c",
            "requests": [
                { "kind": "tool", "title": "Run 3 tools", "detail": "It can list, create and close issues." },
                { "kind": "network", "title": "Reach api.linear.app", "detail": "It sends requests to Linear." },
                { "kind": "credential", "title": "Use your Linear key", "detail": "It reads the key you saved." },
                { "kind": "hook", "title": "Watch turns", "detail": "It sees each turn as it ends." },
                { "kind": "ui_slot", "title": "Add a side tab", "detail": "It shows a tab beside chats." },
                { "kind": "setting", "title": "Keep 2 settings", "detail": "It stores its own options." },
                { "kind": "service", "title": "Run in the background", "detail": "It keeps a worker running." },
                { "kind": "telepathy", "title": "Something new", "detail": "A kind this app does not know." }
            ]
        }))
        .unwrap();
        let kinds: Vec<_> = consent.requests.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RequestKind::Tool,
                RequestKind::Network,
                RequestKind::Credential,
                RequestKind::Hook,
                RequestKind::UiSlot,
                RequestKind::Setting,
                RequestKind::Service,
                RequestKind::Other,
            ]
        );
        assert_eq!(consent.digest, "9f2c");
    }

    #[test]
    fn settings_of_all_four_kinds_decode() {
        let list: PluginSettings = serde_json::from_value(json!({
            "settings": [
                { "id": "notify", "kind": "switch", "label": "Notify on close", "description": null,
                  "section": null, "options": [], "default": true, "value": null },
                { "id": "team", "kind": "choice", "label": "Team", "description": "Where new issues go.",
                  "section": "Issues", "options": [
                    { "value": "eng", "label": "Engineering", "description": null },
                    { "value": "des", "label": "Design", "description": "Product design" }
                  ], "default": "eng", "value": "des" },
                { "id": "prefix", "kind": "text", "label": "Title prefix", "description": null,
                  "section": "Issues", "options": [], "default": "", "value": "[zlo]" },
                { "id": "limit", "kind": "number", "label": "Issues to show", "description": null,
                  "section": null, "options": [], "default": 10, "value": 25 },
                { "id": "future", "kind": "colour_wheel", "label": "?", "options": [], "default": null, "value": null }
            ]
        }))
        .unwrap();
        let kinds: Vec<_> = list.settings.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                SettingKind::Switch,
                SettingKind::Choice,
                SettingKind::Text,
                SettingKind::Number,
                SettingKind::Other
            ]
        );
        assert_eq!(list.settings[0].effective(), &json!(true));
        assert_eq!(list.settings[1].effective(), &json!("des"));
        assert_eq!(list.settings[1].section.as_deref(), Some("Issues"));
        assert_eq!(
            list.settings[1].options[1].description.as_deref(),
            Some("Product design")
        );
    }

    #[test]
    fn slots_decode_and_unknown_mount_points_are_kept_as_other() {
        let list: SlotList = serde_json::from_value(json!({
            "slots": [
                { "extension_id": "linear", "slot_id": "issues", "slot": "panel.tab", "title": "Issues", "icon": "list" },
                { "extension_id": "linear", "slot_id": "state", "slot": "sidebar.session_row.badge", "title": "Issue", "icon": null },
                { "extension_id": "linear", "slot_id": "about", "slot": "settings.page", "title": "Linear", "icon": null },
                { "extension_id": "linear", "slot_id": "x", "slot": "statusbar.item", "title": "X", "icon": null }
            ]
        }))
        .unwrap();
        let kinds: Vec<_> = list.slots.iter().map(|s| s.slot).collect();
        assert_eq!(
            kinds,
            vec![
                SlotKind::PanelTab,
                SlotKind::SessionBadge,
                SlotKind::SettingsPage,
                SlotKind::Other
            ]
        );
    }

    #[test]
    fn a_reply_is_a_tree_or_a_plain_reason() {
        let tree =
            UiReply::from_json(r#"{"tree":{"type":"badge","text":"In review","tone":"accent"}}"#)
                .unwrap();
        assert!(matches!(tree, UiReply::Tree(Node::Badge { .. })));
        let err = UiReply::from_json(r#"{"error":"Linear is not reachable right now."}"#).unwrap();
        assert_eq!(
            err,
            UiReply::Error("Linear is not reachable right now.".into())
        );
    }

    #[tokio::test]
    async fn render_posts_the_slot_and_context() {
        let (base, seen) = serve(200, r#"{"tree":{"type":"text","text":"hi"}}"#).await;
        let ctx = SlotContext {
            thread_id: Some("t1".into()),
        };
        let reply = client(&base)
            .plugin_ui_render("linear", "issues", &ctx)
            .await
            .unwrap();
        assert!(matches!(reply, UiReply::Tree(Node::Text { .. })));
        let seen = seen.lock().unwrap();
        assert!(
            seen[0]
                .line
                .starts_with("POST /api/webchat/v2/plugin-ui/render")
        );
        let body: Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(
            body,
            json!({ "extension_id": "linear", "slot_id": "issues", "context": { "thread_id": "t1" } })
        );
    }

    #[tokio::test]
    async fn an_event_carries_the_action_and_payload() {
        let (base, seen) = serve(200, r#"{"tree":{"type":"divider"}}"#).await;
        let ctx = SlotContext::default();
        client(&base)
            .plugin_ui_event("linear", "issues", &ctx, "close", &json!({ "id": "ENG-1" }))
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert_eq!(
            body,
            json!({
                "extension_id": "linear", "slot_id": "issues", "context": { "thread_id": null },
                "action": "close", "payload": { "id": "ENG-1" }
            })
        );
    }

    #[tokio::test]
    async fn a_refused_setting_keeps_the_plugins_sentence() {
        let (base, seen) = serve(422, r#"{"error":"Pick a number from 1 to 50."}"#).await;
        let err = client(&base)
            .put_plugin_setting("linear", "limit", &json!(99))
            .await
            .unwrap_err();
        assert_eq!(plain_reason(&err), "Pick a number from 1 to 50.");
        let seen = seen.lock().unwrap();
        assert!(
            seen[0]
                .line
                .starts_with("PUT /api/webchat/v2/extensions/linear/settings/limit")
        );
        assert_eq!(seen[0].body, r#"{"value":99}"#);
    }

    #[tokio::test]
    async fn a_saved_setting_comes_back_updated() {
        let (base, _) = serve(
            200,
            r#"{"id":"limit","kind":"number","label":"Issues","default":10,"value":20}"#,
        )
        .await;
        let saved = client(&base)
            .put_plugin_setting("linear", "limit", &json!(20))
            .await
            .unwrap();
        assert_eq!(saved.value, json!(20));
    }

    #[tokio::test]
    async fn consent_is_granted_at_the_reviewed_digest() {
        let (base, seen) = serve(200, r#"{"success":true,"message":"Linear is allowed."}"#).await;
        let reply = client(&base).grant_consent("linear", "9f2c").await.unwrap();
        assert_eq!(reply.message.as_deref(), Some("Linear is allowed."));
        let seen = seen.lock().unwrap();
        assert!(
            seen[0]
                .line
                .starts_with("POST /api/webchat/v2/extensions/linear/consent")
        );
        let body: Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body, json!({ "extension_id": "linear", "digest": "9f2c" }));
    }

    #[tokio::test]
    async fn an_older_backend_answers_not_found() {
        let (base, _) = serve_routes(vec![]).await;
        let c = client(&base);
        assert!(c.plugin_ui_slots().await.unwrap_err().is_not_found());
        assert!(
            c.plugin_settings("linear")
                .await
                .unwrap_err()
                .is_not_found()
        );
    }
}
