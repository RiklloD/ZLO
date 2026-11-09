//! Settings → Permissions: the global auto-approve switch, then every tool as
//! a card, grouped by what it touches. Groups start folded; a search or the
//! Changed filter opens every group with a match. Port of GPUI
//! `pages/settings/tools.rs`.

use std::collections::BTreeMap;

use rune::{Cursor, El, col, row, text};
use rune_kit::{Kit, TextInput};
use zlo_client::wire::ToolPermission;

use super::STATES;
use crate::design::Palette;
use crate::icons;
use crate::pages::common::{
    SM, XS, block_fill, empty_state, glyph, group_header, label, rows_card, segmented, setting_row,
    switch,
};
use crate::state::State;

/// Built-in tools grouped by the first word of their name.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "Files and documents",
        &[
            "read",
            "write",
            "apply",
            "list",
            "glob",
            "grep",
            "document",
            "attach",
            "file",
            "html",
            "structured",
            "edit",
        ],
    ),
    (
        "Shell and web",
        &[
            "shell", "exec", "http", "web", "browser", "fetch", "computer",
        ],
    ),
    ("Memory and skills", &["memory", "skill", "supermemory"]),
    ("Extensions", &["extension", "ironhub", "mcp"]),
    (
        "Automations",
        &["trigger", "automation", "schedule", "routine"],
    ),
    (
        "Threads and projects",
        &[
            "project", "thread", "spawn", "profile", "result", "task", "subagent",
        ],
    ),
    (
        "Messages",
        &["outbound", "notification", "echo", "message", "send"],
    ),
    (
        "Administration",
        &["admin", "llm", "operator", "settings", "config"],
    ),
];
const OTHER: &str = "Other built-in tools";

/// Sort key and title of a tool's group: built-in groups in `GROUPS` order,
/// then extensions by name, then the rest.
fn group_of(id: &str) -> (usize, String) {
    match id.split_once('.') {
        Some(("builtin", name)) => {
            let word = name.split('_').next().unwrap_or(name);
            GROUPS
                .iter()
                .position(|(_, words)| words.contains(&word))
                .map_or((usize::MAX, OTHER.to_string()), |ix| {
                    (ix, GROUPS[ix].0.to_string())
                })
        }
        Some((prefix, _)) => (GROUPS.len(), humanize(prefix)),
        None => (usize::MAX, OTHER.to_string()),
    }
}

/// `builtin.apply_patch` → "Apply patch".
fn humanize(id: &str) -> String {
    let name = id
        .rsplit_once('.')
        .map_or(id, |(_, n)| n)
        .replace(['_', '-'], " ");
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => name,
    }
}

/// GPUI `kit::hover_wash`.
fn hover_wash(p: &Palette) -> rune::Color {
    if p.light {
        rune::Color::BLACK.alpha(0.035)
    } else {
        rune::Color::WHITE.alpha(0.04)
    }
}

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let this = &s.pages.settings;
    let auto = this.auto_approve.unwrap_or(false);
    let toggle = switch(
        k,
        auto,
        this.auto_approve.is_none(),
        "Approve tools automatically",
        move |s: &mut State| super::set_auto_approve(s, !auto),
    );
    let mut page = col().w_full().child(rows_card(
        p,
        [setting_row(
            p,
            "Approve tools automatically",
            Some(if auto {
                "On: shell commands, file writes and network calls run without asking you. Tools set to Ask below still ask."
            } else {
                "Off: IronClaw asks before shell commands, file writes and network calls, unless a tool is set to Allow."
            }),
            toggle,
        )],
    ));
    if this.tools.is_empty() {
        return page;
    }

    let query = this.tool_search.text.trim().to_lowercase();
    let changed_only = this.tools_changed_only;
    let mut groups: BTreeMap<(usize, String), Vec<&(String, ToolPermission)>> = BTreeMap::new();
    for entry in &this.tools {
        let (id, tool) = entry;
        let matches = query.is_empty()
            || id.to_lowercase().contains(&query)
            || tool.description.to_lowercase().contains(&query);
        if matches && (!changed_only || tool.has_override()) {
            groups.entry(group_of(id)).or_default().push(entry);
        }
    }
    let changed = this.tools.iter().filter(|(_, t)| t.has_override()).count();

    let filter = segmented(
        k,
        &["All", "Changed"],
        usize::from(changed_only),
        false,
        |s: &mut State, ix, _| s.pages.settings.tools_changed_only = ix == 1,
    );
    let search = TextInput::new(&this.tool_search)
        .placeholder("Search tools")
        .width(None)
        .on_edit(|s: &mut State| &mut s.pages.settings.tool_search)
        .build(k);
    page = page
        .child(group_header(
            p,
            "Per tool",
            Some("Default follows the switch above. Allow, Ask and Off override it for one tool."),
        ))
        .child(
            row()
                .key("tool-filter-row")
                .w_full()
                .gap(12.)
                .pb(12.)
                .center_y()
                .child(col().grow(1.0).min_w(0.).child(search))
                .child(label(
                    format!("{} tools · {changed} changed", this.tools.len()),
                    XS,
                    400.,
                    p.text_muted,
                ))
                .child(filter),
        );
    if groups.is_empty() {
        return page.child(empty_state(
            p,
            if changed_only {
                "No tool has its own setting."
            } else {
                "No tool matches."
            },
        ));
    }
    // Searching or filtering shows every match without extra clicks.
    let force_open = !query.is_empty() || changed_only;
    let sections = groups.into_iter().map(|((_, title), tools)| {
        let open = force_open || this.tool_groups_open.contains(&title);
        tool_group(s, k, title, tools, open)
    });
    page.child(col().w_full().gap(8.).children(sections))
}

fn tool_group(
    s: &State,
    k: &Kit,
    title: String,
    tools: Vec<&(String, ToolPermission)>,
    open: bool,
) -> El {
    let p = &s.palette;
    let changed = tools.iter().filter(|(_, t)| t.has_override()).count();
    let count = match changed {
        0 => format!("{}", tools.len()),
        n => format!("{} · {n} changed", tools.len()),
    };
    let key = title.clone();
    let head = row()
        .key(format!("tool-group-{title}"))
        .role("button")
        .label(title.clone())
        .focusable(true)
        .gap(8.)
        .px(12.)
        .h(40.)
        .center_y()
        .radius(10.)
        .cursor(Cursor::Pointer)
        .fill(block_fill(p))
        .hover(hover_wash(p))
        .child(glyph(
            if open {
                &icons::CHEVRON_DOWN
            } else {
                &icons::CHEVRON_RIGHT
            },
            13.,
            p.text_muted,
        ))
        .child(label(title.clone(), SM, 500., p.text).grow(1.0))
        .child(label(count, XS, 400., p.text_muted))
        .on_press(move |s: &mut State| {
            let open = &mut s.pages.settings.tool_groups_open;
            if !open.remove(&key) {
                open.insert(key.clone());
            }
        });
    let mut d = col()
        .key(format!("tool-section-{title}"))
        .w_full()
        .gap(8.)
        .child(head);
    if open {
        let mut grid = col().w_full().gap(8.).pb(8.);
        // Two equal columns, as GPUI's `grid_cols(2)`.
        let mut cards = tools
            .into_iter()
            .map(|(id, tool)| tool_card(s, k, id, tool))
            .peekable();
        let mut ix = 0;
        while cards.peek().is_some() {
            let mut pair = row().key(format!("tool-row-{title}-{ix}")).w_full().gap(8.);
            for _ in 0..2 {
                pair = pair.child(match cards.next() {
                    Some(c) => c,
                    None => col().grow(1.0).min_w(0.),
                });
            }
            grid = grid.child(pair);
            ix += 1;
        }
        d = d.child(grid);
    }
    d
}

fn tool_card(s: &State, k: &Kit, id: &str, tool: &ToolPermission) -> El {
    let p = &s.palette;
    let selected = STATES
        .iter()
        .position(|(state, _)| *state == tool.state)
        .unwrap_or(0);
    let labels: Vec<&str> = STATES.iter().map(|(_, label)| *label).collect();
    let pick_id = id.to_string();
    let segments = segmented(
        k,
        &labels,
        selected,
        tool.locked,
        move |s: &mut State, six, _| super::set_tool(s, pick_id.clone(), STATES[six].0),
    );
    let status = if tool.locked {
        "Locked"
    } else if tool.has_override() {
        "Custom"
    } else {
        "Follows switch"
    };
    col()
        .key(format!("tool-card-{id}"))
        .grow(1.0)
        .min_w(0.)
        .padding(12.)
        .gap(8.)
        .radius(10.)
        .border(
            1.,
            if tool.has_override() {
                p.accent.alpha(0.35)
            } else {
                p.border.alpha(p.border.a * 0.6)
            },
        )
        .help(tool.name.clone())
        .child(label(humanize(&tool.name), SM, 500., p.text).truncate())
        .child(
            col().h(34.).child(
                text(tool.description.clone())
                    .font(crate::pages::common::font(XS, 400.))
                    .color(p.text_muted)
                    .wrap()
                    .max_lines(2),
            ),
        )
        .child(
            row()
                .gap(8.)
                .center_y()
                .child(
                    label(status, XS, 400., p.text_faint)
                        .truncate()
                        .grow(1.0)
                        .min_w(0.),
                )
                .child(segments),
        )
}

pub fn fixture(s: &mut State) {
    let tool = |name: &str, desc: &str, state: &str, source: Option<&str>, locked: bool| {
        serde_json::from_value::<ToolPermission>(serde_json::json!({
            "name": name, "description": desc, "state": state,
            "locked": locked, "effective_source": source,
        }))
    };
    let rows = [
        (
            "builtin.apply_patch",
            "Apply a unified diff to files in the workspace.",
            "default",
            Some("global"),
            false,
        ),
        (
            "builtin.read_file",
            "Read a file from the workspace.",
            "always_allow",
            Some("override"),
            false,
        ),
        (
            "builtin.shell",
            "Run a shell command in the project folder.",
            "ask_each_time",
            Some("override"),
            false,
        ),
        (
            "builtin.http_request",
            "Make an HTTP request through the network boundary.",
            "default",
            Some("global"),
            false,
        ),
        (
            "builtin.memory_write",
            "Save a note to persistent memory.",
            "default",
            Some("global"),
            false,
        ),
        (
            "builtin.admin_restart",
            "Restart the gateway.",
            "disabled",
            Some("floor"),
            true,
        ),
        (
            "slack.send_message",
            "Post a message to a Slack channel.",
            "default",
            Some("global"),
            false,
        ),
    ];
    let mut tools = Vec::new();
    for (name, desc, state, source, locked) in rows {
        match tool(name, desc, state, source, locked) {
            Ok(t) => tools.push((name.to_string(), t)),
            Err(e) => tracing::warn!(error = %e, "tool fixture"),
        }
    }
    let this = &mut s.pages.settings;
    this.auto_approve = Some(false);
    this.tools = tools;
    this.tool_groups_open.insert("Files and documents".into());
    this.tool_groups_open.insert("Shell and web".into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_land_in_named_groups() {
        assert_eq!(group_of("builtin.apply_patch").1, "Files and documents");
        assert_eq!(group_of("builtin.extension_install").1, "Extensions");
        assert_eq!(group_of("builtin.zzz_unknown").1, OTHER);
        assert_eq!(group_of("slack.send_message").1, "Slack");
        assert_eq!(humanize("builtin.apply_patch"), "Apply patch");
    }
}
