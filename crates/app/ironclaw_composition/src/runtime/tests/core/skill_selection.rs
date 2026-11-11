//! Skill selector configuration and the `IRONCLAW_REBORN_SKILL_INJECTION` /
//! `_ACTIVATION` env parsing.

/// Wiring guard: the `regex_skill_activation_enabled` flag from
/// [`RebornRuntimeInput`] must reach
/// [`SkillActivationSelectorConfig::regex_activation_enabled`]
/// unchanged, not get clobbered by a stray
/// `..SkillActivationSelectorConfig::default()` spread or by the
/// helper accidentally taking `Default::default()`. Covers the
/// composition-level path that
/// [`filesystem_skill_context_source`] depends on.
#[test]
fn standalone_selector_config_propagates_regex_activation_disabled() {
    let cfg = crate::runtime::skill_context::skill_activation_selector_config(
        false,
        ironclaw_loop_host::SkillInjectionMode::Listing,
        crate::runtime::skill_context::DEFAULT_SKILL_ACTIVATION,
        // Execution available: these cases assert selector config, not the no-process note.
        true,
    );
    assert!(
        !cfg.regex_activation_enabled,
        "regex_skill_activation_enabled=false must propagate into SkillActivationSelectorConfig"
    );
    // Selection is the MODEL's decision, so this must be `ExplicitOnly`.
    //
    // This assertion previously locked `ExplicitAndCriteria`, on the reasoning that
    // criteria selection is what closes the learn→reuse loop -- a learned skill
    // auto-activating on a keyword match rather than only on an explicit `$name`.
    // That reasoning does not survive contact with the corpus: **0 of 30
    // agent-authored skills declare an `activation:` block**, so the scorer could
    // never select a learned skill and the loop it was protecting did not exist.
    // What actually closes the loop is the skill appearing in the listing, which is
    // why this PR makes the listing complete.
    //
    // Kept as an assertion rather than deleted, with the polarity flipped: a revert
    // to `ExplicitAndCriteria` reinstates host-side keyword matching on the model's
    // behalf, which is the #5417 class.
    assert!(matches!(
        cfg.selection_mode,
        ironclaw_loop_host::SkillActivationSelectionMode::ExplicitOnly
    ));
}

#[test]
fn standalone_selector_config_propagates_regex_activation_enabled() {
    let cfg = crate::runtime::skill_context::skill_activation_selector_config(
        true,
        ironclaw_loop_host::SkillInjectionMode::Listing,
        crate::runtime::skill_context::DEFAULT_SKILL_ACTIVATION,
        // Execution available: these cases assert selector config, not the no-process note.
        true,
    );
    assert!(
        cfg.regex_activation_enabled,
        "regex_skill_activation_enabled=true must propagate into SkillActivationSelectorConfig"
    );
}

/// Every branch of the `IRONCLAW_REBORN_SKILL_INJECTION` decision, including the unset one --
/// previously unreachable, since unsetting the key in-process races the other tests here.
#[test]
fn skill_injection_mode_resolves_every_env_branch() {
    use ironclaw_loop_host::SkillInjectionMode;

    assert!(
        matches!(
            crate::runtime::skill_context::skill_injection_mode_from_env_value(Err(std::env::VarError::NotPresent)),
            Ok(mode) if mode == crate::runtime::skill_context::DEFAULT_SKILL_INJECTION_MODE
        ),
        "an unset key must resolve to the product default, not an error"
    );
    assert!(matches!(
        crate::runtime::skill_context::skill_injection_mode_from_env_value(Ok("full".to_string())),
        Ok(SkillInjectionMode::Full)
    ));
    assert!(
        matches!(
            crate::runtime::skill_context::skill_injection_mode_from_env_value(Ok(
                "  Listing  ".to_string()
            )),
            Ok(SkillInjectionMode::Listing)
        ),
        "values are trimmed and case-insensitive"
    );
    assert!(
        matches!(
            crate::runtime::skill_context::skill_injection_mode_from_env_value(Ok(String::new())),
            Ok(SkillInjectionMode::Listing)
        ),
        "an empty value is the same as asking for the listing, not an error"
    );
    assert!(
        crate::runtime::skill_context::skill_injection_mode_from_env_value(
            Ok("bodies".to_string())
        )
        .is_err(),
        "an unrecognized mode must fail loudly rather than silently pick one"
    );
    assert!(
        crate::runtime::skill_context::skill_injection_mode_from_env_value(Err(
            std::env::VarError::NotUnicode(std::ffi::OsString::from("full"))
        ))
        .is_err(),
        "an unreadable value must not be mistaken for an unset key"
    );
}

#[test]
fn standalone_selector_config_uses_large_skill_context_budget() {
    let cfg = crate::runtime::skill_context::skill_activation_selector_config(
        true,
        ironclaw_loop_host::SkillInjectionMode::Listing,
        crate::runtime::skill_context::DEFAULT_SKILL_ACTIVATION,
        // Execution available: these cases assert selector config, not the no-process note.
        true,
    );
    assert_eq!(
        cfg.max_context_tokens, 6000,
        "standalone Reborn skill activation should match the legacy 6000-token skill budget"
    );
}

/// Wiring guard for the `IRONCLAW_REBORN_SKILL_INJECTION` env switch: the
/// parsed injection mode must reach
/// [`SkillActivationSelectorConfig::injection_mode`] unchanged (not get
/// clobbered by the `..default()` spread). The parser still maps an explicit
/// empty value to `listing`; the ENV-ABSENT default is `full` (see
/// `DEFAULT_SKILL_INJECTION_MODE`).
#[test]
fn standalone_selector_config_propagates_injection_mode() {
    for mode in [
        ironclaw_loop_host::SkillInjectionMode::Listing,
        ironclaw_loop_host::SkillInjectionMode::Full,
    ] {
        let cfg = crate::runtime::skill_context::skill_activation_selector_config(
            true,
            mode,
            crate::runtime::skill_context::DEFAULT_SKILL_ACTIVATION,
            true,
        );
        assert_eq!(cfg.injection_mode, mode);
    }
}

/// Skill selection on the Reborn path must be the model's decision, not a host-side
/// keyword guess.
///
/// This exists because changing the default in `activation.rs` was, on its own, a
/// no-op here: `skill_activation_selector_config` used to pin
/// `ExplicitAndCriteria` at the call site, so no real Reborn user could ever see
/// `ExplicitOnly` however the default was written. The bug was invisible to every
/// test in `ironclaw_loop_host`, because those construct their own
/// config — only a test at the composition layer, on the value this function
/// actually returns, can catch it.
///
/// If a later change re-pins the mode here, this fails rather than silently
/// reinstating host-side matching.
#[test]
fn reborn_skill_selection_is_model_decided() {
    let cfg = crate::runtime::skill_context::skill_activation_selector_config(
        true,
        ironclaw_loop_host::SkillInjectionMode::Listing,
        crate::runtime::skill_context::DEFAULT_SKILL_ACTIVATION,
        // Execution available: these cases assert selector config, not the no-process note.
        true,
    );
    assert_eq!(
        cfg.selection_mode,
        ironclaw_loop_host::SkillActivationSelectionMode::ExplicitOnly,
        "Reborn must let the model choose the skill from the listing; pinning \
         ExplicitAndCriteria here makes the host keyword-match on the model's behalf"
    );
}

/// Guards the injection default.
///
/// Currently `Listing`. The measurement argues for `Full` (79.8% -> 85.6% on the
/// 31-task SkillsBench subset, nearai/benchmarks#287, because the model reads the
/// one-line listing and then opens a skill in 0 of 30 runs), but three local-dev
/// tests HANG under `Full` — they drive a mock that expects the listing candidate —
/// so the flip is a maintainer call and `Full` ships as an opt-in switch.
///
/// If you flip `DEFAULT_SKILL_INJECTION_MODE`, update those three tests too:
/// `local_dev_skill_activate_tool_loads_selected_skill_context`,
/// `local_dev_webui_bundle_records_selectable_filesystem_skill_context`,
/// `local_dev_runtime_wires_filesystem_skills_by_default_to_model_calls`.
#[test]
fn skill_injection_mode_default_is_documented_and_guarded() {
    assert_eq!(
        crate::runtime::skill_context::DEFAULT_SKILL_INJECTION_MODE,
        ironclaw_loop_host::SkillInjectionMode::Listing,
        "flipping this default changes three local-dev expectations; see the doc comment"
    );
    // and the opt-in path must still resolve
    assert_eq!(
        crate::runtime::skill_context::skill_injection_mode_from("full").expect("full parses"),
        ironclaw_loop_host::SkillInjectionMode::Full
    );
}

#[test]
fn skill_injection_mode_parses_listing_full_and_defaults() {
    use ironclaw_loop_host::SkillInjectionMode;
    for (value, expected) in [
        ("", SkillInjectionMode::Listing),
        ("listing", SkillInjectionMode::Listing),
        (" Listing ", SkillInjectionMode::Listing),
        ("full", SkillInjectionMode::Full),
        ("FULL", SkillInjectionMode::Full),
    ] {
        assert_eq!(
            crate::runtime::skill_context::skill_injection_mode_from(value)
                .expect("valid mode parses"),
            expected,
            "value {value:?}"
        );
    }
    assert!(
        crate::runtime::skill_context::skill_injection_mode_from("bodies").is_err(),
        "unknown values must fail loud, not silently pick a mode"
    );
}
