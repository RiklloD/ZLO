//! Settings → Permissions: the global auto-approve switch, then every tool as
//! a card, grouped by what it touches. Groups start folded; a search or the
//! Changed filter opens every group with a match.

use std::collections::BTreeMap;

use gpui_kit::component::tooltip::Tooltip;
use ironclaw_gpui_client::wire::ToolPermission;

use super::{STATES, SettingsView};
use crate::design::{Palette, kit};
use crate::pages::{block_fill, group_header, rows_card, setting_row};
use crate::ui::*;

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

impl SettingsView {
    pub(super) fn render_tools(&self, cx: &Context<Self>) -> impl IntoElement {
        let auto = self.auto_approve.unwrap_or(false);
        let switch = Switch::new("auto-approve")
            .checked(auto)
            .disabled(self.auto_approve.is_none())
            .accessibility_label("Approve tools automatically")
            .on_click(
                cx.listener(|this, checked: &bool, _, cx| this.set_auto_approve(*checked, cx)),
            );
        let toggle = rows_card(
            [setting_row(
                "Approve tools automatically",
                Some(
                    if auto {
                        "On: shell commands, file writes and network calls run without asking you. Tools set to Ask below still ask."
                    } else {
                        "Off: IronClaw asks before shell commands, file writes and network calls, unless a tool is set to Allow."
                    }
                    .into(),
                ),
                switch,
                cx,
            )],
            cx,
        );
        let mut page = v_flex().child(toggle);
        if self.tools.is_empty() {
            return page;
        }

        let query = self.tool_search.read(cx).value().trim().to_lowercase();
        let changed_only = self.tools_changed_only;
        let mut groups: BTreeMap<(usize, String), Vec<&(String, ToolPermission)>> = BTreeMap::new();
        for entry in &self.tools {
            let (id, tool) = entry;
            let matches = query.is_empty()
                || id.to_lowercase().contains(&query)
                || tool.description.to_lowercase().contains(&query);
            if matches && (!changed_only || tool.has_override()) {
                groups.entry(group_of(id)).or_default().push(entry);
            }
        }
        let changed = self.tools.iter().filter(|(_, t)| t.has_override()).count();

        let weak = cx.weak_entity();
        let filter = kit::segmented(
            "tool-filter",
            &["All", "Changed"],
            usize::from(changed_only),
            false,
            move |ix, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.tools_changed_only = ix == 1;
                    cx.notify();
                });
            },
            cx,
        );
        page = page
            .child(group_header(
                "Per tool",
                Some("Default follows the switch above. Allow, Ask and Off override it for one tool."),
                cx,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .pb_3()
                    .items_center()
                    .child(div().flex_1().child(Input::new(&self.tool_search).small()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(Palette::of(cx).text_muted)
                            .child(format!("{} tools · {changed} changed", self.tools.len())),
                    )
                    .child(filter),
            );
        if groups.is_empty() {
            return page.child(crate::pages::empty_state(
                if changed_only {
                    "No tool has its own setting."
                } else {
                    "No tool matches."
                },
                cx,
            ));
        }
        // Searching or filtering shows every match without extra clicks.
        let force_open = !query.is_empty() || changed_only;
        let sections = groups.into_iter().map(|((_, title), tools)| {
            let open = force_open || self.tool_groups_open.contains(&title);
            self.render_tool_group(title, tools, open, cx)
        });
        page.child(v_flex().gap_2().children(sections))
    }

    fn render_tool_group(
        &self,
        title: String,
        tools: Vec<&(String, ToolPermission)>,
        open: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let changed = tools.iter().filter(|(_, t)| t.has_override()).count();
        let count = match changed {
            0 => format!("{}", tools.len()),
            n => format!("{} · {n} changed", tools.len()),
        };
        let key = title.clone();
        let head = h_flex()
            .id(SharedString::from(format!("tool-group-{title}")))
            .role(Role::Button)
            .aria_label(title.clone())
            .gap_2()
            .px(px(12.))
            .h(px(40.))
            .items_center()
            .rounded(px(10.))
            .cursor_pointer()
            .bg(block_fill(cx))
            .hover(|s| s.bg(kit::hover_wash(&p)))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(13.))
                .text_color(p.text_muted),
            )
            .child(div().flex_1().text_sm().font_medium().child(title))
            .child(div().text_xs().text_color(p.text_muted).child(count))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.tool_groups_open.remove(&key) {
                    this.tool_groups_open.insert(key.clone());
                }
                cx.notify();
            }));
        v_flex()
            .gap_2()
            .child(head)
            .when(open, |d| {
                d.child(
                    div().grid().grid_cols(2).gap_2().pb_2().children(
                        tools
                            .into_iter()
                            .map(|(id, tool)| self.render_tool_card(id, tool, cx)),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_tool_card(&self, id: &str, tool: &ToolPermission, cx: &Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let selected = STATES
            .iter()
            .position(|(state, _)| *state == tool.state)
            .unwrap_or(0);
        let labels: Vec<&'static str> = STATES.iter().map(|(_, label)| *label).collect();
        let (pick_id, weak) = (id.to_string(), cx.weak_entity());
        let segments = kit::segmented(
            &format!("tool-state:{id}"),
            &labels,
            selected,
            tool.locked,
            move |six, _, cx| {
                let (id, state) = (pick_id.clone(), STATES[six].0);
                let _ = weak.update(cx, |this, cx| this.set_tool(id, state, cx));
            },
            cx,
        );
        let status = if tool.locked {
            "Locked"
        } else if tool.has_override() {
            "Custom"
        } else {
            "Follows switch"
        };
        let full_id: SharedString = tool.name.clone().into();
        v_flex()
            .id(SharedString::from(format!("tool-card-{id}")))
            .min_w_0()
            .p(px(12.))
            .gap_2()
            .rounded(px(10.))
            .border_1()
            .border_color(if tool.has_override() {
                p.accent.opacity(0.35)
            } else {
                p.border.opacity(0.6)
            })
            .tooltip(move |window, cx| Tooltip::new(full_id.clone()).build(window, cx))
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .truncate()
                    .child(humanize(&tool.name)),
            )
            .child(
                div()
                    .h(px(34.))
                    .text_xs()
                    .text_color(p.text_muted)
                    .line_clamp(2)
                    .child(tool.description.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(p.text_faint)
                            .child(status),
                    )
                    .child(segments),
            )
            .into_any_element()
    }
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
