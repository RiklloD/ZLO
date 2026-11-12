use clap::Args;
use ironclaw_composition::{RebornRuntimeComponentStatus, reborn_runtime_readiness_snapshot};
use ironclaw_config::{RebornConfigFile, RebornDoctorReport};

use crate::context::RebornCliContext;
use crate::dto::{CheckCategory, CheckOutcome, DoctorCheck, DoctorDto, DoctorSummary};
use crate::render::{self, OutputMode, Renderable, terminal_safe_text};
use std::io::Write;

#[derive(Debug, Args)]
pub(crate) struct DoctorCommand {
    /// Output as JSON.
    #[arg(long, conflicts_with = "bundle")]
    json: bool,
    /// Write a sanitized diagnostics bundle (zip) for a bug report to FILE
    /// instead of printing checks. Local file only; secrets, tokens and
    /// conversation content are never included.
    #[arg(long, value_name = "FILE")]
    bundle: Option<std::path::PathBuf>,
    /// With --bundle: leave the service log tails out.
    #[arg(long, requires = "bundle")]
    no_logs: bool,
    /// With --bundle: overwrite FILE if it already exists.
    #[arg(long, requires = "bundle")]
    force: bool,
}

impl DoctorCommand {
    /// `true` for `doctor --bundle`, which resolves its home without the
    /// legacy-home migration.
    pub(crate) fn writes_bundle(&self) -> bool {
        self.bundle.is_some()
    }

    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        if let Some(out) = self.bundle {
            let report = crate::diagnostics_bundle::write_bundle(
                &context,
                &crate::diagnostics_bundle::BundleOptions {
                    out,
                    include_logs: !self.no_logs,
                    force: self.force,
                },
            )?;
            println!("Wrote diagnostics bundle {}", report.out.display());
            for entry in &report.entries {
                println!("  {entry}");
            }
            if !report.degraded.is_empty() {
                println!(
                    "Warning: the list of known secret values may be incomplete, so only \
                     pattern-based redaction protects values from these sources (recorded in \
                     manifest.json; consider --no-logs):"
                );
                for reason in &report.degraded {
                    println!("  {}", terminal_safe_text(reason));
                }
            }
            println!("Review it before sharing; nothing was uploaded.");
            return Ok(());
        }
        let dto = build_doctor_dto(&context);
        let mode = if self.json {
            OutputMode::Json
        } else {
            OutputMode::Text
        };
        render::output(&dto, mode)
    }
}

pub(crate) fn build_doctor_dto(context: &RebornCliContext) -> DoctorDto {
    let mut checks = Vec::new();

    let report = RebornDoctorReport::from_config(context.boot_config().clone());

    checks.push(DoctorCheck {
        name: "reborn_home".to_string(),
        category: CheckCategory::Core,
        outcome: if report.home_path().is_dir() {
            CheckOutcome::Pass
        } else {
            CheckOutcome::Fail
        },
        detail: format!(
            "{} ({})",
            report.home_path().display(),
            report.home_source_label()
        ),
    });

    checks.push(DoctorCheck {
        name: "profile".to_string(),
        category: CheckCategory::Core,
        outcome: CheckOutcome::Pass,
        detail: report.profile().to_string(),
    });

    let config_path = context.boot_config().home().config_file_path();
    checks.push(check_config_file(&config_path));

    let providers_path = context.boot_config().home().providers_file_path();
    checks.push(check_providers_file(&providers_path));

    let snapshot = reborn_runtime_readiness_snapshot();

    checks.push(driver_check("text_only_driver", &snapshot.text_only_driver));
    checks.push(driver_check("planned_driver", &snapshot.planned_driver));
    checks.push(driver_check(
        "subagent_planned_driver",
        &snapshot.subagent_planned_driver,
    ));
    checks.push(driver_check(
        "planned_default_profile",
        &snapshot.planned_default_profile,
    ));

    let (pass, fail, skip) = checks
        .iter()
        .fold((0, 0, 0), |counts, check| match check.outcome {
            CheckOutcome::Pass => (counts.0 + 1, counts.1, counts.2),
            CheckOutcome::Fail => (counts.0, counts.1 + 1, counts.2),
            CheckOutcome::Skip => (counts.0, counts.1, counts.2 + 1),
        });

    DoctorDto {
        checks,
        summary: DoctorSummary { pass, fail, skip },
    }
}

fn check_config_file(path: &std::path::Path) -> DoctorCheck {
    match RebornConfigFile::load(path) {
        Ok(Some(_)) => DoctorCheck {
            name: "config_file".to_string(),
            category: CheckCategory::Core,
            outcome: CheckOutcome::Pass,
            detail: "valid".to_string(),
        },
        Ok(None) => DoctorCheck {
            name: "config_file".to_string(),
            category: CheckCategory::Core,
            outcome: CheckOutcome::Skip,
            detail: "absent (using defaults)".to_string(),
        },
        Err(error) => DoctorCheck {
            name: "config_file".to_string(),
            category: CheckCategory::Core,
            outcome: CheckOutcome::Fail,
            detail: error.to_string(),
        },
    }
}

fn check_providers_file(path: &std::path::Path) -> DoctorCheck {
    match std::fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DoctorCheck {
            name: "providers_file".to_string(),
            category: CheckCategory::Core,
            outcome: CheckOutcome::Skip,
            detail: "absent (using built-in providers)".to_string(),
        },
        Err(error) => DoctorCheck {
            name: "providers_file".to_string(),
            category: CheckCategory::Core,
            outcome: CheckOutcome::Fail,
            detail: format!("failed to read provider catalog: {error}"),
        },
        Ok(contents) => {
            match ironclaw_operator::llm_admin::llm_catalog::validate_reborn_provider_catalog_contents(&contents)
            {
                Ok(()) => DoctorCheck {
                    name: "providers_file".to_string(),
                    category: CheckCategory::Core,
                    outcome: CheckOutcome::Pass,
                    detail: "valid provider catalog".to_string(),
                },
                Err(error) => DoctorCheck {
                    name: "providers_file".to_string(),
                    category: CheckCategory::Core,
                    outcome: CheckOutcome::Fail,
                    detail: format!("invalid provider catalog: {error}"),
                },
            }
        }
    }
}

fn driver_check(name: &str, status: &RebornRuntimeComponentStatus) -> DoctorCheck {
    let (outcome, detail) = match status {
        RebornRuntimeComponentStatus::Initialized => {
            (CheckOutcome::Pass, "initialized".to_string())
        }
        RebornRuntimeComponentStatus::Failed(reason) => {
            (CheckOutcome::Fail, format!("unavailable: {reason}"))
        }
    };
    DoctorCheck {
        name: name.to_string(),
        category: CheckCategory::Drivers,
        outcome,
        detail,
    }
}

impl Renderable for DoctorDto {
    fn render_text_to(&self, w: &mut impl Write) -> std::io::Result<()> {
        writeln!(w, "IronClaw Reborn doctor")?;
        writeln!(w)?;
        let mut current_category: Option<CheckCategory> = None;
        for check in &self.checks {
            if current_category != Some(check.category) {
                current_category = Some(check.category);
                let label = match check.category {
                    CheckCategory::Core => "Core",
                    CheckCategory::Drivers => "Drivers",
                };
                writeln!(w, "  {label}")?;
            }
            let icon = match check.outcome {
                CheckOutcome::Pass => "\u{2714}",
                CheckOutcome::Fail => "\u{2718}",
                CheckOutcome::Skip => "-",
            };
            writeln!(
                w,
                "  {icon} {:<28} {}",
                terminal_safe_text(&check.name),
                terminal_safe_text(&check.detail)
            )?;
        }
        writeln!(w)?;
        writeln!(
            w,
            "{} passed, {} failed, {} skipped",
            self.summary.pass, self.summary.fail, self.summary.skip,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::RebornCliContext;

    #[test]
    fn doctor_dto_builds_with_defaults() {
        let (_tmp, context) = RebornCliContext::test_context();
        let dto = build_doctor_dto(&context);
        assert!(!dto.checks.is_empty());
        assert_eq!(
            dto.summary.pass + dto.summary.fail + dto.summary.skip,
            dto.checks.len()
        );
    }

    #[test]
    fn doctor_has_core_and_driver_checks() {
        let (_tmp, context) = RebornCliContext::test_context();
        let dto = build_doctor_dto(&context);
        assert!(dto.checks.iter().any(|c| c.category == CheckCategory::Core));
        assert!(
            dto.checks
                .iter()
                .any(|c| c.category == CheckCategory::Drivers)
        );
    }

    #[test]
    fn doctor_config_file_absent_is_skip() {
        let check = check_config_file(std::path::Path::new("/nonexistent/config.toml"));
        assert_eq!(check.outcome, CheckOutcome::Skip);
    }

    #[test]
    fn doctor_providers_file_absent_is_skip() {
        let check = check_providers_file(std::path::Path::new("/nonexistent/providers.json"));
        assert_eq!(check.outcome, CheckOutcome::Skip);
    }

    #[test]
    fn doctor_valid_config_file_is_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "api_version = \"ironclaw.runtime/v1\"\n").expect("write");
        let check = check_config_file(&path);
        assert_eq!(check.outcome, CheckOutcome::Pass);
    }

    #[test]
    fn doctor_invalid_config_file_is_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not valid { toml").expect("write");
        let check = check_config_file(&path);
        assert_eq!(check.outcome, CheckOutcome::Fail);
    }

    #[test]
    fn doctor_valid_providers_file_is_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("providers.json");
        std::fs::write(&path, "[]").expect("write");
        let check = check_providers_file(&path);
        assert_eq!(check.outcome, CheckOutcome::Pass);
    }

    #[test]
    fn doctor_invalid_providers_file_is_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("providers.json");
        std::fs::write(&path, "not json").expect("write");
        let check = check_providers_file(&path);
        assert_eq!(check.outcome, CheckOutcome::Fail);
    }

    #[test]
    fn doctor_well_formed_but_invalid_providers_catalog_is_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("providers.json");
        std::fs::write(&path, "{}").expect("write");
        let check = check_providers_file(&path);
        assert_eq!(check.outcome, CheckOutcome::Fail);
    }

    #[cfg(unix)]
    #[test]
    fn doctor_unreadable_providers_file_is_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("providers.json");
        std::fs::create_dir(&path).expect("create directory at providers path");
        let check = check_providers_file(&path);
        assert_eq!(check.outcome, CheckOutcome::Fail);
        assert!(check.detail.contains("failed to read"));
    }

    #[test]
    fn driver_check_failed_status_produces_fail_outcome() {
        let status = RebornRuntimeComponentStatus::Failed("timeout".to_string());
        let check = driver_check("test_driver", &status);
        assert_eq!(check.outcome, CheckOutcome::Fail);
        assert_eq!(check.category, CheckCategory::Drivers);
        assert_eq!(check.name, "test_driver");
        assert!(
            check.detail.contains("unavailable: timeout"),
            "detail should contain reason: {}",
            check.detail
        );
    }
}
