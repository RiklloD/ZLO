use std::sync::Arc;

use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
use ironclaw_host_api::{
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, VirtualPath},
};

use super::*;

fn root() -> Arc<InMemoryBackend> {
    Arc::new(InMemoryBackend::default())
}

fn open(root: &Arc<InMemoryBackend>) -> FilesystemEnvironmentAccess<InMemoryBackend> {
    let view = MountView::new(vec![MountGrant::new(
        MountAlias::new("/tenant-shared").expect("alias"),
        VirtualPath::new("/tenants/test/shared").expect("path"),
        MountPermissions::read_write_list_delete(),
    )])
    .expect("view");
    FilesystemEnvironmentAccess::new(
        Arc::new(ScopedFilesystem::with_fixed_view(Arc::clone(root), view)),
        TenantId::new("tenant-a").expect("tenant"),
        UserId::new("owner").expect("user"),
        AgentId::new("agent").expect("agent"),
        None,
    )
}

async fn ready(root: &Arc<InMemoryBackend>) -> FilesystemEnvironmentAccess<InMemoryBackend> {
    let access = open(root);
    access.ensure_environment("Studio").await.expect("ensure");
    access
}

fn client(label: &str) -> PairClient {
    PairClient {
        label: label.to_string(),
        os: "macOS".to_string(),
        app_version: "0.1.0".to_string(),
    }
}

#[tokio::test]
async fn environment_id_is_created_once_and_survives_reopen() {
    let root = root();
    let first = open(&root)
        .ensure_environment("Studio")
        .await
        .expect("first");
    let again = open(&root)
        .ensure_environment("Other name")
        .await
        .expect("again");
    assert_eq!(first, again, "id and label are fixed after the first start");
    assert_eq!(first.label, "Studio");
    assert_eq!(
        open(&root).environment().await.expect("read"),
        first,
        "a reopened store reads the same identity"
    );
}

#[tokio::test]
async fn issued_code_is_crockford_and_stored_only_as_a_hash() {
    let root = root();
    let access = ready(&root).await;
    let issued = access.issue_pairing_code().await.expect("issue");
    let canonical = normalize_pairing_code(&issued.code).expect("well-formed");
    assert_eq!(issued.code, display_pairing_code(&canonical));
    assert!(issued.expires_at > Utc::now() + Duration::minutes(4));

    let raw_path =
        scoped_path(format!("{ENVIRONMENT_ROOT}/pairing-codes/{canonical}.json")).expect("path");
    assert!(
        access
            .read_record::<StoredPairingCode>(&raw_path)
            .await
            .expect("read")
            .is_none(),
        "the plain code must never be a storage key"
    );
    let hashed = pairing_code_path(&canonical).expect("path");
    let versioned = access
        .filesystem
        .get(&access.scope, &hashed)
        .await
        .expect("get")
        .expect("stored under its hash");
    let body = String::from_utf8(versioned.entry.body.to_vec()).expect("utf8");
    assert!(!body.contains(&canonical), "{body}");
}

#[tokio::test]
async fn redeem_succeeds_once_then_every_miss_is_invalid_code() {
    let root = root();
    let access = ready(&root).await;
    let issued = access.issue_pairing_code().await.expect("issue");
    let lowercase_no_dashes = issued.code.replace('-', "").to_ascii_lowercase();
    let paired = access
        .redeem_pairing_code(&lowercase_no_dashes, client("Air"))
        .await
        .expect("redeem");
    let environment = access.environment().await.expect("env");
    assert_eq!(paired.environment_id, environment.environment_id.as_str());
    assert!(
        paired
            .token
            .starts_with(&format!("icd_{}_", paired.device_id))
    );
    // base64url may itself contain `_`, so strip the prefix rather than split.
    let secret = paired
        .token
        .strip_prefix(&format!("icd_{}_", paired.device_id))
        .expect("secret");
    assert_eq!(secret.len(), 43, "32 bytes, base64url, no padding");

    for attempt in [issued.code.as_str(), "ZZZZ-ZZZZ-ZZZZ", "not a code", ""] {
        assert_eq!(
            access.redeem_pairing_code(attempt, client("Air")).await,
            Err(EnvironmentAccessError::InvalidCode),
            "{attempt}"
        );
    }
}

#[tokio::test]
async fn expired_code_is_invalid_code() {
    let root = root();
    let access = ready(&root).await;
    let canonical = mint_pairing_code();
    let past = Utc::now() - Duration::minutes(10);
    access
        .put_record(
            &pairing_code_path(&canonical).expect("path"),
            &StoredPairingCode {
                created_at: past,
                expires_at: past + PAIRING_CODE_TTL,
                consumed_at: None,
            },
            CasExpectation::Absent,
        )
        .await
        .expect("seed");
    assert_eq!(
        access.redeem_pairing_code(&canonical, client("Air")).await,
        Err(EnvironmentAccessError::InvalidCode)
    );
}

#[tokio::test]
async fn concurrent_redemptions_of_one_code_have_exactly_one_winner() {
    let root = root();
    let access = Arc::new(ready(&root).await);
    let issued = access.issue_pairing_code().await.expect("issue");
    let mut tasks = Vec::new();
    for n in 0..8 {
        let access = Arc::clone(&access);
        let code = issued.code.clone();
        tasks.push(tokio::spawn(async move {
            access
                .redeem_pairing_code(&code, client(&format!("racer {n}")))
                .await
        }));
    }
    let mut wins = 0;
    for task in tasks {
        match task.await.expect("join") {
            Ok(_) => wins += 1,
            Err(error) => assert_eq!(error, EnvironmentAccessError::InvalidCode),
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(
        access.list_devices(None).await.expect("list").devices.len(),
        1
    );
}

#[tokio::test]
async fn device_token_authenticates_until_revoked_including_after_reopen() {
    let root = root();
    let access = ready(&root).await;
    let issued = access.issue_pairing_code().await.expect("issue");
    let paired = access
        .redeem_pairing_code(&issued.code, client("Air"))
        .await
        .expect("redeem");
    let device_id = DeviceId::new(paired.device_id.clone()).expect("id");

    assert_eq!(
        access
            .authenticate_device(&paired.token)
            .await
            .expect("auth"),
        Some(device_id.clone())
    );
    let forged = format!(
        "icd_{}_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        paired.device_id
    );
    assert_eq!(
        access.authenticate_device(&forged).await.expect("auth"),
        None
    );

    access.revoke_device(&device_id).await.expect("revoke");
    access
        .revoke_device(&device_id)
        .await
        .expect("revoke is idempotent");
    assert_eq!(
        access
            .authenticate_device(&paired.token)
            .await
            .expect("auth"),
        None
    );

    let reopened = open(&root);
    assert_eq!(
        reopened
            .authenticate_device(&paired.token)
            .await
            .expect("auth"),
        None,
        "revocation is durable across a restart"
    );
    let listed = reopened
        .list_devices(Some(&device_id))
        .await
        .expect("list")
        .devices;
    assert_eq!(listed.len(), 1, "revoked devices stay listed");
    assert!(listed[0].revoked_at.is_some());
    assert!(listed[0].current);
    assert!(
        listed[0].last_seen_at.is_some(),
        "the first auth recorded last_seen_at"
    );
}

#[tokio::test]
async fn revoking_an_unknown_device_is_not_found() {
    let root = root();
    let access = ready(&root).await;
    let unknown = DeviceId::new("0123456789abcdef0123456789abcdef").expect("id");
    assert_eq!(
        access.revoke_device(&unknown).await,
        Err(EnvironmentAccessError::DeviceNotFound)
    );
}

#[tokio::test]
async fn tailnet_owner_pairing_records_how_the_device_paired() {
    let root = root();
    let access = ready(&root).await;
    let paired = access
        .pair_tailnet_owner("owner@example.com", client("Laptop"))
        .await
        .expect("pair");
    let listed = access.list_devices(None).await.expect("list").devices;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].device_id, paired.device_id);
    assert_eq!(listed[0].paired_by, DevicePairedBy::TailnetIdentity);
    assert!(!listed[0].current);
}

#[test]
fn client_fields_are_trimmed_stripped_and_capped() {
    assert_eq!(bounded_client_field("  Air\u{0007} ", "x"), "Air");
    assert_eq!(bounded_client_field("   ", "fallback"), "fallback");
    assert_eq!(
        bounded_client_field(&"é".repeat(500), "x").chars().count(),
        MAX_CLIENT_FIELD_CHARS
    );
}
