//! "Connect a provider instead": IronClaw's own agent on the ChatGPT plan
//! (device sign-in, as Settings → Providers) or an API key, folded under
//! the agents.

use rune::{El, col, el, row, text};
use rune_kit::Kit;
use zlo_client::operator_llm::LlmProviderView;

use crate::design::brands::provider_logo;
use crate::design::{kit as zkit, motion};
use crate::jobs;
use crate::pages::common::controls::{
    Look, Size, button_sized, input, muted, open_url, spinner_line,
};
use crate::pages::common::ts::xs;
use crate::pages::common::{Tone, error_line, notice_line, tag};
use crate::pages::providers::catalog::display_name;
use crate::state::{Page, State};

use super::super::view;

/// The providers offered here: ChatGPT plan first, then the common API
/// keys, then any other already connected. The rest live in Settings.
pub const OFFERED: [&str; 4] = ["openai_codex", "anthropic", "openai", "openrouter"];

fn offered(s: &State) -> Vec<&LlmProviderView> {
    let Some(llm) = &s.pages.providers.llm else {
        return Vec::new();
    };
    let mut out: Vec<&LlmProviderView> = OFFERED
        .iter()
        .filter_map(|id| llm.providers.iter().find(|p| p.id == *id))
        .collect();
    for p in &llm.providers {
        if (p.active || p.api_key_set) && !out.iter().any(|o| o.id == p.id) {
            out.push(p);
        }
    }
    out
}

fn open_key(s: &mut State, id: String) {
    let p = &mut s.pages.providers;
    p.key_input.clear();
    p.key_input.focused = true;
    p.key_editor = Some(id);
}

/// Save the key; the first provider connected becomes the active one so
/// IronClaw's agent can run on it at once.
fn save_key(s: &mut State, provider: LlmProviderView) {
    let input = &mut s.pages.providers.key_input;
    let key = input.text.trim().to_string();
    if key.is_empty() || s.onboarding.agents.saving_key {
        return;
    }
    // Drop the plaintext from the field at once; the request owns it now.
    input.clear();
    s.onboarding.agents.saving_key = true;
    s.onboarding.agents.error = None;
    jobs::run(
        s,
        "onboarding",
        move |c| async move {
            let llm = c.set_provider_key(&provider, &key).await?;
            if llm.active.is_none() {
                return c.set_active_llm(&provider.id, None).await;
            }
            Ok(llm)
        },
        |s, result, tx| {
            // The reply to the person's Save: the editor folds away and the
            // notice wipes in as they watch.
            tx.animated();
            s.onboarding.agents.saving_key = false;
            match result {
                Ok(llm) => {
                    let p = &mut s.pages.providers;
                    p.llm = Some(llm);
                    p.key_editor = None;
                    s.onboarding.agents.notice =
                        Some("API key saved to the encrypted secret store.".into());
                }
                Err(e) => s.onboarding.agents.error = Some(e.to_string()),
            }
        },
    );
}

pub(super) fn section(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let a = &s.onboarding.agents;
    let open = a.providers_open;
    let header = view::list_row(
        s,
        "providers-toggle".into(),
        rune::icon(&crate::icons::BOT, 16.).color(p.text_muted),
        "Connect a provider instead",
        Some(
            text("No CLI needed: IronClaw's own agent runs on your ChatGPT plan or an API key.")
                .font(xs())
                .color(p.text_muted)
                .wrap(),
        ),
        zkit::chevron(
            "providers-chevron",
            open,
            zkit::Turn::Disclose,
            16.,
            p.text_muted,
        ),
    )
    .hover(p.hover)
    .hover_motion(motion::HOVER, motion::HOVER)
    .cursor(rune::Cursor::Pointer)
    .role("button")
    .label("Connect a provider instead")
    .focusable(true)
    .on_press(|s: &mut State| {
        let a = &mut s.onboarding.agents;
        a.providers_open = !a.providers_open;
        if a.providers_open && s.pages.providers.llm.is_none() {
            crate::pages::providers::refresh(s);
        }
    });
    let mut rows = vec![header];
    if open {
        rows.extend(drawer(s, k));
    }
    let mut c = view::section(s, "agents-providers", None, rows).pt(12.);
    if open {
        c = c.child(
            row().w_full().pt(8.).justify_end().child(
                button_sized("All providers", Look::Ghost, Size::XSmall)
                    .icon(&crate::icons::EXTERNAL_LINK)
                    .help("Settings → Providers. Setup comes back when you return.")
                    .on_press(|s: &mut State| crate::workspace::show(s, Page::Providers))
                    .build(k)
                    .key("agents-all-providers")
                    .press_scale(motion::PRESS),
            ),
        );
    }
    c
}

/// The open drawer under "Connect a provider instead": its rows wipe open
/// under the header and fold away faster.
fn drawer(s: &State, k: &Kit) -> Vec<El> {
    let p = &s.palette;
    let a = &s.onboarding.agents;
    let mut lines = col().key("agents-providers-lines").w_full().px(16.).gap(6.);
    let mut any = false;
    if let Some(e) = &a.error {
        lines = lines.child(error_line(p, e).key("agents-error"));
        any = true;
    }
    if let Some(e) = &s.pages.providers.error {
        lines = lines.child(error_line(p, e).key("providers-error"));
        any = true;
    }
    if let Some(n) = &a.notice {
        lines = lines.child(notice_line(n, p.success).key("agents-notice"));
        any = true;
    }
    let mut out = Vec::new();
    if any {
        out.push(lines.py(10.).transition(motion::reveal()));
    }
    if s.pages.providers.llm.is_none() {
        out.push(
            row()
                .key("providers-loading")
                .w_full()
                .px(16.)
                .py(14.)
                .transition(motion::reveal())
                .child(spinner_line(p, k, "Loading providers")),
        );
        return out;
    }
    for pv in offered(s) {
        out.push(provider_card(s, k, pv).transition(motion::reveal()));
    }
    out
}

fn provider_card(s: &State, k: &Kit, pv: &LlmProviderView) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let codex = pv.adapter == "openai_codex";
    let status = if pv.active {
        Some(tag(p, "Active", Tone::Success))
    } else if pv.api_key_set {
        Some(tag(p, "Key saved", Tone::Secondary))
    } else {
        None
    };
    let mut line = row().gap(8.).center_y();
    if let Some(status) = status {
        line = line.child(status);
    }
    let id = pv.id.clone();
    if codex && !pv.active {
        line = line.child(
            button_sized("Sign in with ChatGPT", Look::Outline, Size::Small)
                .disabled(ps.codex.is_some())
                .on_press(move |s: &mut State| crate::pages::providers::start_codex(s, id.clone()))
                .build(k)
                .key(format!("onboarding-codex-{}", pv.id))
                .press_scale(motion::PRESS),
        );
    } else if !codex && pv.accepts_api_key && ps.key_editor.as_deref() != Some(pv.id.as_str()) {
        line = line.child(
            button_sized(
                if pv.api_key_set {
                    "Replace key"
                } else {
                    "Add key"
                },
                Look::Outline,
                Size::Small,
            )
            .on_press(move |s: &mut State| open_key(s, id.clone()))
            .build(k)
            .key(format!("onboarding-key-{}", pv.id))
            .press_scale(motion::PRESS),
        );
    }
    let name = display_name(&pv.id);
    let mut c = col()
        .key(format!("onboarding-provider-{}", pv.id))
        .w_full()
        .child(view::list_row(
            s,
            "line".into(),
            provider_logo(s, &[pv.id.as_str(), pv.adapter.as_str()], 20.),
            &name,
            Some(
                text(pv.description.clone())
                    .font(xs())
                    .color(p.text_muted)
                    .truncate(),
            ),
            line,
        ));
    if let Some(login) = ps.codex.as_ref().filter(|l| l.provider_id == pv.id) {
        let uri = login.uri.clone();
        c = c.child(
            row()
                .key("codex-login")
                .w_full()
                .px(16.)
                .pb(12.)
                .gap(8.)
                .center_y()
                .transition(motion::reveal())
                .child(muted(
                    p,
                    if login.code.is_empty() {
                        "Finish signing in on the ChatGPT page that just opened."
                    } else {
                        "Enter this code on the ChatGPT page that just opened:"
                    },
                ))
                .child(
                    text(login.code.clone())
                        .font(rune::TextStyle::new(16., 600.).mono())
                        .color(p.text)
                        .selectable(true),
                )
                .child(crate::design::kit::Spinner::new().size(12.).build(k))
                .child(el().grow(1.))
                .child(
                    button_sized("Open page", Look::Ghost, Size::XSmall)
                        .on_press_tx(move |_: &mut State, tx| open_url(tx, &uri))
                        .build(k)
                        .key("onboarding-codex-open")
                        .press_scale(motion::PRESS),
                )
                .child(
                    button_sized("Cancel", Look::Ghost, Size::XSmall)
                        .on_press(|s: &mut State| s.pages.providers.codex = None)
                        .build(k)
                        .key("onboarding-codex-cancel")
                        .press_scale(motion::PRESS),
                ),
        );
    }
    if ps.key_editor.as_deref() == Some(pv.id.as_str()) {
        let provider = pv.clone();
        let saving = s.onboarding.agents.saving_key;
        c = c.child(
            row()
                .key("key-editor")
                .w_full()
                .px(16.)
                .pb(12.)
                .gap(8.)
                .center_y()
                .transition(motion::reveal())
                .child(
                    col().grow(1.).min_w(0.).child(
                        input(&ps.key_input)
                            .secure()
                            .placeholder("Paste the API key")
                            .label("API key")
                            .on_edit(|s: &mut State| &mut s.pages.providers.key_input)
                            .build(k)
                            .key(format!("onboarding-key-input-{}", pv.id)),
                    ),
                )
                .child(
                    crate::pages::common::loading(
                        button_sized("Save", Look::Primary, Size::Small)
                            .on_press(move |s: &mut State| save_key(s, provider.clone())),
                        saving,
                    )
                    .build(k)
                    .key("onboarding-key-save")
                    .press_scale(motion::PRESS),
                )
                .child(
                    button_sized("Cancel", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| s.pages.providers.key_editor = None)
                        .build(k)
                        .key("onboarding-key-cancel")
                        .press_scale(motion::PRESS),
                ),
        );
    }
    c
}
