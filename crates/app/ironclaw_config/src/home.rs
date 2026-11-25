use std::{
    env, error,
    ffi::OsString,
    fmt, io,
    path::{Component, Path, PathBuf},
};

use crate::home_migration::{HomeMigration, current_data_location, migrate_legacy_home};
use crate::profile::RebornProfile;

/// Environment variable that selects the ZLO state root (default `~/.zlo`).
pub const REBORN_HOME_ENV: &str = "ZLO_HOME";

/// Pre-ZLO name of [`REBORN_HOME_ENV`]; still honored when `ZLO_HOME` is
/// unset so existing launchd plists, systemd units and scripts keep working.
pub const LEGACY_REBORN_HOME_ENV: &str = "IRONCLAW_REBORN_HOME";

/// Folder under the user's home directory that holds the ZLO state root.
pub const DEFAULT_HOME_DIR_NAME: &str = ".zlo";

/// The desktop app's own log, relative to the user's home directory
/// (macOS). Written by the ZLO app, read by `zlo triage`.
pub const DESKTOP_APP_LOG_RELATIVE_PATH: &str = "Library/Logs/ZLO/rune.log";

/// File in the state root holding the pid of the backend the desktop app
/// started (`zlo-backend serve`); present only while that backend runs.
pub const DESKTOP_BACKEND_PID_FILE: &str = "zlo-backend.pid";

/// Folder in the state root holding triage sessions
/// ([`RebornHome::triage_sessions_dir`]).
pub const TRIAGE_SESSIONS_DIR: &str = "triage";

const V1_BASE_DIR_ENV: &str = "IRONCLAW_BASE_DIR";

/// Source used to resolve [`RebornHome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebornHomeSource {
    /// `ZLO_HOME`.
    Env,
    /// The legacy `IRONCLAW_REBORN_HOME` alias.
    LegacyEnv,
    Default,
}

impl RebornHomeSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Env => REBORN_HOME_ENV,
            Self::LegacyEnv => LEGACY_REBORN_HOME_ENV,
            Self::Default => "default",
        }
    }
}

/// Resolved, validated state root for the standalone Reborn binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebornHome {
    path: PathBuf,
    source: RebornHomeSource,
    migration: HomeMigration,
}

impl RebornHome {
    /// Resolve the home from the process environment, moving a pre-ZLO
    /// `~/.ironclaw/reborn` to `~/.zlo` first when the default applies (see
    /// `home_migration.rs`). An explicit `ZLO_HOME`/`IRONCLAW_REBORN_HOME`
    /// is used as-is and never migrated.
    pub fn resolve_from_env() -> Result<Self, RebornConfigError> {
        let (explicit, explicit_source) = explicit_home(
            env::var_os(REBORN_HOME_ENV),
            env::var_os(LEGACY_REBORN_HOME_ENV),
        );
        Self::resolve_with_source(
            explicit,
            explicit_source,
            env::var_os("HOME"),
            env::var_os("USERPROFILE"),
            env::var_os(V1_BASE_DIR_ENV),
        )?
        .migrate_if_default()
    }

    /// [`Self::resolve_from_env`] without the first-run move: when the
    /// default applies and the pre-ZLO `~/.ironclaw/reborn` still holds the
    /// data, that folder is returned instead of an empty `~/.zlo`. Only for
    /// callers that read (a log viewer, connection discovery); a caller that
    /// writes or starts the backend must use [`Self::resolve_from_env`].
    pub fn resolve_from_env_without_migrating() -> Result<Self, RebornConfigError> {
        let (explicit, explicit_source) = explicit_home(
            env::var_os(REBORN_HOME_ENV),
            env::var_os(LEGACY_REBORN_HOME_ENV),
        );
        Ok(Self::resolve_with_source(
            explicit,
            explicit_source,
            env::var_os("HOME"),
            env::var_os("USERPROFILE"),
            env::var_os(V1_BASE_DIR_ENV),
        )?
        .locate_without_migrating())
    }

    /// [`Self::resolve_from_env_without_migrating`] with the environment
    /// passed in.
    pub fn resolve_from_env_parts_without_migrating(
        reborn_home: Option<OsString>,
        home: Option<OsString>,
        userprofile: Option<OsString>,
    ) -> Result<Self, RebornConfigError> {
        Ok(
            Self::resolve_from_env_parts(reborn_home, home, userprofile)?
                .locate_without_migrating(),
        )
    }

    fn locate_without_migrating(mut self) -> Self {
        if self.source == RebornHomeSource::Default
            && let Some(user_home) = self.path.parent()
        {
            self.path = current_data_location(user_home);
        }
        self
    }

    /// Pure resolution: `reborn_home` is treated as `ZLO_HOME`. Never
    /// touches the filesystem beyond the v1-root overlap probe.
    pub fn resolve_from_env_parts(
        reborn_home: Option<OsString>,
        home: Option<OsString>,
        userprofile: Option<OsString>,
    ) -> Result<Self, RebornConfigError> {
        Self::resolve_with_source(reborn_home, RebornHomeSource::Env, home, userprofile, None)
    }

    /// [`Self::resolve_from_env_parts`] plus the same legacy-home migration
    /// [`Self::resolve_from_env`] performs. This is the production path with
    /// the environment passed in, so tests can drive it against a temp home.
    pub fn resolve_from_env_parts_migrating(
        reborn_home: Option<OsString>,
        home: Option<OsString>,
        userprofile: Option<OsString>,
    ) -> Result<Self, RebornConfigError> {
        Self::resolve_from_env_parts(reborn_home, home, userprofile)?.migrate_if_default()
    }

    fn migrate_if_default(mut self) -> Result<Self, RebornConfigError> {
        if self.source != RebornHomeSource::Default {
            return Ok(self);
        }
        let Some(user_home) = self.path.parent() else {
            return Ok(self); // unreachable: default homes are `<HOME>/.zlo`
        };
        self.migration = migrate_legacy_home(user_home)?;
        Ok(self)
    }

    fn resolve_with_source(
        reborn_home: Option<OsString>,
        explicit_source: RebornHomeSource,
        home: Option<OsString>,
        userprofile: Option<OsString>,
        v1_base_dir: Option<OsString>,
    ) -> Result<Self, RebornConfigError> {
        if let Some(raw_home) = reborn_home {
            let name = explicit_source.label();
            validate_non_empty(&raw_home, name)?;
            let path = PathBuf::from(raw_home);
            validate_absolute(&path, name)?;
            validate_no_parent_components(&path, name)?;
            validate_not_root(&path, name)?;
            validate_not_v1_state_root(
                &path,
                name,
                home.as_ref(),
                userprofile.as_ref(),
                v1_base_dir.as_ref(),
            )?;
            return Ok(Self {
                path,
                source: explicit_source,
                migration: HomeMigration::NotNeeded,
            });
        }

        let mut first_error = None;
        for (raw_home, label) in [(home, "HOME"), (userprofile, "USERPROFILE")] {
            let Some(raw_home) = raw_home else {
                continue;
            };
            match default_home_from_candidate(raw_home, label) {
                Ok(path) => {
                    return Ok(Self {
                        path,
                        source: RebornHomeSource::Default,
                        migration: HomeMigration::NotNeeded,
                    });
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        Err(first_error.unwrap_or(RebornConfigError::MissingHome))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn into_path(self) -> PathBuf {
        self.path
    }

    pub fn source(&self) -> RebornHomeSource {
        self.source
    }

    pub fn source_label(&self) -> &'static str {
        self.source.label()
    }

    /// What resolution did about a pre-ZLO `~/.ironclaw/reborn`; callers
    /// surface [`HomeMigration::notice`] to the user.
    pub fn migration(&self) -> &HomeMigration {
        &self.migration
    }

    /// Absolute path of the operator-edited boot config TOML.
    ///
    /// `$IRONCLAW_REBORN_HOME/config.toml`. The file is **optional**:
    /// `RebornConfigFile::load` returns `Ok(None)` when it doesn't
    /// exist, and the runtime falls back to compiled-in defaults.
    pub fn config_file_path(&self) -> PathBuf {
        self.path.join("config.toml")
    }

    /// Absolute path of the user-overlay LLM provider catalog.
    ///
    /// `$IRONCLAW_REBORN_HOME/providers.json`. Same JSON shape as v1's
    /// `~/.ironclaw/providers.json` so the same editor tooling and
    /// operator muscle memory apply. The file is **optional**: when
    /// it's missing, the runtime uses only the compiled-in built-in
    /// provider definitions. Loading happens in the composition root
    /// via `ironclaw_llm::ProviderRegistry`; this accessor just hands
    /// out the path.
    pub fn providers_file_path(&self) -> PathBuf {
        self.path.join("providers.json")
    }

    /// Folder of the backend's logs: `serve.stdout.log` / `serve.stderr.log`
    /// from the desktop app's backend and the launchd service.
    pub fn logs_dir(&self) -> PathBuf {
        self.path.join("logs")
    }

    /// Folder of triage sessions (`zlo triage`, "Triage an issue"): one
    /// private folder per session holding its context and playbook prompt.
    pub fn triage_sessions_dir(&self) -> PathBuf {
        self.path.join(TRIAGE_SESSIONS_DIR)
    }

    /// The desktop app's backend pid file (see [`DESKTOP_BACKEND_PID_FILE`]).
    pub fn desktop_backend_pid_file_path(&self) -> PathBuf {
        self.path.join(DESKTOP_BACKEND_PID_FILE)
    }
}

/// `ZLO_HOME` wins; the legacy `IRONCLAW_REBORN_HOME` applies only when it
/// is unset.
fn explicit_home(
    zlo_home: Option<OsString>,
    legacy_home: Option<OsString>,
) -> (Option<OsString>, RebornHomeSource) {
    match zlo_home {
        Some(value) => (Some(value), RebornHomeSource::Env),
        None => (legacy_home, RebornHomeSource::LegacyEnv),
    }
}

fn validate_non_empty(value: &OsString, name: &'static str) -> Result<(), RebornConfigError> {
    if value.as_os_str().is_empty() {
        return Err(RebornConfigError::EmptyPath { name });
    }
    Ok(())
}

fn default_home_from_candidate(
    raw_home: OsString,
    label: &'static str,
) -> Result<PathBuf, RebornConfigError> {
    validate_non_empty(&raw_home, label)?;
    let path = PathBuf::from(raw_home);
    validate_absolute(&path, label)?;
    validate_no_parent_components(&path, label)?;
    validate_not_root(&path, label)?;
    Ok(path.join(DEFAULT_HOME_DIR_NAME))
}

fn validate_absolute(path: &Path, name: &'static str) -> Result<(), RebornConfigError> {
    if !path.is_absolute() {
        return Err(RebornConfigError::RelativePath {
            name,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_no_parent_components(path: &Path, name: &'static str) -> Result<(), RebornConfigError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(RebornConfigError::ParentPath {
            name,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_not_v1_state_root(
    path: &Path,
    name: &'static str,
    home: Option<&OsString>,
    userprofile: Option<&OsString>,
    v1_base_dir: Option<&OsString>,
) -> Result<(), RebornConfigError> {
    let home_candidate = home.and_then(default_v1_state_root_from_home);
    let userprofile_candidate = userprofile.and_then(default_v1_state_root_from_home);
    let explicit_base_candidate = v1_base_dir.and_then(v1_state_root_from_base_dir);

    for candidate in [
        home_candidate.as_deref(),
        userprofile_candidate.as_deref(),
        explicit_base_candidate.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if paths_overlap(path, candidate) {
            return Err(RebornConfigError::V1StateRoot {
                name,
                path: path.to_path_buf(),
            });
        }
    }

    Ok(())
}

fn default_v1_state_root_from_home(raw_home: &OsString) -> Option<PathBuf> {
    validated_absolute_candidate(raw_home).map(|path| path.join(".ironclaw"))
}

fn v1_state_root_from_base_dir(raw_base_dir: &OsString) -> Option<PathBuf> {
    if raw_base_dir.as_os_str().is_empty() {
        return None;
    }
    let path = PathBuf::from(raw_base_dir);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    if path.is_absolute() {
        return Some(path);
    }
    env::current_dir().ok().map(|cwd| cwd.join(path))
}

fn validated_absolute_candidate(raw_path: &OsString) -> Option<PathBuf> {
    if raw_path.as_os_str().is_empty() {
        return None;
    }
    let path = PathBuf::from(raw_path);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    Some(path)
}

fn paths_overlap(path: &Path, candidate: &Path) -> bool {
    if path == candidate {
        return true;
    }
    if existing_canonical_pair(path, candidate).is_some_and(|(path, candidate)| path == candidate) {
        return true;
    }
    match (
        normalize_existing_prefix(path),
        normalize_existing_prefix(candidate),
    ) {
        (Some(path), Some(candidate)) => path == candidate,
        _ => false,
    }
}

fn existing_canonical_pair(path: &Path, candidate: &Path) -> Option<(PathBuf, PathBuf)> {
    match (path.canonicalize(), candidate.canonicalize()) {
        (Ok(path), Ok(candidate)) => Some((path, candidate)),
        (Err(error), _) | (_, Err(error)) if missing_path_error(&error) => None,
        _ => None,
    }
}

fn normalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if let Ok(canonical_ancestor) = ancestor.canonicalize() {
            let suffix = path.strip_prefix(ancestor).ok()?;
            return Some(canonical_ancestor.join(suffix));
        }
    }
    None
}

fn missing_path_error(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound)
}

fn validate_not_root(path: &Path, name: &'static str) -> Result<(), RebornConfigError> {
    if path.parent().is_none() {
        return Err(RebornConfigError::RootPath {
            name,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Error returned when standalone Reborn boot configuration is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebornConfigError {
    EmptyPath {
        name: &'static str,
    },
    RelativePath {
        name: &'static str,
        path: PathBuf,
    },
    ParentPath {
        name: &'static str,
        path: PathBuf,
    },
    RootPath {
        name: &'static str,
        path: PathBuf,
    },
    V1StateRoot {
        name: &'static str,
        path: PathBuf,
    },
    MissingHome,
    InvalidProfile {
        name: &'static str,
        value: String,
    },
    /// Moving the pre-ZLO `~/.ironclaw/reborn` to `~/.zlo` failed. ZLO
    /// refuses to start on an empty home while the data sits elsewhere.
    HomeMigration {
        from: PathBuf,
        to: PathBuf,
        reason: String,
    },
}

impl fmt::Display for RebornConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath { name } => write!(formatter, "{name} must not be empty"),
            Self::RelativePath { name, .. } => write!(formatter, "{name} must be an absolute path"),
            Self::ParentPath { name, .. } => {
                write!(
                    formatter,
                    "{name} must not contain parent directory components"
                )
            }
            Self::RootPath { name, .. } => write!(formatter, "{name} must not be filesystem root"),
            Self::V1StateRoot { name, .. } => {
                write!(
                    formatter,
                    "{name} must not point at the v1 IronClaw state root"
                )
            }
            Self::MissingHome => write!(
                formatter,
                "HOME or USERPROFILE must be set when {REBORN_HOME_ENV} is unset"
            ),
            Self::InvalidProfile { name, value } => {
                write!(formatter, "{name} must be one of ")?;
                for (index, profile) in RebornProfile::all().iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    formatter.write_str(profile.as_str())?;
                }
                write!(formatter, "; got {value:?}")
            }
            Self::HomeMigration { from, to, reason } => write!(
                formatter,
                "could not move the IronClaw data folder {} to the ZLO data folder {}: {reason}. \
                 Nothing was deleted. Move it by hand (`mv {} {} && ln -s {} {}`), or set \
                 {REBORN_HOME_ENV}={} to keep using it where it is.",
                from.display(),
                to.display(),
                from.display(),
                to.display(),
                to.display(),
                from.display(),
                from.display(),
            ),
        }
    }
}

impl error::Error for RebornConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zlo_home_wins_over_the_legacy_alias() {
        let (value, source) = explicit_home(Some("/a".into()), Some("/b".into()));
        assert_eq!(value, Some(OsString::from("/a")));
        assert_eq!(source, RebornHomeSource::Env);
    }

    #[test]
    fn legacy_alias_is_honored_and_named_in_errors() {
        let (value, source) = explicit_home(None, Some("relative".into()));
        assert_eq!(source, RebornHomeSource::LegacyEnv);
        let error = RebornHome::resolve_with_source(value, source, None, None, None)
            .expect_err("relative path rejected");
        assert_eq!(
            error,
            RebornConfigError::RelativePath {
                name: LEGACY_REBORN_HOME_ENV,
                path: PathBuf::from("relative"),
            }
        );

        let temp = tempfile::tempdir().expect("tempdir");
        let home = RebornHome::resolve_with_source(
            Some(temp.path().join("old").into_os_string()),
            RebornHomeSource::LegacyEnv,
            None,
            None,
            None,
        )
        .expect("absolute legacy home resolves");
        assert_eq!(home.source_label(), LEGACY_REBORN_HOME_ENV);
        assert_eq!(home.path(), temp.path().join("old"));
    }

    /// `install.sh` and the app's `zlo` link live in `<home>/bin`. They ask
    /// the resolver for the home first (`zlo config path`, which migrates)
    /// and write the link only into what it returns: a `~/.zlo/bin` made
    /// before the move would leave the pre-ZLO data stranded behind an empty
    /// `~/.zlo` (BothExist).
    #[cfg(unix)]
    #[test]
    fn the_command_link_is_written_after_the_home_migrates_so_legacy_data_moves() {
        let user = tempfile::tempdir().expect("user home");
        let legacy = crate::legacy_home_path(user.path());
        std::fs::create_dir_all(legacy.join("threads")).expect("legacy home");
        std::fs::write(legacy.join("config.toml"), "[update]\n").expect("legacy config");
        let resolve = || {
            RebornHome::resolve_from_env_parts_migrating(
                None,
                Some(user.path().as_os_str().to_owned()),
                None,
            )
            .expect("resolves")
        };

        // The install step: resolve (and migrate), then link into its bin/.
        let home = resolve();
        assert_eq!(home.path(), user.path().join(".zlo"));
        assert!(matches!(home.migration(), HomeMigration::Migrated { .. }));
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        std::os::unix::fs::symlink(
            "/Applications/ZLO.app/Contents/MacOS/zlo-backend",
            bin.join("zlo"),
        )
        .expect("link");

        // The data moved with it, the legacy path follows, and the next start
        // sees one settled home rather than two.
        assert!(home.path().join("config.toml").is_file());
        assert!(home.path().join("threads").is_dir());
        assert!(
            std::fs::symlink_metadata(&legacy)
                .expect("legacy")
                .file_type()
                .is_symlink()
        );
        assert_eq!(resolve().migration(), &HomeMigration::NotNeeded);

        // The order matters: a bin/ made before resolving is what strands it.
        let other = tempfile::tempdir().expect("user home");
        let legacy = crate::legacy_home_path(other.path());
        std::fs::create_dir_all(&legacy).expect("legacy home");
        std::fs::write(legacy.join("config.toml"), "").expect("legacy config");
        std::fs::create_dir_all(other.path().join(".zlo/bin")).expect("early bin");
        let early = RebornHome::resolve_from_env_parts_migrating(
            None,
            Some(other.path().as_os_str().to_owned()),
            None,
        )
        .expect("resolves");
        assert!(matches!(early.migration(), HomeMigration::BothExist { .. }));
    }
}
