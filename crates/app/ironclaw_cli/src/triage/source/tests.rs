use std::sync::Mutex;

use super::*;

const MAIN: &str = "a1dc856ee61278d0199c36d4bcdef5fdea4d21fe";
const BUILD: &str = "7027b1f9c2a1d3e4f5a6b7c8d9e0f1a2b3c4d5e6";

#[test]
fn picks_the_build_commit_when_the_build_records_one() {
    let listed = format!("{MAIN}\trefs/heads/main\n");
    assert_eq!(
        pick_source_ref(&listed, Some(BUILD)),
        Ok(SourceRef {
            kind: SourceRefKind::BuildCommit,
            commit: BUILD.into(),
        })
    );
}

#[test]
fn falls_back_to_main_and_to_nothing() {
    let listed = format!("1111111\trefs/heads/mainline\n{MAIN}\trefs/heads/main\n");
    assert_eq!(
        pick_source_ref(&listed, None),
        Ok(SourceRef {
            kind: SourceRefKind::Main,
            commit: MAIN.into(),
        })
    );
    assert_eq!(pick_source_ref("", None), Err(SourceFailure::NoMainBranch));
    assert_eq!(
        pick_source_ref("garbage\n", None),
        Err(SourceFailure::NoMainBranch)
    );
}

/// A commit becomes a folder name and a git argument: only full hashes.
#[test]
fn only_full_hashes_are_picked() {
    for hostile in [
        "../../etc",
        "--upload-pack=touch /tmp/x",
        "7027b1f",
        "A1DC856EE61278D0199C36D4BCDEF5FDEA4D21FE",
        "a1dc856ee61278d0199c36d4bcdef5fdea4d21f ",
    ] {
        let listed = format!("{hostile}\trefs/heads/main\n");
        assert_eq!(
            pick_source_ref(&listed, None),
            Err(SourceFailure::InvalidCommit),
            "{hostile:?}"
        );
        assert_eq!(
            pick_source_ref("", Some(hostile)),
            Err(SourceFailure::InvalidCommit),
            "{hostile:?}"
        );
    }
}

/// Answers each git command; `fetch` can be held until released.
struct StubGit {
    commands: Mutex<Vec<(PathBuf, String)>>,
    head: String,
    hold_fetch: Option<tokio::sync::Semaphore>,
    ls_remote: Result<String, String>,
}

impl StubGit {
    fn new(head: &str) -> Self {
        Self {
            commands: Mutex::new(Vec::new()),
            head: head.into(),
            hold_fetch: None,
            ls_remote: Ok(format!("{MAIN}\trefs/heads/main\n")),
        }
    }
    fn commands(&self) -> Vec<String> {
        self.commands
            .lock()
            .expect("lock")
            .iter()
            .map(|(_, c)| c.clone())
            .collect()
    }
    fn count(&self, command: &str) -> usize {
        self.commands().iter().filter(|c| *c == command).count()
    }
}

#[async_trait]
impl GitRunner for StubGit {
    async fn run(&self, cwd: &Path, args: &[&str], _t: Duration) -> Result<String, GitError> {
        let command = args[0].to_string();
        self.commands
            .lock()
            .expect("lock")
            .push((cwd.to_path_buf(), command.clone()));
        match command.as_str() {
            "ls-remote" => self.ls_remote.clone().map_err(|stderr| GitError::Failed {
                operation: command,
                stderr,
            }),
            "rev-parse" => Ok(format!("{}\n", self.head)),
            "init" => {
                std::fs::create_dir_all(cwd.join(".git")).expect("init");
                Ok(String::new())
            }
            "fetch" => {
                if let Some(gate) = &self.hold_fetch {
                    let _permit = gate.acquire().await;
                }
                Ok(String::new())
            }
            _ => Ok(String::new()),
        }
    }
}

fn cache(home: &Path, git: Arc<StubGit>, commit: Option<&str>) -> SourceCache {
    SourceCache::new(home, git, "1.2.0".into(), commit.map(str::to_string))
}

async fn until(what: impl Fn() -> bool) {
    for _ in 0..200 {
        if what() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn sessions_started_during_a_fetch_share_it() {
    let home = tempfile::tempdir().expect("home");
    let mut stub = StubGit::new(BUILD);
    stub.hold_fetch = Some(tokio::sync::Semaphore::new(0));
    let git = Arc::new(stub);
    let cache = cache(home.path(), Arc::clone(&git), Some(BUILD));

    let first = cache.start().await;
    let second = cache.start().await;
    assert!(matches!(first, SourceClone::Cloning { .. }), "{first:?}");
    assert_eq!(first, second);
    until(|| git.count("fetch") == 1).await;
    assert_eq!(git.count("init"), 1);
    assert_eq!(
        git.count("ls-remote"),
        0,
        "the build commit needs no lookup"
    );
}

/// Two backend processes (two caches on one home): the second shares the
/// first's fetch through the lock instead of starting its own, and keeps
/// the first's live staging folder when it starts up.
#[tokio::test]
async fn overlapping_starts_across_processes_share_one_fetch() {
    let home = tempfile::tempdir().expect("home");
    let mut stub = StubGit::new(BUILD);
    stub.hold_fetch = Some(tokio::sync::Semaphore::new(0));
    let git = Arc::new(stub);
    let first = cache(home.path(), Arc::clone(&git), Some(BUILD));
    assert!(matches!(first.start().await, SourceClone::Cloning { .. }));
    until(|| git.count("fetch") == 1).await;

    let second = cache(home.path(), Arc::clone(&git), Some(BUILD));
    let staged = std::fs::read_dir(home.path().join("source-staging"))
        .expect("staging")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("clone-"))
        .count();
    assert_eq!(staged, 1, "a live fetch's staging folder is not reclaimed");
    assert!(matches!(second.start().await, SourceClone::Cloning { .. }));
    assert_eq!(git.count("init"), 1, "one fetch for both processes");

    git.hold_fetch.as_ref().expect("gate").add_permits(1);
    let directory = home.path().join("source").join(BUILD);
    until(|| directory.join(".git").is_dir()).await;
    // The lock is released with the fetch; the next start reuses it.
    until(|| {
        matches!(
            try_lock_fetch(&home.path().join("source-staging"), BUILD),
            Ok(Some(_))
        )
    })
    .await;
    assert!(matches!(second.start().await, SourceClone::Cloned { .. }));
}

#[tokio::test]
async fn a_finished_fetch_appears_in_the_cache_by_rename() {
    let home = tempfile::tempdir().expect("home");
    let git = Arc::new(StubGit::new(MAIN));
    let cache = cache(home.path(), Arc::clone(&git), None);
    let SourceClone::Cloning {
        directory, source, ..
    } = cache.start().await
    else {
        panic!("expected a fetch to start");
    };
    assert_eq!(source.kind, SourceRefKind::Main);
    assert_eq!(directory, home.path().join("source").join(MAIN));
    until(|| directory.is_dir()).await;
    assert!(directory.join(".git").is_dir(), "the staged fetch moved in");
    let staged = std::fs::read_dir(home.path().join("source-staging"))
        .expect("staging")
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().ends_with(".lock"))
        .count();
    assert_eq!(staged, 0, "staging is empty afterwards");
}

/// A fetch that checks out something else never moves into the cache.
#[tokio::test]
async fn a_fetch_is_verified_before_it_moves_in() {
    let home = tempfile::tempdir().expect("home");
    let git = Arc::new(StubGit::new(MAIN)); // HEAD is not the build commit
    let cache = cache(home.path(), Arc::clone(&git), Some(BUILD));
    assert!(matches!(cache.start().await, SourceClone::Cloning { .. }));
    until(|| git.count("rev-parse") == 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!home.path().join("source").join(BUILD).exists());
}

#[tokio::test]
async fn reuses_a_finished_checkout_without_fetching() {
    let home = tempfile::tempdir().expect("home");
    std::fs::create_dir_all(home.path().join(format!("source/{BUILD}/.git"))).expect("seed");
    let git = Arc::new(StubGit::new(BUILD));
    let cache = cache(home.path(), Arc::clone(&git), Some(BUILD));
    assert!(matches!(cache.start().await, SourceClone::Cloned { .. }));
    assert_eq!(git.commands(), vec!["rev-parse"]);
}

#[tokio::test]
async fn an_invalid_destination_is_never_accepted() {
    // No .git of its own: a parent repository could answer rev-parse.
    let home = tempfile::tempdir().expect("home");
    let directory = home.path().join("source").join(BUILD);
    std::fs::create_dir_all(&directory).expect("seed");
    let git = Arc::new(StubGit::new(BUILD));
    let cache = cache(home.path(), Arc::clone(&git), Some(BUILD));
    let SourceClone::Failed { reason, .. } = cache.start().await else {
        panic!("expected a failure");
    };
    assert_eq!(
        reason,
        SourceFailure::NotACheckout {
            directory: directory.clone(),
            commit: BUILD.into()
        }
    );
    assert!(!git.commands().iter().any(|c| c == "rev-parse"));

    // Its own .git, at another commit.
    std::fs::create_dir_all(directory.join(".git")).expect("seed");
    let git = Arc::new(StubGit::new(MAIN));
    let cache = super::SourceCache::new(home.path(), git, "1.2.0".into(), Some(BUILD.into()));
    assert!(matches!(
        cache.start().await,
        SourceClone::Failed {
            reason: SourceFailure::NotACheckout { .. },
            ..
        }
    ));

    // A symlinked .git is not its own.
    std::fs::remove_dir_all(directory.join(".git")).expect("rm");
    let elsewhere = home.path().join("elsewhere/.git");
    std::fs::create_dir_all(&elsewhere).expect("elsewhere");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&elsewhere, directory.join(".git")).expect("symlink");
    let cache = super::SourceCache::new(
        home.path(),
        Arc::new(StubGit::new(BUILD)),
        "1.2.0".into(),
        Some(BUILD.into()),
    );
    assert!(matches!(cache.start().await, SourceClone::Failed { .. }));
}

/// A folder that appears in the cache while the fetch runs is kept only
/// when it is the same complete checkout.
#[tokio::test]
async fn a_folder_that_wins_the_race_is_verified() {
    let home = tempfile::tempdir().expect("home");
    let mut stub = StubGit::new(BUILD);
    stub.hold_fetch = Some(tokio::sync::Semaphore::new(0));
    let git = Arc::new(stub);
    let cache = cache(home.path(), Arc::clone(&git), Some(BUILD));
    assert!(matches!(cache.start().await, SourceClone::Cloning { .. }));
    until(|| git.count("fetch") == 1).await;
    let directory = home.path().join("source").join(BUILD);
    std::fs::create_dir_all(&directory).expect("hand clone");
    std::fs::write(directory.join("README"), "half a clone").expect("write");
    git.hold_fetch.as_ref().expect("gate").add_permits(1);
    until(|| {
        matches!(
            try_lock_fetch(&home.path().join("source-staging"), BUILD),
            Ok(Some(_))
        )
    })
    .await;
    assert!(
        !directory.join(".git").exists(),
        "the hand clone is left alone"
    );
    assert!(matches!(
        cache.start().await,
        SourceClone::Failed {
            reason: SourceFailure::NotACheckout { .. },
            ..
        }
    ));
}

#[tokio::test]
async fn a_failed_lookup_answers_a_typed_reason_without_git_output() {
    let home = tempfile::tempdir().expect("home");
    let mut stub = StubGit::new("");
    stub.ls_remote =
        Err("fatal: unable to access 'https://user:ghp_secret@github.com/': Could not resolve host: github.com".into());
    let cache = cache(home.path(), Arc::new(stub), None);
    let SourceClone::Failed { reason, build } = cache.start().await else {
        panic!("expected a failure");
    };
    assert_eq!(build, "1.2.0");
    assert_eq!(reason, SourceFailure::Network);
    assert!(!reason.summary().contains("ghp_secret"));

    let mut stub = StubGit::new("");
    stub.ls_remote = Err("remote: Invalid username or token ghp_secret".into());
    let cache = super::SourceCache::new(home.path(), Arc::new(stub), "1.2.0".into(), None);
    let SourceClone::Failed { reason, .. } = cache.start().await else {
        panic!("expected a failure");
    };
    assert_eq!(
        reason,
        SourceFailure::GitFailed {
            operation: "ls-remote".into()
        }
    );
    assert!(
        !reason.summary().contains("ghp_secret"),
        "{}",
        reason.summary()
    );
}

#[tokio::test]
async fn a_hostile_main_hash_never_names_a_folder() {
    let home = tempfile::tempdir().expect("home");
    let mut stub = StubGit::new("");
    stub.ls_remote = Ok("../../../../escape\trefs/heads/main\n".into());
    let git = Arc::new(stub);
    let cache = cache(home.path(), Arc::clone(&git), None);
    assert!(matches!(
        cache.start().await,
        SourceClone::Failed {
            reason: SourceFailure::InvalidCommit,
            ..
        }
    ));
    assert_eq!(git.commands(), vec!["ls-remote"]);
}

#[test]
fn only_abandoned_staging_folders_are_reclaimed() {
    let home = tempfile::tempdir().expect("home");
    let staging = home.path().join("source-staging");
    let live = staging.join("clone-4242-abc");
    let dead = staging.join("clone-4343-def");
    let unknown = staging.join("leftover");
    for dir in [&live, &dead, &unknown] {
        std::fs::create_dir_all(dir).expect("seed");
    }
    std::fs::write(staging.join(format!("{BUILD}.lock")), "").expect("lock");
    let now = SystemTime::now();
    reclaim_abandoned_staging(&staging, now, |pid| pid == 4242);
    assert!(live.is_dir(), "a live owner's fetch stays");
    assert!(!dead.exists(), "a dead owner's fetch goes");
    assert!(!unknown.exists(), "a folder no fetch owns goes");
    assert!(staging.join(format!("{BUILD}.lock")).exists());
    // Older than any fetch can run: its pid was reused.
    reclaim_abandoned_staging(&staging, now + STAGING_ABANDONED_AFTER * 2, |_| true);
    assert!(!live.exists());
}

/// The real [`ProcessGit`] against a recording fake `git`: what each run
/// gets as argv and environment, whatever the backend inherited.
#[cfg(unix)]
mod process {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// A fake git that records `argv` and `env` per run into `$FAKE_GIT_OUT`
    /// and answers `rev-parse` with `$FAKE_HEAD`.
    fn fake_git(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("git");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nn=$$\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$FAKE_GIT_OUT/argv.$n\"\nenv > \"$FAKE_GIT_OUT/env.$n\"\n{body}\n"
            ),
        )
        .expect("fake git");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// `program` as git, starting from `inherited`.
    fn fake_runner(program: &Path, inherited: Vec<(OsString, OsString)>) -> ProcessGit {
        ProcessGit {
            program: program.as_os_str().to_os_string(),
            inherited: Some(inherited),
        }
    }

    fn hostile_env(out: &Path) -> Vec<(OsString, OsString)> {
        [
            ("PATH", std::env::var("PATH").unwrap_or_default()),
            ("HOME", "/home/u".into()),
            ("FAKE_GIT_OUT", out.display().to_string()),
            ("FAKE_HEAD", BUILD.into()),
            ("GIT_DIR", "/evil/.git".into()),
            ("GIT_WORK_TREE", "/evil".into()),
            ("GIT_INDEX_FILE", "/evil/index".into()),
            ("GIT_OBJECT_DIRECTORY", "/evil/objects".into()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/evil/alt".into()),
            ("GIT_CONFIG", "/evil/config".into()),
            ("GIT_CONFIG_GLOBAL", "/evil/gitconfig".into()),
            ("GIT_CONFIG_COUNT", "1".into()),
            ("GIT_CONFIG_KEY_0", "core.sshCommand".into()),
            ("GIT_CONFIG_VALUE_0", "touch /tmp/pwned".into()),
            ("GIT_CONFIG_PARAMETERS", "'core.hooksPath'='/evil'".into()),
            ("GIT_SSH_COMMAND", "touch /tmp/pwned".into()),
            ("GIT_ASKPASS", "/evil/askpass".into()),
            ("SSH_ASKPASS", "/evil/askpass".into()),
            ("GIT_EXEC_PATH", "/evil/libexec".into()),
            ("GIT_TERMINAL_PROMPT", "1".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
    }

    /// Every recorded run: `(argv, env)`.
    /// One recorded run: its argv and its environment.
    type Run = (Vec<String>, Vec<(String, String)>);

    fn runs(out: &Path) -> Vec<Run> {
        let mut runs = Vec::new();
        for entry in std::fs::read_dir(out).expect("out").flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(n) = name.strip_prefix("argv.") else {
                continue;
            };
            let argv = std::fs::read_to_string(entry.path()).expect("argv");
            let env = std::fs::read_to_string(out.join(format!("env.{n}"))).expect("env");
            runs.push((
                argv.lines().map(str::to_string).collect(),
                env.lines()
                    .filter_map(|l| l.split_once('='))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ));
        }
        runs
    }

    fn assert_isolated(argv: &[String], env: &[(String, String)], cwd_parent: &Path) {
        let get = |name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        for routed in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_PARAMETERS",
            "GIT_SSH_COMMAND",
            "GIT_EXEC_PATH",
        ] {
            assert_eq!(get(routed), None, "{routed} reached git: {env:?}");
        }
        assert!(!env.iter().any(|(_, v)| v.contains("/evil")), "{env:?}");
        assert_eq!(get("GIT_CONFIG_NOSYSTEM"), Some("1"));
        assert_eq!(get("GIT_CONFIG_GLOBAL"), Some("/dev/null"));
        assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0"));
        assert_eq!(get("GIT_ASKPASS"), Some(""));
        assert_eq!(get("SSH_ASKPASS"), Some(""));
        assert_eq!(get("GIT_ALLOW_PROTOCOL"), Some("https"));
        assert_eq!(
            get("GIT_CEILING_DIRECTORIES"),
            Some(cwd_parent.to_str().expect("utf-8"))
        );
        assert_eq!(get("HOME"), Some("/home/u"), "the rest is inherited");
        let config: Vec<&str> = argv
            .chunks(2)
            .take_while(|pair| pair[0] == "-c")
            .map(|pair| pair[1].as_str())
            .collect();
        for setting in [
            "core.hooksPath=/dev/null",
            "credential.helper=",
            "core.askPass=",
            "protocol.allow=never",
            "protocol.https.allow=always",
        ] {
            assert!(config.contains(&setting), "{setting} missing: {argv:?}");
        }
    }

    /// The backend's own fetch path (`SourceCache` → `ProcessGit`): every
    /// step it runs is isolated from the environment it inherited.
    #[tokio::test]
    async fn every_fetch_step_runs_isolated() {
        let tmp = tempfile::tempdir().expect("tmp");
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).expect("out");
        let git = fake_git(
            tmp.path(),
            "case \"$*\" in *rev-parse*) echo \"$FAKE_HEAD\";; *\" init \"*) mkdir -p .git;; esac",
        );
        let home = tmp.path().join("home");
        let cache = SourceCache::new(
            &home,
            Arc::new(fake_runner(&git, hostile_env(&out))),
            "1.2.0".into(),
            Some(BUILD.into()),
        );
        assert!(matches!(cache.start().await, SourceClone::Cloning { .. }));
        let directory = home.join("source").join(BUILD);
        until(|| directory.join(".git").is_dir()).await;
        let runs = runs(&out);
        let ops: Vec<&str> = runs
            .iter()
            .map(|(argv, _)| {
                argv.iter()
                    .find(|a| *a != "-c" && !a.contains('='))
                    .map_or("", String::as_str)
            })
            .collect();
        for op in ["init", "remote", "fetch", "checkout", "rev-parse"] {
            assert!(ops.contains(&op), "{op} ran: {ops:?}");
        }
        let staging = home.join("source-staging");
        for (argv, env) in &runs {
            assert_isolated(argv, env, &staging);
        }
        let fetch = runs
            .iter()
            .find(|(argv, _)| argv.contains(&"fetch".to_string()))
            .expect("fetch");
        assert_eq!(fetch.0.last().map(String::as_str), Some(BUILD));
    }

    #[tokio::test]
    async fn ls_remote_runs_isolated() {
        let tmp = tempfile::tempdir().expect("tmp");
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).expect("out");
        let git = fake_git(
            tmp.path(),
            "printf '%s\\trefs/heads/main\\n' \"$FAKE_HEAD\"",
        );
        let runner = fake_runner(&git, hostile_env(&out));
        let cwd = tmp.path().join("cache");
        std::fs::create_dir(&cwd).expect("cwd");
        let listed = runner
            .run(
                &cwd,
                &["ls-remote", ZLO_REPOSITORY_URL, "refs/heads/main"],
                Duration::from_secs(5),
            )
            .await
            .expect("ls-remote");
        assert_eq!(listed, format!("{BUILD}\trefs/heads/main\n"));
        let runs = runs(&out);
        assert_eq!(runs.len(), 1);
        assert_isolated(&runs[0].0, &runs[0].1, tmp.path());
    }

    /// A hung git and anything it started die at the timeout.
    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_group() {
        let tmp = tempfile::tempdir().expect("tmp");
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).expect("out");
        let git = fake_git(
            tmp.path(),
            "sleep 30 &\necho $! > \"$FAKE_GIT_OUT/child.pid\"\nwait",
        );
        let runner = fake_runner(&git, hostile_env(&out));
        let error = runner
            .run(tmp.path(), &["fetch"], Duration::from_millis(500))
            .await
            .expect_err("times out");
        assert!(matches!(error, GitError::TimedOut { .. }), "{error:?}");
        let child: u32 = std::fs::read_to_string(out.join("child.pid"))
            .expect("child pid")
            .trim()
            .parse()
            .expect("pid");
        until(|| !pid_alive(child)).await;
        assert!(!pid_alive(child), "the grandchild {child} still runs");
    }
}
