//! `~/.ironclaw/reborn` → `~/.zlo` migration, driven through
//! `RebornHome::resolve_from_env_parts_migrating` — the same resolution
//! `RebornHome::resolve_from_env` runs at boot — against temp user homes.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
};

use ironclaw_config::{
    HomeMigration, MIGRATION_PENDING_MARKER, RebornConfigError, RebornHome, RebornHomeSource,
};

fn resolve(user_home: &Path) -> Result<RebornHome, RebornConfigError> {
    RebornHome::resolve_from_env_parts_migrating(None, Some(user_home.into()), None)
}

fn legacy(user_home: &Path) -> PathBuf {
    user_home.join(".ironclaw").join("reborn")
}

fn seed_legacy(user_home: &Path) -> PathBuf {
    let legacy = legacy(user_home);
    fs::create_dir_all(legacy.join("workspace/projects")).expect("legacy dirs");
    fs::write(legacy.join("config.toml"), "[webui]\nlisten_port = 3000\n").expect("config");
    fs::write(legacy.join("webui-token"), "t".repeat(64)).expect("token");
    fs::write(legacy.join("workspace/projects/notes.md"), "hello").expect("note");
    legacy
}

#[test]
fn fresh_install_resolves_to_zlo_and_creates_nothing() {
    let temp = tempfile::tempdir().expect("tempdir");

    let home = resolve(temp.path()).expect("fresh home resolves");

    assert_eq!(home.path(), temp.path().join(".zlo"));
    assert_eq!(home.source(), RebornHomeSource::Default);
    assert_eq!(home.migration(), &HomeMigration::NotNeeded);
    assert!(!temp.path().join(".zlo").exists());
    assert!(!temp.path().join(".ironclaw").exists());
}

#[test]
fn existing_install_is_moved_and_old_path_links_to_new() {
    let temp = tempfile::tempdir().expect("tempdir");
    let legacy = seed_legacy(temp.path());
    let zlo = temp.path().join(".zlo");

    let home = resolve(temp.path()).expect("migration succeeds");

    assert_eq!(home.path(), zlo);
    assert_eq!(
        home.migration(),
        &HomeMigration::Migrated {
            from: legacy.clone(),
            to: zlo.clone()
        }
    );
    assert!(home.migration().notice().is_some());
    // The data is in the new folder…
    assert!(fs::symlink_metadata(&zlo).expect("zlo").is_dir());
    assert_eq!(
        fs::read_to_string(zlo.join("workspace/projects/notes.md")).expect("moved note"),
        "hello"
    );
    assert!(!zlo.join(MIGRATION_PENDING_MARKER).exists());
    // …and the old path is a link to it, so stored absolute paths resolve.
    let meta = fs::symlink_metadata(&legacy).expect("legacy link");
    assert!(meta.file_type().is_symlink());
    assert_eq!(fs::read_link(&legacy).expect("read link"), zlo);
    assert_eq!(
        fs::read_to_string(legacy.join("workspace/projects/notes.md")).expect("note via old path"),
        "hello"
    );
}

#[test]
fn stored_old_paths_canonicalize_inside_the_new_home() {
    // Containment checks in the backend (workspace/skill roots, mentions
    // roots, project roots) canonicalize both sides. A path recorded before
    // the move must still land under the canonical new home.
    let temp = tempfile::tempdir().expect("tempdir");
    let legacy = seed_legacy(temp.path());
    let stored = legacy.join("workspace/projects/notes.md");

    let home = resolve(temp.path()).expect("migration succeeds");

    let canonical_home = home.path().canonicalize().expect("canonical home");
    let canonical_stored = stored.canonicalize().expect("old path still resolves");
    assert!(canonical_stored.starts_with(&canonical_home));
    assert_eq!(
        legacy.canonicalize().expect("canonical legacy"),
        canonical_home
    );
}

#[test]
fn already_migrated_home_is_a_no_op() {
    let temp = tempfile::tempdir().expect("tempdir");
    seed_legacy(temp.path());
    resolve(temp.path()).expect("first start migrates");

    let again = resolve(temp.path()).expect("second start");

    assert_eq!(again.migration(), &HomeMigration::NotNeeded);
    assert!(
        fs::symlink_metadata(legacy(temp.path()))
            .expect("link kept")
            .file_type()
            .is_symlink()
    );
    assert!(temp.path().join(".zlo/config.toml").is_file());
}

#[test]
fn empty_zlo_folder_does_not_hide_existing_data() {
    // Something created an empty ~/.zlo before the first migrating start:
    // the data must still move rather than starting on the empty folder.
    let temp = tempfile::tempdir().expect("tempdir");
    seed_legacy(temp.path());
    fs::create_dir(temp.path().join(".zlo")).expect("empty zlo");

    let home = resolve(temp.path()).expect("migration succeeds");

    assert!(matches!(home.migration(), HomeMigration::Migrated { .. }));
    assert!(home.path().join("config.toml").is_file());
}

#[test]
fn both_folders_with_data_use_zlo_and_leave_old_untouched() {
    let temp = tempfile::tempdir().expect("tempdir");
    let legacy = seed_legacy(temp.path());
    let zlo = temp.path().join(".zlo");
    fs::create_dir(&zlo).expect("zlo");
    fs::write(zlo.join("config.toml"), "").expect("zlo config");

    let home = resolve(temp.path()).expect("resolves");

    assert_eq!(home.path(), zlo);
    assert_eq!(
        home.migration(),
        &HomeMigration::BothExist {
            legacy: legacy.clone(),
            current: zlo.clone()
        }
    );
    let notice = home.migration().notice().expect("warns");
    assert!(notice.starts_with("warning:"), "{notice}");
    assert!(fs::symlink_metadata(&legacy).expect("legacy").is_dir());
    assert!(legacy.join("workspace/projects/notes.md").is_file());
}

#[test]
fn file_at_zlo_path_is_never_overwritten() {
    let temp = tempfile::tempdir().expect("tempdir");
    let legacy = seed_legacy(temp.path());
    let zlo = temp.path().join(".zlo");
    fs::write(&zlo, "not a folder").expect("zlo file");

    let home = resolve(temp.path()).expect("resolves without touching data");

    assert!(matches!(home.migration(), HomeMigration::BothExist { .. }));
    assert_eq!(fs::read_to_string(&zlo).expect("file kept"), "not a folder");
    assert!(legacy.join("config.toml").is_file());
}

#[test]
fn unwritable_legacy_parent_fails_loudly_instead_of_using_an_empty_home() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let legacy = seed_legacy(temp.path());
    let parent = temp.path().join(".ironclaw");
    // Read-only parent: the lock file and the rename both need write access.
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).expect("chmod");
    let writable_anyway = fs::write(parent.join("probe"), "").is_ok(); // root ignores modes

    let result = resolve(temp.path());

    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).expect("restore");
    if writable_anyway {
        return;
    }
    let error = result.expect_err("migration must fail loudly");
    assert!(
        matches!(&error, RebornConfigError::HomeMigration { from, to, .. }
            if *from == legacy && *to == temp.path().join(".zlo")),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("Nothing was deleted"), "{message}");
    assert!(message.contains("ZLO_HOME="), "{message}");
    assert!(legacy.join("config.toml").is_file(), "data stays put");
    assert!(!temp.path().join(".zlo").exists(), "no empty home created");
}

#[test]
fn interrupted_migration_is_finished_on_next_start() {
    // Simulate a crash after the move but before the link.
    let temp = tempfile::tempdir().expect("tempdir");
    let legacy = seed_legacy(temp.path());
    let zlo = temp.path().join(".zlo");
    fs::write(legacy.join(MIGRATION_PENDING_MARKER), "").expect("marker");
    fs::rename(&legacy, &zlo).expect("move");

    let home = resolve(temp.path()).expect("recovers");

    assert!(matches!(home.migration(), HomeMigration::Migrated { .. }));
    assert_eq!(fs::read_link(&legacy).expect("link created"), zlo);
    assert!(!zlo.join(MIGRATION_PENDING_MARKER).exists());
}

#[test]
fn explicit_home_is_never_migrated() {
    let temp = tempfile::tempdir().expect("tempdir");
    let legacy = seed_legacy(temp.path());

    let home = RebornHome::resolve_from_env_parts_migrating(
        Some(legacy.clone().into_os_string()),
        Some(temp.path().into()),
        None,
    )
    .expect("explicit legacy path is still honored");

    assert_eq!(home.path(), legacy);
    assert_eq!(home.migration(), &HomeMigration::NotNeeded);
    assert!(fs::symlink_metadata(&legacy).expect("legacy").is_dir());
    assert!(!temp.path().join(".zlo").exists());
}

#[test]
fn concurrent_starts_migrate_exactly_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    seed_legacy(temp.path());
    let user_home = Arc::new(temp.path().to_path_buf());
    let starts = 8;
    let barrier = Arc::new(Barrier::new(starts));

    let outcomes: Vec<HomeMigration> = (0..starts)
        .map(|_| {
            let user_home = Arc::clone(&user_home);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                resolve(&user_home).map(|home| home.migration().clone())
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("thread")
                .expect("every start resolves")
        })
        .collect();

    let migrated = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, HomeMigration::Migrated { .. }))
        .count();
    assert_eq!(migrated, 1, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| !matches!(outcome, HomeMigration::BothExist { .. })),
        "{outcomes:?}"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join(".zlo/workspace/projects/notes.md")).expect("note"),
        "hello"
    );
    assert!(
        fs::symlink_metadata(legacy(temp.path()))
            .expect("link")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn read_only_resolution_finds_unmoved_data_without_moving_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let legacy = seed_legacy(temp.path());

    let home =
        RebornHome::resolve_from_env_parts_without_migrating(None, Some(temp.path().into()), None)
            .expect("resolves");

    assert_eq!(home.path(), legacy, "points at where the data is");
    assert_eq!(home.migration(), &HomeMigration::NotNeeded);
    assert!(fs::symlink_metadata(&legacy).expect("legacy").is_dir());
    assert!(!temp.path().join(".zlo").exists(), "nothing moved");

    // After the move, the read-only path follows the data to ~/.zlo.
    resolve(temp.path()).expect("migrate");
    let home =
        RebornHome::resolve_from_env_parts_without_migrating(None, Some(temp.path().into()), None)
            .expect("resolves");
    assert_eq!(home.path(), temp.path().join(".zlo"));

    // Fresh install: ~/.zlo, nothing created.
    let fresh = tempfile::tempdir().expect("tempdir");
    let home =
        RebornHome::resolve_from_env_parts_without_migrating(None, Some(fresh.path().into()), None)
            .expect("resolves");
    assert_eq!(home.path(), fresh.path().join(".zlo"));
    assert!(!fresh.path().join(".zlo").exists());
}
