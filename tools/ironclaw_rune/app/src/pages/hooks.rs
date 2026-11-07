//! Hooks: the hooks a run would install: name, where it comes from (which
//! fixes its trust class), the hook points it attaches to, and whether it
//! is live. Read-only — the framework switch is boot configuration and
//! per-hook enablement has no backend yet. Port of GPUI `pages/hooks.rs`.

use crate::pages::common::controls::{header_with, outlined_tag, refresh_icon, rule, spinner};
use crate::pages::common::ts::{SM, SM_MEDIUM, XS};
use crate::pages::common::{Tone, empty_state, error_line, page_body};
use ironclaw_gpui_client::hooks_catalog::{
    HookCatalog, HookCatalogEntry, HookSource, HookTrustClassView,
};
use rune::{Cx, El, col, row, text};
use rune_kit::Kit;

use crate::jobs;
use crate::state::{Conn, Page, State};

#[derive(Default)]
pub struct HooksState {
    pub catalog: Option<HookCatalog>,
    pub error: Option<String>,
}

fn source_label(source: HookSource) -> &'static str {
    match source {
        HookSource::Builtin => "Built-in",
        HookSource::Extension => "Extension",
        HookSource::Skill => "Skill",
        HookSource::User => "User",
    }
}

fn trust_label(trust: HookTrustClassView) -> &'static str {
    match trust {
        HookTrustClassView::Builtin => "builtin trust",
        HookTrustClassView::Trusted => "trusted",
        HookTrustClassView::Installed => "installed (restricted)",
        HookTrustClassView::SelfAuthored => "self-authored (observe only)",
    }
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "hooks",
        |c| async move { c.hooks_catalog().await },
        |s, result, _| {
            let h = &mut s.pages.hooks;
            match result {
                Ok(catalog) => {
                    h.catalog = Some(catalog);
                    h.error = None;
                }
                Err(e) => h.error = Some(e.to_string()),
            }
        },
    );
}

fn render_hook(s: &State, k: &Kit, hook: &HookCatalogEntry) -> El {
    let p = &s.palette;
    let _ = k;
    let meta = format!(
        "{} · {} · {}{}",
        source_label(hook.source),
        trust_label(hook.trust_class),
        if hook.points.is_empty() {
            "no hook point".to_string()
        } else {
            hook.points.join(", ")
        },
        hook.owner
            .as_deref()
            .map(|owner| format!(" · {owner}"))
            .unwrap_or_default()
    );
    let mut info = col()
        .grow(1.)
        .min_w(0.)
        .child(text(hook.name.clone()).font(SM_MEDIUM).color(p.text))
        .child(text(hook.id.clone()).font(XS).color(p.text_muted))
        .child(text(meta).font(XS).color(p.text_muted).wrap());
    if let Some(note) = &hook.note {
        info = info.child(text(note.clone()).font(XS).color(p.text_muted).wrap());
    }
    col()
        .key(format!("hook-{}", hook.id))
        .w_full()
        .child(
            row()
                .w_full()
                .gap(12.)
                .py(8.)
                .child(info)
                .child(if hook.enabled {
                    outlined_tag(p, Tone::Success, "On")
                } else {
                    outlined_tag(p, Tone::Secondary, "Off")
                }),
        )
        .child(rule(p))
}

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let h = &s.pages.hooks;
    let actions = vec![
        refresh_icon()
            .on_press(|s: &mut State| refresh(s))
            .build(k)
            .key("refresh-hooks"),
    ];
    let content = match &h.catalog {
        None => spinner(k),
        Some(catalog) => {
            let on_off = |b: bool| if b { "on" } else { "off" };
            let mut c = col().w_full().child(
                text(format!(
                    "Hook framework {} (HOOKS_ENABLED); third-party hooks {} \
                     (HOOKS_THIRD_PARTY_ENABLED). Read-only: these switches are boot settings.",
                    on_off(catalog.framework_enabled),
                    on_off(catalog.third_party_enabled),
                ))
                .font(SM)
                .color(p.text_muted)
                .wrap()
                .pb(12.),
            );
            if catalog.hooks.is_empty() {
                c = c.child(empty_state(p, "No hooks are installed."));
            }
            c.children(catalog.hooks.iter().map(|hook| render_hook(s, k, hook)))
        }
    };
    let mut body = col().w_full().child(header_with(p, "Hooks", actions));
    if let Some(e) = &h.error {
        body = body.child(error_line(p, e));
    }
    page_body("hooks-page", body.child(content))
}

/// `--scene hooks`: the framework on, three hooks, one off.
pub fn fixture_hooks(s: &mut State) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Hooks;
    let entry =
        |id: &str, name: &str, source, trust, points: &[&str], enabled, note: Option<&str>| {
            HookCatalogEntry {
                id: id.into(),
                name: name.into(),
                source,
                trust_class: trust,
                owner: (source == HookSource::Extension).then(|| "github".to_string()),
                points: points.iter().map(|x| x.to_string()).collect(),
                enabled,
                note: note.map(Into::into),
            }
        };
    s.pages.hooks.catalog = Some(HookCatalog {
        framework_enabled: true,
        third_party_enabled: false,
        toggle_supported: false,
        hooks: vec![
            entry(
                "memory.curation.after_turn",
                "Memory curation",
                HookSource::Builtin,
                HookTrustClassView::Builtin,
                &["after_turn"],
                true,
                Some("Keeps the memory index in step with what the agent learned."),
            ),
            entry(
                "github.pr_guard",
                "PR guard",
                HookSource::Extension,
                HookTrustClassView::Installed,
                &["before_capability"],
                false,
                Some("Off: third-party hooks are disabled (HOOKS_THIRD_PARTY_ENABLED)."),
            ),
            entry(
                "user.audit_log",
                "Audit log",
                HookSource::User,
                HookTrustClassView::SelfAuthored,
                &[],
                true,
                None,
            ),
        ],
    });
}
