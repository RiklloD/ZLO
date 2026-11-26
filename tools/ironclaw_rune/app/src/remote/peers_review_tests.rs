use std::time::{Duration, Instant};

use rune::Tx;
use zlo_client::environment_access::ENVIRONMENT_PROTOCOL_VERSION;
use zlo_client::environments::{Active, Route, SavedEnvironment, load_registry};
use zlo_client::inbox::{InboxListResponse, InboxRow, InboxState, InboxStatus};
use zlo_client::tailnet::{Discovery, Peer as TailnetPeer};
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
    descriptor_with_addresses(id, label, &[])
}

fn descriptor_with_addresses(id: &str, label: &str, addresses: &[&str]) -> String {
    serde_json::json!({
        "environment_id": id,
        "label": label,
        "server_version": "0.41.0",
        "protocol_version": ENVIRONMENT_PROTOCOL_VERSION,
        "pairing": ["one_time_code"],
        "tailnet_url": null,
        "addresses": addresses,
        "peer_links": false
    })
    .to_string()
}

fn session_json() -> String {
    r#"{"tenant_id":"t","user_id":"u"}"#.to_string()
}

fn empty_inbox_json() -> String {
    r#"{"rows":[],"profiles":[],"last_used_profile_id":null}"#.to_string()
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

fn focus_on(s: &mut State, id: &str, label: &str, gw: &Gateway, thread_id: &str) {
    s.remote.registry.active = Active::Environment(id.into());
    connect::adopt(s, Computer::remote(id, label), None);
    s.rt.backend = Some(gw.backend());
    s.session = Some(parsed_session());
    s.workspace.inbox.list = parsed_inbox(thread_id, label, None);
    s.conn = Conn::Ready;
}

/// P1-A: `focus_thread(B, unlisted)` must open the thread on B even when A is
/// the default computer for new threads and the thread is absent from B's
/// inbox — both on warm handover and on a cold `switch_to`.
#[test]
fn p1_a_focus_thread_opens_unlisted_thread_on_target_for_handover_and_cold_switch() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(empty_inbox_json()),
        Answer::ok(notifications_json("n-b", "t-unlisted")),
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
    // Default new threads to A.
    s.prefs.default_computer = Some("env-a".into());
    focus_on(&mut s, "env-a", "Box A", &gw_a, "t-a");

    sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));

    let a_before = gw_a.paths().len();
    focus_thread(&mut s, "env-b", "t-unlisted".into());
    assert_eq!(
        s.onboarding.computer.key(),
        "env-b",
        "handover must stay on B, not bounce back to default computer A"
    );
    assert_eq!(crate::workspace::active_id(&s), Some("t-unlisted"));
    pump.run_for(&mut s, Duration::from_millis(100));
    assert_eq!(
        gw_a.paths().len(),
        a_before,
        "A must not receive requests for B's thread: {:?}",
        gw_a.paths()
    );

    // Cold switch: B is not a ready peer; its inbox does not list the target thread.
    let gw_c = Gateway::start(vec![
        Answer::ok(descriptor_json("env-c", "Box C")),
        Answer::ok(session_json()),
        Answer::ok(empty_inbox_json()),
        Answer::ok("{}"),
    ]);
    let mut s2 = state();
    let mut pump2 = Pump::install(&mut s2);
    s2.remote.registry.run_local_backend = false;
    s2.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    s2.remote
        .registry
        .upsert(saved_env("env-c", "Box C", &gw_c.base_url));
    s2.remote.tokens.set("env-c", "tok-c").expect("token c");
    s2.prefs.default_computer = Some("env-a".into());
    focus_on(&mut s2, "env-a", "Box A", &gw_a, "t-a");

    focus_thread(&mut s2, "env-c", "t-cold-unlisted".into());
    assert!(
        pump2.until(&mut s2, Duration::from_secs(5), |s| {
            s.onboarding.computer.key() == "env-c" && matches!(s.conn, Conn::Ready)
        }),
        "cold switch to C did not land"
    );
    assert_eq!(
        crate::workspace::active_id(&s2),
        Some("t-cold-unlisted"),
        "cold switch must open the requested thread even when absent from inbox"
    );
}

/// P1-C: handover during a real `workspace::refresh_inbox` must reset the
/// focused inbox's request state so the new computer's refreshes are not wedged.
#[test]
fn p1_c_handover_during_inbox_refresh_does_not_wedge_new_computer() {
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
        Answer::ok(notifications_json("n-b", "t-b")),
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
    // Sidebar filter state must survive handover.
    s.workspace.inbox.profile = Some("prof-keep".into());
    s.workspace.inbox.show_working = true;

    sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));

    // Start a real focused inbox refresh on A (sets `inbox.loading = true`).
    crate::workspace::refresh_inbox(&mut s);
    assert!(s.workspace.inbox.is_loading());

    // Hand over to B while A's inbox refresh is still in flight.
    focus(&mut s, "env-b");
    assert!(
        !s.workspace.inbox.is_loading(),
        "inbox loading flag must reset on computer switch"
    );
    assert_eq!(s.workspace.inbox.profile.as_deref(), Some("prof-keep"));
    assert!(s.workspace.inbox.show_working);

    // Let A's stale inbox reply land and be dropped.
    pump.run_for(&mut s, Duration::from_millis(350));

    // Now B's inbox refresh must actually send and apply.
    gw_b.script(vec![Answer::ok(inbox_json(
        "t-b-fresh",
        "Fresh Beta",
        None,
    ))]);
    let b_before = gw_b.requests_to("/inbox");
    crate::workspace::refresh_inbox(&mut s);
    assert!(
        pump.until(&mut s, Duration::from_secs(5), |s| {
            s.workspace.inbox.row("t-b-fresh").is_some()
        }),
        "new computer's inbox refresh was wedged after handover"
    );
    assert_eq!(gw_b.requests_to("/inbox"), b_before + 1);
}

/// P2-E: a cold A→B switch must not persist B as `registry.active` on disk
/// before B commits, even if a peer saves the registry while B is in flight.
#[test]
fn p2_e_cold_switch_does_not_persist_uncommitted_target_on_background_save() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let reg_path = dir.path().join("environments.json");

    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    // B fails after a delay while C connects and learns a new route.
    let gw_b = Gateway::start(vec![Answer::fail().after(Duration::from_millis(250))]);
    let gw_c = Gateway::start(vec![
        Answer::ok(descriptor_with_addresses(
            "env-c",
            "Box C",
            &["http://100.64.0.9:3000"],
        )),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-c", "Gamma", None)),
        Answer::ok(notifications_json("n-c", "t-c")),
    ]);

    let mut s = state();
    let mut pump = Pump::install(&mut s);
    s.remote.registry_path = Some(reg_path.clone());
    s.remote.registry.run_local_backend = false;
    s.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    s.remote
        .registry
        .upsert(saved_env("env-b", "Box B", &gw_b.base_url));
    s.remote
        .registry
        .upsert(saved_env("env-c", "Box C", &gw_c.base_url));
    s.remote.tokens.set("env-b", "tok-b").expect("token b");
    s.remote.tokens.set("env-c", "tok-c").expect("token c");
    focus_on(&mut s, "env-a", "Box A", &gw_a, "t-a");
    crate::remote::save_registry(&mut s).expect("initial save");

    let on_disk = load_registry(&reg_path)
        .expect("read")
        .expect("registry exists");
    assert_eq!(on_disk.active, Active::Environment("env-a".into()));

    // Start cold switch A→B AND peer C connect concurrently.
    connect::switch_to(&mut s, Active::Environment("env-b".into()));
    sync(&mut s);

    // Wait for peer C to connect (which learns a route and calls save_registry).
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-c").is_some_and(|p| p.conn == PeerConn::Ready)
    }));

    // Registry on disk must still name A, not the uncommitted B.
    let mid_disk = load_registry(&reg_path)
        .expect("read")
        .expect("registry exists");
    assert_eq!(
        mid_disk.active,
        Active::Environment("env-a".into()),
        "uncommitted switch target B must not be written to disk"
    );

    // Let B fail: both memory and disk stay on A.
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        s.remote.switching.is_none()
    }));
    assert_eq!(
        s.remote.registry.active,
        Active::Environment("env-a".into())
    );
    let after_fail = load_registry(&reg_path)
        .expect("read")
        .expect("registry exists");
    assert_eq!(after_fail.active, Active::Environment("env-a".into()));

    // Now make B healthy and switch again: once B commits, disk has B.
    gw_b.script(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta", None)),
        Answer::ok("{}"),
    ]);
    connect::switch_to(&mut s, Active::Environment("env-b".into()));
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        s.onboarding.computer.key() == "env-b" && s.remote.switching.is_none()
    }));
    let after_commit = load_registry(&reg_path)
        .expect("read")
        .expect("registry exists");
    assert_eq!(after_commit.active, Active::Environment("env-b".into()));
}

/// P2-F (1): peers must start and connect at launch even if the saved focused
/// backend fails to connect.
#[test]
fn p2_f_peers_connect_at_launch_even_when_focused_backend_fails() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::fail()]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta", None)),
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
    s.remote.registry.active = Active::Environment("env-a".into());
    s.remote.tokens.set("env-a", "tok-a").expect("token a");
    s.remote.tokens.set("env-b", "tok-b").expect("token b");

    connect::connect(&mut s);
    assert!(
        pump.until(&mut s, Duration::from_secs(5), |s| {
            matches!(s.conn, Conn::Failed { .. })
                && get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
        }),
        "peer B must connect even while focused A failed (conn={:?}, peer={:?})",
        s.conn,
        get(&s, "env-b").map(|p| &p.conn)
    );
}

/// P2-F (2): a transport error during a Ready peer's refresh moves it to
/// `PeerConn::Connecting` with retry backoff.
#[test]
fn p2_f_ready_peer_transport_error_moves_to_connecting_with_backoff() {
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta", None)),
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

    // Drop the TCP connection on B's next refresh (transport error).
    gw_b.script(vec![Answer::drop_connection()]);
    refresh(&mut s);
    assert!(
        pump.until(&mut s, Duration::from_secs(5), |s| {
            get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Connecting && p.retry_at.is_some())
        }),
        "transport error on ready peer must move it into Connecting with backoff (got {:?})",
        get(&s, "env-b").map(|p| (&p.conn, p.retry_at.is_some()))
    );
}

/// P2-F (3): an Asleep peer is re-evaluated on window activation and on the
/// Computers page's explicit refresh without waiting for tailnet discovery.
#[test]
fn p2_f_asleep_peer_re_evaluates_on_window_activation_and_computers_refresh() {
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta", None)),
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
    // Stale tailnet discovery says B's host is offline.
    let host_b = zlo_client::environments::host_of(&gw_b.base_url).to_ascii_lowercase();
    s.remote.page.discovery = Some(Discovery {
        peers: vec![TailnetPeer {
            dns_name: host_b,
            host_name: "box-b".into(),
            online: false,
            addresses: Vec::new(),
        }],
        found: Vec::new(),
    });
    focus_on(&mut s, "env-a", "Box A", &gw_a, "t-a");

    sync(&mut s);
    assert_eq!(get(&s, "env-b").expect("b").conn, PeerConn::Asleep);

    // Window activation wakes asleep peers and probes them.
    let mut tx = Tx::default();
    crate::workspace::window_focus(&mut s, true, &mut tx);
    assert!(
        pump.until(&mut s, Duration::from_secs(5), |s| {
            get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
        }),
        "window activation must re-evaluate asleep peers"
    );

    // Put B back to Asleep and trigger Computers page's explicit refresh.
    gw_b.script(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta", None)),
        Answer::ok(notifications_json("n-b", "t-b")),
    ]);
    if let Some(p) = s.remote.peers.get_mut("env-b") {
        p.conn = PeerConn::Asleep;
        p.backend = None;
    }
    crate::pages::computers::refresh_for_test(&mut s);
    assert!(
        pump.until(&mut s, Duration::from_secs(5), |s| {
            get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
        }),
        "Computers page refresh must re-evaluate asleep peers"
    );
}

/// P2-G: demoting a computer with unconfirmed sent work refreshes it right away
/// and polls on the sent cadence until the backend confirms or grace ends;
/// window activation also reaches peers even when focused conn is not Ready.
#[test]
fn p2_g_demoted_peer_with_unconfirmed_sent_work_refreshes_and_polls_on_sent_cadence() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![
        // First refresh after demotion: backend hasn't listed the run yet.
        Answer::ok(inbox_json("t-a", "Alpha", None)),
        Answer::ok(notifications_json("n-a", "t-a")),
    ]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta", None)),
        Answer::ok(notifications_json("n-b", "t-b")),
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

    // User sends a prompt on A, then immediately switches focus to B before
    // A's backend lists the run as working.
    crate::workspace::prompt_sent(&mut s, "t-a");
    let a_before = gw_a.requests_to("/inbox");
    focus(&mut s, "env-b");

    // Demoted A must refresh right away and stay armed on the sent cadence (~1s).
    assert!(
        pump.until(&mut s, Duration::from_secs(5), |_| {
            gw_a.requests_to("/inbox") > a_before
        }),
        "demoted peer with unconfirmed sent work must refresh immediately"
    );
    // Let the refresh reply land.
    pump.run_for(&mut s, Duration::from_millis(100));
    let poll_at = get(&s, "env-a")
        .and_then(|p| p.poll_at)
        .expect("demoted peer must keep polling on sent cadence while unconfirmed");
    assert!(
        poll_at.saturating_duration_since(Instant::now()) <= Duration::from_secs(2),
        "expected sent-cadence poll (<= 2s), got {:?}",
        poll_at.saturating_duration_since(Instant::now())
    );

    // Backend confirms the run is working -> switches to normal 5s poll;
    // then finishes -> poll stops completely (no idle timer).
    gw_a.script(vec![
        Answer::ok(inbox_json("t-a", "Alpha", Some("working"))),
        Answer::ok(notifications_json("n-a", "t-a")),
    ]);
    if let Some(p) = s.remote.peers.get_mut("env-a") {
        p.poll_at = Some(Instant::now() - Duration::from_millis(10));
    }
    let mut tx = Tx::default();
    poll(&mut s, &mut tx);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-a").is_some_and(|p| {
            p.inbox
                .as_ref()
                .and_then(|i| i.rows.first())
                .and_then(|r| r.status)
                == Some(InboxStatus::Working)
        })
    }));

    gw_a.script(vec![
        Answer::ok(inbox_json("t-a", "Alpha done", None)),
        Answer::ok(notifications_json("n-a", "t-a")),
    ]);
    if let Some(p) = s.remote.peers.get_mut("env-a") {
        p.poll_at = Some(Instant::now() - Duration::from_millis(10));
    }
    poll(&mut s, &mut tx);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-a").is_some_and(|p| p.poll_at.is_none())
    }));

    // Window activation reaches ready peers even when focused conn is Failed.
    s.conn = Conn::Failed {
        message: "down".into(),
        retry_in: None,
    };
    let a_before_focus = gw_a.requests_to("/inbox");
    crate::workspace::window_focus(&mut s, true, &mut tx);
    assert!(
        pump.until(&mut s, Duration::from_secs(5), |_| {
            gw_a.requests_to("/inbox") > a_before_focus
        }),
        "window activation must refresh ready peers even when focused conn is Failed"
    );
}

/// P2-H: turning off "Run IronClaw on this Mac" invalidates a managed focused
/// connection so turning it back on reconnects; an unmanaged backend stays Ready.
#[test]
fn p2_h_turning_off_managed_local_backend_invalidates_conn_and_re_enabling_reconnects() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_local = Gateway::start(vec![Answer::ok("{}")]);

    // Managed backend: turning off invalidates Conn::Ready, turning on reconnects.
    let mut s = state();
    s.remote.registry.active = Active::ThisMac;
    s.remote.registry.run_local_backend = true;
    connect::adopt(&mut s, Computer::ThisMac, None);
    s.rt.backend = Some(gw_local.backend());
    s.conn = Conn::Ready;
    crate::service::set_managed_for_test(true);

    crate::remote::pairing::set_run_local(&mut s, false);
    assert!(
        !matches!(s.conn, Conn::Ready),
        "stopping a managed local backend must invalidate Conn::Ready"
    );
    assert!(
        s.rt.backend.is_none(),
        "dead managed client must be dropped"
    );

    crate::remote::pairing::set_run_local(&mut s, true);
    assert!(
        matches!(s.conn, Conn::Connecting),
        "re-enabling must start a reconnect (got {:?})",
        s.conn
    );

    // Unmanaged backend: turning off keeps Conn::Ready and its client.
    let mut s2 = state();
    s2.remote.registry.active = Active::ThisMac;
    s2.remote.registry.run_local_backend = true;
    connect::adopt(&mut s2, Computer::ThisMac, None);
    s2.rt.backend = Some(gw_local.backend());
    s2.conn = Conn::Ready;
    crate::service::set_managed_for_test(false);

    crate::remote::pairing::set_run_local(&mut s2, false);
    assert!(
        matches!(s2.conn, Conn::Ready),
        "unmanaged local backend must stay Ready"
    );
    assert!(s2.rt.backend.is_some());
}

/// P3-I: `peers::poll` must only submit `tx.wake_after` when the next deadline
/// actually changes (no redundant wakes on repeated poll runs).
#[test]
fn p3_i_peers_poll_only_requests_wake_when_next_deadline_changes() {
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Working", Some("working"))),
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

    // Reset wake state by setting a known future poll_at.
    if let Some(p) = s.remote.peers.get_mut("env-b") {
        p.poll_at = Some(Instant::now() + Duration::from_secs(4));
    }
    let mut tx1 = Tx::default();
    poll(&mut s, &mut tx1);
    assert!(
        crate::workspace::asked_wake(&tx1),
        "first poll for a new deadline must ask for a wake"
    );

    // Second poll with the exact same deadline must NOT ask for another wake.
    let mut tx2 = Tx::default();
    poll(&mut s, &mut tx2);
    assert!(
        !crate::workspace::asked_wake(&tx2),
        "repeated poll with unchanged deadline must not re-submit wake_after"
    );

    // Changing the deadline asks again.
    if let Some(p) = s.remote.peers.get_mut("env-b") {
        p.poll_at = Some(Instant::now() + Duration::from_secs(2));
    }
    let mut tx3 = Tx::default();
    poll(&mut s, &mut tx3);
    assert!(
        crate::workspace::asked_wake(&tx3),
        "changed deadline must ask for a new wake"
    );
}

/// P3-J: handover when leaving a Failed/Connecting screen must preserve the
/// crossfade `tx.animated()` that `workspace::connected` requests.
#[test]
fn p3_j_handover_from_failed_screen_asks_for_crossfade_motion() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "Beta", None)),
        Answer::ok(notifications_json("n-b", "t-b")),
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

    // Focused computer is on the Failed screen.
    s.conn = Conn::Failed {
        message: "Box A unreachable".into(),
        retry_in: None,
    };
    pump.asked_motion = false;

    focus(&mut s, "env-b");
    assert!(
        pump.until(&mut s, Duration::from_secs(5), |s| {
            s.onboarding.computer.key() == "env-b" && matches!(s.conn, Conn::Ready)
        }),
        "handover from Failed screen did not complete"
    );
    assert!(
        pump.asked_motion,
        "handover leaving a Failed screen must ask for crossfade motion on a live Tx"
    );
}
