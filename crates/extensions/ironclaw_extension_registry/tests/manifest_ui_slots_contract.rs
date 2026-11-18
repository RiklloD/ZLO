//! `[[ui.slots]]` manifest contract: slots parse into the resolved record,
//! survive the persisted wire round trip, and a render tool that is not a
//! side-effect-free, product-origin-allowed tool of the same manifest is
//! rejected at parse time.

#![deny(clippy::let_underscore_must_use)]
use std::sync::Arc;

use ironclaw_extension_contracts::plugin_ui::PluginUiSlotKind;
use ironclaw_extension_registry::{
    CapabilityProviderHostApiContract, ExtensionManifestRecord, HostApiContractRegistry,
    ManifestSource, ResolvedExtensionManifest,
};
use ironclaw_host_api::host_port::HostPortCatalog;

const DEMO_MANIFEST: &str =
    include_str!("../../../../tests/fixtures/extensions/plugin-ui-demo/manifest.toml");

fn parse(toml: &str) -> Result<ExtensionManifestRecord, String> {
    let mut contracts = HostApiContractRegistry::new();
    contracts
        .register(Arc::new(
            CapabilityProviderHostApiContract::new().expect("contract"),
        ))
        .expect("register contract");
    ExtensionManifestRecord::from_toml(
        toml,
        ManifestSource::HostBundled,
        &HostPortCatalog::empty(),
        None,
        &contracts,
        None,
    )
    .map_err(|error| error.to_string())
}

/// A minimal plugin whose render tool and slot table are spliced in.
fn plugin(tool_extra: &str, slots: &str) -> String {
    format!(
        r#"
schema_version = "reborn.extension_manifest.v3"
id = "uiprobe"
name = "UI Probe"
version = "0.1.0"
description = "probe"
trust = "first_party_requested"

[runtime]
kind = "first_party"
service = "uiprobe/v1"

[[tools]]
id = "uiprobe.render"
description = "render"
default_permission = "allow"
input_schema_ref = "schemas/render.json"
{tool_extra}

{slots}
"#
    )
}

const ONE_SLOT: &str = r#"
[[ui.slots]]
id = "main"
slot = "panel.tab"
title = "Main"
render = "uiprobe.render"
"#;

const PRODUCT_ALLOWED: &str = r#"origin_gate_matrix = { loop_run = "forbidden", product = "consent_sufficient", automation = "forbidden" }"#;

#[test]
fn fixture_slots_resolve_and_survive_the_persisted_wire_round_trip() {
    let record = parse(DEMO_MANIFEST).expect("fixture parses");
    let slots = &record.resolved().ui_slots;
    let kinds: Vec<_> = slots
        .iter()
        .map(|slot| (slot.id.as_str(), slot.slot))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("panel", PluginUiSlotKind::PanelTab),
            ("badge", PluginUiSlotKind::SidebarSessionRowBadge),
            ("settings", PluginUiSlotKind::SettingsPage),
            ("broken", PluginUiSlotKind::PanelTab),
            ("oversize", PluginUiSlotKind::PanelTab),
        ]
    );
    assert_eq!(slots[0].icon.as_deref(), Some("icons/demo.svg"));
    assert_eq!(slots[0].render.as_str(), "plugin-ui-demo.render");

    let wire = serde_json::to_value(record.resolved()).expect("serializes");
    let back: ResolvedExtensionManifest = serde_json::from_value(wire).expect("rehydrates");
    assert_eq!(&back, record.resolved());
}

#[test]
fn a_manifest_without_ui_slots_keeps_its_persisted_shape() {
    let record = parse(&plugin(PRODUCT_ALLOWED, "")).expect("parses");
    assert!(record.resolved().ui_slots.is_empty());
    let wire = serde_json::to_value(record.resolved()).expect("serializes");
    assert!(
        wire.get("ui_slots").is_none(),
        "an empty slot list must not change the digested wire shape"
    );
}

#[test]
fn render_tool_must_be_allowed_for_product_origin() {
    // No matrix at all: product defaults to forbidden.
    let error = parse(&plugin("", ONE_SLOT)).expect_err("forbidden by default");
    assert!(error.contains("must allow product origin"), "{error}");

    let gated = r#"origin_gate_matrix = { loop_run = "forbidden", product = "ask_always", automation = "forbidden" }"#;
    let error = parse(&plugin(gated, ONE_SLOT)).expect_err("a gated render tool");
    assert!(error.contains("must allow product origin"), "{error}");
}

#[test]
fn render_tool_must_be_side_effect_free() {
    for effects in [
        r#"effects = ["network"]"#,
        r#"effects = ["external_write"]"#,
        r#"effects = ["write_filesystem"]"#,
        r#"effects = ["use_secret"]"#,
    ] {
        let tool = format!("{PRODUCT_ALLOWED}\n{effects}");
        let error = parse(&plugin(&tool, ONE_SLOT)).expect_err(effects);
        assert!(
            error.contains("must be side-effect-free"),
            "{effects}: {error}"
        );
    }
    let read_only = format!("{PRODUCT_ALLOWED}\neffects = [\"read_filesystem\"]");
    parse(&plugin(&read_only, ONE_SLOT)).expect("a read-only render tool is fine");
}

#[test]
fn render_must_name_one_of_the_manifests_own_tools() {
    let foreign = ONE_SLOT.replace("uiprobe.render", "otherext.render");
    let error = parse(&plugin(PRODUCT_ALLOWED, &foreign)).expect_err("foreign tool");
    assert!(
        error.contains("is not one of this extension's [[tools]]"),
        "{error}"
    );
}

#[test]
fn slot_table_shape_is_fail_closed() {
    let duplicate = format!("{ONE_SLOT}\n{ONE_SLOT}");
    let error = parse(&plugin(PRODUCT_ALLOWED, &duplicate)).expect_err("duplicate id");
    assert!(error.contains("declared twice"), "{error}");

    let unknown_kind = ONE_SLOT.replace("panel.tab", "toolbar.button");
    assert!(parse(&plugin(PRODUCT_ALLOWED, &unknown_kind)).is_err());

    let unknown_field = format!("{ONE_SLOT}script = \"alert(1)\"\n");
    assert!(parse(&plugin(PRODUCT_ALLOWED, &unknown_field)).is_err());

    let escaping_icon = format!("{ONE_SLOT}icon = \"../../secrets\"\n");
    assert!(parse(&plugin(PRODUCT_ALLOWED, &escaping_icon)).is_err());

    let unknown_ui_key = "[ui]\nwidgets = []\n";
    assert!(parse(&plugin(PRODUCT_ALLOWED, unknown_ui_key)).is_err());
}
