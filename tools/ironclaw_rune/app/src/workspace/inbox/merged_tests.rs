use std::time::Duration;

use rune::{App, AppSpec, Env, Runtime};
use zlo_client::environment_access::ENVIRONMENT_PROTOCOL_VERSION;
use zlo_client::environments::{Active, Route, SavedEnvironment, THIS_MAC};
use zlo_client::inbox::{InboxAction, InboxListResponse, InboxRow, InboxState, InboxStatus};
use zlo_client::wire::SessionInfo;

use super::*;
use crate::jobs::harness::Pump;
use crate::onboarding::gateway::Computer;
use crate::pages::computers::fixtures::STUDIO;
use crate::remote::{self, EnvState};
use crate::state::{Conn, Page, State};
use crate::test_gateway::{Answer, Gateway};
use crate::workspace::inbox::{jump_target, shelves};

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

fn runtime_with(fill: impl FnOnce(&mut State)) -> Runtime<State> {
    let app = App::new(AppSpec::new(
        "ZLO test",
        (1280., 820.),
        no_init,
        crate::view,
    ));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    fill(&mut s);
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);
    rt.frame(1. / 60.);
    rt
}

fn make_row(id: &str, title: &str, state: InboxState, last_activity_at: i64) -> InboxRow {
    InboxRow {
        thread_id: id.into(),
        title: Some(title.into()),
        profile_id: "default".into(),
        state,
        status: (state == InboxState::Working).then_some(InboxStatus::Working),
        engines: vec![],
        last_activity_at,
        snooze_until: None,
        settled_at: (state == InboxState::Settled).then_some(last_activity_at),
        limit_reset_at: None,
        working_since: (state == InboxState::Working).then_some(last_activity_at),
        parent_thread_id: None,
        automation: false,
        worktree_branch: None,
        title_pending: false,
        imported: None,
    }
}

fn click_key(rt: &mut Runtime<State>, key: &str) {
    let rect = rt
        .elements()
        .into_iter()
        .find(|e| !e.exiting && e.key.as_deref() == Some(key))
        .map(|e| e.rect)
        .unwrap_or_else(|| panic!("element `{key}` not found"));
    let x = rect[0] + rect[2] / 2.;
    let y = rect[1] + rect[3] / 2.;
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.frame(1. / 60.);
    rt.pointer_up(x, y);
    rt.frame(1. / 60.);
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

fn inbox_json(rows: serde_json::Value) -> String {
    serde_json::json!({
        "rows": rows,
        "profiles": [],
        "last_used_profile_id": null
    })
    .to_string()
}

fn row_json(id: &str, title: &str, state: &str, at: i64) -> serde_json::Value {
    serde_json::json!({
        "thread_id": id,
        "profile_id": "default",
        "title": title,
        "state": state,
        "updated_at": at,
        "last_activity_at": at,
        "engines": []
    })
}

fn two_live_gateways() -> (State, Pump, Gateway, Gateway) {
    let mut s = crate::chat::fixtures::test_state();
    s.fixture = false;
    s.remote.registry.run_local_backend = false;
    let mut pump = Pump::install(&mut s);

    let local_gw = Gateway::start(vec![]);
    let peer_gw = Gateway::start(vec![
        Answer::ok(descriptor_json(STUDIO, "Studio")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json(serde_json::json!([row_json(
            "peer-1",
            "Build on Studio",
            "inbox",
            5000
        ),]))),
        Answer::ok(r#"{"notifications":[],"unread_count":0}"#),
        Answer::ok("{}"),
    ]);

    let local_env = SavedEnvironment {
        environment_id: "env-local".into(),
        label: "Local Box".into(),
        routes: vec![Route::for_origin(&local_gw.base_url)],
        legacy_token: false,
    };
    let env = SavedEnvironment {
        environment_id: STUDIO.into(),
        label: "Studio".into(),
        routes: vec![Route::for_origin(&peer_gw.base_url)],
        legacy_token: false,
    };
    s.remote.registry.upsert(local_env);
    s.remote.registry.upsert(env);
    s.remote
        .tokens
        .set("env-local", "local-tok")
        .expect("store local token");
    s.remote
        .tokens
        .set(STUDIO, "peer-tok")
        .expect("store peer token");

    s.remote.registry.active = Active::Environment("env-local".into());
    crate::remote::connect::adopt(&mut s, Computer::remote("env-local", "Local Box"), None);
    s.rt.backend = Some(local_gw.backend());
    let session: SessionInfo = serde_json::from_str(&session_json()).expect("session");
    s.session = Some(session);
    s.workspace.inbox.list = InboxListResponse {
        rows: vec![make_row("local-1", "Local task", InboxState::Inbox, 3000)],
        ..Default::default()
    };
    s.conn = Conn::Ready;

    peers::sync(&mut s);
    let ready = pump.until(&mut s, Duration::from_secs(5), |s| {
        peers::get(s, STUDIO).is_some_and(|p| p.conn == PeerConn::Ready)
    });
    assert!(ready, "peer gateway connected");
    (s, pump, local_gw, peer_gw)
}

#[test]
fn merged_shelves_sort_by_recency_and_fold_working_rows_from_every_computer() {
    let mut s = crate::chat::fixtures::test_state();
    s.fixture = true;
    s.conn = Conn::Ready;
    s.workspace.inbox.list = InboxListResponse {
        rows: vec![
            make_row("l-pin", "Local pin", InboxState::Pinned, 100),
            make_row("l-old", "Local older", InboxState::Inbox, 200),
            make_row("l-new", "Local newer", InboxState::Inbox, 400),
            make_row("l-work", "Local working", InboxState::Working, 300),
            make_row("l-set", "Local settled", InboxState::Settled, 50),
        ],
        ..Default::default()
    };
    let mut peer = peers::Peer::fixture(Computer::remote(STUDIO, "Studio"), PeerConn::Ready);
    peer.inbox = Some(InboxListResponse {
        rows: vec![
            make_row("p-pin", "Peer pin", InboxState::Pinned, 500),
            make_row("p-mid", "Peer middle", InboxState::Inbox, 350),
            make_row("p-work", "Peer working", InboxState::Working, 600),
            make_row("p-set", "Peer settled", InboxState::Settled, 90),
        ],
        ..Default::default()
    });
    s.remote.peers.insert_fixture(peer);

    let sh = shelves(&s);
    let ids =
        |v: &[Entry<'_>]| -> Vec<String> { v.iter().map(|e| e.row.thread_id.clone()).collect() };
    assert_eq!(ids(&sh.pinned), ["p-pin", "l-pin"]);
    assert_eq!(ids(&sh.main), ["l-new", "p-mid", "l-old"]);
    assert_eq!(
        ids(&sh.working),
        ["p-work", "l-work"],
        "working rows from every computer fold into the Working shelf"
    );
    assert_eq!(ids(&sh.settled), ["p-set", "l-set"]);

    // ⌘1..⌘5 follow the merged order (Pinned, then Inbox; Working is folded).
    assert_eq!(jump_target(&s, 1), Some(RowId::new(STUDIO, "p-pin")));
    assert_eq!(jump_target(&s, 2), Some(RowId::new(THIS_MAC, "l-pin")));
    assert_eq!(jump_target(&s, 3), Some(RowId::new(THIS_MAC, "l-new")));
    assert_eq!(jump_target(&s, 4), Some(RowId::new(STUDIO, "p-mid")));
    assert_eq!(jump_target(&s, 5), Some(RowId::new(THIS_MAC, "l-old")));
}

#[test]
fn equal_thread_ids_on_two_computers_never_collide() {
    let rt = runtime_with(|s| {
        crate::workspace::fixtures::fixture_shell(s);
        s.workspace.inbox.list = InboxListResponse {
            rows: vec![make_row(
                "shared-id",
                "Local thread",
                InboxState::Inbox,
                200,
            )],
            ..Default::default()
        };
        s.chat = Some(crate::chat::ChatState::new("shared-id".into()));
        let mut peer = peers::Peer::fixture(Computer::remote(STUDIO, "Studio"), PeerConn::Ready);
        let mut peer_sub = make_row("peer-sub", "Peer subagent", InboxState::Inbox, 150);
        peer_sub.parent_thread_id = Some("shared-id".into());
        peer.inbox = Some(InboxListResponse {
            rows: vec![
                make_row("shared-id", "Studio thread", InboxState::Inbox, 300),
                peer_sub,
            ],
            ..Default::default()
        });
        s.remote.peers.insert_fixture(peer);
    });

    let keys: Vec<String> = rt
        .elements()
        .into_iter()
        .filter_map(|e| (!e.exiting).then_some(e.key).flatten())
        .collect();
    assert!(keys.iter().any(|k| k == "row-this-mac:shared-id"));
    assert!(keys.iter().any(|k| k == &format!("row-{STUDIO}:shared-id")));

    // Subagent on Studio folds under Studio's parent, never This Mac's.
    let sh = shelves(&rt.state);
    assert_eq!(sh.tree.summary(&RowId::new(THIS_MAC, "shared-id")).total, 0);
    assert_eq!(sh.tree.summary(&RowId::new(STUDIO, "shared-id")).total, 1);
    assert_eq!(
        active_row(&rt.state),
        Some(RowId::new(THIS_MAC, "shared-id"))
    );
}

#[test]
fn computer_tags_show_only_on_peer_rows_when_more_than_one_computer_is_connected() {
    let single = runtime_with(crate::workspace::fixtures::fixture_shell);
    let single_tags = single
        .elements()
        .into_iter()
        .filter(|e| {
            !e.exiting
                && e.key
                    .as_deref()
                    .is_some_and(|k| k.starts_with("computer-tag-") || k == "sidebar-peer-status")
        })
        .count();
    assert_eq!(single_tags, 0, "single computer shows no tags or peer line");

    let two = runtime_with(crate::workspace::fixtures::fixture_inbox_two_computers);
    let tag_keys: Vec<String> = two
        .elements()
        .into_iter()
        .filter_map(|e| {
            (!e.exiting)
                .then_some(e.key)
                .flatten()
                .filter(|k| k.starts_with("computer-tag-"))
        })
        .collect();
    assert!(!tag_keys.is_empty(), "peer rows carry a computer tag");
    assert!(
        tag_keys.iter().all(|k| k.contains(STUDIO)),
        "focused rows carry no tag: {tag_keys:?}"
    );
}

#[test]
fn asleep_peer_shows_quiet_foot_line_that_opens_computers_settings() {
    let mut rt = runtime_with(crate::workspace::fixtures::fixture_inbox_peer_asleep);
    assert_eq!(
        peer_status_line(&rt.state).as_deref(),
        Some("Studio is asleep")
    );
    assert!(
        rt.elements()
            .into_iter()
            .any(|e| !e.exiting && e.key.as_deref() == Some("sidebar-peer-status")),
        "foot status line mounts"
    );
    assert!(
        rt.elements().into_iter().all(|e| {
            e.exiting
                || !e
                    .key
                    .as_deref()
                    .is_some_and(|k| k.starts_with("computer-tag-"))
        }),
        "no computer tags while only one computer is connected"
    );

    click_key(&mut rt, "sidebar-peer-status");
    assert_eq!(rt.state.page, Page::Computers);
}

#[test]
fn clicking_a_peer_row_hands_over_focus_and_opens_its_thread() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (s, _pump, local_gw, peer_gw) = two_live_gateways();
    let peer_before = peer_gw.paths().len();
    let local_before = local_gw.paths().len();

    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);
    rt.frame(1. / 60.);

    click_key(&mut rt, &format!("row-{STUDIO}:peer-1"));

    assert_eq!(rt.state.onboarding.computer.key(), STUDIO);
    assert_eq!(
        crate::workspace::active_id(&rt.state),
        Some("peer-1"),
        "peer thread is open after handover"
    );
    assert_eq!(
        local_gw.paths().len(),
        local_before,
        "no request sent to the old computer on handover"
    );
    assert!(
        peers::get(&rt.state, "env-local").is_some_and(|p| p.conn == PeerConn::Ready),
        "old focused computer became a ready peer"
    );
    // Handover itself sent zero descriptor/session/inbox requests.
    let new_peer_reqs: Vec<String> = peer_gw
        .paths()
        .into_iter()
        .skip(peer_before)
        .filter(|r| {
            r.contains("well-known") || r.contains("/api/session") || r.contains("/api/inbox")
        })
        .collect();
    assert!(
        new_peer_reqs.is_empty(),
        "handover sent no connect requests: {new_peer_reqs:?}"
    );
}

#[test]
fn marking_a_peer_row_routes_to_the_peers_gateway_and_updates_its_inbox() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (mut s, mut pump, local_gw, peer_gw) = two_live_gateways();
    peer_gw.script(vec![Answer::ok(inbox_json(serde_json::json!([row_json(
        "peer-1",
        "Build on Studio",
        "pinned",
        5000
    ),])))]);
    let local_before = local_gw.paths().len();

    mark_row(&mut s, RowId::new(STUDIO, "peer-1"), InboxAction::Pin, None);

    let updated = pump.until(&mut s, Duration::from_secs(5), |s| {
        shelves(s)
            .pinned
            .iter()
            .any(|e| e.id == RowId::new(STUDIO, "peer-1"))
    });
    assert!(
        updated,
        "peer row moved to Pinned after peer gateway answered"
    );
    assert_eq!(
        local_gw.paths().len(),
        local_before,
        "focused gateway received no mark request"
    );
    assert!(
        peer_gw.paths().iter().any(|r| r.contains("/inbox")),
        "peer gateway received the mark: {:?}",
        peer_gw.paths()
    );
}

#[test]
fn computers_page_reflects_peer_state_and_connect_performs_handover() {
    let _guard = crate::service::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (mut s, _pump, _local_gw, _peer_gw) = two_live_gateways();
    assert_eq!(remote::state_of(&s, "env-local"), EnvState::Connected);
    assert_eq!(remote::state_of(&s, STUDIO), EnvState::Connected);

    s.page = Page::Computers;
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);
    rt.frame(1. / 60.);

    click_key(&mut rt, &format!("connect-{STUDIO}"));
    assert_eq!(
        rt.state.onboarding.computer.key(),
        STUDIO,
        "Connect on a ready peer hands over focus"
    );
}
