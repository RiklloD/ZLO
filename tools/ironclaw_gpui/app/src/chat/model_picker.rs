//! The composer's model pill and its popover (t3code's picker): a rail of
//! providers on the left, search, then models grouped by provider. Any
//! provider the gateway's catalog lists can be picked, mid-thread too: the
//! backend switches in place, moves an unused thread, or continues in a fork
//! carrying the context (`POST /threads/{id}/route`).
//!
//! Without a thread (the new-thread screen) the pick is kept locally and the
//! catalog is read through any existing thread; the catalog is per user, so
//! every thread returns the same list.
//!
//! Like t3code, the user can star models (the star tab lists them) and hide
//! models or whole providers (hover icons on a row or a group title;
//! right-click a rail icon). Hidden providers sit as chips under the list,
//! one click shows them again. The preferences live in the backend settings,
//! written through the same route.
//!
//! Beside the model pill, a thinking pill picks the reasoning effort when the
//! model has one (t3code's "Reasoning" option): the levels come from the
//! catalog per model, and the pick rides the same route, per thread.

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::orchestration::{PickerPrefs, RouteChange, RouteProvider, ThreadRoute};

use crate::assets::ExtraIcon;
use crate::design::{self, Palette};
use crate::jobs;
use crate::ui::*;

/// IronClaw's own loop; the only provider with a "Default" (profile) model.
const NATIVE: &str = "ironclaw";
const POPOVER_WIDTH: f32 = 380.;
const POPOVER_LIST_HEIGHT: f32 = 320.;
/// Recent picks shown first in "All", after MonoCode's `pickers.rs`.
const RECENTS: usize = 6;

pub enum PickerEvent {
    /// The backend applied a pick. `fork_thread_id` set: open that thread.
    Changed(RouteChange),
    Failed(String),
}

impl EventEmitter<PickerEvent> for ModelPicker {}

/// The rail tab: starred models, every provider, or one provider.
#[derive(Clone, PartialEq)]
enum Rail {
    Favorites,
    All,
    Provider(String),
}

pub struct ModelPicker {
    backend: Backend,
    /// `None` on the new-thread screen.
    thread_id: Option<String>,
    /// The thread the catalog and the preferences are read and written through.
    catalog_thread: Option<String>,
    route: Option<ThreadRoute>,
    prefs: PickerPrefs,
    /// `(provider, model)` shown on the pill; `model` "default" is the
    /// profile's model.
    current: Option<(String, String)>,
    open: bool,
    rail: Rail,
    /// Show hidden providers and models (dimmed) so they can be restored.
    show_hidden: bool,
    /// The reasoning effort; `None`: the provider's default.
    effort: Option<String>,
    effort_open: bool,
    busy: bool,
    search: Entity<InputState>,
    _search: Subscription,
}

impl ModelPicker {
    /// `catalog_thread` is the thread the catalog is read through.
    pub fn new(
        backend: Backend,
        thread_id: Option<String>,
        catalog_thread: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search models"));
        let sub = cx.subscribe(&search, |_, _, event, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        });
        let catalog_thread = thread_id.clone().or(catalog_thread);
        let mut this = Self {
            backend,
            thread_id,
            catalog_thread: catalog_thread.clone(),
            route: None,
            prefs: PickerPrefs::default(),
            current: None,
            open: false,
            rail: Rail::All,
            show_hidden: false,
            effort: None,
            effort_open: false,
            busy: false,
            search,
            _search: sub,
        };
        if let Some(id) = catalog_thread {
            this.load(id, cx);
        }
        this
    }

    fn load(&mut self, thread_id: String, cx: &mut Context<Self>) {
        let own = self.thread_id.is_some();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.thread_route(&thread_id).await },
            move |this, result, cx| {
                // silent-ok: an older gateway has no route; the pill stays hidden.
                let Ok(route) = result else { return };
                if own {
                    this.current = route_pick(&route);
                    this.effort = route.effort.clone();
                }
                this.prefs = route.picker.clone();
                if !this.prefs.favorites.is_empty() {
                    this.rail = Rail::Favorites;
                }
                this.route = Some(route);
                cx.notify();
            },
        );
    }

    /// The local pick on the new-thread screen, to apply once it exists.
    pub fn pending(&self) -> Option<(String, String)> {
        self.current.clone()
    }

    /// The local effort pick on the new-thread screen.
    pub fn pending_effort(&self) -> Option<String> {
        self.effort.clone()
    }

    fn current_provider(&self) -> &str {
        self.current
            .as_ref()
            .map_or(NATIVE, |(provider, _)| provider)
    }

    /// The efforts `model` on `provider` takes; empty when it has no knob.
    fn efforts_of(&self, provider: &str, model: &str) -> &[String] {
        self.providers()
            .iter()
            .find(|p| p.provider == provider)
            .map_or(&[], |p| p.efforts_for(model))
    }

    fn pick_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        self.effort_open = false;
        let previous = std::mem::replace(&mut self.effort, effort.clone());
        cx.notify();
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let wire = effort.unwrap_or_else(|| "default".into());
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_thread_effort(&thread_id, &wire).await },
            move |this, result, cx| {
                if let Err(e) = result {
                    this.effort = previous;
                    cx.emit(PickerEvent::Failed(e.to_string()));
                    cx.notify();
                }
            },
        );
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.effort_open = false;
        self.open = !self.open;
        if self.open {
            self.search.update(cx, |s, cx| {
                s.set_value("", window, cx);
                s.focus(window, cx);
            });
        }
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open || self.effort_open {
            self.open = false;
            self.effort_open = false;
            cx.notify();
        }
    }

    fn pick(&mut self, provider: String, model: String, cx: &mut Context<Self>) {
        self.open = false;
        let recent = (provider.clone(), model.clone());
        crate::design::prefs::Prefs::update(cx, |p| {
            p.recent_models.retain(|r| *r != recent);
            p.recent_models.insert(0, recent);
            p.recent_models.truncate(RECENTS);
        });
        // The effort carries over when the new model takes the same level.
        let effort = self
            .effort
            .clone()
            .filter(|e| self.efforts_of(&provider, &model).contains(e));
        let Some(thread_id) = self.thread_id.clone() else {
            self.current = Some((provider, model));
            self.effort = effort;
            cx.notify();
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        let (p, m) = (provider.clone(), model.clone());
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                c.set_thread_route(&thread_id, &p, &m, effort.as_deref())
                    .await
            },
            move |this, result, cx| {
                this.busy = false;
                match result {
                    Ok(change) => {
                        if change.fork_thread_id.is_none() {
                            this.current = Some((provider, model));
                            this.effort = change.effort.clone();
                            if let Some(route) = this.route.as_mut() {
                                route.fresh = route.fresh && change.moved != "in_place";
                            }
                        }
                        cx.emit(PickerEvent::Changed(change));
                    }
                    Err(e) => cx.emit(PickerEvent::Failed(e.to_string())),
                }
                cx.notify();
            },
        );
    }

    fn providers(&self) -> &[RouteProvider] {
        self.route.as_ref().map_or(&[], |r| r.providers.as_slice())
    }

    /// Apply a preference change locally, then save the whole document.
    fn edit_prefs(&mut self, edit: impl FnOnce(&mut PickerPrefs), cx: &mut Context<Self>) {
        edit(&mut self.prefs);
        if self.prefs.favorites.is_empty() && self.rail == Rail::Favorites {
            self.rail = Rail::All;
        }
        cx.notify();
        let Some(thread_id) = self.catalog_thread.clone() else {
            return;
        };
        let prefs = self.prefs.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_model_picker(&thread_id, &prefs).await },
            |_, result, cx| {
                if let Err(e) = result {
                    cx.emit(PickerEvent::Failed(e.to_string()));
                }
            },
        );
    }

    /// Hidden providers plus hidden models, for the "Show hidden" toggle.
    fn hidden_count(&self) -> usize {
        self.prefs.hidden_providers.len() + self.prefs.hidden_models.len()
    }

    fn label(&self) -> (String, Option<String>) {
        match &self.current {
            Some((provider, model)) if model != "default" => {
                (pretty_model(model), Some(provider.clone()))
            }
            Some((provider, _)) => ("Default model".into(), Some(provider.clone())),
            None => ("Default model".into(), Some(NATIVE.into())),
        }
    }

    fn render_popover(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        let fresh = self.route.as_ref().is_none_or(|r| r.fresh) || self.thread_id.is_none();
        let current_runtime = self.route.as_ref().map(|r| r.runtime.clone());

        let rail_item = |id: ElementId, mark: AnyElement, on: bool, tip: String| {
            div()
                .id(id)
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(design::CONTROL_RADIUS))
                .cursor_pointer()
                .when(on, |d| d.bg(p.active))
                .hover(|s| s.bg(p.hover))
                .aria_label(tip)
                .child(mark)
        };
        let tab = |id: &'static str, mark: AnyElement, rail: Rail, tip: &str, this: &Self| {
            let on = this.rail == rail;
            rail_item(id.into(), mark, on, tip.into())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.rail = rail.clone();
                    cx.notify();
                }))
                .into_any_element()
        };
        let mut rail = vec![
            tab(
                "rail-favorites",
                Icon::new(IconName::Star).small().into_any_element(),
                Rail::Favorites,
                "Favorites",
                self,
            ),
            tab(
                "rail-all",
                Icon::new(ExtraIcon::LayoutGrid).small().into_any_element(),
                Rail::All,
                "All providers",
                self,
            ),
        ];
        for (ix, provider) in self.providers().iter().enumerate() {
            let hidden = self.prefs.provider_hidden(&provider.provider);
            if hidden && !self.show_hidden {
                continue;
            }
            let key = provider.provider.clone();
            let toggle_key = key.clone();
            let on = self.rail == Rail::Provider(key.clone());
            rail.push(
                rail_item(
                    ("rail", ix).into(),
                    provider_mark(&provider.runtime, &provider.provider, 18., cx)
                        .into_any_element(),
                    on,
                    format!(
                        "{} (right-click to {})",
                        provider.name,
                        if hidden { "show" } else { "hide" }
                    ),
                )
                .when(hidden, |d| d.opacity(0.4))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.rail = Rail::Provider(key.clone());
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, _, cx| this.toggle_provider(toggle_key.clone(), cx)),
                )
                .into_any_element(),
            );
        }

        // `(provider, model)` pairs to list, grouped under a header each.
        let matches = |provider: &RouteProvider, model: &str| {
            query.is_empty()
                || model.to_lowercase().contains(&query)
                || pretty_model(model).to_lowercase().contains(&query)
                || provider.name.to_lowercase().contains(&query)
        };
        // `None` provider: the favorites group, which has no hide button.
        type Group<'a> = (
            String,
            Option<&'a RouteProvider>,
            Vec<(&'a RouteProvider, String)>,
        );
        let mut groups: Vec<Group> = Vec::new();
        if self.rail == Rail::Favorites {
            let picks = self
                .prefs
                .favorites
                .iter()
                .filter_map(|f| {
                    let provider = self.providers().iter().find(|p| p.provider == f.provider)?;
                    matches(provider, &f.model).then(|| (provider, f.model.clone()))
                })
                .collect();
            groups.push(("Favorites".into(), None, picks));
        } else {
            if self.rail == Rail::All && query.is_empty() {
                let picks: Vec<_> = crate::design::prefs::Prefs::of(cx)
                    .recent_models
                    .iter()
                    .filter_map(|(provider, model)| {
                        let p = self.providers().iter().find(|p| p.provider == *provider)?;
                        (!self.prefs.provider_hidden(provider)
                            && !self.prefs.model_hidden(provider, model))
                        .then(|| (p, model.clone()))
                    })
                    .collect();
                groups.push(("Recent".into(), None, picks));
            }
            for provider in self.providers() {
                if let Rail::Provider(only) = &self.rail
                    && *only != provider.provider
                {
                    continue;
                }
                if self.prefs.provider_hidden(&provider.provider) && !self.show_hidden {
                    continue;
                }
                let mut models: Vec<String> = Vec::new();
                if provider.provider == NATIVE {
                    models.push("default".into());
                }
                models.extend(provider.models.iter().cloned());
                let picks = models
                    .into_iter()
                    .filter(|m| matches(provider, m))
                    .filter(|m| self.show_hidden || !self.prefs.model_hidden(&provider.provider, m))
                    .map(|m| (provider, m))
                    .collect();
                groups.push((provider.name.clone(), Some(provider), picks));
            }
        }

        let mut rows: Vec<AnyElement> = Vec::new();
        for (title, provider, picks) in groups {
            if picks.is_empty() {
                continue;
            }
            rows.push(self.group_header(title, provider, cx));
            for (provider, model) in picks {
                rows.push(self.model_row(provider, model, fresh, current_runtime.as_deref(), cx));
            }
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(p.text_faint)
                    .child(if self.route.is_none() {
                        "Loading models…"
                    } else if self.rail == Rail::Favorites {
                        "No favorites yet. Star a model to pin it here."
                    } else {
                        "No model matches."
                    })
                    .into_any_element(),
            );
        }
        let hidden = self.hidden_count();
        let hidden_providers = self.render_hidden_providers(cx);

        h_flex()
            .id("model-popover")
            .w(px(POPOVER_WIDTH))
            .items_start()
            .rounded(px(design::PANEL_RADIUS))
            .border_1()
            .border_color(p.border_strong)
            .bg(p.surface_overlay)
            .shadow_lg()
            .overflow_hidden()
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .child(
                v_flex()
                    .id("model-rail")
                    .flex_none()
                    .max_h(px(POPOVER_LIST_HEIGHT + 56.))
                    .overflow_y_scroll()
                    .p_1()
                    .gap_1()
                    .border_r_1()
                    .border_color(p.border)
                    .children(rail),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().p_2().child(Input::new(&self.search).small()))
                    .child(
                        v_flex()
                            .id("model-list")
                            .max_h(px(POPOVER_LIST_HEIGHT))
                            .overflow_y_scroll()
                            .px_1()
                            .pb_1()
                            .children(rows),
                    )
                    .children(hidden_providers)
                    .when(hidden > 0, |d| {
                        d.child(
                            div()
                                .id("model-show-hidden")
                                .px_3()
                                .py_1p5()
                                .border_t_1()
                                .border_color(p.border)
                                .text_xs()
                                .text_color(p.text_faint)
                                .cursor_pointer()
                                .hover(|s| s.text_color(p.text))
                                .child(if self.show_hidden {
                                    "Done showing hidden".to_string()
                                } else {
                                    format!("Show hidden ({hidden})")
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_hidden = !this.show_hidden;
                                    cx.notify();
                                })),
                        )
                    }),
            )
    }

    /// A provider group's title; hovering shows a button that hides (or,
    /// while hidden ones are shown, restores) the whole provider.
    fn group_header(
        &self,
        title: String,
        provider: Option<&RouteProvider>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let header = h_flex()
            .px_2()
            .pt_2()
            .pb_1()
            .gap_1()
            .items_center()
            .text_xs()
            .font_medium()
            .text_color(p.text_faint)
            .child(div().flex_1().truncate().child(title.to_uppercase()));
        let Some(provider) = provider else {
            return header.into_any_element();
        };
        let key = provider.provider.clone();
        let hidden = self.prefs.provider_hidden(&key);
        let group = SharedString::from(format!("pick-group-{key}"));
        header
            .id(SharedString::from(format!("group-{key}")))
            .group(group.clone())
            .child(
                div()
                    .when(!hidden, |d| {
                        d.invisible().group_hover(group, |s| s.visible())
                    })
                    .child(
                        Button::new(SharedString::from(format!("hide-provider-{key}")))
                            .icon(if hidden {
                                IconName::Eye
                            } else {
                                IconName::EyeOff
                            })
                            .ghost()
                            .xsmall()
                            .tooltip(if hidden {
                                "Show this provider"
                            } else {
                                "Hide this provider"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_provider(key.clone(), cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn toggle_provider(&mut self, key: String, cx: &mut Context<Self>) {
        if self.rail == Rail::Provider(key.clone()) {
            self.rail = Rail::All;
        }
        self.edit_prefs(|prefs| prefs.toggle_provider(&key), cx);
    }

    /// The hidden providers as chips; clicking one shows it again.
    fn render_hidden_providers(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = *Palette::of(cx);
        let hidden: Vec<&RouteProvider> = self
            .providers()
            .iter()
            .filter(|x| self.prefs.provider_hidden(&x.provider))
            .collect();
        if hidden.is_empty() {
            return None;
        }
        let chips = hidden.into_iter().map(|provider| {
            let key = provider.provider.clone();
            h_flex()
                .id(SharedString::from(format!("unhide-{key}")))
                .role(Role::Button)
                .aria_label(format!("Show {}", provider.name))
                .gap_1()
                .px_1p5()
                .py_0p5()
                .items_center()
                .rounded(px(design::CONTROL_RADIUS))
                .border_1()
                .border_color(p.border)
                .cursor_pointer()
                .hover(|s| s.bg(p.hover).text_color(p.text))
                .child(provider_mark(
                    &provider.runtime,
                    &provider.provider,
                    12.,
                    cx,
                ))
                .child(provider.name.clone())
                .child(Icon::new(IconName::Eye).xsmall())
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_provider(key.clone(), cx)))
        });
        Some(
            v_flex()
                .px_3()
                .py_1p5()
                .gap_1()
                .border_t_1()
                .border_color(p.border)
                .text_xs()
                .text_color(p.text_faint)
                .child("Hidden providers")
                .child(h_flex().flex_wrap().gap_1().children(chips))
                .into_any_element(),
        )
    }

    /// One pickable model, with hover actions to star and hide it.
    fn model_row(
        &self,
        provider: &RouteProvider,
        model: String,
        fresh: bool,
        current_runtime: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let on = self
            .current
            .as_ref()
            .is_some_and(|(cp, cm)| *cp == provider.provider && *cm == model);
        let favorite = self.prefs.favorite(&provider.provider, &model);
        let hidden = self.prefs.model_hidden(&provider.provider, &model);
        // Another runtime with history: the pick continues in a fork.
        let forks = !fresh && current_runtime != Some(provider.runtime.as_str());
        let title = if model == "default" {
            "Default (profile model)".to_string()
        } else {
            pretty_model(&model)
        };
        let id = format!("{}-{model}", provider.provider);
        let group = SharedString::from(format!("pick-row-{id}"));
        let (pv, md) = (provider.provider.clone(), model.clone());
        let (fav_p, fav_m) = (pv.clone(), md.clone());
        let (hide_p, hide_m) = (pv.clone(), md.clone());
        h_flex()
            .id(SharedString::from(format!("pick-{id}")))
            .group(group.clone())
            .role(Role::Button)
            .aria_label(title.clone())
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded(px(design::CONTROL_RADIUS))
            .cursor_pointer()
            .when(on, |d| d.bg(p.active))
            .when(hidden, |d| d.opacity(0.45))
            .hover(|s| s.bg(p.hover))
            .child(provider_mark(
                &provider.runtime,
                &provider.provider,
                16.,
                cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.text_faint)
                            .truncate()
                            .child(if forks {
                                format!("{} · continues in a new thread", provider.name)
                            } else if !provider.authenticated {
                                format!("{} · not signed in", provider.name)
                            } else {
                                provider.name.clone()
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .when(!favorite, |d| {
                        d.invisible().group_hover(group.clone(), |s| s.visible())
                    })
                    .child(
                        Button::new(SharedString::from(format!("fav-{id}")))
                            .icon(if favorite {
                                IconName::StarFill
                            } else {
                                IconName::Star
                            })
                            .ghost()
                            .xsmall()
                            .tooltip(if favorite { "Unstar" } else { "Star" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                let (p, m) = (fav_p.clone(), fav_m.clone());
                                this.edit_prefs(|prefs| prefs.toggle_favorite(&p, &m), cx);
                            })),
                    )
                    .child(
                        div()
                            .invisible()
                            .group_hover(group.clone(), |s| s.visible())
                            .child(
                                Button::new(SharedString::from(format!("hide-{id}")))
                                    .icon(if hidden {
                                        IconName::Eye
                                    } else {
                                        IconName::EyeOff
                                    })
                                    .ghost()
                                    .xsmall()
                                    .tooltip(if hidden { "Show" } else { "Hide" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        let (p, m) = (hide_p.clone(), hide_m.clone());
                                        this.edit_prefs(|prefs| prefs.toggle_model(&p, &m), cx);
                                    })),
                            ),
                    ),
            )
            .when(on, |d| d.child(Icon::new(IconName::Check).small()))
            .on_click(cx.listener(move |this, _, _, cx| this.pick(pv.clone(), md.clone(), cx)))
            .into_any_element()
    }
}

impl ModelPicker {
    fn render_effort_popover(
        &self,
        efforts: &[String],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = *Palette::of(cx);
        let row = |id: SharedString, title: String, value: Option<String>, this: &Self| {
            let on = this.effort == value;
            h_flex()
                .id(id)
                .role(Role::Button)
                .aria_label(title.clone())
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded(px(design::CONTROL_RADIUS))
                .cursor_pointer()
                .text_sm()
                .when(on, |d| d.bg(p.active))
                .hover(|s| s.bg(p.hover))
                .child(div().flex_1().child(title))
                .when(on, |d| d.child(Icon::new(IconName::Check).small()))
                .on_click(cx.listener(move |this, _, _, cx| this.pick_effort(value.clone(), cx)))
                .into_any_element()
        };
        let mut rows = vec![
            div()
                .px_2()
                .pt_1()
                .pb_1()
                .text_xs()
                .font_medium()
                .text_color(p.text_faint)
                .child("THINKING")
                .into_any_element(),
            row("effort-default".into(), "Default".into(), None, self),
        ];
        for effort in efforts {
            rows.push(row(
                SharedString::from(format!("effort-{effort}")),
                effort_label(effort),
                Some(effort.clone()),
                self,
            ));
        }
        v_flex()
            .id("effort-popover")
            .w(px(180.))
            .p_1()
            .rounded(px(design::PANEL_RADIUS))
            .border_1()
            .border_color(p.border_strong)
            .bg(p.surface_overlay)
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .children(rows)
    }

    /// The thinking pill, when the current model has levels.
    fn render_effort(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = self.current.as_ref().map_or("default", |(_, m)| m.as_str());
        let efforts = self.efforts_of(self.current_provider(), model).to_vec();
        if efforts.is_empty() {
            return None;
        }
        let p = *Palette::of(cx);
        let label = self
            .effort
            .as_deref()
            .map_or_else(|| "Default".to_string(), effort_label);
        let pill = h_flex()
            .id("effort-pill")
            .role(Role::Button)
            .aria_label(format!("Thinking: {label}"))
            .gap_1()
            .px_2()
            .h(px(28.))
            .items_center()
            .rounded(px(design::CONTROL_RADIUS))
            .text_sm()
            .text_color(p.text_muted)
            .cursor_pointer()
            .when(self.effort_open, |d| d.bg(p.active).text_color(p.text))
            .hover(|s| s.bg(p.hover).text_color(p.text))
            .child(Icon::new(ExtraIcon::Sparkles).xsmall())
            .child(label)
            .child(Icon::new(IconName::ChevronDown).xsmall())
            .on_click(cx.listener(|this, _, _, cx| {
                this.open = false;
                this.effort_open = !this.effort_open;
                cx.notify();
            }));
        Some(
            div()
                .relative()
                .child(pill)
                .when(self.effort_open, |d| {
                    d.child(gpui_kit::deferred(
                        div()
                            .absolute()
                            .bottom(px(34.))
                            .left_0()
                            .child(self.render_effort_popover(&efforts, cx)),
                    ))
                })
                .into_any_element(),
        )
    }
}

impl Render for ModelPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        if self.route.as_ref().is_none_or(|r| r.providers.is_empty()) && self.current.is_none() {
            return div().into_any_element();
        }
        let (label, provider) = self.label();
        let runtime = self
            .providers()
            .iter()
            .find(|x| Some(&x.provider) == provider.as_ref())
            .map(|x| x.runtime.clone())
            .unwrap_or_default();
        let pill = h_flex()
            .id("model-pill")
            .role(Role::Button)
            .aria_label(format!("Model: {label}"))
            .gap_1p5()
            .px_2()
            .h(px(28.))
            .items_center()
            .rounded(px(design::CONTROL_RADIUS))
            .text_sm()
            .text_color(p.text_muted)
            .cursor_pointer()
            .when(self.open, |d| d.bg(p.active).text_color(p.text))
            .hover(|s| s.bg(p.hover).text_color(p.text))
            .child(provider_mark(
                &runtime,
                provider.as_deref().unwrap_or(NATIVE),
                14.,
                cx,
            ))
            .child(label)
            .when(self.busy, |d| d.child(Spinner::new().xsmall()))
            .when(!self.busy, |d| {
                d.child(Icon::new(IconName::ChevronDown).xsmall())
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx)));
        let model = div().relative().child(pill).when(self.open, |d| {
            d.child(gpui_kit::deferred(
                div()
                    .absolute()
                    .bottom(px(34.))
                    .left_0()
                    .child(self.render_popover(cx)),
            ))
        });
        h_flex()
            .gap_0p5()
            .items_center()
            .child(model)
            .children(self.render_effort(cx))
            .into_any_element()
    }
}

/// The thread's pick as `(provider, model)`; no model is the default.
fn route_pick(route: &ThreadRoute) -> Option<(String, String)> {
    let provider = route.provider.clone().unwrap_or_else(|| NATIVE.into());
    Some((
        provider,
        route.model.clone().unwrap_or_else(|| "default".into()),
    ))
}

/// "xhigh" -> "Extra High", as t3code names the levels.
fn effort_label(effort: &str) -> String {
    match effort {
        "xhigh" => "Extra High".into(),
        other => pretty_model(other),
    }
}

/// "claude-opus-5-5" -> "Claude Opus 5.5", "gpt-6.1-sol" -> "GPT 6.1 Sol".
pub fn pretty_model(id: &str) -> String {
    let tail = id.rsplit('/').next().unwrap_or(id);
    let mut out: Vec<String> = Vec::new();
    for part in tail.split(['-', '_']) {
        if part.is_empty() {
            continue;
        }
        let numeric = part.chars().all(|c| c.is_ascii_digit() || c == '.');
        match out.last_mut() {
            Some(last) if numeric && last.chars().all(|c| c.is_ascii_digit() || c == '.') => {
                last.push('.');
                last.push_str(part);
            }
            _ if part.eq_ignore_ascii_case("gpt") => out.push("GPT".into()),
            _ => {
                let mut chars = part.chars();
                let first = chars.next().map(|c| c.to_ascii_uppercase());
                out.push(first.into_iter().chain(chars).collect());
            }
        }
    }
    if out.is_empty() {
        id.to_string()
    } else {
        out.join(" ")
    }
}

/// The provider's logo (t3code and lobe-icons), falling back to a letter.
pub fn provider_mark(runtime: &str, provider: &str, size: f32, cx: &App) -> impl IntoElement {
    crate::design::brands::provider_logo(&[provider, runtime], size, cx)
}

#[cfg(test)]
mod tests {
    use super::{effort_label, pretty_model};

    #[test]
    fn effort_levels_read_like_t3code() {
        assert_eq!(effort_label("xhigh"), "Extra High");
        assert_eq!(effort_label("minimal"), "Minimal");
        assert_eq!(effort_label("max"), "Max");
    }

    #[test]
    fn model_ids_read_like_names() {
        assert_eq!(pretty_model("claude-opus-5-5"), "Claude Opus 5.5");
        assert_eq!(pretty_model("gpt-6.1-sol"), "GPT 6.1 Sol");
        assert_eq!(
            pretty_model("anthropic/claude-haiku-5-5"),
            "Claude Haiku 5.5"
        );
    }
}
