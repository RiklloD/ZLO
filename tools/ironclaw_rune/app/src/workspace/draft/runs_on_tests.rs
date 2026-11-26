use std::time::Duration;

use rune::{Env, Runtime};
use zlo_client::attachments::{Body, StagedFile};
use zlo_client::computer_use::{
    ComputerUseEngineState, ComputerUsePermissionState, ComputerUsePlatform, ComputerUseReadiness,
};
use zlo_client::environment_access::ENVIRONMENT_PROTOCOL_VERSION;
use zlo_client::environments::{Active, Route, SavedEnvironment};
use zlo_client::inbox::{InboxListResponse, InboxProfile, InboxRow};
use zlo_client::notification_inbox::ProductNotification;
use zlo_client::wire::SessionInfo;

use super::*;
use crate::jobs::harness::Pump;
use crate::onboarding::gateway::Computer;
use crate::remote::connect;
use crate::remote::peers::{self, Peer, PeerConn};
use crate::state::{Conn, Page, State};
use crate::test_gateway::{Answer, Gateway};

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

fn inbox_json(thread_id: &str, title: &str, profiles: &[(&str, &str)]) -> String {
    let profs: Vec<_> = profiles
        .iter()
        .map(|(id, name)| {
            serde_json::json!({
                "profile_id": id,
                "display_name": name,
                "git_branch": null,
                "last_used_at": null
            })
        })
        .collect();
    serde_json::json!({
        "rows": [{
            "thread_id": thread_id,
            "profile_id": "default",
            "title": title,
            "state": "inbox",
            "status": null,
            "updated_at": 1700000000000i64,
            "last_activity_at": 1700000000000i64,
            "engines": []
        }],
        "profiles": profs,
        "last_used_profile_id": profiles.first().map(|(id, _)| *id)
    })
    .to_string()
}

fn providers_json(provider: &str, model: &str) -> String {
    serde_json::json!({
        "providers": [
            {
                "provider": "ironclaw",
                "name": "IronClaw",
                "runtime": "ironclaw",
                "authenticated": true,
                "models": ["claude-sonnet-5-5"],
                "efforts": []
            },
            {
                "provider": provider,
                "name": provider,
                "runtime": provider,
                "authenticated": true,
                "models": [model],
                "efforts": ["low", "high"]
            }
        ],
        "picker": {
            "hidden_providers": [],
            "hidden_models": [],
            "favorites": []
        }
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

fn parsed_inbox(thread_id: &str, title: &str, profiles: &[(&str, &str)]) -> InboxListResponse {
    let mut list = InboxListResponse::default();
    let row: InboxRow = serde_json::from_value(serde_json::json!({
        "thread_id": thread_id,
        "profile_id": "default",
        "title": title,
        "state": "inbox",
        "updated_at": 1700000000000i64,
        "last_activity_at": 1700000000000i64,
        "engines": []
    }))
    .expect("row");
    list.rows.push(row);
    list.profiles = profiles
        .iter()
        .map(|(id, name)| InboxProfile {
            profile_id: (*id).into(),
            display_name: (*name).into(),
            git_branch: None,
            last_used_at: None,
        })
        .collect();
    list.last_used_profile_id = profiles.first().map(|(id, _)| (*id).into());
    list
}

fn notif(id: &str, thread: &str) -> ProductNotification {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "kind": "approval_required",
        "severity": "warning",
        "action": { "kind": "open_thread", "thread_id": thread },
        "thread_id": thread,
        "turn_run_id": null,
        "created_at": "2026-11-26T00:00:00Z",
        "updated_at": "2026-11-26T00:00:00Z",
        "read_at": null,
        "resolved_at": null
    }))
    .expect("notification")
}

/// Start two scripted gateways (`mac` focused, `studio` ready peer).
/// `studio_script_tail` answers any requests sent to `studio` after the initial
/// peer handshake (descriptor, session, inbox, notifications).
fn two_gateways(
    studio_profiles: &[(&str, &str)],
    studio_tail: Vec<Answer>,
) -> (State, Pump, Gateway, Gateway) {
    let mut s = crate::chat::fixtures::test_state();
    s.fixture = false;
    let mut pump = Pump::install(&mut s);

    let mac_gw = Gateway::start(vec![Answer::ok("{}")]);
    let mut studio_script = vec![
        Answer::ok(descriptor_json("studio", "Studio")),
        Answer::ok(session_json()),
        Answer::ok(inbox_json("t-studio", "Studio thread", studio_profiles)),
        Answer::ok(r#"{"notifications":[],"unread_count":0}"#),
    ];
    studio_script.extend(studio_tail);
    let studio_gw = Gateway::start(studio_script);

    s.remote.registry.run_local_backend = false;
    s.remote.registry.environments = vec![
        saved_env("mac", "This Mac", &mac_gw.base_url),
        saved_env("studio", "Studio", &studio_gw.base_url),
    ];
    s.remote.registry.active = Active::Environment("mac".into());
    let _ = s.remote.tokens.set("mac", "tok-mac");
    let _ = s.remote.tokens.set("studio", "tok-studio");

    connect::adopt(&mut s, Computer::remote("mac", "This Mac"), None);
    s.rt.backend = Some(mac_gw.backend());
    s.session = Some(parsed_session());
    s.workspace.inbox.list = parsed_inbox(
        "t-mac",
        "Mac thread",
        &[("default", "Default"), ("work", "Work")],
    );
    s.conn = Conn::Ready;

    peers::sync(&mut s);
    assert!(pump.until(&mut s, Duration::from_secs(3), |s| {
        peers::get(s, "studio").is_some_and(|p| p.conn == PeerConn::Ready)
    }));

    (s, pump, mac_gw, studio_gw)
}

fn click_key(rt: &mut Runtime<State>, key: &str) {
    let e = rt
        .elements()
        .into_iter()
        .find(|e| !e.exiting && e.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("{key} is drawn"));
    let [x, y, w, h] = e.rect;
    let (x, y) = (x + w / 2., y + h / 2.);
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1. / 60.);
}

#[test]
fn picking_a_computer_keeps_draft_and_falls_back_missing_profile_and_model() {
    let (mut s, mut pump, _mac_gw, _studio_gw) = two_gateways(
        &[("default", "Default")],
        vec![Answer::ok(providers_json("codex", "gpt-6.1-sol"))],
    );

    crate::workspace::draft::open(&mut s);
    crate::workspace::draft::start_in_profile(&mut s, "work".into());
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.replace_text("Check the build");
        d.input.set_caret(5);
        d.staged.push(StagedFile {
            key: 7,
            filename: "PLAN.md".into(),
            mime_type: "text/markdown".into(),
            size: 120,
            body: Body::Bytes(vec![1, 2, 3]),
        });
        d.picker = crate::chat::model_picker::fixture();
        d.picker.thread_id = None;
    }
    crate::chat::model_picker::pick(
        &mut s,
        crate::chat::model_picker::DRAFT,
        "anthropic".into(),
        "claude-opus-5-5".into(),
    );

    pick(&mut s, "studio".into());

    assert_eq!(s.onboarding.computer.key(), "studio");
    assert_eq!(s.page, Page::Draft);
    let d = s.workspace.draft.as_ref().expect("draft restored");
    assert_eq!(d.input.text, "Check the build");
    assert_eq!(d.input.range().1, 5, "caret preserved");
    assert_eq!(d.staged.len(), 1);
    assert_eq!(d.staged[0].filename, "PLAN.md");
    assert_eq!(d.profile, "default", "missing profile fell back to default");
    assert!(
        d.picker.pending().is_none(),
        "stale model id is never sent before the new catalog loads"
    );

    // Feed the target computer's providers catalog through `model_picker::load`.
    crate::chat::model_picker::load(&mut s, crate::chat::model_picker::DRAFT);
    assert!(pump.until(&mut s, Duration::from_secs(3), |s| {
        s.workspace
            .draft
            .as_ref()
            .and_then(|d| d.picker.route.as_ref())
            .is_some()
    }));
    let d = s.workspace.draft.as_ref().expect("draft");
    assert_eq!(
        d.picker.current,
        Some(("ironclaw".into(), "claude-sonnet-5-5".into())),
        "missing model fell back to target computer's default"
    );
}

#[test]
fn picking_a_computer_keeps_profile_and_model_when_target_offers_them() {
    let (mut s, mut pump, _mac_gw, _studio_gw) = two_gateways(
        &[("default", "Default"), ("work", "Work")],
        vec![Answer::ok(providers_json("anthropic", "claude-opus-5-5"))],
    );

    crate::workspace::draft::open(&mut s);
    crate::workspace::draft::start_in_profile(&mut s, "work".into());
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.replace_text("Keep everything");
        d.picker = crate::chat::model_picker::fixture();
        d.picker.thread_id = None;
    }
    crate::chat::model_picker::pick(
        &mut s,
        crate::chat::model_picker::DRAFT,
        "anthropic".into(),
        "claude-opus-5-5".into(),
    );

    pick(&mut s, "studio".into());
    assert_eq!(s.onboarding.computer.key(), "studio");
    crate::chat::model_picker::load(&mut s, crate::chat::model_picker::DRAFT);
    assert!(pump.until(&mut s, Duration::from_secs(3), |s| {
        s.workspace
            .draft
            .as_ref()
            .and_then(|d| d.picker.route.as_ref())
            .is_some()
    }));
    let d = s.workspace.draft.as_ref().expect("draft");
    assert_eq!(d.profile, "work");
    assert_eq!(
        d.picker.current,
        Some(("anthropic".into(), "claude-opus-5-5".into()))
    );
}

#[test]
fn default_computer_pref_applied_when_ready_and_skipped_when_unavailable() {
    let (mut s, _pump, _mac_gw, _studio_gw) = two_gateways(&[("default", "Default")], vec![]);

    // 1. Named ready computer: opening a new draft moves focus there.
    s.prefs.default_computer = Some("studio".into());
    s.workspace.draft = None;
    crate::workspace::draft::open(&mut s);
    assert_eq!(s.onboarding.computer.key(), "studio");
    assert_eq!(s.page, Page::Draft);
    assert!(
        s.workspace
            .draft
            .as_ref()
            .is_some_and(|d| d.computer_note.is_none())
    );

    // 2. Named asleep computer: draft stays where it is and says so in one line.
    s.remote
        .registry
        .environments
        .push(saved_env("mini", "Mac mini", "http://127.0.0.1:1"));
    s.remote.peers.insert_fixture(Peer::fixture(
        Computer::remote("mini", "Mac mini"),
        PeerConn::Asleep,
    ));
    s.prefs.default_computer = Some("mini".into());
    s.workspace.draft = None;
    crate::workspace::draft::open(&mut s);
    assert_eq!(s.onboarding.computer.key(), "studio");
    let note = s
        .workspace
        .draft
        .as_ref()
        .and_then(|d| d.computer_note.as_deref())
        .expect("one-line note shown");
    assert!(
        note.contains("Mac mini") && note.contains("asleep"),
        "{note}"
    );

    // 3. Removed computer falls back to "last used" with no note.
    s.prefs.default_computer = Some("deleted-computer".into());
    assert!(default_computer::effective(&s).is_none());
    s.workspace.draft = None;
    crate::workspace::draft::open(&mut s);
    assert_eq!(s.onboarding.computer.key(), "studio");
    assert!(
        s.workspace
            .draft
            .as_ref()
            .is_some_and(|d| d.computer_note.is_none())
    );
}

#[test]
fn alert_click_routes_through_peers_focus_thread_and_bell_includes_peers() {
    let (mut s, _pump, _mac_gw, _studio_gw) = two_gateways(&[("default", "Default")], vec![]);

    // `two_gateways` already primed "studio" on peer connect; a new item posts now.
    let n2 = notif("p2", "t-studio");
    let fresh = s
        .workspace
        .alerts
        .take_new("studio", std::slice::from_ref(&n2));
    assert_eq!(fresh.len(), 1);
    s.workspace
        .alerts
        .post("studio", Some("Studio"), &n2, Some("Studio thread"));

    let tag = crate::alerts::notification_tag("studio", "p2");
    crate::workspace::alert_clicked(&mut s, &tag);
    assert_eq!(s.onboarding.computer.key(), "studio");
    assert_eq!(crate::workspace::active_id(&s), Some("t-studio"));

    // Bell count includes peers' unread notifications.
    let mut s2 = crate::chat::fixtures::test_state();
    s2.fixture = true;
    s2.conn = Conn::Ready;
    let mut peer = Peer::fixture(Computer::remote("studio", "Studio"), PeerConn::Ready);
    peer.notifications = vec![notif("a", "t1"), notif("b", "t2")];
    s2.remote.peers.insert_fixture(peer);
    crate::workspace::set_unread(&mut s2, 1);
    assert_eq!(s2.workspace.unread, 3);
}

#[test]
fn computer_use_run_on_moves_draft_and_existing_chat_only_explains() {
    let (mut s, _pump, _mac_gw, _studio_gw) = two_gateways(&[("default", "Default")], vec![]);

    let unsupported = ComputerUseReadiness {
        platform: ComputerUsePlatform::Unsupported,
        engine: ComputerUseEngineState::Missing,
        accessibility: ComputerUsePermissionState::NotApplicable,
        screen_recording: ComputerUsePermissionState::NotApplicable,
    };
    let ready = ComputerUseReadiness {
        platform: ComputerUsePlatform::Macos,
        engine: ComputerUseEngineState::Present,
        accessibility: ComputerUsePermissionState::Granted,
        screen_recording: ComputerUsePermissionState::Granted,
    };

    s.remote.focused_computer_readiness = Some(unsupported);
    peers::set_computer_readiness(&mut s, "studio", ready);
    assert_eq!(
        crate::chat::computer::remote::capable_peer(&s),
        Some(("studio".into(), "Studio".into()))
    );

    crate::workspace::draft::open(&mut s);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.replace_text("Click the button");
    }

    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);
    let has_run_on = rt
        .elements()
        .into_iter()
        .any(|e| !e.exiting && e.key.as_deref() == Some("draft-cu-run-on"));
    assert!(has_run_on, "draft shows Run on Studio action");

    click_key(&mut rt, "draft-cu-run-on");

    assert_eq!(rt.state.onboarding.computer.key(), "studio");
    assert_eq!(
        rt.state
            .workspace
            .draft
            .as_ref()
            .map(|d| d.input.text.as_str()),
        Some("Click the button")
    );
}

#[test]
fn notifications_page_lists_peer_items_tagged_and_opening_focuses_peer() {
    let mut s = crate::chat::fixtures::test_state();
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Notifications;
    crate::pages::notifications::fixture(&mut s);

    let mut peer = Peer::fixture(Computer::remote("studio", "Studio"), PeerConn::Ready);
    peer.notifications = vec![notif("p1", "t-studio")];
    peer.inbox = Some(parsed_inbox("t-studio", "Studio deploy", &[]));
    s.remote.peers.insert_fixture(peer);

    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);

    let peer_row = rt
        .elements()
        .into_iter()
        .find(|e| !e.exiting && e.key.as_deref() == Some("notification-studio-p1"))
        .expect("peer notification row rendered");
    assert!(
        peer_row
            .label
            .as_deref()
            .is_some_and(|l| l.contains("Studio")),
        "peer row tagged with Studio: {:?}",
        peer_row.label
    );

    click_key(&mut rt, "notification-studio-p1");

    assert_eq!(
        rt.state.remote.registry.active,
        Active::Environment("studio".into())
    );
    assert_eq!(
        rt.state
            .remote
            .last_thread
            .get("studio")
            .map(String::as_str),
        Some("t-studio")
    );
}
