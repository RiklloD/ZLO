use ironclaw_config::RebornBootConfig;

/// Per-invocation context shared by Reborn CLI commands.
#[derive(Debug, Clone)]
pub(crate) struct RebornCliContext {
    boot_config: RebornBootConfig,
}

impl RebornCliContext {
    /// Resolves the home (moving a pre-ZLO `~/.ironclaw/reborn` to `~/.zlo`
    /// on first run — see `ironclaw_config::migrate_legacy_home`) and tells
    /// the user on stderr when that move happened or both folders exist.
    pub(crate) fn resolve_from_env() -> anyhow::Result<Self> {
        let boot_config = RebornBootConfig::resolve_from_env()?;
        if let Some(notice) = boot_config.home().migration().notice() {
            eprintln!("{notice}");
        }
        Ok(Self { boot_config })
    }

    #[cfg(test)]
    pub(crate) fn from_boot_config(boot_config: RebornBootConfig) -> Self {
        Self { boot_config }
    }

    #[cfg(test)]
    pub(crate) fn test_context() -> (tempfile::TempDir, Self) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = RebornBootConfig::resolve_from_env_parts(
            None,
            Some(tmp.path().as_os_str().to_os_string()),
            None,
            None,
        )
        .expect("config must resolve with HOME set");
        (tmp, Self::from_boot_config(config))
    }

    pub(crate) fn boot_config(&self) -> &RebornBootConfig {
        &self.boot_config
    }
}
