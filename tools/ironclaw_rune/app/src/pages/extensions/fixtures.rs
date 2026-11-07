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
