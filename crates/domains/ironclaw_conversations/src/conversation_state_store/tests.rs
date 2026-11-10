use std::sync::Arc;

use ironclaw_filesystem::{
    Fault, FaultInjecting, FaultKind, FilesystemOperation, InMemoryBackend, RootFilesystem,
    ScopedFilesystem,
};
use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_host_api::{
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, VirtualPath},
};

use super::{ConversationStateStore, RebornFilesystemConversationServices};
use crate::memory::{ActorKey, InMemoryState};
use crate::state_store::{ConversationStateRepository, PersistedConversationState};
use crate::{
    AdapterInstallationId, AdapterKind, ConversationActorPairingService, InboundTurnError,
};
use ironclaw_extension_contracts::external::ExternalActorRef;

const STALE: &str = "stale conversation state revision";

fn scoped<F>(backend: Arc<F>) -> Arc<ScopedFilesystem<F>>
where
    F: RootFilesystem,
{
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/conversations").expect("alias"),
        VirtualPath::new("/tenants/tenant-a/users/alice/conversations").expect("target"),
        MountPermissions::read_write_list_delete(),
    )])
    .expect("mount view");
    Arc::new(ScopedFilesystem::with_fixed_view(backend, mounts))
}

fn actor_key(actor: &str) -> ActorKey {
    ActorKey::new(
        &TenantId::new("tenant-a").unwrap(),
        &AdapterKind::new("telegram").unwrap(),
        &AdapterInstallationId::new("default-installation").unwrap(),
        &ExternalActorRef::new("user", actor, None::<String>).unwrap(),
    )
}

/// The loaded document with one more pairing — the shape every mutation has:
/// start from what was loaded, change it, save against what was loaded.
fn with_pairing(loaded: &PersistedConversationState, actor: &str) -> InMemoryState {
    let mut state = loaded.state.clone();
    state
        .pairings
        .insert(actor_key(actor), UserId::new(actor).unwrap());
    state
}

async fn save<F>(
    store: &ConversationStateStore<F>,
    loaded: &PersistedConversationState,
    state: &InMemoryState,
) -> Result<i64, InboundTurnError>
where
    F: RootFilesystem,
{
    store
        .save_state(loaded.revision, loaded.version, state)
        .await
        .map(|(revision, _)| revision)
}

fn assert_stale(result: Result<i64, InboundTurnError>) {
    match result {
        Err(InboundTurnError::DurableState { reason }) => assert_eq!(reason, STALE),
        other => panic!("expected a stale-writer rejection, got {other:?}"),
    }
}

async fn stale_writer_scenario<F>(
    first: ConversationStateStore<F>,
    second: ConversationStateStore<F>,
) where
    F: RootFilesystem,
{
    // Both writers start from the absent document.
    let first_loaded = first.load_state().await.unwrap();
    let second_loaded = second.load_state().await.unwrap();
    assert_eq!(
        save(&first, &first_loaded, &with_pairing(&first_loaded, "a"))
            .await
            .unwrap(),
        1
    );
    assert_stale(save(&second, &second_loaded, &with_pairing(&second_loaded, "b")).await);

    // Both writers start from the same existing document.
    let first_loaded = first.load_state().await.unwrap();
    let second_loaded = second.load_state().await.unwrap();
    assert_eq!(second_loaded.revision, 1);
    assert_eq!(
        save(&first, &first_loaded, &with_pairing(&first_loaded, "c"))
            .await
            .unwrap(),
        2
    );
    assert_stale(save(&second, &second_loaded, &with_pairing(&second_loaded, "d")).await);

    // The loser's write never landed, and after a reload it can retry.
    let reloaded = second.load_state().await.unwrap();
    assert_eq!(reloaded.revision, 2);
    assert!(reloaded.state.pairings.contains_key(&actor_key("a")));
    assert!(reloaded.state.pairings.contains_key(&actor_key("c")));
    assert!(!reloaded.state.pairings.contains_key(&actor_key("b")));
    assert!(!reloaded.state.pairings.contains_key(&actor_key("d")));
    assert_eq!(
        save(&second, &reloaded, &with_pairing(&reloaded, "d"))
            .await
            .unwrap(),
        3
    );
    let settled = first.load_state().await.unwrap();
    assert_eq!(settled.revision, 3);
    assert_eq!(settled.state.pairings.len(), 3);
}

#[tokio::test]
async fn a_stale_writer_is_rejected_and_succeeds_after_reloading() {
    let backend = Arc::new(InMemoryBackend::new());
    stale_writer_scenario(
        ConversationStateStore::new(scoped(Arc::clone(&backend))),
        ConversationStateStore::new(scoped(backend)),
    )
    .await;
}

#[tokio::test]
async fn a_backend_that_refuses_compare_and_swap_still_checks_the_revision() {
    let backend = Arc::new(FaultInjecting::new(InMemoryBackend::new()));
    let store = ConversationStateStore::new(scoped(Arc::clone(&backend)));
    let other = ConversationStateStore::new(scoped(Arc::clone(&backend)));
    let loaded = store.load_state().await.unwrap();
    save(&store, &loaded, &with_pairing(&loaded, "a"))
        .await
        .unwrap();
    let refuse_next_write = || {
        backend.add_fault(
            Fault::on(FilesystemOperation::WriteFile)
                .nth(1)
                .returning(FaultKind::Unsupported),
        )
    };

    // Up to date: the write falls back to an unconditional put and lands.
    let loaded = store.load_state().await.unwrap();
    let stale = other.load_state().await.unwrap();
    refuse_next_write();
    assert_eq!(
        save(&store, &loaded, &with_pairing(&loaded, "b"))
            .await
            .unwrap(),
        2
    );

    // Behind: the stored revision still exposes the stale writer.
    refuse_next_write();
    assert_stale(save(&other, &stale, &with_pairing(&stale, "c")).await);
    let settled = store.load_state().await.unwrap();
    assert_eq!(settled.revision, 2);
    assert!(!settled.state.pairings.contains_key(&actor_key("c")));
}

#[tokio::test]
async fn saving_on_a_cas_backend_does_not_read_the_document_back() {
    let backend = Arc::new(FaultInjecting::new(InMemoryBackend::new()));
    let store = ConversationStateStore::new(scoped(Arc::clone(&backend)));
    let loaded = store.load_state().await.unwrap();
    save(&store, &loaded, &with_pairing(&loaded, "a"))
        .await
        .unwrap();
    let loaded = store.load_state().await.unwrap();

    let reads_before = backend.count(FilesystemOperation::ReadFile);
    let writes_before = backend.count(FilesystemOperation::WriteFile);
    assert_eq!(
        save(&store, &loaded, &with_pairing(&loaded, "b"))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        backend.count(FilesystemOperation::ReadFile),
        reads_before,
        "the loaded version already proves what the writer saw"
    );
    assert_eq!(
        backend.count(FilesystemOperation::WriteFile),
        writes_before + 1
    );
}

#[tokio::test]
async fn a_lost_compare_and_swap_is_reported_as_a_stale_writer() {
    let backend = Arc::new(FaultInjecting::new(InMemoryBackend::new()));
    let store = ConversationStateStore::new(scoped(Arc::clone(&backend)));
    let loaded = store.load_state().await.unwrap();
    save(&store, &loaded, &with_pairing(&loaded, "a"))
        .await
        .unwrap();
    let loaded = store.load_state().await.unwrap();

    // Another writer lands between this writer's load and its write.
    let other = ConversationStateStore::new(scoped(Arc::clone(&backend)));
    let other_loaded = other.load_state().await.unwrap();
    save(&other, &other_loaded, &with_pairing(&other_loaded, "b"))
        .await
        .unwrap();

    assert_stale(save(&store, &loaded, &with_pairing(&loaded, "c")).await);
    let settled = store.load_state().await.unwrap();
    assert_eq!(settled.revision, 2);
    assert!(!settled.state.pairings.contains_key(&actor_key("c")));
}

#[tokio::test]
async fn a_failed_write_leaves_the_stored_document_unchanged() {
    let backend = Arc::new(FaultInjecting::new(InMemoryBackend::new()));
    let store = ConversationStateStore::new(scoped(Arc::clone(&backend)));
    let loaded = store.load_state().await.unwrap();
    save(&store, &loaded, &with_pairing(&loaded, "a"))
        .await
        .unwrap();
    let loaded = store.load_state().await.unwrap();

    backend.add_fault(
        Fault::on(FilesystemOperation::WriteFile)
            .nth(1)
            .backend("injected write failure"),
    );
    let failed = save(&store, &loaded, &with_pairing(&loaded, "b")).await;
    assert!(
        matches!(&failed, Err(InboundTurnError::DurableState { reason }) if reason != STALE),
        "a backend failure is not a stale-writer rejection: {failed:?}"
    );

    let after_failure = store.load_state().await.unwrap();
    assert_eq!(after_failure.revision, 1);
    assert!(!after_failure.state.pairings.contains_key(&actor_key("b")));
    assert_eq!(
        save(&store, &after_failure, &with_pairing(&after_failure, "b"))
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn a_revision_overflow_is_refused_before_any_write() {
    let backend = Arc::new(FaultInjecting::new(InMemoryBackend::new()));
    let store = ConversationStateStore::new(scoped(Arc::clone(&backend)));
    let mut loaded = store.load_state().await.unwrap();
    loaded.revision = i64::MAX;

    let refused = save(&store, &loaded, &with_pairing(&loaded, "a")).await;
    assert!(matches!(
        refused,
        Err(InboundTurnError::DurableState { reason })
            if reason == "conversation state revision overflow"
    ));
    assert_eq!(backend.count(FilesystemOperation::WriteFile), 0);
}

#[tokio::test]
async fn a_failed_write_through_the_service_rolls_back_and_the_retry_lands() {
    let backend = Arc::new(FaultInjecting::new(InMemoryBackend::new()));
    let services = RebornFilesystemConversationServices::new(scoped(Arc::clone(&backend)))
        .await
        .unwrap();
    let pair = |actor: &'static str| {
        ConversationActorPairingService::pair_external_actor(
            &services,
            TenantId::new("tenant-a").unwrap(),
            AdapterKind::new("telegram").unwrap(),
            AdapterInstallationId::new("default-installation").unwrap(),
            ExternalActorRef::new("user", actor, None::<String>).unwrap(),
            UserId::new(actor).unwrap(),
        )
    };
    pair("a").await.unwrap();

    backend.add_fault(
        Fault::on(FilesystemOperation::WriteFile)
            .nth(1)
            .backend("injected write failure"),
    );
    assert!(pair("b").await.is_err());
    assert!(
        !services
            .inner()
            .snapshot_for_test()
            .pairings
            .contains_key(&actor_key("b")),
        "a mutation that failed to persist is not visible in memory"
    );

    pair("b").await.unwrap();
    let store = ConversationStateStore::new(scoped(backend));
    let durable = store.load_state().await.unwrap();
    assert_eq!(durable.revision, 2);
    assert!(durable.state.pairings.contains_key(&actor_key("a")));
    assert!(durable.state.pairings.contains_key(&actor_key("b")));
}

#[tokio::test]
async fn a_second_service_instance_does_not_overwrite_the_first() {
    let backend = Arc::new(InMemoryBackend::new());
    let first = RebornFilesystemConversationServices::new(scoped(Arc::clone(&backend)))
        .await
        .unwrap();
    let second = RebornFilesystemConversationServices::new(scoped(Arc::clone(&backend)))
        .await
        .unwrap();
    for (services, actor) in [(&first, "a"), (&second, "b"), (&first, "c")] {
        ConversationActorPairingService::pair_external_actor(
            services,
            TenantId::new("tenant-a").unwrap(),
            AdapterKind::new("telegram").unwrap(),
            AdapterInstallationId::new("default-installation").unwrap(),
            ExternalActorRef::new("user", actor, None::<String>).unwrap(),
            UserId::new(actor).unwrap(),
        )
        .await
        .unwrap();
    }
    let durable = ConversationStateStore::new(scoped(backend))
        .load_state()
        .await
        .unwrap();
    assert_eq!(durable.revision, 3);
    assert_eq!(durable.state.pairings.len(), 3);
}
