use std::time::Duration;

use zlo_client::environment_access::ENVIRONMENT_PROTOCOL_VERSION;
use zlo_client::environments::{Active, Route, SavedEnvironment};

use super::*;
use crate::jobs::harness::Pump;
use crate::onboarding::gateway::Computer;
use crate::remote::peers::{self, PeerConn};
use crate::remote::{connect, inbox_stream};
use crate::test_gateway::{Answer, Gateway};

fn inbox_json(thread_id: &str) -> String {
    serde_json::json!({
        "rows": [{
            "thread_id": thread_id,
            "profile_id": "default",
            "title": thread_id,
            "state": "inbox",
            "status": null,
            "updated_at": 1700000000000i64,
            "last_activity_at": 1700000000000i64,
            "engines": []
        }],
        "profiles": [],
        "last_used_profile_id": null
    })
    .to_string()
}

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

fn saved_env(id: &str, label: &str, base_url: &str) -> SavedEnvironment {
    SavedEnvironment {
        environment_id: id.into(),
        label: label.into(),
        routes: vec![Route::for_origin(base_url)],
        legacy_token: false,
    }
}

/// A focused computer A and a ready peer B, each on its own gateway whose
/// every later answer is its inbox. The window is in front.
fn two_computers(streams_a: bool, streams_b: bool) -> (State, Pump, Gateway, Gateway) {
    let gw_a = Gateway::start(vec![Answer::ok(inbox_json("t-a"))]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(r#"{"tenant_id":"t","user_id":"u"}"#),
        Answer::ok(inbox_json("t-b")),
    ]);
    if streams_a {
        gw_a.serve_inbox_events();
    }
    if streams_b {
        gw_b.serve_inbox_events();
    }
    let mut s = crate::chat::fixtures::test_state();
    s.fixture = false;
    s.workspace.window_active = true;
    let mut pump = Pump::install(&mut s);
    s.remote.registry.run_local_backend = false;
    s.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    s.remote
        .registry
        .upsert(saved_env("env-b", "Box B", &gw_b.base_url));
    s.remote.tokens.set("env-b", "tok-b").expect("token b");
    s.remote.registry.active = Active::Environment("env-a".into());
    connect::adopt(&mut s, Computer::remote("env-a", "Box A"), None);
    s.rt.backend = Some(gw_a.backend());
    s.session = serde_json::from_str(r#"{"tenant_id":"t","user_id":"u"}"#).ok();
    s.conn = Conn::Ready;
    peers::sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        peers::get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));
    (s, pump, gw_a, gw_b)
}

/// The requests for an inbox list a gateway has seen.
fn inbox_reads(gw: &Gateway) -> usize {
    gw.requests_to("/inbox")
}

fn settle(s: &mut State, pump: &mut Pump) {
    pump.run_for(s, Duration::from_millis(300));
}

#[test]
fn one_stream_per_connected_computer_and_a_hint_refreshes_only_its_own() {
    let (mut s, mut pump, gw_a, gw_b) = two_computers(true, true);
    inbox_stream::sync(&mut s);
    assert!(s.remote.inbox_streams.has_focused());
    assert!(s.remote.inbox_streams.has_peer("env-b"));
    assert_eq!(s.remote.inbox_streams.open_count(), 2);
    // Each stream opening reads its inbox once (what changed meanwhile).
    settle(&mut s, &mut pump);
    let (a, b) = (inbox_reads(&gw_a), inbox_reads(&gw_b));

    gw_b.hint_inbox_changed();
    assert!(
        pump.until(&mut s, Duration::from_secs(3), |_| inbox_reads(&gw_b) > b),
        "the peer re-read its inbox on its hint"
    );
    settle(&mut s, &mut pump);
    assert_eq!(inbox_reads(&gw_a), a, "the focused computer was not asked");

    let b = inbox_reads(&gw_b);
    gw_a.hint_inbox_changed();
    assert!(
        pump.until(&mut s, Duration::from_secs(3), |_| inbox_reads(&gw_a) > a),
        "the focused inbox re-read on its hint"
    );
    settle(&mut s, &mut pump);
    assert_eq!(inbox_reads(&gw_b), b, "the peer was not asked");

    // Syncing again with nothing changed opens nothing new.
    inbox_stream::sync(&mut s);
    assert_eq!(gw_a.requests_to("/inbox/events?"), 0);
    assert_eq!(s.remote.inbox_streams.open_count(), 2);
}

#[test]
fn streams_follow_a_handover() {
    let (mut s, mut pump, gw_a, gw_b) = two_computers(true, true);
    inbox_stream::sync(&mut s);
    settle(&mut s, &mut pump);

    peers::focus(&mut s, "env-b");
    assert_eq!(s.onboarding.computer.key(), "env-b");
    inbox_stream::sync(&mut s);
    settle(&mut s, &mut pump);

    assert!(s.remote.inbox_streams.has_focused());
    assert!(s.remote.inbox_streams.has_peer("env-a"));
    assert!(!s.remote.inbox_streams.has_peer("env-b"));
    assert_eq!(s.remote.inbox_streams.open_count(), 2);
    // The old streams were closed, not left open beside the new ones.
    assert_eq!(gw_a.inbox_streams_open(), 1);
    assert_eq!(gw_b.inbox_streams_open(), 1);

    // B is focused now: its hint refreshes the focused inbox.
    let b = inbox_reads(&gw_b);
    gw_b.hint_inbox_changed();
    assert!(pump.until(&mut s, Duration::from_secs(3), |_| inbox_reads(&gw_b) > b));
}

#[test]
fn a_backend_without_the_route_is_asked_once() {
    let (mut s, mut pump, gw_a, gw_b) = two_computers(false, true);
    inbox_stream::sync(&mut s);
    settle(&mut s, &mut pump);
    assert!(!s.remote.inbox_streams.has_focused(), "404: no stream");
    assert!(s.remote.inbox_streams.has_peer("env-b"));

    let asked = gw_a
        .paths()
        .iter()
        .filter(|p| p.contains("/inbox/events"))
        .count();
    assert_eq!(asked, 1);
    for _ in 0..5 {
        inbox_stream::sync(&mut s);
        settle(&mut s, &mut pump);
    }
    let asked_again = gw_a
        .paths()
        .iter()
        .filter(|p| p.contains("/inbox/events"))
        .count();
    assert_eq!(asked_again, 1, "not retried on the same connection");
    let _ = gw_b;
}

#[test]
fn a_hidden_window_closes_the_streams_and_coming_back_reopens_them() {
    let (mut s, mut pump, gw_a, gw_b) = two_computers(true, true);
    inbox_stream::sync(&mut s);
    settle(&mut s, &mut pump);
    assert_eq!(s.remote.inbox_streams.open_count(), 2);

    s.workspace.window_active = false;
    inbox_stream::sync(&mut s);
    assert_eq!(s.remote.inbox_streams.open_count(), 0);
    settle(&mut s, &mut pump);
    assert_eq!(gw_a.inbox_streams_open(), 0);
    assert_eq!(gw_b.inbox_streams_open(), 0);

    s.workspace.window_active = true;
    inbox_stream::sync(&mut s);
    assert_eq!(s.remote.inbox_streams.open_count(), 2);
}

#[test]
fn a_fixture_opens_no_stream() {
    let (mut s, _pump, gw_a, _gw_b) = two_computers(true, true);
    s.fixture = true;
    inbox_stream::sync(&mut s);
    assert_eq!(s.remote.inbox_streams.open_count(), 0);
    assert!(!gw_a.paths().iter().any(|p| p.contains("/inbox/events")));
}
