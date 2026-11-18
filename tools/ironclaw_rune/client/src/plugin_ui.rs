//! The plugin UI node tree, `reborn.plugin_ui.v1`, decoded from untrusted
//! JSON.
//!
//! A plugin describes its view as data; the app draws it with its own
//! components and theme. The tree is treated as hostile: it is decoded by
//! hand from a [`Value`] (whose parser already bounds nesting) under
//! [`MAX_DEPTH`] and [`MAX_NODES`], strings are capped, an unknown node type
//! becomes [`Node::Unknown`] (drawn as nothing), and a known node with a
//! missing or mistyped field is unknown too. Plugins get no styling control
//! beyond `style` and `tone`.

use serde_json::Value;

/// Deepest nesting accepted, the root being depth 1.
pub const MAX_DEPTH: usize = 8;
/// Most nodes accepted in one tree (list items count, as do their
/// trailing nodes).
pub const MAX_NODES: usize = 500;
/// Longest string kept from any field; the rest is cut.
const MAX_TEXT: usize = 4_000;
/// Longest markdown source kept.
const MAX_MARKDOWN: usize = 32_000;
/// Widest gap accepted between children, in points.
const MAX_GAP: f32 = 32.;
/// Largest action payload kept, encoded.
const MAX_PAYLOAD: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    #[default]
    Neutral,
    Success,
    Warning,
    Danger,
    Accent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextStyle {
    Title,
    #[default]
    Body,
    Caption,
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonStyle {
    Primary,
    #[default]
    Secondary,
    Ghost,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub title: String,
    pub subtitle: Option<String>,
    pub trailing: Option<Box<Node>>,
    pub action: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Col {
        gap: Option<f32>,
        children: Vec<Node>,
    },
    Row {
        gap: Option<f32>,
        children: Vec<Node>,
    },
    Text {
        text: String,
        style: TextStyle,
        /// `None` keeps the style's own ink.
        tone: Option<Tone>,
    },
    Badge {
        text: String,
        tone: Tone,
    },
    Button {
        label: String,
        action: String,
        payload: Value,
        style: ButtonStyle,
    },
    List {
        items: Vec<ListItem>,
    },
    Markdown {
        source: String,
    },
    Divider,
    /// A type this app does not draw, or a malformed node.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("This view nests too deeply to show.")]
    TooDeep,
    #[error("This view is too large to show.")]
    TooLarge,
}

/// Decode a tree, or refuse it whole when it breaks a limit.
pub fn decode(value: &Value) -> Result<Node, TreeError> {
    let mut count = 0;
    node(value, 1, &mut count)
}

fn bump(count: &mut usize) -> Result<(), TreeError> {
    *count += 1;
    if *count > MAX_NODES {
        Err(TreeError::TooLarge)
    } else {
        Ok(())
    }
}

fn node(v: &Value, depth: usize, count: &mut usize) -> Result<Node, TreeError> {
    if depth > MAX_DEPTH {
        return Err(TreeError::TooDeep);
    }
    bump(count)?;
    let Some(obj) = v.as_object() else {
        return Ok(Node::Unknown);
    };
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or_default();
    Ok(match ty {
        "col" | "row" => {
            let gap = obj
                .get("gap")
                .and_then(Value::as_f64)
                .map(|g| (g as f32).clamp(0., MAX_GAP));
            let mut children = Vec::new();
            for child in obj
                .get("children")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                children.push(node(child, depth + 1, count)?);
            }
            if ty == "col" {
                Node::Col { gap, children }
            } else {
                Node::Row { gap, children }
            }
        }
        "text" => match string(obj.get("text"), MAX_TEXT) {
            Some(text) => Node::Text {
                text,
                style: match obj.get("style").and_then(Value::as_str) {
                    Some("title") => TextStyle::Title,
                    Some("caption") => TextStyle::Caption,
                    Some("mono") => TextStyle::Mono,
                    _ => TextStyle::Body,
                },
                tone: obj.get("tone").and_then(tone),
            },
            None => Node::Unknown,
        },
        "badge" => match string(obj.get("text"), 80) {
            Some(text) => Node::Badge {
                text,
                tone: obj.get("tone").and_then(tone).unwrap_or_default(),
            },
            None => Node::Unknown,
        },
        "button" => match (string(obj.get("label"), 80), string(obj.get("action"), 200)) {
            (Some(label), Some(action)) => Node::Button {
                label,
                action,
                payload: payload(obj.get("payload")),
                style: match obj.get("style").and_then(Value::as_str) {
                    Some("primary") => ButtonStyle::Primary,
                    Some("ghost") => ButtonStyle::Ghost,
                    _ => ButtonStyle::Secondary,
                },
            },
            _ => Node::Unknown,
        },
        "list" => {
            let mut items = Vec::new();
            for item in obj
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                bump(count)?;
                let Some(it) = item.as_object() else { continue };
                let Some(title) = string(it.get("title"), MAX_TEXT) else {
                    continue;
                };
                let trailing = match it.get("trailing") {
                    Some(t) if !t.is_null() => Some(Box::new(node(t, depth + 2, count)?)),
                    _ => None,
                };
                items.push(ListItem {
                    title,
                    subtitle: string(it.get("subtitle"), MAX_TEXT),
                    trailing,
                    action: string(it.get("action"), 200),
                    payload: payload(it.get("payload")),
                });
            }
            Node::List { items }
        }
        "markdown" => match string(obj.get("source"), MAX_MARKDOWN) {
            Some(source) => Node::Markdown { source },
            None => Node::Unknown,
        },
        "divider" => Node::Divider,
        _ => Node::Unknown,
    })
}

fn tone(v: &Value) -> Option<Tone> {
    Some(match v.as_str()? {
        "neutral" => Tone::Neutral,
        "success" => Tone::Success,
        "warning" => Tone::Warning,
        "danger" => Tone::Danger,
        "accent" => Tone::Accent,
        _ => return None,
    })
}

/// A string field, cut to `max` bytes on a character boundary, with control
/// characters other than newlines and tabs dropped.
/// A button's or list item's payload, sent back as it came. One over
/// [`MAX_PAYLOAD`] is dropped (the action is still sent) rather than held
/// in the tree and copied on every press.
fn payload(v: Option<&Value>) -> Value {
    let Some(v) = v else { return Value::Null };
    match serde_json::to_vec(v) {
        Ok(bytes) if bytes.len() <= MAX_PAYLOAD => v.clone(),
        _ => Value::Null,
    }
}

fn string(v: Option<&Value>, max: usize) -> Option<String> {
    let raw = v?.as_str()?;
    let mut out = String::with_capacity(raw.len().min(max));
    for c in raw.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    Some(out)
}

impl Node {
    /// What a session-row badge shows: a lone `badge` or `text`, nothing
    /// else (the badge slot draws one small chip).
    pub fn as_badge(&self) -> Option<(&str, Tone)> {
        match self {
            Node::Badge { text, tone } if !text.is_empty() => Some((text, *tone)),
            Node::Text { text, tone, .. } if !text.is_empty() => {
                Some((text, tone.unwrap_or_default()))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_oversized_payload_is_dropped_and_the_button_kept() {
        let big = "x".repeat(MAX_PAYLOAD + 1);
        let tree = decode(&json!({
            "type": "button", "label": "Go", "action": "go", "payload": { "blob": big }
        }));
        match tree {
            Ok(Node::Button {
                action, payload, ..
            }) => {
                assert_eq!(action, "go");
                assert_eq!(payload, Value::Null);
            }
            other => panic!("expected a button, got {other:?}"),
        }
    }

    #[test]
    fn every_node_type_decodes() {
        let tree = decode(&json!({
            "type": "col", "gap": 8, "children": [
                { "type": "text", "text": "Issues", "style": "title" },
                { "type": "text", "text": "3 open", "style": "caption", "tone": "warning" },
                { "type": "row", "children": [
                    { "type": "badge", "text": "In review", "tone": "accent" },
                    { "type": "button", "label": "Refresh", "action": "refresh", "style": "ghost" }
                ]},
                { "type": "divider" },
                { "type": "list", "items": [
                    { "title": "ENG-12 Fix login", "subtitle": "Lorik", "action": "open",
                      "payload": { "id": "ENG-12" },
                      "trailing": { "type": "badge", "text": "Todo" } },
                    { "title": "ENG-9" }
                ]},
                { "type": "markdown", "source": "**Done** this week" },
                { "type": "hologram", "beam": 3 }
            ]
        }))
        .unwrap();
        let Node::Col { gap, children } = tree else {
            panic!("a col")
        };
        assert_eq!(gap, Some(8.));
        assert_eq!(children.len(), 7);
        assert_eq!(
            children[0],
            Node::Text {
                text: "Issues".into(),
                style: TextStyle::Title,
                tone: None
            }
        );
        assert!(matches!(
            &children[2],
            Node::Row { children, .. } if matches!(children[1], Node::Button { style: ButtonStyle::Ghost, .. })
        ));
        let Node::List { items } = &children[4] else {
            panic!("a list")
        };
        assert_eq!(items[0].payload, json!({ "id": "ENG-12" }));
        assert!(matches!(
            items[0].trailing.as_deref(),
            Some(Node::Badge {
                tone: Tone::Neutral,
                ..
            })
        ));
        assert_eq!(items[1].action, None);
        assert_eq!(children[6], Node::Unknown);
    }

    #[test]
    fn malformed_nodes_draw_as_nothing() {
        assert_eq!(decode(&json!({ "type": "text" })).unwrap(), Node::Unknown);
        assert_eq!(
            decode(&json!({ "type": "button", "label": "Go" })).unwrap(),
            Node::Unknown
        );
        assert_eq!(decode(&json!("text")).unwrap(), Node::Unknown);
        // An unknown tone falls back to the style's own ink.
        assert!(matches!(
            decode(&json!({ "type": "text", "text": "x", "tone": "#ff0000" })).unwrap(),
            Node::Text { tone: None, .. }
        ));
    }

    #[test]
    fn a_tree_deeper_than_eight_is_refused() {
        let mut v = json!({ "type": "text", "text": "leaf" });
        for _ in 0..7 {
            v = json!({ "type": "col", "children": [v] });
        }
        assert!(decode(&v).is_ok(), "eight levels are fine");
        let v = json!({ "type": "col", "children": [v] });
        assert_eq!(decode(&v), Err(TreeError::TooDeep));
    }

    #[test]
    fn a_tree_over_five_hundred_nodes_is_refused() {
        let leaf = json!({ "type": "divider" });
        let ok = json!({ "type": "col", "children": vec![leaf.clone(); 499] });
        assert!(decode(&ok).is_ok());
        let big = json!({ "type": "col", "children": vec![leaf; 500] });
        assert_eq!(decode(&big), Err(TreeError::TooLarge));
        // List items count too.
        let items = vec![json!({ "title": "x" }); 500];
        assert_eq!(
            decode(&json!({ "type": "list", "items": items })),
            Err(TreeError::TooLarge)
        );
    }

    #[test]
    fn strings_are_capped_and_scrubbed() {
        let long = "é".repeat(5_000);
        let Node::Text { text, .. } = decode(&json!({ "type": "text", "text": long })).unwrap()
        else {
            panic!("text")
        };
        assert!(text.len() <= MAX_TEXT);
        let Node::Text { text, .. } =
            decode(&json!({ "type": "text", "text": "a\u{1b}[31mb\nc" })).unwrap()
        else {
            panic!("text")
        };
        assert_eq!(text, "a[31mb\nc");
        assert!(matches!(
            decode(&json!({ "type": "col", "gap": 900, "children": [] })).unwrap(),
            Node::Col { gap: Some(g), .. } if g == MAX_GAP
        ));
    }

    #[test]
    fn a_badge_slot_takes_one_badge_or_text() {
        let b = decode(&json!({ "type": "badge", "text": "Blocked", "tone": "danger" })).unwrap();
        assert_eq!(b.as_badge(), Some(("Blocked", Tone::Danger)));
        let t = decode(&json!({ "type": "text", "text": "ENG-4" })).unwrap();
        assert_eq!(t.as_badge(), Some(("ENG-4", Tone::Neutral)));
        let c = decode(&json!({ "type": "col", "children": [] })).unwrap();
        assert_eq!(c.as_badge(), None);
    }
}
