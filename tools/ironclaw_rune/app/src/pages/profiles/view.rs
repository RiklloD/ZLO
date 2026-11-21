//! Profiles rendering: the list, a profile's detail (header and editor),
//! and the small pieces they share.

use super::*;
use crate::design::motion;
use crate::pages::extensions::drill::{self, Level};

/// The Profiles list ↔ a profile's detail drill-down.
pub(super) const DRILL: &str = "profiles-drill";

/// One list row: initials, name, what it runs and when it last ran.
fn render_row(s: &State, k: &Kit, p_info: &ProfileInfo) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let id = p_info.profile_id.clone();
    let mut meta = vec![format!(
        "{} thread{}",
        p_info.thread_count,
        if p_info.thread_count == 1 { "" } else { "s" }
    )];
    // A default no signed-in provider offers runs nowhere; say so.
    let offered = |provider: &str| {
        ps.catalog.providers.iter().any(|c| {
            c.provider == provider
                && p_info
                    .settings
                    .default_model
                    .as_ref()
                    .is_none_or(|m| c.models.is_empty() || c.models.contains(m))
        })
    };
    match p_info.settings.default_provider.as_deref() {
        Some(provider) if !ps.catalog.providers.is_empty() && !offered(provider) => {
            meta.push("model unavailable".into());
        }
        _ => meta.extend(p_info.settings.default_model.clone()),
    }
    if let Some(branch) = &p_info.git_branch {
        meta.push(format!("on {branch}"));
    }
    if let Some(at) = p_info
        .last_used_at
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        meta.push(format!("used {}", jobs::when(at)));
    }
    let mut name_row = row().gap(8.0).center_y().child(
        styled(p_info.settings.display_name.clone(), sm_medium(), p.text)
            .truncate()
            .shrink(),
    );
    if p_info.is_default {
        name_row = name_row.child(Badge::new("Default").build(k));
    }
    let thread_id = id.clone();
    row()
        .key(format!("profile-row-{id}"))
        .role("button")
        .label(p_info.settings.display_name.clone())
        .hover_group()
        .w_full()
        .min_h(56.0)
        .px(16.0)
        .py(10.0)
        .gap(12.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .hover(hover_wash(p))
        .hover_motion(motion::HOVER, motion::HOVER)
        .motion(motion::ROW_RESORT)
        .on_press(move |s: &mut State| select(s, Some(id.clone())))
        .press_scale(motion::PRESS_CARD)
        .child(avatar(&p_info.profile_id, &p_info.settings.display_name))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(1.0)
                .child(name_row)
                .child(styled(meta.join(" · "), xs(), p.text_muted).truncate()),
        )
        // Shown on hover: start a thread without opening the profile.
        .child(
            el().group_opacity(0.0, 1.0).child(
                press_button(
                    k,
                    "profile-thread",
                    "New thread",
                    Look::Ghost,
                    Some(&icons::PLUS),
                    false,
                    move |s: &mut State| new_thread(s, thread_id.clone()),
                )
                .label("New thread on this profile")
                .hover_alone(),
            ),
        )
        .child(glyph(&icons::CHEVRON_RIGHT, 14.0, p.text_faint))
}

/// The list: profiles that ran, then the ones that never did.
fn render_list(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let (used, unused): (Vec<_>, Vec<_>) = s
        .pages
        .profiles
        .items
        .iter()
        .partition(|p| p.last_used_at.is_some());
    let mut list = col().key("profiles-list").w_full();
    for (key, title, group) in [
        ("used", "Recently used", used),
        ("unused", "Not used yet", unused),
    ] {
        if group.is_empty() {
            continue;
        }
        let rows: Vec<El> = group.into_iter().map(|pi| render_row(s, k, pi)).collect();
        list = list
            .child(group_header(p, title, None).key(format!("group-{key}")))
            .child(rows_card(p, rows).key(format!("card-{key}")));
    }
    list
}

/// The chosen folder (or the default) with Choose and Default buttons.
fn folder_row(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let shown = match (&ps.picks.folder, ps.selected_profile()) {
        (Some(folder), _) => folder.clone(),
        (None, Some(profile)) => format!("Default: {}", profile.effective_folder),
        (None, None) => "Default: ~/.ironclaw/profiles/<id>".into(),
    };
    let mut control = row().gap(4.0).center_y();
    if ps.picks.folder.is_some() {
        control = control.child(press_button(
            k,
            "profile-folder-reset",
            "Use default",
            Look::Ghost,
            None,
            false,
            |s: &mut State| s.pages.profiles.picks.folder = None,
        ));
    }
    control = control.child(
        rune_kit::Button::new("Choose…")
            .variant(rune_kit::ButtonVariant::Outline)
            .control_size(rune_kit::ControlSize::Regular)
            .icon(&icons::FOLDER_OPEN)
            .on_press_tx(pickers::pick_folder)
            .build(k)
            .key("profile-folder-pick"),
    );
    keyed_setting_row(p, "row-folder", "Working folder", Some(&shown), control)
}

/// A picker control sized for the right side of a setting row.
fn picker(s: &State, k: &Kit, layer: &mut Vec<El>, menu: Menu, summary: String) -> El {
    el().w(CONTROL_WIDTH)
        .child(pickers::trigger(s, k, layer, menu, summary))
}

/// "All tools" for an empty set, else "3 tools allowed".
fn count_summary(n: usize, empty: &str, noun: &str, verb: &str) -> String {
    match n {
        0 => empty.to_string(),
        1 => format!("1 {noun} {verb}"),
        n => format!("{n} {noun}s {verb}"),
    }
}

fn render_editor(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let profile = ps.selected_profile();
    let creating = ps.selected.is_none();
    let field =
        |edit: &TextEdit, placeholder: &str, key: &str, access: fn(&mut State) -> &mut TextEdit| {
            TextInput::new(edit)
                .width(Some(CONTROL_WIDTH))
                .placeholder(placeholder)
                .label(placeholder)
                .on_edit(access)
                .build(k)
                .key(key.to_string())
        };

    let mut general = Vec::new();
    if creating {
        general.push(keyed_setting_row(
            p,
            "row-id",
            "Id",
            Some("Lowercase, used as the folder name. It cannot change later."),
            field(
                &ps.id,
                "profile-id (lowercase, used as folder name)",
                "profile-id",
                |s| &mut s.pages.profiles.id,
            ),
        ));
    }
    general.push(keyed_setting_row(
        p,
        "row-name",
        "Display name",
        None,
        field(&ps.name, "Display name", "profile-name", |s| {
            &mut s.pages.profiles.name
        }),
    ));

    let model = [
        keyed_setting_row(
            p,
            "row-model",
            "Default model",
            Some("New threads on this profile start here."),
            picker(s, k, layer, Menu::Model, pickers::model_summary(ps)),
        ),
        keyed_setting_row(
            p,
            "row-approval",
            "Approvals",
            Some("Whether tool calls run on their own or wait for you."),
            picker(s, k, layer, Menu::Approval, pickers::approval_summary(ps)),
        ),
    ];
    let workspace = [
        folder_row(s, k),
        keyed_setting_row(
            p,
            "row-memory",
            "Supermemory container",
            Some("Long-term memory this profile reads and writes."),
            picker(
                s,
                k,
                layer,
                Menu::Memory,
                ps.picks.memory.clone().unwrap_or_else(|| "None".into()),
            ),
        ),
    ];
    let capabilities = [
        keyed_setting_row(
            p,
            "row-tools",
            "Tools",
            Some("Limit the tools this profile can call."),
            picker(
                s,
                k,
                layer,
                Menu::Tools,
                count_summary(ps.picks.tools.len(), "All tools", "tool", "allowed"),
            ),
        ),
        keyed_setting_row(
            p,
            "row-skills",
            "Skills",
            Some("Skills hidden from this profile."),
            picker(
                s,
                k,
                layer,
                Menu::Skills,
                count_summary(
                    ps.picks.hidden_skills.len(),
                    "None hidden",
                    "skill",
                    "hidden",
                ),
            ),
        ),
        keyed_setting_row(
            p,
            "row-mcp",
            "MCP servers",
            Some("Servers this profile can reach."),
            picker(
                s,
                k,
                layer,
                Menu::Mcp,
                count_summary(ps.picks.mcp.len(), "All MCP servers", "server", "enabled"),
            ),
        ),
    ];
    let soul = el()
        .key("profile-soul")
        .w_full()
        .padding(12.0)
        .radius(12.0)
        .fill(block_fill(p))
        .child(
            // Grows to the whole text, no inner scroll: a capped box under
            // the pointer took the page's wheel at its end, so the footer
            // could not be reached. The page scrolls, and follows the caret.
            TextArea::new(&ps.soul)
                .width(None)
                .rows(6)
                .animate_edit_layout()
                .placeholder("SOUL.md: who this profile is and how it works")
                .label("SOUL.md")
                .on_edit(|s: &mut State| &mut s.pages.profiles.soul)
                .build(k),
        );

    let can_delete = profile.is_some_and(|p| !p.is_default);
    let mut footer = row()
        .key("profile-footer")
        .w_full()
        .pt(24.0)
        .gap(8.0)
        .center_y();
    if can_delete {
        footer = footer.child(if ps.confirm_delete {
            press_button(
                k,
                "profile-delete-confirm",
                "Archive this profile?",
                Look::Danger,
                None,
                false,
                delete,
            )
        } else {
            press_button(
                k,
                "profile-delete",
                "Archive",
                Look::Ghost,
                None,
                false,
                |s: &mut State| s.pages.profiles.confirm_delete = true,
            )
        });
    }
    footer = footer.child(el().grow(1.0));
    if let Some(n) = &ps.notice {
        // "Saved." slides in beside the buttons when the save lands.
        footer = footer.child(
            styled(n.clone(), xs(), p.text_muted)
                .key("profile-notice")
                .transition(notice_slide()),
        );
    }
    footer = footer
        .child(press_button(
            k,
            "profile-cancel",
            if creating { "Cancel" } else { "Done" },
            Look::Ghost,
            None,
            false,
            back,
        ))
        .child(press_button(
            k,
            "profile-save",
            if creating { "Create profile" } else { "Save" },
            Look::Primary,
            None,
            ps.saving,
            save,
        ));

    col()
        .key("profile-editor")
        .w_full()
        .child(group_header(p, "General", None).pt(16.0).key("g-general"))
        .child(rows_card(p, general).key("c-general"))
        .child(group_header(p, "Model and access", None).key("g-model"))
        .child(rows_card(p, model).key("c-model"))
        .child(group_header(p, "Workspace", None).key("g-workspace"))
        .child(rows_card(p, workspace).key("c-workspace"))
        .child(
            group_header(
                p,
                "Capabilities",
                Some("Skills, keys and MCP servers are shared; a profile only narrows them."),
            )
            .key("g-capabilities"),
        )
        .child(rows_card(p, capabilities).key("c-capabilities"))
        .child(
            group_header(p, "SOUL.md", Some("Who this profile is and how it works.")).key("g-soul"),
        )
        .child(soul)
        .child(footer)
}

/// A footer notice arriving: a short slide from the right with a fade,
/// leaving faster.
fn notice_slide() -> rune::Transition {
    rune::Transition::offset(8., 0.)
        .combined(rune::Transition::opacity())
        .springs(rune::SpringPair::with_exit_ratio(motion::SLIDE, 0.6))
}

/// The detail view's top: back link, name, and a New thread button.
fn render_detail_header(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let profile = s.pages.profiles.selected_profile();
    let title = profile
        .map(|p| p.settings.display_name.clone())
        .unwrap_or_else(|| "New profile".into());
    let back_link = row()
        .key("profiles-back")
        .role("button")
        .label("Back to profiles")
        .gap(4.0)
        .px(8.0)
        .pb(10.0)
        .center_y()
        .cursor(Cursor::Pointer)
        // Hovered, the link brightens to the body ink (both glyph and
        // word, on the hover fade); pressed, it shrinks a hair.
        .hover_group()
        .hover_motion(motion::HOVER, motion::HOVER)
        .child(glyph(&icons::CHEVRON_LEFT, 12.0, p.text_muted).group_color(p.text))
        .child(styled("Profiles", xs(), p.text_muted).group_color(p.text))
        .on_press(back)
        .press_scale(motion::PRESS);
    let mut top = row().w_full().px(8.0).gap(12.0).center_y();
    if let Some(pi) = profile {
        top = top.child(avatar(&pi.profile_id, &pi.settings.display_name));
    }
    let mut titles = col()
        .grow(1.0)
        .min_w(0.0)
        .child(styled(title, page_title(), p.text).truncate());
    if let Some(pi) = profile {
        titles = titles.child(styled(
            format!(
                "{} · {} thread{}",
                pi.profile_id,
                pi.thread_count,
                if pi.thread_count == 1 { "" } else { "s" }
            ),
            xs(),
            p.text_muted,
        ));
    }
    top = top.child(titles);
    if let Some(pi) = profile {
        let id = pi.profile_id.clone();
        top = top.child(press_button(
            k,
            "profile-detail-thread",
            "New thread",
            Look::Outline,
            Some(&icons::PLUS),
            false,
            move |s: &mut State| new_thread(s, id.clone()),
        ));
    }
    col()
        .key("profile-detail-header")
        .w_full()
        .child(back_link)
        .child(top)
}

/// A profile's initials on a tint picked from its id, so each profile keeps
/// its color across launches.
fn avatar(id: &str, name: &str) -> El {
    let hue = id
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)))
        % 360;
    let tint = hsl(hue as f32 / 360., 0.5, 0.6);
    let initials: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    row()
        .w(30.0)
        .h(30.0)
        .radius(15.0)
        .center()
        .fill(tint.alpha(0.16))
        .child(styled(initials, xs_semibold(), tint))
        .a11y_hidden()
}

/// HSL (all 0..1) to an opaque colour, as GPUI's `hsla`.
fn hsl(h: f32, s: f32, l: f32) -> Color {
    let c = (1. - (2. * l - 1.).abs()) * s;
    let hp = h * 6.;
    let x = c * (1. - (hp % 2. - 1.).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.),
        1 => (x, c, 0.),
        2 => (0., c, x),
        3 => (0., x, c),
        4 => (x, 0., c),
        _ => (c, 0., x),
    };
    let m = l - c / 2.;
    let byte = |v: f32| ((v + m).clamp(0., 1.) * 255.).round() as u32;
    crate::design::rgb(byte(r) << 16 | byte(g) << 8 | byte(b))
}

pub(super) fn import_summary(report: &HermesImportResponse) -> String {
    let mut out = if report.imported.is_empty() {
        "Nothing new to import.".to_string()
    } else {
        format!(
            "Imported {}: {}.",
            report.imported.len(),
            report.imported.join(", ")
        )
    };
    let skipped: Vec<String> = report
        .skipped
        .iter()
        .filter(|s| s.reason != "excluded")
        .map(|s| format!("{} ({})", s.source, s.reason))
        .collect();
    if !skipped.is_empty() {
        out.push_str(&format!(" Skipped: {}.", skipped.join("; ")));
    }
    out
}

pub(super) fn page(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let ps = &s.pages.profiles;
    let body = if ps.open {
        let mut b = col().w_full().child(render_detail_header(s, k));
        if let Some(e) = &ps.error {
            b = b.child(el().key("profile-error").pt(12.0).child(error_line(p, e)));
        }
        let b = b.child(render_editor(s, k, layer));
        return drill::page(DRILL, Level::Detail, page_body("profiles-page", b));
    } else {
        let actions = row()
            .gap(4.0)
            .center_y()
            .child(press_button(
                k,
                "profiles-import",
                "Import from Hermes",
                Look::Ghost,
                None,
                ps.importing,
                import,
            ))
            .child(keyed_refresh(k, "refresh-profiles", refresh))
            .child(press_button(
                k,
                "profile-new",
                "New profile",
                Look::Primary,
                Some(&icons::PLUS),
                false,
                new_profile,
            ));
        let mut b = col()
            .w_full()
            .child(header_with(p, "Profiles", vec![actions]));
        if let Some(e) = &ps.error {
            b = b.child(error_line(p, e).key("profiles-error"));
        }
        if let Some(n) = &ps.notice {
            b = b.child(
                el().key("profiles-notice")
                    .transition(motion::reveal())
                    .pb(4.0)
                    .child(styled(n.clone(), sm(), p.text_muted).wrap()),
            );
        }
        if ps.loaded && ps.items.is_empty() {
            b = b.child(empty_state(p, "No profiles yet."));
        }
        b.child(render_list(s, k))
    };
    drill::page(DRILL, Level::List, page_body("profiles-page", body))
}
