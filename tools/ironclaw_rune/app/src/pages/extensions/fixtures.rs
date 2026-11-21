//! Fixture data for the Extensions scene (`--scene extensions`), decoded
//! from gateway-shaped JSON so it follows the wire contract.

use serde::de::DeserializeOwned;
use serde_json::json;

use super::{McpForm, Pairing, Setup, setup_inputs};
use crate::state::{Conn, Page, State};

/// Decode fixture JSON; a contract change shows up as a warning and an
/// empty fixture rather than a crash.
pub fn decode<T: DeserializeOwned>(what: &str, value: serde_json::Value) -> Option<T> {
    match serde_json::from_value(value) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(fixture = what, error = %e, "fixture does not decode");
            None
        }
    }
}

fn package(id: &str) -> serde_json::Value {
    json!({ "kind": "extension", "id": id })
}

pub fn extensions(s: &mut State) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Extensions;
    let installed = json!([
        {
            "package_ref": package("telegram"),
            "display_name": "Telegram",
            "runtime": "wasm",
            "description": "Chat with IronClaw from Telegram",
            "tools": ["telegram_send"],
            "installation_state": "active",
            "version": "0.4.1",
            "surfaces": [{ "kind": "channel", "inbound": true, "outbound": true }]
        },
        {
            "package_ref": package("linear"),
            "display_name": "Linear",
            "runtime": "mcp",
            "description": "Issues, projects and cycles from Linear",
            "tools": ["list_issues", "create_issue", "update_issue"],
            "installation_state": "setup_needed",
            "surfaces": [{ "kind": "tool" }]
        },
        {
            "package_ref": package("github"),
            "display_name": "GitHub",
            "runtime": "wasm",
            "description": "Repositories, pull requests and issues",
            "tools": ["gh_pr_list"],
            "installation_state": "active",
            "activation_error": "The token was rejected (401). Set it up again.",
            "version": "1.2.0"
        }
    ]);
    let catalog = json!([
        {
            "package_ref": package("slack"),
            "display_name": "Slack",
            "runtime": "wasm",
            "description": "Talk to IronClaw in Slack channels and DMs",
            "installed": false
        },
        {
            "package_ref": package("gmail"),
            "display_name": "Gmail",
            "runtime": "first_party",
            "description": "Read, search and draft email",
            "installed": false
        }
    ]);
    let setup = json!({
        "package_ref": package("linear"),
        "phase": "setup_needed",
        "message": "Paste a Linear API key to finish.",
        "secrets": [{
            "name": "linear_api_key",
            "provider": "linear",
            "prompt": "Linear API key",
            "optional": false,
            "provided": false,
            "setup": { "kind": "manual_token" }
        }],
        "fields": [{
            "name": "team",
            "prompt": "Default team",
            "optional": true,
            "placeholder": "ENG"
        }],
        "onboarding": { "setup_url": "https://linear.app/settings/api" }
    });
    let pending = json!({
        "connected": false,
        "pending": {
            "code": "QX7-42B",
            "deep_link": "https://t.me/ironclaw_bot?start=QX7-42B",
            "expires_at": "2026-11-07T12:30:00Z"
        }
    });
    let ext = &mut s.pages.extensions;
    ext.loaded = true;
    ext.installed = decode::<Vec<_>>("installed extensions", installed).unwrap_or_default();
    ext.catalog = decode::<Vec<_>>("extension registry", catalog).unwrap_or_default();
    ext.notice = Some("Installed Linear. Add its key to finish.".into());
    if let Some(view) = decode("extension setup", setup) {
        ext.setup = Some(Setup {
            id: "linear".into(),
            inputs: setup_inputs(&view),
            view: Some(view),
            submitting: false,
        });
    }
    ext.pairing = Some(Pairing {
        id: "telegram".into(),
        status: decode("pairing status", pending),
        busy: false,
    });
    ext.mcp = Some(McpForm {
        name: rune::TextEdit::new("Sentry"),
        endpoint: rune::TextEdit::new("https://mcp.sentry.dev/mcp"),
        auth: 3,
        submitting: false,
    });
}

/// Plugins in each held state: Linear waits for consent, Sentry changed
/// after it was allowed, Telegram runs.
fn plugin_list(s: &mut State, light: bool) {
    crate::workspace::fixtures::fixture_shell(s);
    s.prefs = Default::default();
    crate::plugins::fixtures::scheme(s, light);
    s.page = Page::Extensions;
    crate::plugins::fixtures::slots(s);
    let installed = json!([
        {
            "package_ref": package("linear"),
            "display_name": "Linear",
            "runtime": "wasm",
            "description": "Issues, projects and cycles from Linear",
            "tools": ["list_issues", "create_issue", "close_issue"],
            "installation_state": "consent_required",
            "version": "0.3.0",
            "consent": {
                "digest": "9f2c41e07ab3d55e1c0f",
                "requests": [
                    { "kind": "tool", "title": "Use 3 tools", "detail": "It can list, create and close issues when the agent asks." },
                    { "kind": "network", "title": "Reach api.linear.app", "detail": "It sends requests to Linear and nowhere else." },
                    { "kind": "credential", "title": "Use your Linear key", "detail": "The backend adds the key to its requests; the plugin never sees it." },
                    { "kind": "ui_slot", "title": "Add a tab and a badge", "detail": "It shows a Linear tab beside chats and a badge on each session." },
                    { "kind": "setting", "title": "Keep 4 settings", "detail": "It stores its own options, which you can change in Settings." }
                ]
            }
        },
        {
            "package_ref": package("sentry"),
            "display_name": "Sentry",
            "runtime": "mcp",
            "description": "Errors and releases from Sentry",
            "tools": ["list_issues"],
            "installation_state": "tampered",
            "version": "1.1.0",
            "consent": {
                "digest": "07d1aa9e3b",
                "requests": [
                    { "kind": "network", "title": "Reach sentry.io and errors.example.net", "detail": "The new version also talks to a second server." },
                    { "kind": "hook", "title": "Watch every turn", "detail": "It sees each turn as it ends." }
                ]
            }
        },
        {
            "package_ref": package("telegram"),
            "display_name": "Telegram",
            "runtime": "wasm",
            "description": "Chat with IronClaw from Telegram",
            "tools": ["telegram_send"],
            "installation_state": "active",
            "version": "0.4.1",
            "surfaces": [{ "kind": "channel", "inbound": true, "outbound": true }]
        }
    ]);
    let ext = &mut s.pages.extensions;
    ext.loaded = true;
    ext.installed = decode::<Vec<_>>("installed plugins", installed).unwrap_or_default();
}

/// The consent sheet open on Linear, over the list.
pub fn plugins_consent(s: &mut State, light: bool) {
    plugin_list(s, light);
    super::plugin::open_consent(s, "linear".into());
}

/// The list itself: a plugin waiting for consent and one that changed.
pub fn plugins_list(s: &mut State) {
    plugin_list(s, false);
}

/// Linear's own page: settings of all four kinds (one save refused) and
/// its `settings.page` slot.
pub fn plugin_settings(s: &mut State, light: bool) {
    plugin_list(s, light);
    if let Some(e) = s.pages.extensions.installed.first_mut() {
        e.installation_state = zlo_client::plugins::PluginState::Active;
    }
    let list = json!({ "settings": [
        { "id": "notify", "kind": "switch", "label": "Comment when a chat closes an issue",
          "description": "Adds a short note with a link back to the chat.",
          "section": null, "options": [], "default": true, "value": null },
        { "id": "link", "kind": "switch", "label": "Link issues named in a message",
          "description": null, "section": null, "options": [], "default": false, "value": false },
        { "id": "team", "kind": "choice", "label": "Default team", "description": "Where new issues go.",
          "section": "Issues", "options": [
            { "value": "eng", "label": "Engineering", "description": null },
            { "value": "des", "label": "Design", "description": null },
            { "value": "ops", "label": "Ops", "description": null }
          ], "default": "eng", "value": "eng" },
        { "id": "priority", "kind": "choice", "label": "Priority for new issues", "description": null,
          "section": "Issues", "options": [
            { "value": 0, "label": "None", "description": "Linear decides." },
            { "value": 2, "label": "High", "description": "Shows at the top of the team's queue." },
            { "value": 3, "label": "Medium", "description": null }
          ], "default": 0, "value": 2 },
        { "id": "prefix", "kind": "text", "label": "Title prefix", "description": "Put before every issue title the agent writes.",
          "section": "Issues", "options": [], "default": "", "value": "[zlo]" },
        { "id": "limit", "kind": "number", "label": "Issues in the tab", "description": "How many the Linear tab lists.",
          "section": "Issues", "options": [], "default": 10, "value": 25 }
    ]});
    let settings: zlo_client::plugins::PluginSettings =
        decode("plugin settings", list).unwrap_or_default();
    super::plugin::open_detail(s, "linear".into());
    if let Some(d) = s.pages.extensions.detail.as_mut() {
        d.inputs = settings
            .settings
            .iter()
            .filter(|x| {
                matches!(
                    x.kind,
                    zlo_client::plugins::SettingKind::Text
                        | zlo_client::plugins::SettingKind::Number
                )
            })
            .map(|x| {
                (
                    x.id.clone(),
                    rune::TextEdit::new(super::plugin::field_text(x.effective())),
                )
            })
            .collect();
        // Typed past the plugin's limit and refused.
        if let Some((_, edit)) = d.inputs.iter_mut().find(|(id, _)| id == "limit") {
            edit.replace_text("250");
        }
        d.errors
            .insert("limit".into(), "Pick a number from 1 to 50.".into());
        d.settings = super::plugin::SettingsLoad::Ready(settings.settings);
    }
    crate::plugins::fixtures::put(s, "about", None, crate::plugins::fixtures::page_tree());
}
