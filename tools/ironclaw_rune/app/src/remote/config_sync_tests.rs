use std::time::Duration;

use zlo_client::environment_access::ENVIRONMENT_PROTOCOL_VERSION;
use zlo_client::environments::{Active, Route, SavedEnvironment};

use super::*;
use crate::jobs::harness::Pump;
use crate::onboarding::gateway::Computer;
use crate::remote::peers::{self, PeerConn};
use crate::remote::{config_sync, connect};
use crate::test_gateway::{Answer, Gateway};

const INBOX: &str = r#"{"rows":[],"profiles":[],"last_used_profile_id":null}"#;
const SESSION: &str = r#"{"tenant_id":"t","user_id":"u"}"#;

fn descriptor_json(id: &str, label: &str) -> String {
    serde_json::json!({
        "environment_id": id,
        "label": label,
        "server_version": "0.41.0",
        "protocol_version": ENVIRONMENT_PROTOCOL_VERSION,
        "pairing": ["one_time_code"],
        "tailnet_url": null,
        "addresses": [],
        "peer_links": false
    })
    .to_string()
}

fn manifest(id: &str, config: &str, prompts: &str) -> Answer {
    Answer::ok(
        serde_json::json!({
            "schema_version": 1,
            "environment_id": id,
            "os": "macos",
            "domains": {
                "config": {"hash": config, "item_count": 1},
                "prompts": {"hash": prompts, "item_count": 1}
            }
        })
        .to_string(),
    )
}

fn export() -> Answer {
    Answer::ok(
        serde_json::json!({
            "outcome": "exported",
            "schema_version": 1,
            "source_environment_id": "mac",
            "source_os": "macos",
            "generated_at": "2026-11-26T00:00:00Z",
            "domains": {
                "prompts": {"kind": "prompts", "hash": "p1", "system_md": "Be brief."}
            }
        })
        .to_string(),
    )
}

fn applied(restart: &[&str]) -> Answer {
    Answer::ok(
        serde_json::json!({
            "outcome": "completed",
            "schema_version": 1,
            "target_environment_id": "env-b",
            "domains": {
                "prompts": {"status": "applied", "applied_count": 1, "skipped_count": 0}
            },
            "restart_required": restart
        })
        .to_string(),
    )
}

/// `focused` has the focus on gateway A; Box B is a ready peer on gateway B.
fn two_computers(focused: Computer) -> (State, Pump, Gateway, Gateway) {
    let gw_a = Gateway::start(vec![Answer::ok(INBOX)]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(SESSION),
        Answer::ok(INBOX),
    ]);
    let mut s = crate::chat::fixtures::test_state();
    s.fixture = false;
    s.workspace.window_active = true;
    let mut pump = Pump::install(&mut s);
    s.remote.registry.run_local_backend = false;
    s.remote.registry.upsert(SavedEnvironment {
        environment_id: "env-b".into(),
        label: "Box B".into(),
        routes: vec![Route::for_origin(&gw_b.base_url)],
        legacy_token: false,
    });
    s.remote.tokens.set("env-b", "tok-b").expect("token b");
    s.remote.registry.active = match &focused {
        Computer::ThisMac => Active::ThisMac,
        other => Active::Environment(other.key()),
    };
    connect::adopt(&mut s, focused, None);
    s.rt.backend = Some(gw_a.backend());
    s.session = serde_json::from_str(SESSION).ok();
    s.conn = Conn::Ready;
    peers::sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        peers::get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));
    (s, pump, gw_a, gw_b)
}

fn settle(s: &mut State, pump: &mut Pump) {
    pump.run_for(s, Duration::from_millis(400));
}

fn text(s: &State) -> String {
    config_sync::line(s, "env-b", "Box B")
        .map(|l| l.text)
        .unwrap_or_default()
}

#[test]
fn a_connected_computer_gets_what_differs_once_and_says_what_is_left() {
    let (mut s, mut pump, gw_a, gw_b) = two_computers(Computer::ThisMac);
    gw_a.route("/config-sync/manifest", manifest("mac", "c1", "p1"));
    gw_a.route("/config-sync/export", export());
    gw_b.route("/config-sync/manifest", manifest("env-b", "c1", "p0"));
    gw_b.route("/config-sync/apply", applied(&["budget"]));

    config_sync::sync(&mut s);
    assert_eq!(text(&s), "Syncing settings from This Mac…");
    settle(&mut s, &mut pump);
    // Read from the Mac, written to the box, never the other way.
    assert_eq!(gw_a.requests_to("/config-sync/export"), 1);
    assert_eq!(gw_a.requests_to("/config-sync/apply"), 0);
    assert_eq!(gw_b.requests_to("/config-sync/apply"), 1);
    assert_eq!(gw_b.requests_to("/config-sync/export"), 0);
    assert_eq!(
        text(&s),
        "Settings synced. Restart IronClaw on Box B to finish (budget)"
    );

    // Nothing is due: the poll's call sends nothing.
    config_sync::sync(&mut s);
    settle(&mut s, &mut pump);
    assert_eq!(gw_b.requests_to("/config-sync/manifest"), 1);

    // "Sync now" checks again; it matches now and the restart note stays.
    gw_b.route("/config-sync/manifest", manifest("env-b", "c1", "p1"));
    config_sync::sync_now(&mut s, "env-b");
    settle(&mut s, &mut pump);
    assert_eq!(gw_b.requests_to("/config-sync/manifest"), 2);
    assert_eq!(gw_b.requests_to("/config-sync/apply"), 1);
    assert_eq!(
        text(&s),
        "Settings synced. Restart IronClaw on Box B to finish (budget)"
    );
}

#[test]
fn matching_computers_exchange_two_reads_and_nothing_else() {
    let (mut s, mut pump, gw_a, gw_b) = two_computers(Computer::ThisMac);
    gw_a.route("/config-sync/manifest", manifest("mac", "c1", "p1"));
    gw_b.route("/config-sync/manifest", manifest("env-b", "c1", "p1"));
    config_sync::sync(&mut s);
    settle(&mut s, &mut pump);
    assert_eq!(gw_a.requests_to("/config-sync/manifest"), 1);
    assert_eq!(gw_b.requests_to("/config-sync/manifest"), 1);
    assert_eq!(gw_a.requests_to("/config-sync/export"), 0);
    assert_eq!(gw_b.requests_to("/config-sync/apply"), 0);
    let line = config_sync::line(&s, "env-b", "Box B").expect("line");
    assert_eq!(line.text, "Settings match This Mac");
    assert_eq!(line.tone, config_sync::Tone::Good);

    // Leaving a page too soon after a check does not ask again; the
    // window coming back later does.
    s.page = crate::state::Page::Notifications;
    config_sync::sync(&mut s);
    settle(&mut s, &mut pump);
    assert_eq!(gw_b.requests_to("/config-sync/manifest"), 1);
    if let Some(row) = s.remote.config_sync.rows.get_mut("env-b") {
        row.checked_at = Instant::now().checked_sub(Duration::from_secs(120));
    }
    config_sync::window_active(&mut s);
    config_sync::sync(&mut s);
    settle(&mut s, &mut pump);
    assert_eq!(gw_b.requests_to("/config-sync/manifest"), 2);
}

#[test]
fn an_older_backend_is_named_and_a_switched_off_computer_is_left_alone() {
    let (mut s, mut pump, gw_a, gw_b) = two_computers(Computer::ThisMac);
    gw_a.route("/config-sync/manifest", manifest("mac", "c1", "p1"));
    // Box B has no such routes (404).
    config_sync::sync(&mut s);
    settle(&mut s, &mut pump);
    assert_eq!(text(&s), "Update IronClaw on Box B to sync settings");
    assert_eq!(gw_a.requests_to("/config-sync/export"), 0);

    let before = gw_b.requests_to("/config-sync/manifest");
    config_sync::set_enabled(&mut s, "env-b", false);
    config_sync::sync_now(&mut s, "env-b");
    settle(&mut s, &mut pump);
    assert_eq!(gw_b.requests_to("/config-sync/manifest"), before);
    let line = config_sync::line(&s, "env-b", "Box B").expect("line");
    assert_eq!(line.text, "Settings sync from This Mac is off");
    assert!(!line.on && !line.can_sync);

    // Back on: it is checked at once.
    config_sync::set_enabled(&mut s, "env-b", true);
    settle(&mut s, &mut pump);
    assert_eq!(gw_b.requests_to("/config-sync/manifest"), before + 1);
}

#[test]
fn nothing_is_sent_between_two_other_computers() {
    let (mut s, mut pump, gw_a, gw_b) = two_computers(Computer::remote("env-a", "Box A"));
    gw_a.route("/config-sync/manifest", manifest("env-a", "c1", "p1"));
    gw_b.route("/config-sync/manifest", manifest("env-b", "c0", "p0"));
    config_sync::sync(&mut s);
    config_sync::sync_now(&mut s, "env-b");
    settle(&mut s, &mut pump);
    assert_eq!(gw_a.requests_to("/config-sync/manifest"), 0);
    assert_eq!(gw_b.requests_to("/config-sync/manifest"), 0);
}

#[test]
fn a_reply_for_a_computer_that_went_away_is_dropped() {
    let (mut s, mut pump, gw_a, gw_b) = two_computers(Computer::ThisMac);
    gw_a.route("/config-sync/manifest", manifest("mac", "c1", "p1"));
    gw_b.route(
        "/config-sync/manifest",
        manifest("env-b", "c1", "p1").after(Duration::from_millis(200)),
    );
    config_sync::sync(&mut s);
    // Sync is turned off while the pass is in flight.
    config_sync::set_enabled(&mut s, "env-b", false);
    settle(&mut s, &mut pump);
    assert!(s.remote.config_sync.rows.is_empty());
}
