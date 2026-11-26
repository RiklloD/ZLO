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
            folder: None,
            icon_revision: None,
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
    two_gateways_full(
        studio_profiles,
        r#"{"notifications":[],"unread_count":0}"#.into(),
        studio_tail,
    )
}

fn two_gateways_full(
    studio_profiles: &[(&str, &str)],
    studio_notifs_json: String,
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
        Answer::ok(studio_notifs_json),
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

/// A profile picked from the focused computer's list starts there, even
/// when new threads default to another computer: the pick names that
/// computer's profile. The new-thread button with nothing picked yet goes
/// to the default computer first, so the profiles it offers are that
/// computer's.
#[test]
fn an_explicit_profile_pick_stays_on_its_computer() {
    let (mut s, _pump, _mac_gw, _studio_gw) =
        two_gateways(&[("default", "Default"), ("work", "Work")], vec![]);
    s.prefs.default_computer = Some("studio".into());

    s.workspace.draft = None;
    crate::workspace::draft::start_in_profile(&mut s, "work".into());
    assert_eq!(s.onboarding.computer.key(), "mac", "no handover");
    assert_eq!(s.workspace.draft.as_ref().expect("draft").profile, "work");

    // Shift (⇧⌘N): the current profile, here.
    s.workspace.draft = None;
    crate::workspace::inbox::picker::new_thread(&mut s, true);
    assert_eq!(s.onboarding.computer.key(), "mac", "⇧⌘N stays");

    s.workspace.draft = None;
    crate::workspace::inbox::picker::new_thread(&mut s, false);
    assert_eq!(
        s.onboarding.computer.key(),
        "studio",
        "the button goes to the default computer before offering profiles"
    );
    assert!(!crate::workspace::command::is_new_thread_in(&s));
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
    peer.unread_count = 2;
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
    // A fixture peer has no warm connection: the click rides on the switch
    // it started, as an intent bound to that attempt.
    let pending = rt.state.remote.pending_thread.as_ref().expect("intent");
    assert_eq!(pending.computer, "studio");
    assert_eq!(pending.thread_id, "t-studio");
    assert!(pending.live(&rt.state));
}

#[test]
fn picking_a_computer_swaps_with_the_destinations_saved_draft() {
    let (mut s, _pump, _mac_gw, _studio_gw) = two_gateways(&[("default", "Default")], vec![]);

    // Leave an unsent new-thread draft saved on studio.
    peers::focus(&mut s, "studio");
    crate::workspace::draft::open(&mut s);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.replace_text("Studio existing draft");
    }
    crate::workspace::draft::record(&mut s);

    // Move back to mac and write a different draft.
    peers::focus(&mut s, "mac");
    crate::workspace::draft::open(&mut s);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.replace_text("Mac carried draft");
        d.staged.push(StagedFile {
            key: 9,
            filename: "NOTE.md".into(),
            mime_type: "text/markdown".into(),
            size: 42,
            body: Body::Bytes(vec![4, 5, 6]),
        });
    }
    crate::workspace::draft::record(&mut s);

    // Pick studio in "Runs on": the carried draft takes the screen there,
    // unmerged, and studio's waiting draft is neither destroyed nor mixed in.
    pick(&mut s, "studio".into());

    assert_eq!(s.onboarding.computer.key(), "studio");
    assert_eq!(s.page, Page::Draft);
    let d = s.workspace.draft.as_ref().expect("draft open on studio");
    assert_eq!(d.input.text, "Mac carried draft");
    assert_eq!(d.staged.len(), 1);
    assert_eq!(d.staged[0].filename, "NOTE.md");
    assert_eq!(
        crate::drafts::restore(&s, crate::drafts::NEW_THREAD)
            .map(|d| d.text)
            .as_deref(),
        Some("Mac carried draft"),
        "studio's slot holds what is on screen"
    );

    // The two swapped: mac's slot holds studio's waiting draft, and never a
    // second copy of the one that moved.
    peers::focus(&mut s, "mac");
    assert_eq!(
        crate::drafts::restore(&s, crate::drafts::NEW_THREAD)
            .map(|d| d.text)
            .as_deref(),
        Some("Studio existing draft")
    );
}

/// A computer that is not connected is no destination: the pick does
/// nothing, starts no switch, and the draft stays on its screen and disk.
#[test]
fn picking_a_computer_that_is_not_connected_moves_nothing() {
    let (mut s, _pump, _mac_gw, _studio_gw) = two_gateways(&[("default", "Default")], vec![]);
    s.remote.peers.insert_fixture(Peer::fixture(
        Computer::remote("mini", "Mac mini"),
        PeerConn::Asleep,
    ));
    crate::workspace::draft::open(&mut s);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.replace_text("Keep me on mac");
    }
    crate::workspace::draft::record(&mut s);
    let attempt = s.workspace.connect_gen;

    pick(&mut s, "mini".into());

    assert_eq!(s.onboarding.computer.key(), "mac");
    assert_eq!(s.workspace.connect_gen, attempt, "no switch started");
    assert!(s.remote.switching.is_none());
    assert!(s.workspace.carried_draft.is_none());
    assert_eq!(
        s.workspace.draft.as_ref().map(|d| d.input.text.as_str()),
        Some("Keep me on mac")
    );
}

#[test]
fn failed_or_superseded_cold_switch_keeps_source_draft_and_does_not_leak_carry() {
    let (mut s, mut pump, _mac_gw, _studio_gw) = two_gateways(&[("default", "Default")], vec![]);

    // Add a third saved computer that refuses connections (closed loopback port).
    let dead_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let dead_addr = dead_listener.local_addr().expect("addr");
    drop(dead_listener);
    let dead_url = format!("http://{dead_addr}");
    s.remote
        .registry
        .environments
        .push(saved_env("mini", "Mac mini", &dead_url));
    let _ = s.remote.tokens.set("mini", "tok-mini");

    crate::workspace::draft::open(&mut s);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.replace_text("Keep me on mac");
    }
    crate::workspace::draft::record(&mut s);

    // 1. Cold switch to unreachable mini fails: draft stays intact on mac.
    pick(&mut s, "mini".into());
    assert!(pump.until(&mut s, Duration::from_secs(5), |s| {
        s.remote.switching.is_none()
    }));
    assert_eq!(s.onboarding.computer.key(), "mac");
    assert_eq!(
        s.workspace.draft.as_ref().map(|d| d.input.text.as_str()),
        Some("Keep me on mac")
    );
    assert_eq!(
        crate::drafts::restore(&s, crate::drafts::NEW_THREAD)
            .map(|d| d.text)
            .as_deref(),
        Some("Keep me on mac")
    );

    // 2. Start a cold switch to mini, then supersede it by focusing studio.
    pick(&mut s, "mini".into());
    peers::focus(&mut s, "studio");

    assert_eq!(s.onboarding.computer.key(), "studio");
    assert_eq!(
        s.page,
        Page::Chat,
        "superseding switch to studio must not consume mini's carried draft"
    );
    assert_eq!(crate::workspace::active_id(&s), Some("t-studio"));
    assert!(s.workspace.draft.is_none());
    assert!(
        !crate::drafts::has(&s, crate::drafts::NEW_THREAD),
        "studio must not have mini's carried draft written to its disk store"
    );
    assert!(s.workspace.carried_draft.is_none());

    // Source draft on mac is still intact.
    peers::focus(&mut s, "mac");
    assert_eq!(
        crate::drafts::restore(&s, crate::drafts::NEW_THREAD)
            .map(|d| d.text)
            .as_deref(),
        Some("Keep me on mac")
    );
}

#[test]
fn peer_unread_total_uses_server_count_and_mark_all_read_reaches_peer() {
    // 20 newer READ notifications on studio, plus 5 older UNREAD ones reported
    // only in the server's `unread_count`.
    let read_items: Vec<serde_json::Value> = (0..20)
        .map(|i| {
            serde_json::json!({
                "id": format!("r{i}"),
                "kind": "run_completed",
                "severity": "success",
                "action": { "kind": "open_thread", "thread_id": "t-studio" },
                "thread_id": "t-studio",
                "turn_run_id": null,
                "created_at": "2026-11-26T00:00:00Z",
                "updated_at": "2026-11-26T00:00:00Z",
                "read_at": "2026-11-26T00:01:00Z",
                "resolved_at": null
            })
        })
        .collect();
    let studio_notifs = serde_json::json!({
        "notifications": read_items,
        "unread_count": 5
    })
    .to_string();

    let (mut s, mut pump, _mac_gw, studio_gw) = two_gateways_full(
        &[("default", "Default")],
        studio_notifs,
        vec![
            Answer::ok(r#"{"updated":5}"#),
            Answer::ok(inbox_json(
                "t-studio",
                "Studio thread",
                &[("default", "Default")],
            )),
            Answer::ok(r#"{"notifications":[],"unread_count":0}"#),
        ],
    );

    crate::workspace::set_unread(&mut s, 0);
    assert_eq!(
        s.workspace.unread, 5,
        "bell count must use server unread_count, not count of latest 20 items"
    );

    s.page = Page::Notifications;
    s.pages.notifications.loaded = true;
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);
    assert!(
        rt.elements()
            .into_iter()
            .any(|e| !e.exiting && e.key.as_deref() == Some("mark-all-read")),
        "Mark all read button must show when a peer has server unread_count > 0"
    );

    click_key(&mut rt, "mark-all-read");
    assert!(
        pump.until(&mut rt.state, Duration::from_secs(3), |_| {
            studio_gw
                .paths()
                .iter()
                .any(|p| p == "/api/webchat/v2/notifications/read-all")
        }),
        "studio gateway must receive mark-all-read: {:?}",
        studio_gw.paths()
    );
}

#[test]
fn failed_peer_mark_all_read_rolls_back_optimistic_read_flag() {
    let studio_notifs = serde_json::json!({
        "notifications": [notif("p1", "t-studio")],
        "unread_count": 1
    })
    .to_string();
    let (mut s, mut pump, _mac_gw, _studio_gw) = two_gateways_full(
        &[("default", "Default")],
        studio_notifs,
        vec![Answer::fail()],
    );

    s.page = Page::Notifications;
    s.pages.notifications.loaded = true;
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);

    let has_peer_unread_dot = |rt: &Runtime<State>| {
        rt.elements().into_iter().any(|e| {
            !e.exiting
                && e.key.as_deref() == Some("unread")
                && e.path.contains("notification-studio-p1")
        })
    };
    assert!(has_peer_unread_dot(&rt));

    click_key(&mut rt, "mark-all-read");
    assert!(
        !has_peer_unread_dot(&rt),
        "unread dot hides optimistically on click"
    );

    assert!(pump.until(&mut rt.state, Duration::from_secs(3), |s| {
        s.pages.notifications.error.is_some()
    }));
    rt.update(|_, _| {});
    rt.frame(1. / 60.);
    assert!(
        has_peer_unread_dot(&rt),
        "failed peer mark-all-read must roll back the optimistic read flag"
    );
}

#[test]
fn side_chat_and_local_search_or_sent_state_do_not_cross_computers() {
    let (mut s, _pump, _mac_gw, _studio_gw) = two_gateways(&[("default", "Default")], vec![]);

    // Put the same thread id on both mac and studio.
    s.workspace.inbox.list = parsed_inbox("shared-1", "Mac shared", &[("default", "Default")]);
    let studio_inbox = parsed_inbox("shared-1", "Studio shared", &[("default", "Default")]);
    peers::apply_inbox(&mut s, "studio", studio_inbox, &mut rune::Tx::default());

    // Record "shared-1" as a side chat on mac.
    s.update_prefs(|p| {
        p.side_chats
            .entry("mac~parent-mac".into())
            .or_default()
            .push("shared-1".into());
    });

    // Populate focused message search and optimistic sent state on mac.
    s.workspace.message_search.fixture(
        "deploy",
        vec![zlo_client::history::HistoryThreadResult {
            thread_id: "shared-1".into(),
            title: Some("Mac shared".into()),
            updated_at: None,
            origin: zlo_client::history::HistoryThreadOrigin::Own,
            lineage_root_thread_id: None,
            related_thread_ids: Vec::new(),
            hits: vec![zlo_client::history::HistoryHit {
                thread_id: "shared-1".into(),
                message_id: "m1".into(),
                sequence: 1,
                role: zlo_client::history::HistorySearchRole::User,
                created_at: None,
                snippet: "mac deploy snippet".into(),
                highlights: Vec::new(),
            }],
        }],
    );
    crate::workspace::prompt_sent(&mut s, "shared-1");

    // Switch focus to studio.
    peers::focus(&mut s, "studio");
    assert_eq!(s.onboarding.computer.key(), "studio");

    // 1. Studio's ordinary row "shared-1" must NOT be hidden by mac's side chat.
    let sh = crate::workspace::inbox::shelves(&s);
    assert!(
        sh.main
            .iter()
            .any(|e| e.id == crate::workspace::RowId::new("studio", "shared-1")),
        "studio's row with the same id must not be hidden by mac's side chat"
    );

    // 2. Mac's cached message-search hit must be cleared on switch.
    assert!(
        s.workspace
            .message_search
            .hit("deploy", "shared-1")
            .is_none(),
        "focused message search results must clear on computer switch"
    );

    // 3. Mac's optimistic sent state must be cleared on switch.
    let studio_row = s
        .workspace
        .inbox
        .row("shared-1")
        .expect("studio row present");
    assert_eq!(
        s.workspace.inbox.shown_status(studio_row),
        None,
        "optimistic sent state must clear on computer switch"
    );
}

#[test]
fn clicking_peer_notification_with_live_gateways_opens_thread_and_marks_read_on_peer() {
    let studio_notifs = serde_json::json!({
        "notifications": [notif("p1", "t-studio")],
        "unread_count": 1
    })
    .to_string();
    let (mut s, mut pump, mac_gw, studio_gw) = two_gateways_full(
        &[("default", "Default")],
        studio_notifs,
        vec![
            Answer::ok(r#"{"updated":1}"#),
            Answer::ok(r#"{"notifications":[],"unread_count":0}"#),
        ],
    );
    let mac_before = mac_gw.paths().len();

    s.page = Page::Notifications;
    s.pages.notifications.loaded = true;
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);

    click_key(&mut rt, "notification-studio-p1");

    assert_eq!(rt.state.onboarding.computer.key(), "studio");
    assert_eq!(
        crate::workspace::active_id(&rt.state),
        Some("t-studio"),
        "clicking a peer notification must open that thread on the peer"
    );
    assert!(
        pump.until(&mut rt.state, Duration::from_secs(3), |_| {
            studio_gw
                .paths()
                .iter()
                .any(|p| p == "/api/webchat/v2/notifications/p1/read")
        }),
        "studio gateway must receive exact mark-read path: {:?}",
        studio_gw.paths()
    );
    assert!(
        !mac_gw
            .paths()
            .iter()
            .skip(mac_before)
            .any(|p| p.contains("/notifications/p1/read")),
        "mac gateway must not receive studio's mark-read: {:?}",
        mac_gw.paths()
    );
}

/// A file staged by its path on the source is read for the destination
/// when the draft moves to another computer, keeping its key (its card in
/// the text still points at it), so the send there does not refuse it.
#[test]
fn a_moved_drafts_local_file_is_read_for_the_other_computer() {
    let (mut s, mut pump, _mac_gw, _studio_gw) = two_gateways(&[("default", "Default")], vec![]);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("NOTE.md");
    std::fs::write(&path, b"hello").expect("write");

    crate::workspace::draft::open(&mut s);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.replace_text("Read this");
        d.staged.push(StagedFile {
            key: 77,
            filename: "NOTE.md".into(),
            mime_type: "text/markdown".into(),
            size: 5,
            body: Body::Path(path.clone()),
        });
    }
    crate::workspace::draft::record(&mut s);

    pick(&mut s, "studio".into());
    assert_eq!(s.onboarding.computer.key(), "studio");

    assert!(
        pump.until(&mut s, Duration::from_secs(5), |s| {
            s.workspace.draft.as_ref().is_some_and(|d| {
                d.staged
                    .iter()
                    .any(|f| f.key == 77 && f.bytes() == Some(b"hello".as_slice()))
            })
        }),
        "the file was read for studio, under its own key"
    );
    let d = s.workspace.draft.as_ref().expect("draft");
    assert!(crate::remote::check_outgoing(&s, &d.staged).is_ok());
    assert_eq!(d.input.text, "Read this");
    // The saved copy holds the bytes too: leaving and reopening the draft
    // does not bring the path back.
    let saved = crate::drafts::restore(&s, crate::drafts::NEW_THREAD).expect("saved");
    assert!(saved.files.iter().all(|f| f.bytes().is_some()));
}

/// A triage draft follows the focus (Settings → Computers → "Switch to").
/// A draft that was waiting on the computer it lands on is not overwritten:
/// it goes to the slot the triage draft left.
#[test]
fn a_triage_draft_following_the_focus_keeps_the_waiting_draft() {
    let (mut s, _pump, _mac_gw, _studio_gw) = two_gateways(&[("default", "Default")], vec![]);

    peers::focus(&mut s, "studio");
    crate::workspace::draft::open(&mut s);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.replace_text("Studio waiting draft");
    }
    crate::workspace::draft::record(&mut s);

    peers::focus(&mut s, "mac");
    crate::workspace::triage_test::start(&mut s);
    assert!(crate::workspace::triage::is_triage_draft(&s));
    assert!(
        control_hidden(&s),
        "a triage draft offers no Runs on control: it does not move by that route"
    );

    peers::focus(&mut s, "studio");

    assert_eq!(s.onboarding.computer.key(), "studio");
    assert!(crate::workspace::triage::is_triage_draft(&s));
    peers::focus(&mut s, "mac");
    assert_eq!(
        crate::drafts::restore(&s, crate::drafts::NEW_THREAD)
            .map(|d| d.text)
            .as_deref(),
        Some("Studio waiting draft"),
        "the waiting draft moved to the slot the triage draft left"
    );
}

fn control_hidden(s: &State) -> bool {
    let k = crate::design::kit(&s.palette);
    let mut layer = Vec::new();
    control(s, &k, &mut layer).is_none()
}
