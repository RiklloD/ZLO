//! Second review round: navigation intents bound to their attempt, removal
//! during a switch, and This Mac's backend switched off while focused.

use super::*;

/// A saved computer nothing answers at (a port that was just closed).
fn dead_env(id: &str, label: &str) -> SavedEnvironment {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    saved_env(id, label, &format!("http://{addr}"))
}

fn two_computers(s: &mut State, gw_a: &Gateway, other: SavedEnvironment) {
    s.remote.registry.run_local_backend = false;
    s.remote
        .registry
        .upsert(saved_env("env-a", "Box A", &gw_a.base_url));
    s.remote
        .tokens
        .set(&other.environment_id, "tok")
        .expect("token");
    s.remote.registry.upsert(other);
    focus_on(s, "env-a", "Box A", gw_a, "t-a");
}

/// A thread click that rode on a failed cold switch is over with it: it can
/// never fire on a later connection.
#[test]
fn a_failed_cold_switch_drops_its_thread_intent() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let mut s = state();
    let mut pump = Pump::install(&mut s);
    two_computers(&mut s, &gw_a, dead_env("env-c", "Box C"));

    focus_thread(&mut s, "env-c", "t-c".into());
    let intent = s
        .remote
        .pending_thread
        .clone()
        .expect("intent while switching");
    assert!(intent.live(&s));
    assert!(
        !s.remote.last_thread.contains_key("env-c"),
        "the last thread is what was opened, not what was asked"
    );

    assert!(pump.until(&mut s, Duration::from_secs(8), |s| {
        s.remote.switching.is_none()
    }));
    assert_eq!(s.onboarding.computer.key(), "env-a");
    assert!(s.remote.pending_thread.is_none(), "the attempt failed");
    assert_eq!(
        s.remote.registry.active,
        Active::Environment("env-a".into())
    );
}

/// Opening something else while the switch is pending is the newer
/// navigation; a newer connect attempt makes an old intent stale.
#[test]
fn a_newer_navigation_or_attempt_supersedes_a_thread_intent() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let mut s = state();
    let _pump = Pump::install(&mut s);
    two_computers(&mut s, &gw_a, dead_env("env-c", "Box C"));

    focus_thread(&mut s, "env-c", "t-c".into());
    assert!(s.remote.pending_thread.is_some());
    crate::workspace::open_thread(&mut s, "t-a".into());
    assert!(s.remote.pending_thread.is_none(), "the person moved on");

    focus_thread(&mut s, "env-c", "t-c".into());
    let intent = s.remote.pending_thread.clone().expect("intent");
    s.workspace.connect_gen.bump();
    assert!(!intent.live(&s), "another attempt started since");
}

/// Removing the computer a switch is heading for cancels the switch: its
/// late answer installs nothing, and the saved pick names a saved computer.
#[test]
fn removing_a_switchs_destination_cancels_the_switch() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_c = Gateway::start(vec![
        Answer::ok(descriptor_json("env-c", "Box C")),
        Answer::ok(session_json()),
        Answer::ok(empty_inbox_json()),
        Answer::ok("{}"),
    ]);
    let mut s = state();
    let mut pump = Pump::install(&mut s);
    two_computers(&mut s, &gw_a, saved_env("env-c", "Box C", &gw_c.base_url));

    focus_thread(&mut s, "env-c", "t-c".into());
    assert!(s.remote.switching.is_some());
    crate::remote::pairing::remove(&mut s, "env-c".into());
    assert!(s.remote.switching.is_none());
    assert!(s.remote.pending_thread.is_none());

    pump.run_for(&mut s, Duration::from_millis(600));
    assert_eq!(s.onboarding.computer.key(), "env-a", "nothing installed");
    assert_eq!(
        s.remote.registry.active,
        Active::Environment("env-a".into())
    );
    assert!(s.remote.registry.get("env-c").is_none());
}

/// Removing the focused computer never leaves the saved pick naming it.
#[test]
fn removing_the_focused_computer_saves_a_valid_pick() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let mut s = state();
    let _pump = Pump::install(&mut s);
    two_computers(&mut s, &gw_a, dead_env("env-c", "Box C"));

    crate::remote::pairing::remove(&mut s, "env-a".into());

    // This Mac runs no backend here: the first computer still saved.
    assert_eq!(
        s.remote.registry.active,
        Active::Environment("env-c".into())
    );
    assert!(s.remote.switching.is_none());
    assert!(s.remote.registry.get("env-a").is_none());
}

/// Removing the focused computer while another is connected moves there on
/// its warm connection, not to a connecting screen.
#[test]
fn removing_the_focused_computer_moves_to_a_connected_one() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "On B", None)),
        Answer::ok(notifications_json("n-b", "t-b")),
        Answer::ok("{}"),
    ]);
    let mut s = state();
    let mut pump = Pump::install(&mut s);
    two_computers(&mut s, &gw_a, saved_env("env-b", "Box B", &gw_b.base_url));
    sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));

    crate::remote::pairing::remove(&mut s, "env-a".into());

    assert_eq!(s.onboarding.computer.key(), "env-b");
    assert!(matches!(s.conn, Conn::Ready));
    assert_eq!(
        s.remote.registry.active,
        Active::Environment("env-b".into())
    );
    assert!(
        get(&s, "env-a").is_none(),
        "the removed computer is no peer"
    );
}

/// Two deferred moves asked from a failed screen: the later one wins, and a
/// connect retry in between cancels neither.
#[test]
fn the_latest_deferred_move_wins() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_a = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "On B", None)),
        Answer::ok(notifications_json("n-b", "t-b")),
        Answer::ok("{}"),
    ]);
    let gw_c = Gateway::start(vec![
        Answer::ok(descriptor_json("env-c", "Box C")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-c", "On C", None)),
        Answer::ok(notifications_json("n-c", "t-c")),
        Answer::ok("{}"),
    ]);
    let mut s = state();
    let mut pump = Pump::install(&mut s);
    two_computers(&mut s, &gw_a, saved_env("env-b", "Box B", &gw_b.base_url));
    s.remote
        .registry
        .upsert(saved_env("env-c", "Box C", &gw_c.base_url));
    s.remote.tokens.set("env-c", "tok-c").expect("token c");
    sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        ["env-b", "env-c"]
            .iter()
            .all(|k| get(s, k).is_some_and(|p| p.conn == PeerConn::Ready))
    }));
    // The focused connection failed: moves are deferred to a live `Tx`.
    s.conn = Conn::Failed {
        message: "down".into(),
        retry_in: None,
    };

    focus(&mut s, "env-b");
    s.workspace.connect_gen.bump(); // a connect retry in between
    focus(&mut s, "env-c");
    pump.run_for(&mut s, Duration::from_millis(200));

    assert_eq!(s.onboarding.computer.key(), "env-c");
    assert!(matches!(s.conn, Conn::Ready));
}

/// This Mac's managed backend is switched off while it has the focus and
/// another computer is connected: work goes on there, on its warm
/// connection, instead of on a dead client or a failed screen.
#[test]
fn switching_off_this_macs_backend_moves_to_a_connected_computer() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let gw_local = Gateway::start(vec![Answer::ok("{}")]);
    let gw_b = Gateway::start(vec![
        Answer::ok(descriptor_json("env-b", "Box B")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-b", "On B", None)),
        Answer::ok(notifications_json("n-b", "t-b")),
        Answer::ok("{}"),
    ]);
    let mut s = state();
    let mut pump = Pump::install(&mut s);
    s.remote.registry.active = Active::ThisMac;
    s.remote.registry.run_local_backend = true;
    s.remote
        .registry
        .upsert(saved_env("env-b", "Box B", &gw_b.base_url));
    s.remote.tokens.set("env-b", "tok-b").expect("token b");
    connect::adopt(&mut s, Computer::ThisMac, None);
    s.rt.backend = Some(gw_local.backend());
    s.session = Some(parsed_session());
    s.conn = Conn::Ready;
    sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        get(s, "env-b").is_some_and(|p| p.conn == PeerConn::Ready)
    }));
    crate::service::set_managed_for_test(true);

    crate::remote::pairing::set_run_local(&mut s, false);
    crate::service::set_managed_for_test(false);

    assert_eq!(s.onboarding.computer.key(), "env-b");
    assert!(matches!(s.conn, Conn::Ready));
    assert!(s.rt.backend.is_some());
    assert!(
        get(&s, zlo_client::environments::THIS_MAC).is_none(),
        "its stopped backend is no peer"
    );
    assert_eq!(
        s.remote.registry.active,
        Active::Environment("env-b".into())
    );
}
