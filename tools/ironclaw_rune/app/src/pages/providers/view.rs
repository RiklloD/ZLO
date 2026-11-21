//! Providers page rendering (GPUI `ProvidersView`'s `render_*` methods).

use crate::pages::common::controls::{
    Look, Size, button_sized, header_with, icon_button_sized, input, muted, open_url, outlined_tag,
    refresh_icon, rule, section_title, spinner, spinner_line,
};
use crate::pages::common::ts::{MONO_XS, body_semibold, sm, sm_medium, xs, xs_medium};
use crate::pages::common::{Tone, card, empty_state, error_line, loading, notice_line, page_body};
use rune::{Cx, El, col, el, row, text};
use rune_kit::Kit;
use zlo_client::operator_llm::{LlmConfigSnapshot, LlmProviderView};
use zlo_client::orchestration::RouteProvider;
use zlo_client::task_models::TaskModelChoice;

use super::catalog::{Kind, display_name, kind_of};
use super::{CODEX_ADAPTER, Models, ProvidersState, Tab, connected, task_choices};
use crate::design::brands::provider_logo;
use crate::design::kit::{self, Turn};
use crate::design::motion;
use crate::icons;
use crate::pages::common::pressable;
use crate::state::State;

pub(super) const LOGO: f32 = 22.;
const TILE_LOGO: f32 = 20.;

pub fn page(s: &State, cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let actions = vec![
        refresh_icon()
            .on_press(|s: &mut State| super::refresh(s))
            .build(k)
            .key("refresh-providers"),
    ];
    let connected_rows: Vec<El> = ps
        .llm
        .as_ref()
        .map(|llm| {
            connected(llm)
                .into_iter()
                .map(|(_, provider)| render_provider(s, k, provider))
                .collect()
        })
        .unwrap_or_default();
    let mut body = col().w_full().child(header_with(p, "Providers", actions));
    if let Some(e) = &ps.error {
        body = body.child(error_line(p, e));
    }
    if let Some(n) = &ps.notice {
        body = body.child(notice_line(n, p.success));
    }
    body = body.child(section_title(p, "Connected", 24.));
    body = body.child(if ps.llm.is_none() {
        spinner(k)
    } else if connected_rows.is_empty() {
        empty_state(p, "Nothing connected yet. Add a provider below.")
    } else {
        col()
            .key("providers-connected")
            .w_full()
            .gap(12.)
            .children(connected_rows)
    });
    if ps.task_models.is_some() {
        body = body
            .child(section_title(p, "Background tasks", 24.))
            .child(render_task_models(s, k));
    }
    body = body
        .child(section_title(p, "Add a provider", 24.))
        .child(render_tabs(s, k))
        .child(col().key(ADD_KEY).w_full().pt(12.).child(match ps.tab {
            Tab::Accounts => tab_panel("accounts", render_accounts(s, k, tile_columns(cx))),
            Tab::Cli => tab_panel("cli", super::cli::render_cli(s, k)),
        }));
    page_body("providers-page", body)
}

/// One tab's content under "Add a provider", keyed by tab so switching
/// tabs crossfades the two instead of swapping them in one frame.
fn tab_panel(tab: &str, content: El) -> El {
    col()
        .key(format!("providers-tab-{tab}"))
        .w_full()
        .transition(motion::fade())
        .child(content)
}

fn render_task_models(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let current = ps
        .task_models
        .as_ref()
        .and_then(|m| m.thread_titles.clone());
    let label = current.as_ref().map_or_else(
        || "Off".to_string(),
        |c| format!("{} · {}", display_name(&c.provider_id), c.model),
    );
    let mut card = card(p).key("task-models").child(
        row()
            .w_full()
            .gap(12.)
            .center_y()
            .child(
                col()
                    .grow(1.)
                    .min_w(0.)
                    .child(text("Thread titles").font(body_semibold()).color(p.text))
                    .child(muted(
                        p,
                        "Names a thread after its first reply. One model for every profile.",
                    )),
            )
            .child(title_trigger(s, k, label)),
    );
    if ps.titles_open {
        let off = crate::pages::common::choice_look(current.is_none());
        let mut list = col()
            .key("title-models")
            .w_full()
            .gap(8.)
            .pt(8.)
            .transition(motion::reveal())
            .child(
                row().child(pressable(
                    button_sized("Off", off, Size::Small)
                        .on_press(|s: &mut State| super::set_title_model(s, None))
                        .build(k)
                        .key("title-model-off"),
                )),
            );
        for (pix, (provider, name, models)) in task_choices(ps).into_iter().enumerate() {
            let buttons = models.into_iter().enumerate().map(|(mix, model)| {
                let selected = current
                    .as_ref()
                    .is_some_and(|c| c.provider_id == provider && c.model == model);
                let choice = TaskModelChoice {
                    provider_id: provider.clone(),
                    model: model.clone(),
                };
                pressable(
                    button_sized(
                        model,
                        crate::pages::common::choice_look(selected),
                        Size::Small,
                    )
                    .on_press(move |s: &mut State| super::set_title_model(s, Some(choice.clone())))
                    .build(k),
                )
                .key(format!("title-model-{pix}-{mix}"))
            });
            list = list.child(
                col()
                    .w_full()
                    .gap(4.)
                    .child(text(name).font(xs()).color(p.text_muted))
                    .child(row().w_full().flex_wrap().gap(4.).children(buttons)),
            );
        }
        card = card.child(list);
    }
    card
}

/// The thread-title model's button: its chevron turns over as the list
/// opens and shuts (`kit::chevron`) instead of swapping glyphs. Drawn as the
/// kit's outline button with its icon slot carrying the turning chevron, so
/// at rest it is the same button.
fn title_trigger(s: &State, k: &Kit, label: String) -> El {
    let open = s.pages.providers.titles_open;
    pressable(
        button_sized(label.clone(), Look::Outline, Size::Small)
            .expanded(open)
            .label_with(move |font, ink| {
                row()
                    .gap(6.)
                    .center_y()
                    .child(kit::chevron("title-model-chev", open, Turn::Flip, 16., ink))
                    .child(text(label).key("label").font(font).color(ink))
            })
            .on_press(|s: &mut State| {
                let p = &mut s.pages.providers;
                p.titles_open = !p.titles_open;
            })
            .build(k),
    )
    .key("title-model")
}

/// The narrowest a connected provider's logo and name get before its
/// status and actions wrap below them, at the 16 pt UI font.
const IDENTITY_MIN: f32 = 200.;

fn render_provider(s: &State, k: &Kit, pv: &LlmProviderView) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let ix = pv.id.as_str();
    let usable = pv.api_key_set || !pv.api_key_required;
    let editing = ps.key_editor.as_deref() == Some(pv.id.as_str());
    let status = if pv.active {
        outlined_tag(
            p,
            Tone::Success,
            format!(
                "Active · {}",
                pv.active_model
                    .clone()
                    .unwrap_or_else(|| pv.default_model.clone())
            ),
        )
    } else if pv.api_key_set {
        outlined_tag(p, Tone::Secondary, "Key saved")
    } else if pv.api_key_required {
        outlined_tag(p, Tone::Warning, "Needs key")
    } else {
        outlined_tag(p, Tone::Secondary, "No key needed")
    };

    let mut actions = row().gap(4.).center_y();
    if pv.can_list_models && usable {
        let provider = pv.clone();
        actions = actions.child(
            button_sized("Models", Look::Ghost, Size::Small)
                .on_press(move |s: &mut State| super::toggle_models(s, provider.clone()))
                .build(k)
                .key(format!("models-{ix}"))
                .press_scale(motion::PRESS),
        );
    }
    if usable && pv.adapter != CODEX_ADAPTER {
        let provider = pv.clone();
        let running = ps.probes.get(&pv.id).is_some_and(Option::is_none);
        actions = actions.child(
            loading(
                button_sized("Test", Look::Ghost, Size::Small)
                    .on_press(move |s: &mut State| super::test_connection(s, provider.clone())),
                running,
            )
            .build(k)
            .key(format!("test-{ix}"))
            .press_scale(motion::PRESS),
        );
    }
    if pv.adapter == CODEX_ADAPTER && !pv.active {
        let id = pv.id.clone();
        actions = actions.child(
            button_sized("Sign in with ChatGPT", Look::Outline, Size::Small)
                .disabled(ps.codex.is_some())
                .on_press(move |s: &mut State| super::start_codex(s, id.clone()))
                .build(k)
                .key(format!("codex-login-{ix}"))
                .press_scale(motion::PRESS),
        );
    }
    if pv.accepts_api_key {
        let id = pv.id.clone();
        actions = actions.child(
            button_sized(
                if pv.api_key_set {
                    "Replace key"
                } else {
                    "Add key"
                },
                Look::Ghost,
                Size::Small,
            )
            .on_press(move |s: &mut State| super::open_key_editor(s, id.clone()))
            .build(k)
            .key(format!("set-key-{ix}"))
            .press_scale(motion::PRESS),
        );
    }
    // Remove sits last. A built-in stays in the catalog: removing it drops
    // its stored key and settings, and it returns to the list below.
    if pv.active || pv.api_key_set || !pv.builtin {
        let id = pv.id.clone();
        let help = if pv.active {
            "Switch to another provider before removing this one"
        } else {
            "Remove provider"
        };
        actions = actions.child(
            icon_button_sized(&icons::CLOSE, help, Size::XSmall)
                .disabled(pv.active)
                .on_press(move |s: &mut State| s.pages.providers.confirm_delete = Some(id.clone()))
                .build(k)
                .key(format!("delete-provider-{ix}"))
                .press_scale(motion::PRESS),
        );
    }

    // The name keeps at least IDENTITY_MIN; when the status and actions do
    // not fit beside it (a narrow window, a large UI font) they wrap under
    // it instead of drawing over the name.
    let identity = row()
        .w(0.)
        .grow(1.)
        .min_w(IDENTITY_MIN * crate::design::styles::type_scale())
        .gap(12.)
        .center_y()
        .child(provider_logo(
            s,
            &[pv.id.as_str(), pv.adapter.as_str()],
            LOGO,
        ))
        .child(
            col()
                .grow(1.)
                .min_w(0.)
                .child(
                    text(display_name(&pv.id))
                        .font(body_semibold())
                        .color(p.text)
                        .truncate(),
                )
                .child(
                    text(pv.description.clone())
                        .font(xs())
                        .color(p.text_muted)
                        .truncate(),
                ),
        );
    // Keyed by id: when the active provider moves to the top, or a provider
    // joins the connected list after the person's own action, the cards
    // glide to their new places (`ROW_RESORT`) and a new one wipes in.
    let mut card = card(p)
        .key(format!("provider-{}", pv.id))
        .motion(motion::ROW_RESORT)
        .transition(motion::reveal())
        .child(
            row()
                .w_full()
                .flex_wrap()
                .gap(12.)
                .center_y()
                .child(identity)
                .child(row().gap(8.).center_y().child(status).child(actions)),
        );

    if let Some(models) = ps.models.get(&pv.id) {
        let content = match models {
            Models::Loading => spinner_line(p, k, "Loading models").key("loading"),
            Models::Failed(e) => error_line(p, e).key("failed"),
            Models::Loaded(list) => {
                let current = pv.active.then(|| pv.active_model.clone()).flatten();
                row().key("list").w_full().flex_wrap().gap(4.).children(
                    list.iter().enumerate().map(|(mix, model)| {
                        let selected = current.as_deref() == Some(model.as_str());
                        let (id, m) = (pv.id.clone(), model.clone());
                        pressable(
                            button_sized(
                                model.clone(),
                                crate::pages::common::choice_look(selected),
                                Size::Small,
                            )
                            .on_press(move |s: &mut State| {
                                super::set_active(s, id.clone(), Some(m.clone()))
                            })
                            .build(k),
                        )
                        .key(format!("model-{mix}"))
                    }),
                )
            }
        };
        // The list opens under the card's head; the spinner gives way to
        // the models (or the error) in place.
        card = card.child(
            col()
                .key("models")
                .w_full()
                .transition(motion::reveal())
                .child(content.transition(motion::fade())),
        );
    }
    if let Some(Some((ok, msg))) = ps.probes.get(&pv.id) {
        card = card.child(
            row()
                .key("probe")
                .transition(motion::reveal())
                .w_full()
                .gap(8.)
                .center_y()
                .child(if *ok {
                    outlined_tag(p, Tone::Success, "Connected")
                } else {
                    outlined_tag(p, Tone::Danger, "Failed")
                })
                .child(text(msg.clone()).font(xs()).color(p.text).wrap().grow(1.)),
        );
    }
    if ps.confirm_delete.as_deref() == Some(pv.id.as_str()) {
        let id = pv.id.clone();
        card = card.child(
            row()
                .key("confirm-delete")
                .transition(motion::reveal())
                .w_full()
                .gap(8.)
                .center_y()
                .child(
                    text(format!(
                        "Remove {} and its stored API key?",
                        display_name(&pv.id)
                    ))
                    .font(sm())
                    .color(p.text)
                    .wrap()
                    .grow(1.),
                )
                .child(
                    button_sized("Remove", Look::Danger, Size::Small)
                        .on_press(move |s: &mut State| super::delete_provider(s, id.clone()))
                        .build(k)
                        .key(format!("confirm-delete-{ix}"))
                        .press_scale(motion::PRESS),
                )
                .child(
                    button_sized("Cancel", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| s.pages.providers.confirm_delete = None)
                        .build(k)
                        .key(format!("cancel-delete-{ix}"))
                        .press_scale(motion::PRESS),
                ),
        );
    }
    if let Some(login) = ps.codex.as_ref().filter(|l| l.provider_id == pv.id) {
        let uri = login.uri.clone();
        card = card.child(
            col()
                .key("device-login")
                .transition(motion::reveal())
                .w_full()
                .gap(8.)
                .child(rule(p))
                .child(col().w_full().gap(6.).child(muted(
                    p,
                    if login.code.is_empty() {
                        "Finish signing in on the ChatGPT page that just opened. Allow IronClaw to use your plan."
                    } else {
                        "Enter this code on the ChatGPT page that just opened:"
                    },
                ))
                .child(
                    row()
                        .gap(8.)
                        .center_y()
                        .child(
                            text(login.code.clone())
                                .font(rune::TextStyle::new(18., 600.).mono())
                                .color(p.text)
                                .selectable(true),
                        )
                        .child(crate::design::kit::Spinner::new().size(12.).build(k))
                        .child(
                            button_sized("Open page", Look::Ghost, Size::Small)
                                .on_press_tx(move |_: &mut State, tx| open_url(tx, &uri))
                                .build(k)
                                .key(format!("codex-open-{ix}"))
                                .press_scale(motion::PRESS),
                        )
                        .child(
                            button_sized("Cancel", Look::Ghost, Size::Small)
                                .on_press(|s: &mut State| s.pages.providers.codex = None)
                                .build(k)
                                .key(format!("codex-cancel-{ix}"))
                                .press_scale(motion::PRESS),
                        ),
                )),
        );
    }
    if pv.active {
        card = card.child(render_quick_switch(s, k, pv));
    }
    if let Some(route) = catalog_row(ps, pv) {
        card = card.child(render_picker_models(s, k, route));
    }
    if editing {
        let provider = pv.clone();
        card = card.child(
            row()
                .key("key-editor")
                .transition(motion::reveal())
                .w_full()
                .gap(8.)
                .center_y()
                .child(
                    col().grow(1.).min_w(0.).child(
                        input(&ps.key_input)
                            .secure()
                            .placeholder("Paste the new API key")
                            .label("API key")
                            .on_edit(|s: &mut State| &mut s.pages.providers.key_input)
                            .build(k)
                            .key(format!("key-input-{ix}")),
                    ),
                )
                .child(
                    button_sized("Save", Look::Primary, Size::Small)
                        .on_press(move |s: &mut State| super::save_key(s, provider.clone()))
                        .build(k)
                        .key(format!("save-key-{ix}"))
                        .press_scale(motion::PRESS),
                )
                .child(
                    button_sized("Cancel", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| s.pages.providers.key_editor = None)
                        .build(k)
                        .key(format!("cancel-key-{ix}"))
                        .press_scale(motion::PRESS),
                ),
        );
    }
    card
}

/// A connected provider's row in the model picker's catalog: the active
/// provider runs as IronClaw's own loop (the `ironclaw` row), the others
/// under their own id.
fn catalog_row<'a>(ps: &'a ProvidersState, pv: &LlmProviderView) -> Option<&'a RouteProvider> {
    let id = if pv.active {
        crate::chat::model_picker::NATIVE
    } else {
        pv.id.as_str()
    };
    ps.runtimes
        .as_ref()?
        .iter()
        .find(|r| r.provider == id && !r.models.is_empty())
}

/// Which of a provider's models the chat's model picker offers: a folded
/// line ("Models in the picker · 7 of 12"), open, one switch per model and
/// a show-all / hide-all pair. Needs the picker preferences.
pub(super) fn render_picker_models(s: &State, k: &Kit, route: &RouteProvider) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let Some(prefs) = ps.picker.as_ref() else {
        return el();
    };
    let id = route.provider.clone();
    let shown = route
        .models
        .iter()
        .filter(|m| !prefs.model_hidden(&id, m))
        .count();
    let open = ps.picker_open.contains(&id);
    let toggle = id.clone();
    let head = row()
        .key(format!("picker-models-{id}"))
        .w_full()
        .gap(8.)
        .center_y()
        .role("button")
        .label("Models in the picker")
        .expanded(open)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut State| super::toggle_picker_list(s, toggle.clone()))
        .child(text("Models in the picker").font(xs_medium()).color(p.text))
        .child(
            text(format!("{shown} of {}", route.models.len()))
                .font(xs())
                .color(p.text_muted)
                .grow(1.),
        )
        .child(kit::chevron(
            &format!("picker-models-{id}-chev"),
            open,
            Turn::Flip,
            12.,
            p.text_muted,
        ));
    let c = col().w_full().gap(6.).child(rule(p)).child(head);
    if !open {
        return c;
    }
    let (all_on, all_off) = (
        (id.clone(), route.models.clone()),
        (id.clone(), route.models.clone()),
    );
    // The open list wipes in under its line and back out.
    let mut body = col()
        .key(format!("picker-models-{id}-list"))
        .w_full()
        .gap(6.)
        .transition(motion::reveal());
    body = body.child(
        row()
            .gap(4.)
            .child(
                button_sized("Show all", Look::Ghost, Size::XSmall)
                    .disabled(shown == route.models.len())
                    .on_press(move |s: &mut State| {
                        super::set_all_shown(s, all_on.0.clone(), all_on.1.clone(), true)
                    })
                    .build(k)
                    .key(format!("picker-all-{id}")),
            )
            .child(
                button_sized("Hide all", Look::Ghost, Size::XSmall)
                    .disabled(shown == 0)
                    .on_press(move |s: &mut State| {
                        super::set_all_shown(s, all_off.0.clone(), all_off.1.clone(), false)
                    })
                    .build(k)
                    .key(format!("picker-none-{id}")),
            ),
    );
    let mut list = col().w_full();
    for model in &route.models {
        let on = !prefs.model_hidden(&id, model);
        let name = crate::chat::model_picker::pretty_model(model);
        let (pv, md) = (id.clone(), model.clone());
        list = list.child(
            row()
                .key(format!("picker-model-{id}-{model}"))
                .w_full()
                .gap(12.)
                .py(4.)
                .center_y()
                .child(
                    col()
                        .grow(1.)
                        .min_w(0.)
                        .child(
                            text(name.clone())
                                .font(sm())
                                .color(if on { p.text } else { p.text_muted })
                                .truncate(),
                        )
                        .child(
                            text(model.clone())
                                .font(MONO_XS)
                                .color(p.text_faint)
                                .truncate(),
                        ),
                )
                .child(crate::pages::common::switch(
                    k,
                    on,
                    false,
                    &format!("Show {name} in the model picker"),
                    move |s: &mut State| super::set_model_shown(s, pv.clone(), md.clone(), !on),
                )),
        );
    }
    c.child(body.child(list))
}

/// Models the chat's switcher offers. The gateway binds the list to the
/// active provider, so it lives on that provider's card.
fn render_quick_switch(s: &State, k: &Kit, pv: &LlmProviderView) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let pinned = ps
        .catalog
        .as_ref()
        .map(|c| c.models.clone())
        .unwrap_or_default();
    let active = pv.active_model.clone();
    let mut c = col()
        .w_full()
        .gap(6.)
        .child(rule(p))
        .child(text("Quick switch in chat").font(xs_medium()).color(p.text))
        .child(muted(
            p,
            if pinned.is_empty() {
                "Add models here to switch between them from the chat composer."
            } else {
                "Click a model to remove it."
            },
        ));
    if !pinned.is_empty() {
        c = c.child(
            row()
                .w_full()
                .flex_wrap()
                .gap(4.)
                .children(pinned.iter().map(|model| {
                    let (m, a) = (model.clone(), active.clone());
                    pressable(
                        button_sized(model.clone(), Look::Outline, Size::XSmall)
                            .icon(&icons::CLOSE)
                            .on_press(move |s: &mut State| {
                                super::toggle_pin(s, m.clone(), a.clone())
                            })
                            .build(k),
                    )
                    .key(format!("pinned-{model}"))
                    .motion(motion::ROW_RESORT)
                    .transition(motion::glyph_pop())
                })),
        );
    }
    if !ps.pin_open {
        c = c.child(
            row().child(
                pressable(
                    button_sized("Add model", Look::Ghost, Size::XSmall)
                        .icon(&icons::PLUS)
                        .on_press(|s: &mut State| super::open_pin_input(s))
                        .build(k),
                )
                .key("pin-open"),
            ),
        );
    } else {
        c = c.child(
            row()
                .key("pin-editor")
                .transition(motion::reveal())
                .w_full()
                .gap(8.)
                .center_y()
                .child(
                    col().grow(1.).min_w(0.).child(
                        input(&ps.pin_input)
                            .placeholder("Model id, e.g. glm-5.3")
                            .on_edit(|s: &mut State| &mut s.pages.providers.pin_input)
                            .on_submit(|s: &mut State, _| super::add_pin(s))
                            .build(k)
                            .key("pin-input"),
                    ),
                )
                .child(
                    pressable(
                        button_sized("Add", Look::Outline, Size::Small)
                            .on_press(|s: &mut State| super::add_pin(s))
                            .build(k),
                    )
                    .key("pin-add"),
                ),
        );
    }
    c
}

fn render_tabs(s: &State, k: &Kit) -> El {
    let ps = &s.pages.providers;
    // The search wraps under the tabs when the page is too narrow for both.
    row()
        .w_full()
        .flex_wrap()
        .gap(8.)
        .center_y()
        .child(crate::pages::common::tab_bar(
            k,
            "provider-tabs",
            &[
                (Tab::Accounts, "Accounts & API keys"),
                (Tab::Cli, "CLI agents"),
            ],
            ps.tab,
            |s: &mut State, which| {
                let p = &mut s.pages.providers;
                p.tab = which;
                p.adding = None;
            },
        ))
        .child(el().grow(1.))
        .child(
            col().w(220.).child(
                input(&ps.search)
                    .placeholder("Search providers")
                    .leading_icon(&icons::SEARCH)
                    .on_edit(|s: &mut State| &mut s.pages.providers.search)
                    .build(k)
                    .key("providers-search"),
            ),
        )
}

fn render_tile(s: &State, pv: &LlmProviderView) -> El {
    let p = &s.palette;
    let open = s.pages.providers.adding.as_deref() == Some(pv.id.as_str());
    let provider = pv.clone();
    let base = p.hover;
    // Keyed by provider id, not its place in the catalog: a tile keeps its
    // identity as tiles around it leave (a provider connected) and glides
    // to its new cell; its open/closed rim morphs instead of flipping.
    col()
        .key(format!("tile-{}", pv.id))
        .motion(motion::ROW_RESORT)
        .role("button")
        .label(display_name(&pv.id))
        .focusable(true)
        .w_full()
        .min_w(0.)
        .padding(12.)
        .radius(12.)
        .border(1., if open { p.accent } else { p.border })
        .fill(if open { p.active } else { base })
        .hover(p.hover)
        .hover_motion(motion::HOVER, motion::HOVER)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut State| super::open_tile(s, &provider))
        .press_scale(motion::PRESS_CARD)
        .child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(provider_logo(
                    s,
                    &[pv.id.as_str(), pv.adapter.as_str()],
                    TILE_LOGO,
                ))
                .child(
                    text(display_name(&pv.id))
                        .font(sm_medium())
                        .color(p.text)
                        .truncate()
                        .grow(1.)
                        .min_w(0.),
                )
                .child(text(kind_of(&pv.id).label()).font(xs()).color(p.text_muted)),
        )
        .child(
            text(pv.description.clone())
                .font(xs())
                .color(p.text_muted)
                .truncate()
                .pt(4.),
        )
}

fn matches(query: &str, pv: &LlmProviderView) -> bool {
    query.is_empty()
        || pv.id.contains(query)
        || display_name(&pv.id).to_lowercase().contains(query)
        || pv.description.to_lowercase().contains(query)
}

/// The "Add a provider" column, whose last layout picks the tile columns.
const ADD_KEY: &str = "providers-add";
/// The narrowest a provider tile gets before the grid drops a column, at
/// the 16 pt UI font (scaled with it).
const TILE_MIN: f32 = 200.;
const TILE_GAP: f32 = 8.;

/// One to three tile columns, as many as fit at [`TILE_MIN`] in the width
/// the column had last frame (three before the first layout).
fn tile_columns(cx: &Cx<State>) -> usize {
    let Some(width) = cx.window_layout(ADD_KEY).map(|r| r[2]) else {
        return 3;
    };
    let min = TILE_MIN * crate::design::styles::type_scale();
    (((width + TILE_GAP) / (min + TILE_GAP)).floor() as usize).clamp(1, 3)
}

fn render_accounts(s: &State, k: &Kit, columns: usize) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let Some(llm) = ps.llm.as_ref() else {
        return spinner(k);
    };
    let query = ps.search.text.trim().to_lowercase();
    let open = open_tile(ps, llm);
    let mut groups = Vec::new();
    for kind in [Kind::Account, Kind::ApiKey, Kind::Local] {
        let tiles: Vec<El> = llm
            .providers
            .iter()
            .filter(|pv| !(pv.active || pv.api_key_set) && kind_of(&pv.id) == kind)
            .filter(|pv| matches(&query, pv))
            .map(|pv| render_tile(s, pv))
            .collect();
        if tiles.is_empty() {
            continue;
        }
        groups.push(
            col()
                .key(format!("group-{}", kind.label()))
                .w_full()
                .gap(8.)
                .child(
                    text(match kind {
                        Kind::Account => "Use a subscription or an existing login",
                        Kind::ApiKey => "Paste an API key",
                        Kind::Local => "Run on this Mac",
                    })
                    .font(xs_medium())
                    .color(p.text_muted),
                )
                // Equal columns that fill the page's width (as many as
                // fit, see `tile_columns`), so names and descriptions get
                // the room instead of a ragged right edge.
                .child(
                    el().w_full()
                        .equal_columns(columns)
                        .gap(TILE_GAP)
                        .children(tiles),
                ),
        );
    }
    let mut c = col().w_full().gap(16.);
    if let Some(pv) = open {
        c = c.child(render_provider(s, k, pv));
    }
    if groups.is_empty() {
        c = c.child(empty_state(p, "No provider matches."));
    }
    c.children(groups)
}

/// The tile opened from "Add a provider", while it is not connected yet.
fn open_tile<'a>(ps: &ProvidersState, llm: &'a LlmConfigSnapshot) -> Option<&'a LlmProviderView> {
    let id = ps.adding.as_ref()?;
    llm.providers
        .iter()
        .find(|pv| pv.id == *id && !(pv.active || pv.api_key_set))
}
