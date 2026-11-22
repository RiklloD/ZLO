//! Built-in coding tools: batch `read_file` (`paths[]`), opt-in `LINE:HASH`
//! anchors (`hashline`), and anchored `apply_patch` edits — all driven through
//! the host runtime so mount confinement, sensitivity, read-before-edit, and
//! the null-sentinel normalization run exactly as in production.

#![deny(clippy::let_underscore_must_use)]
use ironclaw_host_api::mount::MountPermissions;
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_runtime::{APPLY_PATCH_CAPABILITY_ID, READ_FILE_CAPABILITY_ID};
use serde_json::{Value, json};

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{context::*, fixtures::*, invoke::*, runtimes::*};

/// `LINE:HASH` anchors from a hashline read's content, in line order.
fn read_anchors(read: &Value) -> Vec<String> {
    read["content"]
        .as_str()
        .expect("read content")
        .lines()
        .filter_map(|line| {
            line.split_once('│')
                .map(|(gutter, _)| gutter.trim().to_string())
        })
        .collect()
}

#[tokio::test]
async fn builtin_read_file_batch_reads_every_path_in_order_with_per_file_results() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(temp.path().join("b.txt"), "beta\nbeta two\n").unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let single = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/a.txt"}),
        context.clone(),
    )
    .await
    .unwrap();
    let batch = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"paths": ["/workspace/b.txt", "/workspace/a.txt"]}),
        context.clone(),
    )
    .await
    .unwrap();

    assert_eq!(batch["files_read"], json!(2));
    assert_eq!(batch["files_failed"], json!(0));
    let files = batch["files"].as_array().expect("files array");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], json!("/workspace/b.txt"));
    assert_eq!(files[0]["total_lines"], json!(2));
    // Each entry is exactly the single-read result shape.
    assert_eq!(files[1], single);

    // A "null" placeholder for the unused single path is treated as absent.
    let tolerated = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "null", "paths": ["/workspace/a.txt"]}),
        context,
    )
    .await
    .unwrap();
    assert_eq!(tolerated["files_read"], json!(1));
}

#[tokio::test]
async fn builtin_read_file_batch_confines_every_path_and_never_upgrades_a_denial() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("ok.txt"), "fine\n").unwrap();
    std::fs::write(temp.path().join(".env"), "TOKEN=secret\n").unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let mixed = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"paths": ["/workspace/ok.txt", "/workspace/.env", "/etc/passwd", "/workspace/missing.txt"]}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(mixed["files_read"], json!(1));
    assert_eq!(mixed["files_failed"], json!(3));
    let files = mixed["files"].as_array().expect("files array");
    assert_eq!(files[0]["content"], json!("     1│ fine"));
    for denied in &files[1..] {
        assert!(denied.get("content").is_none(), "{denied} leaked content");
        assert!(denied["error"].is_string(), "{denied} carries an error");
    }
    assert!(
        !mixed.to_string().contains("TOKEN=secret"),
        "a sensitive file must never be read by a batch"
    );

    // When every path fails, the batch fails with the first failure's kind —
    // a batch of denials is a denial, not an empty success.
    let all_denied = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"paths": ["/workspace/.env"]}),
        context.clone(),
    )
    .await
    .unwrap_err();
    let single_denied = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/.env"}),
        context,
    )
    .await
    .unwrap_err();
    assert_eq!(all_denied, single_denied);
}

#[tokio::test]
async fn builtin_read_file_batch_rejects_malformed_and_oversized_path_lists() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.txt"), "alpha\n").unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let seventeen = vec!["/workspace/a.txt"; 17];
    for input in [
        json!({"paths": []}),
        json!({"paths": seventeen}),
        json!({"paths": [1]}),
        json!({"path": "/workspace/a.txt", "paths": ["/workspace/a.txt"]}),
        json!({"paths": ["/workspace/a.txt"], "hashline": "yes"}),
    ] {
        let failure = invoke_with_context(
            &runtime,
            READ_FILE_CAPABILITY_ID,
            input.clone(),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(failure, FailureKind::InputEncode, "{input}");
    }
}

#[tokio::test]
async fn builtin_read_file_batch_shares_one_output_budget_across_files() {
    let temp = tempfile::tempdir().unwrap();
    // 16 files x ~40 KiB each: a single read would show each file whole
    // (under the 64 KiB per-read cap), but the batch must not return ~640 KiB.
    let body = (0..800)
        .map(|index| format!("line {index:04} {}", "x".repeat(40)))
        .collect::<Vec<_>>()
        .join("\n");
    let mut paths = Vec::new();
    for index in 0..16 {
        let name = format!("f{index}.txt");
        std::fs::write(temp.path().join(&name), &body).unwrap();
        paths.push(format!("/workspace/{name}"));
    }
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let batch = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"paths": paths}),
        context.clone(),
    )
    .await
    .unwrap();
    let per_file = batch["per_file_max_bytes"].as_u64().expect("budget") as usize;
    assert_eq!(per_file, 8 * 1024);
    let total: usize = batch["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| {
            assert_eq!(file["truncated"], json!(true));
            assert!(file["next_offset"].is_u64(), "truncated file can resume");
            file["content"].as_str().expect("content").len()
        })
        .sum();
    assert!(
        total <= 128 * 1024,
        "batch content {total} exceeds the shared budget"
    );

    // A budget-truncated batch read is not a full read: it must not unlock
    // edits on the file.
    let refused = invoke_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({"path": "/workspace/f0.txt", "old_string": "line 0000", "new_string": "LINE"}),
        context,
    )
    .await
    .unwrap_err();
    assert_eq!(refused, FailureKind::OperationFailed);
}

#[tokio::test]
async fn builtin_read_file_hashline_is_opt_in_and_anchors_apply_patch_edits() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("code.rs"),
        "fn a() {}\nlet x = 1;\nlet x = 1;\nfn b() {}\n",
    )
    .unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let plain = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        plain["content"],
        json!("     1│ fn a() {}\n     2│ let x = 1;\n     3│ let x = 1;\n     4│ fn b() {}"),
        "default output is unchanged when hashline is not requested"
    );

    let hashed = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs", "hashline": true}),
        context.clone(),
    )
    .await
    .unwrap();
    let anchors = read_anchors(&hashed);
    assert_eq!(anchors.len(), 4);
    assert!(
        anchors[0].starts_with("1:") && anchors[0].len() == 6,
        "{anchors:?}"
    );
    // Identical lines hash identically; the line number disambiguates them.
    assert_eq!(anchors[1][2..], anchors[2][2..]);

    // An anchored edit lands on line 3 even though its text is not unique —
    // the case an old_string edit cannot express without more context.
    let patched = invoke_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({
            "path": "/workspace/code.rs",
            "edits": [{"anchor": anchors[2], "new_string": "let y = 2;"}]
        }),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(patched["success"], json!(true));
    assert_eq!(patched["match_method"], json!("LineAnchor"));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("code.rs")).unwrap(),
        "fn a() {}\nlet x = 1;\nlet y = 2;\nfn b() {}\n"
    );

    // The same anchor is now stale (line 3 changed): rejected with fresh
    // anchors, and the file is untouched.
    let stale = invoke_failure_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({
            "path": "/workspace/code.rs",
            "edits": [{"anchor": anchors[2], "new_string": "let z = 3;"}]
        }),
        context.clone(),
    )
    .await;
    assert_eq!(stale.kind, FailureKind::OperationFailed);
    let message = stale.message.expect("stale anchor is explained");
    assert!(message.contains("is stale"), "{message}");
    let fresh = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs", "hashline": true}),
        context.clone(),
    )
    .await
    .unwrap();
    let fresh_anchors = read_anchors(&fresh);
    assert!(
        message.contains(&fresh_anchors[2]),
        "the rejection carries line 3's fresh anchor: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("code.rs")).unwrap(),
        "fn a() {}\nlet x = 1;\nlet y = 2;\nfn b() {}\n"
    );

    // A range edit with the fresh anchors deletes lines 2..=3.
    invoke_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({
            "path": "/workspace/code.rs",
            "edits": [{"anchor": fresh_anchors[1], "end_anchor": fresh_anchors[2], "new_string": ""}]
        }),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("code.rs")).unwrap(),
        "fn a() {}\nfn b() {}\n"
    );

    // The classic old_string path is unaffected.
    let classic = invoke_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs", "old_string": "fn b() {}", "new_string": "fn c() {}"}),
        context,
    )
    .await
    .unwrap();
    assert!(classic.get("match_method").is_none());
}

/// The apply_patch input schema no longer encodes its cross-field rules, so
/// every shape the retired top-level `oneOf`/`allOf` rejected must still be
/// refused at dispatch — as a correctable input error, with the file intact.
#[tokio::test]
async fn builtin_apply_patch_rejects_every_retired_schema_shape_at_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("main.rs"), "old\nother\n").unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);
    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/main.rs"}),
        context.clone(),
    )
    .await
    .unwrap();

    for input in [
        json!({"path": "/workspace/main.rs", "old_string": "null", "new_string": "new"}),
        json!({"path": "/workspace/main.rs", "old_string": "old", "new_string": "null"}),
        json!({"path": "/workspace/main.rs", "edits": [
            {"old_string": "old", "new_string": "new"},
            {"old_string": "other", "new_string": "replacement"}
        ], "replace_all": true}),
        json!({"path": "/workspace/main.rs", "old_string": "null", "new_string": null, "edits": [
            {"old_string": "old", "new_string": "new"},
            {"old_string": "other", "new_string": "replacement"}
        ], "replace_all": true}),
        json!({"path": "/workspace/main.rs", "old_string": "old", "new_string": "new",
               "edits": [{"old_string": "old", "new_string": "new"}]}),
        json!({"path": "/workspace/main.rs", "old_string": "old",
               "edits": [{"old_string": "old", "new_string": "new"}]}),
    ] {
        let failure = invoke_failure_with_context(
            &runtime,
            APPLY_PATCH_CAPABILITY_ID,
            input.clone(),
            context.clone(),
        )
        .await;
        assert_eq!(failure.kind, FailureKind::InputEncode, "{input}");
        assert!(
            failure
                .message
                .as_deref()
                .is_some_and(|message| message.starts_with("apply_patch input rejected")),
            "{input} names the violated rule: {:?}",
            failure.message
        );
    }
    assert_eq!(
        std::fs::read_to_string(temp.path().join("main.rs")).unwrap(),
        "old\nother\n"
    );
}
