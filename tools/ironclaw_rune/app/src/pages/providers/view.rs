//! Providers page rendering (GPUI `ProvidersView`'s `render_*` methods).

use ironclaw_gpui_client::operator_llm::{LlmConfigSnapshot, LlmProviderView};
use ironclaw_gpui_client::orchestration::RouteProvider;
use ironclaw_gpui_client::task_models::TaskModelChoice;
use rune::{Cx, El, col, el, row, text};
use rune_kit::Kit;

use super::catalog::{KNOWN_CLIS, Kind, display_name, kind_of};
use crate::design::brands::provider_logo;
use super::{CODEX_ADAPTER, Models, ProvidersState, Tab, connected, task_choices};
use crate::icons;
use crate::pages::c_widgets::{
    BODY_SEMIBOLD, Look, MONO_XS, SM, SM_MEDIUM, Size, Tone, XS, XS_MEDIUM, button, card,
    empty_state, error_line, header, icon_button, input, loading, muted, notice_line, open_url,
    page_body, refresh_button, rule, section_title, spinner, spinner_line, tag,
};
use crate::state::State;

const TILE_WIDTH: f32 = 196.;
const LOGO: f32 = 22.;
const TILE_LOGO: f32 = 20.;

pub fn page(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let actions = vec![
        refresh_button()
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
                .map(|(ix, provider)| render_provider(s, k, ix, provider))
                .collect()
        })
        .unwrap_or_default();
    let mut body = col().w_full().child(header(p, "Providers", actions));
    if let Some(e) = &ps.error {
        body = body.child(error_line(p, e));
    }
    if let Some(n) = &ps.notice {
        body = body.child(notice_line(p, n, p.success));
    }
    body = body.child(section_title(p, "Connected", 24.));
    body = body.child(if ps.llm.is_none() {
        spinner(k)
    } else if connected_rows.is_empty() {
        empty_state(p, "Nothing connected yet. Add a provider below.")
    } else {
        col().w_full().gap(12.).children(connected_rows)
    });
    if ps.task_models.is_some() {
        body = body
            .child(section_title(p, "Background tasks", 24.))
            .child(render_task_models(s, k));
    }
    body = body
        .child(section_title(p, "Add a provider", 24.))
        .child(render_tabs(s, k))
        .child(col().w_full().pt(12.).child(match ps.tab {
            Tab::Accounts => render_accounts(s, k),
            Tab::Cli => render_cli(s, k),
        }));
    page_body("providers-page", body)
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
                    .child(text("Thread titles").font(BODY_SEMIBOLD).color(p.text))
                    .child(muted(
                        p,
                        "Names a thread after its first reply. One model for every profile.",
                    )),
            )
            .child(
                button(label, Look::Outline, Size::Small)
                    .icon(if ps.titles_open {
                        &icons::CHEVRON_UP
                    } else {
                        &icons::CHEVRON_DOWN
                    })
                    .on_press(|s: &mut State| {
                        let p = &mut s.pages.providers;
                        p.titles_open = !p.titles_open;
                    })
                    .build(k)
                    .key("title-model"),
            ),
    );
    if ps.titles_open {
        let off = if current.is_none() {
            Look::Primary
        } else {
            Look::Outline
        };
        let mut list = col().w_full().gap(8.).pt(8.).child(
            row().child(
                button("Off", off, Size::Small)
                    .on_press(|s: &mut State| super::set_title_model(s, None))
                    .build(k)
                    .key("title-model-off"),
            ),
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
                button(
                    model,
                    if selected {
                        Look::Primary
                    } else {
                        Look::Outline
                    },
                    Size::Small,
                )
                .on_press(move |s: &mut State| super::set_title_model(s, Some(choice.clone())))
                .build(k)
                .key(format!("title-model-{pix}-{mix}"))
            });
            list = list.child(
                col()
                    .w_full()
                    .gap(4.)
                    .child(text(name).font(XS).color(p.text_muted))
                    .child(row().w_full().flex_wrap().gap(4.).children(buttons)),
            );
        }
        card = card.child(list);
    }
    card
}

fn render_provider(s: &State, k: &Kit, ix: usize, pv: &LlmProviderView) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let usable = pv.api_key_set || !pv.api_key_required;
    let editing = ps.key_editor.as_deref() == Some(pv.id.as_str());
    let status = if pv.active {
        tag(
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
        tag(p, Tone::Secondary, "Key saved")
    } else if pv.api_key_required {
        tag(p, Tone::Warning, "Needs key")
    } else {
        tag(p, Tone::Secondary, "No key needed")
    };

    let mut actions = row().gap(4.).center_y();
    if pv.can_list_models && usable {
        let provider = pv.clone();
        actions = actions.child(
            button("Models", Look::Ghost, Size::Small)
                .on_press(move |s: &mut State| super::toggle_models(s, provider.clone()))
                .build(k)
                .key(format!("models-{ix}")),
        );
    }
    if usable && pv.adapter != CODEX_ADAPTER {
        let provider = pv.clone();
        let running = ps.probes.get(&pv.id).is_some_and(Option::is_none);
        actions = actions.child(
            loading(
                button("Test", Look::Ghost, Size::Small)
                    .on_press(move |s: &mut State| super::test_connection(s, provider.clone())),
                running,
            )
            .build(k)
            .key(format!("test-{ix}")),
        );
    }
    if pv.adapter == CODEX_ADAPTER && !pv.active {
        let id = pv.id.clone();
        actions = actions.child(
            button("Sign in with ChatGPT", Look::Outline, Size::Small)
                .disabled(ps.codex.is_some())
                .on_press(move |s: &mut State| super::start_codex(s, id.clone()))
                .build(k)
                .key(format!("codex-login-{ix}")),
        );
    }
    if pv.accepts_api_key {
        let id = pv.id.clone();
        actions = actions.child(
            button(
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
            .key(format!("set-key-{ix}")),
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
            icon_button(&icons::CLOSE, help, Size::XSmall)
                .disabled(pv.active)
                .on_press(move |s: &mut State| s.pages.providers.confirm_delete = Some(id.clone()))
                .build(k)
                .key(format!("delete-provider-{ix}")),
        );
    }

    let mut card = card(p).key(format!("provider-{}", pv.id)).child(
        row()
            .w_full()
            .gap(12.)
            .center_y()
            .child(provider_logo(s, &[pv.id.as_str(), pv.adapter.as_str()],
                LOGO,
            ))
            .child(
                col()
                    .grow(1.)
                    .min_w(0.)
                    .child(text(display_name(&pv.id)).font(BODY_SEMIBOLD).color(p.text))
                    .child(
                        text(pv.description.clone())
                            .font(XS)
                            .color(p.text_muted)
                            .truncate(),
                    ),
            )
            .child(status)
            .child(actions),
    );

    if let Some(models) = ps.models.get(&pv.id) {
        card = card.child(match models {
            Models::Loading => spinner_line(p, k, "Loading models"),
            Models::Failed(e) => error_line(p, e),
            Models::Loaded(list) => {
                let current = pv.active.then(|| pv.active_model.clone()).flatten();
                row()
                    .w_full()
                    .flex_wrap()
                    .gap(4.)
                    .children(list.iter().enumerate().map(|(mix, model)| {
                        let selected = current.as_deref() == Some(model.as_str());
                        let (id, m) = (pv.id.clone(), model.clone());
                        button(
                            model.clone(),
                            if selected {
                                Look::Primary
                            } else {
                                Look::Outline
                            },
                            Size::Small,
                        )
                        .on_press(move |s: &mut State| {
                            super::set_active(s, id.clone(), Some(m.clone()))
                        })
                        .build(k)
                        .key(format!("model-{ix}-{mix}"))
                    }))
            }
        });
    }
    if let Some(Some((ok, msg))) = ps.probes.get(&pv.id) {
        card = card.child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(if *ok {
                    tag(p, Tone::Success, "Connected")
                } else {
                    tag(p, Tone::Danger, "Failed")
                })
                .child(text(msg.clone()).font(XS).color(p.text).wrap().grow(1.)),
        );
    }
    if ps.confirm_delete.as_deref() == Some(pv.id.as_str()) {
        let id = pv.id.clone();
        card = card.child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(
                    text(format!(
                        "Remove {} and its stored API key?",
                        display_name(&pv.id)
                    ))
                    .font(SM)
                    .color(p.text)
                    .wrap()
                    .grow(1.),
                )
                .child(
                    button("Remove", Look::Danger, Size::Small)
                        .on_press(move |s: &mut State| super::delete_provider(s, id.clone()))
                        .build(k)
                        .key(format!("confirm-delete-{ix}")),
                )
                .child(
                    button("Cancel", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| s.pages.providers.confirm_delete = None)
                        .build(k)
                        .key(format!("cancel-delete-{ix}")),
                ),
        );
    }
    if let Some(login) = ps.codex.as_ref().filter(|l| l.provider_id == pv.id) {
        let uri = login.uri.clone();
        card = card.child(rule(p)).child(
            col()
                .w_full()
                .gap(6.)
                .child(muted(
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
                        .child(rune_kit::Spinner::new().size(12.).build(k))
                        .child(
                            button("Open page", Look::Ghost, Size::Small)
                                .on_press_tx(move |_: &mut State, tx| open_url(tx, &uri))
                                .build(k)
                                .key(format!("codex-open-{ix}")),
                        )
                        .child(
                            button("Cancel", Look::Ghost, Size::Small)
                                .on_press(|s: &mut State| s.pages.providers.codex = None)
                                .build(k)
                                .key(format!("codex-cancel-{ix}")),
                        ),
                ),
        );
    }
    if pv.active {
        card = card.child(render_quick_switch(s, k, pv));
    }
    if editing {
        let provider = pv.clone();
        card = card.child(
            row()
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
                    button("Save", Look::Primary, Size::Small)
                        .on_press(move |s: &mut State| super::save_key(s, provider.clone()))
                        .build(k)
                        .key(format!("save-key-{ix}")),
                )
                .child(
                    button("Cancel", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| s.pages.providers.key_editor = None)
                        .build(k)
                        .key(format!("cancel-key-{ix}")),
                ),
        );
    }
    card
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
        .child(text("Quick switch in chat").font(XS_MEDIUM).color(p.text))
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
                .children(pinned.iter().enumerate().map(|(pix, model)| {
                    let (m, a) = (model.clone(), active.clone());
                    button(model.clone(), Look::Outline, Size::XSmall)
                        .icon(&icons::CLOSE)
                        .on_press(move |s: &mut State| super::toggle_pin(s, m.clone(), a.clone()))
                        .build(k)
                        .key(format!("pinned-{pix}"))
                })),
        );
    }
    if !ps.pin_open {
        c = c.child(
            row().child(
                button("Add model", Look::Ghost, Size::XSmall)
                    .icon(&icons::PLUS)
                    .on_press(|s: &mut State| super::open_pin_input(s))
                    .build(k)
                    .key("pin-open"),
            ),
        );
    } else {
        c = c.child(
            row()
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
                    button("Add", Look::Outline, Size::Small)
                        .on_press(|s: &mut State| super::add_pin(s))
                        .build(k)
                        .key("pin-add"),
                ),
        );
    }
    c
}

fn render_tabs(s: &State, k: &Kit) -> El {
    let ps = &s.pages.providers;
    let tab = |key: &'static str, label: &'static str, which: Tab| {
        button(
            label,
            if ps.tab == which {
                Look::Primary
            } else {
                Look::Ghost
            },
            Size::Small,
        )
        .on_press(move |s: &mut State| {
            let p = &mut s.pages.providers;
            p.tab = which;
            p.adding = None;
        })
        .build(k)
        .key(key)
    };
    row()
        .w_full()
        .gap(8.)
        .center_y()
        .child(tab("tab-accounts", "Accounts & API keys", Tab::Accounts))
        .child(tab("tab-cli", "CLI agents", Tab::Cli))
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

fn render_tile(s: &State, ix: usize, pv: &LlmProviderView) -> El {
    let p = &s.palette;
    let open = s.pages.providers.adding.as_deref() == Some(pv.id.as_str());
    let provider = pv.clone();
    let base = p.hover;
    col()
        .key(format!("tile-{ix}"))
        .role("button")
        .label(display_name(&pv.id))
        .focusable(true)
        .w(TILE_WIDTH)
        .padding(12.)
        .radius(12.)
        .border(1., if open { p.accent } else { p.border })
        .fill(if open { p.active } else { base })
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut State| super::open_tile(s, &provider))
        .child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(provider_logo(s, &[pv.id.as_str(), pv.adapter.as_str()],
                    TILE_LOGO,
                ))
                .child(
                    text(display_name(&pv.id))
                        .font(SM_MEDIUM)
                        .color(p.text)
                        .truncate()
                        .grow(1.)
                        .min_w(0.),
                )
                .child(text(kind_of(&pv.id).label()).font(XS).color(p.text_muted)),
        )
        .child(
            text(pv.description.clone())
                .font(XS)
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

fn render_accounts(s: &State, k: &Kit) -> El {
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
            .enumerate()
            .filter(|(_, pv)| !(pv.active || pv.api_key_set) && kind_of(&pv.id) == kind)
            .filter(|(_, pv)| matches(&query, pv))
            .map(|(ix, pv)| render_tile(s, ix, pv))
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
                    .font(XS_MEDIUM)
                    .color(p.text_muted),
                )
                .child(row().w_full().flex_wrap().gap(8.).children(tiles)),
        );
    }
    let mut c = col().w_full().gap(16.);
    if let Some((ix, pv)) = open {
        c = c.child(render_provider(s, k, ix, pv));
    }
    if groups.is_empty() {
        c = c.child(empty_state(p, "No provider matches."));
    }
    c.children(groups)
}

/// The tile opened from "Add a provider", while it is not connected yet.
fn open_tile<'a>(
    ps: &ProvidersState,
    llm: &'a LlmConfigSnapshot,
) -> Option<(usize, &'a LlmProviderView)> {
    let id = ps.adding.as_ref()?;
    llm.providers
        .iter()
        .enumerate()
        .find(|(_, pv)| pv.id == *id && !(pv.active || pv.api_key_set))
}

/// One CLI agent row: logo, name, blurb, a shell command to copy, status,
/// and the remove / add-back button.
struct CliRow {
    id: String,
    logo_ids: Vec<String>,
    name: String,
    blurb: String,
    status: El,
    command: Option<(&'static str, &'static str)>,
    remove: Option<(String, bool)>,
}

fn cli_row(s: &State, k: &Kit, r: CliRow) -> El {
    let p = &s.palette;
    let ids: Vec<&str> = r.logo_ids.iter().map(String::as_str).collect();
    let hidden = r.remove.as_ref().is_some_and(|(_, h)| *h);
    let mut info = col()
        .grow(1.)
        .min_w(0.)
        .child(text(r.name).font(BODY_SEMIBOLD).color(p.text))
        .child(text(r.blurb).font(XS).color(p.text_muted).wrap());
    if let Some((label, cmd)) = r.command {
        info = info.child(
            row()
                .pt(4.)
                .gap(8.)
                .center_y()
                .child(text(label).font(XS).color(p.text_muted))
                .child(text(cmd).font(MONO_XS).color(p.text).selectable(true))
                .child(
                    icon_button(&icons::COPY, "Copy command", Size::XSmall)
                        .on_press_tx(move |_: &mut State, tx| tx.copy(cmd))
                        .build(k)
                        .key(format!("copy-{}", r.id)),
                ),
        );
    }
    let mut line = row()
        .w_full()
        .gap(12.)
        .center_y()
        .child(provider_logo(s, &ids, LOGO))
        .child(info)
        .child(r.status);
    if let Some((provider, hidden)) = r.remove {
        let b = if hidden {
            button("Add back", Look::Outline, Size::XSmall)
                .on_press(move |s: &mut State| super::toggle_cli_hidden(s, provider.clone()))
                .build(k)
        } else {
            icon_button(
                &icons::CLOSE,
                "Remove from ZLO (the CLI stays installed)",
                Size::XSmall,
            )
            .on_press(move |s: &mut State| super::toggle_cli_hidden(s, provider.clone()))
            .build(k)
        };
        line = line.child(b.key(format!("cli-remove-{}", r.id)));
    }
    let c = card(p).key(format!("cli-{}", r.id)).child(line);
    if hidden { c.opacity(0.6) } else { c }
}

fn render_cli(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let runtimes = ps.runtimes.clone().unwrap_or_default();
    let query = ps.search.text.trim().to_lowercase();
    let mut rows: Vec<El> = Vec::new();
    for known in KNOWN_CLIS {
        if !query.is_empty() && !known.name.to_lowercase().contains(&query) {
            continue;
        }
        let found: Vec<&RouteProvider> = runtimes
            .iter()
            .filter(|r| {
                r.provider.starts_with(known.runtime) || r.runtime.starts_with(known.runtime)
            })
            .collect();
        if found.is_empty() {
            let status = if known.planned {
                tag(p, Tone::Secondary, "Driver coming next")
            } else if ps.runtimes.is_none() {
                tag(p, Tone::Secondary, "Unknown")
            } else {
                tag(p, Tone::Warning, "Not detected")
            };
            rows.push(cli_row(s, k,
                CliRow {
                    id: known.runtime.into(),
                    logo_ids: vec![known.runtime.to_string()],
                    name: known.name.to_string(),
                    blurb: known.blurb.to_string(),
                    status,
                    command: known.install.map(|c| ("Install", c)),
                    remove: None,
                },
            ));
            continue;
        }
        for r in found {
            let hidden = ps
                .picker
                .as_ref()
                .is_some_and(|pp| pp.provider_hidden(&r.provider));
            let status = if hidden {
                tag(p, Tone::Secondary, "Removed")
            } else if r.authenticated {
                tag(
                    p,
                    Tone::Success,
                    format!("Ready · {} models", r.models.len()),
                )
            } else {
                tag(p, Tone::Warning, "Not signed in")
            };
            let command = (!r.authenticated)
                .then_some(known.login)
                .flatten()
                .map(|c| ("Sign in", c));
            rows.push(cli_row(s, k,
                CliRow {
                    id: r.provider.clone(),
                    logo_ids: vec![r.provider.clone(), r.runtime.clone()],
                    name: r.name.clone(),
                    blurb: known.blurb.to_string(),
                    status,
                    command,
                    remove: ps.picker.is_some().then(|| (r.provider.clone(), hidden)),
                },
            ));
        }
    }
    let mut c = col().w_full().gap(12.);
    if ps.runtimes.is_none() {
        c = c.child(muted(
            p,
            "Start one thread to let IronClaw report which CLI agents it found.",
        ));
    }
    c.children(rows)
}
