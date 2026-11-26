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
        // An `agy` that replaces itself in place mid-turn breaks the
        // running agent (t3code #39363); the user updates it themselves.
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "1")
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

/// The folder of `open` and `xdg-open` stubs that fail, under the state
/// folder, (re)written for an agent ZLO starts on its own. agy finds the
/// browser opener on PATH (it names no absolute `/usr/bin/open`), so with
/// this folder first on PATH a sign-in that wants a browser fails instead
/// of opening one. Each stub is written beside its final name and renamed
/// over it, so a concurrent start never sees a torn file.
pub(super) async fn browser_stubs(config: &AntigravityConfig) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    const STUB: &str =
        "#!/bin/sh\n# ZLO: an agent started in the background never opens a browser.\nexit 1\n";
    let dir = config.state_dir.join("no-browser");
    private_dir_all(&dir).await?;
    for name in ["open", "xdg-open"] {
        let path = dir.join(name);
        let current = tokio::fs::symlink_metadata(&path).await.ok(); // silent-ok: missing is rewritten
        let intact = match current {
            Some(meta) if meta.is_file() && meta.permissions().mode() & 0o777 == 0o700 => {
                tokio::fs::read_to_string(&path).await.ok().as_deref() == Some(STUB) // silent-ok: unreadable is rewritten
            }
            _ => false,
        };
        if intact {
            continue;
        }
        let tmp = dir.join(format!(".{name}.{}", uuid::Uuid::new_v4()));
        tokio::fs::write(&tmp, STUB).await?;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).await?;
        tokio::fs::rename(&tmp, &path).await?;
    }
    Ok(dir)
}

/// Put the [`browser_stubs`] folder first on the agent's PATH.
pub(super) fn without_browser(cmd: &mut Command, stubs: &Path) -> Result<(), String> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::iter::once(stubs.to_path_buf()).chain(
        // An empty entry would mean the agent's working folder.
        std::env::split_paths(&inherited).filter(|p| !p.as_os_str().is_empty()),
    ))
    .map_err(|e| format!("browser stubs on PATH: {e}"))?;
    cmd.env("PATH", path);
    Ok(())
}

/// Profile folders, the work folder and the session map. The auth method
/// goes in the profile's `settings.json` (never a credential); an existing
/// file (a shared profile) is left alone.
///
/// The profile holds the account's sign-in, so every folder ZLO creates in
/// it is 0700, and a profile ZLO owns (under its state folder) is tightened
/// to 0700 along with its `config` folder. A shared profile another app
/// made keeps its mode. Threat model: the profile is private to the user;
/// another process of the same user racing the skill links inside it is
/// out of scope (it could change the profile directly anyway), so the
/// links are made by path, not through held directory handles.
pub(super) async fn prepare_dirs(config: &AntigravityConfig) -> std::io::Result<()> {
    let acp = config.profile_dir.join(APP_DATA_DIR);
    private_dir_all(&config.profile_dir).await?;
    private_dir_all(&acp.join("tmp")).await?;
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
    if config.owns_profile() {
        tighten(&config.profile_dir).await?;
        tighten(&config.profile_dir.join("config")).await?;
    }
    Ok(())
}

/// `create_dir_all`, with every folder it creates 0700.
pub(super) async fn private_dir_all(path: &Path) -> std::io::Result<()> {
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .await
}

/// Drop group and other access from a real folder ZLO owns; a missing
/// folder or a symlink is left alone.
async fn tighten(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mode = meta.permissions().mode();
    if meta.is_dir() && !meta.file_type().is_symlink() && mode & 0o077 != 0 {
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o700)).await?;
    }
    Ok(())
}

/// What Antigravity reads for every project, relative to a Gemini
/// directory: the skill folders, the skill manifest (`config/skills.json`,
/// t3code #39765) and the global rules (`AGENTS.md`, `GEMINI.md`, t3code
/// #39531).
pub(crate) const USER_CONFIG_ENTRIES: [&str; 5] = [
    "config/skills",
    "antigravity-cli/skills",
    "config/skills.json",
    "AGENTS.md",
    "GEMINI.md",
];

/// The private profile isolates sign-in, hooks and MCP servers, so the
/// user's own global skills (`~/.gemini/config/skills`,
/// `~/.gemini/antigravity-cli/skills`, `~/.gemini/config/skills.json`) and
/// rules (`~/.gemini/AGENTS.md`, `~/.gemini/GEMINI.md`) are linked into it;
/// a file the profile has of its own stays. Only a missing
/// entry, an empty placeholder folder (the agent makes one on launch) or a
/// link of ours (to exactly the managed target) is replaced; a folder with
/// content and a link pointing anywhere else are the user's and stay. No
/// symlinked folder inside the profile is followed on the way there.
pub(super) async fn link_user_skills(profile: &Path, user_gemini: &Path) {
    for rel in USER_CONFIG_ENTRIES {
        let (link, target) = (profile.join(rel), user_gemini.join(rel));
        if link == target {
            continue;
        }
        if let Err(error) = link_one(profile, Path::new(rel), &target).await {
            tracing::debug!(%error, rel, "antigravity: user skills not linked");
        }
    }
}

/// Make `profile/rel` a link to `target`, or drop a dangling link of ours.
async fn link_one(profile: &Path, rel: &Path, target: &Path) -> std::io::Result<()> {
    let link = profile.join(rel);
    // Every folder between the profile and the link must be a real folder
    // (or not exist yet): a symlink there would send the changes below
    // outside the private profile.
    let mut dir = profile.to_path_buf();
    if let Some(parent) = rel.parent() {
        for part in parent.components() {
            dir.push(part);
            match tokio::fs::symlink_metadata(&dir).await {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    return Err(std::io::Error::other(
                        "a folder on the way to the skills link is not a plain folder",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    let target_exists = tokio::fs::try_exists(target).await.unwrap_or(false); // silent-ok: an unreadable target is treated as absent
    match tokio::fs::symlink_metadata(&link).await {
        Ok(meta) if meta.file_type().is_symlink() => {
            if tokio::fs::read_link(&link).await? != target {
                // The user's own link: never ours to change.
                return Ok(());
            }
            if !target_exists {
                // Ours, and now dangling.
                tokio::fs::remove_file(&link).await?;
            }
            return Ok(());
        }
        // `remove_dir` refuses a folder with content: only the placeholder
        // goes.
        Ok(meta) if meta.is_dir() && target_exists => tokio::fs::remove_dir(&link).await?,
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if !target_exists {
        return Ok(());
    }
    if let Some(parent) = link.parent() {
        private_dir_all(parent).await?;
    }
    tokio::fs::symlink(target, &link).await
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

/// How long `agy --version` may take, output included.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// The most of `agy --version`'s output read.
const VERSION_PROBE_CAP: u64 = 4096;

/// The installed `agy`, when it is new enough to serve ACP. Asks
/// `agy --version` with a clean environment and a short deadline.
pub async fn find_cli(home: &Path) -> Option<PathBuf> {
    let agy = crate::rpc::find_on_path(home, "agy")?;
    match probe_cli(&agy).await {
        CliProbe::Usable => Some(agy),
        CliProbe::Rejected | CliProbe::Unanswered => None,
    }
}

/// What `agy --version` said about one binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliProbe {
    /// New enough to serve ACP.
    Usable,
    /// It answered, and the answer rules it out (too old, or not a
    /// version): asking the same binary again gives the same answer.
    Rejected,
    /// It did not start, did not answer in time, or its answer could not be
    /// read: worth asking again later.
    Unanswered,
}

/// Ask `agy` at `agy` whether it can serve ACP.
pub async fn probe_cli(agy: &Path) -> CliProbe {
    let version = match cli_version(agy).await {
        VersionProbe::Version(version) => version,
        VersionProbe::Unparsable => return CliProbe::Rejected,
        VersionProbe::Unanswered => return CliProbe::Unanswered,
    };
    if version < FIRST_ACP_CLI {
        tracing::debug!(?version, "antigravity: agy has no ACP server");
        return CliProbe::Rejected;
    }
    if parse_version(EXPECTED_CLI_VERSION) != Some(version) {
        tracing::debug!(
            ?version,
            expected = EXPECTED_CLI_VERSION,
            "antigravity: agy version differs from the checked release"
        );
    }
    CliProbe::Usable
}

#[derive(Debug, PartialEq, Eq)]
enum VersionProbe {
    Version((u64, u64, u64)),
    /// It answered something that is not a version.
    Unparsable,
    Unanswered,
}

/// SIGKILLs a probe's process group when dropped: also when the request
/// that asked is dropped mid-probe, where `kill_on_drop` covers only the
/// leader.
struct GroupKill(Option<i32>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            crate::rpc::kill_group(pgid, "antigravity: agy --version group");
        }
    }
}

/// `agy --version`, bounded: one deadline covers the first output line
/// (read up to [`VERSION_PROBE_CAP`]) and the exit, and the probe's whole
/// process group is killed afterwards, so a descendant holding stdout open
/// neither blocks the probe nor outlives it.
async fn cli_version(agy: &Path) -> VersionProbe {
    use tokio::io::AsyncBufReadExt;
    let child = Command::new(agy)
        .arg("--version")
        .env_clear()
        .envs(
            KEPT_ENV
                .iter()
                .filter_map(|k| Some((k, std::env::var_os(k)?))),
        )
        // Killed after its deadline: never mid-way through replacing itself.
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            // An agy that does not start is no ACP agent.
            tracing::debug!(%error, "antigravity: agy --version did not start");
            return VersionProbe::Unanswered;
        }
    };
    let group = GroupKill(crate::rpc::group_of(&child));
    let Some(stdout) = child.stdout.take() else {
        return VersionProbe::Unanswered;
    };
    let probe = async {
        let mut line = String::new();
        let mut reader =
            tokio::io::BufReader::new(tokio::io::AsyncReadExt::take(stdout, VERSION_PROBE_CAP));
        let read = reader.read_line(&mut line).await;
        let exited = child.wait().await;
        (read, exited, line)
    };
    let outcome = tokio::time::timeout(VERSION_PROBE_TIMEOUT, probe).await;
    drop(group);
    match outcome {
        // Only a clean exit with output is an answer: a failing or silent
        // run may be passing (a wrapper's error on stderr), so it is asked
        // again later rather than ruled out.
        Ok((Ok(_), Ok(status), line)) if status.success() && !line.trim().is_empty() => {
            match parse_version(&line) {
                Some(version) => VersionProbe::Version(version),
                None => VersionProbe::Unparsable,
            }
        }
        Ok((read, exited, _)) => {
            tracing::debug!(?read, ?exited, "antigravity: agy --version unreadable");
            VersionProbe::Unanswered
        }
        Err(_elapsed) => {
            tracing::debug!("antigravity: agy --version timed out");
            if let Err(error) = child.kill().await {
                tracing::debug!(%error, "antigravity: agy --version not stopped");
            }
            VersionProbe::Unanswered
        }
    }
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
pub async fn find_agent(home: &Path) -> Option<AgentLaunch> {
    match find_cli(home).await {
        Some(agy) => Some(AgentLaunch::Cli { agy }),
        None => find_runtime(home),
    }
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

    fn test_config(profile: PathBuf, state: PathBuf, user: Option<PathBuf>) -> AntigravityConfig {
        AntigravityConfig {
            runtime_id: "antigravity".into(),
            display_name: "Antigravity".into(),
            launch: AgentLaunch::Cli {
                agy: PathBuf::from("agy"),
            },
            profile_dir: profile,
            user_gemini_dir: user,
            auth_method: "oauth-personal".into(),
            auth_timeout: Duration::from_secs(5),
            work_dir: state.join("work"),
            state_dir: state,
            background_hold: Duration::from_secs(30),
            prompt_silence: Duration::from_secs(120),
            keyring: None,
        }
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // Review (2nd round, C): the profile and its `config` folder are
    // private (0700) when ZLO creates them, and tightened when ZLO owns the
    // profile; a shared profile someone else made keeps its mode.
    #[tokio::test]
    async fn profile_folders_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let open = std::fs::Permissions::from_mode(0o755);
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("gemini");
        std::fs::create_dir_all(user.join("config/skills")).unwrap();

        // Created by ZLO: private all the way down.
        let state = dir.path().join("state");
        let fresh = test_config(state.join("profile"), state.clone(), Some(user.clone()));
        prepare_dirs(&fresh).await.unwrap();
        assert_eq!(mode(&fresh.profile_dir), 0o700);
        assert_eq!(mode(&fresh.profile_dir.join("config")), 0o700);
        assert_eq!(mode(&fresh.profile_dir.join(APP_DATA_DIR)), 0o700);

        // ZLO's own profile left looser: tightened.
        std::fs::set_permissions(&fresh.profile_dir, open.clone()).unwrap();
        std::fs::set_permissions(fresh.profile_dir.join("config"), open.clone()).unwrap();
        prepare_dirs(&fresh).await.unwrap();
        assert_eq!(mode(&fresh.profile_dir), 0o700);
        assert_eq!(mode(&fresh.profile_dir.join("config")), 0o700);

        // A shared profile (another app's) that exists: its mode stays, and
        // the `config` folder ZLO creates inside it is private.
        let shared = dir.path().join("hermes/profile");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&shared, open).unwrap();
        let foreign = test_config(shared.clone(), dir.path().join("state2"), Some(user));
        prepare_dirs(&foreign).await.unwrap();
        assert_eq!(mode(&shared), 0o755);
        assert_eq!(mode(&shared.join("config")), 0o700);
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

    /// t3code #39531 / #39765: the user's global rules and skills.json
    /// are linked into the private profile; the profile's own file stays.
    #[tokio::test]
    async fn user_rules_and_skills_json_are_linked_and_the_profiles_own_stay() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, user) = (dir.path().join("profile"), dir.path().join("gemini"));
        std::fs::create_dir_all(user.join("config")).unwrap();
        std::fs::write(user.join("AGENTS.md"), "global agents rule").unwrap();
        std::fs::write(user.join("GEMINI.md"), "global gemini rule").unwrap();
        std::fs::write(user.join("config/skills.json"), "{}").unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        link_user_skills(&profile, &user).await;
        for rel in ["AGENTS.md", "GEMINI.md", "config/skills.json"] {
            assert_eq!(
                std::fs::read_link(profile.join(rel)).unwrap(),
                user.join(rel),
                "{rel}"
            );
        }
        std::fs::remove_file(user.join("AGENTS.md")).unwrap();
        std::fs::remove_file(profile.join("GEMINI.md")).unwrap();
        std::fs::write(profile.join("GEMINI.md"), "profile's own rule").unwrap();
        link_user_skills(&profile, &user).await;
        assert!(std::fs::symlink_metadata(profile.join("AGENTS.md")).is_err());
        assert_eq!(
            std::fs::read_to_string(profile.join("GEMINI.md")).unwrap(),
            "profile's own rule"
        );
    }

    #[test]
    fn the_agent_never_updates_itself_mid_turn() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("p"), dir.path().join("s"), None);
        let command = agent_command(&config);
        let env: Vec<_> = command.as_std().get_envs().collect();
        assert!(
            env.iter()
                .any(|(k, v)| *k == "AGY_CLI_DISABLE_AUTO_UPDATE" && v.is_some_and(|v| v == "1")),
            "{env:?}"
        );
    }

    // Review: a link the user made (pointing anywhere but the managed
    // target) is theirs, present target or not.
    #[tokio::test]
    async fn user_skills_link_keeps_foreign_links() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, user) = (dir.path().join("profile"), dir.path().join("gemini"));
        let elsewhere = dir.path().join("my-skills");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(profile.join("config")).unwrap();
        std::fs::create_dir_all(profile.join("antigravity-cli")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, profile.join("config/skills")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, profile.join("antigravity-cli/skills")).unwrap();
        // The global target exists for one entry and is absent for the other.
        std::fs::create_dir_all(user.join("config/skills")).unwrap();
        link_user_skills(&profile, &user).await;
        for rel in ["config/skills", "antigravity-cli/skills"] {
            assert_eq!(
                std::fs::read_link(profile.join(rel)).unwrap(),
                elsewhere,
                "{rel}: a foreign link is kept"
            );
        }
    }

    // Review: a symlinked parent inside the profile is never followed, so
    // nothing lands outside the private profile.
    #[tokio::test]
    async fn user_skills_link_never_follows_a_symlinked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let (profile, user) = (dir.path().join("profile"), dir.path().join("gemini"));
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(user.join("config/skills")).unwrap();
        std::os::unix::fs::symlink(&outside, profile.join("config")).unwrap();
        link_user_skills(&profile, &user).await;
        assert!(
            std::fs::symlink_metadata(outside.join("skills")).is_err(),
            "a link was made outside the profile"
        );
    }

    // Review: the version probe is bounded even when a descendant keeps
    // its stdout open, and the descendant goes with the probe's group.
    #[tokio::test]
    async fn version_probe_is_bounded_when_a_descendant_holds_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let agy = dir.path().join("agy");
        std::fs::write(
            &agy,
            format!(
                "#!/bin/sh\necho 1.5.11\nsleep 30 &\necho $! > '{}'\n",
                pid_file.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let version = cli_version(&agy).await;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "probe blocked for {:?}",
            started.elapsed()
        );
        assert_eq!(version, VersionProbe::Version((1, 5, 11)));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the probe's descendant survived");
    }
}
