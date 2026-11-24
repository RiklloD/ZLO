//! `zlo triage [--agent claude|codex|antigravity] [--model M]`: write a
//! triage session (machine context + playbook prompt) under the ZLO home and
//! start a coding agent CLI in this terminal to investigate the install.
//!
//! Works when the app or the backend will not start — that is its point: it
//! reads no config file, opens no database, and starts no runtime. It only
//! resolves the home from the environment (without the legacy-home move),
//! writes `<ZLO_HOME>/triage/<time>-<random>/`, and runs the agent there. A
//! broken setting it can work around (an unknown `ZLO_PROFILE`) does not
//! stop it: the session is written and the problem is listed in its context.

use std::ffi::OsStr;
use std::io::{BufRead, IsTerminal as _, Write};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use ironclaw_config::{RebornHome, RebornProfile};

use crate::triage::prompt::{TriageEntry, TriagePaths, launch_prompt};
use crate::triage::session::{
    PidFileState, SessionInput, TriageSession, describe_backend, is_zlo_process, read_pid_file,
    runtime_state_dir, write_session,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AgentId {
    Claude,
    Codex,
    Antigravity,
}

pub(crate) struct TriageAgent {
    pub(crate) id: AgentId,
    /// The answer the picker also accepts by name.
    pub(crate) name: &'static str,
    pub(crate) command: &'static str,
    pub(crate) label: &'static str,
    state_dir_env: &'static str,
    default_state_dir: &'static str,
    prompt_args: &'static [&'static str],
}

pub(crate) const TRIAGE_AGENTS: [TriageAgent; 3] = [
    TriageAgent {
        id: AgentId::Claude,
        name: "claude",
        command: "claude",
        label: "Claude Code",
        state_dir_env: "CLAUDE_CONFIG_DIR",
        default_state_dir: ".claude",
        prompt_args: &[],
    },
    TriageAgent {
        id: AgentId::Codex,
        name: "codex",
        command: "codex",
        label: "Codex",
        state_dir_env: "CODEX_HOME",
        default_state_dir: ".codex",
        prompt_args: &[],
    },
    TriageAgent {
        id: AgentId::Antigravity,
        name: "antigravity",
        command: "agy",
        label: "Antigravity",
        state_dir_env: "GEMINI_HOME",
        default_state_dir: ".gemini",
        prompt_args: &["--prompt-interactive"],
    },
];

#[derive(Debug, Args)]
pub(crate) struct TriageCommand {
    /// Agent CLI to use. Default: ask when several are installed.
    #[arg(long, value_enum)]
    agent: Option<AgentId>,
    /// Model passed through to the agent CLI. Default: the agent's default.
    #[arg(long)]
    model: Option<String>,
}

/// The picker's answer: its number, or the agent's name in any case.
pub(crate) fn match_agent_answer(agents: &[&TriageAgent], answer: &str) -> Option<usize> {
    let trimmed = answer.trim();
    (1..=agents.len())
        .find(|n| n.to_string() == trimmed)
        .map(|n| n - 1)
        .or_else(|| {
            let lower = trimmed.to_lowercase();
            agents.iter().position(|agent| agent.name == lower)
        })
}

/// Ask until the answer names an agent. `None` on end of input (Ctrl+D) or
/// a raw Ctrl+C / Ctrl+D byte.
pub(crate) fn pick_agent(
    agents: &[&TriageAgent],
    mut input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<Option<usize>> {
    let menu: Vec<String> = agents
        .iter()
        .enumerate()
        .map(|(index, agent)| format!("  [{}] {}", index + 1, agent.label))
        .collect();
    loop {
        write!(output, "Run triage with:\n{}\n> ", menu.join("\n"))?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 || line.contains(['\u{3}', '\u{4}']) {
            return Ok(None);
        }
        if let Some(index) = match_agent_answer(agents, &line) {
            return Ok(Some(index));
        }
    }
}

/// The picker on this terminal; Ctrl+C cancels it like end of input.
fn pick_agent_on_terminal(agents: &[&'static TriageAgent]) -> anyhow::Result<Option<usize>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let owned: Vec<&'static TriageAgent> = agents.to_vec();
    let picked = runtime.block_on(async move {
        let ask = tokio::task::spawn_blocking(move || {
            pick_agent(&owned, std::io::stdin().lock(), std::io::stdout())
        });
        tokio::select! {
            answer = ask => answer.map_err(anyhow::Error::from)?.map_err(anyhow::Error::from),
            interrupted = tokio::signal::ctrl_c() => interrupted.map(|()| None).map_err(anyhow::Error::from),
        }
    });
    // The reader may still wait on stdin after Ctrl+C; the command ends next.
    runtime.shutdown_background();
    picked
}

/// The executable `command` on `path` (a `PATH` value), as an absolute
/// path: the agent starts in the session folder, where a relative `PATH`
/// entry would point somewhere else. `extensions` are tried in order after
/// the bare name (Windows' `PATHEXT`; none elsewhere).
pub(crate) fn find_on_path(
    command: &str,
    path: &OsStr,
    cwd: &Path,
    extensions: &[String],
) -> Option<PathBuf> {
    let names: Vec<String> = std::iter::once(command.to_string())
        .chain(extensions.iter().map(|ext| format!("{command}{ext}")))
        .collect();
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| names.iter().map(move |name| cwd.join(&dir).join(name)))
        .find(|candidate| is_executable(candidate))
}

/// Executable extensions: Windows' `PATHEXT` (`.COM;.EXE;.BAT;.CMD` when
/// unset), nothing elsewhere.
pub(crate) fn executable_extensions(pathext: Option<&OsStr>) -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    pathext
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .map(str::trim)
        .filter(|ext| ext.starts_with('.'))
        .map(str::to_string)
        .collect()
}

/// The profile from `ZLO_PROFILE`, else the default with a line for the
/// context saying why: triage must start even when that setting is broken.
pub(crate) fn profile_or_diagnostic(
    value: Option<std::ffi::OsString>,
) -> (RebornProfile, Option<String>) {
    let shown = value
        .as_ref()
        .map(|v| v.to_string_lossy().chars().take(80).collect::<String>());
    match RebornProfile::from_env_value(value) {
        Ok(profile) => (profile, None),
        Err(_) => {
            let fallback = RebornProfile::default();
            (
                fallback,
                Some(format!(
                    "`{}` is set to `{}`, which is not a ZLO profile, so other `zlo` commands \
                     refuse to start. The paths below assume the default profile ({}). Valid \
                     values: {}.",
                    ironclaw_config::REBORN_PROFILE_ENV,
                    shown.unwrap_or_default().replace('`', "'"),
                    fallback.as_str(),
                    RebornProfile::all()
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            )
        }
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false; // silent-ok: an unreadable candidate is not a match
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn describe_files(session: &TriageSession) -> Vec<String> {
    vec![
        "The triage prompt and machine context were written to:".into(),
        format!("  {}", session.prompt_file.display()),
        format!("  {}", session.context_file.display()),
    ]
}

impl TriageCommand {
    /// Resolves the home on its own (no legacy-home move, no config file)
    /// and the profile leniently, then runs.
    pub(crate) fn execute_from_env(self) -> anyhow::Result<()> {
        let home = RebornHome::resolve_from_env_without_migrating()?;
        let (profile, diagnostic) = profile_or_diagnostic(ironclaw_config::profile_env_value());
        self.execute(&home, profile, diagnostic.into_iter().collect())
    }

    fn execute(
        self,
        home: &RebornHome,
        profile: RebornProfile,
        diagnostics: Vec<String>,
    ) -> anyhow::Result<()> {
        let user_home = crate::triage::user_home().unwrap_or_else(|| home.path().to_path_buf());
        let paths = TriagePaths::derive(home, profile, &user_home);
        let build = crate::commands::update::current_build();
        let launched_as = std::env::current_exe()
            .map(|exe| format!("{} triage", exe.display()))
            .unwrap_or_else(|error| {
                tracing::debug!(%error, "current_exe unavailable");
                "zlo triage".into()
            });
        let entry = TriageEntry::Cli {
            cli_version: crate::triage::describe_build(&build),
            launched_as,
        };
        let pid_file = read_pid_file(&paths.desktop_backend_pid_file, is_zlo_process);
        let server = describe_backend(&pid_file, crate::commands::status::resolve_service_state());
        let session = write_session(&SessionInput {
            paths: &paths,
            entry: &entry,
            server,
            source_clone: None,
            diagnostics: &diagnostics,
            now: chrono::Utc::now(),
        })?;
        if let PidFileState::Unreadable(reason) = &pid_file {
            tracing::debug!(%reason, "desktop backend pid file unreadable");
        }

        let path = std::env::var_os("PATH").unwrap_or_default();
        let cwd = std::env::current_dir()?;
        let extensions = executable_extensions(std::env::var_os("PATHEXT").as_deref());
        let installed: Vec<(&'static TriageAgent, PathBuf)> = TRIAGE_AGENTS
            .iter()
            .filter_map(|agent| {
                Some((
                    agent,
                    find_on_path(agent.command, &path, &cwd, &extensions)?,
                ))
            })
            .collect();

        let selected = match self.agent {
            Some(requested) => {
                let Some(found) = installed.iter().find(|(agent, _)| agent.id == requested) else {
                    let command = TRIAGE_AGENTS
                        .iter()
                        .find(|agent| agent.id == requested)
                        .map_or("agent", |agent| agent.command);
                    anyhow::bail!("`{command}` is not installed or was not found on PATH.");
                };
                Some(found)
            }
            None if installed.len() == 1 => installed.first(),
            None if installed.len() > 1 => {
                // The picker needs terminal output too, or its invisible prompt hangs.
                if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                    anyhow::bail!(
                        "Multiple supported agent CLIs are installed and there is no terminal \
                         to ask which to use. Re-run with --agent claude, --agent codex, or \
                         --agent antigravity."
                    );
                }
                let agents: Vec<&'static TriageAgent> =
                    installed.iter().map(|(agent, _)| *agent).collect();
                match pick_agent_on_terminal(&agents)? {
                    Some(index) => installed.get(index),
                    None => {
                        println!("\nNo agent started.");
                        return Ok(());
                    }
                }
            }
            None => None,
        };

        let Some((agent, command)) = selected else {
            let mut lines = vec![
                "No supported agent CLI (claude, codex, agy) was found on this machine.".into(),
                String::new(),
            ];
            lines.extend(describe_files(&session));
            lines.push(String::new());
            lines.push("Paste the prompt file into any coding agent to run triage by hand.".into());
            println!("{}", lines.join("\n"));
            return Ok(());
        };

        let launch = launch_prompt(&session.prompt_file);
        let mut args: Vec<String> = Vec::new();
        if let Some(model) = &self.model {
            args.extend(["--model".to_string(), model.clone()]);
        }
        args.extend(agent.prompt_args.iter().map(|arg| (*arg).to_string()));
        args.push(launch.clone());
        println!("Starting {}. It will ask what went wrong.\n", agent.label);
        let status = std::process::Command::new(command)
            .args(&args)
            .current_dir(&session.session_dir)
            .status()
            .map_err(|error| anyhow::anyhow!("Could not start `{}`: {error}", agent.command))?;
        let code = status
            .code()
            .unwrap_or(if status.success() { 0 } else { 1 });
        if code == 0 {
            return Ok(());
        }
        let state_dir = runtime_state_dir(&user_home, agent.state_dir_env, agent.default_state_dir);
        let mut lines = vec![
            String::new(),
            format!("{} exited with code {code}.", agent.label),
            String::new(),
            format!(
                "{} keeps its state in {}.",
                agent.label,
                state_dir.display()
            ),
            format!(
                "If that drive is out of space, free some up or set {} to a folder on another drive.",
                agent.state_dir_env
            ),
            String::new(),
        ];
        lines.extend(describe_files(&session));
        lines.push(String::new());
        lines.push(format!(
            "To retry, start `{}` yourself with any flags it suggested, or use any coding agent, \
             and give it this prompt:",
            agent.command
        ));
        lines.push(launch);
        println!("{}", lines.join("\n"));
        // The agent's own exit code, as `zlo triage`'s.
        std::process::exit(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents() -> Vec<&'static TriageAgent> {
        vec![&TRIAGE_AGENTS[0], &TRIAGE_AGENTS[1]]
    }

    #[test]
    fn the_picker_reads_numbers_and_names_only() {
        let agents = agents();
        for (answer, picked) in [
            ("2", Some(1)),
            (" 1 ", Some(0)),
            ("CLAUDE", Some(0)),
            ("1.5", None),
            ("1x", None),
            ("2codex", None),
            ("3", None),
            ("0", None),
        ] {
            assert_eq!(match_agent_answer(&agents, answer), picked, "{answer:?}");
        }
    }

    #[test]
    fn the_picker_asks_again_after_an_invalid_answer() {
        let mut output = Vec::new();
        let picked = pick_agent(&agents(), "nope\n2\n".as_bytes(), &mut output).expect("io");
        assert_eq!(picked, Some(1));
        let shown = String::from_utf8(output).expect("utf-8");
        assert_eq!(shown.matches("Run triage with:").count(), 2);
        assert!(shown.contains("  [1] Claude Code\n  [2] Codex\n> "));
    }

    #[test]
    fn the_picker_is_cancelled_by_end_of_input_and_control_keys() {
        for input in ["", "\u{3}", "\u{4}\n"] {
            let picked = pick_agent(&agents(), input.as_bytes(), Vec::new()).expect("io");
            assert_eq!(picked, None, "{input:?}");
        }
    }

    #[test]
    fn a_relative_path_entry_resolves_from_where_zlo_ran() {
        let tmp = tempfile::tempdir().expect("tmp");
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).expect("bin");
        let agent = bin.join("codex");
        std::fs::write(&agent, "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            std::fs::write(bin.join("claude"), "not executable").expect("write");
            assert_eq!(
                find_on_path("claude", OsStr::new("bin"), tmp.path(), &[]),
                None
            );
        }
        assert_eq!(
            find_on_path("codex", OsStr::new("bin"), tmp.path(), &[]),
            Some(tmp.path().join("bin").join("codex"))
        );
        assert_eq!(
            find_on_path("agy", OsStr::new("bin"), tmp.path(), &[]),
            None
        );
    }

    /// Windows names executables by extension (`codex.CMD` from npm): the
    /// bare name is tried first, then each `PATHEXT` extension in order.
    #[test]
    fn pathext_extensions_resolve_after_the_bare_name() {
        let tmp = tempfile::tempdir().expect("tmp");
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).expect("bin");
        let agent = bin.join("codex.CMD");
        std::fs::write(&agent, "@echo off\r\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let exts = [".EXE".to_string(), ".CMD".to_string()];
        assert_eq!(
            find_on_path("codex", OsStr::new("bin"), tmp.path(), &exts),
            Some(agent)
        );
        assert_eq!(
            find_on_path("codex", OsStr::new("bin"), tmp.path(), &[]),
            None
        );
        if cfg!(windows) {
            assert_eq!(
                executable_extensions(Some(OsStr::new(".EXE; .CMD;junk"))),
                exts
            );
        } else {
            assert!(executable_extensions(Some(OsStr::new(".EXE"))).is_empty());
        }
    }

    #[test]
    fn an_unknown_profile_falls_back_with_a_diagnostic() {
        let (profile, diagnostic) = profile_or_diagnostic(Some("no-such`profile".into()));
        assert_eq!(profile, RebornProfile::default());
        let diagnostic = diagnostic.expect("a diagnostic");
        assert!(diagnostic.contains("ZLO_PROFILE"), "{diagnostic}");
        assert!(diagnostic.contains("no-such'profile"), "{diagnostic}");
        assert_eq!(
            profile_or_diagnostic(None),
            (RebornProfile::default(), None)
        );
    }
}
