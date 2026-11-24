//! How a CLI runtime lists the profile's skills, from the shared skill
//! usage ledger (`ironclaw_skills::usage`), and how it counts their uses.
//!
//! - The profile index ([`ProfileSkills::entries`]) is re-rendered per the
//!   listing policy: full skills keep their description, name-only ones are
//!   listed by name and SKILL.md path, off ones are left out.
//!   A skill the turn's text asks for (`$name`, `/name`) is listed in full
//!   whatever its listing or pin.
//! - The profile's skills are recorded as the runtime's catalog.
//! - A turn that successfully reads a shown skill's SKILL.md counts one use
//!   of it: a typed read of that file, or a shell command that only reads it
//!   (`cat`, `sed`, `head`, …), once it completes. A path merely named (in
//!   an `echo`, in output, in model text) is not a read.
//!
//! Advisory throughout: every store call is bounded
//! ([`SKILL_USAGE_ADVISORY_TIMEOUT`]); a failed or stalled one lists every
//! skill in full, as the index always did, and never fails or holds a turn.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::runtime::{ProfileSkills, TurnProfile};
use ironclaw_skills::usage::{
    SKILL_USAGE_ADVISORY_TIMEOUT, SharedSkillUsage, SkillLedger, SkillListing, SkillListingPolicy,
    SkillListingRequest, SkillListingState, SkillUsageDocument, SkillUsageError, SkillUsageName,
    SkillUsageObservation, SkillUsageOwner, SkillUsageSource, bounded, force_requested, listing,
    requested_skills,
};
use serde_json::Value;

/// The shared skill usage store and whose document a CLI runtime writes
/// (the deployment's owner: CLI runtimes run for the local person).
#[derive(Debug, Clone)]
pub struct CliSkillUsage {
    store: SharedSkillUsage,
    owner: Option<SkillUsageOwner>,
}

impl Default for CliSkillUsage {
    /// Manages nothing: every runtime lists its skills as before.
    fn default() -> Self {
        Self {
            store: SharedSkillUsage::default(),
            owner: None,
        }
    }
}

impl CliSkillUsage {
    pub fn new(store: SharedSkillUsage, owner: SkillUsageOwner) -> Self {
        Self {
            store,
            owner: Some(owner),
        }
    }

    /// The owner's document; `None` when listing is not managed or the
    /// store failed or stalled (then nothing changes).
    pub(crate) async fn document(&self) -> Option<SkillUsageDocument> {
        let owner = self.owner.as_ref()?;
        match bounded(SKILL_USAGE_ADVISORY_TIMEOUT, self.store.store().load(owner)).await {
            Ok(document) => document,
            Err(error) => {
                tracing::debug!(%error, "cli skills: usage not read; listing all skills");
                None
            }
        }
    }

    pub(crate) async fn record(
        &self,
        source: &SkillUsageSource,
        observation: SkillUsageObservation,
    ) {
        let Some(owner) = self.owner.as_ref() else {
            return; // silent-ok: an unmanaged deployment keeps no ledger
        };
        if observation.is_empty() {
            return;
        }
        if let Err(error) = bounded(
            SKILL_USAGE_ADVISORY_TIMEOUT,
            self.store
                .store()
                .record(owner, source, observation, now_ms()),
        )
        .await
        {
            tracing::debug!(%error, "cli skills: usage not recorded");
        }
    }

    /// Merge an imported ledger; `Ok` only once the store persisted it (an
    /// unmanaged deployment persists nothing, so it is an error here: the
    /// caller keeps its source for a later import).
    pub(crate) async fn import_ledger(
        &self,
        source: &SkillUsageSource,
        ledger: SkillLedger,
    ) -> Result<(), SkillUsageError> {
        let Some(owner) = self.owner.as_ref() else {
            return Err(SkillUsageError::Backend {
                reason: "skill listing is not managed in this deployment".to_string(),
            });
        };
        bounded(
            SKILL_USAGE_ADVISORY_TIMEOUT,
            self.store.store().import_ledger(owner, source, ledger),
        )
        .await
    }
}

pub(crate) fn now_ms() -> u64 {
    u64::try_from(ironclaw_orchestrator::model::now_ms()).unwrap_or(0) // silent-ok: a clock before 1970 only ages every use
}

/// `name` as the ledger holds it; `None` for a name it cannot (that skill
/// then stays listed as the runtime lists it).
pub(crate) fn usage_name(name: &str) -> Option<SkillUsageName> {
    SkillUsageName::new(name).ok() // silent-ok: see the doc comment
}

/// How each of `catalog` is listed from `document`; every `requested`
/// skill (the turn asked for it) is full over any pin.
pub(crate) fn decide(
    document: &SkillUsageDocument,
    catalog: &[SkillUsageName],
    always_full: &BTreeSet<SkillUsageName>,
    extra_ledger: Option<&SkillLedger>,
    requested: &BTreeSet<SkillUsageName>,
) -> BTreeMap<SkillUsageName, SkillListingState> {
    let mut ledger = document.combined_ledger();
    for (name, theirs) in extra_ledger.into_iter().flatten() {
        let entry = ledger.entry(name.clone()).or_default();
        *entry = entry.merged(*theirs);
    }
    let mut decided = listing(
        &SkillListingPolicy::default(),
        SkillListingRequest {
            catalog,
            ledger: &ledger,
            pins: &document.pins,
            always_full,
            now_ms: now_ms(),
        },
    );
    force_requested(
        &mut decided,
        &requested
            .iter()
            .filter(|name| catalog.contains(name))
            .cloned()
            .collect(),
    );
    decided
}

/// One runtime's side of listing management for agents that read the
/// profile index (Antigravity, Codex, Muse).
#[derive(Debug)]
pub(crate) struct CliSkills {
    usage: CliSkillUsage,
    source: Option<SkillUsageSource>,
    /// Per thread, the skills its sessions were shown and their SKILL.md
    /// paths (what a read is matched against).
    shown: Mutex<HashMap<ThreadId, Arc<Vec<ShownSkill>>>>,
}

impl CliSkills {
    pub(crate) fn new(usage: CliSkillUsage, runtime_id: &str) -> Self {
        let source = SkillUsageSource::cli_runtime(runtime_id)
            .inspect_err(|error| tracing::debug!(%error, "cli skills: runtime id not a source"))
            .ok(); // silent-ok: logged; the runtime then lists every skill in full
        Self {
            usage,
            source,
            shown: Mutex::new(HashMap::new()),
        }
    }

    /// `profile` with its skill index rendered per the listing (and the
    /// profile's skills recorded as this runtime's catalog); the skills
    /// `text` asks for stay full. Unchanged when listing is not managed.
    ///
    /// The index reaches a session once (its preamble), so a skill asked
    /// for on a later turn would still be listed as the session first saw
    /// it: [`ListedProfile::requested`] names each requested skill for this
    /// turn's context.
    pub(crate) async fn listed_profile(
        &self,
        thread: &ThreadId,
        profile: Option<&TurnProfile>,
        text: &str,
    ) -> ListedProfile {
        let Some(profile) = profile.cloned() else {
            return ListedProfile::default();
        };
        let Some(skills) = profile.skills.as_ref() else {
            return ListedProfile::unchanged(profile);
        };
        let catalog: Vec<SkillUsageName> = skills
            .entries
            .iter()
            .filter_map(|e| usage_name(&e.name))
            .collect();
        let shown_skills: Vec<ShownSkill> = skills
            .entries
            .iter()
            .filter_map(|e| {
                Some(ShownSkill {
                    name: usage_name(&e.name)?,
                    skill_md: e.skill_md.display().to_string(),
                })
            })
            .collect();
        if let Ok(mut shown) = self.shown.lock() {
            shown.insert(thread.clone(), Arc::new(shown_skills));
        }
        let (Some(source), Some(document)) = (&self.source, self.usage.document().await) else {
            return ListedProfile::unchanged(profile);
        };
        if document.catalog(source) != catalog.as_slice() {
            self.usage
                .record(
                    source,
                    SkillUsageObservation {
                        catalog: Some(catalog.clone()),
                        used: Vec::new(),
                    },
                )
                .await;
        }
        let requested_names = requested_skills(text, &catalog);
        let decided = decide(
            &document,
            &catalog,
            &BTreeSet::new(),
            None,
            &requested_names,
        );
        let index = render_index(skills, &decided);
        let requested = requested_note(skills, &requested_names);
        ListedProfile {
            profile: Some(TurnProfile {
                skills: Some(ProfileSkills {
                    index,
                    ..skills.clone()
                }),
                ..profile
            }),
            requested,
        }
    }

    /// A fresh watch for one turn of `thread`.
    pub(crate) fn watch(&self, thread: &ThreadId) -> SkillReadWatch {
        let shown = self
            .shown
            .lock()
            .ok()
            .and_then(|shown| shown.get(thread).cloned())
            .unwrap_or_default(); // silent-ok: a poisoned map only follows a panic elsewhere; nothing is matched
        SkillReadWatch::new(shown)
    }

    /// Count the skills one turn read.
    pub(crate) async fn record(&self, watch: SkillReadWatch) {
        let Some(source) = &self.source else {
            return; // silent-ok: logged at construction
        };
        let used: Vec<SkillUsageName> = watch.used.into_iter().collect();
        if used.is_empty() {
            return;
        }
        self.usage
            .record(
                source,
                SkillUsageObservation {
                    catalog: None,
                    used,
                },
            )
            .await;
    }
}

/// A turn's profile as its runtime sends it, and the note naming the skills
/// the turn's text asked for (`None` when it asked for none, or listing is
/// not managed and the index already lists every skill in full).
#[derive(Debug, Default)]
pub(crate) struct ListedProfile {
    pub profile: Option<TurnProfile>,
    pub requested: Option<String>,
}

impl ListedProfile {
    fn unchanged(profile: TurnProfile) -> Self {
        Self {
            profile: Some(profile),
            requested: None,
        }
    }
}

/// The context note for `requested`: each skill in full, with its path.
fn requested_note(skills: &ProfileSkills, requested: &BTreeSet<SkillUsageName>) -> Option<String> {
    let lines: Vec<String> = skills
        .entries
        .iter()
        .filter(|e| usage_name(&e.name).is_some_and(|n| requested.contains(&n)))
        .map(|e| format!("- {}: {} ({})", e.name, e.description, e.skill_md.display()))
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "Skills asked for in this message. Read each SKILL.md and follow it:\n{}",
            lines.join("\n")
        )
    })
}

/// The index lines per listing: `- name: description (path)` for a full
/// skill, `- name (path)` for a name-only one, nothing for an off one. A
/// skill the listing has no entry for stays full.
pub(crate) fn render_index(
    skills: &ProfileSkills,
    decided: &BTreeMap<SkillUsageName, SkillListingState>,
) -> String {
    skills
        .entries
        .iter()
        .filter_map(|entry| {
            let listed = usage_name(&entry.name)
                .and_then(|n| decided.get(&n).map(|s| s.listing))
                .unwrap_or(SkillListing::Full);
            let path = entry.skill_md.display();
            match listed {
                SkillListing::Off => None,
                SkillListing::NameOnly => Some(format!("- {} ({path})", entry.name)),
                SkillListing::Full => {
                    Some(format!("- {}: {} ({path})", entry.name, entry.description))
                }
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One shown skill: its ledger name and the SKILL.md path the index gave.
#[derive(Debug, Clone)]
pub(crate) struct ShownSkill {
    name: SkillUsageName,
    skill_md: String,
}

/// Shell programs that only print a file (with flags and, for `sed`, a
/// script): a completed run of one naming a SKILL.md read it.
const READ_PROGRAMS: [&str; 8] = ["cat", "head", "tail", "sed", "less", "more", "bat", "nl"];
/// Shells a runtime wraps a command in (`/bin/zsh -lc '…'`).
const WRAPPING_SHELLS: [&str; 3] = ["sh", "bash", "zsh"];
/// Muse's file-read tools.
const MUSE_READ_TOOLS: [&str; 4] = ["read_file", "read", "Read", "view_file"];
/// Where a read's path sits in a tool's input (ACP `rawInput`, Muse `args`).
const PATH_FIELDS: [&str; 7] = [
    "AbsolutePath",
    "FilePath",
    "TargetFile",
    "path",
    "file_path",
    "filePath",
    "absolute_path",
];
/// Where a command sits in ACP `rawInput`.
const COMMAND_FIELDS: [&str; 4] = ["CommandLine", "command_line", "commandLine", "command"];

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// A command that finished without an error status or a non-zero exit.
fn succeeded(item: &Value) -> bool {
    str_field(item, "status") == Some("completed")
        && item
            .get("exitCode")
            .and_then(Value::as_i64)
            .is_none_or(|code| code == 0)
}

/// What one ACP tool call has said about itself so far (its first frame
/// names the kind and input; later updates may carry only the status).
#[derive(Debug, Default)]
struct AcpCall {
    kind: Option<String>,
    paths: Vec<String>,
    command: Option<String>,
}

/// Which of a turn's shown skills it read: a skill counts once for the
/// turn, on a typed read of its SKILL.md or a read-only shell command of
/// it that completed successfully.
#[derive(Debug, Default)]
pub(crate) struct SkillReadWatch {
    shown: Arc<Vec<ShownSkill>>,
    acp_calls: HashMap<String, AcpCall>,
    used: BTreeSet<SkillUsageName>,
}

impl SkillReadWatch {
    fn new(shown: Arc<Vec<ShownSkill>>) -> Self {
        Self {
            shown,
            acp_calls: HashMap::new(),
            used: BTreeSet::new(),
        }
    }

    /// The shown skill whose SKILL.md `path` is: its exact path, or a
    /// relative path ending in `skills/<name>/SKILL.md`.
    fn read_path(&mut self, path: &str) {
        let path = path.trim();
        for skill in self.shown.iter() {
            let relative = format!("skills/{}/SKILL.md", skill.name);
            let hit = path == skill.skill_md
                || (!Path::new(path).is_absolute()
                    && (path == relative || path.ends_with(&format!("/{relative}"))));
            if hit {
                self.used.insert(skill.name.clone());
            }
        }
    }

    fn read_command(&mut self, command: &str) {
        for path in command_read_paths(command) {
            self.read_path(&path);
        }
    }

    /// One ACP `session/update` `update` (Antigravity): a `tool_call` /
    /// `tool_call_update` of kind `read` (its `locations` or input path),
    /// or of kind `execute` whose command only reads, counts once its
    /// status is `completed`.
    pub(crate) fn push_acp(&mut self, update: &Value) {
        if self.shown.is_empty()
            || !matches!(
                str_field(update, "sessionUpdate"),
                Some("tool_call" | "tool_call_update")
            )
        {
            return;
        }
        let Some(id) = str_field(update, "toolCallId") else {
            return;
        };
        let call = self.acp_calls.entry(id.to_string()).or_default();
        if let Some(kind) = str_field(update, "kind") {
            call.kind = Some(kind.to_string());
        }
        let input = &update["rawInput"];
        let located = update["locations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| str_field(l, "path"));
        let given = PATH_FIELDS.iter().filter_map(|k| str_field(input, k));
        call.paths.extend(located.chain(given).map(str::to_string));
        if let Some(command) = COMMAND_FIELDS.iter().find_map(|k| str_field(input, k)) {
            call.command = Some(command.to_string());
        }
        match str_field(update, "status") {
            Some("completed") => {
                let Some(call) = self.acp_calls.remove(id) else {
                    return;
                };
                match call.kind.as_deref() {
                    Some("read") => call.paths.iter().for_each(|p| self.read_path(p)),
                    Some("execute") => {
                        if let Some(command) = &call.command {
                            self.read_command(command);
                        }
                    }
                    _ => {}
                }
            }
            Some("failed") => {
                self.acp_calls.remove(id);
            }
            _ => {}
        }
    }

    /// One Codex app-server notification: a completed `commandExecution`
    /// item that exited 0 and either reports a `read` command action of the
    /// file or is itself a read-only command of it.
    pub(crate) fn push_codex(&mut self, method: &str, params: &Value) {
        if self.shown.is_empty() || method != "item/completed" {
            return;
        }
        let item = &params["item"];
        if str_field(item, "type") != Some("commandExecution") || !succeeded(item) {
            return;
        }
        let actions: Vec<String> = item["commandActions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| str_field(a, "type") == Some("read"))
            .filter_map(|a| str_field(a, "path").map(str::to_string))
            .collect();
        for path in &actions {
            self.read_path(path);
        }
        if let Some(command) = str_field(item, "command") {
            self.read_command(command);
        }
    }

    /// One Muse MSP notification: a completed item that is a file-read tool
    /// call of the file (`args` path) or a shell run that only read it.
    pub(crate) fn push_muse(&mut self, method: &str, params: &Value) {
        if self.shown.is_empty() || !matches!(method, "item/updated" | "item/completed") {
            return;
        }
        let item = &params["item"];
        if !succeeded(item) {
            return;
        }
        let tool = str_field(item, "tool");
        if str_field(item, "kind") == Some("toolCall")
            && tool.is_some_and(|t| MUSE_READ_TOOLS.contains(&t))
        {
            let args: Option<Value> =
                str_field(item, "args").and_then(|a| serde_json::from_str(a).ok()); // silent-ok: arguments that are not JSON name no path
            let paths: Vec<String> = args
                .iter()
                .flat_map(|a| PATH_FIELDS.iter().filter_map(|k| str_field(a, k)))
                .map(str::to_string)
                .collect();
            for path in &paths {
                self.read_path(path);
            }
            return;
        }
        if let Some(command) = str_field(item, "commandText") {
            self.read_command(command);
        }
    }
}

/// The file arguments of `command` when it is one read-only program
/// ([`READ_PROGRAMS`]) run on its own, possibly inside `sh|bash|zsh -c`;
/// empty for anything else (pipes, chains, redirects, substitutions, or any
/// other program, `echo` included).
fn command_read_paths(command: &str) -> Vec<String> {
    let Some(words) = shell_words(command) else {
        return Vec::new();
    };
    let program = |word: &str| word.rsplit('/').next().unwrap_or(word).to_string();
    let Some(first) = words.first() else {
        return Vec::new();
    };
    if WRAPPING_SHELLS.contains(&program(first).as_str()) {
        return match words.as_slice() {
            [_, flag, inner] if matches!(flag.as_str(), "-c" | "-lc") => command_read_paths(inner),
            _ => Vec::new(),
        };
    }
    if !READ_PROGRAMS.contains(&program(first).as_str()) {
        return Vec::new();
    }
    words[1..]
        .iter()
        .filter(|w| !w.starts_with('-'))
        .cloned()
        .collect()
}

/// `command` split into words (single and double quotes, backslash
/// escapes); `None` when it holds an unquoted operator, redirect or
/// substitution, i.e. is more than one plain program run.
fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        '$' | '`' => return None,
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next()?);
            }
            c if c.is_whitespace() && c != '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            ';' | '&' | '|' | '>' | '<' | '`' | '$' | '(' | ')' | '\n' => return None,
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_orchestrator::runtime::ProfileSkillEntry;
    use ironclaw_skills::usage::{SkillListingBasis, SkillUse};

    fn skills() -> ProfileSkills {
        let entry = |name: &str, description: &str| ProfileSkillEntry {
            name: name.into(),
            description: description.into(),
            skill_md: format!("/p/skills/{name}/SKILL.md").into(),
        };
        ProfileSkills {
            plugin_dir: "/p".into(),
            index: String::new(),
            entries: vec![
                entry("hot", "Used a lot"),
                entry("cold", "Rarely used"),
                entry("gone", "Pinned off"),
            ],
        }
    }

    #[test]
    fn the_index_lists_name_only_skills_by_name_and_path_and_drops_off_ones() {
        let state = |listing| SkillListingState {
            listing,
            basis: SkillListingBasis::Auto,
        };
        let decided = BTreeMap::from([
            (usage_name("hot").unwrap(), state(SkillListing::Full)),
            (usage_name("cold").unwrap(), state(SkillListing::NameOnly)),
            (usage_name("gone").unwrap(), state(SkillListing::Off)),
        ]);
        assert_eq!(
            render_index(&skills(), &decided),
            "- hot: Used a lot (/p/skills/hot/SKILL.md)\n- cold (/p/skills/cold/SKILL.md)"
        );
    }

    fn watch() -> SkillReadWatch {
        let shown = |name: &str| ShownSkill {
            name: usage_name(name).unwrap(),
            skill_md: format!("/p/skills/{name}/SKILL.md"),
        };
        SkillReadWatch::new(Arc::new(vec![shown("hot"), shown("cold"), shown("other")]))
    }

    fn used(watch: &SkillReadWatch) -> Vec<&str> {
        watch.used.iter().map(SkillUsageName::as_str).collect()
    }

    #[test]
    fn only_a_completed_read_only_command_of_the_file_is_a_read() {
        for (command, read) in [
            ("cat /p/skills/cold/SKILL.md", true),
            ("sed -n 1,80p /p/skills/cold/SKILL.md", true),
            ("head -n 40 '/p/skills/cold/SKILL.md'", true),
            (
                r#"/bin/zsh -lc "sed -n 1,200p /p/skills/cold/SKILL.md""#,
                true,
            ),
            ("cat ./skills/cold/SKILL.md", true),
            ("echo /p/skills/cold/SKILL.md", false),
            ("cat /p/skills/cold/SKILL.md > /tmp/x", false),
            ("cat /p/skills/cold/SKILL.md && rm -rf x", false),
            ("cat /elsewhere/skills/cold/SKILL.md", false),
            ("cat /p/skills/cold/SKILL.md.bak", false),
            ("grep -r skills/cold/SKILL.md .", false),
        ] {
            let mut w = watch();
            w.read_command(command);
            assert_eq!(used(&w) == ["cold"], read, "{command}");
        }
    }

    #[test]
    fn codex_counts_a_completed_successful_command_execution_only() {
        let item = |status: &str, exit: i64, command: &str| {
            serde_json::json!({"item": {"type": "commandExecution", "id": "c", "command": command,
                "status": status, "exitCode": exit, "aggregatedOutput": "see /p/skills/hot/SKILL.md"}})
        };
        let mut w = watch();
        w.push_codex(
            "item/started",
            &item("inProgress", 0, "cat /p/skills/cold/SKILL.md"),
        );
        w.push_codex(
            "item/completed",
            &item("failed", 1, "cat /p/skills/cold/SKILL.md"),
        );
        w.push_codex(
            "item/completed",
            &item("completed", 1, "cat /p/skills/cold/SKILL.md"),
        );
        w.push_codex("item/completed", &item("completed", 0, "ls /p/skills"));
        assert!(
            used(&w).is_empty(),
            "no successful read yet; output never counts"
        );
        w.push_codex(
            "item/completed",
            &serde_json::json!({"item": {"type": "commandExecution", "id": "c", "command": "rg x",
                "status": "completed", "exitCode": 0,
                "commandActions": [{"type": "read", "path": "/p/skills/other/SKILL.md"}]}}),
        );
        w.push_codex(
            "item/completed",
            &item("completed", 0, "cat /p/skills/cold/SKILL.md"),
        );
        w.push_codex(
            "item/completed",
            &serde_json::json!({"item": {"type": "agentMessage", "text": "cat /p/skills/hot/SKILL.md"}}),
        );
        assert_eq!(used(&w), ["cold", "other"]);
    }

    #[test]
    fn antigravity_counts_a_completed_read_or_execute_call() {
        let mut w = watch();
        // A read: the kind and location come first, the status later.
        w.push_acp(&serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "r1",
            "kind": "read", "status": "pending", "locations": [{"path": "/p/skills/cold/SKILL.md"}]}));
        assert!(used(&w).is_empty(), "not finished yet");
        w.push_acp(
            &serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "r1",
            "status": "completed"}),
        );
        // A failed read, an echo, and output naming a path do not count.
        w.push_acp(
            &serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "r2",
            "kind": "read", "rawInput": {"AbsolutePath": "/p/skills/hot/SKILL.md"}}),
        );
        w.push_acp(
            &serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "r2",
            "status": "failed"}),
        );
        w.push_acp(
            &serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "e1",
            "kind": "execute", "status": "completed",
            "rawInput": {"CommandLine": "echo /p/skills/hot/SKILL.md"}}),
        );
        w.push_acp(
            &serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "e2",
            "kind": "execute", "status": "completed", "rawInput": {"CommandLine": "ls"},
            "rawOutput": {"output": "/p/skills/hot/SKILL.md"}}),
        );
        w.push_acp(&serde_json::json!({"sessionUpdate": "agent_message_chunk",
            "content": {"text": "/p/skills/hot/SKILL.md"}}));
        // A shell read counts.
        w.push_acp(
            &serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "e3",
            "kind": "execute", "rawInput": {"CommandLine": "cat /p/skills/other/SKILL.md"}}),
        );
        w.push_acp(
            &serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "e3",
            "status": "completed"}),
        );
        assert_eq!(used(&w), ["cold", "other"]);
    }

    #[test]
    fn muse_counts_a_completed_read_tool_or_shell_read() {
        let item = |kind: &str, status: &str, extra: serde_json::Value| {
            let mut item = serde_json::json!({"itemId": "i", "kind": kind, "status": status});
            item.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::json!({ "item": item })
        };
        let args = |path: &str| serde_json::json!({"path": path}).to_string();
        let mut w = watch();
        w.push_muse(
            "item/started",
            &item(
                "toolCall",
                "inProgress",
                serde_json::json!({"tool": "read_file", "args": args("/p/skills/cold/SKILL.md")}),
            ),
        );
        w.push_muse(
            "item/completed",
            &item(
                "toolCall",
                "failed",
                serde_json::json!({"tool": "read_file", "args": args("/p/skills/hot/SKILL.md")}),
            ),
        );
        w.push_muse(
            "item/completed",
            &item(
                "userShell",
                "completed",
                serde_json::json!({"commandText": "echo /p/skills/hot/SKILL.md", "exitCode": 0}),
            ),
        );
        w.push_muse(
            "item/completed",
            &item(
                "toolCall",
                "completed",
                serde_json::json!({"tool": "grep", "visibleOutput": "/p/skills/hot/SKILL.md"}),
            ),
        );
        assert!(used(&w).is_empty());
        w.push_muse(
            "item/completed",
            &item(
                "toolCall",
                "completed",
                serde_json::json!({"tool": "read_file", "args": args("/p/skills/cold/SKILL.md")}),
            ),
        );
        w.push_muse(
            "item/completed",
            &item(
                "userShell",
                "completed",
                serde_json::json!({"commandText": "cat /p/skills/other/SKILL.md", "exitCode": 0}),
            ),
        );
        assert_eq!(used(&w), ["cold", "other"]);
    }

    #[test]
    fn a_requested_skill_stays_full_over_an_off_pin() {
        let mut document = SkillUsageDocument::default();
        document
            .set_pin(
                &usage_name("gone").unwrap(),
                Some(ironclaw_skills::usage::SkillPin::Off),
            )
            .unwrap();
        let catalog = vec![usage_name("gone").unwrap(), usage_name("cold").unwrap()];
        let requested = requested_skills("please use $gone", &catalog);
        let got = decide(&document, &catalog, &BTreeSet::new(), None, &requested);
        assert_eq!(got[&catalog[0]].listing, SkillListing::Full);
        assert_eq!(got[&catalog[1]].listing, SkillListing::NameOnly);
    }

    #[test]
    fn decide_merges_an_extra_ledger() {
        let document = SkillUsageDocument::default();
        let catalog = vec![usage_name("a").unwrap()];
        let extra = SkillLedger::from([(
            usage_name("a").unwrap(),
            SkillUse {
                count: 3,
                last_used_ms: now_ms(),
            },
        )]);
        let got = decide(
            &document,
            &catalog,
            &BTreeSet::new(),
            Some(&extra),
            &BTreeSet::new(),
        );
        assert_eq!(got[&catalog[0]].listing, SkillListing::Full);
    }
}
