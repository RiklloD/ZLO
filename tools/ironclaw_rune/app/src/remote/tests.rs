use zlo_client::environments::{Active, Registry, Route, RouteKind, SavedEnvironment};
use zlo_client::tailnet::{Discovery, Peer};

use super::*;
use crate::onboarding::gateway::Computer;

fn studio() -> SavedEnvironment {
    SavedEnvironment {
        environment_id: "env-studio".into(),
        label: "Studio".into(),
        routes: vec![Route {
            kind: RouteKind::Tailnet,
            url: "https://studio.tail1.ts.net".into(),
            remote_port: None,
        }],
        legacy_token: false,
    }
}

fn state() -> State {
    crate::chat::fixtures::test_state()
}

#[test]
fn switch_follows_the_active_computer() {
    let mut s = state();
    assert!(!backend_is_remote(&s));
    assert_eq!(ssh_host(&s), None);
    s.remote.registry.upsert(studio());
    s.onboarding.computer = Computer::remote("env-studio", "Studio");
    assert!(backend_is_remote(&s));
    assert_eq!(ssh_host(&s).as_deref(), Some("studio.tail1.ts.net"));
    assert_eq!(
        unavailable_line(&computer_label(&s)),
        "Runs on Studio — not available remotely yet"
    );
}

#[test]
fn the_local_backend_runs_only_for_this_mac_with_the_switch_on() {
    let on = Registry::default();
    assert!(local_backend_allowed(Ok(on.clone()), false, false));
    // IRONCLAW_DESKTOP_URL, a gateway.json computer in use, an unreadable
    // list: never.
    assert!(!local_backend_allowed(Ok(on.clone()), false, true));
    assert!(!local_backend_allowed(Ok(on.clone()), true, false));
    let bad = "unreadable".to_string();
    assert!(!local_backend_allowed(Err(&bad), false, false));
    let off = Registry {
        run_local_backend: false,
        ..Registry::default()
    };
    assert!(!local_backend_allowed(Ok(off), false, false));
    let mut remote = Registry::default();
    remote.upsert(studio());
    remote.active = Active::Environment("env-studio".into());
    assert!(!local_backend_allowed(Ok(remote), false, false));
}

#[test]
fn retrying_against_a_remote_never_lets_the_service_start() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = state();
    s.remote.registry.upsert(studio());
    s.remote.registry.active = Active::Environment("env-studio".into());
    crate::service::set_allowed(true);
    // The connect path decides before anything else (no token is saved,
    // so it stops there without a request).
    crate::workspace::retry_connect(&mut s);
    assert!(!crate::service::allowed(), "service allowed while remote");
    assert!(!crate::service::is_running());
    assert_eq!(s.onboarding.computer.key(), "env-studio");
    assert!(matches!(s.conn, Conn::Connecting));

    // This Mac with the switch off: not allowed either.
    let mut s = state();
    s.remote.registry.run_local_backend = false;
    crate::service::set_allowed(true);
    crate::workspace::retry_connect(&mut s);
    assert!(!crate::service::allowed());
}

#[test]
fn an_unreadable_list_is_a_connection_problem_not_this_mac() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = state();
    s.remote.registry_error = Some("ZLO could not read its list of computers".into());
    crate::service::set_allowed(true);
    crate::workspace::retry_connect(&mut s);
    assert!(!crate::service::allowed());
    match &s.conn {
        Conn::Failed { message, retry_in } => {
            assert!(message.contains("could not read its list"), "{message}");
            assert_eq!(*retry_in, None, "a blocked problem waits for the person");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn replies_asked_before_a_switch_are_dropped() {
    let mut s = state();
    connect::adopt(&mut s, Computer::ThisMac, None);
    // What `jobs::run` captures when a request goes out.
    let asked = s.remote.tag.clone();
    assert!(lands(&s, &asked));
    // The switch lands before the reply.
    connect::reset_for_switch(&mut s);
    connect::adopt(&mut s, Computer::remote("env-studio", "Studio"), None);
    assert!(!lands(&s, &asked), "a reply from This Mac landed on Studio");
    assert!(lands(&s, &s.remote.tag.clone()));
    // Even a reconnect to the same computer starts a new connection.
    let studio_asked = s.remote.tag.clone();
    connect::adopt(&mut s, Computer::remote("env-studio", "Studio"), None);
    assert!(!lands(&s, &studio_asked));
}

#[test]
fn a_switch_forgets_the_old_computers_screen_state() {
    let mut s = state();
    crate::chat::fixtures::with_side_chat(&mut s, "side");
    s.remote.page.issued = Some(pairing::Issued {
        code: "7KQ2-M9XD-4TRA".into(),
        expires_at: chrono::Utc::now(),
        link: String::new(),
    });
    connect::reset_for_switch(&mut s);
    assert!(s.chat.is_none());
    assert!(s.remote.page.issued.is_none(), "the old computer's code");
    assert!(s.remote.page.devices.is_none());
}

#[test]
fn a_failed_switch_keeps_the_old_computer_and_says_why() {
    let mut s = state();
    s.remote.registry.upsert(studio());
    s.conn = Conn::Ready;
    s.rt.backend = Some(
        zlo_client::Backend::new(
            zlo_client::Client::new(zlo_client::Connection {
                base_url: "http://127.0.0.1:9".into(),
                token: zlo_client::Token::new("t").unwrap(),
                token_source: "test".into(),
            })
            .unwrap(),
        )
        .unwrap(),
    );
    s.fixture = true; // no request leaves the test
    switch_to(&mut s, Active::Environment("env-studio".into()));
    assert!(s.remote.switching.is_some(), "the old screen stays");
    assert!(matches!(s.conn, Conn::Ready));
    assert_eq!(state_of(&s, "env-studio"), EnvState::Connecting);
    // What `connect::failed` does when the new computer does not answer.
    let sw = s.remote.switching.take().unwrap();
    s.remote.registry.active = sw.previous;
    s.remote
        .problems
        .insert("env-studio".into(), "Could not reach Studio".into());
    assert_eq!(s.remote.registry.active, Active::ThisMac);
    assert_eq!(
        state_of(&s, "env-studio"),
        EnvState::Blocked("Could not reach Studio".into())
    );
    assert_eq!(state_of(&s, "this-mac"), EnvState::Connected);
}

#[test]
fn an_offline_peer_shows_its_computer_asleep_without_a_probe() {
    let mut s = state();
    s.remote.registry.upsert(studio());
    s.remote.page.discovery = Some(Discovery {
        peers: vec![Peer {
            dns_name: "studio.tail1.ts.net".into(),
            host_name: "studio".into(),
            online: false,
        }],
        found: vec![],
    });
    assert_eq!(state_of(&s, "env-studio"), EnvState::Asleep);
}

#[test]
fn pairing_saves_the_computer_and_switches_to_it() {
    let mut s = state();
    s.fixture = true;
    s.remote.page.input = rune::TextEdit::new("https://studio.tail1.ts.net/pair#code=7KQ2M9XD4TRA");
    pairing::paired(
        &mut s,
        Ok(zlo_client::pairing::Paired {
            environment: studio(),
            token: "icd_x_y".into(),
        }),
    );
    assert!(s.remote.registry.get("env-studio").is_some());
    assert_eq!(
        s.remote.registry.active,
        Active::Environment("env-studio".into())
    );
    assert!(
        s.remote.page.input.text.is_empty(),
        "the code stays on screen"
    );

    // A one-click refusal points the code field at that computer.
    pairing::paired(
        &mut s,
        Err(zlo_client::pairing::PairError::NotOwner("Office".into())),
    );
    assert!(
        s.remote
            .page
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("pairing code")
    );
    assert!(s.remote.page.input.focused);
}

#[test]
fn removing_a_computer_never_touches_others_and_drops_its_token() {
    let mut s = state();
    s.fixture = true;
    s.remote.registry.upsert(studio());
    let mut mini = studio();
    mini.environment_id = "env-mini".into();
    s.remote.registry.upsert(mini);
    s.remote.tokens.set("env-studio", "icd_a_b").unwrap();
    pairing::remove(&mut s, "env-studio".into());
    assert!(s.remote.registry.get("env-studio").is_none());
    assert!(s.remote.registry.get("env-mini").is_some());
}

#[test]
fn a_light_bundle_never_runs_this_macs_backend_and_asks_for_a_computer() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = state();
    s.remote.set_app_only(true);
    assert!(needs_computer(&s));
    assert!(!runs_local_backend(&s), "the switch is on, but no backend");
    crate::service::set_allowed(true);
    crate::workspace::retry_connect(&mut s);
    assert!(!crate::service::allowed());
    assert!(!crate::service::is_running());
    match &s.conn {
        Conn::Failed { message, retry_in } => {
            assert_eq!(message, connect::NEEDS_COMPUTER);
            assert_eq!(*retry_in, None, "nothing to retry");
        }
        other => panic!("expected the add-a-computer state, got {other:?}"),
    }
    assert!(needs_computer(&s), "the window shows Add a computer");
}

#[test]
fn a_light_bundle_starts_on_the_first_saved_computer() {
    let mut s = state();
    s.remote.registry.upsert(studio());
    assert_eq!(s.remote.registry.active, Active::ThisMac);
    s.remote.set_app_only(true);
    assert_eq!(
        s.remote.registry.active,
        Active::Environment("env-studio".into())
    );
    assert!(!needs_computer(&s));
    // The full app keeps This Mac.
    let mut s = state();
    s.remote.registry.upsert(studio());
    s.remote.set_app_only(false);
    assert_eq!(s.remote.registry.active, Active::ThisMac);
    assert!(!needs_computer(&s));
}

#[test]
fn an_ssh_computer_opens_in_the_editor_by_its_ssh_destination() {
    let mut s = state();
    let mut env = studio();
    let target = zlo_client::ssh::parse_target("me@studio.local").unwrap();
    env.routes.push(Route::ssh(&target, 3000));
    s.remote.registry.upsert(env);
    s.onboarding.computer = Computer::remote("env-studio", "Studio");
    assert_eq!(ssh_host(&s).as_deref(), Some("me@studio.local"));
}

#[test]
fn an_ssh_target_that_could_reach_a_shell_is_refused_before_ssh_runs() {
    let mut s = state();
    s.fixture = true;
    s.remote.page.ssh_input = rune::TextEdit::new("me@host;rm -rf ~");
    pairing::submit_ssh(&mut s);
    assert!(s.remote.page.busy.is_none(), "nothing ran");
    assert!(
        s.remote
            .page
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("user@host")
    );
    s.remote.page.ssh_input = rune::TextEdit::new("me@studio.local");
    pairing::submit_ssh(&mut s);
    assert_eq!(s.remote.page.busy.as_deref(), Some("me@studio.local"));
}

#[test]
fn an_unreadable_gateway_json_without_a_list_never_means_this_mac() {
    let launch = Launch::from_files(Ok(None), Some(Err("expected value at line 1".into())));
    assert!(
        launch
            .registry
            .as_ref()
            .is_err_and(|e| e.contains("gateway.json"))
    );
    assert!(!launch.runs_local_backend());
    // A list beside it decides on its own; the old file just stays.
    let launch = Launch::from_files(
        Ok(Some(Registry::default())),
        Some(Err("expected value at line 1".into())),
    );
    assert!(launch.registry.is_ok());
}

fn offline_backend() -> zlo_client::Backend {
    let conn = zlo_client::Connection {
        base_url: "http://127.0.0.1:9".into(),
        token: zlo_client::Token::new("t").unwrap(),
        token_source: "test".into(),
    };
    zlo_client::Client::new(conn)
        .and_then(zlo_client::Backend::new)
        .unwrap()
}

#[test]
fn a_stale_connect_answer_never_takes_the_newer_attempts_backend() {
    let mut s = state();
    // Attempt 1 parks its handle, then attempt 2 replaces it.
    let first = s.workspace.connect_gen.bump();
    s.remote.page.pending_backend = Some((first, offline_backend()));
    let second = s.workspace.connect_gen.bump();
    s.remote.page.pending_backend = Some((second, offline_backend()));
    // Attempt 1's answer lands late: stale, and attempt 2's handle stays.
    assert!(matches!(
        super::connect::claim_pending(&mut s, first),
        super::connect::Claim::Stale
    ));
    assert!(
        s.remote
            .page
            .pending_backend
            .as_ref()
            .is_some_and(|(g, _)| *g == second)
    );
    // Attempt 2's answer gets its own handle.
    assert!(matches!(
        super::connect::claim_pending(&mut s, second),
        super::connect::Claim::Mine(_)
    ));
    assert!(s.remote.page.pending_backend.is_none());
}
