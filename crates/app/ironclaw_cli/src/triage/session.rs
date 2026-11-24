//! Writes `context.md` and the playbook prompt into a new private
//! `<ZLO_HOME>/triage/<time>-<random>` folder (0700, files 0600, created
//! exclusively). Both `zlo triage` and the in-app entry start from these
//! files. Every path the context names comes from its owner: home
//! resolution (`ironclaw_config`), the standalone database and master-key
//! names (`ironclaw_composition::setup_transfer`), the WebUI token file, the
//! service module, the diagnostics bundle's log list, and each agent
//! runtime's folders (`ironclaw_cli_providers`, through
//! `ironclaw_composition::agent_runtime_paths`).

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use ironclaw_config::{DESKTOP_APP_LOG_RELATIVE_PATH, RebornHome, RebornProfile};
use ironclaw_product_contracts::triage::{TRIAGE_CONTEXT_FILE_NAME, TRIAGE_PROMPT_FILE_NAME};

use crate::dto::ServiceStateDto;
use crate::triage::prompt::{
    ContextInput, SourceClone, TriageEntry, TriagePaths, build_context, seed_prompt,
};

/// The coding-agent CLIs `zlo triage` launches, with where each keeps its
/// own state: `(label, env override, default folder under the user's home)`.
pub(crate) const CLI_STATE_DIRS: [(&str, &str, &str); 2] = [
    ("Claude Code (claude)", "CLAUDE_CONFIG_DIR", ".claude"),
    ("Antigravity CLI (agy)", "GEMINI_HOME", ".gemini"),
];

/// `env` override, else `default` under the user's home.
pub(crate) fn runtime_state_dir(user_home: &Path, env: &str, default: &str) -> PathBuf {
    if !env.is_empty()
        && let Some(value) = std::env::var_os(env).filter(|v| !v.is_empty())
    {
        return PathBuf::from(value);
    }
    user_home.join(default)
}

/// Every coding-agent runtime's own state, named by its owner
/// (`ironclaw_cli_providers`) where ZLO keeps it: the agent can check
/// harness health there.
fn runtime_state(user_home: &Path) -> Vec<(String, PathBuf)> {
    use ironclaw_composition::agent_runtime_paths as paths;
    let [
        (claude_label, claude_env, claude_dir),
        (agy_label, agy_env, agy_dir),
    ] = CLI_STATE_DIRS;
    vec![
        (
            claude_label.into(),
            runtime_state_dir(user_home, claude_env, claude_dir),
        ),
        ("Codex (codex)".into(), paths::codex_home(user_home)),
        (
            agy_label.into(),
            runtime_state_dir(user_home, agy_env, agy_dir),
        ),
        (
            "ZLO's Antigravity runtime".into(),
            paths::antigravity_state_dir(user_home),
        ),
        (
            "ZLO's Antigravity profile (may be the one Hermes signed in)".into(),
            paths::antigravity_profile_dir(user_home),
        ),
        (
            "ZLO's Muse Code runtime".into(),
            paths::muse_state_dir(user_home),
        ),
    ]
}

/// What the agent must never read: ZLO's own secrets and every agent
/// runtime's credentials (files and Keychain items).
fn secrets(state_dir: &Path, master_key: &Path, user_home: &Path) -> Vec<String> {
    use ironclaw_composition::agent_runtime_paths as paths;
    let show = |path: PathBuf| path.display().to_string();
    let claude = runtime_state_dir(user_home, "CLAUDE_CONFIG_DIR", ".claude");
    let gemini = runtime_state_dir(user_home, "GEMINI_HOME", ".gemini");
    vec![
        format!(
            "ZLO's WebUI token: {}",
            show(crate::webui_token::webui_token_file_path(state_dir))
        ),
        format!(
            "ZLO's secrets master key: {}",
            show(master_key.to_path_buf())
        ),
        "the ZLO items in the login Keychain, and the encrypted secret records in the database"
            .into(),
        format!(
            "Claude Code's sign-in: {} and the Keychain item `Claude Code-credentials`",
            show(claude.join(".credentials.json"))
        ),
        format!(
            "Codex's sign-in: {}",
            show(paths::codex_home(user_home).join("auth.json"))
        ),
        format!(
            "Antigravity's sign-in: {}, {}, {}, and the Keychain items `gemini` and \
             `antigravity`",
            show(gemini.join("oauth_creds.json")),
            show(paths::antigravity_sign_in_dir(&gemini)),
            show(paths::antigravity_sign_in_dir(
                &paths::antigravity_profile_dir(user_home)
            ))
        ),
        format!(
            "Muse Code's sign-in (what `muse login` writes) and any auth, token or credentials \
             file under {}",
            show(paths::muse_state_dir(user_home))
        ),
    ]
}

impl TriagePaths {
    pub(crate) fn derive(home: &RebornHome, profile: RebornProfile, user_home: &Path) -> Self {
        let state_dir = home.path().to_path_buf();
        let storage_root = state_dir.join(profile.local_runtime_storage_subdir());
        let database =
            ironclaw_composition::setup_transfer::standalone_database_file(&storage_root);
        let master_key =
            storage_root.join(ironclaw_composition::setup_transfer::SECRETS_MASTER_KEY_FILE);
        let update_log = crate::commands::update::UpdatePaths::new(&state_dir).log_file();
        Self {
            database,
            config_file: home.config_file_path(),
            providers_file: home.providers_file_path(),
            logs_dir: home.logs_dir(),
            backend_logs: crate::diagnostics_bundle::log_files(&state_dir)
                .into_iter()
                .filter(|log| *log != update_log)
                .collect(),
            update_log,
            desktop_app_log: cfg!(target_os = "macos")
                .then(|| user_home.join(DESKTOP_APP_LOG_RELATIVE_PATH)),
            desktop_backend_pid_file: home.desktop_backend_pid_file_path(),
            service: crate::commands::service::service_definition(),
            runtime_state: runtime_state(user_home),
            secrets: secrets(&state_dir, &master_key, user_home),
            source_cache_dir: crate::triage::source::source_cache_dir(&state_dir),
            sessions_dir: home.triage_sessions_dir(),
            state_dir,
        }
    }
}

/// What the desktop app's backend pid file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PidFileState {
    Absent,
    Alive(u32),
    Dead(u32),
    Unreadable(String),
}

/// Read the pid file and ask the OS whether that pid is a ZLO backend.
pub(crate) fn read_pid_file(path: &Path, alive: impl Fn(u32) -> bool) -> PidFileState {
    match std::fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PidFileState::Absent,
        Err(error) => PidFileState::Unreadable(error.to_string()),
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(pid) if alive(pid) => PidFileState::Alive(pid),
            Ok(pid) => PidFileState::Dead(pid),
            Err(error) => PidFileState::Unreadable(format!("not a pid ({error})")),
        },
    }
}

/// Whether `pid` runs a ZLO backend (`zlo`, `zlo-backend`, or a pre-ZLO
/// `ironclaw`), the same test the desktop app uses before stopping one.
pub(crate) fn is_zlo_process(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok() // silent-ok: no `ps` reads as "not running"; the line says so
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .is_some_and(|comm| comm.contains("zlo") || comm.contains("ironclaw"))
}

/// The "Backend process" line of a `zlo triage` context.
pub(crate) fn describe_backend(pid_file: &PidFileState, service: ServiceStateDto) -> String {
    let service = match service {
        ServiceStateDto::Running => "the `zlo service` job is running",
        ServiceStateDto::Stopped => "the `zlo service` job is installed but stopped",
        ServiceStateDto::NotInstalled => "no `zlo service` job is installed",
        ServiceStateDto::Unknown => "the `zlo service` state could not be read",
    };
    let app = match pid_file {
        PidFileState::Absent => {
            "no backend started by the desktop app (no pid file; the app may be closed, \
             or the backend runs another way)"
                .to_string()
        }
        PidFileState::Alive(pid) => format!("running (pid {pid}, started by the desktop app)"),
        PidFileState::Dead(pid) => format!(
            "not running (pid file is stale: pid {pid} is not a ZLO backend; the app or its \
             backend may have crashed)"
        ),
        PidFileState::Unreadable(reason) => format!("unknown (pid file unreadable: {reason})"),
    };
    format!(
        "{app}; {service}. Version and commit not recorded: ask a running backend at \
         <origin>/.well-known/ironclaw/environment"
    )
}

/// The OS, architecture and kernel release.
pub(crate) fn describe_os() -> String {
    let release = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok() // silent-ok: the release is decoration; OS and arch still print
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|r| !r.is_empty());
    match release {
        Some(release) => format!(
            "{} {} ({release})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        None => format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriageSession {
    pub(crate) session_dir: PathBuf,
    pub(crate) context_file: PathBuf,
    pub(crate) prompt_file: PathBuf,
}

pub(crate) struct SessionInput<'a> {
    pub(crate) paths: &'a TriagePaths,
    pub(crate) entry: &'a TriageEntry,
    pub(crate) server: String,
    pub(crate) source_clone: Option<&'a SourceClone>,
    pub(crate) diagnostics: &'a [String],
    pub(crate) now: DateTime<Utc>,
}

/// The sessions folder, created private, and refused when it is a symlink,
/// not a folder, or (unix) not this user's.
fn sessions_root(root: &Path) -> std::io::Result<()> {
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match private_dir_builder().create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let meta = std::fs::symlink_metadata(root)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is not a plain folder",
            root.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        // SAFETY: `geteuid` reads this process's effective uid; no memory.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me {
            return Err(std::io::Error::other(format!(
                "{} belongs to another user",
                root.display()
            )));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn private_dir_builder() -> std::fs::DirBuilder {
    #[allow(unused_mut)] // `mode` is unix-only
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
}

/// Create `path` (never an existing file, never through a symlink), owner
/// read/write only, holding `text`.
fn write_new(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// Write the session's two files into a new private folder of its own and
/// answer where they are, only once both are written. The folder's name is
/// unguessable: it is what lets a session's chip be read back for the
/// session's own thread (`ironclaw_assistant`'s triage grant).
pub(crate) fn write_session(input: &SessionInput<'_>) -> std::io::Result<TriageSession> {
    let stamp = input.now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let root = &input.paths.sessions_dir;
    sessions_root(root)?;
    let session_dir = root.join(format!(
        "{}-{}",
        stamp.replace([':', '.'], "-"),
        uuid::Uuid::new_v4().simple()
    ));
    // Fails when it exists: a session folder is never reused.
    private_dir_builder().create(&session_dir)?;
    let context_file = session_dir.join(TRIAGE_CONTEXT_FILE_NAME);
    let prompt_file = session_dir.join(TRIAGE_PROMPT_FILE_NAME);
    let written = write_new(
        &context_file,
        &build_context(&ContextInput {
            generated_at: stamp,
            entry: input.entry,
            os: describe_os(),
            server: input.server.clone(),
            source_clone: input.source_clone,
            paths: input.paths,
            diagnostics: input.diagnostics,
        }),
    )
    .and_then(|()| write_new(&prompt_file, &seed_prompt(&context_file, input.entry)));
    if let Err(error) = written {
        // A half-written session is never linked; its folder goes.
        if let Err(cleanup) = std::fs::remove_dir_all(&session_dir) {
            tracing::debug!(%cleanup, "triage: half-written session folder not removed");
        }
        return Err(error);
    }
    Ok(TriageSession {
        session_dir,
        context_file,
        prompt_file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn paths_in(tmp: &Path) -> TriagePaths {
        let home = RebornHome::resolve_from_env_parts_without_migrating(
            Some(tmp.join("zlo home").into_os_string()),
            Some(tmp.as_os_str().to_os_string()),
            None,
        )
        .expect("home");
        TriagePaths::derive(&home, RebornProfile::Standalone, tmp)
    }

    fn write(paths: &TriagePaths, now: DateTime<Utc>) -> std::io::Result<TriageSession> {
        let entry = TriageEntry::Cli {
            cli_version: "1.2.0".into(),
            launched_as: "zlo triage".into(),
        };
        write_session(&SessionInput {
            paths,
            entry: &entry,
            server: describe_backend(&PidFileState::Absent, ServiceStateDto::NotInstalled),
            source_clone: None,
            diagnostics: &[],
            now,
        })
    }

    #[test]
    fn writes_context_and_prompt_into_a_timestamped_session_folder() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = paths_in(tmp.path());
        let now = Utc.with_ymd_and_hms(2026, 11, 24, 9, 30, 5).unwrap();
        let session = write(&paths, now).expect("session");
        assert_eq!(
            session.session_dir.parent(),
            Some(tmp.path().join("zlo home/triage").as_path())
        );
        let name = session
            .session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .expect("name");
        let random = name
            .strip_prefix("2026-11-24T09-30-05-000Z-")
            .expect("timestamped");
        assert_eq!(random.len(), 32, "unguessable: {name}");
        assert_eq!(
            session.prompt_file,
            session.session_dir.join("triage-playbook.md")
        );
        let prompt = std::fs::read_to_string(&session.prompt_file).expect("prompt");
        assert!(prompt.contains(&session.context_file.display().to_string()));
        let context = std::fs::read_to_string(&session.context_file).expect("context");
        let home_dir = tmp.path().join("zlo home");
        for path in [
            home_dir.join("local-dev/reborn-local-dev.db"),
            home_dir.join("config.toml"),
            home_dir.join("logs/serve.stderr.log"),
            home_dir.join("zlo-backend.pid"),
            home_dir.join("webui-token"),
            home_dir.join("source"),
        ] {
            assert!(
                context.contains(&path.display().to_string()),
                "context names {}:\n{context}",
                path.display()
            );
        }
        assert!(context.contains("no `zlo service` job is installed"));
    }

    /// Two sessions in the same millisecond never share (or overwrite) a
    /// folder, and both are private to this user.
    #[test]
    fn every_session_gets_its_own_private_folder() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = paths_in(tmp.path());
        let now = Utc.with_ymd_and_hms(2026, 11, 24, 9, 30, 5).unwrap();
        let first = write(&paths, now).expect("first");
        let second = write(&paths, now).expect("second");
        assert_ne!(first.session_dir, second.session_dir);
        assert!(first.prompt_file.is_file() && second.prompt_file.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode(&paths.sessions_dir), 0o700);
            assert_eq!(mode(&first.session_dir), 0o700);
            assert_eq!(mode(&first.context_file), 0o600);
            assert_eq!(mode(&first.prompt_file), 0o600);
        }
    }

    /// A sessions folder that is a symlink is refused: nothing is written
    /// where it points, and no link comes back.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_sessions_folder_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = paths_in(tmp.path());
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::fs::create_dir_all(paths.sessions_dir.parent().expect("parent")).expect("home");
        std::os::unix::fs::symlink(&elsewhere, &paths.sessions_dir).expect("symlink");
        let now = Utc.with_ymd_and_hms(2026, 11, 24, 9, 30, 5).unwrap();
        assert!(write(&paths, now).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).expect("dir").count(), 0);
    }

    /// A session whose second file cannot be written is not returned, and
    /// its folder is gone.
    #[cfg(unix)]
    #[test]
    fn a_half_written_session_is_never_linked() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = paths_in(tmp.path());
        let now = Utc.with_ymd_and_hms(2026, 11, 24, 9, 30, 5).unwrap();
        write(&paths, now).expect("root exists");
        // A read-only sessions folder: the session folder cannot be made.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&paths.sessions_dir, std::fs::Permissions::from_mode(0o500))
            .expect("chmod");
        let result = write(&paths, now);
        std::fs::set_permissions(&paths.sessions_dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod back");
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_dir(&paths.sessions_dir).expect("dir").count(),
            1
        );
    }

    /// Both entry points name every agent runtime's credentials as never to
    /// be read, beside ZLO's own secrets.
    #[test]
    fn the_context_forbids_every_agent_credential() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = paths_in(tmp.path());
        let session = write(&paths, Utc::now()).expect("session");
        let context = std::fs::read_to_string(&session.context_file).expect("context");
        let forbidden = context
            .split("- Secrets and agent credentials (NEVER read")
            .nth(1)
            .expect("the secrets list");
        for needle in [
            "webui-token",
            "Claude Code-credentials",
            ".credentials.json",
            "auth.json",
            "oauth_creds.json",
            "antigravity-acp",
            "`gemini`",
            "`antigravity`",
            "muse login",
        ] {
            assert!(forbidden.contains(needle), "{needle} missing:\n{forbidden}");
        }
    }

    #[test]
    fn the_pid_file_reports_running_stale_absent_and_garbage() {
        let tmp = tempfile::tempdir().expect("tmp");
        let file = tmp.path().join("zlo-backend.pid");
        assert_eq!(read_pid_file(&file, |_| true), PidFileState::Absent);
        std::fs::write(&file, "4242\n").expect("write");
        assert_eq!(
            read_pid_file(&file, |p| p == 4242),
            PidFileState::Alive(4242)
        );
        assert_eq!(read_pid_file(&file, |_| false), PidFileState::Dead(4242));
        let alive = describe_backend(&PidFileState::Alive(4242), ServiceStateDto::Running);
        assert!(alive.starts_with("running (pid 4242, started by the desktop app)"));
        let dead = describe_backend(&PidFileState::Dead(4242), ServiceStateDto::Stopped);
        assert!(dead.contains("stale: pid 4242"));
        std::fs::write(&file, "nope").expect("write");
        assert!(matches!(
            read_pid_file(&file, |_| true),
            PidFileState::Unreadable(_)
        ));
    }

    #[test]
    fn a_pid_with_no_process_is_not_a_backend() {
        assert!(!is_zlo_process(u32::MAX - 7));
    }

    /// Runtime folders come from their owners: ZLO's Antigravity profile is
    /// Hermes's when Hermes signed one in, and Muse's state is its own.
    #[test]
    fn runtime_folders_come_from_their_owners() {
        let tmp = tempfile::tempdir().expect("tmp");
        let hermes = tmp
            .path()
            .join(".hermes/plugin-data/t3code/antigravity/profile");
        std::fs::create_dir_all(&hermes).expect("hermes");
        let state = runtime_state(tmp.path());
        let find = |label: &str| {
            state
                .iter()
                .find(|(l, _)| l.starts_with(label))
                .map(|(_, p)| p.clone())
        };
        assert_eq!(find("ZLO's Antigravity profile"), Some(hermes));
        assert_eq!(
            find("ZLO's Muse Code runtime"),
            Some(ironclaw_composition::agent_runtime_paths::muse_state_dir(
                tmp.path()
            ))
        );
        assert_eq!(
            runtime_state_dir(Path::new("/home/u"), "", ".x"),
            PathBuf::from("/home/u/.x")
        );
    }
}
