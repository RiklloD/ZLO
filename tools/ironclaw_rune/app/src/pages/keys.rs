//! Settings → Keys: every key ZLO holds, in one place.
//!
//! - **Services**: keys built-in services need (Supermemory). Add, replace
//!   (the gateway tests the key with the service before it stores it),
//!   remove, or use the copy Hermes already holds.
//! - **Plugins**: installed plugins that take a token; set up on their
//!   Plugins card.
//! - **Model providers**: which providers have a stored key; managed on
//!   Providers.
//!
//! A key value goes up once and never comes back: the page only knows
//! whether one is stored.

use rune::{Cx, El, TextEdit, col, row, text};
use rune_kit::Kit;
use zlo_client::ClientError;
use zlo_client::plugins::PluginState;
use zlo_client::product_wire::RebornExtensionSurface;
use zlo_client::service_keys::{ServiceKeyCheck, ServiceKeySetResponse, ServiceKeyStatus};
use zlo_client::wire::InstalledExtension;

use crate::design::motion;
use crate::jobs;
use crate::pages::common::controls::{
    Look, Size, button_sized, header_with, input, muted, open_url, outlined_tag, refresh_icon,
    section_title, spinner,
};
use crate::pages::common::ts::sm_medium;
use crate::pages::common::{Tone, empty_state, error_line, notice_line, page_body, rows_card};
use crate::state::{Page, State};

pub struct KeysState {
    pub keys: Option<Vec<ServiceKeyStatus>>,
    /// The key whose editor is open.
    pub editing: Option<String>,
    pub input: TextEdit,
    /// The key a request is running for.
    pub busy: Option<String>,
    pub confirm_remove: Option<String>,
    pub error: Option<String>,
    pub notice: Option<String>,
}

impl Default for KeysState {
    fn default() -> Self {
        Self {
            keys: None,
            editing: None,
            input: TextEdit::new("").secure(),
            busy: None,
            confirm_remove: None,
            error: None,
            notice: None,
        }
    }
}

fn st(s: &mut State) -> &mut KeysState {
    &mut s.pages.keys
}

pub fn opened(s: &mut State) {
    refresh(s);
    crate::pages::extensions::refresh(s);
    crate::pages::providers::refresh(s);
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "keys",
        |c| async move { c.service_keys().await },
        |s, result, _| {
            let k = st(s);
            match result {
                Ok(list) => {
                    k.keys = Some(list.keys);
                    k.error = None;
                }
                Err(e) => k.error = Some(describe_error(&e, "")),
            }
        },
    );
}

/// The person-facing line for a stored key.
pub fn describe_set(label: &str, response: &ServiceKeySetResponse) -> String {
    let from = response
        .source
        .as_deref()
        .map(|source| format!(" from {source}"))
        .unwrap_or_default();
    match (response.stored, response.check) {
        (false, _) => format!("{label}: the key did not save. Try again."),
        (true, ServiceKeyCheck::Verified) => format!("{label} key saved{from} and working."),
        (true, ServiceKeyCheck::Unverified) => format!(
            "{label} key saved{from}, but {label} could not be reached to test it. \
             It will be used once it answers."
        ),
        (true, ServiceKeyCheck::NotChecked) => format!("{label} key saved{from}."),
    }
}

/// The person-facing line for a failed key call: a refused key says so,
/// anything else keeps the gateway's sanitized message.
pub fn describe_error(error: &ClientError, label: &str) -> String {
    match error {
        ClientError::Api { body, .. } if body.field.as_deref() == Some("value") => {
            match body.validation_code.as_deref() {
                Some("blank") => "Paste a key first.".to_string(),
                Some("too_long") => "That is too long to be a key.".to_string(),
                Some("invalid_control_character") => {
                    "A key has no spaces or line breaks; check what was pasted.".to_string()
                }
                _ => format!("{label} refused this key. Check it and paste it again."),
            }
        }
        ClientError::Api { status: 503, .. } => format!(
            "{label} could not be reached to test the key, so it was not saved. \
             Try again in a moment."
        ),
        ClientError::Api { status: 403, .. } => {
            "Only this computer's operator can change keys.".to_string()
        }
        other => other.to_string(),
    }
}

fn label_of(s: &State, id: &str) -> String {
    s.pages
        .keys
        .keys
        .as_ref()
        .and_then(|keys| keys.iter().find(|k| k.id == id))
        .map(|k| k.label.clone())
        .unwrap_or_else(|| "The service".to_string())
}

fn open_editor(s: &mut State, id: String) {
    let k = st(s);
    k.input.clear();
    k.input.focused = true;
    k.editing = Some(id);
    k.confirm_remove = None;
}

fn save(s: &mut State) {
    let Some(id) = st(s).editing.clone() else {
        return;
    };
    let value = st(s).input.text.trim().to_string();
    if value.is_empty() {
        return;
    }
    // The request owns the plaintext now; the field forgets it.
    st(s).input.clear();
    let label = label_of(s, &id);
    st(s).busy = Some(id.clone());
    jobs::run(
        s,
        "keys",
        move |c| async move { c.service_key_set(&id, &value).await },
        move |s, result, tx| {
            tx.animated();
            let k = st(s);
            k.busy = None;
            match result {
                Ok(response) => {
                    k.editing = None;
                    k.error = None;
                    k.notice = Some(describe_set(&label, &response));
                    key_changed(s);
                }
                Err(e) => k.error = Some(describe_error(&e, &label)),
            }
        },
    );
}

fn import(s: &mut State, id: String) {
    let label = label_of(s, &id);
    st(s).busy = Some(id.clone());
    jobs::run(
        s,
        "keys",
        move |c| async move { c.service_key_import(&id).await },
        move |s, result, tx| {
            tx.animated();
            let k = st(s);
            k.busy = None;
            match result {
                Ok(response) => {
                    k.error = None;
                    k.notice = Some(describe_set(&label, &response));
                    key_changed(s);
                }
                Err(e) => k.error = Some(describe_error(&e, &label)),
            }
        },
    );
}

fn remove(s: &mut State, id: String) {
    let label = label_of(s, &id);
    let k = st(s);
    k.busy = Some(id.clone());
    k.confirm_remove = None;
    jobs::run(
        s,
        "keys",
        move |c| async move { c.service_key_remove(&id).await },
        move |s, result, tx| {
            tx.animated();
            let k = st(s);
            k.busy = None;
            match result {
                Ok(response) if !response.stored => {
                    k.error = None;
                    k.notice = Some(format!("{label} key removed."));
                    key_changed(s);
                }
                Ok(_) => k.error = Some(format!("{label} key is still stored; try again.")),
                Err(e) => k.error = Some(describe_error(&e, &label)),
            }
        },
    );
}

/// A stored key changed: this page and the Memory page read it again.
fn key_changed(s: &mut State) {
    refresh(s);
    crate::pages::memory::key_changed(s);
}

/// Installed plugins that take a key or a sign-in (channels pair instead).
fn keyed_plugins(installed: &[InstalledExtension]) -> Vec<&InstalledExtension> {
    installed
        .iter()
        .filter(|e| !e.is_channel())
        .filter(|e| {
            e.onboarding.is_some()
                || e.surfaces
                    .iter()
                    .any(|surface| matches!(surface, RebornExtensionSurface::Auth))
        })
        .collect()
}

fn service_row(s: &State, k: &Kit, key: &ServiceKeyStatus) -> El {
    let p = &s.palette;
    let ks = &s.pages.keys;
    let busy = ks.busy.as_deref() == Some(key.id.as_str());
    let mut info = col()
        .grow(1.)
        .min_w(0.)
        .gap(2.)
        .child(text(key.label.clone()).font(sm_medium()).color(p.text))
        .child(muted(p, key.purpose.clone()));
    if let Some(source) = &key.import_from {
        info = info.child(muted(
            p,
            if key.stored {
                format!("{source} holds a different key.")
            } else {
                format!("Found in {source}.")
            },
        ));
    }
    let mut actions = row().gap(8.).center_y();
    let id = key.id.clone();
    if key.import_from.is_some() {
        let id = id.clone();
        actions = actions.child(
            button_sized(
                if key.stored {
                    "Use Hermes' key"
                } else {
                    "Use it"
                },
                Look::Primary,
                Size::Small,
            )
            .disabled(busy)
            .on_press(move |s: &mut State| import(s, id.clone()))
            .build(k)
            .key(format!("import-{}", key.id))
            .press_scale(motion::PRESS),
        );
    }
    {
        let id = id.clone();
        actions = actions.child(
            button_sized(
                if key.stored { "Replace" } else { "Add key" },
                if key.stored || key.import_from.is_some() {
                    Look::Secondary
                } else {
                    Look::Primary
                },
                Size::Small,
            )
            .disabled(busy)
            .on_press(move |s: &mut State| open_editor(s, id.clone()))
            .build(k)
            .key(format!("edit-{}", key.id))
            .press_scale(motion::PRESS),
        );
    }
    if key.stored {
        let confirming = ks.confirm_remove.as_deref() == Some(key.id.as_str());
        let id = id.clone();
        actions = actions.child(
            button_sized(
                if confirming { "Remove now" } else { "Remove" },
                if confirming {
                    Look::Danger
                } else {
                    Look::Ghost
                },
                Size::Small,
            )
            .disabled(busy)
            .on_press(move |s: &mut State| {
                if st(s).confirm_remove.as_deref() == Some(id.as_str()) {
                    remove(s, id.clone());
                } else {
                    st(s).confirm_remove = Some(id.clone());
                }
            })
            .build(k)
            .key(format!("remove-{}", key.id))
            .press_scale(motion::PRESS),
        );
    }
    if let Some(url) = key.docs_url.clone() {
        actions = actions.child(
            button_sized("Get a key", Look::Ghost, Size::Small)
                .on_press_tx(move |_: &mut State, tx| open_url(tx, &url))
                .build(k)
                .key(format!("docs-{}", key.id))
                .press_scale(motion::PRESS),
        );
    }
    let mut block = col().key(format!("key-{}", key.id)).w_full().child(
        row()
            .w_full()
            .gap(16.)
            .px(16.)
            .py(12.)
            .items_start()
            .child(info)
            .child(if busy {
                spinner(k).key("key-busy")
            } else if key.stored {
                outlined_tag(p, Tone::Success, "Stored")
            } else {
                outlined_tag(p, Tone::Secondary, "Not set")
            }),
    );
    block = block.child(actions.px(16.).pb(12.));
    if ks.editing.as_deref() == Some(key.id.as_str()) {
        block = block.child(
            row()
                .key(format!("key-editor-{}", key.id))
                .transition(motion::reveal())
                .w_full()
                .gap(8.)
                .px(16.)
                .pb(12.)
                .center_y()
                .child(
                    col().grow(1.).min_w(0.).child(
                        input(&ks.input)
                            .secure()
                            .placeholder(format!("Paste your {} API key", key.label))
                            .label("API key")
                            .on_edit(|s: &mut State| &mut s.pages.keys.input)
                            .on_submit(|s: &mut State, _| save(s))
                            .build(k)
                            .key(format!("key-input-{}", key.id)),
                    ),
                )
                .child(
                    button_sized("Save", Look::Primary, Size::Small)
                        .disabled(busy)
                        .on_press(|s: &mut State| save(s))
                        .build(k)
                        .key(format!("save-{}", key.id))
                        .press_scale(motion::PRESS),
                )
                .child(
                    button_sized("Cancel", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| {
                            let k = st(s);
                            k.editing = None;
                            k.input.clear();
                        })
                        .build(k)
                        .key(format!("cancel-{}", key.id))
                        .press_scale(motion::PRESS),
                ),
        );
    }
    block
}

fn plugin_row(s: &State, k: &Kit, ext: &InstalledExtension) -> El {
    let p = &s.palette;
    let ready = ext.installation_state == PluginState::Active;
    let id = ext.id().to_string();
    row()
        .key(format!("plugin-{id}"))
        .w_full()
        .gap(16.)
        .px(16.)
        .py(12.)
        .center_y()
        .child(
            col()
                .grow(1.)
                .min_w(0.)
                .child(
                    text(ext.display_name.clone())
                        .font(sm_medium())
                        .color(p.text),
                )
                .child(muted(
                    p,
                    if ready {
                        "Connected."
                    } else {
                        "Needs a key or a sign-in."
                    },
                )),
        )
        .child(if ready {
            outlined_tag(p, Tone::Success, "Ready")
        } else {
            outlined_tag(p, Tone::Warning, "Set up")
        })
        .child(
            button_sized(
                if ready { "Change" } else { "Set up" },
                Look::Secondary,
                Size::Small,
            )
            .on_press(move |s: &mut State| {
                crate::workspace::show(s, Page::Extensions);
                crate::pages::extensions::open_setup_for(s, id.clone());
            })
            .build(k)
            .key(format!("plugin-setup-{}", ext.id()))
            .press_scale(motion::PRESS),
        )
}

fn provider_rows(s: &State, k: &Kit) -> Vec<El> {
    let p = &s.palette;
    let Some(llm) = s.pages.providers.llm.as_ref() else {
        return Vec::new();
    };
    llm.providers
        .iter()
        .filter(|provider| provider.api_key_set)
        .map(|provider| {
            row()
                .key(format!("provider-{}", provider.id))
                .w_full()
                .gap(16.)
                .px(16.)
                .py(12.)
                .center_y()
                .child(
                    text(crate::pages::providers::catalog::display_name(&provider.id))
                        .font(sm_medium())
                        .color(p.text)
                        .grow(1.)
                        .min_w(0.),
                )
                .child(outlined_tag(p, Tone::Success, "Stored"))
                .child(
                    button_sized("Manage", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| crate::workspace::show(s, Page::Providers))
                        .build(k)
                        .key(format!("provider-manage-{}", provider.id))
                        .press_scale(motion::PRESS),
                )
        })
        .collect()
}

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let ks = &s.pages.keys;
    let actions = vec![
        refresh_icon()
            .on_press(|s: &mut State| opened(s))
            .build(k)
            .key("refresh-keys")
            .press_scale(motion::PRESS),
    ];
    let mut body = col().w_full().child(header_with(p, "Keys", actions));
    if let Some(e) = &ks.error {
        body = body.child(error_line(p, e).key("keys-error"));
    }
    if let Some(n) = &ks.notice {
        body = body.child(notice_line(n, p.success).key("keys-notice"));
    }
    body = body.child(section_title(p, "Services", 0.));
    body = body.child(match &ks.keys {
        None => spinner(k).key("keys-loading").transition(motion::fade()),
        Some(keys) if keys.is_empty() => empty_state(p, "This gateway has no service keys."),
        Some(keys) => rows_card(p, keys.iter().map(|key| service_row(s, k, key)))
            .key("keys-services")
            .transition(motion::fade()),
    });
    let plugins = keyed_plugins(&s.pages.extensions.installed);
    body = body.child(section_title(p, "Plugins", 20.));
    body = body.child(if plugins.is_empty() {
        muted(
            p,
            "No installed plugin takes a key. Add plugins on the Plugins page.",
        )
        .key("keys-no-plugins")
    } else {
        rows_card(p, plugins.into_iter().map(|ext| plugin_row(s, k, ext))).key("keys-plugins")
    });
    let providers = provider_rows(s, k);
    body = body.child(section_title(p, "Model providers", 20.));
    body = body.child(if providers.is_empty() {
        muted(
            p,
            "No provider API key is stored. Sign in or add one on Providers.",
        )
        .key("keys-no-providers")
    } else {
        rows_card(p, providers).key("keys-providers")
    });
    page_body("keys-page", body)
}

/// `--scene keys`: Supermemory not set with a copy found in Hermes.
pub fn fixture_keys(s: &mut State) {
    s.fixture = true;
    s.conn = crate::state::Conn::Ready;
    s.page = Page::Keys;
    s.pages.keys.keys = Some(vec![ServiceKeyStatus {
        id: "supermemory_api_key".into(),
        label: "Supermemory".into(),
        purpose: "Cloud memory: agents recall from and save to your Supermemory account \
                  instead of local memory."
            .into(),
        docs_url: Some("https://console.supermemory.ai".into()),
        stored: false,
        import_from: Some("Hermes (10 profiles)".into()),
    }]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlo_client::ClientError;

    fn api(field: Option<&str>, code: Option<&str>) -> ClientError {
        ClientError::Api {
            status: 400,
            message: "invalid".into(),
            body: Box::new(zlo_client::ApiErrorBody {
                error: None,
                kind: None,
                retryable: false,
                field: field.map(Into::into),
                validation_code: code.map(Into::into),
            }),
        }
    }

    #[test]
    fn a_refused_key_reads_as_refused_by_the_service() {
        let line = describe_error(&api(Some("value"), Some("invalid_value")), "Supermemory");
        assert_eq!(
            line,
            "Supermemory refused this key. Check it and paste it again."
        );
        assert_eq!(
            describe_error(&api(Some("value"), Some("blank")), "X"),
            "Paste a key first."
        );
    }

    #[test]
    fn an_unreachable_service_says_nothing_was_saved() {
        let error = ClientError::Api {
            status: 503,
            message: "unavailable".into(),
            body: Box::new(zlo_client::ApiErrorBody {
                error: None,
                kind: None,
                retryable: true,
                field: None,
                validation_code: None,
            }),
        };
        assert!(describe_error(&error, "Supermemory").contains("not saved"));
    }

    #[test]
    fn set_lines_follow_the_evidence() {
        let response = |stored, check, source: Option<&str>| ServiceKeySetResponse {
            id: "k".into(),
            stored,
            check,
            source: source.map(Into::into),
        };
        assert_eq!(
            describe_set(
                "Supermemory",
                &response(true, ServiceKeyCheck::Verified, Some("Hermes"))
            ),
            "Supermemory key saved from Hermes and working."
        );
        assert!(
            describe_set("S", &response(true, ServiceKeyCheck::Unverified, None))
                .contains("could not be reached")
        );
        assert!(
            describe_set("S", &response(false, ServiceKeyCheck::Verified, None))
                .contains("did not save")
        );
    }
}
