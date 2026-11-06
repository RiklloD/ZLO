//! The hooks a run would install: name, where it comes from (which fixes its
//! trust class), the hook points it attaches to, and whether it is live.
//! Read-only — the framework switch is boot configuration and per-hook
//! enablement has no backend yet.

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::hooks_catalog::{
    HookCatalog, HookCatalogEntry, HookSource, HookTrustClassView,
};

use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

pub struct HooksView {
    backend: Backend,
    catalog: Option<HookCatalog>,
    error: Option<SharedString>,
}

impl EventEmitter<PageEvent> for HooksView {}

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

impl HooksView {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            catalog: None,
            error: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.hooks_catalog().await },
            |this, result, cx| {
                match result {
                    Ok(catalog) => {
                        this.catalog = Some(catalog);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn render_hook(&self, hook: &HookCatalogEntry, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
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
        h_flex()
            .gap_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().font_medium().child(hook.name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(hook.id.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .whitespace_normal()
                            .child(meta),
                    )
                    .when_some(hook.note.clone(), |d, note| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .whitespace_normal()
                                .child(note),
                        )
                    }),
            )
            .child(if hook.enabled {
                Tag::success().small().child("On").into_any_element()
            } else {
                Tag::secondary().small().child("Off").into_any_element()
            })
            .into_any_element()
    }
}

impl Render for HooksView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let actions = refresh_button("refresh-hooks")
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let content = match &self.catalog {
            None => Spinner::new().into_any_element(),
            Some(catalog) => v_flex()
                .child(
                    div()
                        .pb_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .whitespace_normal()
                        .child(format!(
                            "Hook framework {} (HOOKS_ENABLED); third-party hooks {} \
                             (HOOKS_THIRD_PARTY_ENABLED). Read-only: these switches are boot \
                             settings.",
                            if catalog.framework_enabled {
                                "on"
                            } else {
                                "off"
                            },
                            if catalog.third_party_enabled {
                                "on"
                            } else {
                                "off"
                            },
                        )),
                )
                .when(catalog.hooks.is_empty(), |d| {
                    d.child(empty_state("No hooks are installed.", cx))
                })
                .children(catalog.hooks.iter().map(|hook| self.render_hook(hook, cx)))
                .into_any_element(),
        };
        let body = v_flex()
            .child(header("Hooks", actions))
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .child(content);
        page_body("hooks-page", body)
    }
}
