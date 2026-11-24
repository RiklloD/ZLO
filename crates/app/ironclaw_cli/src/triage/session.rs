//! Writes `context.md` and the playbook prompt into a fresh
//! `<ZLO_HOME>/triage/<time>` folder. Both `zlo triage` and the in-app entry
//! start from these files. Every path the context names comes from its
//! owner: home resolution (`ironclaw_config`), the standalone database and
//! master-key names (`ironclaw_composition::setup_transfer`), the WebUI token
//! file, the service module, the diagnostics bundle's log list.

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use ironclaw_config::{DESKTOP_APP_LOG_RELATIVE_PATH, RebornHome, RebornProfile};
use ironclaw_product_contracts::triage::TRIAGE_PROMPT_FILE_NAME;

use crate::dto::ServiceStateDto;
use crate::triage::prompt::{
    ContextInput, SourceClone, TriageEntry, TriagePaths, build_context, seed_prompt,
};

/// The coding-agent runtimes ZLO drives, with where each keeps its own
/// state: `(label, env override, default folder under the user's home)`.
/// `zlo triage` launches the first three; the context names all of them so
/// the agent can check harness health.
pub(crate) const RUNTIME_STATE_DIRS: [(&str, &str, &str); 4] = [
    ("Claude Code (claude)", "CLAUDE_CONFIG_DIR", ".claude"),
    ("Codex (codex)", "CODEX_HOME", ".codex"),
    ("Antigravity CLI (agy)", "GEMINI_HOME", ".gemini"),
    // ZLO's own Antigravity profile and transcripts
    // (`ironclaw_cli_providers::antigravity::default_config`).
    ("ZLO's Antigravity runtime", "", ".ironclaw/antigravity"),
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
            runtime_state: RUNTIME_STATE_DIRS
                .iter()
                .map(|(label, env, default)| {
                    (
                        (*label).to_string(),
                        runtime_state_dir(user_home, env, default),
                    )
                })
                .collect(),
            secrets: vec![
                crate::webui_token::webui_token_file_path(&state_dir)
                    .display()
                    .to_string(),
                master_key.display().to_string(),
                "the ZLO items in the login Keychain".into(),
                "the encrypted secret records in the database".into(),
            ],
            source_cache_dir: crate::triage::source::source_cache_dir(&state_dir),
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
    pub(crate) now: DateTime<Utc>,
}

/// Write the session's two files and answer where they are.
pub(crate) fn write_session(input: &SessionInput<'_>) -> std::io::Result<TriageSession> {
    let stamp = input.now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let session_dir = input
        .paths
        .state_dir
        .join("triage")
        .join(stamp.replace([':', '.'], "-"));
    std::fs::create_dir_all(&session_dir)?;
    let context_file = session_dir.join("context.md");
    std::fs::write(
        &context_file,
        build_context(&ContextInput {
            generated_at: stamp,
            entry: input.entry,
            os: describe_os(),
            server: input.server.clone(),
            source_clone: input.source_clone,
            paths: input.paths,
        }),
    )?;
    let prompt_file = session_dir.join(TRIAGE_PROMPT_FILE_NAME);
    std::fs::write(&prompt_file, seed_prompt(&context_file, input.entry))?;
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

    #[test]
    fn writes_context_and_prompt_into_a_timestamped_session_folder() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = RebornHome::resolve_from_env_parts_without_migrating(
            Some(tmp.path().join("zlo home").into_os_string()),
            Some(tmp.path().as_os_str().to_os_string()),
            None,
        )
        .expect("home");
        let paths = TriagePaths::derive(&home, RebornProfile::Standalone, tmp.path());
        let entry = TriageEntry::Cli {
            cli_version: "1.2.0".into(),
            launched_as: "zlo triage".into(),
        };
        let now = Utc.with_ymd_and_hms(2026, 11, 24, 9, 30, 5).unwrap();
        let session = write_session(&SessionInput {
            paths: &paths,
            entry: &entry,
            server: describe_backend(&PidFileState::Absent, ServiceStateDto::NotInstalled),
            source_clone: None,
            now,
        })
        .expect("session");
        assert_eq!(
            session.session_dir,
            tmp.path().join("zlo home/triage/2026-11-24T09-30-05-000Z")
        );
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

    #[test]
    fn runtime_state_honours_the_env_override() {
        let home = Path::new("/home/u");
        assert_eq!(
            runtime_state_dir(home, "", ".ironclaw/antigravity"),
            PathBuf::from("/home/u/.ironclaw/antigravity")
        );
    }
}
