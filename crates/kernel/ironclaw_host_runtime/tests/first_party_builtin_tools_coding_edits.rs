//! Built-in coding tools: permissions, listing limits, apply_patch, and write_file guards.

use ironclaw_host_api::mount::MountPermissions;
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_runtime::{
    APPLY_PATCH_CAPABILITY_ID, GLOB_CAPABILITY_ID, GREP_CAPABILITY_ID, LIST_DIR_CAPABILITY_ID,
    READ_FILE_CAPABILITY_ID, WRITE_FILE_CAPABILITY_ID,
};
use serde_json::json;

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{context::*, fixtures::*, invoke::*, runtimes::*};

#[tokio::test]
async fn builtin_coding_write_allows_v1_sized_payloads_past_default_input_cap() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);
    let content = "x".repeat(2 * 1024 * 1024);

    let wrote = invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/large.txt", "content": content}),
        context,
    )
    .await
    .unwrap();
    assert_eq!(wrote["bytes_written"], json!(2 * 1024 * 1024));
}

#[cfg(unix)]
#[tokio::test]
async fn builtin_coding_grep_skips_sensitive_files_and_symlink_targets() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join(".env"), "TOKEN=secret\n").unwrap();
    std::os::unix::fs::symlink(temp.path().join(".env"), temp.path().join("safe_link.rs")).unwrap();
    std::fs::write(temp.path().join("visible.rs"), "TOKEN=visible\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let grepped = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({"path": "/workspace", "pattern": "TOKEN", "output_mode": "content"}),
        context.clone(),
    )
    .await
    .unwrap();
    let content = grepped["content"].as_str().unwrap();
    assert!(content.contains("visible.rs"));
    assert!(!content.contains("secret"));
    assert!(!content.contains("safe_link"));

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
            .all(|entry| !entry.as_str().unwrap_or_default().contains(".env"))
    );
    assert!(
        entries
            .iter()
            .all(|entry| !entry.as_str().unwrap_or_default().contains("safe_link"))
    );
}

#[tokio::test]
async fn builtin_coding_grep_requires_read_permission_but_glob_only_requires_list() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(
        temp.path().join("src/secret.rs"),
        "let token = \"secret\";\n",
    )
    .unwrap();

    let (filesystem, mounts) = mounted_filesystem(
        temp.path(),
        MountPermissions {
            read: false,
            write: false,
            delete: false,
            list: true,
            execute: false,
        },
    );
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let globbed = invoke_with_context(
        &runtime,
        GLOB_CAPABILITY_ID,
        json!({"path": "/workspace", "pattern": "**/*.rs"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(globbed["files"], json!(["src/secret.rs"]));

    let error = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({"path": "/workspace", "pattern": "secret"}),
        context,
    )
    .await
    .unwrap_err();
    assert_eq!(error, FailureKind::FilesystemDenied);
}

#[tokio::test]
async fn builtin_coding_grep_denies_directory_without_list_grant() {
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

    let error = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({"path": "/workspace", "pattern": "needle"}),
        context,
    )
    .await
    .unwrap_err();
    assert_eq!(error, FailureKind::FilesystemDenied);
}

#[tokio::test]
async fn builtin_coding_list_and_glob_require_list_permission() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("README.md"), "alpha\n").unwrap();

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

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/README.md"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(read["content"], json!("     1│ alpha"));

    for capability in [LIST_DIR_CAPABILITY_ID, GLOB_CAPABILITY_ID] {
        let error = invoke_with_context(
            &runtime,
            capability,
            json!({"path": "/workspace", "pattern": "**/*.md"}),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, FailureKind::FilesystemDenied);
    }
}

#[tokio::test]
async fn builtin_coding_list_truncates_like_v1_after_500_entries() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..501 {
        std::fs::write(temp.path().join(format!("file-{index:03}.txt")), "a").unwrap();
    }

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let listed = invoke_with_context(
        &runtime,
        LIST_DIR_CAPABILITY_ID,
        json!({"path": "/workspace"}),
        execution_context_with_mounts(all_builtin_capability_ids(), mounts),
    )
    .await
    .unwrap();

    assert_eq!(listed["count"], json!(500));
    assert_eq!(listed["truncated"], json!(true));
    assert_eq!(listed["entries"].as_array().unwrap().len(), 500);
}

#[tokio::test]
async fn builtin_coding_read_rejects_files_larger_than_v1_limit_before_loading_contents() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("big.txt"),
        vec![b'x'; 10 * 1024 * 1024 + 1],
    )
    .unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let error = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/big.txt"}),
        execution_context_with_mounts(all_builtin_capability_ids(), mounts),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::Resource);
}

#[tokio::test]
async fn builtin_coding_read_rejects_non_file_paths() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let error = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/src"}),
        execution_context_with_mounts(all_builtin_capability_ids(), mounts),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::Resource);
}

#[tokio::test]
async fn builtin_apply_patch_matches_exact_unique_and_replace_all_behavior() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("code.rs"), "old\nold\nunique\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs"}),
        context.clone(),
    )
    .await
    .unwrap();

    let duplicate = invoke_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs", "old_string": "old", "new_string": "new"}),
        context.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(duplicate, FailureKind::OperationFailed);

    let replaced = invoke_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({
            "path": "/workspace/code.rs",
            "old_string": "old",
            "new_string": "new",
            "replace_all": true
        }),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(replaced["success"], json!(true));
    assert_eq!(replaced["replacements"], json!(2));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("code.rs")).unwrap(),
        "new\nnew\nunique\n"
    );

    let no_op = invoke_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs", "old_string": "new", "new_string": "new"}),
        context,
    )
    .await
    .unwrap_err();
    assert_eq!(no_op, FailureKind::InputEncode);
}

#[tokio::test]
async fn builtin_apply_patch_requires_prior_read_of_existing_file() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("code.rs"), "old\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    // Read-before-edit guard: an unread file cannot be patched, and the
    // rejection tells the model how to recover.
    let failure = invoke_failure_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs", "old_string": "old", "new_string": "new"}),
        context.clone(),
    )
    .await;
    assert_eq!(failure.kind, FailureKind::OperationFailed);
    assert_eq!(
        failure.message.as_deref(),
        Some(
            "apply_patch failed for path workspace code.rs: read it in full with read_file before \
             editing it. Ranged reads (offset or limit) and default reads truncated at the line \
             or byte cap do not count as having seen the whole file; a file too large to read in \
             full cannot be edited with this tool"
        )
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("code.rs")).unwrap(),
        "old\n"
    );

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs"}),
        context.clone(),
    )
    .await
    .unwrap();

    let patched = invoke_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs", "old_string": "old", "new_string": "new"}),
        context,
    )
    .await
    .unwrap();

    assert_eq!(patched["success"], json!(true));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("code.rs")).unwrap(),
        "new\n"
    );
}

#[tokio::test]
async fn builtin_apply_patch_accepts_file_content_written_by_same_scope() {
    let temp = tempfile::tempdir().unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/math_utils.py", "content": "def multiply(a, b):\n    return a + b\n"}),
        context.clone(),
    )
    .await
    .unwrap();

    let patched = invoke_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({
            "path": "/workspace/math_utils.py",
            "old_string": "    return a + b",
            "new_string": "    return a * b"
        }),
        context,
    )
    .await
    .unwrap();

    assert_eq!(patched["success"], json!(true));
    assert_eq!(patched["replacements"], json!(1));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("math_utils.py")).unwrap(),
        "def multiply(a, b):\n    return a * b\n"
    );
}

#[tokio::test]
async fn builtin_write_file_rejects_docx_after_extracted_read_without_changing_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let original = skill_bundle_zip(&[(
        "word/document.xml",
        br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Original text</w:t></w:r></w:p></w:body></w:document>"#,
    )]);
    let document_path = temp.path().join("review.docx");
    std::fs::write(&document_path, &original).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/review.docx"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert!(read["content"].as_str().unwrap().contains("Original text"));

    let failure = invoke_failure_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/review.docx", "content": "Replacement text"}),
        context,
    )
    .await;
    assert_eq!(failure.kind, FailureKind::OperationFailed);
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("binary documents cannot be edited with text tools")
    );
    assert_eq!(std::fs::read(document_path).unwrap(), original);
}

#[tokio::test]
async fn builtin_write_file_rejects_existing_pdf_overwrite_without_changing_bytes() {
    let temp = tempfile::tempdir().unwrap();
    // A minimal real PDF header so extraction/decode paths treat it as binary.
    let original = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n";
    let document_path = temp.path().join("report.pdf");
    std::fs::write(&document_path, original).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let failure = invoke_failure_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/report.pdf", "content": "Replacement text"}),
        context,
    )
    .await;
    assert_eq!(failure.kind, FailureKind::OperationFailed);
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("binary documents cannot be edited with text tools")
    );
    assert_eq!(std::fs::read(document_path).unwrap(), original.to_vec());
}

#[tokio::test]
async fn builtin_write_file_allows_new_pdf_creation() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let output = invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/fresh.pdf", "content": "%PDF-1.4 new file\n"}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(output["success"].as_bool(), Some(true));
    assert_eq!(
        std::fs::read(temp.path().join("fresh.pdf")).unwrap(),
        b"%PDF-1.4 new file\n".to_vec()
    );
}

#[tokio::test]
async fn builtin_apply_patch_rejects_when_old_string_is_no_longer_present() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("code.rs"), "old\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    std::fs::write(temp.path().join("code.rs"), "changed\n").unwrap();

    // Read the current content so the failure exercises the match path, not
    // the read-before-edit guard.
    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs"}),
        context.clone(),
    )
    .await
    .unwrap();

    let missing = invoke_failure_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({"path": "/workspace/code.rs", "old_string": "old", "new_string": "new"}),
        context,
    )
    .await;
    assert_eq!(missing.kind, FailureKind::OperationFailed);
    assert_eq!(
        missing.message.as_deref(),
        Some("apply_patch failed for path workspace code.rs: old_string matched 0 times")
    );
}

#[tokio::test]
async fn builtin_coding_write_is_denied_by_read_only_mount() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("README.md"), "alpha\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let error = invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/blocked.txt", "content": "nope"}),
        execution_context_with_mounts(all_builtin_capability_ids(), mounts),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::FilesystemDenied);
    assert!(!temp.path().join("blocked.txt").exists());
}

#[tokio::test]
async fn builtin_write_file_rejects_extracted_read_representation_at_unlisted_extension() {
    // The extension guard names the OOXML/PDF containers, but `read_file` also
    // extracts .rtf — which the guard deliberately does not list. This is the
    // only live surface of the second defense (the recorded READ REPRESENTATION),
    // and it is what stops an extracted-text read from authorizing a raw
    // overwrite for any format the extension list does not enumerate.
    let temp = tempfile::tempdir().unwrap();
    let original = br"{\rtf1\ansi Original clause text.\par}".to_vec();
    let document_path = temp.path().join("contract.rtf");
    std::fs::write(&document_path, &original).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/contract.rtf"}),
        context.clone(),
    )
    .await
    .expect("rtf reads as extracted text");

    let failure = invoke_failure_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/contract.rtf", "content": "Revised clause text."}),
        context,
    )
    .await;
    assert_eq!(failure.kind, FailureKind::OperationFailed);
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("binary documents cannot be edited with text tools")
    );
    assert_eq!(std::fs::read(document_path).unwrap(), original);
}

#[tokio::test]
async fn builtin_write_file_rejects_macro_enabled_ooxml_formats() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    for extension in ["docm", "xlsm", "pptm"] {
        let path = format!("/workspace/macro.{extension}");
        let failure = invoke_failure_with_context(
            &runtime,
            WRITE_FILE_CAPABILITY_ID,
            json!({"path": path, "content": "not office bytes"}),
            context.clone(),
        )
        .await;
        assert_eq!(failure.kind, FailureKind::OperationFailed);
        assert!(
            failure
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("binary documents cannot be edited with text tools"),
            "{extension}: {:?}",
            failure.message
        );
        assert!(!temp.path().join(format!("macro.{extension}")).exists());
    }
}

#[tokio::test]
async fn builtin_write_file_still_overwrites_text_log_with_stray_nul() {
    // The binary backstop on the write path must use the SAME leniency as the
    // read path (`read_file_tolerates_stray_nul_and_invalid_utf8_in_text_logs`).
    // A strict any-NUL probe here makes a syslog the model just read
    // unwritable — and reports it as a "binary document", which it is not.
    let temp = tempfile::tempdir().unwrap();
    let mut original = b"Jan  1 00:00:00 host sshd[1]: Failed password for root\n".to_vec();
    original.push(0u8); // one stray NUL: tolerated on read, must stay writable
    original.extend_from_slice(b"Jan  1 00:00:01 host sshd[1]: more log line\n");
    let log_path = temp.path().join("syslog.log");
    std::fs::write(&log_path, &original).unwrap();

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
    .expect("text log with a stray NUL must read");

    invoke_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/syslog.log", "content": "redacted log\n"}),
        context,
    )
    .await
    .expect("a text log the model could read must stay writable");
    assert_eq!(std::fs::read(&log_path).unwrap(), b"redacted log\n");
}

#[tokio::test]
async fn builtin_apply_patch_rejects_a_binary_document_with_an_actionable_reason() {
    // #6898 named this as the sibling defect: apply_patch already refused a
    // docx, but via the bare binary probe, so the model saw an opaque failure
    // and could not tell why. It must now fail for the SAME stated reason
    // write_file does, and leave the bytes alone.
    let temp = tempfile::tempdir().unwrap();
    let original = skill_bundle_zip(&[(
        "word/document.xml",
        br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Original text</w:t></w:r></w:p></w:body></w:document>"#,
    )]);
    let document_path = temp.path().join("review.docx");
    std::fs::write(&document_path, &original).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/review.docx"}),
        context.clone(),
    )
    .await
    .unwrap();

    let failure = invoke_failure_with_context(
        &runtime,
        APPLY_PATCH_CAPABILITY_ID,
        json!({
            "path": "/workspace/review.docx",
            "old_string": "Original text",
            "new_string": "Revised text",
        }),
        context,
    )
    .await;
    assert_eq!(failure.kind, FailureKind::OperationFailed);
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("binary documents cannot be edited with text tools"),
        "apply_patch must explain itself like write_file does, got {:?}",
        failure.message
    );
    assert_eq!(std::fs::read(document_path).unwrap(), original);
}
