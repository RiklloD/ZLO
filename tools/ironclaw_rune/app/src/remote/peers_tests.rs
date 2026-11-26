use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rune::Tx;
use zlo_client::environment_access::ENVIRONMENT_PROTOCOL_VERSION;
use zlo_client::environments::{Active, Route, SavedEnvironment, THIS_MAC};
use zlo_client::inbox::{InboxListResponse, InboxRow, InboxState, InboxStatus};
use zlo_client::wire::SessionInfo;

use super::*;
use crate::jobs::harness::Pump;
use crate::remote::connect;
use crate::test_gateway::{Answer, Gateway};

fn state() -> State {
    let mut s = crate::chat::fixtures::test_state();
    s.fixture = false;
    s
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

fn session_json() -> String {
    r#"{"tenant_id":"t","user_id":"u"}"#.to_string()
}

fn inbox_json(thread_id: &str, title: &str, status: Option<&str>) -> String {
    serde_json::json!({
        "rows": [{
            "thread_id": thread_id,
            "profile_id": "default",
            "title": title,
            "state": "inbox",
            "status": status,
            "updated_at": 1700000000000i64,
            "last_activity_at": 1700000000000i64,
            "engines": []
        }],
        "profiles": [],
        "last_used_profile_id": null
    })
    .to_string()
}

fn notifications_json(id: &str, thread_id: &str) -> String {
    serde_json::json!({
        "notifications": [{
            "id": id,
            "kind": "run_completed",
            "severity": "success",
            "action": { "kind": "open_thread", "thread_id": thread_id },
            "thread_id": thread_id,
            "turn_run_id": null,
            "created_at": "2026-11-26T00:00:00Z",
            "updated_at": "2026-11-26T00:00:00Z",
            "read_at": null,
            "resolved_at": null
        }],
        "unread_count": 1
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

fn parsed_session() -> SessionInfo {
    serde_json::from_str(&session_json()).expect("session")
}

fn parsed_inbox(thread_id: &str, title: &str, status: Option<InboxStatus>) -> InboxListResponse {
    let mut list = InboxListResponse::default();
    let mut row: InboxRow = serde_json::from_value(serde_json::json!({
        "thread_id": thread_id,
        "profile_id": "default",
        "title": title,
        "state": "inbox",
        "updated_at": 1700000000000i64,
        "last_activity_at": 1700000000000i64,
        "engines": []
    }))
    .expect("row");
    row.state = InboxState::Inbox;
    row.status = status;
    list.rows.push(row);
    list
}

/// Set up a focused connection on `gw` without network I/O.
fn focus_on(s: &mut State, id: &str, label: &str, gw: &Gateway, thread_id: &str) {
    s.remote.registry.active = Active::Environment(id.into());
    connect::adopt(s, Computer::remote(id, label), None);
    s.rt.backend = Some(gw.backend());
    s.session = Some(parsed_session());
    s.workspace.inbox.list = parsed_inbox(thread_id, label, None);
    s.conn = Conn::Ready;
}

#[test]
fn peer_set_follows_registry_focus_and_local_backend_switch() {
    let mut s = state();
    // Fixture mode prevents real connections while testing sync reconciliation.
    s.fixture = true;

    // Single computer (This Mac, no saved computers): no peers, no poll work.
    sync(&mut s);
    assert!(list(&s).is_empty());
    let mut tx = Tx::default();
    assert!(!poll(&mut s, &mut tx));

    // Add two saved computers while This Mac is focused.
    let studio = saved_env("env-studio", "Studio", "http://127.0.0.1:9");
    let mini = saved_env("env-mini", "Mini", "http://127.0.0.1:9");
    s.remote.registry.upsert(studio.clone());
    s.remote.registry.upsert(mini.clone());
    sync(&mut s);
    let keys: Vec<String> = list(&s).iter().map(|p| p.computer.key()).collect();
    assert_eq!(keys, vec!["env-studio", "env-mini"]);

    // Focus Studio with local backend switch ON: This Mac becomes a peer too.
    s.remote.registry.active = Active::Environment("env-studio".into());
    s.onboarding.computer = Computer::remote("env-studio", "Studio");
    assert!(crate::remote::runs_local_backend(&s));
    sync(&mut s);
    let keys: Vec<String> = list(&s).iter().map(|p| p.computer.key()).collect();
    assert_eq!(keys, vec![THIS_MAC, "env-mini"]);

    // `usable` lists the focused computer first, then every peer.
    let choices = usable(&s);
    assert_eq!(choices.len(), 3);
    assert_eq!(choices[0].key, "env-studio");
    assert!(!choices[0].this_mac);
    assert_eq!(choices[1].key, THIS_MAC);
    assert!(choices[1].this_mac);
    assert_eq!(choices[2].key, "env-mini");

    // Turn local backend switch OFF: This Mac leaves the peer set.
    s.remote.registry.run_local_backend = false;
    sync(&mut s);
    let keys: Vec<String> = list(&s).iter().map(|p| p.computer.key()).collect();
    assert_eq!(keys, vec!["env-mini"]);

    // Relabel Mini in the registry: sync updates label without dropping peer.
    let old_gen = get(&s, "env-mini").expect("mini").generation;
    let mut renamed = mini;
    renamed.label = "Mac Mini".into();
    s.remote.registry.upsert(renamed);
    sync(&mut s);
    let mini_peer = get(&s, "env-mini").expect("mini");
    assert_eq!(mini_peer.computer.label(), "Mac Mini");
    assert_eq!(mini_peer.generation, old_gen);

    // Light bundle never adds This Mac as a peer even with the switch on.
    s.remote.registry.run_local_backend = true;
    s.remote.set_app_only(true);
    sync(&mut s);
    assert!(get(&s, THIS_MAC).is_none());

    // Remove Mini: peer set is empty.
    s.remote.registry.remove("env-mini");
    sync(&mut s);
    assert!(list(&s).is_empty());
}

#[test]
fn two_scripted_gateways_both_connect_and_hold_inboxes_and_notifications() {
    let gw_a = Gateway::start(vec![Answer::ok(inbox_json("t-a", "Alpha thread", None))]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta thread", None)),
        Answer::ok(notifications_json("n-b1", "t-b")),
    ]);

    let mut s = state();
    let mut pump = Pump::install(&mut s);
    s.remote.registry.run_local_backend = false;
    s.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    s.remote
        .registry
        .upsert(saved_env("env-b", "Box B", &gw_b.base_url));
    s.remote.tokens.set("env-a", "tok-a").expect("token a");
    s.remote.tokens.set("env-b", "tok-b").expect("token b");
    focus_on(&mut s, "env-a", "Box A", &gw_a, "t-a");

    sync(&mut s);
    assert!(
        pump.until(&mut s, Duration::from_secs(5), |s| {
            get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
        }),
        "peer B did not connect"
    );

    // Both inboxes are held: A in the focused slot, B on the peer.
    assert_eq!(s.workspace.inbox.list.rows[0].thread_id, "t-a");
    let peer_b = get(&s, "env-b").expect("peer b");
    assert_eq!(peer_b.conn, PeerConn::Ready);
    assert!(peer_b.session.is_some());
    assert_eq!(
        peer_b
            .inbox
            .as_ref()
            .and_then(|i| i.rows.first())
            .map(|r| r.thread_id.as_str()),
        Some("t-b")
    );
    assert_eq!(peer_b.notifications.len(), 1);
    assert_eq!(peer_b.notifications[0].id, "n-b1");
}

#[test]
fn handover_swaps_connections_without_connect_requests_and_drops_in_flight_reply() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![
        Answer::ok(inbox_json("t-a", "Alpha", None)).after(Duration::from_millis(250)),
    ]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta", None)),
        Answer::ok(notifications_json("n-b1", "t-b")),
        Answer::ok("{}"),
    ]);

    let mut s = state();
    let mut pump = Pump::install(&mut s);
    s.remote.registry.run_local_backend = false;
    s.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    s.remote
        .registry
        .upsert(saved_env("env-b", "Box B", &gw_b.base_url));
    s.remote.tokens.set("env-a", "tok-a").expect("token a");
    s.remote.tokens.set("env-b", "tok-b").expect("token b");
    focus_on(&mut s, "env-a", "Box A", &gw_a, "t-a");

    sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));

    let a_before = gw_a.paths().len();
    let b_desc_before = gw_b.requests_to("/environment");
    let b_sess_before = gw_b.requests_to("/session");

    // Ask a delayed request on the old focused computer (A) before handover.
    let stale_landed = Arc::new(AtomicBool::new(false));
    let flag = stale_landed.clone();
    crate::jobs::run(
        &s,
        "stale-before-handover",
        |c| async move { c.inbox().await },
        move |_, _, _| {
            flag.store(true, Ordering::SeqCst);
        },
    );

    let connect_gen_before = s.workspace.connect_gen;

    // Instant handover on the UI thread.
    focus(&mut s, "env-b");

    // Synchronously swapped: no connecting screen, B is focused, A is now a ready peer.
    assert!(matches!(s.conn, Conn::Ready));
    assert_eq!(s.onboarding.computer.key(), "env-b");
    assert_eq!(
        s.remote.registry.active,
        Active::Environment("env-b".into())
    );
    assert_eq!(s.workspace.inbox.list.rows[0].thread_id, "t-b");
    assert_ne!(
        s.workspace.connect_gen, connect_gen_before,
        "handover cancels any in-flight connect attempt"
    );
    assert!(get(&s, "env-b").is_none());
    let peer_a = get(&s, "env-a").expect("old focused became a peer");
    assert_eq!(peer_a.conn, PeerConn::Ready);
    assert_eq!(
        peer_a
            .inbox
            .as_ref()
            .and_then(|i| i.rows.first())
            .map(|r| r.thread_id.as_str()),
        Some("t-a")
    );

    // Wait for the delayed request on A to finish and confirm it was dropped.
    pump.run_for(&mut s, Duration::from_millis(400));
    assert!(
        !stale_landed.load(Ordering::SeqCst),
        "reply asked before handover must be dropped"
    );
    // Zero connect requests (/environment, /session) went out on B,
    // and A only saw the one pre-handover request (zero new ones after handover).
    assert_eq!(gw_b.requests_to("/environment"), b_desc_before);
    assert_eq!(gw_b.requests_to("/session"), b_sess_before);
    assert_eq!(
        gw_a.paths().len(),
        a_before + 1,
        "only the pre-handover request hit A: {:?}",
        gw_a.paths()
    );
}

#[test]
fn peer_failure_never_touches_focused_conn_retries_and_drops_stale_replies() {
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_retry = Gateway::start(vec![Answer::fail()]);
    let gw_stale = Gateway::start(vec![
        Answer::ok(descriptor_json("env-stale", "Stale")).after(Duration::from_millis(200)),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-s", "Stale", None)),
        Answer::ok(notifications_json("n-s", "t-s")),
    ]);

    let mut s = state();
    let mut pump = Pump::install(&mut s);
    s.remote.registry.run_local_backend = false;
    s.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    // Blocked peer: no token saved.
    s.remote
        .registry
        .upsert(saved_env("env-blocked", "Blocked", "http://127.0.0.1:9"));
    // Unreachable peer: returns 503 initially.
    s.remote
        .registry
        .upsert(saved_env("env-retry", "Retry", &gw_retry.base_url));
    s.remote
        .tokens
        .set("env-retry", "tok-r")
        .expect("token retry");
    // Slow peer: removed before its reply arrives.
    s.remote
        .registry
        .upsert(saved_env("env-stale", "Stale", &gw_stale.base_url));
    s.remote
        .tokens
        .set("env-stale", "tok-s")
        .expect("token stale");

    focus_on(&mut s, "env-a", "Box A", &gw_a, "t-a");
    sync(&mut s);

    // Drop the slow peer while its resolve is still in flight.
    s.remote.registry.remove("env-stale");
    sync(&mut s);
    assert!(get(&s, "env-stale").is_none());

    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        let blocked_done =
            get(s, "env-blocked").is_some_and(|p| matches!(p.conn, PeerConn::Failed(_)));
        let retry_scheduled = get(s, "env-retry").is_some_and(|p| p.retry_at.is_some());
        blocked_done && retry_scheduled
    }));

    // Focused connection was never touched.
    assert!(matches!(s.conn, Conn::Ready));
    assert!(get(&s, "env-blocked").is_some_and(|p| p.retry_at.is_none()));
    assert!(get(&s, "env-retry").is_some_and(|p| p.conn == PeerConn::Connecting));

    // Stale reply for the removed peer lands and is dropped cleanly.
    pump.run_for(&mut s, Duration::from_millis(300));
    assert!(get(&s, "env-stale").is_none());

    // Now make the retry gateway healthy and advance its retry deadline.
    gw_retry.script(vec![
        Answer::ok(descriptor_json("env-retry", "Retry")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-r", "Recovered", None)),
        Answer::ok(notifications_json("n-r", "t-r")),
    ]);
    if let Some(p) = s.remote.peers.get_mut("env-retry") {
        p.retry_at = Some(Instant::now() - Duration::from_secs(1));
    }

    // While hidden, poll does NOT retry.
    s.workspace.window_active = false;
    let mut tx = Tx::default();
    assert!(!poll(&mut s, &mut tx));
    assert!(get(&s, "env-retry").is_some_and(|p| p.retry_at.is_some()));

    // Back in front: poll fires the retry immediately.
    s.workspace.window_active = true;
    assert!(poll(&mut s, &mut tx));
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-retry").is_some_and(|p| p.conn == PeerConn::Ready)
    }));
}

#[test]
fn peers_run_reaches_peer_gateway_not_focused_and_drops_when_stale() {
    let gw_a = Gateway::start(vec![Answer::ok(inbox_json("t-a", "Alpha", None))]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta", None)),
        Answer::ok(notifications_json("n-b", "t-b")),
        Answer::ok(inbox_json("t-b", "Beta updated", None)),
    ]);

    let mut s = state();
    let mut pump = Pump::install(&mut s);
    s.remote.registry.run_local_backend = false;
    s.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    s.remote
        .registry
        .upsert(saved_env("env-b", "Box B", &gw_b.base_url));
    s.remote.tokens.set("env-b", "tok-b").expect("token b");
    focus_on(&mut s, "env-a", "Box A", &gw_a, "t-a");

    sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));

    let a_before = gw_a.paths().len();
    let b_before = gw_b.requests_to("/inbox");
    let got_reply = Arc::new(AtomicBool::new(false));
    let flag = got_reply.clone();
    run(
        &s,
        "env-b",
        "test-peer-run",
        |c| async move { c.inbox().await },
        move |_, result, _| {
            if result.is_ok() {
                flag.store(true, Ordering::SeqCst);
            }
        },
    );
    assert!(pump.until(&mut s, Duration::from_secs(5), |_| {
        got_reply.load(Ordering::SeqCst)
    }));
    assert_eq!(gw_a.paths().len(), a_before, "focused gateway untouched");
    assert_eq!(gw_b.requests_to("/inbox"), b_before + 1);

    // If the peer's connection changes before `done` lands, `done` is dropped.
    gw_b.script(vec![
        Answer::ok(inbox_json("t-b", "Slow", None)).after(Duration::from_millis(200)),
    ]);
    let stale_done = Arc::new(AtomicBool::new(false));
    let flag = stale_done.clone();
    run(
        &s,
        "env-b",
        "test-stale-peer-run",
        |c| async move { c.inbox().await },
        move |_, _, _| {
            flag.store(true, Ordering::SeqCst);
        },
    );
    if let Some(p) = s.remote.peers.get_mut("env-b") {
        p.generation.bump();
    }
    pump.run_for(&mut s, Duration::from_millis(300));
    assert!(!stale_done.load(Ordering::SeqCst));
}

#[test]
fn working_row_poll_starts_and_stops_with_working_row_and_window_focus() {
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Idle", None)),
        Answer::ok(notifications_json("n-b", "t-b")),
    ]);

    let mut s = state();
    let mut pump = Pump::install(&mut s);
    s.remote.registry.run_local_backend = false;
    s.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    s.remote
        .registry
        .upsert(saved_env("env-b", "Box B", &gw_b.base_url));
    s.remote.tokens.set("env-b", "tok-b").expect("token b");
    focus_on(&mut s, "env-a", "Box A", &gw_a, "t-a");

    sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));

    // Idle peer: no poll timer armed.
    let mut tx = Tx::default();
    poll(&mut s, &mut tx);
    assert!(get(&s, "env-b").expect("b").poll_at.is_none());

    // A row starts working with the window active: poll timer arms.
    if let Some(p) = s.remote.peers.get_mut("env-b") {
        p.inbox = Some(parsed_inbox("t-b", "Working", Some(InboxStatus::Working)));
    }
    s.workspace.window_active = true;
    poll(&mut s, &mut tx);
    assert!(get(&s, "env-b").expect("b").poll_at.is_some());

    // Window loses focus: poll timer stops.
    s.workspace.window_active = false;
    poll(&mut s, &mut tx);
    assert!(get(&s, "env-b").expect("b").poll_at.is_none());

    // Window comes back and deadline expires: poll refreshes the peer, and
    // when the reply says the row is idle again, the timer stops.
    gw_b.script(vec![
        Answer::ok(inbox_json("t-b", "Finished", None)),
        Answer::ok(notifications_json("n-b", "t-b")),
    ]);
    s.workspace.window_active = true;
    if let Some(p) = s.remote.peers.get_mut("env-b") {
        p.poll_at = Some(Instant::now() - Duration::from_secs(1));
    }
    let before = gw_b.requests_to("/inbox");
    poll(&mut s, &mut tx);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-b").is_some_and(|p| {
            p.inbox
                .as_ref()
                .and_then(|i| i.rows.first())
                .and_then(|r| r.title.as_deref())
                == Some("Finished")
        })
    }));
    assert_eq!(gw_b.requests_to("/inbox"), before + 1);
    assert!(
        get(&s, "env-b").expect("b").poll_at.is_none(),
        "poll stops once no row is working"
    );
}

#[test]
fn focus_thread_hands_over_and_opens_target_thread_immediately() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b2", "Target thread", None)),
        Answer::ok(notifications_json("n-b", "t-b2")),
        Answer::ok("{}"),
    ]);

    let mut s = state();
    let mut pump = Pump::install(&mut s);
    s.remote.registry.run_local_backend = false;
    s.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    s.remote
        .registry
        .upsert(saved_env("env-b", "Box B", &gw_b.base_url));
    s.remote.tokens.set("env-b", "tok-b").expect("token b");
    focus_on(&mut s, "env-a", "Box A", &gw_a, "t-a");

    sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));

    focus_thread(&mut s, "env-b", "t-b2".into());
    assert_eq!(s.onboarding.computer.key(), "env-b");
    assert_eq!(crate::workspace::active_id(&s), Some("t-b2"));

    // Fixture helpers populate `Peers` directly without network I/O.
    let mut fx = state();
    fx.fixture = true;
    fx.remote.peers.insert_fixture(Peer::fixture(
        Computer::remote("env-x", "X"),
        PeerConn::Asleep,
    ));
    assert_eq!(fx.remote.peers.len(), 1);
    assert_eq!(get(&fx, "env-x").expect("x").conn, PeerConn::Asleep);
}
