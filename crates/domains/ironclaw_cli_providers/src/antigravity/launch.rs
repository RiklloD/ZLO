//! How the Antigravity agent is found and started: the native `agy` CLI's
//! `agy acp` stdio server (1.5 and later), else the separately shipped ACP
//! runtime (`agy_acp_server.par` beside `localharness_external`) that T3
//! Code and Hermes used to install. Both read the same profile layout
//! (`<GEMINI_HOME>/antigravity-acp/acp_token.json`), so a sign-in made with
//! either one keeps working with the other.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

use super::AntigravityConfig;

/// The `agy` release the native launch was checked against.
pub const EXPECTED_CLI_VERSION: &str = "1.5.11";

/// The first `agy` release with an ACP server (`agy acp`); it also steers a
/// running turn with the next `session/prompt`.
const FIRST_ACP_CLI: (u64, u64, u64) = (1, 5, 0);

/// The data folder, under `--gemini-dir`, the native CLI keeps its sign-in
/// and conversations in. The legacy runtime uses the same name, which is
/// what keeps one profile valid for both.
pub(super) const APP_DATA_DIR: &str = "antigravity-acp";

/// The only inherited variables the agent sees. Everything else (ambient
/// cloud credentials, API keys, agent knobs) is dropped: the agent signs in
/// through its own profile only.
const KEPT_ENV: [&str; 10] = [
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "SSL_CERT_FILE",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
];

/// Which agent program a turn starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLaunch {
    /// `agy acp --gemini-dir <profile> --app-data-dir antigravity-acp`.
    /// Never `--dangerously-skip-permissions`: every tool call asks.
    Cli { agy: PathBuf },
    /// The legacy one-file runtime and its harness.
    Runtime { server: PathBuf, harness: PathBuf },
}

impl AgentLaunch {
    /// Whether the agent can take a second `session/prompt` as a steer.
    /// Checked again against the version `initialize` reports.
    pub(super) fn may_steer(&self) -> bool {
        matches!(self, Self::Cli { .. })
    }
}

/// The agent process: allowlisted environment, its own profile, its own
/// process group, no browser.
pub(super) fn agent_command(c: &AntigravityConfig) -> Command {
    let mut cmd = match &c.launch {
        AgentLaunch::Cli { agy } => {
            let mut cmd = Command::new(agy);
            cmd.arg("acp")
                .arg("--gemini-dir")
                .arg(&c.profile_dir)
                // Joined beneath --gemini-dir by the CLI.
                .arg("--app-data-dir")
                .arg(APP_DATA_DIR)
                .arg("--disable-telemetry");
            cmd
        }
        AgentLaunch::Runtime { server, .. } => Command::new(server),
    };
    cmd.env_clear();
    for key in KEPT_ENV {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    // The legacy runtime unpacks itself per launch; keep that under the
    // profile, where it can be reclaimed.
    let tmp = c.profile_dir.join(APP_DATA_DIR).join("tmp");
    cmd.env("GEMINI_HOME", &c.profile_dir)
        .env("AGY_ACP_FORCE_FILE_STORAGE", "1")
        .env("PYTHONUNBUFFERED", "1")
        .env("ELECTRON_RUN_AS_NODE", "1")
        .env("TMPDIR", tmp)
        // A background turn never opens a browser on its own.
        .env("BROWSER", "/usr/bin/true");
    if let AgentLaunch::Runtime { harness, .. } = &c.launch {
        cmd.env("ANTIGRAVITY_HARNESS_PATH", harness);
    }
    cmd.current_dir(&c.work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    cmd
}

/// Profile folders, the work folder and the session map. The auth method
/// goes in the profile's `settings.json` (never a credential); an existing
/// file (a shared profile) is left alone.
pub(super) async fn prepare_dirs(config: &AntigravityConfig) -> std::io::Result<()> {
    let acp = config.profile_dir.join(APP_DATA_DIR);
    tokio::fs::create_dir_all(acp.join("tmp")).await?;
    tokio::fs::create_dir_all(&config.work_dir).await?;
    tokio::fs::create_dir_all(config.state_dir.join("sessions")).await?;
    let settings = acp.join("settings.json");
    if !tokio::fs::try_exists(&settings).await.unwrap_or(false) {
        // silent-ok: a failed probe rewrites the same small file
        let body = json!({"auth": {"type": config.auth_method}}).to_string();
        tokio::fs::write(&settings, format!("{body}\n")).await?;
    }
    if let Some(user) = &config.user_gemini_dir {
        link_user_skills(&config.profile_dir, user).await;
    }
    Ok(())
}

/// The skill folders Antigravity reads for every project, relative to a
/// Gemini directory.
const USER_SKILL_DIRS: [&str; 2] = ["config/skills", "antigravity-cli/skills"];

/// The private profile isolates sign-in, hooks and MCP servers, so the
/// user's own global skills (`~/.gemini/config/skills`,
/// `~/.gemini/antigravity-cli/skills`) are linked into it. Only a missing
/// entry, an empty placeholder folder (the agent makes one on launch) or a
/// link of ours is replaced: a folder with content is never touched.
pub(super) async fn link_user_skills(profile: &Path, user_gemini: &Path) {
    for rel in USER_SKILL_DIRS {
        let (link, target) = (profile.join(rel), user_gemini.join(rel));
        if link == target {
            continue;
        }
        if let Err(error) = link_one(&link, &target).await {
            tracing::debug!(%error, rel, "antigravity: user skills not linked");
        }
    }
}

async fn link_one(link: &Path, target: &Path) -> std::io::Result<()> {
    let existing = tokio::fs::read_link(link).await.ok(); // silent-ok: not a link (or absent) is handled below
    if !tokio::fs::try_exists(target).await.unwrap_or(false) {
        // silent-ok: an unreadable target is treated as absent
        // Nothing to share; drop a link of ours that now dangles.
        if existing.is_some() {
            tokio::fs::remove_file(link).await?;
        }
        return Ok(());
    }
    match existing {
        Some(current) if current == target => return Ok(()),
        Some(_) => tokio::fs::remove_file(link).await?,
        None => match tokio::fs::symlink_metadata(link).await {
            // `remove_dir` refuses a folder with content: only the
            // placeholder goes.
            Ok(meta) if meta.is_dir() => tokio::fs::remove_dir(link).await?,
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        },
    }
    if let Some(dir) = link.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::symlink(target, link).await
}

/// `major.minor.patch` at the start of `raw`.
pub(super) fn parse_version(raw: &str) -> Option<(u64, u64, u64)> {
    let mut parts = raw.trim().splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((major, minor, patch.parse().ok()?))
}

/// Whether an `initialize` answer comes from an agent that steers with a
/// second `session/prompt` (`agentInfo.version` 1.5.0 or later).
pub(super) fn steers_with_prompt(initialize: &Value) -> bool {
    initialize["agentInfo"]["version"]
        .as_str()
        .and_then(parse_version)
        .is_some_and(|v| v >= FIRST_ACP_CLI)
}

/// Whether the agent appends `_meta.antigravity.systemPrompt.append` to its
/// system instructions on `session/new` and `session/resume`.
pub(super) fn appends_system_prompt(initialize: &Value) -> bool {
    initialize["agentCapabilities"]["_meta"]["antigravity"]["systemPrompt"]["append"]
        .as_bool()
        .unwrap_or(false)
}

/// The installed `agy`, when it is new enough to serve ACP. Asks
/// `agy --version` with a clean environment and a short deadline.
pub fn find_cli(home: &Path) -> Option<PathBuf> {
    let agy = crate::rpc::find_on_path(home, "agy")?;
    let version = cli_version(&agy)?;
    if version < FIRST_ACP_CLI {
        tracing::debug!(?version, "antigravity: agy has no ACP server");
        return None;
    }
    if parse_version(EXPECTED_CLI_VERSION) != Some(version) {
        tracing::debug!(
            ?version,
            expected = EXPECTED_CLI_VERSION,
            "antigravity: agy version differs from the checked release"
        );
    }
    Some(agy)
}

fn cli_version(agy: &Path) -> Option<(u64, u64, u64)> {
    let mut child = std::process::Command::new(agy)
        .arg("--version")
        .env_clear()
        .envs(
            KEPT_ENV
                .iter()
                .filter_map(|k| Some((k, std::env::var_os(k)?))),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?; // silent-ok: an agy that does not start is no ACP agent
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                if let Err(error) = child.kill() {
                    tracing::debug!(%error, "antigravity: agy --version not stopped");
                }
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "reaping a killed probe; its status is irrelevant"
                )]
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?; // silent-ok: unreadable output is no version
    parse_version(&out)
}

/// The legacy ACP runtime: Hermes's managed copy, then T3 Code's.
pub fn find_runtime(home: &Path) -> Option<AgentLaunch> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "x86_64") => "linux-x64",
        _ => return None,
    };
    [
        home.join(".hermes/plugin-data/t3code/antigravity-acp"),
        home.join(".t3/tools/antigravity-acp"),
    ]
    .into_iter()
    .find_map(|dir| {
        let base = dir.join(platform);
        let active = std::fs::read_to_string(base.join("active.json")).ok()?; // silent-ok: no install here
        let release: Value = serde_json::from_str(&active).ok()?; // silent-ok: a torn marker is no install
        let version = base.join("versions").join(release["releaseId"].as_str()?);
        let server = version.join("agy_acp_server.par");
        let harness = version.join("localharness_external");
        (server.is_file() && harness.is_file()).then_some(AgentLaunch::Runtime { server, harness })
    })
}

/// The agent to run: the native CLI when it serves ACP, else the legacy
/// runtime.
pub fn find_agent(home: &Path) -> Option<AgentLaunch> {
    find_cli(home)
        .map(|agy| AgentLaunch::Cli { agy })
        .or_else(|| find_runtime(home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_gate_steering() {
        assert_eq!(parse_version("1.5.11\n"), Some((1, 5, 11)));
        assert_eq!(parse_version("1.4.5-rc1"), Some((1, 4, 5)));
        assert_eq!(parse_version("garbage"), None);
        assert!(steers_with_prompt(
            &json!({"agentInfo": {"version": "1.5.11"}})
        ));
        assert!(!steers_with_prompt(
            &json!({"agentInfo": {"version": "1.3.0"}})
        ));
        assert!(!steers_with_prompt(&json!({})));
        assert!(appends_system_prompt(&json!({
            "agentCapabilities": {"_meta": {"antigravity": {"systemPrompt": {"append": true}}}}
        })));
        assert!(!appends_system_prompt(&json!({"agentCapabilities": {}})));
    }

    #[tokio::test]
    async fn user_skills_link_replaces_only_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, user) = (dir.path().join("profile"), dir.path().join("gemini"));
        std::fs::create_dir_all(user.join("config/skills/a")).unwrap();
        std::fs::create_dir_all(user.join("antigravity-cli/skills")).unwrap();
        // The agent's empty placeholder goes; a folder with content stays.
        std::fs::create_dir_all(profile.join("config/skills")).unwrap();
        std::fs::create_dir_all(profile.join("antigravity-cli/skills/mine")).unwrap();
        link_user_skills(&profile, &user).await;
        assert_eq!(
            std::fs::read_link(profile.join("config/skills")).unwrap(),
            user.join("config/skills")
        );
        assert!(profile.join("antigravity-cli/skills/mine").is_dir());
        // Idempotent, and a link whose target is gone is removed.
        link_user_skills(&profile, &user).await;
        std::fs::remove_dir_all(user.join("config/skills")).unwrap();
        link_user_skills(&profile, &user).await;
        assert!(
            std::fs::symlink_metadata(profile.join("config/skills")).is_err(),
            "dangling link removed"
        );
    }
}
