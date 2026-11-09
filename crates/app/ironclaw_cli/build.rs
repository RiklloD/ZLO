//! Stamp the commit this binary is built from as `IRONCLAW_BUILD_COMMIT`,
//! read by `zlo update` and the environment descriptor's
//! `build_commit`. A build outside a git checkout (release tarball, Docker
//! context without `.git`) sets nothing; an explicit `IRONCLAW_BUILD_COMMIT`
//! in the build environment wins.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=IRONCLAW_BUILD_COMMIT");
    println!("cargo:rerun-if-changed=build.rs");
    if let Ok(commit) = std::env::var("IRONCLAW_BUILD_COMMIT") {
        if is_full_hash(&commit) {
            println!("cargo:rustc-env=IRONCLAW_BUILD_COMMIT={commit}");
        }
        return;
    }
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
        return;
    };
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
