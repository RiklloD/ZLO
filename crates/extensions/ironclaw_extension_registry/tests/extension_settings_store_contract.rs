//! Contract for the per-user package settings store: values are validated
//! against the declaration, isolated per user scope, read back through the
//! [`ExtensionSettingsReader`] port the runtime lanes use, and cleared by
//! removal.

#![deny(clippy::let_underscore_must_use)]

use std::collections::BTreeMap;
use std::sync::Arc;

use ironclaw_extension_registry::settings::{
    ExtensionSettingDecl, ExtensionSettingsError, ExtensionSettingsReader, ExtensionSettingsStore,
    effective_setting_values,
};
use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
use ironclaw_host_api::ids::{ExtensionId, InvocationId, TenantId, UserId};
use ironclaw_host_api::mount::{MountGrant, MountPermissions, MountView};
use ironclaw_host_api::path::{MountAlias, VirtualPath};
use ironclaw_host_api::resource::ResourceScope;
use serde_json::{Value, json};

fn scope(user: &str) -> ResourceScope {
    ResourceScope {
        tenant_id: TenantId::new("tenant-a").unwrap(),
        user_id: UserId::new(user).unwrap(),
        agent_id: None,
        project_id: None,
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    }
}

/// The production per-user shape: `/extension-settings` resolves under the
/// caller's own tenant/user subtree.
fn store() -> ExtensionSettingsStore<InMemoryBackend> {
    let filesystem = ScopedFilesystem::new(Arc::new(InMemoryBackend::new()), |scope| {
        MountView::new(vec![MountGrant::new(
            MountAlias::new("/extension-settings")?,
            VirtualPath::new(format!(
                "/tenants/{}/users/{}/extension-settings",
                scope.tenant_id.as_str(),
                scope.user_id.as_str()
            ))?,
            MountPermissions::read_write_list_delete(),
        )])
    });
    ExtensionSettingsStore::new(Arc::new(filesystem))
}

fn declarations() -> Vec<ExtensionSettingDecl> {
    toml::from_str::<BTreeMap<String, Vec<ExtensionSettingDecl>>>(
        r#"
[[settings]]
id = "units"
kind = "choice"
label = "Units"
default = "metric"
options = [{ value = "metric", label = "Metric" }, { value = "imperial", label = "Imperial" }]

[[settings]]
id = "days"
kind = "number"
label = "Days"
default = 3
"#,
    )
    .unwrap()
    .remove("settings")
    .unwrap()
}

#[tokio::test]
async fn set_validates_persists_per_user_and_clears_on_removal() {
    let store = store();
    let extension = ExtensionId::new("weather-guard").unwrap();
    let settings = declarations();
    let alice = scope("alice");
    let bob = scope("bob");

    let stored = store
        .set(&alice, &extension, &settings, "units", json!("imperial"))
        .await
        .unwrap();
    assert_eq!(stored, json!("imperial"));

    // Read back through the lane-facing port and resolve against defaults.
    let alice_values = store.stored_values(&alice, &extension).await.unwrap();
    assert_eq!(
        effective_setting_values(&settings, &alice_values),
        BTreeMap::from([
            ("days".to_string(), json!(3)),
            ("units".to_string(), json!("imperial")),
        ])
    );
    // Another user's scope sees only defaults.
    assert!(
        store
            .stored_values(&bob, &extension)
            .await
            .unwrap()
            .is_empty()
    );

    // Invalid values and unknown ids are refused with a plain reason and
    // leave the stored record unchanged.
    let error = store
        .set(&alice, &extension, &settings, "units", json!("kelvin"))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "`units`: expected one of `metric`, `imperial`"
    );
    let error = store
        .set(&alice, &extension, &settings, "days", Value::from("three"))
        .await
        .unwrap_err();
    assert!(matches!(error, ExtensionSettingsError::InvalidValue { .. }));
    let error = store
        .set(&alice, &extension, &settings, "nope", json!(true))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ExtensionSettingsError::UnknownSetting { .. }
    ));
    assert_eq!(
        store.stored_values(&alice, &extension).await.unwrap(),
        BTreeMap::from([("units".to_string(), json!("imperial"))])
    );

    // Removal forgets the values; a reinstall starts from defaults.
    store.clear(&alice, &extension).await.unwrap();
    assert!(
        store
            .stored_values(&alice, &extension)
            .await
            .unwrap()
            .is_empty()
    );
    // Clearing an absent record is a no-op.
    store.clear(&bob, &extension).await.unwrap();
}
