//! The `@` menu's mention sources (files, folders, threads, the git diff,
//! a Changes-tab selection) and the composer's mention chips.
//!
//! Mentions travel as Markdown links (`ironclaw_product_contracts::mention`):
//! the composer shows each as an atomic chip, sends it as `[@label](uri)`,
//! and turns a sent message's links back into chips for edit and resend.
//! The gateway expands them at send time inside the project folder the
//! message names as its mention root.
//!
//! Files come from one listing per project folder: `git ls-files` (so
//! `.gitignore` holds) or, outside a repository, a bounded walk that skips
//! hidden and build folders. Listings are cached per folder and rebuilt in
//! the background once stale ([`IndexCache`]).

use std::collections::HashMap;
use std::io::Read as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use ironclaw_product_contracts::mention::{
    LineRange, MentionPart, MentionSpan, MentionUri, mention_parts, mention_spans,
    strip_mention_context,
};

/// Paths one listing holds; a bigger tree is cut (and says so).
const INDEX_LIMIT: usize = 100_000;
/// Bytes of `git ls-files` output read.
const GIT_OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;
/// A listing older than this is rebuilt on the next `@`.
const INDEX_TTL: Duration = Duration::from_secs(20);
/// Folders a walk never enters.
const HEAVY_DIRS: [&str; 7] = [
    "node_modules",
    "target",
    ".git",
    "dist",
    "build",
    ".venv",
    "__pycache__",
];
/// Rows per section.
const FILE_ROWS: usize = 8;
const FOLDER_ROWS: usize = 4;
const THREAD_ROWS: usize = 5;

/// The files and folders under a project folder, relative to it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileIndex {
    pub root: PathBuf,
    pub files: Vec<String>,
    pub dirs: Vec<String>,
    /// The folder is a git work tree (the git diff mention applies).
    pub git: bool,
    /// The listing stopped at [`INDEX_LIMIT`].
    pub truncated: bool,
}

impl FileIndex {
    /// List `root`. Blocking: run it off the UI thread.
    pub fn build(root: &Path) -> Self {
        let (files, git, truncated) = match git_files(root) {
            Some((files, truncated)) => (files, true, truncated),
            None => {
                let (files, truncated) = walk(root);
                (files, false, truncated)
            }
        };
        Self::from_files(root.to_path_buf(), files, git, truncated)
    }

    fn from_files(root: PathBuf, mut files: Vec<String>, git: bool, truncated: bool) -> Self {
        files.sort();
        files.dedup();
        let mut dirs: Vec<String> = files
            .iter()
            .flat_map(|f| f.match_indices('/').map(move |(i, _)| f[..i].to_string()))
            .collect();
        dirs.sort();
        dirs.dedup();
        Self {
            root,
            files,
            dirs,
            git,
            truncated,
        }
    }
}

/// Listings by folder, with their age.
#[derive(Default)]
pub struct IndexCache {
    entries: HashMap<PathBuf, (Instant, Arc<FileIndex>)>,
    building: std::collections::HashSet<PathBuf>,
}

impl IndexCache {
    /// The cached listing (possibly stale) and whether a rebuild should start.
    pub fn get(&mut self, root: &Path) -> (Option<Arc<FileIndex>>, bool) {
        let entry = self.entries.get(root);
        let stale = entry.is_none_or(|(at, _)| at.elapsed() > INDEX_TTL);
        let start = stale && self.building.insert(root.to_path_buf());
        (entry.map(|(_, index)| Arc::clone(index)), start)
    }

    pub fn put(&mut self, index: FileIndex) {
        let root = index.root.clone();
        self.building.remove(&root);
        self.entries.insert(root, (Instant::now(), Arc::new(index)));
    }

    /// A rebuild failed or was dropped: allow the next one.
    pub fn abandon(&mut self, root: &Path) {
        self.building.remove(root);
    }
}

/// `git ls-files` of `root`: tracked plus untracked-but-not-ignored.
fn git_files(root: &Path) -> Option<(Vec<String>, bool)> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = Vec::new();
    let read = child
        .stdout
        .take()
        .map(|s| s.take(GIT_OUTPUT_LIMIT).read_to_end(&mut out));
    let cut = out.len() as u64 >= GIT_OUTPUT_LIMIT;
    if cut {
        let _ = child.kill();
    }
    let status = child.wait().ok()?;
    if read.is_none_or(|r| r.is_err()) || (!cut && !status.success()) {
        return None;
    }
    let mut files: Vec<String> = out
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let truncated = cut || files.len() > INDEX_LIMIT;
    files.truncate(INDEX_LIMIT);
    Some((files, truncated))
}

/// A bounded walk outside git: hidden and build folders are skipped.
fn walk(root: &Path) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if files.len() >= INDEX_LIMIT {
                return (files, true);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || HEAVY_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root) {
                files.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    (files, false)
}

/// A thread the `@` menu can offer, from the inbox list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadEntry {
    pub thread_id: String,
    pub title: String,
    /// Unix ms of the latest activity.
    pub last_activity_at: i64,
}

/// Which `@` section an entry sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MentionKind {
    Changes,
    File,
    Folder,
    Thread,
}

impl MentionKind {
    pub fn section(self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::File => "Files",
            Self::Folder => "Folders",
            Self::Thread => "Threads",
        }
    }
}

/// One `@` row: the mention it inserts, its chip label, and the muted
/// column (parent folder, thread age, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionEntry {
    pub uri: MentionUri,
    pub label: String,
    pub detail: String,
    pub kind: MentionKind,
}

/// What the `@` menu can see for this composer.
#[derive(Default, Clone, Copy)]
pub struct MentionContext<'a> {
    pub index: Option<&'a FileIndex>,
    pub threads: &'a [ThreadEntry],
    /// Lines picked in the Changes tab: absolute path and range.
    pub selection: Option<(&'a Path, LineRange)>,
    /// The thread the composer belongs to, left out of Threads.
    pub current_thread: Option<&'a str>,
}

/// `@` rows for `query`, section by section.
pub fn mention_matches(query: &str, ctx: &MentionContext<'_>) -> Vec<MentionEntry> {
    let mut out = Vec::new();
    let query = query.trim();
    if let Some(index) = ctx.index {
        let root = index.root.to_string_lossy().into_owned();
        if let Some((path, range)) = ctx.selection {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if query.is_empty()
                || fuzzy_score(query, "selection").is_some()
                || fuzzy_score(query, &name).is_some()
            {
                out.push(MentionEntry {
                    uri: MentionUri::Selection {
                        path: path.to_string_lossy().into_owned(),
                        range,
                    },
                    label: format!("{name}:{}-{}", range.start, range.end),
                    detail: "Selected lines in Changes".into(),
                    kind: MentionKind::Changes,
                });
            }
        }
        if index.git
            && (query.is_empty()
                || fuzzy_score(query, "diff").is_some()
                || fuzzy_score(query, "changes").is_some())
        {
            out.push(MentionEntry {
                uri: MentionUri::GitDiff { cwd: root.clone() },
                label: "diff".into(),
                detail: "Uncommitted changes".into(),
                kind: MentionKind::Changes,
            });
        }
        let files = rank(query, &index.files, FILE_ROWS);
        let labels = disambiguate(&files);
        for (rel, label) in files.iter().zip(labels) {
            out.push(MentionEntry {
                uri: MentionUri::File {
                    path: join(&root, rel),
                    line_range: None,
                },
                label,
                detail: parent(rel).to_string(),
                kind: MentionKind::File,
            });
        }
        let dirs = rank(query, &index.dirs, FOLDER_ROWS);
        let labels = disambiguate(&dirs);
        for (rel, label) in dirs.iter().zip(labels) {
            out.push(MentionEntry {
                uri: MentionUri::Directory {
                    path: join(&root, rel),
                },
                label: format!("{label}/"),
                detail: parent(rel).to_string(),
                kind: MentionKind::Folder,
            });
        }
    }
    let mut threads: Vec<(i32, &ThreadEntry)> = ctx
        .threads
        .iter()
        .filter(|t| Some(t.thread_id.as_str()) != ctx.current_thread)
        .filter_map(|t| {
            if query.is_empty() {
                Some((0, t))
            } else {
                fuzzy_score(query, &t.title).map(|s| (s, t))
            }
        })
        .collect();
    threads.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.last_activity_at.cmp(&a.1.last_activity_at))
    });
    out.extend(
        threads
            .into_iter()
            .take(THREAD_ROWS)
            .map(|(_, t)| MentionEntry {
                uri: MentionUri::Thread {
                    id: t.thread_id.clone(),
                },
                label: t.title.clone(),
                detail: "Thread".into(),
                kind: MentionKind::Thread,
            }),
    );
    out
}

fn join(root: &str, rel: &str) -> String {
    format!("{}/{rel}", root.trim_end_matches('/'))
}

fn parent(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(p, _)| p)
}

/// The best `limit` of `paths` for `query`. An empty query keeps the
/// shallowest, shortest paths.
pub fn rank<'a>(query: &str, paths: &'a [String], limit: usize) -> Vec<&'a str> {
    let mut scored: Vec<(i32, &str)> = if query.is_empty() {
        paths
            .iter()
            .map(|p| {
                let depth = p.matches('/').count() as i32;
                (-(depth * 100) - p.len() as i32, p.as_str())
            })
            .collect()
    } else {
        paths
            .iter()
            .filter_map(|p| fuzzy_score(query, p).map(|s| (s, p.as_str())))
            .collect()
    };
    let cmp = |a: &(i32, &str), b: &(i32, &str)| b.0.cmp(&a.0).then(a.1.cmp(b.1));
    if scored.len() > limit {
        scored.select_nth_unstable_by(limit, cmp);
        scored.truncate(limit);
    }
    scored.sort_by(cmp);
    scored.into_iter().map(|(_, p)| p).collect()
}

/// Fuzzy score of `query` against a path (higher is better), `None` when its
/// characters do not all appear in order. A match inside the file name beats
/// one spread over folders; word starts and runs score extra.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if query.is_empty() {
        return Some(0);
    }
    let name_start = candidate.rfind('/').map_or(0, |i| i + 1);
    let name = &candidate[name_start..];
    let lower_name: String = name.to_lowercase();
    let query_text: String = query.iter().collect();
    let mut bonus = 0;
    if lower_name == query_text {
        bonus += 1_000;
    } else if lower_name.starts_with(&query_text) {
        bonus += 400;
    } else if lower_name.contains(&query_text) {
        bonus += 200;
    }
    let in_name = subsequence(&query, name).map(|s| s + 300);
    let anywhere = subsequence(&query, candidate);
    let best = in_name.max(anywhere)?;
    Some(best + bonus - (candidate.len() as i32) / 4)
}

/// Greedy in-order match of `query` (lowercase) in `text`.
fn subsequence(query: &[char], text: &str) -> Option<i32> {
    let chars: Vec<char> = text.chars().collect();
    let mut score = 0;
    let mut qi = 0;
    let mut last: Option<usize> = None;
    for (i, c) in chars.iter().enumerate() {
        if qi == query.len() {
            break;
        }
        if c.to_lowercase().eq(std::iter::once(query[qi])) {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let boundary = prev.is_none_or(|p| {
                matches!(p, '/' | '_' | '-' | '.' | ' ') || (p.is_lowercase() && c.is_uppercase())
            });
            score += 10;
            if boundary {
                score += 15;
            }
            match last {
                Some(l) if l + 1 == i => score += 20,
                Some(l) => score -= ((i - l - 1) as i32).min(10),
                None => {}
            }
            last = Some(i);
            qi += 1;
        }
    }
    (qi == query.len()).then_some(score)
}

/// Short labels for `paths`: the last component, plus as many parent
/// components as it takes to tell same-named paths apart.
pub fn disambiguate(paths: &[&str]) -> Vec<String> {
    let parts: Vec<Vec<&str>> = paths.iter().map(|p| p.split('/').collect()).collect();
    let mut depth = vec![1usize; paths.len()];
    loop {
        let labels: Vec<String> = parts
            .iter()
            .zip(&depth)
            .map(|(p, d)| p[p.len().saturating_sub(*d)..].join("/"))
            .collect();
        let mut grow = false;
        for i in 0..paths.len() {
            let clash = (0..paths.len()).any(|j| j != i && labels[j] == labels[i]);
            if clash && depth[i] < parts[i].len() {
                depth[i] += 1;
                grow = true;
            }
        }
        if !grow {
            return labels;
        }
    }
}

/// Prefix of a composer token that stands for a mention.
const TOKEN_PREFIX: &str = "mention:";

/// The composer token id for `uri`.
pub fn token_id(uri: &MentionUri) -> String {
    format!("{TOKEN_PREFIX}{}", uri.to_uri())
}

/// The mention a composer token stands for, if it is one.
pub fn token_mention(id: &str) -> Option<MentionUri> {
    MentionUri::parse(id.strip_prefix(TOKEN_PREFIX)?).ok()
}

/// The chip text for a label: `@label`, one line.
pub fn chip_text(label: &str) -> String {
    let label: String = label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("@{}", label.trim())
}

/// What a chip sends: the Markdown link, or the `/skill` and `@tool` words
/// the backend already understands.
pub fn outgoing(uri: &MentionUri, chip: &str) -> String {
    match uri {
        MentionUri::Skill { name } => format!("/{name}"),
        MentionUri::Extension { name } => format!("@{name}"),
        _ => uri.to_link(chip.strip_prefix('@').unwrap_or(chip)),
    }
}

/// A chip to restore into the composer: its byte range in the plain text,
/// token id and chip text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredChip {
    pub range: Range<usize>,
    pub id: String,
    pub text: String,
}

/// A sent message's text with each mention link folded back to its chip,
/// for edit and resend.
pub fn restore(text: &str) -> (String, Vec<RestoredChip>) {
    let text = strip_mention_context(text);
    let mut out = String::with_capacity(text.len());
    let mut chips = Vec::new();
    for part in mention_parts(text) {
        match part {
            MentionPart::Text(t) => out.push_str(t),
            MentionPart::Mention(span) => {
                let chip = chip_text(&span.label);
                let start = out.len();
                out.push_str(&chip);
                chips.push(RestoredChip {
                    range: start..out.len(),
                    id: token_id(&span.uri),
                    text: chip,
                });
            }
        }
    }
    (out, chips)
}

/// Whether `text` holds a mention that reads the disk (and so needs the
/// project folder as its mention root).
pub fn needs_root(text: &str) -> bool {
    mention_spans(text).iter().any(|s| s.uri.path().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(files: &[&str]) -> FileIndex {
        FileIndex::from_files(
            PathBuf::from("/repo"),
            files.iter().map(|s| s.to_string()).collect(),
            true,
            false,
        )
    }

    #[test]
    fn fuzzy_ranking_prefers_file_names_word_starts_and_runs() {
        let paths: Vec<String> = [
            "src/chat/menu.rs",
            "src/main.rs",
            "docs/menus/overview.md",
            "src/workspace/draft.rs",
            "app/src/chat/model_picker.rs",
            "Cargo.toml",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(rank("menu", &paths, 3)[0], "src/chat/menu.rs");
        assert_eq!(rank("main", &paths, 1), ["src/main.rs"]);
        assert_eq!(rank("mp", &paths, 1), ["app/src/chat/model_picker.rs"]);
        assert_eq!(rank("cargo", &paths, 1), ["Cargo.toml"]);
        assert_eq!(rank("wsdraft", &paths, 1), ["src/workspace/draft.rs"]);
        assert!(rank("zzz", &paths, 5).is_empty());
        // No query: shallow, short paths first.
        assert_eq!(rank("", &paths, 1), ["Cargo.toml"]);
        assert!(fuzzy_score("menu", "menu.rs") > fuzzy_score("menu", "src/m/e/n/u.rs"));
    }

    #[test]
    fn same_named_paths_are_told_apart_by_their_parents() {
        assert_eq!(
            disambiguate(&["app/chat/mod.rs", "app/side/mod.rs", "app/main.rs"]),
            ["chat/mod.rs", "side/mod.rs", "main.rs"]
        );
        assert_eq!(
            disambiguate(&["a/x/lib.rs", "b/x/lib.rs"]),
            ["a/x/lib.rs", "b/x/lib.rs"]
        );
        assert_eq!(
            disambiguate(&["lib.rs", "src/lib.rs"]),
            ["lib.rs", "src/lib.rs"]
        );
    }

    #[test]
    fn at_menu_offers_sections_in_order() {
        let index = index(&["src/chat/mod.rs", "src/side/mod.rs", "README.md"]);
        let threads = vec![
            ThreadEntry {
                thread_id: "t1".into(),
                title: "Fix the mod loader".into(),
                last_activity_at: 1,
            },
            ThreadEntry {
                thread_id: "t2".into(),
                title: "Current".into(),
                last_activity_at: 2,
            },
        ];
        let ctx = MentionContext {
            index: Some(&index),
            threads: &threads,
            selection: None,
            current_thread: Some("t2"),
        };
        let rows = mention_matches("mod", &ctx);
        let kinds: Vec<MentionKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [MentionKind::File, MentionKind::File, MentionKind::Thread]
        );
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["chat/mod.rs", "side/mod.rs", "Fix the mod loader"]);
        assert_eq!(
            rows[0].uri,
            MentionUri::File {
                path: "/repo/src/chat/mod.rs".into(),
                line_range: None
            }
        );
        let all = mention_matches("", &ctx);
        assert_eq!(all[0].kind, MentionKind::Changes);
        assert!(all.iter().any(|r| r.label == "src/"));
        assert!(!all.iter().any(|r| r.label == "Current"));
    }

    #[test]
    fn chips_round_trip_through_the_sent_text() {
        let uri = MentionUri::File {
            path: "/repo/src/a b.rs".into(),
            line_range: LineRange::new(3, 4),
        };
        let id = token_id(&uri);
        assert_eq!(token_mention(&id), Some(uri.clone()));
        let sent = format!("look at {} now", outgoing(&uri, &chip_text("a b.rs")));
        assert!(needs_root(&sent));
        let stored = format!(
            "{sent}\n\n---\nMentioned context (expanded by ZLO from the @-mentions above):\nX\n[End of mentioned context]"
        );
        let (text, chips) = restore(&stored);
        assert_eq!(text, "look at @a b.rs now");
        assert_eq!(chips.len(), 1);
        assert_eq!(&text[chips[0].range.clone()], "@a b.rs");
        assert_eq!(token_mention(&chips[0].id), Some(uri));
        let skill = MentionUri::Skill { name: "rev".into() };
        assert_eq!(outgoing(&skill, "@rev"), "/rev");
        assert!(!needs_root("see [@t](zlo://thread/t1)"));
    }

    #[test]
    fn listings_follow_gitignore_and_the_cache_rebuilds_when_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("node_modules/x/i.js"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        let walked = FileIndex::build(&dir);
        assert!(!walked.git);
        assert_eq!(walked.files, ["src/a.rs"]);
        assert_eq!(walked.dirs, ["src"]);

        let git = |args: &[&str]| Command::new("git").arg("-C").arg(&dir).args(args).output();
        if git(&["init", "-q"]).is_ok_and(|o| o.status.success()) {
            std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
            std::fs::write(dir.join("ignored.txt"), "").unwrap();
            let listed = FileIndex::build(&dir);
            assert!(listed.git);
            assert!(listed.files.contains(&"src/a.rs".to_string()));
            assert!(!listed.files.contains(&"ignored.txt".to_string()));
        }

        let mut cache = IndexCache::default();
        assert_eq!(cache.get(&dir), (None, true));
        // One rebuild at a time.
        assert_eq!(cache.get(&dir), (None, false));
        cache.put(walked.clone());
        let (cached, rebuild) = cache.get(&dir);
        assert_eq!(cached.as_deref(), Some(&walked));
        assert!(!rebuild);
    }
}
