//! The front window's accessibility tree as compact text lines (role,
//! label, value, frame), so the agent can read the screen and not only look
//! at it. The walk has a node cap, a depth cap and a deadline, and every AX
//! call a per-app timeout, so a huge or hung app never stalls the capture;
//! the text then has a size budget of about 4k tokens.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use super::cf::{self, CfRef, Owned};

const MAX_NODES: usize = 2500;
const MAX_DEPTH: usize = 64;
const DEADLINE: Duration = Duration::from_millis(1500);
/// Seconds one AX message may take before the app counts as busy.
const MESSAGE_TIMEOUT: f32 = 0.25;
/// About 4k tokens at ~4 characters a token.
pub(super) const TEXT_BUDGET: usize = 16_000;
const FIELD_CHARS: usize = 160;

/// Attribute order of one `AXUIElementCopyMultipleAttributeValues` call.
const ATTRIBUTES: [&str; 7] = [
    "AXRole",
    "AXTitle",
    "AXDescription",
    "AXValue",
    "AXPosition",
    "AXSize",
    "AXChildren",
];

/// Roles worth a line even with no label or value: things you can press
/// or type into.
const CONTROLS: [&str; 14] = [
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXMenuButton",
    "AXComboBox",
    "AXTextField",
    "AXTextArea",
    "AXSearchField",
    "AXSlider",
    "AXLink",
    "AXTab",
    "AXDisclosureTriangle",
    "AXIncrementor",
];

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> CfRef;
    fn AXUIElementCopyAttributeValue(element: CfRef, attribute: CfRef, value: *mut CfRef) -> i32;
    fn AXUIElementCopyMultipleAttributeValues(
        element: CfRef,
        attributes: CfRef,
        options: u32,
        values: *mut CfRef,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CfRef, seconds: f32) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetType(value: CfRef) -> u32;
    fn AXValueGetValue(value: CfRef, kind: u32, out: *mut c_void) -> bool;
}

const AX_VALUE_POINT: u32 = 1;
const AX_VALUE_SIZE: u32 = 2;

/// One element of the tree, in walk (pre-)order.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Node {
    pub depth: usize,
    pub role: String,
    pub label: String,
    pub value: String,
    /// x, y, width, height in screen points.
    pub frame: Option<(f64, f64, f64, f64)>,
}

pub(super) struct Tree {
    /// The window's AX title, for when CoreGraphics gave none.
    pub title: String,
    pub nodes: Vec<Node>,
    /// The walk stopped at a cap or the deadline.
    pub partial: bool,
}

pub(super) fn trusted() -> bool {
    // SAFETY: a plain status query.
    unsafe { AXIsProcessTrusted() }
}

/// Walk the app's focused window (else its main or first window).
pub(super) fn walk(pid: i32) -> Result<Tree, String> {
    if !trusted() {
        return Err("Accessibility permission is off for Numoh.".into());
    }
    // SAFETY: create rule; null handled by `Owned::new`.
    let app = Owned::new(unsafe { AXUIElementCreateApplication(pid) })
        .ok_or("The app has no accessibility element.")?;
    let started = Instant::now();
    limit(&app, started);
    let window = copy(&app, "AXFocusedWindow")
        .or_else(|| copy(&app, "AXMainWindow"))
        .or_else(|| {
            let windows = copy(&app, "AXWindows")?;
            first_element(windows.0)
        })
        .ok_or("The app reports no window to accessibility.")?;
    if started.elapsed() > DEADLINE {
        return Err("The app did not answer accessibility in time.".into());
    }
    limit(&window, started);
    let title = copy(&window, "AXTitle")
        .and_then(|t| cf::string(t.0))
        .unwrap_or_default();
    let Some(names) = attribute_names() else {
        return Err("Could not build the attribute list.".into());
    };
    let mut nodes = Vec::new();
    let mut partial = false;
    let mut stack = vec![(window, 0usize)];
    while let Some((element, depth)) = stack.pop() {
        if nodes.len() >= MAX_NODES || started.elapsed() > DEADLINE {
            partial = true;
            break;
        }
        limit(&element, started);
        // Never hold more children than nodes still left to visit.
        let room = MAX_NODES.saturating_sub(nodes.len() + stack.len());
        let Some((node, children, more)) = read(&element, &names, depth, room) else {
            continue;
        };
        nodes.push(node);
        partial |= more;
        if depth + 1 >= MAX_DEPTH {
            partial |= !children.is_empty();
            continue;
        }
        // Reversed so the first child is walked first.
        stack.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
    }
    Ok(Tree {
        title,
        nodes,
        partial,
    })
}

/// Cap the element's AX message timeout at the time the walk has left.
fn limit(element: &Owned, started: Instant) {
    let left = DEADLINE.saturating_sub(started.elapsed()).as_secs_f32();
    // SAFETY: a live AX element.
    unsafe { AXUIElementSetMessagingTimeout(element.0, MESSAGE_TIMEOUT.min(left).max(0.01)) };
}

fn copy(element: &Owned, attribute: &str) -> Option<Owned> {
    let name = Owned::string(attribute)?;
    let mut value: CfRef = std::ptr::null();
    // SAFETY: live element and name; `value` follows the copy rule.
    let error = unsafe { AXUIElementCopyAttributeValue(element.0, name.0, &mut value) };
    if error != 0 {
        return None;
    }
    Owned::new(value)
}

fn first_element(array: CfRef) -> Option<Owned> {
    // SAFETY: plain type id query.
    if !cf::type_is(array, unsafe { cf::CFArrayGetTypeID() }) {
        return None;
    }
    // SAFETY: a live CFArray; the element is retained so it outlives it.
    unsafe {
        if cf::CFArrayGetCount(array) == 0 {
            return None;
        }
        Owned::new(cf::CFRetain(cf::CFArrayGetValueAtIndex(array, 0)))
    }
}

/// The attribute names as one CFArray, built once per walk.
fn attribute_names() -> Option<(Owned, Vec<Owned>)> {
    let strings: Vec<Owned> = ATTRIBUTES.iter().filter_map(|a| Owned::string(a)).collect();
    if strings.len() != ATTRIBUTES.len() {
        return None;
    }
    let raw: Vec<CfRef> = strings.iter().map(|s| s.0).collect();
    // SAFETY: the values are live CFStrings, retained by the array.
    let array = Owned::new(unsafe {
        cf::CFArrayCreate(
            std::ptr::null(),
            raw.as_ptr(),
            raw.len() as isize,
            &raw const cf::kCFTypeArrayCallBacks,
        )
    })?;
    Some((array, strings))
}

/// One element's fields and up to `room` of its children, in one AX round
/// trip; `true` when children were left out.
fn read(
    element: &Owned,
    names: &(Owned, Vec<Owned>),
    depth: usize,
    room: usize,
) -> Option<(Node, Vec<Owned>, bool)> {
    let mut values: CfRef = std::ptr::null();
    // SAFETY: live element and name array; `values` follows the copy rule.
    let error =
        unsafe { AXUIElementCopyMultipleAttributeValues(element.0, names.0.0, 0, &mut values) };
    let values = Owned::new(values).filter(|_| error == 0)?;
    // SAFETY: a live CFArray with one slot per requested attribute; a
    // missing attribute holds an AXValue of the error type, which the
    // readers below reject.
    let at = |ix: usize| unsafe {
        if (ix as isize) < cf::CFArrayGetCount(values.0) {
            cf::CFArrayGetValueAtIndex(values.0, ix as isize)
        } else {
            std::ptr::null()
        }
    };
    let title = text(at(1));
    let description = text(at(2));
    let label = if title.is_empty() || title == description {
        description
    } else if description.is_empty() {
        title
    } else {
        format!("{title} ({description})")
    };
    let frame = match (
        ax_value::<[f64; 2]>(at(4), AX_VALUE_POINT),
        ax_value::<[f64; 2]>(at(5), AX_VALUE_SIZE),
    ) {
        (Some([x, y]), Some([w, h])) => Some((x, y, w, h)),
        _ => None,
    };
    let mut children = Vec::new();
    let mut more = false;
    let kids = at(6);
    // SAFETY: type-checked; each child is retained so it outlives `values`.
    if cf::type_is(kids, unsafe { cf::CFArrayGetTypeID() }) {
        let count = unsafe { cf::CFArrayGetCount(kids) };
        let kept = count.min(room as isize);
        more = kept < count;
        for ix in 0..kept {
            let child = unsafe { cf::CFArrayGetValueAtIndex(kids, ix) };
            if let Some(child) = Owned::new(unsafe { cf::CFRetain(child) }) {
                children.push(child);
            }
        }
    }
    let node = Node {
        depth,
        role: cf::string(at(0)).unwrap_or_default(),
        label,
        value: text(at(3)),
        frame,
    };
    Some((node, children, more))
}

/// A string, number or boolean attribute as one tidy line of text.
fn text(value: CfRef) -> String {
    let raw = if let Some(s) = cf::string(value) {
        s
    } else if let Some(n) = cf::number(value) {
        if n.fract() == 0. {
            format!("{}", n as i64)
        } else {
            format!("{n:.2}")
        }
    // SAFETY: plain type id query, then a type-checked read.
    } else if cf::type_is(value, unsafe { cf::CFBooleanGetTypeID() }) {
        (if unsafe { cf::CFBooleanGetValue(value) } {
            "true"
        } else {
            "false"
        })
        .to_string()
    } else {
        String::new()
    };
    tidy(&raw, FIELD_CHARS)
}

/// A point or size AXValue as two doubles.
fn ax_value<T: Default>(value: CfRef, kind: u32) -> Option<T> {
    // SAFETY: type-checked, then read into a slot of the size AX writes
    // for that kind (CGPoint and CGSize are two doubles each).
    unsafe {
        if !cf::type_is(value, AXValueGetTypeID()) || AXValueGetType(value) != kind {
            return None;
        }
        let mut out = T::default();
        AXValueGetValue(value, kind, (&raw mut out).cast()).then_some(out)
    }
}

/// Whitespace folded to single spaces, cut at `max` characters.
fn tidy(text: &str, max: usize) -> String {
    let folded = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.chars().count() <= max {
        return folded;
    }
    let mut cut: String = folded.chars().take(max).collect();
    cut.push('…');
    cut
}

/// The nodes as indented lines, within `budget` characters. Unlabelled
/// containers get no line, and indentation counts only printed ancestors,
/// so deep web pages stay narrow.
pub(super) fn render(nodes: &[Node], partial: bool, budget: usize) -> String {
    let mut out = String::new();
    let mut printed: Vec<usize> = Vec::new();
    let mut cut = partial;
    for node in nodes {
        while printed.last().is_some_and(|d| *d >= node.depth) {
            printed.pop();
        }
        let control = CONTROLS.contains(&node.role.as_str());
        if node.label.is_empty() && node.value.is_empty() && !control {
            continue;
        }
        let mut line = "  ".repeat(printed.len());
        line.push_str(
            node.role
                .strip_prefix("AX")
                .filter(|r| !r.is_empty())
                .unwrap_or(&node.role),
        );
        if !node.label.is_empty() {
            line.push_str(&format!(" \"{}\"", node.label));
        }
        if !node.value.is_empty() && node.value != node.label {
            line.push_str(&format!(" = \"{}\"", node.value));
        }
        if let Some((x, y, w, h)) = node.frame {
            line.push_str(&format!(" @{x:.0},{y:.0} {w:.0}x{h:.0}"));
        }
        line.push('\n');
        if out.len() + line.len() > budget {
            cut = true;
            break;
        }
        out.push_str(&line);
        printed.push(node.depth);
    }
    if cut {
        out.push_str("… (tree cut short to stay small)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Node, render};

    fn node(depth: usize, role: &str, label: &str, value: &str) -> Node {
        Node {
            depth,
            role: role.into(),
            label: label.into(),
            value: value.into(),
            frame: Some((10., 20., 30., 40.)),
        }
    }

    #[test]
    fn render_skips_bare_containers_and_keeps_to_the_budget() {
        let nodes = [
            node(0, "AXWindow", "Apple", ""),
            node(1, "AXGroup", "", ""),
            node(2, "AXButton", "Reload this page", ""),
            node(2, "AXTextField", "Address", "apple.com"),
            node(1, "AXStaticText", "", "Hello"),
        ];
        let text = render(&nodes, false, 10_000);
        assert_eq!(
            text,
            "Window \"Apple\" @10,20 30x40\n\
             \x20 Button \"Reload this page\" @10,20 30x40\n\
             \x20 TextField \"Address\" = \"apple.com\" @10,20 30x40\n\
             \x20 StaticText = \"Hello\" @10,20 30x40\n"
        );
        let short = render(&nodes, false, 60);
        assert!(short.starts_with("Window \"Apple\""));
        assert!(short.ends_with("… (tree cut short to stay small)\n"));
        assert!(!short.contains("Address"));
    }
}
