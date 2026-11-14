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
        bind_trace_commons_home(&boot_config)?;
        Ok(Self { boot_config })
    }

    /// [`Self::resolve_from_env`] without the first-run home move: when only
    /// the pre-ZLO `~/.ironclaw/reborn` exists it is used where it is. For
    /// read-only commands (`doctor --bundle`) that must not change the home.
    pub(crate) fn resolve_from_env_without_migrating() -> anyhow::Result<Self> {
        let home = ironclaw_config::RebornHome::resolve_from_env_without_migrating()?;
        let profile =
            ironclaw_config::RebornProfile::from_env_value(ironclaw_config::profile_env_value())?;
        let boot_config = RebornBootConfig::new(home, profile);
        bind_trace_commons_home(&boot_config)?;
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

/// Keep Trace Commons' local state (`trace_contributions/`) inside the
/// resolved ZLO home rather than the crate default `~/.ironclaw`, so no
/// process this binary starts writes outside its home.
fn bind_trace_commons_home(boot_config: &RebornBootConfig) -> anyhow::Result<()> {
    let home = boot_config.home().path().to_path_buf();
    ironclaw_trace_commons::location::set_base_dir(home).map_err(|existing| {
        anyhow::anyhow!(
            "Trace Commons state is already bound to {} in this process",
            existing.display()
        )
    })
}
