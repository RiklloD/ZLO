//! Stage 3 profiles on the product side: the `profiles.*` views and
//! commands answered by the orchestrating decorator, the one-time Hermes
//! importer, and the two turn decorators that make a profile matter at run
//! time: its SOUL.md joins the identity context, and its capability allowlist
//! and enabled MCP servers narrow the visible surface. Records live in `ironclaw_orchestrator`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::capability_surface::CapabilitySurfacePolicy;
use ironclaw_host_api::ids::{CapabilityId, ExtensionId, ThreadId};
use ironclaw_loop_contracts::{LoopRunContext, PromptMode};
use ironclaw_loop_host::{
    CapabilityResolveError, CapabilitySurfaceProfileResolver, HostIdentityContextBuildError,
    HostIdentityContextCandidate, HostIdentityContextSource, HostIdentityMessageContent,
    IdentityApplicability, IdentityFileName, identity_message_ref,
};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::model::ThreadScope;
use ironclaw_orchestrator::profiles::{
    ProfileOwner, ProfileRecord, parse_profile_slug, profile_not_found,
};
use ironclaw_product_contracts::inbox::GoalStatus;
use ironclaw_product_contracts::profiles::*;
use ironclaw_turns::LoopMessageRef;
use serde::Deserialize;

type Result<T> = std::result::Result<T, OrchestratorError>;

const SOUL_FILE: &str = "SOUL.md";
/// Hermes profiles that never come over.
const HERMES_EXCLUDED: &[&str] = &["hermes-builder"];

/// Where profile folders and the Hermes source live on this machine.
#[derive(Debug, Clone)]
pub struct ProfileHome {
    user_home: PathBuf,
}

impl ProfileHome {
    pub fn new(user_home: PathBuf) -> Self {
        Self { user_home }
    }

    /// `$HOME`, when set.
    pub fn from_env() -> Option<Self> {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(|v| Self::new(PathBuf::from(v)))
    }

    fn expand(&self, folder: &str) -> PathBuf {
        match folder.strip_prefix("~/") {
            Some(rest) => self.user_home.join(rest),
            None => PathBuf::from(folder),
        }
    }

    /// The folder CLI agents run in: the configured one or
    /// `~/.ironclaw/profiles/<id>/`.
    pub fn folder(&self, record: &ProfileRecord) -> PathBuf {
        match record.settings.working_folder.as_deref() {
            Some(folder) => self.expand(folder),
            None => self
                .user_home
                .join(".ironclaw/profiles")
                .join(record.profile_id.as_str()),
        }
    }

    /// Where each profile's skills are mirrored for CLI agents.
    pub(crate) fn cli_skills(&self) -> PathBuf {
        self.user_home.join(".ironclaw/cli-skills")
    }

    fn hermes_profiles(&self) -> PathBuf {
        self.user_home.join(".hermes/profiles")
    }
}

/// Current branch of a git checkout (short sha when detached).
fn git_branch(folder: &Path) -> Option<String> {
    let head = std::fs::read_to_string(folder.join(".git/HEAD")).ok()?; // silent-ok: not a git repo is the common case
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None => head.get(..7).map(str::to_string),
    }
}

pub(crate) fn info(home: &ProfileHome, record: ProfileRecord) -> ProfileInfo {
    let folder = home.folder(&record);
    ProfileInfo {
        is_default: record.is_default(),
        git_branch: git_branch(&folder),
        effective_folder: folder.to_string_lossy().into_owned(),
        profile_id: record.profile_id.to_string(),
        settings: record.settings,
        thread_count: record.thread_count,
        last_used_at: record.last_used_at,
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}

pub(crate) async fn list(
    engine: &Engine,
    home: &ProfileHome,
    scope: &ThreadScope,
) -> Result<ProfileListResponse> {
    let records = engine.store().profiles(&ProfileOwner::from(scope)).await?;
    let last_used_profile_id = records
        .first()
        .filter(|r| r.last_used_at.is_some())
        .map(|r| r.profile_id.to_string());
    Ok(ProfileListResponse {
        profiles: records.into_iter().map(|r| info(home, r)).collect(),
        last_used_profile_id,
    })
}

pub(crate) async fn get(
    engine: &Engine,
    home: &ProfileHome,
    scope: &ThreadScope,
    raw: &str,
) -> Result<ProfileInfo> {
    let owner = ProfileOwner::from(scope);
    let id = parse_profile_slug(raw)?;
    engine.store().ensure_default_profile(&owner).await?;
    let record = engine
        .store()
        .profile(&owner, &id, false)
        .await?
        .ok_or_else(|| profile_not_found(&id))?;
    Ok(info(home, record))
}

pub(crate) async fn write(
    engine: &Engine,
    home: &ProfileHome,
    scope: &ThreadScope,
    request: ProfileWriteRequest,
    create: bool,
) -> Result<ProfileInfo> {
    let owner = ProfileOwner::from(scope);
    let id = parse_profile_slug(&request.profile_id)?;
    engine
        .check_profile_defaults(&request.settings, scope)
        .await?;
    let record = if create {
        engine
            .store()
            .create_profile(&owner, &id, &request.settings)
            .await?
    } else {
        engine
            .store()
            .update_profile(&owner, &id, &request.settings)
            .await?
    };
    if record.settings.working_folder.is_none() {
        let folder = home.folder(&record);
        if let Err(error) = std::fs::create_dir_all(&folder) {
            tracing::debug!(%error, "profile folder could not be created");
        }
    }
    Ok(info(home, record))
}

pub(crate) async fn delete(engine: &Engine, scope: &ThreadScope, raw: &str) -> Result<()> {
    let id = parse_profile_slug(raw)?;
    engine
        .store()
        .archive_profile(&ProfileOwner::from(scope), &id)
        .await
}

/// The profile a new thread joins, validated before the thread exists.
pub(crate) async fn require_active(
    engine: &Engine,
    scope: &ThreadScope,
    raw: &str,
) -> Result<ironclaw_orchestrator::model::ProfileId> {
    let owner = ProfileOwner::from(scope);
    let id = parse_profile_slug(raw)?;
    engine.store().ensure_default_profile(&owner).await?;
    match engine.store().profile(&owner, &id, false).await? {
        Some(record) => Ok(record.profile_id),
        None => Err(profile_not_found(&id)),
    }
}

/// Default model of the thread's profile, for submits that pick none.
pub(crate) async fn default_model(engine: &Engine, thread: &ThreadId) -> Result<Option<String>> {
    Ok(engine
        .store()
        .thread_profile(thread)
        .await?
        .and_then(|p| p.settings.default_model))
}

// ── Hermes import ───────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct HermesConfig {
    #[serde(default)]
    model: Option<HermesModel>,
}

#[derive(Debug, Default, Deserialize)]
struct HermesModel {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HermesSupermemory {
    #[serde(default)]
    container_tag: Option<String>,
    #[serde(default)]
    enable_custom_container_tags: bool,
    #[serde(default)]
    custom_containers: Vec<String>,
    #[serde(default)]
    custom_container_instructions: Option<String>,
}

/// Hermes name → (IronClaw id, display name, working folder).
fn hermes_identity(name: &str) -> (String, String, Option<String>) {
    if name == "ironclaw" {
        return (
            "ironclaw-builder".into(),
            "IronClaw Builder".into(),
            Some("~/ironclaw".into()),
        );
    }
    let display = name
        .split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ");
    (name.to_string(), display, None)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Hermes provider and model ids as IronClaw names them: `openai-codex` is
/// IronClaw's own loop on the ChatGPT plan, `zai` is `glm`, and the `-900k`
/// context variants run as their base model.
fn hermes_route(
    provider: Option<String>,
    model: Option<String>,
) -> (Option<String>, Option<String>) {
    let provider = provider.map(|p| match p.as_str() {
        "openai-codex" => "ironclaw".to_string(),
        "zai" => "glm".to_string(),
        _ => p,
    });
    let model = model.map(|m| match m.strip_suffix("-900k") {
        Some(base) => base.to_string(),
        None => m,
    });
    (provider, model)
}

/// Read one Hermes profile directory into settings. Never writes there.
fn read_hermes_profile(
    dir: &Path,
    display_name: String,
    folder: Option<String>,
) -> std::result::Result<ProfileSettings, String> {
    let soul = match std::fs::read_to_string(dir.join(SOUL_FILE)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("SOUL.md unreadable: {error}")),
    };
    let config: HermesConfig = match std::fs::read_to_string(dir.join("config.yaml")) {
        Ok(text) => serde_yaml::from_str(&text).map_err(|e| format!("config.yaml: {e}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => HermesConfig::default(),
        Err(error) => return Err(format!("config.yaml unreadable: {error}")),
    };
    let memory: Option<HermesSupermemory> =
        match std::fs::read_to_string(dir.join("supermemory.json")) {
            Ok(text) => {
                Some(serde_json::from_str(&text).map_err(|e| format!("supermemory.json: {e}"))?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("supermemory.json unreadable: {error}")),
        };
    let model = config.model.unwrap_or_default();
    let (default_provider, default_model) =
        hermes_route(non_empty(model.provider), non_empty(model.default));
    let supermemory = memory.and_then(|m| {
        let container = non_empty(m.container_tag)?;
        let custom = m.enable_custom_container_tags;
        Some(ProfileSupermemory {
            read_containers: if custom {
                m.custom_containers
                    .into_iter()
                    .filter(|c| !c.trim().is_empty() && *c != container)
                    .collect()
            } else {
                Vec::new()
            },
            instructions: if custom {
                non_empty(m.custom_container_instructions)
            } else {
                None
            },
            container,
        })
    });
    Ok(ProfileSettings {
        display_name,
        soul,
        default_provider,
        default_model,
        working_folder: folder,
        supermemory,
        ..ProfileSettings::default()
    })
}

/// Copy every Hermes profile (but `hermes-builder`) that is not already
/// here. Existing profiles, archived ones included, are left untouched, so a
/// second run imports nothing.
pub(crate) async fn import_hermes(
    engine: &Engine,
    home: &ProfileHome,
    scope: &ThreadScope,
) -> Result<HermesImportResponse> {
    let owner = ProfileOwner::from(scope);
    let root = home.hermes_profiles();
    let entries = std::fs::read_dir(&root).map_err(|error| {
        tracing::debug!(%error, "hermes profiles folder unreadable");
        OrchestratorError::request(
            "hermes_not_found",
            "No Hermes profiles found in ~/.hermes/profiles.",
        )
    })?;
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok()) // silent-ok: an unreadable entry is reported as absent
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    let mut out = HermesImportResponse::default();
    let mut skip = |source: &str, reason: String| {
        out.skipped.push(ProfileImportSkipped {
            source: source.to_string(),
            reason,
        })
    };
    let mut imported = Vec::new();
    for name in &names {
        if HERMES_EXCLUDED.contains(&name.as_str()) {
            skip(name, "excluded".into());
            continue;
        }
        let (raw_id, display, folder) = hermes_identity(name);
        let Ok(id) = parse_profile_slug(&raw_id) else {
            skip(name, "name is not a valid profile id".into());
            continue;
        };
        if engine.store().profile(&owner, &id, true).await?.is_some() {
            skip(name, format!("profile '{id}' already exists"));
            continue;
        }
        let settings = match read_hermes_profile(&root.join(name), display, folder) {
            Ok(settings) => settings,
            Err(reason) => {
                skip(name, reason);
                continue;
            }
        };
        match engine.store().create_profile(&owner, &id, &settings).await {
            Ok(_) => imported.push(id.to_string()),
            Err(error) if error.is_correctable() => skip(name, error.to_string()),
            Err(error) => return Err(error),
        }
    }
    out.imported = imported;
    Ok(out)
}

// ── turn decorators ─────────────────────────────────────────────────────

/// Adds the thread's profile SOUL.md after the inner identity files.
pub struct ProfileIdentityContextSource {
    inner: Arc<dyn HostIdentityContextSource>,
    engine: Arc<Engine>,
}

impl ProfileIdentityContextSource {
    pub fn new(inner: Arc<dyn HostIdentityContextSource>, engine: Arc<Engine>) -> Self {
        Self { inner, engine }
    }

    /// The thread's SOUL.md and its message ref, when it has one. The
    /// thread's active goal rides in the same file, after the soul, so the
    /// model sees it on every turn without a second identity file name.
    async fn soul(
        &self,
        thread: &ThreadId,
    ) -> std::result::Result<
        Option<(IdentityFileName, LoopMessageRef, String)>,
        HostIdentityContextBuildError,
    > {
        let store = self.engine.store();
        let soul = match store.thread_profile(thread).await {
            Ok(profile) => profile.map(|p| p.settings.soul),
            Err(error) => {
                tracing::debug!(%error, "profile lookup failed; turn runs without SOUL.md");
                None // silent-ok: SYSTEM.md still applies; the soul is additive
            }
        };
        let goal = match store.goal(thread).await {
            Ok(goal) => goal.filter(|g| g.status == GoalStatus::Active),
            Err(error) => {
                tracing::debug!(%error, "goal lookup failed; turn runs without its goal");
                None // silent-ok: the goal is additive context, the turn still runs
            }
        };
        let Some(content) = profile_context(soul.as_deref(), goal.map(|g| g.objective).as_deref())
        else {
            return Ok(None);
        };
        let name = IdentityFileName::new(SOUL_FILE)?;
        let message_ref = identity_message_ref(&name, &content)
            .map_err(|_error| HostIdentityContextBuildError::Internal)?;
        Ok(Some((name, message_ref, content)))
    }
}

/// SOUL.md as the turn sees it: the profile soul, then the thread goal.
fn profile_context(soul: Option<&str>, goal: Option<&str>) -> Option<String> {
    let soul = soul.map(str::trim).filter(|s| !s.is_empty());
    let goal = goal.map(str::trim).filter(|g| !g.is_empty()).map(|g| {
        format!(
            "## Thread goal\n\nThe user set this goal for the thread; keep working toward it:\n{g}"
        )
    });
    match (soul, goal) {
        (None, None) => None,
        (Some(soul), None) => Some(soul.to_string()),
        (None, Some(goal)) => Some(goal),
        (Some(soul), Some(goal)) => Some(format!("{soul}\n\n{goal}")),
    }
}

#[async_trait]
impl HostIdentityContextSource for ProfileIdentityContextSource {
    async fn load_identity_candidates(
        &self,
        run_context: &LoopRunContext,
        mode: PromptMode,
    ) -> std::result::Result<Vec<HostIdentityContextCandidate>, HostIdentityContextBuildError> {
        let mut candidates = self
            .inner
            .load_identity_candidates(run_context, mode)
            .await?;
        if let Some((name, message_ref, soul)) = self.soul(&run_context.thread_id).await? {
            candidates.push(HostIdentityContextCandidate::new_trusted(
                name,
                message_ref,
                format!("profile identity file {SOUL_FILE} available"),
                IdentityApplicability::Always,
                soul.len(),
            ));
        }
        Ok(candidates)
    }

    async fn resolve_identity_message_content(
        &self,
        run_context: &LoopRunContext,
        message_ref: &LoopMessageRef,
    ) -> std::result::Result<Option<HostIdentityMessageContent>, HostIdentityContextBuildError>
    {
        if let Some(found) = self
            .inner
            .resolve_identity_message_content(run_context, message_ref)
            .await?
        {
            return Ok(Some(found));
        }
        Ok(self
            .soul(&run_context.thread_id)
            .await?
            .filter(|(_, r, _)| r == message_ref)
            .map(|(name, _, content)| HostIdentityMessageContent { name, content }))
    }
}

/// Lists the live MCP-runtime capabilities as `(server, capability)`; the
/// server is the capability's provider extension.
pub type McpCapabilityLister = Arc<dyn Fn() -> Vec<(ExtensionId, CapabilityId)> + Send + Sync>;

/// Narrows the inner surface to the thread profile's capability allowlist
/// and hides the tools of MCP servers the profile does not enable.
pub struct ProfileCapabilitySurfaceResolver {
    inner: Arc<dyn CapabilitySurfaceProfileResolver>,
    engine: Arc<Engine>,
    mcp_capabilities: McpCapabilityLister,
}

impl ProfileCapabilitySurfaceResolver {
    pub fn new(
        inner: Arc<dyn CapabilitySurfaceProfileResolver>,
        engine: Arc<Engine>,
        mcp_capabilities: McpCapabilityLister,
    ) -> Self {
        Self {
            inner,
            engine,
            mcp_capabilities,
        }
    }
}

#[async_trait]
impl CapabilitySurfaceProfileResolver for ProfileCapabilitySurfaceResolver {
    async fn resolve(
        &self,
        run_context: &LoopRunContext,
    ) -> std::result::Result<CapabilitySurfacePolicy, CapabilityResolveError> {
        let mut policy = self.inner.resolve(run_context).await?;
        // Fail closed: an unreadable profile must not widen the surface.
        let profile = self
            .engine
            .store()
            .thread_profile(&run_context.thread_id)
            .await
            .map_err(|error| CapabilityResolveError::unavailable(error.to_string()))?;
        let Some(settings) = profile.map(|p| p.settings) else {
            return Ok(policy);
        };
        if let Some(allowed) = &settings.allowed_capabilities {
            let ids = allowed
                .iter()
                .map(|raw| {
                    CapabilityId::new(raw.as_str())
                        .map_err(|error| CapabilityResolveError::unavailable(error.to_string()))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            policy = policy.narrow_to_capability_ids(ids);
        }
        if let Some(enabled) = &settings.enabled_mcp_servers {
            let denied = (self.mcp_capabilities)()
                .into_iter()
                .filter(|(server, _)| !enabled.iter().any(|e| e == server.as_str()))
                .map(|(_, id)| id);
            policy = policy.deny_capability_ids(denied);
        }
        Ok(policy)
    }
}

/// Wrap the runtime's identity source with profile SOUL.md injection.
pub fn profile_identity_source(
    inner: Arc<dyn HostIdentityContextSource>,
    engine: &Arc<Engine>,
) -> Arc<dyn HostIdentityContextSource> {
    Arc::new(ProfileIdentityContextSource::new(inner, Arc::clone(engine)))
}

/// Wrap the runtime's capability surface with profile allowlists.
pub fn profile_surface_resolver(
    inner: Arc<dyn CapabilitySurfaceProfileResolver>,
    engine: &Arc<Engine>,
    mcp_capabilities: McpCapabilityLister,
) -> Arc<dyn CapabilitySurfaceProfileResolver> {
    Arc::new(ProfileCapabilitySurfaceResolver::new(
        inner,
        Arc::clone(engine),
        mcp_capabilities,
    ))
}

/// Supermemory containers per profile: the `supermemory` block of the
/// caller's thread profile, else the env binding.
pub struct ProfileSupermemoryBindings {
    engine: Arc<Engine>,
    fallback: Arc<dyn ironclaw_supermemory::SupermemoryBindingSource>,
}

impl ProfileSupermemoryBindings {
    pub fn new(
        engine: Arc<Engine>,
        fallback: Arc<dyn ironclaw_supermemory::SupermemoryBindingSource>,
    ) -> Self {
        Self { engine, fallback }
    }
}

#[async_trait]
impl ironclaw_supermemory::SupermemoryBindingSource for ProfileSupermemoryBindings {
    async fn binding_for(
        &self,
        caller: &ironclaw_supermemory::SupermemoryCaller,
    ) -> std::result::Result<
        Option<ironclaw_supermemory::SupermemoryBinding>,
        ironclaw_supermemory::SupermemoryError,
    > {
        if let Some(thread) = &caller.thread_id {
            let profile = self
                .engine
                .store()
                .thread_profile(thread)
                .await
                .map_err(
                    |error| ironclaw_supermemory::SupermemoryError::InvalidConfig {
                        reason: format!("thread profile unreadable: {error}"),
                    },
                )?;
            if let Some(sm) = profile.and_then(|p| p.settings.supermemory)
                && !sm.container.trim().is_empty()
            {
                return Ok(Some(ironclaw_supermemory::SupermemoryBinding {
                    primary_container: sm.container,
                    read_containers: sm.read_containers,
                    instructions: sm.instructions,
                }));
            }
        }
        self.fallback.binding_for(caller).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoIdentityFiles;

    #[async_trait]
    impl HostIdentityContextSource for NoIdentityFiles {
        async fn load_identity_candidates(
            &self,
            _run_context: &LoopRunContext,
            _mode: PromptMode,
        ) -> std::result::Result<Vec<HostIdentityContextCandidate>, HostIdentityContextBuildError>
        {
            Ok(Vec::new())
        }

        async fn resolve_identity_message_content(
            &self,
            _run_context: &LoopRunContext,
            _message_ref: &LoopMessageRef,
        ) -> std::result::Result<Option<HostIdentityMessageContent>, HostIdentityContextBuildError>
        {
            Ok(None)
        }
    }

    /// The turn's identity context carries the active goal, and drops it
    /// once the goal is done.
    #[tokio::test]
    async fn active_goal_reaches_the_native_loop_prompt() {
        use ironclaw_host_api::ids::TenantId;
        use ironclaw_loop_contracts::{
            InMemoryRunProfileResolver, RunProfileResolutionRequest, RunProfileResolver,
        };
        use ironclaw_turns::{TurnId, TurnRunId, TurnScope};

        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime =
            Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        let thread = ThreadId::new("goal-thread").unwrap();
        let source = profile_identity_source(Arc::new(NoIdentityFiles), &engine);
        let profile = InMemoryRunProfileResolver::default()
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .unwrap();
        let run = LoopRunContext::new(
            TurnScope::new(TenantId::new("t1").unwrap(), None, None, thread.clone()),
            TurnId::new(),
            TurnRunId::new(),
            profile,
        );
        let context = || async {
            let candidates = source
                .load_identity_candidates(&run, PromptMode::TextOnly)
                .await
                .unwrap();
            let candidate = candidates.first()?;
            let message_ref = candidate.message_ref.clone().unwrap();
            source
                .resolve_identity_message_content(&run, &message_ref)
                .await
                .unwrap()
                .map(|c| c.content)
        };
        assert_eq!(context().await, None, "no soul and no goal: nothing added");

        engine
            .store()
            .set_goal(&thread, Some("Ship stage 5"), None, 1)
            .await
            .unwrap();
        let content = context()
            .await
            .expect("the goal is in the identity context");
        assert!(content.contains("Ship stage 5"), "{content}");

        engine
            .store()
            .set_goal(&thread, None, Some(GoalStatus::Done), 2)
            .await
            .unwrap();
        assert_eq!(context().await, None, "a done goal is not sent");
    }

    struct AllowAll;

    #[async_trait]
    impl CapabilitySurfaceProfileResolver for AllowAll {
        async fn resolve(
            &self,
            _run_context: &LoopRunContext,
        ) -> std::result::Result<CapabilitySurfacePolicy, CapabilityResolveError> {
            Ok(CapabilitySurfacePolicy::allow_all())
        }
    }

    /// A profile that enables some MCP servers loses the tools of the
    /// others; plain tools and profiles without the list are untouched.
    #[tokio::test]
    async fn profile_hides_tools_of_mcp_servers_it_does_not_enable() {
        use ironclaw_host_api::ids::{TenantId, UserId};
        use ironclaw_loop_contracts::{
            InMemoryRunProfileResolver, RunProfileResolutionRequest, RunProfileResolver,
        };
        use ironclaw_turns::{TurnId, TurnRunId, TurnScope};

        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime =
            Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        let scope = ThreadScope {
            tenant_id: TenantId::new("t1").unwrap(),
            user_id: UserId::new("u1").unwrap(),
            agent_id: None,
            project_id: None,
        };
        let owner = ProfileOwner::from(&scope);
        let work = parse_profile_slug("work").unwrap();
        let settings = ProfileSettings {
            display_name: "Work".into(),
            enabled_mcp_servers: Some(vec!["notion".into()]),
            ..ProfileSettings::default()
        };
        engine
            .store()
            .create_profile(&owner, &work, &settings)
            .await
            .unwrap();
        let (on_work, on_default) = (
            ThreadId::new("work-thread").unwrap(),
            ThreadId::new("default-thread").unwrap(),
        );
        engine.ensure_thread(&on_work, &scope, None).await.unwrap();
        engine
            .ensure_thread(&on_default, &scope, None)
            .await
            .unwrap();
        engine
            .store()
            .set_thread_profile(&on_work, &work)
            .await
            .unwrap();

        let id = |raw: &str| CapabilityId::new(raw).unwrap();
        let mcp: McpCapabilityLister = Arc::new(move || {
            vec![
                (ExtensionId::new("notion").unwrap(), id("notion.search")),
                (ExtensionId::new("linear").unwrap(), id("linear.issues")),
            ]
        });
        let resolver = profile_surface_resolver(Arc::new(AllowAll), &engine, mcp);
        let run_profile = InMemoryRunProfileResolver::default()
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .unwrap();
        let policy_for = |thread: &ThreadId| {
            let run = LoopRunContext::new(
                TurnScope::new(TenantId::new("t1").unwrap(), None, None, thread.clone()),
                TurnId::new(),
                TurnRunId::new(),
                run_profile.clone(),
            );
            let resolver = Arc::clone(&resolver);
            async move { resolver.resolve(&run).await.unwrap() }
        };

        let work_policy = policy_for(&on_work).await;
        assert!(work_policy.permits_capability_id(&id("notion.search")));
        assert!(!work_policy.permits_capability_id(&id("linear.issues")));
        assert!(work_policy.permits_capability_id(&id("builtin.shell")));

        let default_policy = policy_for(&on_default).await;
        assert!(default_policy.permits_capability_id(&id("linear.issues")));
    }

    #[test]
    fn hermes_routes_map_to_ironclaw_ids() {
        let route = |p: &str, m: &str| hermes_route(Some(p.into()), Some(m.into()));
        assert_eq!(
            route("openai-codex", "gpt-6.1-sol-900k"),
            (Some("ironclaw".into()), Some("gpt-6.1-sol".into()))
        );
        assert_eq!(
            route("zai", "glm-5.3"),
            (Some("glm".into()), Some("glm-5.3".into()))
        );
        assert_eq!(
            route("claude-cli-2", "claude-opus-5-5"),
            (Some("claude-cli-2".into()), Some("claude-opus-5-5".into()))
        );
    }

    #[test]
    fn hermes_names_map_to_profiles() {
        assert_eq!(
            hermes_identity("ironclaw"),
            (
                "ironclaw-builder".to_string(),
                "IronClaw Builder".to_string(),
                Some("~/ironclaw".to_string())
            )
        );
        assert_eq!(hermes_identity("financial-advisor").1, "Financial Advisor");
    }

    #[test]
    fn hermes_profile_reads_soul_model_and_memory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "be kind").unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "model:\n  default: claude-opus-5-5\n  provider: claude-cli-2\nplugins: {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("supermemory.json"),
            r#"{"container_tag":"finance","enable_custom_container_tags":true,"custom_containers":["hermes","finance"],"custom_container_instructions":"x"}"#,
        )
        .unwrap();
        let settings = read_hermes_profile(dir.path(), "Finance".into(), None).unwrap();
        assert_eq!(settings.soul, "be kind");
        assert_eq!(settings.default_model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(settings.default_provider.as_deref(), Some("claude-cli-2"));
        let memory = settings.supermemory.unwrap();
        assert_eq!(memory.container, "finance");
        assert_eq!(memory.read_containers, vec!["hermes".to_string()]);
        assert_eq!(memory.instructions.as_deref(), Some("x"));
    }
}
