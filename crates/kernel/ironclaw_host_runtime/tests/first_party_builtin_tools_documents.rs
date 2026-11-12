//! Built-in document capabilities (#6898 item 3): structured reads, document_edit, html_to_pdf.

use ironclaw_host_api::mount::MountPermissions;
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_runtime::{
    DOCUMENT_EDIT_CAPABILITY_ID, HTML_TO_PDF_CAPABILITY_ID, READ_FILE_CAPABILITY_ID,
    WRITE_FILE_CAPABILITY_ID,
};
use serde_json::json;

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{context::*, fixtures::*, invoke::*, runtimes::*};

// --- document capabilities (#6898 item 3) ----------------------------------

/// A `.docx` carrying a real redline: "sixty" struck out, "thirty" proposed.
fn redlined_contract_docx() -> Vec<u8> {
    let body = concat!(
        r#"<w:p><w:r><w:t>Master Services Agreement</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t xml:space="preserve">Clause 4: the review period is </w:t></w:r>"#,
        r#"<w:del w:id="1" w:author="Reviewer"><w:r><w:delText>sixty</w:delText></w:r></w:del>"#,
        r#"<w:ins w:id="2" w:author="Reviewer"><w:r><w:t>thirty</w:t></w:r></w:ins>"#,
        r#"<w:r><w:t xml:space="preserve"> days.</w:t></w:r></w:p>"#,
    );
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    );
    skill_bundle_zip(&[
        ("[Content_Types].xml", br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#),
        ("word/styles.xml", br#"<?xml version="1.0"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#),
        ("word/document.xml", document.as_bytes()),
    ])
}

#[tokio::test]
async fn read_file_surfaces_docx_tracked_changes_instead_of_flattened_text() {
    // The read half of #6898 item 3, folded into read_file. Flat extraction
    // showed the struck-out "sixty" as if it were still in the contract; the
    // structured view separates it as a Deleted revision and keeps it out of
    // the paragraph's final text.
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("contract.docx"), redlined_contract_docx()).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let read = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/contract.docx"}),
        context,
    )
    .await
    .unwrap();
    let content = read["content"].as_str().unwrap();
    assert!(content.contains("\"format\": \"docx\""), "{content}");
    assert!(content.contains("\"kind\": \"deleted\""), "{content}");
    assert!(content.contains("\"author\": \"Reviewer\""), "{content}");
    // Paragraph ids are what document_edit addresses, so the read must expose them.
    assert!(content.contains("\"id\": \"p2\""), "{content}");
}

#[tokio::test]
async fn document_edit_accepts_redlines_into_a_new_file_leaving_the_original_intact() {
    // The user-facing journey: a redlined contract in, a clean contract out,
    // and the original still exactly as it was uploaded.
    let temp = tempfile::tempdir().unwrap();
    let original = redlined_contract_docx();
    let source = temp.path().join("contract.docx");
    std::fs::write(&source, &original).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/contract.docx"}),
        context.clone(),
    )
    .await
    .unwrap();

    let edited = invoke_with_context(
        &runtime,
        DOCUMENT_EDIT_CAPABILITY_ID,
        json!({
            "path": "/workspace/contract.docx",
            "output_path": "/workspace/contract-final.docx",
            "edits": [{"op": "resolve_all_revisions", "disposition": "accept"}],
        }),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(edited["success"], json!(true));

    assert_eq!(
        std::fs::read(&source).unwrap(),
        original,
        "document_edit must never modify its source"
    );

    let reread = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/contract-final.docx"}),
        context,
    )
    .await
    .unwrap();
    let content = reread["content"].as_str().unwrap();
    assert!(
        content.contains("thirty"),
        "accepted insertion survives: {content}"
    );
    assert!(
        !content.contains("sixty"),
        "accepted deletion is gone: {content}"
    );
    assert!(
        !content.contains("\"kind\":"),
        "the result must carry no unresolved revisions: {content}"
    );
}

#[tokio::test]
async fn an_edited_document_requires_a_fresh_structured_read_before_another_edit() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("contract.docx"), redlined_contract_docx()).unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/contract.docx"}),
        context.clone(),
    )
    .await
    .unwrap();
    invoke_with_context(
        &runtime,
        DOCUMENT_EDIT_CAPABILITY_ID,
        json!({
            "path": "/workspace/contract.docx",
            "output_path": "/workspace/first.docx",
            "edits": [{"op": "resolve_all_revisions", "disposition": "accept"}],
        }),
        context.clone(),
    )
    .await
    .unwrap();

    let failure = invoke_failure_with_context(
        &runtime,
        DOCUMENT_EDIT_CAPABILITY_ID,
        json!({
            "path": "/workspace/first.docx",
            "output_path": "/workspace/second.docx",
            "edits": [{"op": "replace_paragraph_text", "paragraph": "p1", "text": "Changed"}],
        }),
        context,
    )
    .await;
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("read it in full with read_file"),
        "{:?}",
        failure.message
    );
}

#[tokio::test]
async fn oversized_structured_views_fail_instead_of_authorizing_partial_addresses() {
    let temp = tempfile::tempdir().unwrap();
    let paragraphs = (0..1200)
        .map(|index| {
            format!(
                r#"<w:p><w:r><w:t>paragraph {index} {}</w:t></w:r></w:p>"#,
                "x".repeat(60)
            )
        })
        .collect::<String>();
    let document = format!(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{paragraphs}</w:body></w:document>"#
    );
    let docx = skill_bundle_zip(&[("word/document.xml", document.as_bytes())]);
    std::fs::write(temp.path().join("large.docx"), docx).unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let failure = invoke_failure_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/large.docx"}),
        context.clone(),
    )
    .await;
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("structured document view exceeds the response limit"),
        "{:?}",
        failure.message
    );
    let edit_failure = invoke_failure_with_context(
        &runtime,
        DOCUMENT_EDIT_CAPABILITY_ID,
        json!({
            "path": "/workspace/large.docx",
            "output_path": "/workspace/out.docx",
            "edits": [{"op": "replace_paragraph_text", "paragraph": "p1", "text": "Changed"}],
        }),
        context,
    )
    .await;
    assert!(
        edit_failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("read it in full with read_file")
    );
}

#[tokio::test]
async fn document_edit_requires_a_prior_structured_read() {
    // Same read-before-edit guarantee write_file has, on the representation
    // whose addresses the edits name.
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("contract.docx"), redlined_contract_docx()).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let failure = invoke_failure_with_context(
        &runtime,
        DOCUMENT_EDIT_CAPABILITY_ID,
        json!({
            "path": "/workspace/contract.docx",
            "output_path": "/workspace/out.docx",
            "edits": [{"op": "resolve_all_revisions", "disposition": "accept"}],
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
            .contains("read it in full with read_file"),
        "got {:?}",
        failure.message
    );
}

#[tokio::test]
async fn document_edit_refuses_to_write_over_its_own_source() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("contract.docx"), redlined_contract_docx()).unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let failure = invoke_failure_with_context(
        &runtime,
        DOCUMENT_EDIT_CAPABILITY_ID,
        json!({
            "path": "/workspace/contract.docx",
            "output_path": "/workspace/contract.docx",
            "edits": [{"op": "resolve_all_revisions", "disposition": "accept"}],
        }),
        context,
    )
    .await;
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("must differ from path"),
        "got {:?}",
        failure.message
    );
}

#[tokio::test]
async fn document_edit_refuses_to_overwrite_an_existing_output() {
    let temp = tempfile::tempdir().unwrap();
    let source = redlined_contract_docx();
    let existing = b"existing output must survive";
    std::fs::write(temp.path().join("contract.docx"), source).unwrap();
    std::fs::write(temp.path().join("contract-final.docx"), existing).unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/contract.docx"}),
        context.clone(),
    )
    .await
    .unwrap();

    let failure = invoke_failure_with_context(
        &runtime,
        DOCUMENT_EDIT_CAPABILITY_ID,
        json!({
            "path": "/workspace/contract.docx",
            "output_path": "/workspace/contract-final.docx",
            "edits": [{"op": "resolve_all_revisions", "disposition": "accept"}],
        }),
        context,
    )
    .await;
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("will not overwrite"),
        "got {:?}",
        failure.message
    );
    assert_eq!(
        std::fs::read(temp.path().join("contract-final.docx")).unwrap(),
        existing,
        "the refused edit must leave the existing output untouched"
    );
}

#[tokio::test]
async fn html_to_pdf_writes_a_pdf_and_refuses_to_overwrite_an_existing_one() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    let rendered = invoke_with_context(
        &runtime,
        HTML_TO_PDF_CAPABILITY_ID,
        json!({
            "path": "/workspace/report.pdf",
            "html": "<h1>Quarterly Report</h1><p>Revenue up <strong>12%</strong>.</p>",
            "title": "Quarterly Report",
        }),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(rendered["success"], json!(true));
    let bytes = std::fs::read(temp.path().join("report.pdf")).unwrap();
    assert!(bytes.starts_with(b"%PDF-"), "must be a real PDF");

    // A rendered PDF is derived; silently replacing one the user uploaded is
    // the same class of loss the binary-write guard prevents.
    let failure = invoke_failure_with_context(
        &runtime,
        HTML_TO_PDF_CAPABILITY_ID,
        json!({"path": "/workspace/report.pdf", "html": "<p>again</p>"}),
        context,
    )
    .await;
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("will not overwrite"),
        "got {:?}",
        failure.message
    );
    assert_eq!(
        std::fs::read(temp.path().join("report.pdf")).unwrap(),
        bytes,
        "the refused render must leave the existing file untouched"
    );
}

#[tokio::test]
async fn html_to_pdf_rejects_unbounded_html_before_rendering() {
    let temp = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);
    let failure = invoke_failure_with_context(
        &runtime,
        HTML_TO_PDF_CAPABILITY_ID,
        json!({"path": "/workspace/large.pdf", "html": "x".repeat(1024 * 1024 + 1)}),
        context,
    )
    .await;
    assert_eq!(failure.kind, FailureKind::Resource);
    assert!(!temp.path().join("large.pdf").exists());
}

#[tokio::test]
async fn a_structured_read_still_does_not_authorize_a_raw_write_file_overwrite() {
    // The #6898 guard and the new structured path must not cancel each other:
    // reading a docx structurally gives document_edit authority, never
    // write_file authority.
    let temp = tempfile::tempdir().unwrap();
    let original = redlined_contract_docx();
    let path = temp.path().join("contract.docx");
    std::fs::write(&path, &original).unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(all_builtin_capability_ids(), mounts);

    invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/contract.docx"}),
        context.clone(),
    )
    .await
    .unwrap();

    let failure = invoke_failure_with_context(
        &runtime,
        WRITE_FILE_CAPABILITY_ID,
        json!({"path": "/workspace/contract.docx", "content": "Clause 4: thirty days."}),
        context,
    )
    .await;
    assert!(
        failure
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("binary documents cannot be edited with text tools"),
        "got {:?}",
        failure.message
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
}
