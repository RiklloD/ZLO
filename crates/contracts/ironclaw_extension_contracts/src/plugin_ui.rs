//! Plugin UI slot protocol — `reborn.plugin_ui.v1`.
//!
//! An extension ("plugin" to users) may declare UI slots in its manifest
//! (`[[ui.slots]]`). The host renders a slot by invoking one of the
//! extension's own tools through the normal mediated capability path; the
//! tool returns a **node tree as data**, never code. The backend may run on a
//! different machine from the client, so this vocabulary is the whole wire
//! contract between the two: the host validates every tree here before it
//! leaves the backend, and the client renders only these node kinds.
//!
//! Shared by the manifest registry (slot declarations), the product surface
//! (tree validation), and the WebUI/desktop transports (wire shape).

use std::fmt;

use ironclaw_host_api::ids::CapabilityId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::runtime::ExtensionAssetPath;

/// Version tag of the node-tree vocabulary below.
pub const PLUGIN_UI_SCHEMA_VERSION: &str = "reborn.plugin_ui.v1";
/// Maximum nesting depth of a tree (the root is depth 1).
pub const PLUGIN_UI_MAX_DEPTH: usize = 8;
/// Maximum number of nodes in one tree (list items and their `trailing`
/// nodes included).
pub const PLUGIN_UI_MAX_NODES: usize = 500;
/// Maximum JSON-encoded size of one tree.
pub const PLUGIN_UI_MAX_ENCODED_BYTES: usize = 64 * 1024;
/// Maximum JSON-encoded size of an event payload sent back to a render tool.
pub const PLUGIN_UI_MAX_EVENT_PAYLOAD_BYTES: usize = 16 * 1024;
/// Maximum number of `[[ui.slots]]` one manifest may declare.
pub const PLUGIN_UI_MAX_SLOTS: usize = 16;
const MAX_ACTION_BYTES: usize = 64;
const MAX_SLOT_ID_BYTES: usize = 64;
const MAX_TITLE_CHARS: usize = 80;
const MAX_GAP: f64 = 64.0;

/// Where in the client a slot renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PluginUiSlotKind {
    /// A tab in the side panel.
    #[serde(rename = "panel.tab")]
    PanelTab,
    /// A small badge on each session row in the sidebar. Must render a single
    /// `badge` or `text` node.
    #[serde(rename = "sidebar.session_row.badge")]
    SidebarSessionRowBadge,
    /// A page under Settings.
    #[serde(rename = "settings.page")]
    SettingsPage,
}

impl PluginUiSlotKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PanelTab => "panel.tab",
            Self::SidebarSessionRowBadge => "sidebar.session_row.badge",
            Self::SettingsPage => "settings.page",
        }
    }
}

/// A slot's id, unique within one extension: `[a-z0-9][a-z0-9_-]*`, at most
/// 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct PluginUiSlotId(String);

impl PluginUiSlotId {
    fn validate(value: &str) -> Result<(), PluginUiSlotError> {
        let mut chars = value.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let rest_ok =
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !first_ok || !rest_ok || value.len() > MAX_SLOT_ID_BYTES {
            return Err(PluginUiSlotError::InvalidSlotId {
                id: value.to_string(),
            });
        }
        Ok(())
    }

    pub fn new(value: impl Into<String>) -> Result<Self, PluginUiSlotError> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PluginUiSlotId {
    type Error = PluginUiSlotError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl fmt::Display for PluginUiSlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One `[[ui.slots]]` declaration, as carried in the resolved manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginUiSlotDescriptor {
    pub id: PluginUiSlotId,
    pub slot: PluginUiSlotKind,
    pub title: String,
    /// Package-relative asset path of the slot's icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The extension's own tool that produces the tree.
    pub render: CapabilityId,
}

impl PluginUiSlotDescriptor {
    /// Shape checks that need only the declaration itself. Cross-checks
    /// against the manifest's tools live with the manifest parser.
    pub fn validate(&self) -> Result<(), PluginUiSlotError> {
        let title = self.title.trim();
        if title.is_empty()
            || self.title.chars().count() > MAX_TITLE_CHARS
            || self.title.chars().any(char::is_control)
        {
            return Err(PluginUiSlotError::InvalidTitle {
                slot_id: self.id.to_string(),
            });
        }
        if let Some(icon) = &self.icon {
            ExtensionAssetPath::new(icon.clone()).map_err(|error| {
                PluginUiSlotError::InvalidIcon {
                    slot_id: self.id.to_string(),
                    reason: error.reason,
                }
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PluginUiSlotError {
    #[error("ui slot id `{id}` must match [a-z0-9][a-z0-9_-]* and be at most 64 bytes")]
    InvalidSlotId { id: String },
    #[error("ui slot `{slot_id}` title must be 1-80 printable characters")]
    InvalidTitle { slot_id: String },
    #[error("ui slot `{slot_id}` icon is not a package asset path: {reason}")]
    InvalidIcon { slot_id: String, reason: String },
}

/// An action name a node sends back to the render tool when the user
/// activates it: `[A-Za-z0-9_.:-]+`, at most 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct PluginUiAction(String);

impl PluginUiAction {
    fn validate(value: &str) -> Result<(), String> {
        if value.is_empty()
            || value.len() > MAX_ACTION_BYTES
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
        {
            return Err(format!(
                "action `{value}` must be 1-64 characters of letters, digits, `_`, `.`, `:` or `-`"
            ));
        }
        Ok(())
    }

    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PluginUiAction {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiTone {
    Neutral,
    Success,
    Warning,
    Danger,
    Accent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiTextStyle {
    Title,
    Body,
    Caption,
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiButtonStyle {
    Primary,
    Secondary,
    Ghost,
}

/// One list row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginUiListItem {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing: Option<Box<PluginUiNode>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<PluginUiAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

/// A validated `reborn.plugin_ui.v1` node. Construct from untrusted JSON only
/// through [`PluginUiNode::parse`]; serialization reproduces the wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginUiNode {
    Col {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gap: Option<f64>,
        children: Vec<PluginUiNode>,
    },
    Row {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gap: Option<f64>,
        children: Vec<PluginUiNode>,
    },
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<PluginUiTextStyle>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<PluginUiTone>,
    },
    Badge {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<PluginUiTone>,
    },
    Button {
        label: String,
        action: PluginUiAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<PluginUiButtonStyle>,
    },
    List {
        items: Vec<PluginUiListItem>,
    },
    Markdown {
        source: String,
    },
    Divider,
}

/// Why a tree was rejected. `Display` is a plain, user-presentable reason; it
/// never echoes tree content beyond a field or type name.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PluginUiTreeError {
    #[error("the plugin returned {bytes} bytes of UI, more than the {max}-byte limit")]
    TooLarge { bytes: usize, max: usize },
    #[error("the plugin's UI nests deeper than {max} levels")]
    TooDeep { max: usize },
    #[error("the plugin's UI has more than {max} elements")]
    TooManyNodes { max: usize },
    #[error("the plugin's UI is invalid at {path}: {reason}")]
    Invalid { path: String, reason: String },
    #[error("a {slot} slot must render a single badge or text element")]
    WrongRootForSlot { slot: &'static str },
}

impl PluginUiNode {
    /// Validate untrusted JSON (a render tool's output) as one tree.
    pub fn parse(value: &Value) -> Result<Self, PluginUiTreeError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| invalid("$", error.to_string()))?
            .len();
        if bytes > PLUGIN_UI_MAX_ENCODED_BYTES {
            return Err(PluginUiTreeError::TooLarge {
                bytes,
                max: PLUGIN_UI_MAX_ENCODED_BYTES,
            });
        }
        let mut count = 0;
        parse_node(value, "$", 1, &mut count)
    }

    /// [`Self::parse`] plus the per-slot root constraint.
    pub fn parse_for_slot(
        value: &Value,
        slot: PluginUiSlotKind,
    ) -> Result<Self, PluginUiTreeError> {
        let node = Self::parse(value)?;
        if slot == PluginUiSlotKind::SidebarSessionRowBadge
            && !matches!(node, Self::Badge { .. } | Self::Text { .. })
        {
            return Err(PluginUiTreeError::WrongRootForSlot {
                slot: slot.as_str(),
            });
        }
        Ok(node)
    }
}

fn invalid(path: &str, reason: impl Into<String>) -> PluginUiTreeError {
    PluginUiTreeError::Invalid {
        path: path.to_string(),
        reason: reason.into(),
    }
}

struct Fields<'a> {
    map: &'a Map<String, Value>,
    path: &'a str,
}

impl<'a> Fields<'a> {
    fn only(&self, allowed: &[&str]) -> Result<(), PluginUiTreeError> {
        for key in self.map.keys() {
            if key != "type" && !allowed.contains(&key.as_str()) {
                return Err(invalid(self.path, format!("unknown field `{key}`")));
            }
        }
        Ok(())
    }

    fn present(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key).filter(|value| !value.is_null())
    }

    fn string(&self, key: &str) -> Result<String, PluginUiTreeError> {
        self.opt_string(key)?
            .ok_or_else(|| invalid(self.path, format!("missing string field `{key}`")))
    }

    fn opt_string(&self, key: &str) -> Result<Option<String>, PluginUiTreeError> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(invalid(self.path, format!("`{key}` must be a string"))),
        }
    }

    fn opt_enum<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, PluginUiTreeError> {
        self.present(key)
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|_| invalid(self.path, format!("`{key}` has an unsupported value")))
            })
            .transpose()
    }

    fn opt_gap(&self) -> Result<Option<f64>, PluginUiTreeError> {
        match self.present("gap") {
            None => Ok(None),
            Some(value) => value
                .as_f64()
                .filter(|gap| gap.is_finite() && (0.0..=MAX_GAP).contains(gap))
                .map(Some)
                .ok_or_else(|| invalid(self.path, "`gap` must be a number from 0 to 64")),
        }
    }

    fn opt_action(&self) -> Result<Option<PluginUiAction>, PluginUiTreeError> {
        self.opt_string("action")?
            .map(|action| PluginUiAction::new(action).map_err(|reason| invalid(self.path, reason)))
            .transpose()
    }

    fn array(&self, key: &str) -> Result<&'a Vec<Value>, PluginUiTreeError> {
        match self.present(key) {
            Some(Value::Array(items)) => Ok(items),
            _ => Err(invalid(self.path, format!("missing array field `{key}`"))),
        }
    }
}

fn parse_node(
    value: &Value,
    path: &str,
    depth: usize,
    count: &mut usize,
) -> Result<PluginUiNode, PluginUiTreeError> {
    if depth > PLUGIN_UI_MAX_DEPTH {
        return Err(PluginUiTreeError::TooDeep {
            max: PLUGIN_UI_MAX_DEPTH,
        });
    }
    *count += 1;
    if *count > PLUGIN_UI_MAX_NODES {
        return Err(PluginUiTreeError::TooManyNodes {
            max: PLUGIN_UI_MAX_NODES,
        });
    }
    let Value::Object(map) = value else {
        return Err(invalid(path, "an element must be an object"));
    };
    let fields = Fields { map, path };
    let kind = match map.get("type") {
        Some(Value::String(kind)) => kind.as_str(),
        _ => return Err(invalid(path, "missing string field `type`")),
    };
    let children = |count: &mut usize| -> Result<Vec<PluginUiNode>, PluginUiTreeError> {
        fields
            .array("children")?
            .iter()
            .enumerate()
            .map(|(index, child)| {
                parse_node(
                    child,
                    &format!("{path}.children[{index}]"),
                    depth + 1,
                    count,
                )
            })
            .collect()
    };
    match kind {
        "col" | "row" => {
            fields.only(&["gap", "children"])?;
            let gap = fields.opt_gap()?;
            let children = children(count)?;
            Ok(if kind == "col" {
                PluginUiNode::Col { gap, children }
            } else {
                PluginUiNode::Row { gap, children }
            })
        }
        "text" => {
            fields.only(&["text", "style", "tone"])?;
            Ok(PluginUiNode::Text {
                text: fields.string("text")?,
                style: fields.opt_enum("style")?,
                tone: fields.opt_enum("tone")?,
            })
        }
        "badge" => {
            fields.only(&["text", "tone"])?;
            Ok(PluginUiNode::Badge {
                text: fields.string("text")?,
                tone: fields.opt_enum("tone")?,
            })
        }
        "button" => {
            fields.only(&["label", "action", "payload", "style"])?;
            Ok(PluginUiNode::Button {
                label: fields.string("label")?,
                action: fields
                    .opt_action()?
                    .ok_or_else(|| invalid(path, "missing string field `action`"))?,
                payload: fields.present("payload").cloned(),
                style: fields.opt_enum("style")?,
            })
        }
        "list" => {
            fields.only(&["items"])?;
            let items = fields
                .array("items")?
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    parse_list_item(item, &format!("{path}.items[{index}]"), depth, count)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(PluginUiNode::List { items })
        }
        "markdown" => {
            fields.only(&["source"])?;
            Ok(PluginUiNode::Markdown {
                source: fields.string("source")?,
            })
        }
        "divider" => {
            fields.only(&[])?;
            Ok(PluginUiNode::Divider)
        }
        other => Err(invalid(path, format!("unknown element type `{other}`"))),
    }
}

fn parse_list_item(
    value: &Value,
    path: &str,
    depth: usize,
    count: &mut usize,
) -> Result<PluginUiListItem, PluginUiTreeError> {
    let Value::Object(map) = value else {
        return Err(invalid(path, "a list item must be an object"));
    };
    let fields = Fields { map, path };
    if map.contains_key("type") {
        return Err(invalid(path, "unknown field `type`"));
    }
    fields.only(&["title", "subtitle", "trailing", "action", "payload"])?;
    let trailing = fields
        .present("trailing")
        .map(|node| parse_node(node, &format!("{path}.trailing"), depth + 1, count))
        .transpose()?
        .map(Box::new);
    Ok(PluginUiListItem {
        title: fields.string("title")?,
        subtitle: fields.opt_string("subtitle")?,
        trailing,
        action: fields.opt_action()?,
        payload: fields.present("payload").cloned(),
    })
}
