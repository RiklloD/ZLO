//! The owner's installed skills read straight from the skill store, so the
//! orchestrator can mirror each profile's set for CLI agents.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_assistant::orchestration::{SkillFiles, UserSkillFiles};
use ironclaw_filesystem::{CompositeRootFilesystem, FileType, ScopedFilesystem};
use ironclaw_host_api::path::ScopedPath;
use ironclaw_host_api::resource::ResourceScope;

/// Same root the first-party skill extension reads user skills from.
const USER_SKILLS_ROOT: &str = "/skills";
const MAX_SKILLS: usize = 512;
const MAX_FILES_PER_SKILL: usize = 64;
const MAX_DEPTH: usize = 4;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

pub(crate) struct StoreUserSkillFiles {
    filesystem: Arc<ScopedFilesystem<CompositeRootFilesystem>>,
    owner: ResourceScope,
}

impl StoreUserSkillFiles {
    pub(crate) fn new(
        filesystem: Arc<ScopedFilesystem<CompositeRootFilesystem>>,
        owner: ResourceScope,
    ) -> Self {
        Self { filesystem, owner }
    }

    fn path(raw: &str) -> Result<ScopedPath, String> {
        ScopedPath::new(raw).map_err(|e| e.to_string())
    }

    /// Every file under `dir`, as paths relative to `base`. Dot-files
    /// (install metadata) stay behind.
    async fn files(
        &self,
        base: &str,
        dir: &str,
        depth: usize,
        out: &mut Vec<(String, Vec<u8>)>,
        budget: &mut usize,
    ) -> Result<(), String> {
        let entries = self
            .filesystem
            .list_dir(&self.owner, &Self::path(dir)?)
            .await
            .map_err(|e| e.to_string())?;
        for entry in entries {
            if entry.name.starts_with('.') || out.len() >= MAX_FILES_PER_SKILL {
                continue;
            }
            let child = format!("{dir}/{}", entry.name);
            match entry.file_type {
                FileType::Directory if depth < MAX_DEPTH => {
                    Box::pin(self.files(base, &child, depth + 1, out, budget)).await?;
                }
                FileType::File => {
                    let bytes = self
                        .filesystem
                        .read_bytes(&self.owner, &Self::path(&child)?)
                        .await
                        .map_err(|e| e.to_string())?;
                    *budget = budget.saturating_sub(bytes.len());
                    if *budget == 0 {
                        return Err("skills exceed the mirror size limit".into());
                    }
                    let relative = child
                        .strip_prefix(base)
                        .unwrap_or(&child)
                        .trim_start_matches('/');
                    out.push((relative.to_string(), bytes));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[async_trait]
impl UserSkillFiles for StoreUserSkillFiles {
    async fn user_skills(&self) -> Result<Vec<SkillFiles>, String> {
        let entries = self
            .filesystem
            .list_dir(&self.owner, &Self::path(USER_SKILLS_ROOT)?)
            .await
            .map_err(|e| e.to_string())?;
        let mut budget = MAX_TOTAL_BYTES;
        let mut skills = Vec::new();
        for entry in entries.into_iter().take(MAX_SKILLS) {
            if entry.file_type != FileType::Directory || entry.name.starts_with('.') {
                continue;
            }
            let base = format!("{USER_SKILLS_ROOT}/{}", entry.name);
            let mut files = Vec::new();
            self.files(&base, &base, 0, &mut files, &mut budget).await?;
            // A folder without SKILL.md is not a skill.
            if files.iter().any(|(path, _)| path == "SKILL.md") {
                skills.push(SkillFiles {
                    name: entry.name,
                    files,
                });
            }
        }
        Ok(skills)
    }
}
