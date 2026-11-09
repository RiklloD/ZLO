//! Whether computer use can run on this machine: the engine, and the two
//! macOS permissions it needs (Accessibility for clicks and typing, Screen
//! Recording for window pictures).
//!
//! The engine runs in-process under the backend
//! (`OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY`, see `config`), so macOS
//! charges both permissions to the process that started the backend, which
//! is the one asking here: what this process is granted is what the engine
//! gets. The checks are the preflight calls (`AXIsProcessTrusted`,
//! `CGPreflightScreenCaptureAccess`): status reads that never prompt and
//! never add the process to a System Settings list. Off macOS computer use
//! is unsupported.

use std::sync::Arc;

use ironclaw_product_contracts::computer_use::{
    ComputerUseEngineState, ComputerUsePermissionState, ComputerUsePlatform, ComputerUseReadiness,
    ComputerUseReadinessProbe,
};

use crate::config::ComputerUseConfig;

/// The permission preflight, behind a seam so tests are deterministic.
/// `None` means it cannot be determined here.
pub trait PermissionPreflight: Send + Sync {
    fn accessibility(&self) -> Option<bool>;
    fn screen_recording(&self) -> Option<bool>;
}

/// The host's own preflight: macOS status calls, `None` elsewhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostPermissionPreflight;

impl PermissionPreflight for HostPermissionPreflight {
    fn accessibility(&self) -> Option<bool> {
        mac::accessibility()
    }

    fn screen_recording(&self) -> Option<bool> {
        mac::screen_recording()
    }
}

/// [`ComputerUseReadinessProbe`] for the machine the backend runs on.
pub struct HostComputerUseReadiness {
    platform: ComputerUsePlatform,
    engine: ComputerUseEngineState,
    preflight: Arc<dyn PermissionPreflight>,
}

impl HostComputerUseReadiness {
    /// From the configuration the backend started with (`None`: computer
    /// use is off) and whether `COMPUTER_USE_ENABLED` turned it off.
    pub fn from_env(config: Option<&ComputerUseConfig>) -> Self {
        let disabled = std::env::var("COMPUTER_USE_ENABLED").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        });
        Self::new(
            host_platform(),
            engine_state(config, disabled),
            Arc::new(HostPermissionPreflight),
        )
    }

    pub fn new(
        platform: ComputerUsePlatform,
        engine: ComputerUseEngineState,
        preflight: Arc<dyn PermissionPreflight>,
    ) -> Self {
        Self {
            platform,
            engine,
            preflight,
        }
    }
}

impl ComputerUseReadinessProbe for HostComputerUseReadiness {
    fn readiness(&self) -> ComputerUseReadiness {
        if self.platform == ComputerUsePlatform::Unsupported {
            return ComputerUseReadiness {
                platform: self.platform,
                engine: self.engine,
                accessibility: ComputerUsePermissionState::NotApplicable,
                screen_recording: ComputerUsePermissionState::NotApplicable,
            };
        }
        let state = |granted: Option<bool>| match granted {
            Some(true) => ComputerUsePermissionState::Granted,
            Some(false) => ComputerUsePermissionState::Missing,
            None => ComputerUsePermissionState::Unknown,
        };
        ComputerUseReadiness {
            platform: self.platform,
            engine: self.engine,
            accessibility: state(self.preflight.accessibility()),
            screen_recording: state(self.preflight.screen_recording()),
        }
    }
}

fn host_platform() -> ComputerUsePlatform {
    if cfg!(target_os = "macos") {
        ComputerUsePlatform::Macos
    } else {
        ComputerUsePlatform::Unsupported
    }
}

fn engine_state(config: Option<&ComputerUseConfig>, disabled: bool) -> ComputerUseEngineState {
    match config {
        Some(config) if config.binary.is_file() => ComputerUseEngineState::Present,
        Some(_) => ComputerUseEngineState::Missing,
        None if disabled => ComputerUseEngineState::Disabled,
        None => ComputerUseEngineState::Missing,
    }
}

#[cfg(target_os = "macos")]
mod mac {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }

    pub(super) fn accessibility() -> Option<bool> {
        // SAFETY: a status query with no arguments; it never prompts.
        Some(unsafe { AXIsProcessTrusted() })
    }

    pub(super) fn screen_recording() -> Option<bool> {
        // SAFETY: a status query with no arguments; it never prompts.
        Some(unsafe { CGPreflightScreenCaptureAccess() })
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    pub(super) fn accessibility() -> Option<bool> {
        None
    }

    pub(super) fn screen_recording() -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Option<bool>, Option<bool>);

    impl PermissionPreflight for Fixed {
        fn accessibility(&self) -> Option<bool> {
            self.0
        }
        fn screen_recording(&self) -> Option<bool> {
            self.1
        }
    }

    fn probe(
        platform: ComputerUsePlatform,
        engine: ComputerUseEngineState,
        ax: Option<bool>,
        screen: Option<bool>,
    ) -> ComputerUseReadiness {
        HostComputerUseReadiness::new(platform, engine, Arc::new(Fixed(ax, screen))).readiness()
    }

    #[test]
    fn readiness_reports_each_permission_from_the_preflight() {
        use ComputerUsePermissionState::*;
        let ready = probe(
            ComputerUsePlatform::Macos,
            ComputerUseEngineState::Present,
            Some(true),
            Some(true),
        );
        assert!(ready.ready());
        let screen = probe(
            ComputerUsePlatform::Macos,
            ComputerUseEngineState::Present,
            Some(true),
            Some(false),
        );
        assert_eq!(
            (screen.accessibility, screen.screen_recording),
            (Granted, Missing)
        );
        assert!(!screen.ready());
        let unknown = probe(
            ComputerUsePlatform::Macos,
            ComputerUseEngineState::Present,
            None,
            Some(true),
        );
        assert_eq!(unknown.accessibility, Unknown);
        assert!(!unknown.ready());
        let no_engine = probe(
            ComputerUsePlatform::Macos,
            ComputerUseEngineState::Missing,
            Some(true),
            Some(true),
        );
        assert!(!no_engine.ready());
    }

    #[test]
    fn non_macos_hosts_are_unsupported_and_never_probed() {
        struct Panics;
        impl PermissionPreflight for Panics {
            fn accessibility(&self) -> Option<bool> {
                panic!("probed off macOS")
            }
            fn screen_recording(&self) -> Option<bool> {
                panic!("probed off macOS")
            }
        }
        let readiness = HostComputerUseReadiness::new(
            ComputerUsePlatform::Unsupported,
            ComputerUseEngineState::Missing,
            Arc::new(Panics),
        )
        .readiness();
        assert_eq!(
            readiness.accessibility,
            ComputerUsePermissionState::NotApplicable
        );
        assert!(!readiness.ready());
    }

    #[test]
    fn engine_state_follows_the_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("OpenComputerUse");
        std::fs::write(&binary, "").unwrap();
        let present = ComputerUseConfig::new(&binary);
        assert_eq!(
            engine_state(Some(&present), false),
            ComputerUseEngineState::Present
        );
        let gone = ComputerUseConfig::new(dir.path().join("missing"));
        assert_eq!(
            engine_state(Some(&gone), false),
            ComputerUseEngineState::Missing
        );
        assert_eq!(engine_state(None, true), ComputerUseEngineState::Disabled);
        assert_eq!(engine_state(None, false), ComputerUseEngineState::Missing);
    }
}
