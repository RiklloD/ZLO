//! `@`-mentions expanded at send time (ZLO's rich mentions).
//!
//! A message names files, folders, threads, the git diff or a Changes-tab
//! selection as [`MentionUri`] links. Before the turn runs, each one becomes
//! context appended after [`MENTION_CONTEXT_OPEN`]: the stored message keeps
//! the typed text first, and clients show only that part.
//!
//! Containment: a host path is read only when its canonical form (symlinks
//! resolved) lies inside one of the message's `mention_roots`, themselves
//! canonical folders other than `/`, and never when it looks sensitive
//! (`ironclaw_safety::sensitive_paths`). Without roots nothing on disk is
//! read. This is the same host-local trust the desktop's path attachments
//! (`cli_attachments::take_local`) already rely on. Thread mentions resolve
//! only against the caller's own threads ([`ThreadDigests`]).
//!
//! Bounds: every block is size-capped and the whole context fits a budget the
//! caller passes (the native loop's message cap, or the CLI prompt cap).
//! Content is leak-scanned and the context is wrapped as untrusted data.
//!
//! CLI engines (Claude, Codex) read files themselves, so a whole-file or
//! folder mention reaches them as its path; ranges, threads and diffs are
//! inlined because the CLI cannot see them as they were at send time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_product_contracts::mention::{
    LineRange, MENTION_CONTEXT_CLOSE, MENTION_CONTEXT_OPEN, MentionPart, MentionSpan, MentionUri,
    mention_parts, strip_mention_context,
};
use ironclaw_safety::LeakDetector;
use ironclaw_safety::sensitive_paths::is_sensitive_path_str;
use tokio::io::AsyncReadExt as _;

/// Largest slice of one file that is inlined.
const FILE_BYTES: usize = 24 * 1024;
/// Entries a folder listing shows.
const TREE_ENTRIES: usize = 300;
/// Folder levels a listing descends.
const TREE_DEPTH: usize = 4;
/// Largest git diff that is inlined.
const DIFF_BYTES: usize = 32 * 1024;
/// Exchanges (user + assistant messages) a thread mention carries.
const THREAD_MESSAGES: usize = 8;
/// Characters kept of each message of a mentioned thread.
const THREAD_MESSAGE_CHARS: usize = 1_500;
/// Mentions one message may expand; the rest stay links.
const MAX_MENTIONS: usize = 24;
/// A git call that takes longer than this is abandoned.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Folders a listing never descends into when git cannot say what's ignored.
const HEAVY_DIRS: [&str; 6] = ["node_modules", "target", ".git", "dist", "build", ".venv"];

/// Which runtime takes the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MentionRuntime {
    /// IronClaw's own loop: everything is inlined.
    Loop,
    /// A CLI engine with its own file access: whole files go by path.
    Cli,
}

/// What a thread mention carries, already scoped to the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ThreadDigest {
    pub(super) title: Option<String>,
    /// The latest compaction summary, when the thread has one.
    pub(super) summary: Option<String>,
    /// Oldest first, `(who, text)`.
    pub(super) messages: Vec<(&'static str, String)>,
}

/// Reads mentioned threads for the sender. `None` when the thread is not one
/// of the caller's (or is gone): the mention then stays a link.
#[async_trait]
pub(super) trait ThreadDigests: Send + Sync {
    async fn digest(&self, thread_id: &str) -> Option<ThreadDigest>;
}

/// `content` with its mentions expanded. Skill and extension links become
/// the `/name` and `@name` words the loop already understands; the rest
/// stay links, and their context is appended. `budget` bounds the bytes
/// added.
pub(super) async fn expand(
    content: &str,
    roots: &[String],
    runtime: MentionRuntime,
    threads: &dyn ThreadDigests,
    budget: usize,
) -> String {
    let typed = strip_mention_context(content);
    let parts = mention_parts(typed);
    let mut text = String::with_capacity(typed.len());
    let mut mentions: Vec<MentionSpan> = Vec::new();
    for part in parts {
        match part {
            MentionPart::Text(t) => text.push_str(t),
            MentionPart::Mention(span) => match &span.uri {
                MentionUri::Skill { name } => text.push_str(&format!("/{name}")),
                MentionUri::Extension { name } => text.push_str(&format!("@{name}")),
                _ => {
                    text.push_str(&typed[span.range.clone()]);
                    if !mentions.iter().any(|m| m.uri == span.uri) {
                        mentions.push(span);
                    }
                }
            },
        }
    }
    if mentions.is_empty() {
        return text;
    }
    let roots = Roots::resolve(roots).await;
    let detector = LeakDetector::new();
    let mut blocks: Vec<String> = Vec::new();
    let mut used = 0usize;
    for (ix, span) in mentions.iter().enumerate() {
        let block = if ix >= MAX_MENTIONS {
            format!(
                "@{}: not expanded (too many mentions in one message).",
                span.label
            )
        } else {
            expand_one(span, &roots, runtime, threads).await
        };
        let (block, _) = detector.redact_all_secrets(&block);
        let remaining = budget.saturating_sub(used);
        let block = if block.len() + 2 > remaining {
            let note = format!(
                "@{}: not expanded (message context budget reached).",
                span.label
            );
            if note.len() + 2 > remaining {
                break;
            }
            note
        } else {
            block
        };
        used += block.len() + 2;
        blocks.push(block);
    }
    let body = ironclaw_safety::wrap_external_content(
        "files, folders, threads and diffs the user @-mentioned",
        &blocks.join("\n\n"),
    );
    format!("{text}{MENTION_CONTEXT_OPEN}{body}{MENTION_CONTEXT_CLOSE}")
}

async fn expand_one(
    span: &MentionSpan,
    roots: &Roots,
    runtime: MentionRuntime,
    threads: &dyn ThreadDigests,
) -> String {
    let label = &span.label;
    match &span.uri {
        MentionUri::Thread { id } => match threads.digest(id).await {
            Some(digest) => thread_block(label, &digest),
            None => format!("@{label}: thread not available."),
        },
        uri => {
            let Some(raw) = uri.path() else {
                return format!("@{label}: nothing to expand.");
            };
            let path = match roots.contain(raw).await {
                Ok(path) => path,
                Err(why) => return format!("@{label} ({raw}): not expanded ({why})."),
            };
            match uri {
                MentionUri::File { line_range, .. } => {
                    file_block(label, &path, *line_range, runtime).await
                }
                MentionUri::Selection { range, .. } => {
                    file_block(label, &path, Some(*range), MentionRuntime::Loop).await
                }
                MentionUri::Directory { .. } => dir_block(label, &path, runtime).await,
                MentionUri::GitDiff { .. } => diff_block(label, &path).await,
                MentionUri::Thread { .. }
                | MentionUri::Skill { .. }
                | MentionUri::Extension { .. } => {
                    format!("@{label}: nothing to expand.")
                }
            }
        }
    }
}

/// The message's granted folders, canonical.
struct Roots(Vec<PathBuf>);

impl Roots {
    async fn resolve(raw: &[String]) -> Self {
        let mut out = Vec::new();
        for root in raw.iter().take(8) {
            let path = Path::new(root);
            if !path.is_absolute() {
                continue;
            }
            let Ok(canonical) = tokio::fs::canonicalize(path).await else {
                tracing::debug!("mention root is unreadable");
                continue; // silent-ok: an unreadable root grants nothing
            };
            let is_dir = tokio::fs::metadata(&canonical)
                .await
                .is_ok_and(|m| m.is_dir());
            // `/` would grant the whole disk.
            if is_dir && canonical.parent().is_some() {
                out.push(canonical);
            }
        }
        Self(out)
    }

    /// The canonical path of `raw` when it lies inside a root.
    async fn contain(&self, raw: &str) -> Result<PathBuf, &'static str> {
        if self.0.is_empty() {
            return Err("no project folder was shared with this message");
        }
        let canonical = tokio::fs::canonicalize(raw)
            .await
            .map_err(|_| "not found")?;
        if !self.0.iter().any(|root| canonical.starts_with(root)) {
            return Err("outside the project folder");
        }
        if is_sensitive_path_str(&canonical.to_string_lossy()) {
            return Err("sensitive file");
        }
        Ok(canonical)
    }
}

async fn file_block(
    label: &str,
    path: &Path,
    range: Option<LineRange>,
    runtime: MentionRuntime,
) -> String {
    let shown = path.display();
    let meta = match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return format!("@{label} ({shown}): not a file."),
        Err(_) => return format!("@{label} ({shown}): not found."),
    };
    if runtime == MentionRuntime::Cli && range.is_none() {
        return format!(
            "@{label}: file {shown} ({} bytes). Read it from disk.",
            meta.len()
        );
    }
    // Ranges read further in, so their lines can sit past the first slice.
    let cap = if range.is_some() {
        FILE_BYTES * 16
    } else {
        FILE_BYTES
    };
    let bytes = match read_capped(path, cap).await {
        Ok(bytes) => bytes,
        Err(_) => return format!("@{label} ({shown}): unreadable."),
    };
    let read_all = (bytes.len() as u64) >= meta.len();
    if bytes.iter().take(8 * 1024).any(|b| *b == 0) {
        return format!("@{label}: binary file {shown}, not inlined.");
    }
    let text = String::from_utf8_lossy(&bytes);
    let (body, heading) = match range {
        Some(range) => (
            lines_of(&text, range),
            format!("@{label}: {shown}, lines {}-{}", range.start, range.end),
        ),
        None => (text.into_owned(), format!("@{label}: {shown}")),
    };
    let (body, cut) = truncate(&body, FILE_BYTES);
    let note = if cut || (range.is_none() && !read_all) {
        format!(
            "\n[Truncated: showing the first {FILE_BYTES} bytes of {} total.]",
            meta.len()
        )
    } else {
        String::new()
    };
    format!("{heading}\n{}{note}", fenced(&body, extension(path)))
}

async fn dir_block(label: &str, path: &Path, runtime: MentionRuntime) -> String {
    let shown = path.display();
    if runtime == MentionRuntime::Cli {
        return format!("@{label}: folder {shown}. List or read it from disk as needed.");
    }
    let (entries, complete) = match git_listing(path).await {
        Some(files) => tree_from_files(&files),
        None => walk(path).await,
    };
    let more = if complete {
        String::new()
    } else {
        format!("\n[Listing cut at {TREE_ENTRIES} entries / {TREE_DEPTH} levels.]")
    };
    format!(
        "@{label}: folder {shown}\n{}{more}",
        fenced(&entries.join("\n"), "")
    )
}

async fn diff_block(label: &str, cwd: &Path) -> String {
    let shown = cwd.display();
    let Some(diff) = git_capped(
        cwd,
        &["diff", "HEAD", "--no-color", "--no-ext-diff"],
        DIFF_BYTES + 1,
    )
    .await
    else {
        return format!("@{label}: {shown} is not a git repository.");
    };
    let untracked = git_capped(
        cwd,
        &["ls-files", "--others", "--exclude-standard"],
        8 * 1024,
    )
    .await
    .unwrap_or_default(); // silent-ok: the untracked list only enriches the diff
    let (diff, cut) = truncate(&diff, DIFF_BYTES);
    let mut out = format!("@{label}: uncommitted changes in {shown}\n");
    if diff.trim().is_empty() {
        out.push_str("(no changes to tracked files)\n");
    } else {
        out.push_str(&fenced(&diff, "diff"));
    }
    if cut {
        out.push_str(&format!("\n[Diff truncated at {DIFF_BYTES} bytes.]"));
    }
    let untracked: Vec<&str> = untracked.lines().take(50).collect();
    if !untracked.is_empty() {
        out.push_str(&format!("\nUntracked files: {}", untracked.join(", ")));
    }
    out
}

fn thread_block(label: &str, digest: &ThreadDigest) -> String {
    let title = digest.title.as_deref().unwrap_or(label);
    let mut out = format!("@{label}: thread \"{title}\"");
    if let Some(summary) = &digest.summary {
        let (summary, _) = truncate(summary, THREAD_MESSAGE_CHARS * 2);
        out.push_str(&format!("\nSummary:\n{summary}"));
    }
    let skip = digest.messages.len().saturating_sub(THREAD_MESSAGES);
    if skip > 0 {
        out.push_str(&format!("\n[{skip} earlier messages omitted.]"));
    }
    for (who, text) in digest.messages.iter().skip(skip) {
        let text = strip_mention_context(text);
        let shortened: String = text.chars().take(THREAD_MESSAGE_CHARS).collect();
        let more = if shortened.len() < text.len() {
            " […]"
        } else {
            ""
        };
        out.push_str(&format!("\n{who}: {}{more}", shortened.trim()));
    }
    out
}

/// Up to `cap` bytes of a file.
async fn read_capped(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    let file = tokio::fs::File::open(path).await?;
    let mut out = Vec::with_capacity(cap.min(64 * 1024));
    file.take(cap as u64).read_to_end(&mut out).await?;
    Ok(out)
}

fn lines_of(text: &str, range: LineRange) -> String {
    let start = range.start as usize;
    let count = (range.end - range.start) as usize + 1;
    text.lines()
        .skip(start - 1)
        .take(count)
        .collect::<Vec<_>>()
        .join("\n")
}

/// At most `cap` bytes, cut at a character boundary.
fn truncate(text: &str, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text.to_string(), false);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// A code fence longer than any backtick run inside `body`.
fn fenced(body: &str, lang: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{lang}\n{}\n{fence}", body.trim_end_matches('\n'))
}

fn extension(path: &Path) -> &str {
    path.extension().and_then(|e| e.to_str()).unwrap_or("")
}

/// stdout of a git call, at most `cap` bytes; `None` when git fails.
async fn git_capped(dir: &Path, args: &[&str], cap: usize) -> Option<String> {
    let mut child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| tracing::debug!(%error, "mention: git did not start"))
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let read = async {
        let mut out = Vec::new();
        (&mut stdout)
            .take(cap as u64)
            .read_to_end(&mut out)
            .await
            .map(|_| out)
    };
    let out = tokio::time::timeout(GIT_TIMEOUT, read).await.ok()?.ok()?;
    let full = out.len() < cap;
    drop(stdout);
    if full {
        let status = tokio::time::timeout(GIT_TIMEOUT, child.wait())
            .await
            .ok()?
            .ok()?;
        if !status.success() {
            return None;
        }
    }
    // A capped read leaves git to be killed on drop.
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Tracked and unignored files under `dir`, relative to it, when `dir` is in
/// a git work tree.
async fn git_listing(dir: &Path) -> Option<Vec<String>> {
    let out = git_capped(
        dir,
        &["ls-files", "--cached", "--others", "--exclude-standard"],
        512 * 1024,
    )
    .await?;
    Some(out.lines().map(str::to_string).collect())
}

/// An indented tree from relative file paths, bounded by entries and depth.
fn tree_from_files(files: &[String]) -> (Vec<String>, bool) {
    let mut lines: Vec<String> = Vec::new();
    let mut seen_dirs: std::collections::HashSet<String> = Default::default();
    let mut complete = true;
    let mut sorted: Vec<&String> = files.iter().collect();
    sorted.sort();
    for file in sorted {
        let parts: Vec<&str> = file.split('/').collect();
        for depth in 0..parts.len() {
            if depth >= TREE_DEPTH {
                complete = false;
                break;
            }
            let is_file = depth + 1 == parts.len();
            let key = parts[..=depth].join("/");
            if !is_file && !seen_dirs.insert(key) {
                continue;
            }
            if lines.len() >= TREE_ENTRIES {
                return (lines, false);
            }
            let slash = if is_file { "" } else { "/" };
            lines.push(format!("{}{}{slash}", "  ".repeat(depth), parts[depth]));
        }
    }
    (lines, complete)
}

/// A bounded walk for folders git doesn't know; hidden and build folders
/// are skipped.
async fn walk(root: &Path) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut complete = true;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue; // silent-ok: an unreadable subfolder is left out of the listing
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if files.len() >= TREE_ENTRIES * 4 {
                complete = false;
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || HEAVY_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(name);
            let is_dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
            if is_dir {
                if depth + 1 < TREE_DEPTH {
                    stack.push((path, depth + 1));
                } else {
                    complete = false;
                }
                files.push(format!("{rel}/"));
            } else {
                files.push(rel);
            }
        }
    }
    let files: Vec<String> = files
        .into_iter()
        .map(|f| f.trim_end_matches('/').to_string())
        .collect();
    let (lines, tree_complete) = tree_from_files(&files);
    (lines, complete && tree_complete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Threads(HashMap<String, ThreadDigest>);

    #[async_trait]
    impl ThreadDigests for Threads {
        async fn digest(&self, thread_id: &str) -> Option<ThreadDigest> {
            self.0.get(thread_id).cloned()
        }
    }

    fn file(path: &Path, range: Option<LineRange>) -> String {
        MentionUri::File {
            path: path.to_string_lossy().into_owned(),
            line_range: range,
        }
        .to_link(&path.file_name().unwrap().to_string_lossy())
    }

    fn context(out: &str) -> &str {
        let open = out.find(MENTION_CONTEXT_OPEN).expect("context block");
        &out[open..]
    }

    async fn run(content: &str, roots: &[&Path], runtime: MentionRuntime) -> String {
        let roots: Vec<String> = roots.iter().map(|r| r.to_string_lossy().into()).collect();
        expand(content, &roots, runtime, &Threads::default(), 60_000).await
    }

    #[tokio::test]
    async fn file_contents_inline_for_the_loop_and_go_by_path_for_cli() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main.rs");
        std::fs::write(&main, "fn main() {}\nline two\nline three\n").unwrap();
        let typed = format!("explain {}", file(&main, None));

        let out = run(&typed, &[dir.path()], MentionRuntime::Loop).await;
        assert!(out.starts_with(&typed), "typed text stays first: {out}");
        assert_eq!(strip_mention_context(&out), typed);
        assert!(context(&out).contains("fn main() {}"));
        assert!(context(&out).contains("EXTERNAL, UNTRUSTED"));

        let cli = run(&typed, &[dir.path()], MentionRuntime::Cli).await;
        assert!(!context(&cli).contains("fn main() {}"), "{cli}");
        assert!(context(&cli).contains("Read it from disk"));

        let ranged = format!("see {}", file(&main, LineRange::new(2, 3)));
        let cli = run(&ranged, &[dir.path()], MentionRuntime::Cli).await;
        assert!(context(&cli).contains("line two\nline three"), "{cli}");
        assert!(!context(&cli).contains("fn main"), "{cli}");
    }

    #[tokio::test]
    async fn mentions_never_escape_the_granted_roots() {
        let outer = tempfile::tempdir().unwrap();
        let project = outer.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let secret = outer.path().join("secret.txt");
        std::fs::write(&secret, "TOP-SECRET").unwrap();
        let link = project.join("escape.txt");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let env = project.join(".env");
        std::fs::write(&env, "API=TOP-SECRET").unwrap();

        for target in [&secret, &link, &env] {
            let out = run(&file(target, None), &[&project], MentionRuntime::Loop).await;
            assert!(!out.contains("TOP-SECRET"), "{target:?} leaked: {out}");
            assert!(out.contains("not expanded"), "{out}");
        }
        // No roots: nothing on disk is read at all.
        let inside = project.join("ok.txt");
        std::fs::write(&inside, "fine").unwrap();
        let out = run(&file(&inside, None), &[], MentionRuntime::Loop).await;
        assert!(!out.contains("```"), "{out}");
        // `/` is never a root.
        let out = run(
            &file(&secret, None),
            &[Path::new("/")],
            MentionRuntime::Loop,
        )
        .await;
        assert!(!out.contains("TOP-SECRET"), "{out}");
        // A `..` link is not even a mention.
        let dotted = format!("[@x](file://{}/../secret.txt)", project.display());
        assert_eq!(
            run(&dotted, &[outer.path()], MentionRuntime::Loop).await,
            dotted
        );
    }

    #[tokio::test]
    async fn large_binary_and_budget_bounds_hold() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.txt");
        std::fs::write(&big, "é".repeat(FILE_BYTES)).unwrap();
        let bin = dir.path().join("blob.bin");
        std::fs::write(&bin, [0u8, 1, 2, 3, 0, 9]).unwrap();
        let content = format!("{} {}", file(&big, None), file(&bin, None));
        let out = run(&content, &[dir.path()], MentionRuntime::Loop).await;
        assert!(out.contains("[Truncated: showing the first"), "{out}");
        assert!(out.contains("binary file"), "{out}");
        assert!(
            out.len() < content.len() + FILE_BYTES + 2_000,
            "{}",
            out.len()
        );

        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let tight = expand(
            &content,
            &roots,
            MentionRuntime::Loop,
            &Threads::default(),
            200,
        )
        .await;
        assert!(tight.len() < content.len() + 1_200, "{}", tight.len());
        assert!(tight.contains("budget reached"), "{tight}");
    }

    #[tokio::test]
    async fn folders_list_as_a_bounded_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/deep/a.rs"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
        std::fs::write(dir.path().join("node_modules/x/i.js"), "").unwrap();
        let link = MentionUri::Directory {
            path: dir.path().to_string_lossy().into(),
        }
        .to_link("proj");
        let out = run(&link, &[dir.path()], MentionRuntime::Loop).await;
        assert!(
            out.contains("src/") && out.contains("lib.rs") && out.contains("a.rs"),
            "{out}"
        );
        assert!(!out.contains("i.js"), "{out}");

        let many: Vec<String> = (0..TREE_ENTRIES + 50).map(|i| format!("f{i}.rs")).collect();
        let (lines, complete) = tree_from_files(&many);
        assert_eq!(lines.len(), TREE_ENTRIES);
        assert!(!complete);
    }

    #[tokio::test]
    async fn threads_resolve_through_the_callers_digests_only() {
        let mut threads = Threads::default();
        threads.0.insert(
            "t-mine".into(),
            ThreadDigest {
                title: Some("Fix login".into()),
                summary: Some("We fixed the cookie bug.".into()),
                messages: (0..12).map(|i| ("User", format!("message {i}"))).collect(),
            },
        );
        let mine = MentionUri::Thread {
            id: "t-mine".into(),
        }
        .to_link("Fix login");
        let other = MentionUri::Thread {
            id: "t-other".into(),
        }
        .to_link("Other");
        let out = expand(
            &format!("{mine} vs {other}"),
            &[],
            MentionRuntime::Loop,
            &threads,
            60_000,
        )
        .await;
        assert!(out.contains("thread \"Fix login\""), "{out}");
        assert!(out.contains("We fixed the cookie bug."), "{out}");
        assert!(
            out.contains("message 11") && !out.contains("message 3\n"),
            "{out}"
        );
        assert!(out.contains("4 earlier messages omitted"), "{out}");
        assert!(out.contains("@Other: thread not available."), "{out}");
    }

    #[tokio::test]
    async fn skill_and_extension_links_become_loop_words() {
        let skill = MentionUri::Skill {
            name: "review".into(),
        }
        .to_link("review");
        let ext = MentionUri::Extension {
            name: "github".into(),
        }
        .to_link("github");
        let out = run(&format!("use {skill} and {ext}"), &[], MentionRuntime::Loop).await;
        assert_eq!(out, "use /review and @github");
        // A resend of an already-expanded message is expanded once.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        std::fs::write(&a, "alpha").unwrap();
        let first = run(&file(&a, None), &[dir.path()], MentionRuntime::Loop).await;
        let again = run(&first, &[dir.path()], MentionRuntime::Loop).await;
        assert_eq!(again.matches(MENTION_CONTEXT_OPEN).count(), 1, "{again}");
    }

    #[tokio::test]
    async fn git_diff_is_inlined_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            return; // no git on this machine
        }
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "n").unwrap();
        let link = MentionUri::GitDiff {
            cwd: dir.path().to_string_lossy().into(),
        }
        .to_link("diff");
        let out = run(&link, &[dir.path()], MentionRuntime::Cli).await;
        assert!(out.contains("+two"), "{out}");
        assert!(out.contains("Untracked files: new.txt"), "{out}");
    }
}
