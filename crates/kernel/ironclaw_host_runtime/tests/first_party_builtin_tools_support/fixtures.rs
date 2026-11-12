use super::*;

pub(crate) fn registry() -> ExtensionRegistry {
    let mut registry = ExtensionRegistry::new();
    registry
        .insert(builtin_first_party_package().unwrap())
        .unwrap();
    // Native memory rides the always-on lane alongside builtin; register its
    // package so the `ironclaw.memory.*` capabilities are declared for
    // dispatch + surface in these host-runtime tests (mirrors composition).
    registry
        .insert(native_memory_first_party_package().unwrap())
        .unwrap();
    registry
}

pub(crate) fn capability_id(value: &str) -> CapabilityId {
    CapabilityId::new(value).unwrap()
}

pub(crate) fn provider_id() -> ExtensionId {
    ExtensionId::new("builtin").unwrap()
}

pub(crate) fn all_builtin_capability_ids() -> Vec<&'static str> {
    vec![
        ECHO_CAPABILITY_ID,
        TIME_CAPABILITY_ID,
        PLAN_UPDATE_CAPABILITY_ID,
        JSON_CAPABILITY_ID,
        HTTP_CAPABILITY_ID,
        HTTP_SAVE_CAPABILITY_ID,
        SHELL_CAPABILITY_ID,
        SPAWN_SUBAGENT_CAPABILITY_ID,
        TRACE_COMMONS_ONBOARD_CAPABILITY_ID,
        TRACE_COMMONS_STATUS_CAPABILITY_ID,
        TRACE_COMMONS_CREDITS_CAPABILITY_ID,
        TRACE_COMMONS_PROFILE_TOKEN_CAPABILITY_ID,
        TRACE_COMMONS_PROFILE_SET_CAPABILITY_ID,
        TRACE_COMMONS_ACCOUNT_LOGIN_LINK_CAPABILITY_ID,
        OUTBOUND_DELIVER_CAPABILITY_ID,
        ATTACH_WORKSPACE_FILE_TO_REPLY_CAPABILITY_ID,
        READ_FILE_CAPABILITY_ID,
        WRITE_FILE_CAPABILITY_ID,
        LIST_DIR_CAPABILITY_ID,
        GLOB_CAPABILITY_ID,
        GREP_CAPABILITY_ID,
        APPLY_PATCH_CAPABILITY_ID,
        DOCUMENT_EDIT_CAPABILITY_ID,
        HTML_TO_PDF_CAPABILITY_ID,
        SKILL_LIST_CAPABILITY_ID,
        SKILL_INSTALL_CAPABILITY_ID,
        SKILL_UPDATE_CAPABILITY_ID,
        SKILL_AUTO_ACTIVATE_SET_CAPABILITY_ID,
        SKILL_REMOVE_CAPABILITY_ID,
        TRIGGER_CREATE_CAPABILITY_ID,
        TRIGGER_LIST_CAPABILITY_ID,
        TRIGGER_STATUS_CAPABILITY_ID,
        TRIGGER_REMOVE_CAPABILITY_ID,
        TRIGGER_PAUSE_CAPABILITY_ID,
        TRIGGER_RESUME_CAPABILITY_ID,
        TRIGGER_RUN_CAPABILITY_ID,
    ]
}

pub(crate) fn all_always_on_first_party_capability_ids() -> Vec<&'static str> {
    let mut ids = all_builtin_capability_ids();
    ids.extend([
        MEMORY_SEARCH_CAPABILITY_ID,
        MEMORY_WRITE_CAPABILITY_ID,
        MEMORY_READ_CAPABILITY_ID,
        MEMORY_TREE_CAPABILITY_ID,
    ]);
    ids
}

pub(crate) fn mounted_filesystem(
    path: &Path,
    permissions: MountPermissions,
) -> (DiskFilesystem, MountView) {
    let mut filesystem = DiskFilesystem::new();
    filesystem
        .mount_local(
            VirtualPath::new("/projects/coding-pack").unwrap(),
            HostPath::from_path_buf(path.to_path_buf()),
        )
        .unwrap();
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/coding-pack").unwrap(),
        permissions,
    )])
    .unwrap();
    (filesystem, mounts)
}

pub(crate) fn in_memory_mounted_filesystem(
    permissions: MountPermissions,
) -> (InMemoryBackend, MountView) {
    (InMemoryBackend::new(), workspace_mounts(permissions))
}

pub(crate) fn workspace_mounts(permissions: MountPermissions) -> MountView {
    MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/coding-pack").unwrap(),
        permissions,
    )])
    .unwrap()
}

pub(crate) fn memory_mounts(permissions: MountPermissions) -> MountView {
    MountView::new(vec![MountGrant::new(
        MountAlias::new("/memory").unwrap(),
        VirtualPath::new("/memory").unwrap(),
        permissions,
    )])
    .unwrap()
}

pub(crate) fn mounted_skill_filesystem(path: &Path) -> (DiskFilesystem, MountView) {
    let mut filesystem = DiskFilesystem::new();
    filesystem
        .mount_local(
            VirtualPath::new("/projects/skills").unwrap(),
            HostPath::from_path_buf(path.to_path_buf()),
        )
        .unwrap();
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/skills").unwrap(),
        VirtualPath::new("/projects/skills").unwrap(),
        MountPermissions::read_write_list_delete(),
    )])
    .unwrap();
    (filesystem, mounts)
}

pub(crate) fn mounted_host_filesystem(
    path: &Path,
    permissions: MountPermissions,
) -> (DiskFilesystem, MountView) {
    let mut filesystem = DiskFilesystem::new();
    filesystem
        .mount_local(
            VirtualPath::new("/projects/host").unwrap(),
            HostPath::from_path_buf(path.to_path_buf()),
        )
        .unwrap();
    let raw_alias = path.canonicalize().unwrap().to_string_lossy().into_owned();
    let mounts = MountView::new(vec![
        MountGrant::new(
            MountAlias::new("/host").unwrap(),
            VirtualPath::new("/projects/host").unwrap(),
            permissions.clone(),
        ),
        MountGrant::new(
            MountAlias::new(raw_alias).unwrap(),
            VirtualPath::new("/projects/host").unwrap(),
            permissions,
        ),
    ])
    .unwrap();
    (filesystem, mounts)
}

pub(crate) fn skill_bundle_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    skill_bundle_zip_owned(
        files
            .iter()
            .map(|(path, content)| ((*path).to_string(), (*content).to_vec())),
    )
}

pub(crate) fn skill_bundle_zip_owned(
    files: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> Vec<u8> {
    skill_bundle_zip_with_dirs(std::iter::empty::<String>(), files)
}

pub(crate) fn skill_bundle_zip_with_dirs<D>(
    directories: impl IntoIterator<Item = D>,
    files: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> Vec<u8>
where
    D: AsRef<str>,
{
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let options = zip::write::SimpleFileOptions::default();
    for path in directories {
        writer.add_directory(path.as_ref(), options).unwrap();
    }
    for (path, content) in files {
        writer.start_file(path, options).unwrap();
        writer.write_all(&content).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
