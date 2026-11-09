//! Stamp the commit this binary is built from as `IRONCLAW_BUILD_COMMIT`,
//! read by `zlo update` and the environment descriptor's
//! `build_commit`. A build outside a git checkout (release tarball, Docker
//! context without `.git`) sets nothing; an explicit `IRONCLAW_BUILD_COMMIT`
//! in the build environment wins.
//!
//! Also stamps the checkout itself as `IRONCLAW_BUILD_SOURCE_DIR`, so
//! `zlo update enable` can turn self-update on without being told where the
//! source is. For a build from a linked worktree it is the main checkout
//! (where the followed branch lives), not the worktree.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=IRONCLAW_BUILD_COMMIT");
    println!("cargo:rerun-if-changed=build.rs");
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
        return;
    };
    if let Some(checkout) = main_checkout(&manifest_dir) {
        println!(
            "cargo:rustc-env=IRONCLAW_BUILD_SOURCE_DIR={}",
            checkout.display()
        );
    }
    if let Ok(commit) = std::env::var("IRONCLAW_BUILD_COMMIT") {
        if is_full_hash(&commit) {
            println!("cargo:rustc-env=IRONCLAW_BUILD_COMMIT={commit}");
        }
        return;
    }
    let Some(git_dir) = git(&manifest_dir, &["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    // HEAD moves on checkout; logs/HEAD on every commit, merge, pull and
    // reset. Watch only files that exist (a missing one reruns every build).
    for watched in ["HEAD", "logs/HEAD"] {
        let path = Path::new(&git_dir).join(watched);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Some(commit) = git(&manifest_dir, &["rev-parse", "HEAD"]).filter(|c| is_full_hash(c)) {
        println!("cargo:rustc-env=IRONCLAW_BUILD_COMMIT={commit}");
    }
}

/// The main checkout of the repository `dir` is in: the folder holding the
/// shared `.git` directory. `None` outside a checkout, for a bare
/// repository, or when the path would not survive a `rustc-env` line.
fn main_checkout(dir: &Path) -> Option<PathBuf> {
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common);
    if common.file_name()? != ".git" {
        return None;
    }
    let checkout = common.parent()?.to_path_buf();
    let text = checkout.to_str()?;
    (checkout.is_absolute() && !text.chars().any(char::is_control)).then_some(checkout)
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn is_full_hash(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
