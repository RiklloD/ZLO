//! Extensions page rendering (GPUI `ExtensionsView`'s `render_*` methods).

use crate::pages::common::controls::{
    Look, Size, button_sized, header_with, icon_button_sized, input, open_url, outlined_tag,
    refresh_icon, rule, section_title, spinner_line,
};
use crate::pages::common::ts::{body_semibold, sm, sm_medium, xs, xs_medium};
use crate::pages::common::{Tone, card, empty_state, error_line, loading, notice_line, page_body};
use rune::{Cx, El, TextStyle, col, el, row, text};
use rune_kit::Kit;
use zlo_client::extension_state::LifecyclePublicState;
use zlo_client::product_wire::{RebornExtensionCredentialSetup, RebornExtensionRegistryEntry};
use zlo_client::wire::InstalledExtension;

use super::{MCP_AUTH, McpForm, Pairing, Setup};
use crate::icons;
use crate::jobs;
use crate::state::State;

pub fn page(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let ext = &s.pages.extensions;
    let actions = vec![
        button_sized("MCP server", Look::Outline, Size::Small)
            .icon(&icons::PLUS)
            .on_press(|s: &mut State| super::toggle_mcp(s))
            .build(k)
            .key("add-mcp"),
        loading(
            button_sized("Import bundle", Look::Outline, Size::Small)
                .help("Upload a tool bundle .zip (operator only)")
                .on_press_tx(super::import_bundle),
            ext.importing,
        )
        .build(k)
        .key("import-bundle"),
        refresh_icon()
            .on_press(|s: &mut State| super::refresh(s))
            .build(k)
            .key("refresh-extensions"),
    ];
    let installed: Vec<El> = ext
        .installed
        .iter()
        .enumerate()
        .map(|(ix, e)| render_installed(s, k, ix, e))
        .collect();
    let catalog: Vec<El> = ext
        .catalog
        .iter()
        .filter(|e| !e.installed)
        .enumerate()
        .map(|(ix, e)| render_catalog(s, k, ix, e))
        .collect();
    let mut body = col().w_full().child(header_with(p, "Extensions", actions));
    if let Some(e) = &ext.error {
        body = body.child(error_line(p, e));
    }
    if let Some(n) = &ext.notice {
        body = body.child(notice_line(n, p.text_muted));
    }
    if let Some(f) = &ext.mcp {
        body = body.child(col().w_full().pb(16.).child(render_mcp(s, k, f)));
    }
    body = body.child(section_title(p, "Installed", 0.));
    if ext.loaded && installed.is_empty() {
        body = body.child(empty_state(p, "No extensions installed yet."));
    }
    body = body
        .child(col().w_full().gap(12.).children(installed))
        .child(section_title(p, "Available", 24.))
        .child(col().w_full().children(catalog));
    page_body("extensions-page", body)
}

fn render_pairing(s: &State, k: &Kit, pr: &Pairing) -> El {
    let p = &s.palette;
    let body = match &pr.status {
        None => spinner_line(p, k, "Checking connection"),
        Some(st) if st.connected => row()
            .w_full()
            .gap(8.)
            .center_y()
            .child(outlined_tag(p, Tone::Success, "Connected"))
            .child(el().grow(1.))
            .child(
                loading(
                    button_sized("Disconnect", Look::Outline, Size::Small)
                        .on_press(|s: &mut State| super::unpair(s)),
                    pr.busy,
                )
                .build(k)
                .key("unpair"),
            ),
        Some(st) => match &st.pending {
            Some(code) => {
                let mut actions = row().gap(8.).center_y();
                if let Some(url) = code.deep_link.clone() {
                    actions = actions.child(
                        button_sized("Open in chat app", Look::Outline, Size::Small)
                            .icon(&icons::EXTERNAL_LINK)
                            .on_press_tx(move |_: &mut State, tx| open_url(tx, &url))
                            .build(k)
                            .key("pair-link"),
                    );
                }
                actions = actions
                    .child(
                        loading(
                            button_sized("New code", Look::Ghost, Size::Small)
                                .on_press(|s: &mut State| super::mint_pairing(s)),
                            pr.busy,
                        )
                        .build(k)
                        .key("pair-new"),
                    )
                    .child(text("Waiting for the bot").font(xs()).color(p.text_muted));
                col()
                    .w_full()
                    .gap(8.)
                    .child(
                        text("Send this code to the bot from your chat app:")
                            .font(sm())
                            .color(p.text)
                            .wrap(),
                    )
                    .child(
                        row()
                            .gap(12.)
                            .center_y()
                            .child(
                                text(code.code.clone())
                                    .font(TextStyle::new(20., 600.).mono())
                                    .color(p.text)
                                    .selectable(true),
                            )
                            .child(
                                text(format!("expires {}", jobs::when(code.expires_at)))
                                    .font(xs())
                                    .color(p.text_muted),
                            ),
                    )
                    .child(actions)
            }
            None => row()
                .gap(8.)
                .center_y()
                .child(text("Not connected.").font(sm()).color(p.text))
                .child(
                    loading(
                        button_sized("Get a pairing code", Look::Primary, Size::Small)
                            .on_press(|s: &mut State| super::mint_pairing(s)),
                        pr.busy,
                    )
                    .build(k)
                    .key("pair-mint"),
                ),
        },
    };
    col().w_full().gap(8.).child(rule(p)).child(body)
}

fn render_mcp(s: &State, k: &Kit, form: &McpForm) -> El {
    let p = &s.palette;
    let auth = MCP_AUTH.iter().enumerate().map(|(ix, label)| {
        let look = if ix == form.auth {
            Look::Primary
        } else {
            Look::Outline
        };
        button_sized(*label, look, Size::Small)
            .on_press(move |s: &mut State| {
                if let Some(f) = s.pages.extensions.mcp.as_mut() {
                    f.auth = ix;
                }
            })
            .build(k)
            .key(format!("mcp-auth-{ix}"))
    });
    card(p)
        .key("mcp-form")
        .child(
            text("Custom MCP server")
                .font(body_semibold())
                .color(p.text),
        )
        .child(
            input(&form.name)
                .placeholder("Name, e.g. Linear")
                .on_edit(|s: &mut State| super::mcp_input(s, false))
                .build(k)
                .key("mcp-name"),
        )
        .child(
            input(&form.endpoint)
                .placeholder("https://example.com/mcp")
                .on_edit(|s: &mut State| super::mcp_input(s, true))
                .on_submit(|s: &mut State, _| super::submit_mcp(s))
                .build(k)
                .key("mcp-endpoint"),
        )
        .child(
            row()
                .gap(8.)
                .center_y()
                .child(text("Sign-in").font(xs_medium()).color(p.text))
                .children(auth),
        )
        .child(
            row()
                .gap(8.)
                .child(
                    loading(
                        button_sized("Add server", Look::Primary, Size::Small)
                            .on_press(|s: &mut State| super::submit_mcp(s)),
                        form.submitting,
                    )
                    .build(k)
                    .key("mcp-add"),
                )
                .child(
                    button_sized("Cancel", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| super::toggle_mcp(s))
                        .build(k)
                        .key("mcp-cancel"),
                ),
        )
}

fn render_setup(s: &State, k: &Kit, setup: &Setup) -> El {
    let p = &s.palette;
    let Some(view) = setup.view.as_ref() else {
        return spinner_line(p, k, "Loading setup");
    };
    let needs_browser = view
        .secrets
        .iter()
        .any(|x| !x.provided && !matches!(x.setup, RebornExtensionCredentialSetup::ManualToken));
    let instructions = view
        .onboarding
        .as_ref()
        .and_then(|o| {
            o.credential_instructions
                .clone()
                .or(o.credential_next_step.clone())
        })
        .or(view.message.clone());
    let setup_url = view.onboarding.as_ref().and_then(|o| o.setup_url.clone());

    // (prompt, optional, provided, placeholder), in input order.
    let prompts: Vec<(String, bool, bool, String)> = view
        .secrets
        .iter()
        .filter(|x| matches!(x.setup, RebornExtensionCredentialSetup::ManualToken))
        .map(|x| {
            let placeholder = if x.provided {
                "Saved. Paste a new value to replace it."
            } else {
                "Paste value"
            };
            (
                x.prompt.clone(),
                x.optional,
                x.provided,
                placeholder.to_string(),
            )
        })
        .chain(view.fields.iter().map(|f| {
            (
                f.prompt.clone(),
                f.optional,
                false,
                f.placeholder.clone().unwrap_or_default(),
            )
        }))
        .collect();
    let rows = setup.inputs.iter().zip(prompts).enumerate().map(
        |(i, ((_, secret, edit), (prompt, optional, provided, placeholder)))| {
            let mut label = row()
                .gap(8.)
                .center_y()
                .child(text(prompt.clone()).font(xs_medium()).color(p.text));
            if optional {
                label = label.child(text("optional").font(xs()).color(p.text_muted));
            }
            if provided {
                label = label.child(outlined_tag(p, Tone::Success, "Saved"));
            }
            let field = input(edit)
                .placeholder(placeholder)
                .label(prompt)
                .on_edit(move |s: &mut State| super::setup_input(s, i));
            let field = if *secret { field.secure() } else { field };
            col()
                .w_full()
                .gap(4.)
                .child(label)
                .child(field.build(k).key(format!("setup-input-{i}")))
        },
    );

    let browser = super::browser_url(s);
    let mut buttons = row().gap(8.);
    if !setup.inputs.is_empty() {
        buttons = buttons.child(
            loading(
                button_sized("Save", Look::Primary, Size::Small)
                    .on_press(|s: &mut State| super::submit_setup(s)),
                setup.submitting,
            )
            .build(k)
            .key("submit-setup"),
        );
    }
    if needs_browser {
        buttons = buttons.child(
            button_sized("Sign in via browser", Look::Outline, Size::Small)
                .icon(&icons::EXTERNAL_LINK)
                .on_press_tx(move |_: &mut State, tx| open_url(tx, &browser))
                .build(k)
                .key("setup-in-browser"),
        );
    }
    if let Some(url) = setup_url {
        buttons = buttons.child(
            button_sized("Where to get it", Look::Ghost, Size::Small)
                .icon(&icons::EXTERNAL_LINK)
                .on_press_tx(move |_: &mut State, tx| open_url(tx, &url))
                .build(k)
                .key("setup-url"),
        );
    }
    let mut c = col().w_full().gap(12.).child(rule(p));
    if let Some(t) = instructions {
        c = c.child(text(t).font(sm()).color(p.text).wrap());
    }
    c.children(rows).child(buttons)
}

fn render_installed(s: &State, k: &Kit, ix: usize, e: &InstalledExtension) -> El {
    let p = &s.palette;
    let ext = &s.pages.extensions;
    let id = e.package_ref.id.as_str().to_string();
    let busy = ext.busy.as_deref() == Some(id.as_str());
    let confirming = ext.confirm_remove.as_deref() == Some(id.as_str());
    let setup = ext.setup.as_ref().filter(|x| x.id == id);
    let pairing = ext.pairing.as_ref().filter(|x| x.id == id);
    let channel = e.is_channel() && matches!(e.installation_state, LifecyclePublicState::Active);
    let state_tag = match e.installation_state {
        LifecyclePublicState::Active => outlined_tag(p, Tone::Success, "Active"),
        LifecyclePublicState::SetupNeeded => outlined_tag(p, Tone::Warning, "Needs setup"),
        LifecyclePublicState::Uninstalled => outlined_tag(p, Tone::Secondary, "Removed"),
    };
    let remove_button = if confirming {
        let id = id.clone();
        button_sized("Remove?", Look::Danger, Size::Small)
            .disabled(busy)
            .on_press(move |s: &mut State| super::remove(s, id.clone()))
            .build(k)
            .key(format!("confirm-remove-{ix}"))
    } else {
        let id = id.clone();
        icon_button_sized(&icons::DELETE, "Remove", Size::Small)
            .disabled(busy)
            .on_press(move |s: &mut State| s.pages.extensions.confirm_remove = Some(id.clone()))
            .build(k)
            .key(format!("remove-{ix}"))
    };
    let mut head = row()
        .w_full()
        .gap(8.)
        .center_y()
        .child(
            col()
                .grow(1.)
                .min_w(0.)
                .child(
                    text(e.display_name.clone())
                        .font(body_semibold())
                        .color(p.text),
                )
                .child(
                    text(e.description.clone())
                        .font(xs())
                        .color(p.text_muted)
                        .truncate(),
                ),
        )
        .child(state_tag);
    if channel {
        let pair_id = id.clone();
        head = head.child(
            button_sized(
                if pairing.is_some() {
                    "Close"
                } else {
                    "Connect"
                },
                Look::Outline,
                Size::Small,
            )
            .disabled(busy)
            .on_press(move |s: &mut State| super::toggle_pairing(s, pair_id.clone()))
            .build(k)
            .key(format!("pair-{ix}")),
        );
    }
    let setup_id = id.clone();
    head = head
        .child(
            button_sized(
                if setup.is_some() { "Close" } else { "Set up" },
                Look::Outline,
                Size::Small,
            )
            .disabled(busy)
            .on_press(move |s: &mut State| super::open_setup(s, setup_id.clone()))
            .build(k)
            .key(format!("setup-{ix}")),
        )
        .child(remove_button);
    let mut c = card(p).key(format!("ext-{id}")).child(head).child(
        text(format!(
            "{} · {} tools{}",
            e.runtime,
            e.tools.len(),
            e.version
                .as_deref()
                .map(|v| format!(" · v{v}"))
                .unwrap_or_default()
        ))
        .font(xs())
        .color(p.text_muted),
    );
    if let Some(err) = &e.activation_error {
        c = c.child(error_line(p, err));
    }
    if let Some(x) = setup {
        c = c.child(render_setup(s, k, x));
    }
    if let Some(x) = pairing {
        c = c.child(render_pairing(s, k, x));
    }
    c
}

fn render_catalog(s: &State, k: &Kit, ix: usize, entry: &RebornExtensionRegistryEntry) -> El {
    let p = &s.palette;
    let busy = s.pages.extensions.busy.as_deref() == Some(entry.package_ref.id.as_str());
    let e = entry.clone();
    col()
        .key(format!("catalog-{}", entry.package_ref.id.as_str()))
        .w_full()
        .child(
            row()
                .w_full()
                .gap(12.)
                .py(8.)
                .center_y()
                .child(
                    col()
                        .grow(1.)
                        .min_w(0.)
                        .child(
                            text(entry.display_name.clone())
                                .font(sm_medium())
                                .color(p.text),
                        )
                        .child(
                            text(entry.description.clone())
                                .font(xs())
                                .color(p.text_muted)
                                .wrap(),
                        ),
                )
                .child(outlined_tag(p, Tone::Secondary, entry.runtime.clone()))
                .child(
                    loading(
                        button_sized("Install", Look::Outline, Size::Small)
                            .on_press(move |s: &mut State| super::install(s, &e)),
                        busy,
                    )
                    .build(k)
                    .key(format!("install-{ix}")),
                ),
        )
        .child(rule(p))
}
