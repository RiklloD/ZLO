//! Caller-scoped inbox invalidation hints (`GET /api/webchat/v2/inbox/events`).
//!
//! Emits ephemeral, payload-light [`InboxChangedEvent`] frames whenever a
//! caller's inbox list or unread notifications may have changed (runs
//! starting/blocking/ending, thread create/delete/rename/mark/settle,
//! notification inbox writes). Clients answer by re-reading `GET /inbox` and
//! unread notifications.
//!
//! Producers never block on slow consumers: the shared bus is a bounded
//! `tokio::sync::broadcast` channel, and each subscriber runs a 1-slot
//! drop-to-latest coalescer (at most one event per [`DEFAULT_COALESCE_WINDOW`]
//! per connection).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_notifications::{
    ListNotificationsRequest, MarkAllNotificationsReadRequest, NotificationInboxError,
    NotificationInboxStorePort, NotificationMutationOutcome, NotificationMutationRequest,
    NotificationPage, NotificationRecord, PublishNotificationRequest,
};
use ironclaw_orchestrator::engine::TurnCompletedListener;
use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_product_contracts::inbox::InboxChangedEvent;
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceEventSubscription, ProductSurfaceStreamResponse,
};
use ironclaw_turns::{TurnError, TurnEventKind, TurnEventSink, TurnLifecycleEvent};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

/// Broadcast ring buffer capacity for raw hints before per-subscriber
/// coalescing.
const BUS_CAPACITY: usize = 256;

/// Default minimum interval between emitted hints per connection.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(250);

/// Maximum distinct thread ids carried in one coalesced hint frame.
const MAX_HINT_THREAD_IDS: usize = 16;

#[derive(Debug, Clone)]
struct InboxHint {
    tenant_id: TenantId,
    user_id: Option<UserId>,
    thread_id: Option<String>,
}

impl InboxHint {
    fn matches(&self, caller: &ProductSurfaceCaller) -> bool {
        if self.tenant_id != caller.tenant_id {
            return false;
        }
        self.user_id
            .as_ref()
            .is_none_or(|user_id| user_id == &caller.user_id)
    }
}

/// Process-local fan-out of ephemeral inbox invalidation hints.
#[derive(Clone, Debug)]
pub struct InboxHintBus {
    sender: broadcast::Sender<InboxHint>,
    coalesce_window: Duration,
}

impl Default for InboxHintBus {
    fn default() -> Self {
        Self::new()
    }
}

impl InboxHintBus {
    pub fn new() -> Self {
        Self::with_coalesce_window(DEFAULT_COALESCE_WINDOW)
    }

    pub fn with_coalesce_window(coalesce_window: Duration) -> Self {
        let (sender, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            sender,
            coalesce_window,
        }
    }

    /// Publish an invalidation hint for `(tenant_id, user_id)`. Non-blocking:
    /// if no subscribers are listening or a subscriber lags, the producer
    /// returns immediately.
    pub fn notify(&self, tenant_id: TenantId, user_id: Option<UserId>, thread_id: Option<String>) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a hint with no active subscriber is a no-op by design"
        )]
        let _ = self.sender.send(InboxHint {
            tenant_id,
            user_id,
            thread_id,
        });
    }

    /// Publish an invalidation hint for an authenticated caller.
    pub fn notify_caller(&self, caller: &ProductSurfaceCaller, thread_id: Option<String>) {
        self.notify(
            caller.tenant_id.clone(),
            Some(caller.user_id.clone()),
            thread_id,
        );
    }

    /// Publish a hint derived from a durable [`TurnLifecycleEvent`].
    /// Heartbeats are skipped so idle running turns do not spam inbox reads.
    pub fn notify_turn_event(&self, event: &TurnLifecycleEvent) {
        if matches!(event.kind, TurnEventKind::RunnerHeartbeat) {
            return;
        }
        let user_id = event
            .owner_user_id
            .clone()
            .or_else(|| event.scope.explicit_owner_user_id().cloned());
        self.notify(
            event.scope.tenant_id.clone(),
            user_id,
            Some(event.scope.thread_id.as_str().to_string()),
        );
    }

    /// Open a caller-scoped, coalesced, drop-to-latest subscription suitable
    /// for returning from `ProductSurface::stream_events`.
    pub fn subscribe_for_caller(
        &self,
        caller: &ProductSurfaceCaller,
    ) -> ProductSurfaceEventSubscription {
        let mut rx = self.sender.subscribe();
        let caller = caller.clone();
        let coalesce_window = self.coalesce_window;
        let (tx, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let mut dirty = false;
            let mut thread_ids: Vec<String> = Vec::new();
            let mut last_sent: Option<Instant> = None;

            loop {
                let can_send_now =
                    dirty && last_sent.is_none_or(|sent_at| sent_at.elapsed() >= coalesce_window);
                let next_flush = if dirty && !can_send_now {
                    last_sent.map(|sent_at| sent_at + coalesce_window)
                } else {
                    None
                };

                tokio::select! {
                    biased;
                    _ = tx.closed() => return,
                    permit = tx.reserve(), if can_send_now => {
                        let Ok(permit) = permit else {
                            return;
                        };
                        let event = InboxChangedEvent::new(std::mem::take(&mut thread_ids));
                        dirty = false;
                        last_sent = Some(Instant::now());
                        match serde_json::to_value(&event) {
                            Ok(value) => {
                                permit.send(Ok(ProductSurfaceStreamResponse {
                                    events: vec![value],
                                    next_cursor: None,
                                    subscription: None,
                                }));
                            }
                            Err(error) => {
                                tracing::debug!(
                                    target: "ironclaw_assistant::inbox_hints",
                                    %error,
                                    "failed to serialize InboxChangedEvent",
                                );
                            }
                        }
                    }
                    _ = async {
                        if let Some(deadline) = next_flush {
                            tokio::time::sleep_until(deadline).await;
                        }
                    }, if next_flush.is_some() => {
                        // Loop around; `can_send_now` is now true.
                    }
                    received = rx.recv() => {
                        match received {
                            Ok(hint) => {
                                if hint.matches(&caller) {
                                    dirty = true;
                                    if let Some(id) = hint.thread_id
                                        && thread_ids.len() < MAX_HINT_THREAD_IDS
                                        && !thread_ids.iter().any(|existing| existing == &id)
                                    {
                                        thread_ids.push(id);
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                // Coalesce lag into a single broad hint so a
                                // slow consumer catches up on its next read.
                                dirty = true;
                            }
                            Err(broadcast::error::RecvError::Closed) => return,
                        }
                    }
                }
            }
        });
        ProductSurfaceEventSubscription::new(receiver)
    }
}

/// [`TurnEventSink`] adapter that forwards non-heartbeat lifecycle transitions
/// to an [`InboxHintBus`].
pub struct InboxHintTurnEventSink {
    bus: Arc<InboxHintBus>,
}

impl InboxHintTurnEventSink {
    pub fn new(bus: Arc<InboxHintBus>) -> Self {
        Self { bus }
    }
}

#[async_trait]
impl TurnEventSink for InboxHintTurnEventSink {
    async fn publish(&self, event: TurnLifecycleEvent) -> Result<(), TurnError> {
        self.bus.notify_turn_event(&event);
        Ok(())
    }
}

/// [`TurnCompletedListener`] adapter for the orchestrator engine so completed
/// runs (including external CLI runtimes) emit an inbox hint.
pub struct InboxHintTurnCompletedListener {
    bus: Arc<InboxHintBus>,
}

impl InboxHintTurnCompletedListener {
    pub fn new(bus: Arc<InboxHintBus>) -> Self {
        Self { bus }
    }
}

impl TurnCompletedListener for InboxHintTurnCompletedListener {
    fn turn_completed(&self, thread: &ThreadRecord) {
        self.bus.notify(
            thread.scope.tenant_id.clone(),
            Some(thread.scope.user_id.clone()),
            Some(thread.thread_id.as_str().to_string()),
        );
    }

    fn busy(&self, _thread_id: &ironclaw_host_api::ids::ThreadId) -> bool {
        false
    }
}

/// [`NotificationInboxStorePort`] decorator that emits an [`InboxHint`] after
/// every successful notification write (publish, read, read-all, resolve,
/// reopen, archive).
pub struct NotifyingNotificationInbox {
    inner: Arc<dyn NotificationInboxStorePort>,
    bus: Arc<InboxHintBus>,
}

impl NotifyingNotificationInbox {
    pub fn new(inner: Arc<dyn NotificationInboxStorePort>, bus: Arc<InboxHintBus>) -> Self {
        Self { inner, bus }
    }
}

#[async_trait]
impl NotificationInboxStorePort for NotifyingNotificationInbox {
    async fn publish(
        &self,
        request: PublishNotificationRequest,
    ) -> Result<NotificationRecord, NotificationInboxError> {
        let tenant_id = request.recipient.tenant_id.clone();
        let user_id = request.recipient.user_id.clone();
        let thread_id = request.source.thread_id.as_ref().map(|id| id.to_string());
        let record = self.inner.publish(request).await?;
        self.bus.notify(tenant_id, Some(user_id), thread_id);
        Ok(record)
    }

    async fn list(
        &self,
        request: ListNotificationsRequest,
    ) -> Result<NotificationPage, NotificationInboxError> {
        self.inner.list(request).await
    }

    async fn mark_read(
        &self,
        request: NotificationMutationRequest,
    ) -> Result<NotificationMutationOutcome, NotificationInboxError> {
        let tenant_id = request.recipient.tenant_id.clone();
        let user_id = request.recipient.user_id.clone();
        let outcome = self.inner.mark_read(request).await?;
        if outcome.applied() {
            self.bus.notify(tenant_id, Some(user_id), None);
        }
        Ok(outcome)
    }

    async fn mark_all_read(
        &self,
        request: MarkAllNotificationsReadRequest,
    ) -> Result<NotificationMutationOutcome, NotificationInboxError> {
        let tenant_id = request.recipient.tenant_id.clone();
        let user_id = request.recipient.user_id.clone();
        let outcome = self.inner.mark_all_read(request).await?;
        if outcome.applied() {
            self.bus.notify(tenant_id, Some(user_id), None);
        }
        Ok(outcome)
    }

    async fn resolve(
        &self,
        request: NotificationMutationRequest,
    ) -> Result<NotificationMutationOutcome, NotificationInboxError> {
        let tenant_id = request.recipient.tenant_id.clone();
        let user_id = request.recipient.user_id.clone();
        let outcome = self.inner.resolve(request).await?;
        if outcome.applied() {
            self.bus.notify(tenant_id, Some(user_id), None);
        }
        Ok(outcome)
    }

    async fn reopen(
        &self,
        request: NotificationMutationRequest,
    ) -> Result<NotificationMutationOutcome, NotificationInboxError> {
        let tenant_id = request.recipient.tenant_id.clone();
        let user_id = request.recipient.user_id.clone();
        let outcome = self.inner.reopen(request).await?;
        if outcome.applied() {
            self.bus.notify(tenant_id, Some(user_id), None);
        }
        Ok(outcome)
    }

    async fn archive(
        &self,
        request: NotificationMutationRequest,
    ) -> Result<NotificationMutationOutcome, NotificationInboxError> {
        let tenant_id = request.recipient.tenant_id.clone();
        let user_id = request.recipient.user_id.clone();
        let outcome = self.inner.archive(request).await?;
        if outcome.applied() {
            self.bus.notify(tenant_id, Some(user_id), None);
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
    use ironclaw_host_api::ids::ThreadId;
    use ironclaw_host_api::mount::{MountGrant, MountPermissions, MountView};
    use ironclaw_host_api::path::{MountAlias, VirtualPath};
    use ironclaw_notifications::{
        NotificationAction, NotificationId, NotificationInboxStore, NotificationInitialState,
        NotificationKind, NotificationRecipient, NotificationSeverity, NotificationSource,
    };
    use ironclaw_product_contracts::inbox::INBOX_CHANGED_EVENT_KIND;
    use ironclaw_turns::{EventCursor, TurnRunId, TurnScope, TurnStatus};

    fn caller(tenant: &str, user: &str) -> ProductSurfaceCaller {
        ProductSurfaceCaller::new(
            TenantId::new(tenant).expect("tenant"),
            UserId::new(user).expect("user"),
            None,
            None,
        )
    }

    fn decode_event(resp: ProductSurfaceStreamResponse) -> InboxChangedEvent {
        assert_eq!(resp.events.len(), 1);
        serde_json::from_value(resp.events[0].clone()).expect("InboxChangedEvent")
    }

    #[tokio::test]
    async fn scopes_hints_by_tenant_and_user() {
        let bus = InboxHintBus::with_coalesce_window(Duration::from_millis(20));
        let alice = caller("tenant-a", "alice");
        let bob = caller("tenant-a", "bob");
        let other_tenant = caller("tenant-b", "alice");

        let sub_alice = bus.subscribe_for_caller(&alice);
        let sub_bob = bus.subscribe_for_caller(&bob);
        let sub_other = bus.subscribe_for_caller(&other_tenant);

        bus.notify_caller(&alice, Some("thread-1".into()));

        let got = tokio::time::timeout(Duration::from_millis(200), sub_alice.next())
            .await
            .expect("alice receives hint")
            .expect("stream open")
            .expect("ok");
        let event = decode_event(got);
        assert_eq!(event.kind, INBOX_CHANGED_EVENT_KIND);
        assert_eq!(event.thread_ids, vec!["thread-1".to_string()]);

        assert!(
            tokio::time::timeout(Duration::from_millis(60), sub_bob.next())
                .await
                .is_err(),
            "bob must not receive alice's hint"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(60), sub_other.next())
                .await
                .is_err(),
            "other tenant must not receive tenant-a's hint"
        );
    }

    #[tokio::test]
    async fn coalesces_bursts_to_at_most_one_event_per_window() {
        let window = Duration::from_millis(80);
        let bus = InboxHintBus::with_coalesce_window(window);
        let alice = caller("tenant-a", "alice");
        let sub = bus.subscribe_for_caller(&alice);

        // First hint fires immediately.
        bus.notify_caller(&alice, Some("t0".into()));
        let first = tokio::time::timeout(Duration::from_millis(100), sub.next())
            .await
            .expect("first hint")
            .expect("open")
            .expect("ok");
        assert_eq!(decode_event(first).thread_ids, vec!["t0".to_string()]);

        // Burst within the coalesce window: merges into one trailing event.
        for i in 0..25 {
            bus.notify_caller(&alice, Some(format!("t-{}", i % 5)));
        }

        let start = Instant::now();
        let second = tokio::time::timeout(Duration::from_millis(300), sub.next())
            .await
            .expect("coalesced trailing hint")
            .expect("open")
            .expect("ok");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(40),
            "coalesced hint arrived too soon: {elapsed:?}"
        );
        let event = decode_event(second);
        assert_eq!(event.kind, INBOX_CHANGED_EVENT_KIND);
        assert_eq!(
            event.thread_ids,
            vec!["t-0", "t-1", "t-2", "t-3", "t-4"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );

        // No extra spurious events after the coalesced one.
        assert!(
            tokio::time::timeout(Duration::from_millis(120), sub.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn slow_consumer_never_blocks_producer_and_drops_to_latest() {
        let bus = InboxHintBus::with_coalesce_window(Duration::from_millis(10));
        let alice = caller("tenant-a", "alice");
        let sub = bus.subscribe_for_caller(&alice);

        // Flood far more than BUS_CAPACITY without reading `sub`.
        for i in 0..(BUS_CAPACITY * 4) {
            bus.notify_caller(&alice, Some(format!("t-{i}")));
        }
        // Yield so the subscriber task processes and fills its 1-slot channel.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // First read drains the buffered slot.
        let first = tokio::time::timeout(Duration::from_millis(200), sub.next())
            .await
            .expect("first drain")
            .expect("open")
            .expect("ok");
        assert_eq!(decode_event(first).kind, INBOX_CHANGED_EVENT_KIND);

        // A new hint after the flood still delivers cleanly.
        bus.notify_caller(&alice, Some("after-flood".into()));
        let mut saw_after = false;
        for _ in 0..2 {
            if let Ok(Some(Ok(resp))) =
                tokio::time::timeout(Duration::from_millis(200), sub.next()).await
            {
                let ev = decode_event(resp);
                if ev.thread_ids.iter().any(|id| id == "after-flood") {
                    saw_after = true;
                    break;
                }
            }
        }
        assert!(
            saw_after,
            "subscriber must recover after lag and deliver latest hint"
        );
    }

    #[tokio::test]
    async fn turn_event_sink_skips_heartbeats_and_emits_lifecycle_transitions() {
        let bus = Arc::new(InboxHintBus::with_coalesce_window(Duration::from_millis(
            10,
        )));
        let sink = InboxHintTurnEventSink::new(Arc::clone(&bus));
        let alice = caller("tenant-a", "alice");
        let sub = bus.subscribe_for_caller(&alice);

        let scope = TurnScope::new_with_owner(
            TenantId::new("tenant-a").expect("tenant"),
            None,
            None,
            ThreadId::new("thread-hb").expect("thread"),
            Some(UserId::new("alice").expect("user")),
        );
        let mk_event = |kind: TurnEventKind, status: TurnStatus| TurnLifecycleEvent {
            cursor: EventCursor(1),
            run_id: TurnRunId::new(),
            scope: scope.clone(),
            owner_user_id: Some(UserId::new("alice").expect("user")),
            kind,
            status,
            sanitized_reason: None,
            blocked_gate: None,
            retryable: None,
            detail: None,
            occurred_at: Some(Utc::now()),
        };

        sink.publish(mk_event(
            TurnEventKind::RunnerHeartbeat,
            TurnStatus::Running,
        ))
        .await
        .expect("heartbeat ok");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sub.next())
                .await
                .is_err(),
            "heartbeat must not emit an inbox hint"
        );

        sink.publish(mk_event(TurnEventKind::Submitted, TurnStatus::Queued))
            .await
            .expect("submitted ok");
        let got = tokio::time::timeout(Duration::from_millis(200), sub.next())
            .await
            .expect("submitted hint")
            .expect("open")
            .expect("ok");
        assert_eq!(decode_event(got).thread_ids, vec!["thread-hb".to_string()]);
    }

    #[tokio::test]
    async fn notifying_notification_inbox_emits_on_publish_and_mark_read() {
        let bus = Arc::new(InboxHintBus::with_coalesce_window(Duration::from_millis(
            10,
        )));
        let mounts = MountView::new(vec![MountGrant::new(
            MountAlias::new("/notifications").expect("mount alias"),
            VirtualPath::new("/engine/test/inbox-hints-notifications").expect("mount target"),
            MountPermissions::read_write_list_delete(),
        )])
        .expect("mount view");
        let scoped = Arc::new(ScopedFilesystem::with_fixed_view(
            Arc::new(InMemoryBackend::new()),
            mounts,
        ));
        let inner = Arc::new(NotificationInboxStore::new(
            scoped,
            ironclaw_notifications::NOTIFICATION_INBOX_MAX_RECORDS,
        ));
        let inbox = NotifyingNotificationInbox::new(inner, Arc::clone(&bus));

        let alice = caller("tenant-a", "alice");
        let sub = bus.subscribe_for_caller(&alice);

        let recipient = NotificationRecipient {
            tenant_id: TenantId::new("tenant-a").expect("tenant"),
            user_id: UserId::new("alice").expect("user"),
        };
        let notification_id = NotificationId::new("notif-1").expect("id");
        inbox
            .publish(PublishNotificationRequest {
                id: notification_id.clone(),
                recipient: recipient.clone(),
                kind: NotificationKind::RunCompleted,
                severity: NotificationSeverity::Info,
                source: NotificationSource {
                    thread_id: Some(ThreadId::new("t-notif").expect("thread")),
                    turn_run_id: None,
                    lifecycle_ref: None,
                    credential_providers: Vec::new(),
                },
                action: NotificationAction::OpenThread {
                    thread_id: ThreadId::new("t-notif").expect("thread"),
                },
                initial_state: NotificationInitialState::Open,
                occurred_at: Utc::now(),
            })
            .await
            .expect("publish");

        let first = tokio::time::timeout(Duration::from_millis(200), sub.next())
            .await
            .expect("publish hint")
            .expect("open")
            .expect("ok");
        assert_eq!(decode_event(first).thread_ids, vec!["t-notif".to_string()]);

        tokio::time::sleep(Duration::from_millis(20)).await;
        inbox
            .mark_read(NotificationMutationRequest {
                recipient,
                notification_id,
                occurred_at: Utc::now(),
            })
            .await
            .expect("mark_read");

        let second = tokio::time::timeout(Duration::from_millis(200), sub.next())
            .await
            .expect("mark_read hint")
            .expect("open")
            .expect("ok");
        assert_eq!(decode_event(second).kind, INBOX_CHANGED_EVENT_KIND);
    }
}
