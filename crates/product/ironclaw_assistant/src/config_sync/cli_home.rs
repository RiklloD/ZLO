//! `cli_home` domain handler: the person's own rules and skills files in
//! the homes of the coding-agent CLIs, which those CLIs read by themselves
//! and no ZLO store holds.
//!
//! The roots (a folder and the entries in it that count) are handed in by
//! the host; this handler knows no CLI by name and touches nothing outside
//! them. An item is one file, keyed `<root id>/<path inside the root>`, so
//! the same file matches on two computers whatever the root folder is
//! called there.
//!
//! One-way, the source wins; a file only the receiver has is kept, and
//! nothing is deleted. Hidden files and folders are left out. Links are
//! followed only while they stay inside the root or the person's home, on
//! read and on write.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use async_trait::async_trait;
use ironclaw_product_contracts::config_sync::{
    ConfigSyncDomain, ConfigSyncDomainDigest, ConfigSyncDomainPayload,
};
use uuid::Uuid;

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult, items,
};

const DOMAIN: ConfigSyncDomain = ConfigSyncDomain::CliHome;
/// The mode note of an executable file (`ConfigSyncItemFile::path`).
const EXECUTABLE: &str = "executable";
/// Folders below an entry that are walked.
const MAX_DEPTH: usize = 8;
/// Files listed across every root.
const MAX_FILES: usize = 4_000;

/// One folder of the person's and the entries in it that are carried.
#[derive(Debug, Clone)]
pub struct CliHomeRoot {
    /// Stable name of the root, the first segment of every item key.
    pub id: String,
    pub dir: PathBuf,
    /// Files or folders directly inside `dir`, by relative path.
    pub entries: Vec<String>,
}

/// A listed file: where it is and what was read of it.
struct Listed {
    path: PathBuf,
    hash: String,
}

pub struct CliHomeDomainHandler {
    home: PathBuf,
    roots: Vec<CliHomeRoot>,
    /// Digests by path, valid while size and modification time hold, so a
    /// manifest does not read every file again.
    seen: Mutex<BTreeMap<PathBuf, (u64, SystemTime, String)>>,
}

impl CliHomeDomainHandler {
    pub fn new(home: PathBuf, roots: Vec<CliHomeRoot>) -> Self {
        Self {
            home,
            roots,
            seen: Mutex::new(BTreeMap::new()),
        }
    }

    /// The handler over the roots the CLI runtimes name for the person
    /// whose home is `home` (`ironclaw_cli_providers::user_rules`).
    pub fn for_home(home: PathBuf) -> Self {
        let roots = ironclaw_cli_providers::user_rules::roots(&home)
            .into_iter()
            .map(|root| CliHomeRoot {
                id: root.id.to_string(),
                dir: root.dir,
                entries: root.entries.iter().map(|e| (*e).to_string()).collect(),
            })
            .collect();
        Self::new(home, roots)
    }

    /// `path` resolved, when it stays inside its root or the home.
    fn contained(&self, root: &CliHomeRoot, path: &Path) -> Option<PathBuf> {
        let real = path.canonicalize().ok()?;
        let inside = |base: &Path| base.canonicalize().is_ok_and(|base| real.starts_with(base));
        (inside(&root.dir) || inside(&self.home)).then_some(real)
    }

    fn digest(&self, path: &Path, meta: &std::fs::Metadata) -> io::Result<String> {
        let stamp = (meta.len(), meta.modified()?);
        if let Some((len, modified, hash)) = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            && (*len, *modified) == stamp
        {
            return Ok(hash.clone());
        }
        let hash = file_hash(&std::fs::read(path)?, executable(meta));
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), (stamp.0, stamp.1, hash.clone()));
        Ok(hash)
    }

    /// Every carried file here: item key to where it is and its digest.
    fn list(&self) -> BTreeMap<String, Listed> {
        let mut out = BTreeMap::new();
        for root in &self.roots {
            for entry in &root.entries {
                let key = format!("{}/{entry}", root.id);
                self.walk(root, &root.dir.join(entry), key, 0, &mut out);
            }
        }
        out
    }

    fn walk(
        &self,
        root: &CliHomeRoot,
        path: &Path,
        key: String,
        depth: usize,
        out: &mut BTreeMap<String, Listed>,
    ) {
        if out.len() >= MAX_FILES {
            return;
        }
        let Some(real) = self.contained(root, path) else {
            return;
        };
        let Ok(meta) = std::fs::metadata(&real) else {
            return;
        };
        if meta.is_file() {
            match self.digest(&real, &meta) {
                Ok(hash) => {
                    out.insert(key, Listed { path: real, hash });
                }
                Err(error) => tracing::debug!(%error, %key, "config sync: file not read"),
            }
            return;
        }
        if !meta.is_dir() || depth >= MAX_DEPTH {
            return;
        }
        let Ok(children) = std::fs::read_dir(&real) else {
            return;
        };
        let mut names: Vec<String> = children
            .flatten()
            .filter_map(|child| child.file_name().into_string().ok())
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort();
        for name in names {
            self.walk(
                root,
                &path.join(&name),
                format!("{key}/{name}"),
                depth + 1,
                out,
            );
        }
    }

    /// Where an incoming item belongs here; `None` when its key names no
    /// root and entry of this host, or is not a plain relative path.
    fn target(&self, key: &str) -> Option<(&CliHomeRoot, PathBuf)> {
        let (id, rel) = key.split_once('/')?;
        let root = self.roots.iter().find(|root| root.id == id)?;
        let plain = Path::new(rel).components().all(|part| match part {
            Component::Normal(name) => name.to_str().is_some_and(|name| !name.starts_with('.')),
            _ => false,
        });
        let listed = root.entries.iter().any(|entry| {
            rel == entry
                || rel
                    .strip_prefix(entry.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        });
        (plain && listed && !rel.is_empty()).then(|| (root, root.dir.join(rel)))
    }

    /// Write one file in place of what is there, through a temporary file
    /// beside it.
    fn write(&self, root: &CliHomeRoot, path: &Path, bytes: &[u8], exec: bool) -> io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("the file has no folder"))?;
        // A link on the way must not lead out of the root and the home:
        // checked on what exists before any folder is made through it, and
        // again on the folder itself.
        let existing = parent
            .ancestors()
            .find(|dir| dir.exists())
            .ok_or_else(|| io::Error::other("the file has no folder"))?;
        if self.contained(root, existing).is_none() {
            return Err(io::Error::other(
                "its folder leads outside the person's home",
            ));
        }
        std::fs::create_dir_all(parent)?;
        if self.contained(root, parent).is_none() {
            return Err(io::Error::other(
                "its folder leads outside the person's home",
            ));
        }
        if let Ok(meta) = path.symlink_metadata()
            && !meta.is_file()
            && self.contained(root, path).is_none()
        {
            return Err(io::Error::other(
                "a link outside the person's home is in its place",
            ));
        }
        let temporary = parent.join(format!(".zlo-sync-{}.tmp", Uuid::new_v4()));
        let written = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            set_executable(&temporary, exec)?;
            std::fs::rename(&temporary, path)
        })();
        if written.is_err()
            && let Err(error) = std::fs::remove_file(&temporary)
        {
            tracing::debug!(%error, "config sync: temporary file not removed");
        }
        written
    }
}

fn file_hash(bytes: &[u8], exec: bool) -> String {
    let mode = if exec { EXECUTABLE } else { "" };
    items::files_hash(&[(mode.to_string(), bytes.to_vec())])
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(path: &Path, exec: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if exec { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_executable(_: &Path, _: bool) -> io::Result<()> {
    Ok(())
}

fn page_of(
    payload: &ConfigSyncDomainPayload,
) -> Result<&ironclaw_product_contracts::config_sync::ItemizedDomainPayload, ConfigSyncError> {
    match payload {
        ConfigSyncDomainPayload::CliHome(page) => Ok(page),
        _ => Err(ConfigSyncError::validation(
            "cli_home",
            "expected cli_home payload",
        )),
    }
}

#[async_trait]
impl ConfigSyncDomainHandler for CliHomeDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        DOMAIN
    }

    async fn manifest(
        &self,
        _ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let index: BTreeMap<String, String> = self
            .list()
            .into_iter()
            .map(|(key, listed)| (key, listed.hash))
            .collect();
        Ok(ConfigSyncDomainDigest {
            hash: items::domain_hash(&index),
            updated_at: None,
            item_count: index.len() as u64,
        })
    }

    async fn export(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let listed = self.list();
        let index: BTreeMap<String, String> = listed
            .iter()
            .map(|(key, file)| (key.clone(), file.hash.clone()))
            .collect();
        let page = items::page(&index, &ctx.page, |key| {
            let path = listed.get(&key).map(|file| file.path.clone());
            async move {
                let Some(path) = path else { return Ok(None) };
                let read = std::fs::metadata(&path)
                    .and_then(|meta| Ok((std::fs::read(&path)?, executable(&meta))));
                Ok(match read {
                    Ok((bytes, exec)) => {
                        let mode = if exec { EXECUTABLE } else { "" };
                        Some(vec![(mode.to_string(), bytes)])
                    }
                    // Gone since it was listed.
                    Err(_) => None,
                })
            }
        })
        .await?;
        Ok(ConfigSyncDomainPayload::CliHome(page))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        let page = page_of(payload)?;
        items::validate(DOMAIN, page)?;
        for item in &page.items {
            let one_file = item.files.as_ref().is_none_or(|files| {
                files.len() == 1 && matches!(files[0].path.as_str(), "" | EXECUTABLE)
            });
            if !one_file {
                return Err(ConfigSyncError::validation(
                    "cli_home.items",
                    format!("item `{}` is not one file", item.key),
                ));
            }
        }
        Ok(())
    }

    async fn apply(
        &self,
        ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let page = page_of(payload)?;
        // Files go one way; a return leg has nothing to take.
        if ctx.merge_back {
            return Ok(DomainApplyResult::unchanged(page.items.len() as u64));
        }
        let held = self.list();
        let (mut applied, mut skipped) = (0u64, 0u64);
        let mut warnings: Vec<String> = page
            .too_large
            .iter()
            .map(|item| format!("{item} is too large to sync"))
            .collect();
        for item in &page.items {
            let Some((mode, bytes)) = items::decode(DOMAIN, item)?.and_then(|mut f| f.pop()) else {
                skipped += 1;
                continue;
            };
            let exec = mode == EXECUTABLE;
            // A key this host has no place for (a CLI it does not list) is
            // not an error: the other computer simply has more.
            let Some((root, path)) = self.target(&item.key) else {
                skipped += 1;
                continue;
            };
            if held.get(&item.key).map(|file| file.hash.as_str()) == Some(&file_hash(&bytes, exec))
            {
                skipped += 1;
                continue;
            }
            match self.write(root, &path, &bytes, exec) {
                Ok(()) => applied += 1,
                Err(error) => {
                    tracing::debug!(key = %item.key, %error, "config sync: file not written");
                    warnings.push(format!("{} was not synced: {error}", item.key));
                    skipped += 1;
                }
            }
        }
        let mut result = if applied == 0 {
            DomainApplyResult::unchanged(skipped)
        } else {
            DomainApplyResult::applied(applied, skipped)
        };
        result.outcome.warnings = warnings;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use ironclaw_host_api::ids::{TenantId, UserId};
    use ironclaw_product_contracts::config_sync::{
        ConfigSyncItem, ConfigSyncItemFile, ItemizedDomainPayload,
    };
    use ironclaw_product_contracts::surface::ProductSurfaceCaller;

    use super::*;

    struct Host {
        dir: tempfile::TempDir,
        handler: CliHomeDomainHandler,
    }

    impl Host {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).expect("home");
            let roots = vec![
                CliHomeRoot {
                    id: "alpha".into(),
                    dir: home.join(".alpha"),
                    entries: vec!["RULES.md".into(), "skills".into()],
                },
                CliHomeRoot {
                    id: "beta".into(),
                    dir: home.join(".beta"),
                    entries: vec!["config/skills".into()],
                },
            ];
            let handler = CliHomeDomainHandler::new(home, roots);
            Self { dir, handler }
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join("home").join(rel)
        }
        fn put(&self, rel: &str, text: &str) {
            let path = self.path(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(path, text).expect("write");
        }
        fn get(&self, rel: &str) -> Option<String> {
            std::fs::read_to_string(self.path(rel)).ok()
        }
        async fn hash(&self) -> String {
            self.handler.manifest(&ctx()).await.expect("manifest").hash
        }
    }

    fn ctx() -> ConfigSyncContext {
        ConfigSyncContext {
            caller: ProductSurfaceCaller::new(
                TenantId::new("tenant-a").expect("tenant"),
                UserId::new("alice").expect("user"),
                None,
                None,
            ),
            data_folder: "/data".into(),
            home_dir: "/home/alice".into(),
            page: items::PageRequest::default(),
            merge_back: false,
            peer_environment_id: Some("peer".into()),
        }
    }

    async fn carry(from: &Host, to: &Host) -> DomainApplyResult {
        let payload = from.handler.export(&ctx()).await.expect("export");
        to.handler.validate(&payload).expect("valid");
        to.handler.apply(&ctx(), &payload).await.expect("apply")
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[tokio::test]
    async fn rules_and_skills_arrive_and_nothing_else_does() {
        let (mac, boxed) = (Host::new(), Host::new());
        mac.put(".alpha/RULES.md", "Be brief.\n");
        mac.put(".alpha/skills/deploy/SKILL.md", "# Deploy\n");
        mac.put(".alpha/skills/deploy/run.sh", "#!/bin/sh\necho hi\n");
        mac.put(".beta/config/skills/review/SKILL.md", "# Review\n");
        #[cfg(unix)]
        make_executable(&mac.path(".alpha/skills/deploy/run.sh"));
        // Never carried: a settings file beside the rules, a login, a
        // hidden file inside a carried folder.
        mac.put(".alpha/settings.json", "{\"token\":\"secret\"}");
        mac.put(".beta/oauth_creds.json", "{}");
        mac.put(".alpha/skills/.credentials", "secret");
        // The box's own: kept.
        boxed.put(".alpha/RULES.md", "Old rules.\n");
        boxed.put(".alpha/skills/box-only/SKILL.md", "# Local\n");
        boxed.put(".alpha/settings.json", "{\"box\":true}");

        let result = carry(&mac, &boxed).await;
        assert_eq!(result.outcome.applied_count, 4, "{:?}", result.outcome);
        assert_eq!(boxed.get(".alpha/RULES.md").as_deref(), Some("Be brief.\n"));
        assert_eq!(
            boxed.get(".alpha/skills/deploy/SKILL.md").as_deref(),
            Some("# Deploy\n")
        );
        assert_eq!(
            boxed.get(".beta/config/skills/review/SKILL.md").as_deref(),
            Some("# Review\n")
        );
        #[cfg(unix)]
        {
            let meta = std::fs::metadata(boxed.path(".alpha/skills/deploy/run.sh")).expect("meta");
            assert!(executable(&meta), "the script stays runnable");
        }
        assert_eq!(
            boxed.get(".alpha/settings.json").as_deref(),
            Some("{\"box\":true}")
        );
        assert_eq!(boxed.get(".beta/oauth_creds.json"), None);
        assert_eq!(boxed.get(".alpha/skills/.credentials"), None);
        assert!(boxed.get(".alpha/skills/box-only/SKILL.md").is_some());
        // The Mac was only read, and a second pass writes nothing.
        assert_eq!(mac.get(".alpha/skills/box-only/SKILL.md"), None);
        let again = carry(&mac, &boxed).await;
        assert_eq!(again.outcome.applied_count, 0);

        // Without a file of its own the two sides hash the same.
        std::fs::remove_dir_all(boxed.path(".alpha/skills/box-only")).expect("remove");
        assert_eq!(mac.hash().await, boxed.hash().await);
    }

    fn item(key: &str, text: &str) -> ConfigSyncItem {
        let files = items::encode(vec![(String::new(), text.as_bytes().to_vec())]);
        ConfigSyncItem {
            key: key.into(),
            hash: file_hash(text.as_bytes(), false),
            files: Some(files),
        }
    }

    #[tokio::test]
    async fn a_key_outside_the_listed_entries_writes_nothing() {
        let boxed = Host::new();
        let page = ItemizedDomainPayload {
            hash: String::new(),
            items: vec![
                item("alpha/settings.json", "x"),
                item("alpha/skills/../settings.json", "x"),
                item("alpha/skills/.hidden", "x"),
                item("alpha/skillsX/a.md", "x"),
                item("alpha//etc/passwd", "x"),
                item("gamma/RULES.md", "x"),
                item("alpha", "x"),
            ],
            next: None,
            too_large: Vec::new(),
        };
        let result = boxed
            .handler
            .apply(&ctx(), &ConfigSyncDomainPayload::CliHome(page))
            .await
            .expect("apply");
        assert_eq!(result.outcome.applied_count, 0);
        assert!(!boxed.path(".alpha").exists(), "nothing was created");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_out_of_the_home_is_neither_read_nor_written_through() {
        let (mac, boxed) = (Host::new(), Host::new());
        let outside = boxed.dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::create_dir_all(boxed.path(".alpha")).expect("root");
        std::os::unix::fs::symlink(&outside, boxed.path(".alpha/skills")).expect("link");
        mac.put(".alpha/skills/deploy/SKILL.md", "# Deploy\n");
        let result = carry(&mac, &boxed).await;
        assert_eq!(result.outcome.applied_count, 0);
        assert_eq!(result.outcome.warnings.len(), 1, "{:?}", result.outcome);
        assert!(!outside.join("deploy").exists());

        // And a file reached through such a link is not exported.
        std::fs::write(outside.join("leak.md"), "private").expect("write");
        let ConfigSyncDomainPayload::CliHome(page) =
            boxed.handler.export(&ctx()).await.expect("export")
        else {
            panic!("payload");
        };
        assert!(page.items.is_empty(), "{:?}", page.items);
    }

    #[test]
    fn a_payload_item_must_be_one_file() {
        let handler = Host::new().handler;
        let mut bad = item("alpha/RULES.md", "x");
        bad.files = Some(vec![ConfigSyncItemFile {
            path: "a".into(),
            data_b64: String::new(),
        }]);
        let page = ItemizedDomainPayload {
            hash: String::new(),
            items: vec![bad],
            next: None,
            too_large: Vec::new(),
        };
        assert!(
            handler
                .validate(&ConfigSyncDomainPayload::CliHome(page))
                .is_err()
        );
    }
}
