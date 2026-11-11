//! Built-in coding tools: read/write/list/glob/grep shapes and sensitive-path guards.

use std::{sync::Arc, thread, time::Duration};

use ironclaw_filesystem::LibSqlRootFilesystem;
use ironclaw_filesystem::RootFilesystem;
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_api::{
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, VirtualPath},
};
use ironclaw_host_runtime::{
    APPLY_PATCH_CAPABILITY_ID, GLOB_CAPABILITY_ID, GREP_CAPABILITY_ID, LIST_DIR_CAPABILITY_ID,
    READ_FILE_CAPABILITY_ID, WRITE_FILE_CAPABILITY_ID,
};
use serde_json::json;

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{context::*, fixtures::*, invoke::*, runtimes::*};

#[tokio::test]
async fn builtin_read_file_reads_scoped_virtual_filesystem_through_mount_services() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("README.md"), "scoped read\n").unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem_and_policy(filesystem, network_denied_policy());

    let output = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/README.md"}),
        execution_context_with_mounts([READ_FILE_CAPABILITY_ID], mounts),
    )
    .await
    .unwrap();

    assert_eq!(output["content"], json!("     1│ scoped read"));
    assert_eq!(output["path"], json!("/workspace/README.md"));
    assert_eq!(output["total_lines"], json!(1));
}

#[tokio::test]
async fn builtin_read_file_uses_scoped_mounts_for_hosted_tenant_workspace() {
    let temp = tempfile::tempdir().unwrap();
    tokio::fs::write(temp.path().join("README.md"), "hosted scoped read\n")
        .await
        .unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem_and_policy(filesystem, hosted_dev_policy());

    let output = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/README.md"}),
        execution_context_with_mounts([READ_FILE_CAPABILITY_ID], mounts),
    )
    .await
    .unwrap();

    assert_eq!(output["content"], json!("     1│ hosted scoped read"));
    assert_eq!(output["path"], json!("/workspace/README.md"));
    assert_eq!(output["total_lines"], json!(1));
}

#[tokio::test]
async fn builtin_coding_tools_match_v1_read_write_list_glob_and_grep_shapes() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("README.md"), "alpha\nbeta\n").unwrap();
    std::fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn sample() {\n    // needle\n}\n",
    )
    .unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts.clone());

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/README.md", "offset": 2, "limit": 1}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(read["content"], json!("     2│ beta"));
    assert_eq!(read["total_lines"], json!(2));
    assert_eq!(read["lines_shown"], json!(1));
    assert_eq!(read["truncated_by_default"], json!(false));

    let wrote = invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/generated/deep.txt", "content": "created\n"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(wrote["bytes_written"], json!(8));
    assert_eq!(wrote["success"], json!(true));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("generated/deep.txt")).unwrap(),
        "created\n"
    );

    let wrote_subdir_readme = invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/generated/README.md", "content": "ok\n"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(wrote_subdir_readme["success"], json!(true));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("generated/README.md")).unwrap(),
        "ok\n"
    );

    let listed = invoke_with_context(
        &runtime,
        LIST_DIR_CAPABILITY_ID,
        json!({"path": "/workspace", "recursive": true, "max_depth": 1}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(listed["count"], json!(6));
    assert_eq!(listed["truncated"], json!(false));
    assert_eq!(
        listed["entries"],
        json!([
            "generated/",
            "src/",
            "README.md (11B)",
            "generated/README.md (3B)",
            "generated/deep.txt (8B)",
            "src/lib.rs (34B)"
        ])
    );

    let globbed = invoke_with_context(
        &runtime,
        GLOB_CAPABILITY_ID,
        json!({"path": "/workspace", "pattern": "**/*.rs", "max_results": 5}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(globbed["files"], json!(["src/lib.rs"]));
    assert_eq!(globbed["count"], json!(1));
    assert_eq!(globbed["truncated"], json!(false));

    let grepped = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({"path": "/workspace", "pattern": "NEEDLE", "case_insensitive": true}),
        context,
    )
    .await
    .unwrap();
    assert_eq!(grepped["files"], json!(["src/lib.rs"]));
    assert_eq!(grepped["count"], json!(1));
    assert_eq!(grepped["truncated"], json!(false));
}

#[tokio::test]
async fn read_file_enforces_byte_budget_on_long_lines_and_offers_continuation() {
    // A few very long lines: only 6 lines (well under the 2000-line cap) but
    // ~180 KB total, the shape that let a 310 KB log dump into context and
    // exhaust the pinchbench turn budget. The byte cap must truncate it.
    let temp = tempfile::tempdir().unwrap();
    let wide_line = "x".repeat(30 * 1024);
    let body = (0..6)
        .map(|_| wide_line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(temp.path().join("wide.log"), format!("{body}\n")).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/wide.log"}),
        context.clone(),
    )
    .await
    .unwrap();

    assert_eq!(read["total_lines"], json!(6));
    assert_eq!(read["truncated"], json!(true));
    assert_eq!(read["truncated_by"], json!("bytes"));
    // Stopped well before all 6 lines, and the body stays inside the budget
    // including the continuation notice.
    let shown = read["lines_shown"].as_u64().unwrap();
    assert!(
        (1..6).contains(&shown),
        "expected partial read, got {shown}"
    );
    let content = read["content"].as_str().unwrap();
    assert!(
        content.len() <= 64 * 1024,
        "body exceeded byte budget: {} bytes",
        content.len()
    );
    let next = read["next_offset"].as_u64().unwrap();
    assert_eq!(next, shown + 1);
    assert!(content.contains("run ONE shell command or script"));
    assert!(content.contains("do NOT page through it"));
    assert!(content.contains(&format!("offset={next}")));

    // Resuming from next_offset advances past the already-shown lines.
    let resumed = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/wide.log", "offset": next}),
        context.clone(),
    )
    .await
    .unwrap();
    let resumed_first = resumed["content"].as_str().unwrap();
    assert!(resumed_first.starts_with(&format!("{:>6}│", next)));
}

#[tokio::test]
async fn read_file_saturates_large_limit_without_overflow() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("lines.txt"), "first\nsecond\nthird\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({
            "path": "/workspace/lines.txt",
            "offset": 2,
            "limit": usize::MAX,
        }),
        context,
    )
    .await
    .unwrap();

    assert_eq!(read["total_lines"], json!(3));
    assert_eq!(read["lines_shown"], json!(2));
    assert_eq!(read["truncated"], json!(false));
    assert!(read["truncated_by"].is_null());
    assert!(read["next_offset"].is_null());
    let content = read["content"].as_str().unwrap();
    assert!(content.starts_with("     2│ second"));
    assert!(content.contains("     3│ third"));
    assert!(!content.contains("     1│ first"));
}

#[tokio::test]
async fn read_file_clamps_a_single_line_larger_than_the_whole_budget() {
    // One line bigger than the entire byte budget must still return something
    // (clamped on a UTF-8 boundary) and advance the cursor rather than emit empty.
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("blob.txt"),
        format!("{}\nnext\n", "y".repeat(100 * 1024)),
    )
    .unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/blob.txt"}),
        context.clone(),
    )
    .await
    .unwrap();

    assert_eq!(read["lines_shown"], json!(1));
    assert_eq!(read["truncated_by"], json!("bytes"));
    assert_eq!(read["next_offset"], json!(2));
    let content = read["content"].as_str().unwrap();
    assert!(content.contains("[line truncated]"));
    assert!(
        content.len() <= 64 * 1024,
        "body exceeded byte budget: {} bytes",
        content.len()
    );
}

#[tokio::test]
async fn read_file_tolerates_stray_nul_and_invalid_utf8_in_text_logs() {
    // A real syslog-shaped file with one stray NUL and one invalid UTF-8 byte.
    // The strict probe/decode rejected these, forcing the agent into a grep-only
    // fallback (pinchbench syslog tasks). The read path must now decode it lossily.
    let temp = tempfile::tempdir().unwrap();
    let mut bytes = b"Jan  1 00:00:00 host sshd[1]: Failed password for root\n".to_vec();
    bytes.push(0u8); // stray NUL
    bytes.extend_from_slice(b"\xffJan  1 00:00:01 host sshd[1]: more log line\n"); // invalid UTF-8
    std::fs::write(temp.path().join("syslog.log"), &bytes).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/syslog.log"}),
        context.clone(),
    )
    .await
    .expect("text log with stray NUL / invalid UTF-8 must read, not hard-fail");
    let content = read["content"].as_str().unwrap();
    assert!(content.contains("Failed password for root"));
    assert!(content.contains("more log line"));
}

#[tokio::test]
async fn write_file_overwrites_a_readable_text_log_with_a_stray_nul() {
    let temp = tempfile::tempdir().unwrap();
    let mut original = b"Jan  1 00:00:00 host sshd[1]: Failed password for root\n".to_vec();
    original.push(0u8);
    original.extend_from_slice(b"Jan  1 00:00:01 host sshd[1]: more log line\n");
    let log_path = temp.path().join("syslog.log");
    tokio::fs::write(&log_path, original).await.unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/syslog.log"}),
        context.clone(),
    )
    .await
    .expect("text log with a stray NUL must be readable");

    invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/syslog.log", "content": "redacted log\n"}),
        context,
    )
    .await
    .expect("a text log accepted by read_file must remain writable");

    assert_eq!(tokio::fs::read(log_path).await.unwrap(), b"redacted log\n");
}

#[tokio::test]
async fn read_file_still_rejects_nul_dense_binary() {
    // Genuine binary (NUL-dense): must still be kept out of context rather than
    // dumped as U+FFFD soup. 25% NUL bytes clears both the floor and the ratio.
    let temp = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..4096)
        .map(|i| if i % 4 == 0 { 0u8 } else { b'A' })
        .collect();
    std::fs::write(temp.path().join("blob.bin"), &bytes).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/blob.bin"}),
        context.clone(),
    )
    .await
    .expect_err("NUL-dense binary must still be rejected by read_file");
}

#[tokio::test]
async fn builtin_coding_paths_are_relative_to_requested_root_and_zero_values_match_v1() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("src/nested")).unwrap();
    std::fs::write(temp.path().join("src/lib.rs"), "needle\n").unwrap();
    std::fs::write(temp.path().join("src/nested/mod.rs"), "needle\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/src/lib.rs", "offset": 0, "limit": 0}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(read["content"], json!(""));
    assert_eq!(read["lines_shown"], json!(0));

    let listed = invoke_with_context(
        &runtime,
        LIST_DIR_CAPABILITY_ID,
        json!({"path": "/workspace/src", "recursive": true, "max_depth": 0}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(listed["entries"], json!(["nested/", "lib.rs (7B)"]));

    let globbed_empty_page = invoke_with_context(
        &runtime,
        GLOB_CAPABILITY_ID,
        json!({"path": "/workspace/src", "pattern": "*.rs", "max_results": 0}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(globbed_empty_page["files"], json!([]));
    assert_eq!(globbed_empty_page["count"], json!(0));
    assert_eq!(globbed_empty_page["truncated"], json!(true));

    let globbed = invoke_with_context(
        &runtime,
        GLOB_CAPABILITY_ID,
        json!({"path": "/workspace/src", "pattern": "*.rs"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(globbed["files"], json!(["lib.rs"]));

    let grepped = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({"path": "/workspace/src", "pattern": "needle"}),
        context,
    )
    .await
    .unwrap();
    let grep_files = grepped["files"].as_array().unwrap();
    assert_eq!(grep_files.len(), 2);
    assert!(
        grep_files
            .iter()
            .any(|file| file.as_str() == Some("lib.rs"))
    );
    assert!(
        grep_files
            .iter()
            .any(|file| file.as_str() == Some("nested/mod.rs"))
    );
}

#[tokio::test]
async fn builtin_coding_glob_and_grep_files_with_matches_sort_newest_first() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("old.rs"), "needle old\n").unwrap();
    thread::sleep(Duration::from_millis(1_100));
    std::fs::write(temp.path().join("new.rs"), "needle new\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let globbed = invoke_with_context(
        &runtime,
        GLOB_CAPABILITY_ID,
        json!({"path": "/workspace", "pattern": "*.rs"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(globbed["files"], json!(["new.rs", "old.rs"]));

    let grepped = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({"path": "/workspace", "pattern": "needle"}),
        context,
    )
    .await
    .unwrap();
    assert_eq!(grepped["files"], json!(["new.rs", "old.rs"]));
}

#[tokio::test]
async fn builtin_coding_blocks_sensitive_scoped_paths_like_v1() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join(".env"), "TOKEN=secret\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    for (capability, input) in [
        (READ_FILE_CAPABILITY_ID, json!({"path": "/workspace/.env"})),
        (
            WRITE_FILE_CAPABILITY_ID,
            json!({"path": "/workspace/.env", "content": "changed"}),
        ),
        (
            APPLY_PATCH_CAPABILITY_ID,
            json!({"path": "/workspace/.env", "old_string": "TOKEN", "new_string": "X"}),
        ),
    ] {
        let error = invoke_with_context(&runtime, capability, input, context.clone())
            .await
            .unwrap_err();
        assert_eq!(error, FailureKind::FilesystemDenied);
    }

    let listed = invoke_with_context(
        &runtime,
        LIST_DIR_CAPABILITY_ID,
        json!({"path": "/workspace"}),
        context,
    )
    .await
    .unwrap();
    let entries = listed["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .all(|entry| entry.as_str() != Some(".env (13B)"))
    );
}

#[tokio::test]
async fn builtin_coding_blocks_sensitive_host_paths_like_v1() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".ssh")).unwrap();
    std::fs::create_dir_all(temp.path().join(".aws")).unwrap();
    std::fs::create_dir_all(temp.path().join(".config/gh")).unwrap();
    std::fs::write(temp.path().join(".ssh/id_rsa"), "ssh-secret\n").unwrap();
    std::fs::write(temp.path().join(".aws/credentials"), "aws-secret\n").unwrap();
    std::fs::write(temp.path().join(".config/gh/hosts.yml"), "gh-secret\n").unwrap();
    std::fs::write(temp.path().join(".env"), "TOKEN=secret\n").unwrap();
    std::fs::write(temp.path().join("server.key"), "tls-secret\n").unwrap();
    std::fs::write(temp.path().join("safe.txt"), "safe host file\n").unwrap();

    let (filesystem, mounts) = mounted_host_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    for path in [
        "/host/.ssh/id_rsa",
        "/host/.aws/credentials",
        "/host/.config/gh/hosts.yml",
        "/host/.env",
        "/host/server.key",
    ] {
        let error = invoke_with_context(
            &runtime,
            READ_FILE_CAPABILITY_ID,
            json!({"path": path}),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, FailureKind::FilesystemDenied, "{path}");
    }

    for (capability, input) in [
        (
            WRITE_FILE_CAPABILITY_ID,
            json!({"path": "/host/.env", "content": "changed"}),
        ),
        (
            APPLY_PATCH_CAPABILITY_ID,
            json!({"path": "/host/server.key", "old_string": "tls", "new_string": "x"}),
        ),
    ] {
        let error = invoke_with_context(&runtime, capability, input, context.clone())
            .await
            .unwrap_err();
        assert_eq!(error, FailureKind::FilesystemDenied);
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temp.path().join(".ssh"), temp.path().join("dotssh")).unwrap();
        let error = invoke_with_context(
            &runtime,
            WRITE_FILE_CAPABILITY_ID,
            json!({"path": "/host/dotssh/config", "content": "created through symlink\n"}),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, FailureKind::FilesystemDenied);
        assert!(
            !temp.path().join(".ssh/config").exists(),
            "write must not create files under sensitive canonical parents"
        );

        let error = invoke_with_context(
            &runtime,
            WRITE_FILE_CAPABILITY_ID,
            json!({"path": "/host/dotssh/generated/config", "content": "created through nested symlink\n"}),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, FailureKind::FilesystemDenied);
        assert!(
            !temp.path().join(".ssh/generated").exists(),
            "write must not create intermediate directories under sensitive canonical parents"
        );

        let error = invoke_with_context(
            &runtime,
            LIST_DIR_CAPABILITY_ID,
            json!({"path": "/host/dotssh", "recursive": true}),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, FailureKind::FilesystemDenied);

        let listed = invoke_with_context(
            &runtime,
            LIST_DIR_CAPABILITY_ID,
            json!({"path": "/host", "recursive": true}),
            context.clone(),
        )
        .await
        .unwrap();
        let entries = listed["entries"].as_array().unwrap();
        assert!(
            /* safety: test-only assertion. */
            entries
                .iter()
                .all(|entry| !entry.as_str().unwrap_or_default().contains("id_rsa")),
            "recursive list_dir must not traverse sensitive symlink targets: {entries:?}"
        );
    }

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/host/safe.txt"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(read["content"], json!("     1│ safe host file"));

    invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/host/output.txt", "content": "created\n"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("output.txt")).unwrap(),
        "created\n"
    );

    let raw_host_home = temp.path().canonicalize().unwrap();
    let raw_host_home = raw_host_home.to_string_lossy();
    let raw_sensitive_path = format!("{raw_host_home}/.ssh/id_rsa");
    let error = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": raw_sensitive_path}),
        context.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, FailureKind::FilesystemDenied);

    let raw_safe_path = format!("{raw_host_home}/safe.txt");
    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": raw_safe_path}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(read["content"], json!("     1│ safe host file"));

    let raw_output_path = format!("{raw_host_home}/raw-output.txt");
    invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": raw_output_path, "content": "raw created\n"}),
        context,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("raw-output.txt")).unwrap(),
        "raw created\n"
    );
}

#[tokio::test]
async fn builtin_coding_blocks_sensitive_resolved_libsql_paths() {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("filesystem.db");
    let db = Arc::new(libsql::Builder::new_local(db_path).build().await.unwrap());
    let filesystem = LibSqlRootFilesystem::new(db).expect("filesystem runtime");
    filesystem.run_migrations().await.unwrap();
    filesystem
        .create_dir_all(&VirtualPath::new("/projects/p").unwrap())
        .await
        .unwrap();
    filesystem
        .write_file(
            &VirtualPath::new("/projects/p/.env").unwrap(),
            b"TOKEN=secret\n",
        )
        .await
        .unwrap();

    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/p/.env").unwrap(),
        MountPermissions::read_only(),
    )])
    .unwrap();
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    for (capability, input) in [
        (READ_FILE_CAPABILITY_ID, json!({"path": "/workspace"})),
        (
            GREP_CAPABILITY_ID,
            json!({"path": "/workspace", "pattern": "TOKEN"}),
        ),
    ] {
        let error = invoke_with_context(&runtime, capability, input, context.clone())
            .await
            .unwrap_err();
        assert_eq!(error, FailureKind::FilesystemDenied);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builtin_coding_apply_patch_serializes_concurrent_edits_on_same_path() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("code.rs"), "A\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = std::sync::Arc::new(runtime_with_filesystem(filesystem));
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    // Prime read-before-edit state so both patches start from a valid cached hash.
    invoke_with_context(
        &*runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs"}),
        context.clone(),
    )
    .await
    .unwrap();

    let runtime_a = runtime.clone();
    let ctx_a = context.clone();
    let runtime_b = runtime.clone();
    let ctx_b = context.clone();
    let task_a = tokio::spawn(async move {
        invoke_with_context(
            &*runtime_a,
            APPLY_PATCH_CAPABILITY_ID,
            json!({"path": "/workspace/code.rs", "old_string": "A", "new_string": "X"}),
            ctx_a,
        )
        .await
    });
    let task_b = tokio::spawn(async move {
        invoke_with_context(
            &*runtime_b,
            APPLY_PATCH_CAPABILITY_ID,
            json!({"path": "/workspace/code.rs", "old_string": "A", "new_string": "Y"}),
            ctx_b,
        )
        .await
    });
    let result_a = task_a.await.unwrap();
    let result_b = task_b.await.unwrap();

    // Serialization guarantee: the second patch reads the post-write file and
    // no longer finds `old_string`, so exactly one apply_patch must succeed.
    // Without the per-path edit lock, both calls can read the original file
    // concurrently and silently lose an update.
    let outcomes = [result_a.is_ok(), result_b.is_ok()];
    assert_eq!(
        outcomes.iter().filter(|ok| **ok).count(),
        1,
        "expected exactly one concurrent apply_patch to succeed, got {:?}",
        outcomes
    );
    let final_content = std::fs::read_to_string(temp.path().join("code.rs")).unwrap();
    assert!(
        final_content == "X\n" || final_content == "Y\n",
        "unexpected final content {final_content:?}"
    );
}

#[tokio::test]
async fn builtin_coding_blocks_relative_workspace_protected_paths() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("daily")).unwrap();
    std::fs::create_dir_all(temp.path().join("context")).unwrap();
    std::fs::write(temp.path().join("README.md"), "keep\n").unwrap();
    std::fs::write(temp.path().join("daily/note.md"), "keep\n").unwrap();
    std::fs::write(temp.path().join("context/session.md"), "keep\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "./README.md"}),
        context.clone(),
    )
    .await
    .unwrap();
    let root_readme_write = invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "./README.md", "content": "changed\n"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(root_readme_write["success"], json!(true));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("README.md")).unwrap(),
        "changed\n"
    );

    for (tool_path, host_path) in [
        ("./daily/note.md", "daily/note.md"),
        ("./context/session.md", "context/session.md"),
    ] {
        let write_error = invoke_with_context(
            &runtime,
            WRITE_FILE_CAPABILITY_ID,
            json!({"path": tool_path, "content": "changed\n"}),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(write_error, FailureKind::InputEncode);
        assert_eq!(
            std::fs::read_to_string(temp.path().join(host_path)).unwrap(),
            "keep\n"
        );

        invoke_with_context(
            &runtime,
            READ_FILE_CAPABILITY_ID,
            json!({"path": tool_path}),
            context.clone(),
        )
        .await
        .unwrap();

        let patch_error = invoke_with_context(
            &runtime,
            APPLY_PATCH_CAPABILITY_ID,
            json!({"path": tool_path, "old_string": "keep", "new_string": "changed"}),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(patch_error, FailureKind::InputEncode);
        assert_eq!(
            std::fs::read_to_string(temp.path().join(host_path)).unwrap(),
            "keep\n"
        );
    }
}

#[tokio::test]
async fn builtin_coding_grep_searches_single_file_paths_like_v1() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/lib.rs"), "needle\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(
        temp.path(),
        MountPermissions {
            read: true,
            write: false,
            delete: false,
            list: false,
            execute: false,
        },
    );
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let grepped = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({"path": "/workspace/src/lib.rs", "pattern": "needle"}),
        context,
    )
    .await
    .unwrap();
    assert_eq!(grepped["files"], json!(["lib.rs"]));
}

#[tokio::test]
async fn builtin_coding_grep_applies_filters_before_loading_large_files() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/lib.rs"), "needle\n").unwrap();
    std::fs::write(
        temp.path().join("huge.txt"),
        vec![b'x'; 10 * 1024 * 1024 + 1],
    )
    .unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let grepped = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({"path": "/workspace", "pattern": "needle", "glob": "**/*.rs"}),
        context,
    )
    .await
    .unwrap();
    assert_eq!(grepped["files"], json!(["src/lib.rs"]));
}

#[tokio::test]
async fn builtin_coding_grep_multiline_reports_matched_lines_and_count() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("doc.txt"), "alpha\nbeta\ngamma\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let content = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({
            "path": "/workspace",
            "pattern": "alpha\\nbeta",
            "output_mode": "content",
            "multiline": true
        }),
        context.clone(),
    )
    .await
    .unwrap();
    let output = content["content"].as_str().unwrap();
    assert!(output.contains("doc.txt:1:alpha"));
    assert!(output.contains("doc.txt:2:beta"));

    let counts = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({
            "path": "/workspace",
            "pattern": "alpha\\nbeta",
            "output_mode": "count",
            "multiline": true
        }),
        context,
    )
    .await
    .unwrap();
    assert_eq!(counts["total"], json!(1));
    assert_eq!(counts["counts"], json!([{ "file": "doc.txt", "count": 1 }]));
}

#[tokio::test]
async fn builtin_coding_grep_respects_multiline_false_for_line_anchors() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("doc.txt"), "alpha\nbeta\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let single_line_regex = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({
            "path": "/workspace",
            "pattern": "^beta",
            "multiline": false
        }),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(single_line_regex["files"], json!([]));

    let multiline_regex = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({
            "path": "/workspace",
            "pattern": "^beta",
            "multiline": true
        }),
        context,
    )
    .await
    .unwrap();
    assert_eq!(multiline_regex["files"], json!(["doc.txt"]));
}
