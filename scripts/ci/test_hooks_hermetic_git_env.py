#!/usr/bin/env python3
"""Pin that our git hooks scrub the repository-routing env before running tests.

Git exports GIT_DIR to hooks; in a linked worktree it points at that
worktree's admin dir. A hook that runs tests (the pre-commit hook; the
pre-push hook until it was removed on 2026-11-27) lets test fixtures run `git -C <tempdir> init|config|commit`. With GIT_DIR
inherited those commands act on the real repository: on 2026-11-24 they
committed fixtures onto `main` and wrote core.bare=true, a fake identity and
an include.path into .git/config.

The test builds a throwaway repo with a linked worktree, installs a pre-push
hook made of our pre-commit hook's preamble (everything up to its first non-preamble
line) followed by a fixture-style `git -C <tmp> init && commit`, pushes from
the worktree, and checks that the repo it pushed from is untouched.
"""

from __future__ import annotations

import os
import pathlib
import subprocess
import tempfile
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
HOOKS = ["pre-commit"]
SCRUBBED = "unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE"


def preamble(hook: pathlib.Path) -> str:
    """The hook's lines up to and including its env-scrub `unset` statement."""
    lines = hook.read_text().splitlines(keepends=True)
    out: list[str] = []
    in_unset = False
    for line in lines:
        out.append(line)
        if line.startswith(SCRUBBED):
            in_unset = line.rstrip().endswith("\\")
            if not in_unset:
                return "".join(out)
        elif in_unset:
            in_unset = line.rstrip().endswith("\\")
            if not in_unset:
                return "".join(out)
    raise AssertionError(f"{hook} has no `{SCRUBBED} …` preamble")


def git(*args: str, cwd: pathlib.Path) -> str:
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
    return subprocess.run(
        ["git", "-c", "user.email=a@b", "-c", "user.name=a", *args],
        cwd=cwd, env=env, check=True, capture_output=True, text=True,
    ).stdout.strip()


class HooksScrubGitEnv(unittest.TestCase):
    def test_every_test_running_hook_scrubs_first(self) -> None:
        for name in HOOKS:
            with self.subTest(hook=name):
                preamble(REPO_ROOT / ".githooks" / name)
        preamble(REPO_ROOT / "scripts" / "ci" / "quality_gate.sh")

    def test_fixture_git_in_pre_push_leaves_the_repo_alone(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            git("init", "-q", "--bare", str(root / "remote.git"), cwd=root)
            repo = root / "repo"
            git("init", "-q", "-b", "main", str(repo), cwd=root)
            git("commit", "-q", "--allow-empty", "-m", "base", cwd=repo)
            git("remote", "add", "origin", str(root / "remote.git"), cwd=repo)
            # The worktree holds `main`, like the merge worktree did.
            git("checkout", "-q", "-b", "other", cwd=repo)
            git("worktree", "add", "-q", str(root / "wt"), "main", cwd=repo)
            fixture = root / "fixture"
            hook = repo / ".git" / "hooks" / "pre-push"
            hook.parent.mkdir(parents=True, exist_ok=True)
            hook.write_text(
                preamble(REPO_ROOT / ".githooks" / "pre-commit")
                + f'mkdir -p "{fixture}"\n'
                + f'git -C "{fixture}" init -q\n'
                + f'git -C "{fixture}" config user.email t@example.com\n'
                + f'git -C "{fixture}" -c user.name=t commit -q --allow-empty -m init\n'
            )
            hook.chmod(0o755)
            before = git("rev-parse", "main", cwd=repo)
            git("push", "-q", "origin", "main", cwd=root / "wt")
            self.assertEqual(git("rev-parse", "main", cwd=repo), before)
            config = (repo / ".git" / "config").read_text()
            self.assertNotIn("t@example.com", config)
            self.assertNotIn("bare = true", config)
            self.assertTrue((fixture / ".git").is_dir(), "fixture repo was not created")


if __name__ == "__main__":
    unittest.main()
