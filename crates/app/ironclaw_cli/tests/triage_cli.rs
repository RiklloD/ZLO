//! `zlo triage` through the real binary, with fake agent CLIs on an
//! isolated `PATH` that record how they were launched (so no real agent can
//! ever run). Port of t3code's `apps/server/src/cli/triage.test.ts`.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    /// Fake `commands` in `<root>/bin`, each recording its name, its
    /// arguments (one per line) and its working folder.
    fn new(commands: &[&str]) -> Self {
        let root = tempfile::tempdir().expect("tmp");
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).expect("bin");
        for command in commands {
            write_script(
                &bin.join(command),
                &format!(
                    "#!/bin/sh\n{{ echo \"{command}\"; for a in \"$@\"; do echo \"arg:$a\"; done; echo \"cwd:$(pwd -P)\"; echo \"path:$PATH\"; }} > \"$TRIAGE_TEST_RECORD\"\n"
                ),
            );
        }
        Self { root }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn record(&self) -> PathBuf {
        self.path().join("launch.txt")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zlo"));
        command
            .env_clear()
            .env("PATH", self.path().join("bin"))
            .env("HOME", self.path())
            .env("ZLO_HOME", self.path().join("zlo home"))
            .env("TRIAGE_TEST_RECORD", self.record())
            .env("IRONCLAW_DISABLE_OS_KEYCHAIN", "1")
            .current_dir(self.path())
            .stdin(Stdio::null())
            .arg("triage")
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run zlo")
    }

    /// A fake `command` in `<root>/<dir>` (a folder under the fake `HOME`),
    /// recording itself as `<name>` like the ones on `PATH`.
    fn install_in_home(&self, dir: &str, command: &str, name: &str) -> PathBuf {
        let folder = self.path().join(dir);
        std::fs::create_dir_all(&folder).expect("folder");
        write_script(
            &folder.join(command),
            &format!(
                "#!/bin/sh\n{{ echo \"{name}\"; for a in \"$@\"; do echo \"arg:$a\"; done; echo \"cwd:$(pwd -P)\"; echo \"path:$PATH\"; }} > \"$TRIAGE_TEST_RECORD\"\n"
            ),
        );
        folder
    }

    /// The `PATH` the fake agent started with.
    fn launch_path(&self) -> String {
        let text = std::fs::read_to_string(self.record()).expect("agent ran");
        text.lines()
            .find_map(|line| line.strip_prefix("path:"))
            .expect("path recorded")
            .to_string()
    }

    /// `(command, args, cwd)` the fake agent recorded.
    fn launch(&self) -> (String, Vec<String>, PathBuf) {
        let text = std::fs::read_to_string(self.record()).expect("agent ran");
        let mut lines = text.lines();
        let command = lines.next().expect("name").to_string();
        let mut args = Vec::new();
        let mut cwd = PathBuf::new();
        for line in lines {
            if let Some(arg) = line.strip_prefix("arg:") {
                args.push(arg.to_string());
            } else if let Some(dir) = line.strip_prefix("cwd:") {
                cwd = PathBuf::from(dir);
            }
        }
        (command, args, cwd)
    }
}

fn write_script(path: &Path, text: &str) {
    std::fs::write(path, text).expect("script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn launches_the_chosen_agent_with_the_triage_prompt() {
    for (commands, agent, expected, model) in [
        (
            &["agy", "codex"][..],
            Some("antigravity"),
            "agy",
            Some("test-model"),
        ),
        (&["agy"][..], None, "agy", None),
        (
            &["agy", "codex"][..],
            Some("codex"),
            "codex",
            Some("test-model"),
        ),
        (
            &["agy", "codex", "claude"][..],
            Some("claude"),
            "claude",
            None,
        ),
    ] {
        let fixture = Fixture::new(commands);
        let mut args = Vec::new();
        if let Some(agent) = agent {
            args.extend(["--agent", agent]);
        }
        if let Some(model) = model {
            args.extend(["--model", model]);
        }
        let output = fixture.run(&args);
        assert!(output.status.success(), "{}", text(&output));
        let (command, launched, cwd) = fixture.launch();
        assert_eq!(command, expected);
        let mut flags: Vec<String> = Vec::new();
        if let Some(model) = model {
            flags.extend(["--model".into(), model.into()]);
        }
        if expected == "agy" {
            flags.push("--prompt-interactive".into());
        }
        assert_eq!(launched[..launched.len() - 1], flags[..], "{expected}");
        let prompt_file = cwd.join("triage-playbook.md");
        let last = launched.last().expect("prompt");
        let named = last
            .split('"')
            .nth(1)
            .and_then(|path| Path::new(path).canonicalize().ok())
            .expect("the launch prompt quotes an existing file");
        assert_eq!(named, prompt_file, "{last}");
        let seed = std::fs::read_to_string(&prompt_file).expect("prompt file");
        assert!(seed.contains("context.md"));
        assert!(
            cwd.starts_with(
                fixture
                    .path()
                    .canonicalize()
                    .expect("canon")
                    .join("zlo home/triage")
            ),
            "the session lives under ZLO_HOME: {}",
            cwd.display()
        );
    }
}

#[test]
fn requires_an_explicit_choice_without_a_terminal() {
    let fixture = Fixture::new(&["agy", "codex"]);
    let output = fixture.run(&[]);
    assert!(!output.status.success());
    let all = text(&output);
    assert!(all.contains("--agent antigravity"), "{all}");
    assert!(all.contains("--agent codex"), "{all}");
    assert!(!all.contains("Starting"), "{all}");
    assert!(!fixture.record().exists());
}

#[test]
fn reports_the_missing_agy_executable_when_antigravity_is_requested() {
    let fixture = Fixture::new(&["codex"]);
    let output = fixture.run(&["--agent", "antigravity"]);
    assert!(!output.status.success());
    assert!(
        text(&output).contains("`agy` is not installed or was not found on PATH."),
        "{}",
        text(&output)
    );
    assert!(!fixture.record().exists());
}

#[test]
fn with_no_agent_installed_it_points_at_the_prompt_file() {
    let fixture = Fixture::new(&[]);
    let output = fixture.run(&[]);
    assert!(output.status.success(), "{}", text(&output));
    let all = text(&output);
    assert!(all.contains("No supported agent CLI (claude, codex, agy) was found"));
    assert!(all.contains("triage-playbook.md"));
    assert!(all.contains("Paste the prompt file into any coding agent"));
    // Without a terminal nothing is installed, but the commands are shown.
    assert!(
        all.contains("curl -fsSL https://claude.ai/install.sh | bash"),
        "{all}"
    );
    assert!(
        all.contains("curl -fsSL https://chatgpt.com/codex/install.sh | sh"),
        "{all}"
    );
    assert!(!fixture.record().exists());
}

/// The native installers put the CLI in `~/.local/bin` (Claude's also in
/// `~/.claude/local`) and do not always add it to `PATH`: the app finds it
/// there, so triage must too, and say so.
#[test]
fn an_agent_installed_off_path_in_the_home_is_found_and_started() {
    for dir in [".local/bin", ".claude/local"] {
        let fixture = Fixture::new(&[]);
        let folder = fixture.install_in_home(dir, "claude", "claude");
        let output = fixture.run(&[]);
        assert!(output.status.success(), "{dir}: {}", text(&output));
        let all = text(&output);
        assert!(
            all.contains(&format!(
                "Found Claude Code in {} (not on your PATH).",
                folder.display()
            )),
            "{dir}: {all}"
        );
        let (command, _, cwd) = fixture.launch();
        assert_eq!(command, "claude", "{dir}");
        // Its folder leads the agent's PATH, so what it starts is found too.
        let path = fixture.launch_path();
        assert!(
            path.starts_with(&format!("{}:", folder.display())),
            "{dir}: {path}"
        );
        let context = std::fs::read_to_string(cwd.join("context.md")).expect("context");
        assert!(
            context.contains("## Problems found while starting triage"),
            "{context}"
        );
        assert!(
            context.contains(&folder.join("claude").display().to_string()),
            "{context}"
        );
        assert!(context.contains("not on this terminal's PATH"), "{context}");
    }
}

#[test]
fn an_agent_on_path_wins_over_one_in_the_home() {
    let fixture = Fixture::new(&["claude"]);
    fixture.install_in_home(".local/bin", "claude", "home-claude");
    let output = fixture.run(&["--agent", "claude"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(!text(&output).contains("not on your PATH"));
    let (command, _, _) = fixture.launch();
    assert_eq!(command, "claude");
    assert_eq!(
        fixture.launch_path(),
        fixture.path().join("bin").display().to_string()
    );
}

/// Two agents, one only in the home folder: both count, so with no
/// terminal triage asks for `--agent` instead of starting the one on PATH.
#[test]
fn agents_in_the_home_count_toward_the_choice() {
    let fixture = Fixture::new(&["codex"]);
    fixture.install_in_home(".local/bin", "agy", "agy");
    let output = fixture.run(&[]);
    assert!(!output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("--agent antigravity"));
    let output = fixture.run(&["--agent", "antigravity"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(fixture.launch().0, "agy");
}

#[test]
fn a_failed_agent_launch_points_at_the_prompt_it_could_not_run() {
    let fixture = Fixture::new(&[]);
    write_script(
        &fixture.path().join("bin/codex"),
        "#!/bin/sh\necho 'Error: There is not enough space on the disk.' >&2\nexit 3\n",
    );
    let codex_home = fixture.path().join("codex-home");
    let output = fixture
        .command(&["--agent", "codex"])
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(3), "the agent's code is ours");
    let all = text(&output);
    assert!(all.contains("Codex exited with code 3."), "{all}");
    assert!(all.contains(&format!(
        "Codex keeps its state in {}.",
        codex_home.display()
    )));
    assert!(all.contains("set CODEX_HOME to a folder on another drive"));
    assert!(all.contains("triage-playbook.md"));
    assert!(all.contains("context.md"));
    assert!(all.contains("Read the file \""), "the retry prompt: {all}");
}

#[test]
fn an_agent_on_a_relative_path_entry_starts_from_the_session_folder() {
    let fixture = Fixture::new(&[]);
    write_script(&fixture.path().join("bin/codex"), "#!/bin/sh\nexit 1\n");
    let output = fixture
        .command(&["--agent", "codex"])
        .env("PATH", "bin")
        .output()
        .expect("run");
    // The fake agent ran and exited 1, rather than failing to start.
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("Codex exited with code 1."));
}

/// A broken `ZLO_PROFILE` makes every other command refuse to start; triage
/// still writes its session and lists the problem for the agent.
#[test]
fn a_malformed_profile_is_a_diagnostic_not_a_failure() {
    let fixture = Fixture::new(&[]);
    let output = fixture
        .command(&[])
        .env("ZLO_PROFILE", "not-a-profile")
        .output()
        .expect("run");
    assert!(output.status.success(), "{}", text(&output));
    let all = text(&output);
    let prompt = all
        .lines()
        .map(str::trim)
        .find(|line| line.ends_with("triage-playbook.md"))
        .expect("the prompt file is named");
    let context = std::fs::read_to_string(Path::new(prompt).with_file_name("context.md"))
        .expect("context written");
    assert!(
        context.contains("## Problems found while starting triage"),
        "{context}"
    );
    assert!(
        context.contains("`ZLO_PROFILE` is set to `not-a-profile`"),
        "{context}"
    );
    assert!(
        context.contains(
            &fixture
                .path()
                .join("zlo home/local-dev")
                .display()
                .to_string()
        ),
        "default-profile paths: {context}"
    );
}
