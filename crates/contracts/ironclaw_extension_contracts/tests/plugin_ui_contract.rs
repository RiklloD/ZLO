//! `reborn.plugin_ui.v1` node-tree contract: every documented node kind
//! round-trips to the same wire JSON, and every documented limit rejects.

#![deny(clippy::let_underscore_must_use)]

use ironclaw_extension_contracts::plugin_ui::{
    PLUGIN_UI_MAX_DEPTH, PLUGIN_UI_MAX_NODES, PluginUiNode, PluginUiSlotDescriptor,
    PluginUiSlotKind, PluginUiTreeError,
};
use serde_json::{Value, json};

fn every_node_kind() -> Value {
    json!({
        "type": "col",
        "gap": 8,
        "children": [
            { "type": "text", "text": "Title", "style": "title", "tone": "accent" },
            { "type": "row", "children": [
                { "type": "badge", "text": "3 open", "tone": "warning" },
                { "type": "button", "label": "Refresh", "action": "refresh",
                  "payload": { "force": true }, "style": "primary" }
            ]},
            { "type": "list", "items": [
                { "title": "One", "subtitle": "first", "action": "open", "payload": 1,
                  "trailing": { "type": "badge", "text": "new" } },
                { "title": "Two" }
            ]},
            { "type": "markdown", "source": "**bold**" },
            { "type": "divider" }
        ]
    })
}

#[test]
fn every_node_kind_parses_and_reserializes_to_the_same_wire_shape() {
    let wire = every_node_kind();
    let node = PluginUiNode::parse(&wire).expect("valid tree");
    let mut reserialized = serde_json::to_value(&node).expect("serializes");
    // `gap: 8` comes back as a float; compare numerically.
    assert_eq!(reserialized["gap"].as_f64(), Some(8.0));
    reserialized["gap"] = json!(8);
    assert_eq!(reserialized, wire);
}

#[test]
fn unknown_type_and_unknown_field_are_rejected_with_a_path() {
    let unknown_type = json!({ "type": "col", "children": [{ "type": "video", "src": "x" }] });
    let error = PluginUiNode::parse(&unknown_type).expect_err("unknown type");
    assert_eq!(
        error.to_string(),
        "the plugin's UI is invalid at $.children[0]: unknown element type `video`"
    );

    let unknown_field = json!({ "type": "text", "text": "hi", "onclick": "alert(1)" });
    let error = PluginUiNode::parse(&unknown_field).expect_err("unknown field");
    assert!(
        matches!(error, PluginUiTreeError::Invalid { ref reason, .. } if reason == "unknown field `onclick`")
    );

    let item_field = json!({ "type": "list", "items": [{ "title": "a", "html": "<b>" }] });
    assert!(PluginUiNode::parse(&item_field).is_err());
}

#[test]
fn bad_enum_values_missing_fields_and_bad_actions_are_rejected() {
    for bad in [
        json!({ "type": "badge", "text": "x", "tone": "purple" }),
        json!({ "type": "text", "text": "x", "style": "huge" }),
        json!({ "type": "text" }),
        json!({ "type": "button", "label": "x" }),
        json!({ "type": "button", "label": "x", "action": "has space" }),
        json!({ "type": "col", "gap": 1000, "children": [] }),
        json!({ "type": "col" }),
        json!({ "text": "no type" }),
        json!("just a string"),
    ] {
        assert!(PluginUiNode::parse(&bad).is_err(), "accepted {bad}");
    }
}

#[test]
fn depth_node_count_and_size_limits_are_enforced() {
    let mut deep = json!({ "type": "divider" });
    for _ in 0..PLUGIN_UI_MAX_DEPTH {
        deep = json!({ "type": "col", "children": [deep] });
    }
    assert_eq!(
        PluginUiNode::parse(&deep),
        Err(PluginUiTreeError::TooDeep {
            max: PLUGIN_UI_MAX_DEPTH
        })
    );

    // List items spend the same budget as elements: a list cannot carry
    // thousands of rows under the node limit.
    let rows = json!({
        "type": "list",
        "items": vec![json!({ "title": "r" }); PLUGIN_UI_MAX_NODES],
    });
    assert_eq!(
        PluginUiNode::parse(&rows),
        Err(PluginUiTreeError::TooManyNodes {
            max: PLUGIN_UI_MAX_NODES
        })
    );
    let rows_at_limit = json!({
        "type": "list",
        "items": vec![json!({ "title": "r" }); PLUGIN_UI_MAX_NODES - 1],
    });
    assert!(PluginUiNode::parse(&rows_at_limit).is_ok());

    let many = json!({
        "type": "col",
        "children": vec![json!({ "type": "divider" }); PLUGIN_UI_MAX_NODES],
    });
    assert_eq!(
        PluginUiNode::parse(&many),
        Err(PluginUiTreeError::TooManyNodes {
            max: PLUGIN_UI_MAX_NODES
        })
    );

    let huge = json!({ "type": "markdown", "source": "x".repeat(70 * 1024) });
    assert!(matches!(
        PluginUiNode::parse(&huge),
        Err(PluginUiTreeError::TooLarge { .. })
    ));
}

#[test]
fn session_row_badge_slot_accepts_only_a_badge_or_text_root() {
    let badge = json!({ "type": "badge", "text": "2" });
    let text = json!({ "type": "text", "text": "idle" });
    let col = json!({ "type": "col", "children": [badge.clone()] });
    let slot = PluginUiSlotKind::SidebarSessionRowBadge;
    assert!(PluginUiNode::parse_for_slot(&badge, slot).is_ok());
    assert!(PluginUiNode::parse_for_slot(&text, slot).is_ok());
    assert_eq!(
        PluginUiNode::parse_for_slot(&col, slot),
        Err(PluginUiTreeError::WrongRootForSlot {
            slot: "sidebar.session_row.badge"
        })
    );
    assert!(PluginUiNode::parse_for_slot(&col, PluginUiSlotKind::PanelTab).is_ok());
}

#[test]
fn slot_descriptor_wire_shape_and_validation() {
    let descriptor: PluginUiSlotDescriptor = serde_json::from_value(json!({
        "id": "status",
        "slot": "panel.tab",
        "title": "Status",
        "icon": "assets/status.svg",
        "render": "demo.render_status",
    }))
    .expect("descriptor parses");
    descriptor.validate().expect("descriptor is valid");

    for bad in [
        json!({ "id": "Bad Id", "slot": "panel.tab", "title": "t", "render": "a.b" }),
        json!({ "id": "x", "slot": "toolbar", "title": "t", "render": "a.b" }),
        json!({ "id": "x", "slot": "panel.tab", "title": "t", "render": "a.b", "extra": 1 }),
    ] {
        assert!(
            serde_json::from_value::<PluginUiSlotDescriptor>(bad.clone()).is_err(),
            "accepted {bad}"
        );
    }
    let escaping_icon: PluginUiSlotDescriptor = serde_json::from_value(json!({
        "id": "x", "slot": "settings.page", "title": "t", "icon": "../etc/passwd", "render": "a.b"
    }))
    .expect("shape parses");
    assert!(escaping_icon.validate().is_err());
    let blank_title: PluginUiSlotDescriptor = serde_json::from_value(json!({
        "id": "x", "slot": "settings.page", "title": "  ", "render": "a.b"
    }))
    .expect("shape parses");
    assert!(blank_title.validate().is_err());
}
