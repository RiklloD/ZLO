//! A profile made, renamed, archived or imported in Settings reaches the
//! profile picker (`workspace/inbox/picker.rs`) without a thread in it
//! first: the picker reads the inbox's profile list, not this page's.

use std::time::Duration;

use zlo_client::inbox::{InboxListResponse, InboxProfile};
use zlo_client::profiles::{ProfileInfo, ProfileSettings};

use crate::jobs::harness::Pump;
use crate::state::State;
use crate::test_gateway::{Answer, Gateway};

const INBOX: &str = "/inbox";
const WAIT: Duration = Duration::from_secs(10);

fn info(id: &str, name: &str) -> ProfileInfo {
    ProfileInfo {
        profile_id: id.into(),
        settings: ProfileSettings {
            display_name: name.into(),
            ..ProfileSettings::default()
        },
        is_default: id == "default",
        effective_folder: String::new(),
        git_branch: None,
        thread_count: 0,
        last_used_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn picked(id: &str, name: &str, used: Option<i64>) -> InboxProfile {
    InboxProfile {
        profile_id: id.into(),
        display_name: name.into(),
        git_branch: None,
        last_used_at: used,
        folder: None,
    }
}

fn inbox(profiles: Vec<InboxProfile>, last: Option<&str>) -> String {
    serde_json::to_string(&InboxListResponse {
        rows: vec![],
        profiles,
        last_used_profile_id: last.map(str::to_string),
    })
    .expect("inbox json")
}

/// The picker as the inbox last listed it: Default, and Work when `work`.
fn state(work: bool) -> State {
    let mut s = crate::chat::fixtures::test_state();
    let mut profiles = vec![picked("default", "Default", Some(900))];
    let mut items = vec![info("default", "Default")];
    if work {
        profiles.push(picked("work", "Work", Some(500)));
        items.push(info("work", "Work"));
    }
    s.workspace.inbox.list.profiles = profiles;
    s.workspace.inbox.list.last_used_profile_id =
        Some(if work { "work" } else { "default" }.into());
    s.pages.profiles.items = items;
    s
}

fn live(mut s: State, script: Vec<Answer>) -> (State, Pump, Gateway) {
    let gateway = Gateway::start(script);
    s.rt.backend = Some(gateway.backend());
    let pump = Pump::install(&mut s);
    (s, pump, gateway)
}

fn picker_ids(s: &State) -> Vec<&str> {
    s.workspace
        .inbox
        .list
        .profiles
        .iter()
        .map(|p| p.profile_id.as_str())
        .collect()
}

/// The bug: a new profile stayed out of the picker until a thread
/// started in it. Saving lists it at once, then re-reads the inbox.
#[test]
fn a_saved_new_profile_is_in_the_picker_without_a_thread() {
    let reread = inbox(
        vec![
            picked("default", "Default", Some(900)),
            InboxProfile {
                git_branch: Some("thesis-draft".into()),
                ..picked("thesis", "Thesis", None)
            },
        ],
        Some("default"),
    );
    let (mut s, mut pump, gateway) = live(
        state(false),
        vec![
            Answer::ok(serde_json::to_string(&info("thesis", "Thesis")).expect("json")),
            Answer::ok(reread),
        ],
    );
    super::select(&mut s, None);
    s.pages.profiles.id.replace_text("thesis");
    s.pages.profiles.name.replace_text("Thesis");
    super::save(&mut s);

    assert!(pump.until(&mut s, WAIT, |s| !s.pages.profiles.saving));
    assert_eq!(s.pages.profiles.error, None);
    assert!(
        picker_ids(&s).contains(&"thesis"),
        "the saved reply lists it: {:?}",
        picker_ids(&s)
    );
    assert!(pump.until(&mut s, WAIT, |_| gateway.requests_to(INBOX) >= 1));
    // The re-read lands: only it knows the folder's branch.
    assert!(pump.until(&mut s, WAIT, |s| {
        s.workspace
            .inbox
            .list
            .profiles
            .iter()
            .any(|p| p.profile_id == "thesis" && p.git_branch.as_deref() == Some("thesis-draft"))
    }));
    assert_eq!(picker_ids(&s), ["default", "thesis"]);
    assert!(
        !gateway.paths().iter().any(|p| p.contains("/threads")),
        "no thread was made: {:?}",
        gateway.paths()
    );
}

/// A rename shows under the new name, in place: no second row.
#[test]
fn a_renamed_profile_keeps_one_row_with_its_new_name() {
    let (mut s, mut pump, _gateway) = live(
        state(true),
        vec![
            // `save` re-reads the profiles before an update.
            Answer::ok(
                serde_json::to_string(&zlo_client::profiles::ProfileListResponse {
                    profiles: vec![info("default", "Default"), info("work", "Work")],
                    last_used_profile_id: Some("work".into()),
                })
                .expect("json"),
            ),
            Answer::ok(
                serde_json::to_string(&ProfileInfo {
                    last_used_at: Some(500),
                    ..info("work", "Day job")
                })
                .expect("json"),
            ),
            // Held back, so the test sees the reply's own merge first.
            Answer::ok(inbox(vec![], None)).after(Duration::from_secs(30)),
        ],
    );
    super::select(&mut s, Some("work".into()));
    s.pages.profiles.name.replace_text("Day job");
    super::save(&mut s);

    assert!(pump.until(&mut s, WAIT, |s| !s.pages.profiles.saving));
    let rows: Vec<_> = s
        .workspace
        .inbox
        .list
        .profiles
        .iter()
        .filter(|p| p.profile_id == "work")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].display_name, "Day job");
    assert_eq!(rows[0].last_used_at, Some(500));
}

/// An archived profile leaves the picker, and stops being the one it
/// opens on.
#[test]
fn a_deleted_profile_leaves_the_picker() {
    let reread = inbox(
        vec![picked("default", "Default", Some(900))],
        Some("default"),
    );
    let (mut s, mut pump, gateway) = live(state(true), vec![Answer::ok("{}"), Answer::ok(reread)]);
    super::select(&mut s, Some("work".into()));
    super::delete(&mut s);

    assert!(pump.until(&mut s, WAIT, |s| !picker_ids(s).contains(&"work")));
    assert_ne!(
        s.workspace.inbox.list.last_used_profile_id.as_deref(),
        Some("work")
    );
    assert!(pump.until(&mut s, WAIT, |_| gateway.requests_to(INBOX) >= 1));
}

/// A draft in the archived profile moves to one that exists (it showed
/// "Default" yet sent the archived id), and a sidebar filter on it lifts
/// (its chip is gone, so nothing could turn it off).
#[test]
fn an_archive_moves_the_draft_and_lifts_the_filter() {
    let reread = inbox(
        vec![picked("default", "Default", Some(900))],
        Some("default"),
    );
    let (mut s, mut pump, _gateway) = live(state(true), vec![Answer::ok("{}"), Answer::ok(reread)]);
    crate::workspace::start_in_profile(&mut s, "work".into());
    s.workspace.inbox.profile = Some("work".into());
    assert_eq!(
        s.workspace.draft.as_ref().map(|d| d.profile.as_str()),
        Some("work")
    );

    super::select(&mut s, Some("work".into()));
    super::delete(&mut s);

    assert!(pump.until(&mut s, WAIT, |s| !picker_ids(s).contains(&"work")));
    assert_eq!(
        s.workspace.draft.as_ref().map(|d| d.profile.as_str()),
        Some("default")
    );
    assert_eq!(s.workspace.inbox.profile, None);
}

/// A read that left before an archive answers with the archived profile
/// in it: that answer must not bring it back; the read after does count.
#[test]
fn a_read_from_before_an_archive_does_not_revive_it() {
    let before = inbox(
        vec![
            picked("default", "Default", Some(900)),
            picked("work", "Work", Some(950)),
        ],
        Some("work"),
    );
    let after = inbox(
        vec![InboxProfile {
            git_branch: Some("main".into()),
            ..picked("default", "Default", Some(900))
        }],
        Some("default"),
    );
    let (mut s, mut pump, gateway) = live(
        state(true),
        vec![
            // Long enough that the archive's own reply lands first even on a
            // loaded machine; the test then sees this answer arrive stale.
            Answer::ok(before).after(Duration::from_millis(2_000)),
            Answer::ok("{}"),
            Answer::ok(after),
        ],
    );
    crate::workspace::refresh_inbox(&mut s);
    // Wait for the read to reach the gateway without landing its answer.
    let asked = std::time::Instant::now();
    while gateway.requests_to(INBOX) == 0 {
        assert!(asked.elapsed() < WAIT, "the inbox read never left");
        std::thread::sleep(Duration::from_millis(5));
    }
    super::select(&mut s, Some("work".into()));
    super::delete(&mut s);

    let mut revived = false;
    let mut archived = false;
    assert!(pump.until(&mut s, WAIT, |s| {
        archived |= !s
            .pages
            .profiles
            .items
            .iter()
            .any(|p| p.profile_id == "work");
        revived |= archived && picker_ids(s).contains(&"work");
        s.workspace
            .inbox
            .list
            .profiles
            .iter()
            .any(|p| p.git_branch.is_some())
    }));
    assert!(!revived, "the old read put Work back");
    assert_eq!(picker_ids(&s), ["default"]);
    assert_eq!(
        s.workspace.inbox.list.last_used_profile_id.as_deref(),
        Some("default")
    );
}

/// Imported profiles come from the server: the import re-reads the inbox.
#[test]
fn an_import_rereads_the_picker() {
    // Every read after the import gets the inbox; the page's own reads
    // fail to decode it, which this test does not look at.
    let reread = inbox(
        vec![
            picked("default", "Default", Some(900)),
            picked("hermes", "Hermes", None),
        ],
        Some("default"),
    );
    let (mut s, mut pump, gateway) = live(
        state(false),
        vec![
            Answer::ok(r#"{"imported":["hermes"],"skipped":[]}"#),
            Answer::ok(reread),
        ],
    );
    super::import(&mut s);

    assert!(pump.until(&mut s, WAIT, |s| picker_ids(s).contains(&"hermes")));
    assert!(gateway.requests_to(INBOX) >= 1);
}

/// Before the first inbox read the picker shows a stand-in Default; a
/// saved profile must not replace it with a one-row list.
#[test]
fn a_save_before_the_first_inbox_read_waits_for_the_read() {
    let mut s = crate::chat::fixtures::test_state();
    crate::workspace::profiles_changed(
        &mut s,
        crate::workspace::ProfileChange::Saved(&info("thesis", "Thesis")),
    );
    assert!(s.workspace.inbox.list.profiles.is_empty());
}

/// Pressing Archive while a Save is in flight must not send a concurrent
/// delete that races with the save reply.
#[test]
fn delete_is_ignored_while_a_save_is_in_flight() {
    let reread = inbox(
        vec![
            picked("default", "Default", Some(900)),
            picked("work", "Work 2", Some(500)),
        ],
        Some("work"),
    );
    let (mut s, mut pump, gateway) = live(
        state(true),
        vec![
            Answer::ok(
                serde_json::to_string(&zlo_client::profiles::ProfileListResponse {
                    profiles: vec![info("default", "Default"), info("work", "Work")],
                    last_used_profile_id: Some("work".into()),
                })
                .expect("json"),
            ),
            Answer::ok(serde_json::to_string(&info("work", "Work 2")).expect("json")),
            Answer::ok(reread),
        ],
    );
    super::select(&mut s, Some("work".into()));
    s.pages.profiles.name.replace_text("Work 2");
    super::save(&mut s);
    assert!(s.pages.profiles.saving);

    // Attempt to delete while save is in flight.
    super::delete(&mut s);
    assert!(!s.pages.profiles.deleting);

    assert!(pump.until(&mut s, WAIT, |s| !s.pages.profiles.saving));
    assert!(picker_ids(&s).contains(&"work"));
    assert!(
        !gateway
            .paths()
            .iter()
            .any(|p| p.ends_with("/profiles/work") && gateway.requests_to("/profiles/work") > 1),
        "unexpected extra request: {:?}",
        gateway.paths()
    );
}

/// If the computer switches route while an inbox read is in flight, the
/// rerouted reply must clear `loading` and retry on the new route instead
/// of wedging every future inbox refresh.
#[test]
fn inbox_refresh_recovers_after_route_change() {
    let first = inbox(
        vec![picked("default", "Default", Some(900))],
        Some("default"),
    );
    let second = inbox(
        vec![
            picked("default", "Default", Some(900)),
            picked("thesis", "Thesis", None),
        ],
        Some("default"),
    );
    let (mut s, mut pump, gateway) = live(
        state(false),
        vec![
            Answer::ok(first).after(Duration::from_millis(100)),
            Answer::ok(second),
        ],
    );
    crate::workspace::refresh_inbox(&mut s);
    assert!(s.workspace.inbox.is_loading());
    // Simulate a route change on the same computer while the first read is in flight.
    s.remote.tag.generation.bump();

    assert!(pump.until(&mut s, WAIT, |s| {
        !s.workspace.inbox.is_loading() && picker_ids(s).contains(&"thesis")
    }));
    assert!(gateway.requests_to(INBOX) >= 2);
}
