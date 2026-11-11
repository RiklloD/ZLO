//! Durable-inbox listing and lifecycle mutations (`notifications` charter row).

use super::*;

pub(super) fn notification_recipient(caller: &ProductSurfaceCaller) -> NotificationRecipient {
    NotificationRecipient {
        tenant_id: caller.tenant_id.clone(),
        user_id: caller.user_id.clone(),
    }
}

pub(super) fn notification_mutation_request(
    caller: &ProductSurfaceCaller,
    request: ProductNotificationMutationRequest,
) -> Result<NotificationMutationRequest, ProductSurfaceError> {
    let notification_id = ironclaw_notifications::NotificationId::new(request.notification_id)
        .map_err(|_| {
            ProductSurfaceError::validation(
                "notification_id",
                ProductSurfaceValidationCode::InvalidId,
            )
        })?;
    Ok(NotificationMutationRequest {
        recipient: notification_recipient(caller),
        notification_id,
        occurred_at: Utc::now(),
    })
}

pub(super) fn map_notification_inbox_error(
    error: ironclaw_notifications::NotificationInboxError,
) -> ProductSurfaceError {
    match error {
        // Unlike the backend and serialization reasons below, this one is a
        // fixed literal from this crate — never backend text — so it is safe to
        // record before the boundary sanitizes the client-facing error.
        ironclaw_notifications::NotificationInboxError::InvalidRequest { reason } => {
            tracing::warn!(%reason, "notification inbox rejected a request at the product boundary");
            ProductSurfaceError::validation(
                "notification",
                ProductSurfaceValidationCode::InvalidValue,
            )
        }
        ironclaw_notifications::NotificationInboxError::NotificationNotFound => {
            ProductSurfaceError::not_found()
        }
        ironclaw_notifications::NotificationInboxError::AccessDenied => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::Forbidden, 403, false)
        }
        // The bound reason is filesystem, CAS, and serde text: it carries host
        // paths, mount internals, and payload fragments. This boundary logs the
        // fixed category only, matching how every other backend mapping here
        // discards its payload.
        ironclaw_notifications::NotificationInboxError::Backend { .. } => {
            tracing::warn!("notification inbox backend unavailable at the product boundary");
            ProductSurfaceError::service_unavailable(true)
        }
        ironclaw_notifications::NotificationInboxError::Serialization { .. } => {
            tracing::warn!("notification inbox serialization failed at the product boundary");
            ProductSurfaceError::internal()
        }
    }
}

pub(super) fn product_notification_kind(kind: NotificationKind) -> ProductNotificationKind {
    match kind {
        NotificationKind::ApprovalRequired => ProductNotificationKind::ApprovalRequired,
        NotificationKind::AuthenticationRequired => ProductNotificationKind::AuthenticationRequired,
        NotificationKind::RunBlocked => ProductNotificationKind::RunBlocked,
        NotificationKind::RunFailed => ProductNotificationKind::RunFailed,
        NotificationKind::RunCompleted => ProductNotificationKind::RunCompleted,
        NotificationKind::DeliveryFailed => ProductNotificationKind::DeliveryFailed,
    }
}

pub(super) fn product_notification_severity(
    severity: NotificationSeverity,
) -> ProductNotificationSeverity {
    match severity {
        NotificationSeverity::Info => ProductNotificationSeverity::Info,
        NotificationSeverity::Success => ProductNotificationSeverity::Success,
        NotificationSeverity::Warning => ProductNotificationSeverity::Warning,
        NotificationSeverity::Error => ProductNotificationSeverity::Error,
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub(super) async fn build_notifications_view(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductListNotificationsRequest,
        cursor: Option<String>,
    ) -> Result<ProductListNotificationsResponse, ProductSurfaceError> {
        let limit = request.limit.unwrap_or(30) as usize;
        if limit == 0 || limit > NOTIFICATION_PAGE_LIMIT_MAX {
            return Err(ProductSurfaceError::validation(
                "limit",
                ProductSurfaceValidationCode::InvalidValue,
            ));
        }
        if cursor.is_none() {
            // best-effort-ok: one-release legacy approval discovery must never
            // gate the authoritative durable Inbox read. The attempt guard
            // prevents a failing compatibility backend from being hammered on
            // every browser poll; a process restart creates a fresh retry.
            if let Err(error) = self.backfill_legacy_approval_notifications(&caller).await {
                tracing::warn!(?error, "legacy approval notification backfill failed");
            }
        }
        let page = self
            .notification_inbox
            .list(ListNotificationsRequest {
                recipient: notification_recipient(&caller),
                limit,
                cursor,
                include_archived: false,
            })
            .await
            .map_err(map_notification_inbox_error)?;
        Ok(ProductListNotificationsResponse {
            notifications: page
                .notifications
                .into_iter()
                .map(|record| ProductNotification {
                    id: record.id.as_str().to_string(),
                    kind: product_notification_kind(record.kind),
                    severity: product_notification_severity(record.severity),
                    action: match record.action {
                        NotificationAction::None => ProductNotificationAction::None,
                        NotificationAction::OpenThread { thread_id } => {
                            ProductNotificationAction::OpenThread {
                                thread_id: thread_id.to_string(),
                            }
                        }
                    },
                    thread_id: record.source.thread_id.map(|id| id.to_string()),
                    turn_run_id: record.source.turn_run_id.map(|id| id.to_string()),
                    created_at: record.created_at,
                    updated_at: record.updated_at,
                    read_at: record.read_at,
                    resolved_at: record.resolved_at,
                })
                .collect(),
            next_cursor: page.next_cursor,
            unread_count: page.unread_count,
        })
    }

    pub(super) async fn mark_notification_read(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductNotificationMutationRequest,
    ) -> Result<ProductNotificationMutationResponse, ProductSurfaceError> {
        // Terminal implementation of an authenticated ProductSurface command:
        // the recipient is always re-derived from the verified caller, never
        // accepted from the request body.
        // `updated` reports what the store actually changed. A repeated
        // mark-read succeeds without changing anything, and answering `true`
        // there would hand the client evidence of a durable write that never
        // happened.
        let outcome = self
            .notification_inbox
            .mark_read(notification_mutation_request(&caller, request)?)
            .await
            .map_err(map_notification_inbox_error)?;
        Ok(ProductNotificationMutationResponse {
            updated: outcome.applied(),
        })
    }

    pub(super) async fn mark_all_notifications_read(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<ProductNotificationMutationResponse, ProductSurfaceError> {
        let outcome = self
            .notification_inbox
            .mark_all_read(MarkAllNotificationsReadRequest {
                recipient: notification_recipient(&caller),
                occurred_at: Utc::now(),
            })
            .await
            .map_err(map_notification_inbox_error)?;
        Ok(ProductNotificationMutationResponse {
            updated: outcome.applied(),
        })
    }

    pub(super) async fn archive_notification(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductNotificationMutationRequest,
    ) -> Result<ProductNotificationMutationResponse, ProductSurfaceError> {
        let outcome = self
            .notification_inbox
            .archive(notification_mutation_request(&caller, request)?)
            .await
            .map_err(map_notification_inbox_error)?;
        Ok(ProductNotificationMutationResponse {
            updated: outcome.applied(),
        })
    }
}
