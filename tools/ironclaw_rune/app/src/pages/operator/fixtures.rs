//! Fixture data for the Operator scenes (`operator`, `operator-setup`,
//! `operator-config`, `operator-extensions`, `operator-logs`,
//! `operator-inspector`), decoded from gateway-shaped JSON.

use serde_json::json;

use super::{Tab, config, extensions, inspector, logs, setup};
use crate::pages::extensions::fixtures::decode;
use crate::state::{Conn, Page, State};

fn base(s: &mut State, tab: Tab) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Operator;
    s.pages.operator.tab = tab;
}

fn diagnostic(severity: &str, key: &str, message: &str) -> serde_json::Value {
    json!({
        "key": key,
        "severity": severity,
        "reason_code": "missing_value",
        "message": message,
        "owning_area": "config",
        "remediation": "Set it under Config, or in ~/.ironclaw/.env."
    })
}

fn log_lines() -> serde_json::Value {
    json!([
        { "id": "l3", "timestamp": "2026-11-07T11:02:03.120Z", "level": "error",
          "target": "ironclaw_llm::anthropic", "message": "request failed: 529 overloaded, retrying in 2 s" },
        { "id": "l2", "timestamp": "2026-11-07T11:02:01.004Z", "level": "warn",
          "target": "ironclaw_turns::scheduler", "message": "run r-81c2 waited 1.8 s for a slot" },
        { "id": "l1", "timestamp": "2026-11-07T11:01:59.500Z", "level": "info",
          "target": "ironclaw_webui", "message": "GET /api/v2/threads 200 (4 ms)" }
    ])
}

/// Status: readiness, the service with a confirm, doctor findings.
pub fn status(s: &mut State) {
    base(s, Tab::Status);
    let status = json!({
        "area": "status",
        "status": "available",
        "message": "Status read.",
        "operator_status": {
            "generated_at": "2026-11-07T11:00:00Z",
            "overall": "degraded",
            "checks": [
                { "id": "llm_provider", "status": "ready", "severity": "info",
                  "summary": "anthropic / claude-opus-5-5 answers." },
                { "id": "secret_store", "status": "ready", "severity": "info",
                  "summary": "Encrypted store unlocked." },
                { "id": "web_search", "status": "not_configured", "severity": "warning",
                  "summary": "No search provider key.",
                  "remediation": "Add a Brave or Tavily key under Config." }
            ]
        }
    });
    let service = json!({
        "action": "status",
        "state": "stopped",
        "message": "launchd job not loaded"
    });
    let doctor = json!({
        "area": "diagnostics",
        "status": "available",
        "message": "2 findings.",
        "diagnostics": [
            diagnostic("warning", "tools.web_search.api_key", "Web search has no API key."),
            diagnostic("error", "channels.telegram.bot_token", "The Telegram bot token is missing.")
        ]
    });
    let panel = &mut s.pages.operator.status;
    panel.status = decode("operator status", status);
    panel.service = decode("service lifecycle", service);
    panel.diagnostics = decode("operator diagnostics", doctor);
    panel.confirm_stop = true;
}

pub fn setup(s: &mut State) {
    base(s, Tab::Setup);
    let resp = json!({
        "area": "setup",
        "status": "incomplete",
        "message": "One step left.",
        "active_provider_id": "anthropic",
        "active_model": "claude-opus-5-5",
        "steps": [
            { "name": "llm_provider", "status": "complete", "message": "anthropic is active." },
            { "name": "secret_store", "status": "complete", "message": "Unlocked." },
            { "name": "first_channel", "status": "required", "message": "Connect a chat app." },
            { "name": "service", "status": "unsupported", "message": "No service manager here." }
        ]
    });
    s.pages.operator.setup = Some(setup::SetupPanel {
        setup: decode("operator setup", resp),
        ..Default::default()
    });
}

pub fn config(s: &mut State) {
    base(s, Tab::Config);
    let list = json!({
        "entries": [
            { "key": "agent.max_iterations", "value": 40, "source": "settings",
              "redacted": false, "mutable": true },
            { "key": "approvals.default", "value": { "state": "ask_each_time" },
              "source": "default", "redacted": false, "mutable": true },
            { "key": "llm.anthropic.api_key", "value": null, "source": "secrets",
              "redacted": true, "mutable": false }
        ],
        "precedence": ["env", "settings", "default"],
        "diagnostics": [diagnostic("info", "agent.max_iterations", "Higher than the default of 25.")]
    });
    let mut panel = config::ConfigPanel {
        list: decode("operator config", list),
        notice: Some("Saved agent.max_iterations".into()),
        ..Default::default()
    };
    panel.editor = Some(config::Editor {
        key: "approvals.default".into(),
        input: rune::TextEdit::new("{\"state\":\"always_allow\"}"),
        confirming: true,
        saving: false,
    });
    s.pages.operator.config = Some(panel);
}

pub fn extension_config(s: &mut State) {
    base(s, Tab::Extensions);
    let groups: Option<zlo_client::operator::AdminConfigGroups> = decode(
        "extension configuration",
        json!({ "groups": [
            {
                "group_id": "slack_app",
                "display_name": "Slack app",
                "description": "The Slack app every workspace member connects through.",
                "revision": 3,
                "complete": false,
                "fields": [
                    { "handle": "client_id", "label": "Client ID", "secret": false,
                      "required": true, "provided": true, "value": "1234.5678" },
                    { "handle": "client_secret", "label": "Client secret", "secret": true,
                      "required": true, "provided": false,
                      "description": "From api.slack.com → Basic Information." }
                ],
                "used_by": [{ "display_name": "Slack", "installed": true }]
            }
        ]}),
    );
    let mut panel = extensions::ExtensionConfigPanel {
        loaded: true,
        ..Default::default()
    };
    panel.groups = groups
        .map(|g| g.groups.into_iter().map(extensions::form).collect())
        .unwrap_or_default();
    s.pages.operator.extensions = Some(panel);
}

pub fn logs(s: &mut State) {
    base(s, Tab::Logs);
    let page: Option<Vec<_>> = decode("log entries", log_lines());
    s.pages.operator.logs = Some(logs::LogsPanel {
        entries: page.unwrap_or_default(),
        next_cursor: Some("c1".into()),
        source: Some("memory ring (2000 lines)".into()),
        run: Some("r-81c2f0aa-77".into()),
        loaded: true,
        ..Default::default()
    });
}

/// The inspector on a run whose diagnostics are gone, with thread logs.
pub fn inspector(s: &mut State) {
    base(s, Tab::Inspector);
    let threads = json!([
        { "thread_id": "t1", "title": "Port the frontend to Rune", "updated_at": "2026-11-07T10:58:00Z" },
        { "thread_id": "t2", "title": "Fix the flaky integration test", "updated_at": "2026-11-06T17:20:00Z" }
    ]);
    let now = chrono::Utc::now();
    let panel = inspector::InspectorPanel {
        threads: decode("thread summaries", threads).unwrap_or_default(),
        thread: Some("t1".into()),
        runs: vec![
            inspector::RunRef {
                id: "r-2".into(),
                at: Some(now - chrono::Duration::minutes(4)),
            },
            inspector::RunRef {
                id: "r-1".into(),
                at: Some(now - chrono::Duration::hours(2)),
            },
        ],
        run: Some("r-2".into()),
        snapshot: Some(None),
        prompt: Some(Ok(None)),
        logs: decode("thread logs", log_lines()),
        live_note: Some("Following live".into()),
        ..Default::default()
    };
    s.pages.operator.inspector = Some(panel);
}
