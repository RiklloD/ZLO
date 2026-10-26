//! What one session last learnt about the window it drives, from the state
//! headers and accessibility tree of its results (the Hermes plugin's
//! `codex._State` and `_absorb`): the app, its pid and window, the element
//! lines (labels for the viewer) and the screenshot size (for coordinates).

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub(crate) struct TargetState {
    /// Display name (`Window: "…", App: <name>.`), else the bundle id.
    pub app: String,
    pub bundle: String,
    pub pid: Option<i64>,
    pub window_id: Option<u32>,
    pub title: String,
    /// Element index → short label ("button Save").
    pub elements: HashMap<String, String>,
    /// Pixel size of the latest screenshot.
    pub shot_size: Option<(u32, u32)>,
}

fn regex(cell: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    cell.get_or_init(|| match Regex::new(pattern) {
        Ok(regex) => Some(regex),
        Err(error) => {
            tracing::error!(%error, "computer-use result pattern is invalid");
            None
        }
    })
    .as_ref()
}

macro_rules! pattern {
    ($name:ident, $pattern:expr) => {
        fn $name() -> Option<&'static Regex> {
            static CELL: OnceLock<Option<Regex>> = OnceLock::new();
            regex(&CELL, $pattern)
        }
    };
}

pattern!(header_app, r"(?m)^App=(\S+) \(pid (\d+)\)");
pattern!(header_window, r#"(?m)^Window: "(.*)", App: (.*?)\.$"#);
pattern!(element_line, r"(?m)^\t*(\d+) (.+)$");
pattern!(
    diff_mark,
    r"(?m)^\((?:tree diff:|no accessibility tree changes)"
);
pattern!(diff_added, r"(?m)^\+ (\d+) (.+)$");
pattern!(
    label_trim,
    r"( ID: | Help: |, Secondary Actions: | Secondary Actions: | Description: ).*$"
);
pattern!(description, r"Description: ([^,]+)");

const GENERIC_ROLES: &[&str] = &["button", "text", "row", "cell", "container", "group"];

pub(crate) fn element_label(line: &str) -> String {
    let mut short = label_trim()
        .map_or(line.to_string(), |trim| trim.replace(line, "").to_string())
        .trim()
        .to_string();
    if (short.is_empty() || GENERIC_ROLES.contains(&short.as_str()))
        && let Some(found) = description().and_then(|desc| desc.captures(line))
    {
        short = format!("{short} {}", &found[1]).trim().to_string();
    }
    short.chars().take(60).collect()
}

/// Width and height from a PNG header.
pub(crate) fn png_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    Some((width, height))
}

impl TargetState {
    /// Learn the target from a state or action result. `false` when the text
    /// has no state header (list_apps, errors).
    pub(crate) fn absorb(&mut self, text: &str, png: Option<&[u8]>) -> bool {
        let Some(app) = header_app().and_then(|re| re.captures(text)) else {
            return false;
        };
        self.bundle = app[1].to_string();
        self.pid = app[2].parse().ok();
        match header_window().and_then(|re| re.captures(text)) {
            Some(window) => {
                self.title = window[1].to_string();
                self.app = window[2].to_string();
            }
            None => {
                self.title.clear();
                self.app = self.bundle.clone();
            }
        }
        if diff_mark().is_some_and(|re| re.is_match(text)) {
            // A tree diff lists changed lines only: unchanged elements keep
            // their labels.
            if let Some(added) = diff_added() {
                for line in added.captures_iter(text) {
                    self.elements
                        .insert(line[1].to_string(), element_label(&line[2]));
                }
            }
        } else if let Some(elements) = element_line() {
            self.elements = elements
                .captures_iter(text)
                .map(|line| (line[1].to_string(), element_label(&line[2])))
                .collect();
        }
        if let Some(size) = png.and_then(png_size) {
            self.shot_size = Some(size);
        }
        true
    }

    /// Whether a call's `app` argument names this target.
    pub(crate) fn same_app(&self, app: Option<&Value>) -> bool {
        let Some(app) = app.and_then(Value::as_str) else {
            return false;
        };
        let app = app.trim().to_lowercase();
        !app.is_empty()
            && (app == self.app.to_lowercase()
                || app == self.bundle.to_lowercase()
                || app == self.app.to_lowercase().trim_start_matches('\u{200e}'))
    }

    /// The element's label when the call targets this app by index.
    pub(crate) fn element_for(&self, args: &Value) -> Option<&str> {
        if !self.same_app(args.get("app")) {
            return None;
        }
        let index = match args.get("element_index")? {
            Value::String(index) => index.clone(),
            other => other.to_string(),
        };
        self.elements.get(&index).map(String::as_str)
    }

    /// Screenshot pixels → fractions of the window.
    pub(crate) fn shot_fraction(&self, x: Option<&Value>, y: Option<&Value>) -> Option<(f64, f64)> {
        let (width, height) = self.shot_size?;
        let (x, y) = (x?.as_f64()?, y?.as_f64()?);
        Some((x / f64::from(width), y / f64::from(height)))
    }
}
