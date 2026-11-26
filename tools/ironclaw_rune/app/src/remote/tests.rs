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
            learned: false,
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
fn the_local_backend_follows_the_switch_even_while_a_remote_has_the_focus() {
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
    // Another computer focused with the switch on: This Mac's backend still runs.
    let mut remote = Registry::default();
    remote.upsert(studio());
    remote.active = Active::Environment("env-studio".into());
    assert!(local_backend_allowed(Ok(remote), false, false));
}

#[test]
fn retrying_against_a_remote_respects_the_local_backend_switch() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = state();
    s.remote.registry.upsert(studio());
    s.remote.registry.active = Active::Environment("env-studio".into());
    crate::service::set_allowed(false);
    // Switch is on: This Mac's backend is allowed while Studio has the focus.
    crate::workspace::retry_connect(&mut s);
    assert!(crate::service::allowed(), "service allowed with switch on");
    assert!(!crate::service::is_running());
    assert_eq!(s.onboarding.computer.key(), "env-studio");
    assert!(matches!(s.conn, Conn::Connecting));

    // Switch off while remote is focused: not allowed.
    let mut s = state();
    s.remote.registry.upsert(studio());
    s.remote.registry.active = Active::Environment("env-studio".into());
    s.remote.registry.run_local_backend = false;
    crate::service::set_allowed(true);
    crate::workspace::retry_connect(&mut s);
    assert!(!crate::service::allowed());

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
            addresses: Vec::new(),
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

#[test]
fn a_removed_computer_leaves_no_update_prompt_behind() {
    use super::updates::{RowState, set_row, waiting};
    let mut s = state();
    s.remote.registry.upsert(studio());
    set_row(&mut s, "env-studio".into(), RowState::behind(2));
    assert_eq!(waiting(&s.remote.updates), 1);
    super::pairing::remove(&mut s, "env-studio".into());
    assert_eq!(waiting(&s.remote.updates), 0);
    // A poll that was already on its way answers after the removal.
    set_row(&mut s, "env-studio".into(), RowState::behind(2));
    assert_eq!(
        waiting(&s.remote.updates),
        0,
        "no row for a computer that is gone"
    );
}

#[test]
fn the_update_dialog_closes_when_its_updates_are_gone() {
    use super::updates::{RowState, set_row};
    let mut s = state();
    s.remote.registry.upsert(studio());
    set_row(&mut s, "env-studio".into(), RowState::behind(2));
    s.workspace.confirm_update = Some(vec!["env-studio".into()]);
    // Another computer's news leaves it open.
    set_row(
        &mut s,
        zlo_client::environments::THIS_MAC.into(),
        RowState::behind(1),
    );
    assert!(s.workspace.confirm_update.is_some());
    // Its own computer got updated elsewhere: it closes, and stays closed
    // when that computer falls behind again later.
    set_row(&mut s, "env-studio".into(), RowState::UpToDate);
    assert!(s.workspace.confirm_update.is_none());
    set_row(&mut s, "env-studio".into(), RowState::behind(3));
    assert!(s.workspace.confirm_update.is_none());
}

mod triage_switch {
    use super::*;
    use crate::state::Page;
    use crate::workspace::triage_test as triage;
    use zlo_client::ClientError;
    use zlo_client::triage_calls::{PrepareTriageResponse, TRIAGE_PROMPT_FILE_NAME};
    use zlo_client::wire::SessionInfo;

    fn backend() -> zlo_client::Backend {
        zlo_client::Backend::new(
            zlo_client::Client::new(zlo_client::Connection {
                base_url: "http://127.0.0.1:9".into(),
                token: zlo_client::Token::new("t").unwrap(),
                token_source: "test".into(),
            })
            .unwrap(),
        )
        .unwrap()
    }

    fn session() -> SessionInfo {
        serde_json::from_str(r#"{"tenant_id":"t","user_id":"u"}"#).expect("session")
    }

    fn prepared(path: &str) -> Result<PrepareTriageResponse, ClientError> {
        let link = zlo_client::mentions::MentionUri::File {
            path: path.into(),
            line_range: None,
        }
        .to_link(TRIAGE_PROMPT_FILE_NAME);
        Ok(PrepareTriageResponse {
            prompt: format!("Read {link} and follow it. "),
            server_version: "1.2.0".into(),
        })
    }

    const MAC: &str = "/Users/a/.zlo/triage/1/triage-playbook.md";
    const STUDIO: &str = "/home/b/.zlo/triage/2/triage-playbook.md";

    /// This Mac, connected, with no request leaving the test (the prepare
    /// replies are landed by hand: pending until then).
    fn on_this_mac() -> State {
        let mut s = state();
        s.fixture = true;
        s.remote.registry.upsert(studio());
        connect::adopt(&mut s, Computer::ThisMac, None);
        s.rt.backend = Some(backend());
        s.conn = Conn::Ready;
        s
    }

    /// The real switch: the old computer's state goes, the new one answers.
    fn switch_to_studio(s: &mut State) {
        let inbox = s.workspace.inbox.list.clone();
        let mut tx = rune::Tx::default();
        connect::switched(
            s,
            connect::Mode::Switch,
            backend(),
            Computer::remote("env-studio", "Studio"),
            None,
            session(),
            inbox,
            &mut tx,
        );
    }

    fn outgoing(s: &State) -> String {
        let d = s.workspace.draft.as_ref().expect("draft");
        crate::chat::inline_files::outgoing(&d.input, &d.staged).0
    }

    fn type_after(s: &mut State, text: &str) {
        let d = s.workspace.draft.as_mut().expect("draft");
        let end = d.input.text.len();
        d.input.set_caret(end);
        d.input.insert(text);
    }

    #[test]
    fn a_prepared_triage_draft_moves_and_sends_only_with_the_new_link() {
        let _guard = crate::service::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = on_this_mac();
        triage::start(&mut s);
        assert!(triage::send_block(&s).is_some(), "pending: Send waits");
        triage::land(&mut s, None, prepared(MAC));
        assert!(triage::send_block(&s).is_none());
        type_after(&mut s, "it crashes");

        switch_to_studio(&mut s);
        assert_eq!(s.page, Page::Draft, "the draft stays on screen");
        assert_eq!(s.onboarding.computer.key(), "env-studio");
        let d = s.workspace.draft.as_ref().expect("the draft came along");
        assert!(d.input.text.ends_with("it crashes"), "{}", d.input.text);
        let block = triage::send_block(&s).expect("Studio has not prepared yet");
        assert!(block.contains("preparing"), "{block}");

        // A reply asked of This Mac lands nowhere now.
        triage::land(&mut s, Some("this-mac"), prepared(MAC));
        assert!(triage::send_block(&s).is_some());

        triage::land(&mut s, None, prepared(STUDIO));
        assert!(
            triage::send_block(&s).is_none(),
            "Studio's link: Send works"
        );
        let sent = outgoing(&s);
        assert!(sent.contains("file:///home/b/.zlo/triage/2/"), "{sent}");
        assert!(!sent.contains("/Users/a/"), "{sent}");
        assert!(sent.ends_with("it crashes"), "{sent}");
    }

    #[test]
    fn a_switch_while_preparing_prepares_on_the_new_computer() {
        let _guard = crate::service::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = on_this_mac();
        triage::start(&mut s);
        type_after(&mut s, "agy will not sign in");
        switch_to_studio(&mut s);
        assert!(triage::send_block(&s).is_some(), "still pending");
        let d = s.workspace.draft.as_ref().expect("draft");
        assert_eq!(d.input.text, "agy will not sign in");
        triage::land(&mut s, None, prepared(STUDIO));
        assert!(triage::is_ready(&s));
        let sent = outgoing(&s);
        assert!(
            sent.starts_with("Read [@triage-playbook.md](file:///home/b/"),
            "{sent}"
        );
        assert!(sent.ends_with("agy will not sign in"), "{sent}");
    }

    #[test]
    fn a_failed_prepare_on_the_new_computer_blocks_send_and_points_at_the_terminal() {
        let _guard = crate::service::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = on_this_mac();
        triage::start(&mut s);
        triage::land(&mut s, None, prepared(MAC));
        switch_to_studio(&mut s);
        triage::land(
            &mut s,
            None,
            Err(ClientError::Config(
                "cannot reach the IronClaw gateway".into(),
            )),
        );
        let why = triage::send_block(&s).expect("Send waits");
        assert!(why.contains("zlo triage"), "{why}");
        let d = s.workspace.draft.as_ref().expect("draft");
        assert_eq!(d.error.as_deref(), Some(why.as_str()));
        // Sending is refused with that line, and nothing is sent.
        crate::workspace::send_draft(&mut s);
        let d = s.workspace.draft.as_ref().expect("draft");
        assert!(!d.sending, "nothing was sent");

        // A reply the composer cannot make a chip of: the terminal line too.
        let mut s = on_this_mac();
        triage::start(&mut s);
        triage::land(
            &mut s,
            None,
            Ok(PrepareTriageResponse {
                prompt: "Read C:\\Users\\a\\triage-playbook.md and follow it. ".into(),
                server_version: "1.2.0".into(),
            }),
        );
        // No chip to send with: the words still send, and the error line
        // points at the terminal.
        assert!(
            triage::send_block(&s).is_none(),
            "no chip, nothing to wait for"
        );
        let d = s.workspace.draft.as_ref().expect("draft");
        let why = d.error.as_deref().expect("the terminal line");
        assert!(why.contains("zlo triage"), "{why}");
    }

    /// Review P2: a failed prepare never strands the person's words. With
    /// the chip deleted the draft is an ordinary one and sends; with the
    /// chip kept, connecting again asks the computer to prepare again.
    #[test]
    fn a_failed_triage_draft_retries_on_reconnect_and_sends_without_its_chip() {
        let _guard = crate::service::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let failed = |s: &mut State| {
            triage::start(s);
            triage::land(
                s,
                None,
                Err(ClientError::Config("gateway answered 503".into())),
            );
            let d = s.workspace.draft.as_ref().expect("draft");
            assert!(d.error.is_some(), "the failure is shown");
        };

        let mut s = on_this_mac();
        failed(&mut s);
        assert!(!triage::connected(&mut s), "nothing carried");
        let why = triage::send_block(&s).expect("asked again: still waits");
        assert!(why.contains("preparing"), "{why}");
        triage::land(&mut s, None, prepared(MAC));
        assert!(triage::send_block(&s).is_none(), "the retry's link sends");

        let mut s = on_this_mac();
        triage::start(&mut s);
        triage::land(&mut s, None, prepared(MAC));
        switch_to_studio(&mut s);
        triage::land(
            &mut s,
            None,
            Err(ClientError::Config("gateway answered 503".into())),
        );
        assert!(
            triage::send_block(&s).is_some(),
            "the Mac's chip: Send waits"
        );
        let d = s.workspace.draft.as_mut().expect("draft");
        crate::chat::inline_files::set_content(&mut d.input, "it crashed on launch", &[]);
        assert!(
            triage::send_block(&s).is_none(),
            "no chip: an ordinary draft that sends"
        );
        crate::workspace::send_draft(&mut s);
        assert!(
            s.workspace.draft.as_ref().is_none_or(|d| d.sending),
            "the words went out"
        );
    }

    /// A draft that is not a triage draft goes with the old computer, as
    /// before.
    #[test]
    fn an_ordinary_draft_stays_behind() {
        let _guard = crate::service::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = on_this_mac();
        crate::workspace::start_in_profile(&mut s, zlo_client::profiles::DEFAULT_PROFILE_ID.into());
        type_after(&mut s, "plain words");
        switch_to_studio(&mut s);
        let text = s.workspace.draft.as_ref().map(|d| d.input.text.clone());
        assert_ne!(text.as_deref(), Some("plain words"));
        assert!(s.workspace.triage.is_none());
    }
}

#[test]
fn a_fresh_mac_without_a_token_keeps_waiting_for_its_first_start() {
    // A fresh install: the app starts its backend, which writes the token on
    // its first start. Until then This Mac has no token — the launch must
    // keep waiting (and retry), never stop on a screen that asks for
    // `zlo onboard`.
    let missing = zlo_client::ClientError::NoToken("/nonexistent/.zlo/webui-token".into());
    assert!(!missing.to_string().contains("onboard"), "{missing}");
    let problem = connect::discover_problem(&missing, true);
    assert!(
        matches!(problem, connect::Problem::Unreachable(_)),
        "{problem:?}"
    );
    // A backend this app does not start (switch off, IRONCLAW_DESKTOP_URL)
    // makes no token: the person's to fix.
    assert!(matches!(
        connect::discover_problem(&missing, false),
        connect::Problem::Blocked(_)
    ));
    // Any other config problem still waits for the person.
    let other = zlo_client::ClientError::Config("cannot parse config.toml".into());
    assert!(matches!(
        connect::discover_problem(&other, true),
        connect::Problem::Blocked(_)
    ));

    // Through the caller: within the startup grace the app stays on its
    // connecting screen and knocks again.
    crate::service::clear_startup_failure();
    let mut s = state();
    assert!(runs_local_backend(&s));
    s.launched = std::time::Instant::now();
    crate::workspace::connect_failed(&mut s, problem);
    assert!(matches!(s.conn, Conn::Connecting), "{:?}", s.conn);

    // Past the grace it shows why, without terminal steps, and retries.
    let mut s = state();
    s.launched = std::time::Instant::now() - std::time::Duration::from_secs(120);
    crate::workspace::connect_failed(&mut s, connect::discover_problem(&missing, true));
    match &s.conn {
        Conn::Failed { message, retry_in } => {
            assert!(
                retry_in.is_some(),
                "a first start still under way is retried"
            );
            assert!(!message.contains("zlo onboard"), "{message}");
            assert!(!message.contains("zlo service install"), "{message}");
        }
        other => panic!("expected a retrying failure, got {other:?}"),
    }

    // The backend already stopped with its own error: shown at once (no
    // grace), still retried, and gone once the app connects.
    crate::service::set_startup_failure(Some("Error: bad config.toml".into()));
    let mut s = state();
    s.launched = std::time::Instant::now();
    crate::workspace::connect_failed(&mut s, connect::discover_problem(&missing, true));
    match &s.conn {
        Conn::Failed { message, retry_in } => {
            assert!(message.contains("Error: bad config.toml"), "{message}");
            assert!(retry_in.is_some());
        }
        other => panic!("expected the backend's own error, got {other:?}"),
    }
    crate::service::clear_startup_failure();
}
