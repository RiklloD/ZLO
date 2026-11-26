//! Setup's Memory step: where agents remember things. One of two, before
//! the person goes on:
//!
//! - **Supermemory** (cloud): paste an API key, or use the one Hermes
//!   already holds. The gateway tests the key with Supermemory before it
//!   stores it.
//! - **Local knowledge wiki**: Karpathy's LLM wiki in ZLO local memory
//!   (`wiki/SCHEMA.md`, `index.md`, `log.md`), which agents then keep.
//!
//! A computer whose gateway offers neither (an older backend) does not hold
//! setup up.

use rune::{El, TextEdit, col, row};
use rune_kit::Kit;
use zlo_client::memory_page::MemoryWikiStatus;
use zlo_client::service_keys::ServiceKeyStatus;

use super::view::{StepView, list_row, section, step_shell};
use crate::design::motion;
use crate::jobs::{self, Generation};
use crate::pages::common::controls::{
    Look, Size, button_sized, input, muted, open_url, spinner_line,
};
use crate::pages::common::{Tone, tag};
use crate::state::State;

const SUPERMEMORY_KEY_ID: &str = "supermemory_api_key";

pub struct MemoryStepState {
    /// The Supermemory key row; `None` while loading or when this gateway
    /// has no Keys route (or the person is not its operator).
    pub key: Option<ServiceKeyStatus>,
    /// The wiki; `None` while loading or when the gateway has no wiki route.
    pub wiki: Option<MemoryWikiStatus>,
    pub loaded: bool,
    pub input: TextEdit,
    pub busy: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    load: Generation,
}

impl Default for MemoryStepState {
    fn default() -> Self {
        Self {
            key: None,
            wiki: None,
            loaded: false,
            input: TextEdit::new("").secure(),
            busy: false,
            error: None,
            notice: None,
            load: Generation::default(),
        }
    }
}

fn st(s: &mut State) -> &mut MemoryStepState {
    &mut s.onboarding.memory
}

/// Supermemory has a key, or the wiki exists.
pub fn chosen(m: &MemoryStepState) -> bool {
    m.key.as_ref().is_some_and(|k| k.stored) || m.wiki.as_ref().is_some_and(|w| w.initialized)
}

/// Continue: once a choice is made, or when this gateway offers neither.
pub fn can_continue(m: &MemoryStepState) -> bool {
    chosen(m) || (m.loaded && m.key.is_none() && !m.wiki.as_ref().is_some_and(|w| w.available))
}

pub fn entered(s: &mut State) {
    if s.fixture {
        return;
    }
    let generation = st(s).load.bump();
    jobs::run(
        s,
        "onboarding-memory",
        |c| async move {
            // silent-ok: an older gateway, or a non-operator, has no Keys route; the row then hides.
            let key = c.service_keys().await.ok().and_then(|list| {
                list.keys
                    .into_iter()
                    .find(|key| key.id == SUPERMEMORY_KEY_ID)
            });
            // silent-ok: an older gateway has no wiki route; the row then hides.
            let wiki = c.memory_wiki_status().await.ok();
            Ok::<_, zlo_client::ClientError>((key, wiki))
        },
        move |s, result, _| {
            let m = st(s);
            if m.load != generation {
                return;
            }
            m.loaded = true;
            if let Ok((key, wiki)) = result {
                m.key = key;
                m.wiki = wiki;
            }
        },
    );
}

/// Setup switched computers: what it knew about the old one is gone.
pub fn reset(s: &mut State) {
    s.onboarding.memory = MemoryStepState::default();
}

fn after_key(
    s: &mut State,
    result: Result<zlo_client::service_keys::ServiceKeySetResponse, zlo_client::ClientError>,
) {
    let m = st(s);
    m.busy = false;
    match result {
        Ok(response) => {
            m.error = None;
            m.notice = Some(crate::pages::keys::describe_set("Supermemory", &response));
            if let Some(key) = m.key.as_mut() {
                key.stored = response.stored;
                key.import_from = None;
            }
            crate::pages::memory::key_changed(s);
        }
        Err(e) => m.error = Some(crate::pages::keys::describe_error(&e, "Supermemory")),
    }
}

fn save_key(s: &mut State) {
    let value = st(s).input.text.trim().to_string();
    if value.is_empty() {
        return;
    }
    let m = st(s);
    m.input.clear();
    m.busy = true;
    m.notice = None;
    jobs::run(
        s,
        "onboarding-memory",
        move |c| async move { c.service_key_set(SUPERMEMORY_KEY_ID, &value).await },
        |s, result, tx| {
            tx.animated();
            after_key(s, result);
        },
    );
}

fn import_key(s: &mut State) {
    let m = st(s);
    m.busy = true;
    m.notice = None;
    jobs::run(
        s,
        "onboarding-memory",
        |c| async move { c.service_key_import(SUPERMEMORY_KEY_ID).await },
        |s, result, tx| {
            tx.animated();
            after_key(s, result);
        },
    );
}

fn build_wiki(s: &mut State) {
    let m = st(s);
    m.busy = true;
    m.notice = None;
    jobs::run(
        s,
        "onboarding-memory",
        |c| async move { c.memory_wiki_init().await },
        |s, result, tx| {
            tx.animated();
            let m = st(s);
            m.busy = false;
            match result {
                Ok(done) if done.status.initialized => {
                    m.error = None;
                    m.notice =
                        Some("Wiki built. Your agents keep it under wiki/ in local memory.".into());
                    m.wiki = Some(done.status);
                }
                Ok(_) => m.error = Some("The wiki could not be read back; try again.".into()),
                Err(e) => m.error = Some(e.to_string()),
            }
        },
    );
}

fn supermemory_row(s: &State, k: &Kit, key: &ServiceKeyStatus) -> El {
    let p = &s.palette;
    let m = &s.onboarding.memory;
    let line = if key.stored {
        "Connected. Agents recall from and save to your Supermemory account.".to_string()
    } else if let Some(source) = &key.import_from {
        format!("Found a key in {source}. Use it, or paste another one.")
    } else {
        "Cloud memory shared by every agent and computer. Paste your API key.".to_string()
    };
    let lead = rune::icon(&crate::icons::CLOUD, 16.).color(p.text_muted);
    let trailing = if key.stored {
        tag(p, "Connected", Tone::Success)
    } else {
        rune::el()
    };
    let mut block = col().w_full().child(list_row(
        s,
        "memory-supermemory".into(),
        lead,
        "Supermemory (cloud)",
        Some(muted(p, line)),
        trailing,
    ));
    if !key.stored {
        let mut actions = row().w_full().gap(8.).px(16.).pb(12.).center_y();
        if key.import_from.is_some() {
            actions = actions.child(
                button_sized("Use the key from Hermes", Look::Primary, Size::Small)
                    .disabled(m.busy)
                    .on_press(import_key)
                    .build(k)
                    .key("memory-import-key")
                    .press_scale(motion::PRESS),
            );
        }
        actions = actions
            .child(
                col().grow(1.).min_w(0.).child(
                    input(&m.input)
                        .secure()
                        .placeholder("Paste your Supermemory API key")
                        .label("Supermemory API key")
                        .on_edit(|s: &mut State| &mut s.onboarding.memory.input)
                        .on_submit(|s: &mut State, _| save_key(s))
                        .build(k)
                        .key("memory-key-input"),
                ),
            )
            .child(
                button_sized("Save key", Look::Secondary, Size::Small)
                    .disabled(m.busy)
                    .on_press(save_key)
                    .build(k)
                    .key("memory-save-key")
                    .press_scale(motion::PRESS),
            );
        if let Some(url) = key.docs_url.clone() {
            actions = actions.child(
                button_sized("Get a key", Look::Ghost, Size::Small)
                    .on_press_tx(move |_: &mut State, tx| open_url(tx, &url))
                    .build(k)
                    .key("memory-get-key")
                    .press_scale(motion::PRESS),
            );
        }
        block = block.child(actions);
    }
    block
}

fn wiki_row(s: &State, k: &Kit, wiki: &MemoryWikiStatus) -> El {
    let p = &s.palette;
    let m = &s.onboarding.memory;
    let lead = rune::icon(&crate::icons::BOOK_OPEN, 16.).color(p.text_muted);
    let (line, trailing) = if wiki.initialized {
        (
            format!(
                "Built: {} page{} under wiki/ in local memory.",
                wiki.pages,
                if wiki.pages == 1 { "" } else { "s" }
            ),
            tag(p, "Built", Tone::Success),
        )
    } else {
        (
            "Agents build and keep a wiki on this computer: sources, people, projects \
             and ideas as linked pages, with an index and a change log (Karpathy's LLM wiki)."
                .to_string(),
            button_sized("Build the wiki", Look::Secondary, Size::Small)
                .disabled(m.busy)
                .on_press(build_wiki)
                .build(k)
                .key("memory-build-wiki")
                .press_scale(motion::PRESS),
        )
    };
    list_row(
        s,
        "memory-wiki".into(),
        lead,
        "Local knowledge wiki",
        Some(muted(p, line)),
        trailing,
    )
}

pub fn view(s: &State, k: &Kit) -> StepView {
    let p = &s.palette;
    let m = &s.onboarding.memory;
    let mut body = step_shell(
        s,
        "Choose your memory",
        "Where your agents remember what they learn: your Supermemory account in the \
         cloud, or a knowledge wiki they build on this computer. Pick one now; you can \
         add the other later in Settings.",
    );
    if !m.loaded {
        body = body.child(
            spinner_line(p, k, "Checking memory…")
                .key("memory-loading")
                .pt(16.),
        );
    } else {
        let mut rows = Vec::new();
        if let Some(key) = &m.key {
            rows.push(supermemory_row(s, k, key));
        }
        if let Some(wiki) = m.wiki.as_ref().filter(|w| w.available) {
            rows.push(wiki_row(s, k, wiki));
        }
        if rows.is_empty() {
            body = body.child(
                muted(
                    p,
                    "This computer's ZLO has neither choice yet; update it to pick one.",
                )
                .key("memory-none")
                .pt(16.),
            );
        } else {
            body = body.child(section(s, "memory-choices", None, rows));
        }
    }
    if let Some(e) = &m.error {
        body = body.child(
            muted(p, e.clone())
                .color(p.danger)
                .key("memory-error")
                .pt(10.)
                .transition(motion::reveal()),
        );
    }
    if let Some(n) = &m.notice {
        body = body.child(
            muted(p, n.clone())
                .key("memory-notice")
                .pt(10.)
                .transition(motion::reveal()),
        );
    }
    if m.loaded && !can_continue(m) {
        body = body.child(
            muted(p, "Pick one to continue.")
                .key("memory-pick-one")
                .pt(10.)
                .transition(motion::reveal()),
        );
    }
    StepView::new(body).action(
        button_sized("Continue", Look::Primary, Size::Small)
            .icon(&crate::icons::ARROW_RIGHT)
            .disabled(!can_continue(m) || m.busy)
            .on_press(|s: &mut State| super::go_to(s, super::Step::Projects))
            .build(k)
            .key("memory-continue")
            .press_scale(motion::PRESS),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(stored: bool) -> ServiceKeyStatus {
        ServiceKeyStatus {
            id: SUPERMEMORY_KEY_ID.into(),
            label: "Supermemory".into(),
            purpose: String::new(),
            docs_url: None,
            stored,
            import_from: None,
        }
    }

    fn wiki(available: bool, initialized: bool) -> MemoryWikiStatus {
        MemoryWikiStatus {
            available,
            initialized,
            pages: usize::from(initialized) * 3,
        }
    }

    #[test]
    fn continue_needs_a_key_or_a_wiki_unless_the_gateway_offers_neither() {
        let mut m = MemoryStepState::default();
        assert!(!can_continue(&m), "still loading");
        m.loaded = true;
        m.key = Some(key(false));
        m.wiki = Some(wiki(true, false));
        assert!(!can_continue(&m), "a choice is required");
        m.key = Some(key(true));
        assert!(can_continue(&m), "Supermemory key stored");
        m.key = Some(key(false));
        m.wiki = Some(wiki(true, true));
        assert!(can_continue(&m), "wiki built");
        m.key = None;
        m.wiki = Some(wiki(false, false));
        assert!(can_continue(&m), "an older gateway never blocks setup");
    }
}
