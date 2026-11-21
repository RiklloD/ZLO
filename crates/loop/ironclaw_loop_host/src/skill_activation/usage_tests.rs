// Caller-level coverage for skill listing management in the harness:
// drives `SelectableSkillContextSource` (the production listing builder)
// over a real usage store.

use super::*;
use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
use ironclaw_host_api::mount::{MountGrant, MountPermissions, MountView};
use ironclaw_host_api::path::{MountAlias, VirtualPath};
use ironclaw_skills::usage::{
    FilesystemSkillUsageStore, SkillPin, SkillUsageName, SkillUsageObservation, SkillUsageOwner,
    SkillUsageSource,
};

fn usage_store() -> Arc<FilesystemSkillUsageStore<InMemoryBackend>> {
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/skill-usage").unwrap(),
        VirtualPath::new("/engine/tenants/tenant-a/users/user-a/skill-usage").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();
    Arc::new(FilesystemSkillUsageStore::new(Arc::new(
        ScopedFilesystem::with_fixed_view(Arc::new(InMemoryBackend::new()), mounts),
    )))
}

fn owner() -> SkillUsageOwner {
    SkillUsageOwner {
        tenant_id: ironclaw_host_api::ids::TenantId::new("tenant-a").unwrap(),
        user_id: ironclaw_host_api::ids::UserId::new("user-a").unwrap(),
    }
}

fn name(s: &str) -> SkillUsageName {
    SkillUsageName::new(s).unwrap()
}

async fn harness_count(store: &FilesystemSkillUsageStore<InMemoryBackend>, skill: &str) -> u64 {
    store
        .load(&owner())
        .await
        .unwrap()
        .unwrap()
        .sources
        .get(&SkillUsageSource::Harness)
        .and_then(|record| record.ledger.get(&name(skill)))
        .map(|used| used.count)
        .unwrap_or(0)
}

#[tokio::test]
async fn a_cold_skill_is_listed_by_name_only_and_an_activation_is_recorded() {
    let store = usage_store();
    // code-review was used today, spreadsheet never.
    store
        .record(
            &owner(),
            &SkillUsageSource::Harness,
            SkillUsageObservation {
                catalog: None,
                used: vec![name("code-review")],
            },
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let selectable = SelectableSkillContextSource::new(two_skill_source(), listing_config())
        .with_skill_usage_store(store.clone());
    let context = run_context().await;
    selectable
        .record_user_message(
            context.scope.clone(),
            accepted_message_ref(&context),
            "hello there",
        )
        .unwrap();
    let selected = selectable
        .load_skill_context_candidates(&context)
        .await
        .unwrap();
    let listing = listing_text(&selected);
    assert!(
        listing.contains("- code-review: code-review description"),
        "a hot skill keeps its description: {listing}"
    );
    assert!(
        listing.contains("- spreadsheet") && !listing.contains("spreadsheet description"),
        "a cold skill is listed by name only: {listing}"
    );
    let document = store.load(&owner()).await.unwrap().unwrap();
    assert_eq!(
        document.catalog(&SkillUsageSource::Harness),
        [name("code-review"), name("spreadsheet")],
        "the run's visible skills are the harness catalog"
    );

    // The model loads the cold skill: that is a use.
    selectable
        .activate_skills_for_run(&context, &["spreadsheet".to_string()])
        .await
        .unwrap();
    assert_eq!(harness_count(&store, "spreadsheet").await, 1);
    assert_eq!(harness_count(&store, "code-review").await, 1);
}

#[tokio::test]
async fn an_explicit_mention_counts_and_an_off_pin_leaves_the_listing() {
    let store = usage_store();
    store
        .set_pin(&owner(), &name("code-review"), Some(SkillPin::Off))
        .await
        .unwrap();
    let selectable = SelectableSkillContextSource::new(two_skill_source(), listing_config())
        .with_skill_usage_store(store.clone());
    let context = run_context().await;
    selectable
        .record_user_message(
            context.scope.clone(),
            accepted_message_ref(&context),
            "use $spreadsheet for this",
        )
        .unwrap();
    let selected = selectable
        .load_skill_context_candidates(&context)
        .await
        .unwrap();
    assert!(
        selected
            .iter()
            .filter_map(HostSkillContextCandidate::loaded_skill_md)
            .any(|md| md.contains("SHEET_SENTINEL")),
        "an explicit mention still loads the body"
    );
    assert_eq!(harness_count(&store, "spreadsheet").await, 1);
    let listing = selected
        .iter()
        .filter_map(HostSkillContextCandidate::discoverable_metadata)
        .find(|(n, _)| *n == SKILL_LISTING_CANDIDATE_NAME)
        .map(|(_, text)| text.to_string());
    assert!(
        listing.is_none_or(|text| !text.contains("code-review")),
        "a pinned-off skill is not listed"
    );
}

#[tokio::test]
async fn without_a_store_every_skill_keeps_its_description() {
    let selectable = SelectableSkillContextSource::new(two_skill_source(), listing_config());
    let context = run_context().await;
    selectable
        .record_user_message(
            context.scope.clone(),
            accepted_message_ref(&context),
            "hello there",
        )
        .unwrap();
    let listing = listing_text(
        &selectable
            .load_skill_context_candidates(&context)
            .await
            .unwrap(),
    );
    assert!(listing.contains("- spreadsheet: spreadsheet description"));
}
