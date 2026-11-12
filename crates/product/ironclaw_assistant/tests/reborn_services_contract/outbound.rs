//! Outbound delivery-target wire shapes and the outbound product view.

use super::*;

#[derive(Default)]
struct RecordingOutboundPreferencesService {
    list_calls: Mutex<Vec<ProductSurfaceCaller>>,
}

impl RecordingOutboundPreferencesService {
    fn list_calls(&self) -> Vec<ProductSurfaceCaller> {
        self.list_calls.lock().expect("lock").clone()
    }
}

#[async_trait]
impl OutboundPreferencesProductService for RecordingOutboundPreferencesService {
    async fn list_outbound_delivery_targets(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<RebornOutboundDeliveryTargetListResponse, ProductSurfaceError> {
        self.list_calls.lock().expect("lock").push(caller);
        Ok(RebornOutboundDeliveryTargetListResponse {
            targets: vec![RebornOutboundDeliveryTargetOption {
                target: outbound_target_summary("slack-dm-alpha"),
                capabilities: RebornOutboundDeliveryTargetCapabilities {
                    final_replies: true,
                    gate_prompts: true,
                    auth_prompts: true,
                    notifications: true,
                },
            }],
            next_cursor: None,
        })
    }
}

fn outbound_target_summary(target_id: &str) -> RebornOutboundDeliveryTargetSummary {
    RebornOutboundDeliveryTargetSummary::new(
        outbound_target_id(target_id),
        "slack",
        "Slack DM",
        Some("Slack direct message".to_string()),
    )
    .expect("valid target summary")
}

fn outbound_target_id(target_id: &str) -> RebornOutboundDeliveryTargetId {
    RebornOutboundDeliveryTargetId::new(target_id).expect("valid target id")
}

#[test]
fn channel_connect_action_serializes_neutral_input_placeholder() {
    let action = RebornChannelConnectAction {
        title: "Slack channel access".to_string(),
        instructions: "Choose allowed channels.".to_string(),
        input_placeholder: "C0123456789".to_string(),
        submit_label: "Save channels".to_string(),
        success_message: "Slack channels saved.".to_string(),
        error_message: "Slack channel update failed.".to_string(),
    };

    let serialized = serde_json::to_value(&action).expect("action serializes");
    assert_eq!(serialized["input_placeholder"], "C0123456789");
}

#[test]
fn outbound_target_summary_preserves_client_json_shape() {
    let summary = outbound_target_summary("slack-dm-alpha");

    let serialized = serde_json::to_value(&summary).expect("serialize target summary");
    assert_eq!(
        serialized,
        json!({
            "target_id": "slack-dm-alpha",
            "channel": "slack",
            "display_name": "Slack DM",
            "description": "Slack direct message",
        })
    );

    let deserialized: RebornOutboundDeliveryTargetSummary =
        serde_json::from_value(serialized).expect("deserialize target summary");
    assert_eq!(deserialized.target_id.as_str(), "slack-dm-alpha");
    assert_eq!(deserialized.channel.as_str(), "slack");
    assert_eq!(deserialized.display_name.as_str(), "Slack DM");
    assert_eq!(
        deserialized
            .description
            .as_ref()
            .map(|description| description.as_str()),
        Some("Slack direct message")
    );
}

#[test]
fn outbound_target_list_response_preserves_empty_json_shape_without_cursor() {
    let response = RebornOutboundDeliveryTargetListResponse {
        targets: Vec::new(),
        next_cursor: None,
    };

    let serialized = serde_json::to_value(&response).expect("serialize empty target list");
    assert_eq!(serialized, json!({ "targets": [] }));
    assert!(
        serialized.get("next_cursor").is_none(),
        "None cursor must be omitted from the client payload"
    );

    let deserialized: RebornOutboundDeliveryTargetListResponse =
        serde_json::from_value(json!({ "targets": [] })).expect("deserialize empty target list");
    assert!(deserialized.targets.is_empty());
    assert!(deserialized.next_cursor.is_none());
}

#[test]
fn outbound_target_list_response_preserves_json_shape_with_cursor() {
    let response = RebornOutboundDeliveryTargetListResponse {
        targets: vec![RebornOutboundDeliveryTargetOption {
            target: outbound_target_summary("slack-dm-alpha"),
            capabilities: RebornOutboundDeliveryTargetCapabilities {
                final_replies: true,
                gate_prompts: true,
                auth_prompts: true,
                notifications: true,
            },
        }],
        next_cursor: Some("opaque-page-token".to_string()),
    };

    let serialized = serde_json::to_value(&response).expect("serialize target list with cursor");
    assert_eq!(
        serialized,
        json!({
            "targets": [{
                "target": {
                    "target_id": "slack-dm-alpha",
                    "channel": "slack",
                    "display_name": "Slack DM",
                    "description": "Slack direct message",
                },
                "capabilities": {
                    "final_replies": true,
                    "gate_prompts": true,
                    "auth_prompts": true,
                    "notifications": true,
                },
            }],
            "next_cursor": "opaque-page-token",
        })
    );

    let deserialized: RebornOutboundDeliveryTargetListResponse =
        serde_json::from_value(serialized).expect("deserialize target list with cursor");
    assert_eq!(deserialized, response);
}

#[test]
fn outbound_target_summary_rejects_malformed_display_fields() {
    for (field, invalid_value) in [
        ("channel", json!("")),
        ("channel", json!("slack\ninjected")),
        ("display_name", json!("")),
        ("display_name", json!("Slack DM\u{0000}")),
        ("description", json!("Slack direct\rmessage")),
    ] {
        let mut payload = json!({
            "target_id": "slack-dm-alpha",
            "channel": "slack",
            "display_name": "Slack DM",
            "description": "Slack direct message",
        });
        payload[field] = invalid_value;

        serde_json::from_value::<RebornOutboundDeliveryTargetSummary>(payload)
            .expect_err("malformed target summary display field");
    }

    for (field, invalid_value) in [
        ("channel", json!("a".repeat(129))),
        ("display_name", json!("a".repeat(257))),
        ("description", json!("a".repeat(1025))),
    ] {
        let mut payload = json!({
            "target_id": "slack-dm-alpha",
            "channel": "slack",
            "display_name": "Slack DM",
            "description": "Slack direct message",
        });
        payload[field] = invalid_value;

        serde_json::from_value::<RebornOutboundDeliveryTargetSummary>(payload)
            .expect_err("oversized target summary display field");
    }

    RebornOutboundDeliveryTargetSummary::new(
        outbound_target_id("slack-dm-alpha"),
        "slack",
        "Slack DM\ninjected",
        None,
    )
    .expect_err("constructor rejects malformed display field");
}

#[test]
fn outbound_target_display_fields_reject_whitespace_only_required_values_and_outer_whitespace() {
    for (field, invalid_value) in [
        ("channel", json!(" ")),
        ("channel", json!("\t")),
        ("display_name", json!(" ")),
        ("display_name", json!("\t")),
        ("channel", json!(" slack")),
        ("channel", json!("slack ")),
        ("display_name", json!(" Slack DM")),
        ("display_name", json!("Slack DM ")),
        ("description", json!(" Slack direct message")),
        ("description", json!("Slack direct message ")),
    ] {
        let mut payload = json!({
            "target_id": "slack-dm-alpha",
            "channel": "slack",
            "display_name": "Slack DM",
            "description": "Slack direct message",
        });
        payload[field] = invalid_value;

        serde_json::from_value::<RebornOutboundDeliveryTargetSummary>(payload)
            .expect_err("target summary display fields reject whitespace-only or padded values");
    }
}

#[test]
fn outbound_target_id_and_display_fields_reject_unicode_line_separators() {
    for target_id in [
        "slack-dm-alpha\u{2028}injected",
        "slack-dm-alpha\u{2029}injected",
    ] {
        RebornOutboundDeliveryTargetId::new(target_id)
            .expect_err("target id rejects unicode line separators");
        serde_json::from_value::<RebornSetNotificationChannelsRequest>(json!({
            "target_ids": [target_id],
        }))
        .expect_err("notification-channels request rejects target id unicode line separators");
    }

    for (field, invalid_value) in [
        ("channel", json!("slack\u{2028}injected")),
        ("channel", json!("slack\u{2029}injected")),
        ("display_name", json!("Slack DM\u{2028}injected")),
        ("display_name", json!("Slack DM\u{2029}injected")),
        ("description", json!("Slack direct\u{2028}message")),
        ("description", json!("Slack direct\u{2029}message")),
    ] {
        let mut payload = json!({
            "target_id": "slack-dm-alpha",
            "channel": "slack",
            "display_name": "Slack DM",
            "description": "Slack direct message",
        });
        payload[field] = invalid_value;

        serde_json::from_value::<RebornOutboundDeliveryTargetSummary>(payload)
            .expect_err("target summary display fields reject unicode line separators");
    }
}

#[test]
fn outbound_target_id_and_display_fields_reject_unsafe_unicode_formatting() {
    for target_id in [
        "slack-dm-alpha\u{202e}injected",
        "slack-dm-alpha\u{2066}injected",
        "slack-dm-alpha\u{200b}injected",
        "slack-dm-alpha\u{feff}injected",
    ] {
        RebornOutboundDeliveryTargetId::new(target_id)
            .expect_err("target id rejects unsafe unicode formatting characters");
        serde_json::from_value::<RebornSetNotificationChannelsRequest>(json!({
            "target_ids": [target_id],
        }))
        .expect_err("notification-channels request rejects unsafe unicode formatting characters");
    }

    for (field, invalid_value) in [
        ("channel", json!("slack\u{202e}injected")),
        ("channel", json!("slack\u{2066}injected")),
        ("channel", json!("slack\u{200b}injected")),
        ("channel", json!("slack\u{feff}injected")),
        ("display_name", json!("Slack DM\u{202e}injected")),
        ("display_name", json!("Slack DM\u{2066}injected")),
        ("display_name", json!("Slack DM\u{200b}injected")),
        ("display_name", json!("Slack DM\u{feff}injected")),
        ("description", json!("Slack direct\u{202e}message")),
        ("description", json!("Slack direct\u{2066}message")),
        ("description", json!("Slack direct\u{200b}message")),
        ("description", json!("Slack direct\u{feff}message")),
    ] {
        let mut payload = json!({
            "target_id": "slack-dm-alpha",
            "channel": "slack",
            "display_name": "Slack DM",
            "description": "Slack direct message",
        });
        payload[field] = invalid_value;

        serde_json::from_value::<RebornOutboundDeliveryTargetSummary>(payload)
            .expect_err("target summary display fields reject unsafe unicode formatting");
    }
}

#[test]
fn outbound_target_empty_description_is_accepted() {
    let description =
        RebornOutboundDeliveryTargetDescription::new("").expect("empty description is allowed");
    assert_eq!(description.as_str(), "");

    let summary = RebornOutboundDeliveryTargetSummary::new(
        outbound_target_id("slack-dm-alpha"),
        "slack",
        "Slack DM",
        Some("".to_string()),
    )
    .expect("summary accepts empty description");

    assert_eq!(
        summary
            .description
            .as_ref()
            .map(RebornOutboundDeliveryTargetDescription::as_str),
        Some("")
    );
}

#[tokio::test]
async fn outbound_unwired_target_listing_fails_closed() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let list_error = services
        .query(
            caller(),
            RebornViewQuery {
                view_id: OUTBOUND_DELIVERY_TARGETS_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect_err("unwired target listing");
    assert_eq!(list_error.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(list_error.status_code, 503);
    assert!(!list_error.retryable);
}

#[tokio::test]
async fn outbound_delivery_targets_are_available_as_a_product_view() {
    let outbound_service = Arc::new(RecordingOutboundPreferencesService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_outbound_preferences_product_service(outbound_service.clone());
    let targets_caller = caller_for_user("user-outbound-targets");

    let targets_page = services
        .query(
            targets_caller.clone(),
            RebornViewQuery {
                view_id: OUTBOUND_DELIVERY_TARGETS_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("outbound targets view");
    let targets: RebornOutboundDeliveryTargetListResponse =
        serde_json::from_value(targets_page.payload).expect("outbound targets payload");
    assert_eq!(targets.targets.len(), 1);
    assert_eq!(outbound_service.list_calls(), vec![targets_caller]);
}
