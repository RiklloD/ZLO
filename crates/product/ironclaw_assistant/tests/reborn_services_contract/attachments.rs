//! Inbound attachment landing and timeline attachment refs.

use super::*;

/// Test lander that records what it was asked to land and returns a ref per
/// attachment with a deterministic `storage_key`, so the service test can assert
/// both that decode→land ran and that the returned refs reach the transcript.
#[derive(Default)]
struct RecordingLander {
    landed: Mutex<Vec<(ThreadScope, String, Vec<InboundAttachment>)>>,
}

#[async_trait]
impl InboundAttachmentLander for RecordingLander {
    async fn land(
        &self,
        thread_scope: &ThreadScope,
        message_id: &str,
        attachments: Vec<InboundAttachment>,
    ) -> Result<Vec<AttachmentRef>, ProductSurfaceError> {
        let refs = attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| AttachmentRef {
                id: attachment.id.clone(),
                // The real bridge derives kind from the MIME type; mirror that.
                kind: ironclaw_common::kind_for_mime(&attachment.mime_type),
                mime_type: attachment.mime_type.clone(),
                filename: attachment.filename.clone(),
                size_bytes: Some(attachment.bytes.len() as u64),
                storage_key: Some(format!(
                    "/workspace/attachments/test/{message_id}-{index}-landed"
                )),
                extracted_text: None,
            })
            .collect();
        self.landed.lock().expect("lander mutex").push((
            thread_scope.clone(),
            message_id.to_string(),
            attachments,
        ));
        Ok(refs)
    }

    async fn rollback(
        &self,
        _thread_scope: &ThreadScope,
        _attachments: &[AttachmentRef],
    ) -> Result<(), ProductSurfaceError> {
        Ok(())
    }

    async fn cleanup_stale(
        &self,
        _thread_scope: &ThreadScope,
        _referenced_storage_keys: &[String],
    ) -> Result<ironclaw_attachments::AttachmentCleanupReport, ProductSurfaceError> {
        Ok(ironclaw_attachments::AttachmentCleanupReport::default())
    }
}

#[tokio::test]
async fn submit_turn_lands_attachments_and_persists_refs_on_the_user_message() {
    use base64::Engine;

    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let lander = Arc::new(RecordingLander::default());
    let services = session_services(Arc::clone(&threads), coordinator.clone())
        .with_inbound_attachments(lander.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let pdf_b64 = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.7 body");
    services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-att",
                "thread_id": "thread-alpha",
                "content": "see attached",
                "attachments": [{
                    "mime_type": "application/pdf",
                    "filename": "report.pdf",
                    "data_base64": pdf_b64,
                }],
            }))
            .expect("request"),
        )
        .await
        .expect("submit succeeds");

    // The lander was invoked with the caller-derived thread scope plus the
    // decoded attachment bytes + metadata.
    {
        let landed = lander.landed.lock().expect("lander mutex");
        assert_eq!(landed.len(), 1);
        assert_eq!(landed[0].0, thread_scope_for(&caller()));
        assert_eq!(landed[0].2.len(), 1);
        assert_eq!(landed[0].2[0].mime_type, "application/pdf");
        assert_eq!(landed[0].2[0].filename.as_deref(), Some("report.pdf"));
        assert_eq!(landed[0].2[0].bytes, b"%PDF-1.7 body");
    }

    // The returned refs are persisted on the accepted user message.
    let history = threads
        .list_thread_history(ThreadHistoryRequest {
            scope: thread_scope_for(&caller()),
            thread_id: ThreadId::new("thread-alpha").unwrap(),
        })
        .await
        .expect("history");
    let user_message = history
        .messages
        .iter()
        .find(|message| message.kind == MessageKind::User)
        .expect("user message present");
    assert_eq!(user_message.content.as_deref(), Some("see attached"));
    assert_eq!(user_message.attachments.len(), 1);
    let attachment_ref = &user_message.attachments[0];
    assert_eq!(attachment_ref.kind, AttachmentKind::Document);
    assert_eq!(attachment_ref.mime_type, "application/pdf");
    assert_eq!(attachment_ref.filename.as_deref(), Some("report.pdf"));
    assert!(
        attachment_ref
            .storage_key
            .as_deref()
            .is_some_and(|key| key.ends_with("-landed")),
        "expected landed storage_key, got {:?}",
        attachment_ref.storage_key
    );
}

#[tokio::test]
async fn get_timeline_returns_attachment_refs_on_the_user_message() {
    use base64::Engine;

    // The browser renders attachment cards from the timeline, and they must
    // survive a page refresh. The browser's surface is `get_timeline`, not
    // `list_thread_history`, so drive that path (test through the caller) and
    // assert the projected `ThreadMessageRecord` still carries the refs.
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    let lander = Arc::new(RecordingLander::default());
    let services = session_services(Arc::clone(&threads), coordinator.clone())
        .with_inbound_attachments(lander.clone());
    create_thread_for(&services, caller(), "thread-alpha").await;

    let csv_b64 = base64::engine::general_purpose::STANDARD.encode(b"a,b\n1,2\n");
    services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-att",
                "thread_id": "thread-alpha",
                "content": "spreadsheet attached",
                "attachments": [{
                    "mime_type": "text/csv",
                    "filename": "data.csv",
                    "data_base64": csv_b64,
                }],
            }))
            .expect("request"),
        )
        .await
        .expect("submit succeeds");

    let timeline = services
        .get_timeline(
            caller(),
            RebornTimelineRequest::new("thread-alpha".to_string()),
        )
        .await
        .expect("timeline");

    let user_message = timeline
        .messages
        .iter()
        .find(|message| message.kind == MessageKind::User)
        .expect("user message present in timeline");
    assert_eq!(user_message.attachments.len(), 1);
    let attachment_ref = &user_message.attachments[0];
    assert_eq!(attachment_ref.kind, AttachmentKind::Document);
    assert_eq!(attachment_ref.mime_type, "text/csv");
    assert_eq!(attachment_ref.filename.as_deref(), Some("data.csv"));
    assert!(
        attachment_ref
            .storage_key
            .as_deref()
            .is_some_and(|key| !key.is_empty()),
        "timeline ref must carry a non-empty storage_key so the agent can re-read it later"
    );
}

#[tokio::test]
async fn submit_turn_rejects_attachments_when_no_lander_is_wired() {
    use base64::Engine;

    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let coordinator = Arc::new(FakeTurnCoordinator::default());
    // No `.with_inbound_attachments(...)`: a deployment without attachment
    // support must reject rather than silently drop the files.
    let services = session_services(threads, coordinator);
    create_thread_for(&services, caller(), "thread-alpha").await;

    let pdf_b64 = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.7");
    let err = services
        .submit_turn(
            caller(),
            session_submit_request(json!({
                "client_action_id": "send-att",
                "thread_id": "thread-alpha",
                "content": "see attached",
                "attachments": [{
                    "mime_type": "application/pdf",
                    "data_base64": pdf_b64,
                }],
            }))
            .expect("request"),
        )
        .await
        .expect_err("attachments without a lander must be rejected");
    assert_eq!(err.kind, ProductSurfaceErrorKind::ServiceUnavailable);
}
