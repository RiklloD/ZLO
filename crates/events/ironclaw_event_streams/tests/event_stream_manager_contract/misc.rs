use super::*;

#[tokio::test]
async fn unsupported_view_target_returns_invalid_request() {
    let scope = projection_scope("thread-a");
    let manager = manager(scope.clone());
    let error = manager
        .subscribe(ProjectionSubscribeRequest {
            view: ProjectionViewClass::ProductMission,
            target: ProjectionTarget::Mission {
                mission_id: MissionId::new("mission-a").unwrap(),
            },
            ..subscribe_request(scope, None)
        })
        .await
        .expect_err("unsupported first-slice view");

    assert!(matches!(
        error,
        ProjectionStreamError::InvalidRequest { .. }
    ));
}

#[tokio::test]
async fn zero_subscription_buffer_capacity_is_clamped_to_one() {
    let scope = projection_scope("thread-a");
    let manager = manager(scope.clone());
    let mut subscription = manager
        .subscribe(ProjectionSubscribeRequest {
            capabilities: SubscriberCapabilities { buffer_capacity: 0 },
            ..subscribe_request(scope, None)
        })
        .await
        .expect("zero capacity clamped");

    assert!(matches!(
        subscription.next().await,
        Some(ProjectionStreamItem::Snapshot(_))
    ));
}

#[tokio::test]
async fn oversized_subscription_buffer_capacity_is_rejected() {
    let scope = projection_scope("thread-a");
    let manager = manager(scope.clone());
    let error = manager
        .subscribe(ProjectionSubscribeRequest {
            capabilities: SubscriberCapabilities {
                buffer_capacity: usize::MAX,
            },
            ..subscribe_request(scope, None)
        })
        .await
        .expect_err("oversized buffer rejected");

    assert!(matches!(
        error,
        ProjectionStreamError::InvalidRequest { .. }
    ));
}

#[test]
fn keepalive_items_do_not_advance_projection_cursor_or_carry_payload() {
    assert_eq!(keep_alive_item(), ProjectionStreamItem::KeepAlive);
}
