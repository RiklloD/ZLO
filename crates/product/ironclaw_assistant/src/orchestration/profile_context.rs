//! What a CLI agent (Claude CLI, Codex, Muse, Antigravity) gets from its
//! thread's profile each turn: SOUL.md, the profile folder, and the skills
//! the profile shows. The skill store stays the single source; for CLI
//! agents it is mirrored per profile into `~/.ironclaw/cli-skills/<profile>/`
//! as a Claude Code plugin folder, rewritten only when its content changes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_loop_contracts::LoopRunContext;
use ironclaw_loop_host::{SkillBundleSourceError, SkillRunFilter};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::profiles::ProfileRecord;
use ironclaw_orchestrator::runtime::{
    ProfileContextSource, ProfileSkills, RuntimeError, TurnProfile,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::profiles::ProfileHome;

/// Plugin name, so skills read `/ironclaw:<skill>` in the Claude CLI.
const PLUGIN_NAME: &str = "ironclaw";
const FINGERPRINT_FILE: &str = ".fingerprint";

/// One installed user skill with every file of its folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFiles {
    pub name: String,
    /// `(path relative to the skill folder, bytes)`, `SKILL.md` included.
    pub files: Vec<(String, Vec<u8>)>,
}

/// Read access to the user's installed skills; implemented by the host,
/// which owns the skill store.
#[async_trait]
pub trait UserSkillFiles: Send + Sync {
    async fn user_skills(&self) -> Result<Vec<SkillFiles>, String>;
}

/// [`ProfileContextSource`] over the skill store and the profile folders.
pub struct CliProfileContext {
    home: ProfileHome,
    skills: Arc<dyn UserSkillFiles>,
}

impl CliProfileContext {
    pub fn new(home: ProfileHome, skills: Arc<dyn UserSkillFiles>) -> Self {
        Self { home, skills }
    }
}

#[async_trait]
impl ProfileContextSource for CliProfileContext {
    async fn turn_profile(
        &self,
        profile: &ProfileRecord,
    ) -> Result<TurnProfile, OrchestratorError> {
        let folder = Some(self.home.folder(profile)).filter(|f| f.is_dir());
        let hidden: HashSet<&str> = profile
            .settings
            .hidden_skills
            .iter()
            .map(String::as_str)
            .collect();
        let mut shown = self.skills.user_skills().await.map_err(|reason| {
            OrchestratorError::Runtime(RuntimeError::Unavailable(format!("skill store: {reason}")))
        })?;
        shown.retain(|s| !hidden.contains(s.name.as_str()));
        shown.sort_by(|a, b| a.name.cmp(&b.name));
        let dir = self.home.cli_skills().join(profile.profile_id.as_str());
        let skills = mirror(&dir, &shown).await.map_err(|error| {
            OrchestratorError::Runtime(RuntimeError::Unavailable(format!("skill mirror: {error}")))
        })?;
        Ok(TurnProfile {
            soul: profile.settings.soul.clone(),
            folder,
            skills,
        })
    }
}

/// Write `skills` as a plugin folder at `dir` unless it already holds them.
/// `None` when there is nothing to show.
async fn mirror(dir: &Path, skills: &[SkillFiles]) -> std::io::Result<Option<ProfileSkills>> {
    if skills.is_empty() {
        return Ok(None);
    }
    let fingerprint = fingerprint(skills);
    let current = tokio::fs::read_to_string(dir.join(FINGERPRINT_FILE)).await;
    if !matches!(&current, Ok(found) if *found == fingerprint) {
        write_plugin(dir, skills, &fingerprint).await?;
    }
    Ok(Some(ProfileSkills {
        plugin_dir: dir.to_path_buf(),
        index: index(dir, skills),
    }))
}

fn fingerprint(skills: &[SkillFiles]) -> String {
    let mut hash = Sha256::new();
    for skill in skills {
        hash.update(skill.name.as_bytes());
        for (path, bytes) in &skill.files {
            hash.update([0]);
            hash.update(path.as_bytes());
            hash.update([0]);
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        hash.update([1]);
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the folder beside `dir`, then swap it in, so an agent never reads a
/// half-written skill.
async fn write_plugin(dir: &Path, skills: &[SkillFiles], fingerprint: &str) -> std::io::Result<()> {
    let parent = dir.parent().unwrap_or(dir);
    tokio::fs::create_dir_all(parent).await?;
    let staging = parent.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    let manifest = serde_json::json!({
        "name": PLUGIN_NAME,
        "version": "1.0.0",
        "description": "Skills of this IronClaw profile",
    });
    let meta = staging.join(".claude-plugin");
    tokio::fs::create_dir_all(&meta).await?;
    tokio::fs::write(meta.join("plugin.json"), manifest.to_string()).await?;
    for skill in skills {
        let root = staging.join("skills").join(&skill.name);
        for (path, bytes) in &skill.files {
            let target = root.join(path);
            if let Some(folder) = target.parent() {
                tokio::fs::create_dir_all(folder).await?;
            }
            tokio::fs::write(&target, bytes).await?;
        }
    }
    tokio::fs::write(staging.join(FINGERPRINT_FILE), fingerprint).await?;
    let old = parent.join(format!(".old-{}", uuid::Uuid::new_v4()));
    let had_old = tokio::fs::rename(dir, &old).await.is_ok(); // silent-ok: no previous mirror is the first-run case
    if let Err(error) = tokio::fs::rename(&staging, dir).await {
        // A concurrent turn of the same profile swapped its copy in first.
        let _ = tokio::fs::remove_dir_all(&staging).await; // silent-ok: staging cleanup
        let theirs = tokio::fs::read_to_string(dir.join(FINGERPRINT_FILE)).await;
        if !matches!(&theirs, Ok(found) if found == fingerprint) {
            return Err(error);
        }
    }
    if had_old {
        let _ = tokio::fs::remove_dir_all(&old).await; // silent-ok: a stale copy only costs disk
    }
    Ok(())
}

#[derive(Deserialize)]
struct Frontmatter {
    #[serde(default)]
    description: String,
}

/// One line per skill: name, description and where its SKILL.md is.
fn index(dir: &Path, skills: &[SkillFiles]) -> String {
    skills
        .iter()
        .map(|skill| {
            let path: PathBuf = dir.join("skills").join(&skill.name).join("SKILL.md");
            let description = skill
                .files
                .iter()
                .find(|(p, _)| p == "SKILL.md")
                .and_then(|(_, bytes)| description(&String::from_utf8_lossy(bytes)))
                .unwrap_or_default();
            format!("- {}: {} ({})", skill.name, description, path.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn description(skill_md: &str) -> Option<String> {
    let rest = skill_md.trim_start().strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let front: Frontmatter = serde_yaml::from_str(&rest[..end]).ok()?; // silent-ok: a skill without frontmatter is listed without description
    Some(front.description.trim().to_string()).filter(|d| !d.is_empty())
}

/// Hides each thread profile's `hidden_skills` from IronClaw's own loop.
pub struct ProfileSkillFilter {
    engine: Arc<Engine>,
}

impl ProfileSkillFilter {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl SkillRunFilter for ProfileSkillFilter {
    async fn hidden_skills(
        &self,
        run_context: &LoopRunContext,
    ) -> Result<Vec<String>, SkillBundleSourceError> {
        // Fail closed: an unreadable profile must not show what it hides.
        let profile = self
            .engine
            .store()
            .thread_profile(&run_context.thread_id)
            .await
            .map_err(|error| {
                tracing::debug!(%error, "profile lookup failed; skills unavailable this turn");
                SkillBundleSourceError::SourceUnavailable
            })?;
        Ok(profile
            .map(|p| p.settings.hidden_skills)
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, body: &str) -> SkillFiles {
        SkillFiles {
            name: name.into(),
            files: vec![
                (
                    "SKILL.md".into(),
                    format!("---\nname: {name}\ndescription: {body}\n---\n# {name}\n").into_bytes(),
                ),
                ("references/a.md".into(), b"ref".to_vec()),
            ],
        }
    }

    /// The mirror is a plugin folder with every file, an index naming each
    /// SKILL.md, and it is rewritten only when a skill changes.
    #[tokio::test]
    async fn mirror_writes_a_plugin_and_rewrites_only_on_change() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("ironclaw-builder");
        let skills = vec![skill("full-send", "Use when authorized")];

        let out = mirror(&dir, &skills).await.unwrap().unwrap();
        assert_eq!(out.plugin_dir, dir);
        assert!(dir.join(".claude-plugin/plugin.json").is_file());
        assert!(dir.join("skills/full-send/references/a.md").is_file());
        assert!(
            out.index.contains("- full-send: Use when authorized ("),
            "{}",
            out.index
        );
        assert!(
            out.index.contains("skills/full-send/SKILL.md"),
            "{}",
            out.index
        );

        let marker = dir.join("skills/full-send/marker");
        std::fs::write(&marker, "x").unwrap();
        mirror(&dir, &skills).await.unwrap();
        assert!(marker.is_file(), "unchanged skills: folder left alone");

        mirror(&dir, &[skill("full-send", "changed")])
            .await
            .unwrap();
        assert!(!marker.is_file(), "changed skill: folder rewritten");
        let leftovers: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no staging or old copies left: {leftovers:?}"
        );

        assert_eq!(mirror(&dir, &[]).await.unwrap(), None);
    }
}
