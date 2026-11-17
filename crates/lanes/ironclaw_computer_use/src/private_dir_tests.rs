//! Private directory checks: ancestry validation from `ZLO_HOME` down, and
//! descriptor-relative use that a later path swap cannot redirect.
use std::fs;
use std::path::Path;

use super::*;

#[test]
fn capture_names_match_only_our_patterns() {
    assert!(is_capture_file_name(&new_capture_file_name()));
    assert!(!is_capture_file_name("cap-1234.jpg"));
    assert!(!is_capture_file_name("photo.jpg"));
    assert!(is_legacy_capture_file_name("ironclaw_cu_9_1.jpg"));
    assert!(!is_legacy_capture_file_name("ironclaw_cu_notes.jpg"));
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn open(home: &Path) -> Result<PrivateDir, PrivateDirError> {
        PrivateDir::open(home, &CAPTURE_DIR, current_uid())
    }

    #[test]
    fn creates_the_leaf_private_and_reopens_idempotently() {
        let home = tempfile::tempdir().unwrap();

        let dir = open(home.path()).unwrap();
        assert_eq!(dir.path(), home.path().join("cache/computer-use/captures"));
        assert_eq!(mode(dir.path()), 0o700);
        open(home.path()).unwrap();
    }

    #[test]
    fn a_too_open_leaf_we_own_is_narrowed() {
        let home = tempfile::tempdir().unwrap();
        let leaf = home.path().join("cache/computer-use/captures");
        fs::create_dir_all(&leaf).unwrap();
        fs::set_permissions(&leaf, fs::Permissions::from_mode(0o755)).unwrap();

        open(home.path()).unwrap();

        assert_eq!(mode(&leaf), 0o700);
    }

    #[test]
    fn an_ancestry_owned_by_another_user_is_refused() {
        let home = tempfile::tempdir().unwrap();
        // Ownership seam: expect a uid that owns none of it.
        let other_user = current_uid().wrapping_add(1);

        let refused = PrivateDir::open(home.path(), &CAPTURE_DIR, other_user);

        assert!(
            matches!(refused, Err(PrivateDirError::NotOwned(_))),
            "{refused:?}"
        );
    }

    #[test]
    fn a_group_or_world_writable_ancestor_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let cache = home.path().join("cache");
        fs::create_dir(&cache).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o777)).unwrap();

        let refused = open(home.path());

        assert!(
            matches!(refused, Err(PrivateDirError::Writable(_))),
            "{refused:?}"
        );
    }

    #[test]
    fn a_symlinked_leaf_or_ancestor_is_refused_and_not_followed() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = home.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::create_dir_all(home.path().join("cache/computer-use")).unwrap();
        symlink(&elsewhere, home.path().join("cache/computer-use/captures")).unwrap();

        let refused = open(home.path());
        assert!(
            matches!(refused, Err(PrivateDirError::NotADirectory(_))),
            "{refused:?}"
        );

        let ancestor_home = tempfile::tempdir().unwrap();
        symlink(&elsewhere, ancestor_home.path().join("cache")).unwrap();
        let refused = open(ancestor_home.path());
        assert!(
            matches!(refused, Err(PrivateDirError::NotADirectory(_))),
            "{refused:?}"
        );
        assert_eq!(
            fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "nothing created through the link"
        );
    }

    /// The replacement race made deterministic: the directory is swapped for
    /// a symlink between validation (open) and use. Reads and deletes go
    /// through the held descriptor, so they reach the validated directory's
    /// files and never the symlink target's.
    #[test]
    fn a_directory_swapped_after_validation_cannot_redirect_reads_or_deletes() {
        let home = tempfile::tempdir().unwrap();
        let dir = open(home.path()).unwrap();
        let leaf = dir.path().to_path_buf();
        let name = new_capture_file_name();
        fs::write(leaf.join(&name), b"our capture").unwrap();
        let victim = home.path().join("victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join(&name), b"someone else's file").unwrap();

        // Swap: move the validated directory away, plant a symlink.
        let moved = home.path().join("moved");
        fs::rename(&leaf, &moved).unwrap();
        symlink(&victim, &leaf).unwrap();

        assert_eq!(
            dir.read_owned_file(&name, true).unwrap(),
            b"our capture",
            "the read reaches the held directory, not the symlink target"
        );
        dir.remove(&name).unwrap();
        assert!(
            !moved.join(&name).exists(),
            "the delete hit the held directory"
        );
        assert_eq!(
            fs::read(victim.join(&name)).unwrap(),
            b"someone else's file",
            "the symlink target is untouched"
        );
        assert!(
            dir.verify().is_ok(),
            "the held directory itself is still ours and private"
        );
    }

    #[test]
    fn a_symlinked_or_hard_linked_file_is_not_read() {
        let home = tempfile::tempdir().unwrap();
        let dir = open(home.path()).unwrap();
        let outside = home.path().join("outside.jpg");
        fs::write(&outside, b"not ours to read").unwrap();
        symlink(&outside, dir.path().join("link.jpg")).unwrap();
        fs::hard_link(&outside, dir.path().join("hard.jpg")).unwrap();
        fs::write(dir.path().join("ok.jpg"), b"a capture").unwrap();

        assert!(dir.read_owned_file("link.jpg", true).is_err());
        assert!(dir.read_owned_file("hard.jpg", true).is_err());
        assert_eq!(dir.read_owned_file("ok.jpg", true).unwrap(), b"a capture");
    }

    #[test]
    fn the_stale_sweep_lists_only_our_regular_files() {
        let home = tempfile::tempdir().unwrap();
        let dir = open(home.path()).unwrap();
        let ours = new_capture_file_name();
        fs::write(dir.path().join(&ours), b"x").unwrap();
        let outside = home.path().join("victim.jpg");
        fs::write(&outside, b"x").unwrap();
        let linked = new_capture_file_name();
        symlink(&outside, dir.path().join(&linked)).unwrap();
        fs::write(dir.path().join("notes.jpg"), b"x").unwrap();

        let stale = dir.stale_files(&is_capture_file_name, std::time::Duration::ZERO);

        assert_eq!(stale, vec![ours]);
    }
}
