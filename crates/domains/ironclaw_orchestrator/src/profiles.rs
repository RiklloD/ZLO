//! Profile records (stage 3): the unit a thread belongs to for its whole
//! life. One `orch_profiles` row per (tenant, user, profile id) holds the
//! editable settings as JSON in their wire shape; last use and thread count
//! are derived from `orch_threads`, so a turn never writes a profile row.
//!
//! Threads without a `profile_id` (everything created before profiles, and
//! clients that do not pick one) belong to [`DEFAULT_PROFILE_ID`]. Deleting a
//! profile archives it: it leaves the list, its threads keep resolving it.

use ironclaw_host_api::ids::{CapabilityId, TenantId, ThreadId, UserId};
use ironclaw_product_contracts::profiles::{DEFAULT_PROFILE_ID, ProfileSettings};

use crate::error::OrchestratorError;
use crate::model::{Millis, ProfileId, ThreadScope, now_ms};
use crate::store::{Cols, Store, s};

type Result<T> = std::result::Result<T, OrchestratorError>;

pub(crate) const PROFILES_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_profiles (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  profile_id TEXT NOT NULL,
  settings TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  archived_at INTEGER,
  PRIMARY KEY (tenant_id, user_id, profile_id)
);
CREATE INDEX IF NOT EXISTS orch_threads_profile ON orch_threads(tenant_id, user_id, profile_id);
"#;

const MAX_SLUG_CHARS: usize = 64;
const MAX_DISPLAY_NAME_CHARS: usize = 80;
/// The identity budget is 8,000 tokens (~32 KiB) shared with SYSTEM.md
/// (~5 KiB); a larger SOUL.md would be dropped from the prompt whole.
const MAX_SOUL_BYTES: usize = 24 * 1024;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_ENTRY_BYTES: usize = 200;
const MAX_TEXT_BYTES: usize = 4096;

/// Profiles belong to a user, across that user's agents and projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileOwner {
    pub tenant_id: TenantId,
    pub user_id: UserId,
}

impl From<&ThreadScope> for ProfileOwner {
    fn from(scope: &ThreadScope) -> Self {
        Self {
            tenant_id: scope.tenant_id.clone(),
            user_id: scope.user_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProfileRecord {
    pub profile_id: ProfileId,
    pub settings: ProfileSettings,
    pub created_at: Millis,
    pub updated_at: Millis,
    /// Latest user activity (or creation) of one of its threads.
    pub last_used_at: Option<Millis>,
    pub thread_count: u64,
}

impl ProfileRecord {
    pub fn is_default(&self) -> bool {
        self.profile_id.as_str() == DEFAULT_PROFILE_ID
    }
}

pub fn default_profile_id() -> Result<ProfileId> {
    ProfileId::new(DEFAULT_PROFILE_ID)
}

/// Profile ids double as folder names: lowercase ASCII letters, digits, `-`
/// and `_`, starting with a letter or digit, at most 64 characters.
pub fn parse_profile_slug(raw: &str) -> Result<ProfileId> {
    let valid = !raw.is_empty()
        && raw.len() <= MAX_SLUG_CHARS
        && raw
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        return Err(OrchestratorError::invalid(
            "profile_id must be 1-64 lowercase letters, digits, '-' or '_'",
        ));
    }
    ProfileId::new(raw)
}

fn check_list(name: &str, list: &[String]) -> Result<()> {
    if list.len() > MAX_LIST_ENTRIES
        || list
            .iter()
            .any(|v| v.trim().is_empty() || v.len() > MAX_ENTRY_BYTES)
    {
        return Err(OrchestratorError::invalid(format!(
            "{name} takes at most {MAX_LIST_ENTRIES} non-empty entries of {MAX_ENTRY_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_text(name: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(v) if v.trim().is_empty() || v.len() > MAX_TEXT_BYTES => Err(
            OrchestratorError::invalid(format!("{name} must be non-empty and at most 4 KiB")),
        ),
        _ => Ok(()),
    }
}

/// Bound every field before it is stored or reaches a prompt.
pub fn validate_settings(settings: &ProfileSettings) -> Result<()> {
    let name = settings.display_name.trim();
    if name.is_empty() || name.chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(OrchestratorError::invalid(
            "display_name must be 1-80 characters",
        ));
    }
    if settings.soul.len() > MAX_SOUL_BYTES {
        return Err(OrchestratorError::invalid("soul is limited to 24 KiB"));
    }
    if let Some(list) = &settings.allowed_capabilities {
        check_list("allowed_capabilities", list)?;
        for id in list {
            CapabilityId::new(id.as_str()).map_err(|reason| {
                OrchestratorError::invalid(format!("allowed_capabilities: {reason}"))
            })?;
        }
    }
    check_list("hidden_skills", &settings.hidden_skills)?;
    if let Some(list) = &settings.enabled_mcp_servers {
        check_list("enabled_mcp_servers", list)?;
    }
    check_text("default_provider", settings.default_provider.as_deref())?;
    check_text("default_model", settings.default_model.as_deref())?;
    check_text("working_folder", settings.working_folder.as_deref())?;
    if let Some(folder) = settings.working_folder.as_deref()
        && !(folder.starts_with('/') || folder.starts_with("~/"))
    {
        return Err(OrchestratorError::invalid(
            "working_folder must be an absolute path or start with ~/",
        ));
    }
    if let Some(memory) = &settings.supermemory {
        check_text("supermemory.container", Some(&memory.container))?;
        check_list("supermemory.read_containers", &memory.read_containers)?;
        if memory
            .instructions
            .as_deref()
            .is_some_and(|v| v.len() > MAX_TEXT_BYTES)
        {
            return Err(OrchestratorError::invalid(
                "supermemory.instructions is limited to 4 KiB",
            ));
        }
    }
    Ok(())
}

const PROFILE_SELECT: &str = "SELECT p.profile_id, p.settings, p.created_at, p.updated_at, \
     (SELECT MAX(COALESCE(t.last_user_at, t.created_at)) FROM orch_threads t \
       WHERE t.tenant_id = p.tenant_id AND t.user_id = p.user_id \
         AND COALESCE(t.profile_id, 'default') = p.profile_id), \
     (SELECT COUNT(*) FROM orch_threads t \
       WHERE t.tenant_id = p.tenant_id AND t.user_id = p.user_id \
         AND COALESCE(t.profile_id, 'default') = p.profile_id) \
     FROM orch_profiles p WHERE p.tenant_id = ?1 AND p.user_id = ?2";

fn profile_row(row: &libsql::Row) -> Result<ProfileRecord> {
    let mut c = Cols::new(row);
    Ok(ProfileRecord {
        profile_id: ProfileId::new(c.text()?)?,
        settings: serde_json::from_str(&c.text()?)?,
        created_at: c.int()?,
        updated_at: c.int()?,
        last_used_at: c.oint()?,
        thread_count: c.int()?.max(0) as u64,
    })
}

/// Most recently used first; never-used profiles after, by name.
fn sort_by_last_use(list: &mut [ProfileRecord]) {
    list.sort_by(|a, b| {
        b.last_used_at.cmp(&a.last_used_at).then_with(|| {
            a.settings
                .display_name
                .to_lowercase()
                .cmp(&b.settings.display_name.to_lowercase())
        })
    });
}

fn owner_params(owner: &ProfileOwner) -> Vec<libsql::Value> {
    vec![s(owner.tenant_id.as_str()), s(owner.user_id.as_str())]
}

impl Store {
    /// Active profiles of `owner`, most recently used first. Creates the
    /// default profile on first read.
    pub async fn profiles(&self, owner: &ProfileOwner) -> Result<Vec<ProfileRecord>> {
        self.ensure_default_profile(owner).await?;
        let mut list = self
            .rows(
                &format!("{PROFILE_SELECT} AND p.archived_at IS NULL"),
                owner_params(owner),
                profile_row,
            )
            .await?;
        sort_by_last_use(&mut list);
        Ok(list)
    }

    /// One profile, archived ones included when `include_archived`. Read-only:
    /// the default row appears after [`Self::ensure_default_profile`].
    pub async fn profile(
        &self,
        owner: &ProfileOwner,
        id: &ProfileId,
        include_archived: bool,
    ) -> Result<Option<ProfileRecord>> {
        let mut params = owner_params(owner);
        params.push(s(id.as_str()));
        let archived = if include_archived {
            ""
        } else {
            " AND p.archived_at IS NULL"
        };
        self.row(
            &format!("{PROFILE_SELECT} AND p.profile_id = ?3{archived}"),
            params,
            profile_row,
        )
        .await
    }

    pub async fn ensure_default_profile(&self, owner: &ProfileOwner) -> Result<()> {
        let settings = ProfileSettings {
            display_name: "Default".to_string(),
            ..ProfileSettings::default()
        };
        let now = now_ms();
        let mut params = owner_params(owner);
        params.extend([
            s(DEFAULT_PROFILE_ID),
            s(serde_json::to_string(&settings)?),
            now.into(),
            now.into(),
        ]);
        self.exec(
            "INSERT OR IGNORE INTO orch_profiles \
             (tenant_id, user_id, profile_id, settings, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params,
        )
        .await?;
        Ok(())
    }

    /// Create a profile. An archived profile with the same id comes back
    /// with the new settings; an active one is a conflict.
    pub async fn create_profile(
        &self,
        owner: &ProfileOwner,
        id: &ProfileId,
        settings: &ProfileSettings,
    ) -> Result<ProfileRecord> {
        validate_settings(settings)?;
        let now = now_ms();
        let mut params = owner_params(owner);
        params.extend([
            s(id.as_str()),
            s(serde_json::to_string(settings)?),
            now.into(),
        ]);
        let written = self
            .exec(
                "INSERT INTO orch_profiles \
                 (tenant_id, user_id, profile_id, settings, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
                 ON CONFLICT (tenant_id, user_id, profile_id) DO UPDATE SET \
                 settings = excluded.settings, updated_at = excluded.updated_at, archived_at = NULL \
                 WHERE orch_profiles.archived_at IS NOT NULL",
                params,
            )
            .await?;
        if written == 0 {
            return Err(OrchestratorError::request(
                "profile_exists",
                format!("A profile with id '{id}' already exists."),
            ));
        }
        self.require_profile(owner, id).await
    }

    pub async fn update_profile(
        &self,
        owner: &ProfileOwner,
        id: &ProfileId,
        settings: &ProfileSettings,
    ) -> Result<ProfileRecord> {
        validate_settings(settings)?;
        self.ensure_default_profile(owner).await?;
        let mut params = owner_params(owner);
        params.extend([
            s(id.as_str()),
            s(serde_json::to_string(settings)?),
            now_ms().into(),
        ]);
        let written = self
            .exec(
                "UPDATE orch_profiles SET settings = ?4, updated_at = ?5 \
                 WHERE tenant_id = ?1 AND user_id = ?2 AND profile_id = ?3 AND archived_at IS NULL",
                params,
            )
            .await?;
        if written == 0 {
            return Err(profile_not_found(id));
        }
        self.require_profile(owner, id).await
    }

    /// Archive a profile. The default profile stays.
    pub async fn archive_profile(&self, owner: &ProfileOwner, id: &ProfileId) -> Result<()> {
        if id.as_str() == DEFAULT_PROFILE_ID {
            return Err(OrchestratorError::request(
                "profile_protected",
                "The default profile cannot be deleted.",
            ));
        }
        let mut params = owner_params(owner);
        params.extend([s(id.as_str()), now_ms().into()]);
        let written = self
            .exec(
                "UPDATE orch_profiles SET archived_at = ?4, updated_at = ?4 \
                 WHERE tenant_id = ?1 AND user_id = ?2 AND profile_id = ?3 AND archived_at IS NULL",
                params,
            )
            .await?;
        if written == 0 {
            return Err(profile_not_found(id));
        }
        Ok(())
    }

    /// Assign a profile to a thread that has none yet. A thread keeps its
    /// profile for life, so an assigned thread is left alone (`false`).
    pub async fn set_thread_profile(&self, thread: &ThreadId, id: &ProfileId) -> Result<bool> {
        let written = self
            .exec(
                "UPDATE orch_threads SET profile_id = ?2, updated_at = ?3 \
                 WHERE thread_id = ?1 AND profile_id IS NULL",
                vec![s(thread.as_str()), s(id.as_str()), now_ms().into()],
            )
            .await?;
        Ok(written > 0)
    }

    /// The profile a thread runs as: its own (even when archived), else the
    /// owner's default. `None` when the thread is not registered.
    pub async fn thread_profile(&self, thread: &ThreadId) -> Result<Option<ProfileRecord>> {
        let Some(record) = self.thread(thread).await? else {
            return Ok(None);
        };
        let owner = ProfileOwner::from(&record.scope);
        let id = match record.profile_id {
            Some(id) => id,
            None => default_profile_id()?,
        };
        self.profile(&owner, &id, true).await
    }

    async fn require_profile(&self, owner: &ProfileOwner, id: &ProfileId) -> Result<ProfileRecord> {
        self.profile(owner, id, false)
            .await?
            .ok_or_else(|| OrchestratorError::Store("profile row vanished after write".into()))
    }
}

pub fn profile_not_found(id: &ProfileId) -> OrchestratorError {
    OrchestratorError::not_found("profile_not_found", format!("No profile {id}."))
}
