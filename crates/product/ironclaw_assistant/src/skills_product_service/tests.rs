use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ironclaw_filesystem::DiskFilesystem;
use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_host_api::mount::{MountGrant, MountPermissions, MountView};
use ironclaw_host_api::path::{HostPath, MountAlias, VirtualPath};
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use ironclaw_skills::ScopedSkillManagementPort;
use ironclaw_skills::usage::SharedSkillUsage;

use super::{ScopedSkillsProductService, caller_skill_scope};
use crate::SkillsProductService;

fn caller(user_id: &str) -> ProductSurfaceCaller {
    caller_in_tenant("tenant-alpha", user_id)
}

fn caller_in_tenant(tenant_id: &str, user_id: &str) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new(tenant_id).expect("tenant"),
        UserId::new(user_id).expect("user"),
        None,
        None,
    )
}

#[tokio::test]
async fn skills_product_service_surfaces_shared_auto_activate_learned_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage_root = dir.path().join("standalone");
    std::fs::create_dir_all(&storage_root).expect("storage root");

    let mut filesystem = DiskFilesystem::new();
    filesystem
        .mount_local(
            VirtualPath::new("/projects").expect("valid virtual path"),
            HostPath::from_path_buf(storage_root.clone()),
        )
        .expect("mount storage root");
    let filesystem: Arc<dyn ironclaw_filesystem::RootFilesystem> = Arc::new(filesystem);
    let skill_management = Arc::new(ScopedSkillManagementPort::new_with_mount_resolver(
        UserId::new("runtime-owner").expect("user"),
        filesystem,
        Arc::new(scoped_skill_mounts),
    ));
    // Share the flag the way production composition does: the activation
    // selector holds the same `Arc`, so a toggle here must be observable on
    // that handle (that is the whole point of the live master switch).
    let flag = Arc::new(AtomicBool::new(true));
    let service = ScopedSkillsProductService::new(
        skill_management,
        Some(Arc::clone(&flag)),
        SharedSkillUsage::default(),
    );
    let owner = caller("runtime-owner");

    let listed = service.list_skills(owner.clone()).await.expect("list");
    assert!(
        listed.auto_activate_learned,
        "default master switch must report on"
    );

    flag.store(false, Ordering::Relaxed);
    assert!(
        !flag.load(Ordering::Relaxed),
        "test setup must flip the shared selector flag to false"
    );
    let listed = service.list_skills(owner.clone()).await.expect("list");
    assert!(
        !listed.auto_activate_learned,
        "list must report the master switch as off after disabling"
    );

    flag.store(true, Ordering::Relaxed);
    assert!(
        flag.load(Ordering::Relaxed),
        "test setup must flip the shared selector flag back to true"
    );
    let listed = service.list_skills(owner).await.expect("list");
    assert!(
        listed.auto_activate_learned,
        "list must report the master switch as on after re-enabling"
    );
}

#[tokio::test]
async fn skills_product_service_defaults_auto_activate_learned_when_no_selector_is_wired() {
    // Production assembly mounts the read service but wires no standalone
    // flag-reading selector. The list still renders with a sane default
    // rather than erroring; writes go through the first-party capability.
    let dir = tempfile::tempdir().expect("tempdir");
    let storage_root = dir.path().join("standalone");
    std::fs::create_dir_all(&storage_root).expect("storage root");

    let mut filesystem = DiskFilesystem::new();
    filesystem
        .mount_local(
            VirtualPath::new("/projects").expect("valid virtual path"),
            HostPath::from_path_buf(storage_root.clone()),
        )
        .expect("mount storage root");
    let filesystem: Arc<dyn ironclaw_filesystem::RootFilesystem> = Arc::new(filesystem);
    let skill_management = Arc::new(ScopedSkillManagementPort::new_with_mount_resolver(
        UserId::new("runtime-owner").expect("user"),
        filesystem,
        Arc::new(scoped_skill_mounts),
    ));
    let service =
        ScopedSkillsProductService::new(skill_management, None, SharedSkillUsage::default());
    let owner = caller("runtime-owner");

    let listed = service.list_skills(owner).await.expect("list");
    assert!(
        listed.auto_activate_learned,
        "list defaults to on when no selector flag is wired"
    );
}

#[tokio::test]
async fn skills_product_service_hides_owner_user_skills_from_other_callers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage_root = dir.path().join("standalone");
    std::fs::create_dir_all(&storage_root).expect("storage root");
    std::fs::create_dir_all(storage_root.join("system/skills/system-helper"))
        .expect("system skill dir");
    std::fs::write(
        storage_root.join("system/skills/system-helper/SKILL.md"),
        skill_content("system-helper", "system skill"),
    )
    .expect("system skill");

    let mut filesystem = DiskFilesystem::new();
    filesystem
        .mount_local(
            VirtualPath::new("/projects").expect("valid virtual path"),
            HostPath::from_path_buf(storage_root.clone()),
        )
        .expect("mount storage root");
    let filesystem: Arc<dyn ironclaw_filesystem::RootFilesystem> = Arc::new(filesystem);
    let skill_management = Arc::new(ScopedSkillManagementPort::new_with_mount_resolver(
        UserId::new("runtime-owner").expect("user"),
        filesystem,
        Arc::new(scoped_skill_mounts),
    ));
    let service = ScopedSkillsProductService::new(
        Arc::clone(&skill_management),
        Some(Arc::new(AtomicBool::new(true))),
        SharedSkillUsage::default(),
    );
    let owner = caller("runtime-owner");
    let bob = caller("bob");
    let other_tenant_owner = caller_in_tenant("tenant-beta", "runtime-owner");

    skill_management
        .install_for_scope(
            caller_skill_scope(owner.clone()),
            Some("shared-name"),
            &skill_content("shared-name", "alice skill"),
        )
        .await
        .expect("owner installs skill");

    let owner_skills = service
        .list_skills(owner)
        .await
        .expect("owner lists skills")
        .skills;
    assert!(owner_skills.iter().any(|skill| skill.name == "shared-name"));
    let bob_skills = service
        .list_skills(bob.clone())
        .await
        .expect("bob lists skills")
        .skills;
    assert!(!bob_skills.iter().any(|skill| skill.name == "shared-name"));
    assert!(bob_skills.iter().any(|skill| skill.name == "system-helper"));
    let other_tenant_skills = service
        .list_skills(other_tenant_owner.clone())
        .await
        .expect("same user id in another tenant lists skills")
        .skills;
    assert!(
        !other_tenant_skills
            .iter()
            .any(|skill| skill.name == "shared-name")
    );

    let bob_read = service
        .read_skill_content(bob.clone(), "shared-name".to_string())
        .await
        .expect_err("bob must not read the owner skill root");
    assert_eq!(bob_read.status_code, 404);
    let other_tenant_read = service
        .read_skill_content(other_tenant_owner.clone(), "shared-name".to_string())
        .await
        .expect_err("same user id in another tenant must not read the owner skill root");
    assert_eq!(other_tenant_read.status_code, 404);

    skill_management
        .install_for_scope(
            caller_skill_scope(bob.clone()),
            Some("bob-skill"),
            &skill_content("bob-skill", "bob skill"),
        )
        .await
        .expect("bob installs own skill");
    let bob_content = service
        .read_skill_content(bob.clone(), "bob-skill".to_string())
        .await
        .expect("bob reads own skill");
    assert!(bob_content.content.contains("bob skill"));
    let owner_cannot_read_bob = service
        .read_skill_content(caller("runtime-owner"), "bob-skill".to_string())
        .await
        .expect_err("owner must not read bob skill root");
    assert_eq!(owner_cannot_read_bob.status_code, 404);

    assert!(
        storage_root
            .join("tenants/tenant-alpha/users/runtime-owner/skills/shared-name/SKILL.md")
            .exists()
    );
    assert!(
        storage_root
            .join("tenants/tenant-alpha/users/bob/skills/bob-skill/SKILL.md")
            .exists()
    );
}

fn scoped_skill_mounts(
    scope: &ResourceScope,
) -> Result<MountView, ironclaw_host_api::error::HostApiError> {
    let user_skills = format!(
        "/projects/tenants/{}/users/{}/skills",
        scope.tenant_id.as_str(),
        scope.user_id.as_str()
    );
    MountView::new(vec![
        MountGrant::new(
            MountAlias::new("/skills")?,
            VirtualPath::new(user_skills)?,
            MountPermissions::read_write_list_delete(),
        ),
        MountGrant::new(
            MountAlias::new("/system/skills")?,
            VirtualPath::new("/projects/system/skills")?,
            MountPermissions::read_only(),
        ),
    ])
}

fn skill_content(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\nUse this skill.\n")
}
