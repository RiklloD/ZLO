//! Settings → Providers (t3code's provider settings): the providers IronClaw
//! can run on, in two kinds. Accounts and API keys are IronClaw's own LLM
//! backends (ChatGPT plan sign-in, API keys, local servers); CLI agents are
//! coding-agent runtimes that run a thread in their own harness.
//!
//! Connected providers sit on top; "Add a provider" is a searchable grid.
//! "Background tasks" picks one model per background job (thread titles
//! today); it applies to every profile.
//!
//! Port of GPUI `pages/providers.rs`: the state and the gateway calls live
//! here, the rendering in `providers/view.rs`.

pub(crate) mod catalog;
pub mod fixtures;
mod view;

use std::collections::HashMap;

use zlo_client::operator_llm::{LlmConfigSnapshot, LlmProviderView, UserModelCatalog};
use zlo_client::orchestration::{PickerPrefs, RouteProvider};
use zlo_client::task_models::{TaskModelChoice, TaskModels};
use rune::{Cx, El, TextEdit};
use rune_kit::Kit;

use crate::jobs;
use crate::state::State;
use catalog::{Kind, display_name, kind_of};

const CODEX_ADAPTER: &str = "openai_codex";
/// Device codes and browser sign-ins live up to 15 minutes; poll until then.
const CODEX_POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
const CODEX_POLL_FOR: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Expanded model list for one provider.
pub enum Models {
    Loading,
    Loaded(Vec<String>),
    Failed(String),
}

/// A running ChatGPT sign-in: the code to type at `uri`.
pub struct CodexLogin {
    pub provider_id: String,
    pub code: String,
    pub uri: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Accounts,
    Cli,
}

pub struct ProvidersState {
    pub llm: Option<LlmConfigSnapshot>,
    pub models: HashMap<String, Models>,
    /// Models offered by the chat's quick switcher.
    pub catalog: Option<UserModelCatalog>,
    /// The orchestrator's catalog (CLI runtimes and their sign-in state),
    /// read through any thread; `None` until read or without threads.
    pub runtimes: Option<Vec<RouteProvider>>,
    /// The picker preferences read with the runtimes; removing a CLI agent
    /// hides it there.
    pub picker: Option<PickerPrefs>,
    pub catalog_thread: Option<String>,
    /// The quick switcher's "Add model" field is open.
    pub pin_open: bool,
    pub pin_input: TextEdit,
    /// The provider whose API-key field is open, and the field.
    pub key_editor: Option<String>,
    pub key_input: TextEdit,
    /// Connection test results by provider id (`None` while running).
    pub probes: HashMap<String, Option<(bool, String)>>,
    pub confirm_delete: Option<String>,
    pub codex: Option<CodexLogin>,
    pub tab: Tab,
    pub search: TextEdit,
    /// The tile opened from "Add a provider".
    pub adding: Option<String>,
    /// Background-task models; `None` until read (or an older gateway).
    pub task_models: Option<TaskModels>,
    /// The thread-titles model list is open.
    pub titles_open: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
}

impl Default for ProvidersState {
    fn default() -> Self {
        Self {
            llm: None,
            models: HashMap::new(),
            catalog: None,
            runtimes: None,
            picker: None,
            catalog_thread: None,
            pin_open: false,
            pin_input: TextEdit::new(""),
            key_editor: None,
            key_input: TextEdit::new("").secure(),
            probes: HashMap::new(),
            confirm_delete: None,
            codex: None,
            tab: Tab::Accounts,
            search: TextEdit::new(""),
            adding: None,
            task_models: None,
            titles_open: false,
            error: None,
            notice: None,
        }
    }
}

fn st(s: &mut State) -> &mut ProvidersState {
    &mut s.pages.providers
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    // The orchestrator catalog (CLI agents) is read through any thread.
    let catalog = s
        .workspace
        .inbox
        .list
        .rows
        .first()
        .map(|r| r.thread_id.clone());
    set_catalog_thread(s, catalog);
    refresh(s);
}

pub fn set_catalog_thread(s: &mut State, thread: Option<String>) {
    if thread.is_some() {
        st(s).catalog_thread = thread;
    }
}

pub fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    view::page(s, cx, k, layer)
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "providers",
        |c| async move { c.llm_config().await },
        |s, result, _| match result {
            Ok(llm) => st(s).llm = Some(llm),
            Err(e) => st(s).error = Some(e.to_string()),
        },
    );
    jobs::run(
        s,
        "providers",
        |c| async move { c.model_catalog().await },
        // silent-ok: an older gateway without the route just has no switcher.
        |s, result, _| st(s).catalog = result.ok(),
    );
    jobs::run(
        s,
        "providers",
        |c| async move { c.task_models().await },
        // silent-ok: an older gateway has no task models; the section hides.
        |s, result, _| st(s).task_models = result.ok(),
    );
    if let Some(thread) = st(s).catalog_thread.clone() {
        jobs::run(
            s,
            "providers",
            move |c| async move { c.thread_route(&thread).await },
            |s, result, _| {
                // silent-ok: without the route the CLI tab says it cannot tell.
                let route = result.ok();
                let p = st(s);
                p.picker = route.as_ref().map(|r| r.picker.clone());
                p.runtimes = route.map(|r| r.providers);
            },
        );
    }
}

fn fail(s: &mut State, e: impl ToString) {
    st(s).error = Some(e.to_string());
}

fn set_title_model(s: &mut State, choice: Option<TaskModelChoice>) {
    let models = TaskModels {
        thread_titles: choice,
    };
    st(s).titles_open = false;
    jobs::run(
        s,
        "providers",
        move |c| async move { c.set_task_models(&models).await },
        |s, result, _| match result {
            Ok(saved) => st(s).task_models = Some(saved),
            Err(e) => fail(s, e),
        },
    );
}

/// API providers a task can run on, with their models: the routable
/// providers of the orchestrator catalog, or the connected ones with their
/// default model when no thread exists to read the catalog.
fn task_choices(p: &ProvidersState) -> Vec<(String, String, Vec<String>)> {
    let active = p
        .llm
        .as_ref()
        .and_then(|l| l.active.as_ref())
        .map(|a| a.provider_id.clone());
    let mut out: Vec<(String, String, Vec<String>)> = Vec::new();
    for r in p.runtimes.iter().flatten() {
        // The "ironclaw" row is IronClaw's loop on the active provider.
        let id = if r.provider == "ironclaw" {
            match &active {
                Some(a) => a.clone(),
                None => continue,
            }
        } else if r.runtime == "ironclaw" {
            r.provider.clone()
        } else {
            continue;
        };
        if r.models.is_empty() || out.iter().any(|(p, ..)| *p == id) {
            continue;
        }
        out.push((id.clone(), display_name(&id), r.models.clone()));
    }
    if out.is_empty()
        && let Some(llm) = &p.llm
    {
        for (_, provider) in connected(llm) {
            let model = provider
                .active_model
                .clone()
                .unwrap_or_else(|| provider.default_model.clone());
            out.push((provider.id.clone(), display_name(&provider.id), vec![model]));
        }
    }
    out
}

fn set_active(s: &mut State, provider: String, model: Option<String>) {
    st(s).notice = None;
    // With a quick-switch list saved, the gateway runs chats on that list's
    // default (or the person's pick), not the active model. Keep a model
    // pick honest: make the chosen model the default and clear the pick.
    let switcher = st(s)
        .catalog
        .as_ref()
        .filter(|c| c.selection_enabled)
        .map(|c| c.models.clone());
    jobs::run(
        s,
        "providers",
        move |c| async move {
            let llm = c.set_active_llm(&provider, model.clone()).await?;
            let same_provider = llm
                .active
                .as_ref()
                .is_some_and(|a| a.provider_id == provider);
            if let (Some(mut allowed), Some(model), true) = (switcher, model, same_provider) {
                if !allowed.contains(&model) {
                    allowed.push(model.clone());
                }
                c.set_model_policy(model, allowed).await?;
                c.set_model_preference(None).await?;
            }
            Ok(llm)
        },
        |s, result, _| match result {
            Ok(llm) => {
                let active = llm.active.as_ref().map(|a| {
                    format!(
                        "Now using {}{}",
                        a.provider_id,
                        a.model
                            .as_deref()
                            .map(|m| format!(" / {m}"))
                            .unwrap_or_default()
                    )
                });
                let p = st(s);
                p.llm = Some(llm);
                p.error = None;
                p.notice = active;
                refresh(s);
            }
            Err(e) => fail(s, e),
        },
    );
}

/// Add or remove a model from the quick switcher. The default stays the
/// active model when it is pinned, else the first pinned one.
fn toggle_pin(s: &mut State, model: String, active: Option<String>) {
    let mut allowed = st(s)
        .catalog
        .as_ref()
        .map(|c| c.models.clone())
        .unwrap_or_default();
    if let Some(i) = allowed.iter().position(|m| *m == model) {
        allowed.remove(i);
    } else {
        allowed.push(model);
    }
    let Some(default) = active
        .filter(|a| allowed.contains(a))
        .or_else(|| allowed.first().cloned())
    else {
        fail(s, "Keep at least one model in the quick switcher.");
        return;
    };
    jobs::run(
        s,
        "providers",
        move |c| async move { c.set_model_policy(default, allowed).await },
        |s, result, _| match result {
            Ok(catalog) => {
                let p = st(s);
                p.catalog = Some(catalog);
                p.error = None;
            }
            Err(e) => fail(s, e),
        },
    );
}

/// The quick switcher's "Add" button: pin the typed model id.
fn add_pin(s: &mut State) {
    let input = &mut st(s).pin_input;
    let model = input.text.trim().to_string();
    if model.is_empty() {
        return;
    }
    input.clear();
    let active = st(s)
        .llm
        .as_ref()
        .and_then(|l| l.active.as_ref())
        .and_then(|a| a.model.clone());
    toggle_pin(s, model, active);
}

fn toggle_models(s: &mut State, provider: LlmProviderView) {
    if st(s).models.remove(&provider.id).is_some() {
        return;
    }
    let id = provider.id.clone();
    st(s).models.insert(id.clone(), Models::Loading);
    jobs::run(
        s,
        "providers",
        move |c| async move { c.list_models(&provider).await },
        move |s, result, _| {
            let models = match result {
                Ok(r) if r.ok => Models::Loaded(r.models),
                Ok(r) => Models::Failed(r.message),
                Err(e) => Models::Failed(e.to_string()),
            };
            // Only fill the list if it is still open.
            if let Some(slot) = st(s).models.get_mut(&id) {
                *slot = models;
            }
        },
    );
}

fn test_connection(s: &mut State, provider: LlmProviderView) {
    let id = provider.id.clone();
    st(s).probes.insert(id.clone(), None);
    jobs::run(
        s,
        "providers",
        move |c| async move { c.test_llm_connection(&provider).await },
        move |s, result, _| {
            let outcome = match result {
                Ok(r) => (r.ok, r.message),
                Err(e) => (false, e.to_string()),
            };
            st(s).probes.insert(id, Some(outcome));
        },
    );
}

fn delete_provider(s: &mut State, id: String) {
    st(s).confirm_delete = None;
    jobs::run(
        s,
        "providers",
        move |c| async move { c.delete_llm_provider(&id).await.map(|l| (id, l)) },
        |s, result, _| match result {
            Ok((id, llm)) => {
                let p = st(s);
                p.llm = Some(llm);
                p.error = None;
                p.notice = Some(format!("Removed {} and its stored key.", display_name(&id)));
            }
            Err(e) => fail(s, e),
        },
    );
}

/// Remove a CLI agent from ZLO, or bring it back. The binary stays
/// installed; the agent is hidden from the model picker and this list.
fn toggle_cli_hidden(s: &mut State, provider: String) {
    let p = st(s);
    let (Some(thread), Some(prefs)) = (p.catalog_thread.clone(), p.picker.as_mut()) else {
        fail(s, "Start one thread first, then remove CLI agents here.");
        return;
    };
    prefs.toggle_provider(&provider);
    let prefs = prefs.clone();
    jobs::run(
        s,
        "providers",
        move |c| async move { c.set_model_picker(&thread, &prefs).await },
        |s, result, _| {
            if let Err(e) = result {
                fail(s, e);
            }
            refresh(s);
        },
    );
}

/// Show the device code, open the sign-in page, then wait for the gateway
/// to make Codex active.
pub(crate) fn start_codex(s: &mut State, provider_id: String) {
    st(s).notice = None;
    jobs::run(
        s,
        "providers",
        |c| async move { c.start_codex_login().await },
        move |s, result, tx| match result {
            Ok(start) => {
                crate::pages::common::controls::open_url(tx, &start.verification_uri);
                let p = st(s);
                p.codex = Some(CodexLogin {
                    provider_id: provider_id.clone(),
                    code: start.user_code,
                    uri: start.verification_uri,
                });
                p.error = None;
                wait_for_codex(s, provider_id);
            }
            Err(e) => fail(s, e),
        },
    );
}

fn wait_for_codex(s: &mut State, provider_id: String) {
    let target = provider_id.clone();
    jobs::run(
        s,
        "providers",
        move |c| async move {
            let deadline = tokio::time::Instant::now() + CODEX_POLL_FOR;
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(CODEX_POLL_EVERY).await;
                let llm = c.llm_config().await?;
                if llm.active.as_ref().is_some_and(|a| a.provider_id == target) {
                    return Ok(Some(llm));
                }
            }
            Ok(None)
        },
        move |s, result, _| {
            // Cancelled or replaced while waiting: drop the result.
            if st(s).codex.as_ref().map(|l| &l.provider_id) != Some(&provider_id) {
                return;
            }
            st(s).codex = None;
            match result {
                Ok(Some(llm)) => {
                    let p = st(s);
                    p.llm = Some(llm);
                    p.notice = Some(
                        "Signed in with ChatGPT. Your plan is now the active provider.".into(),
                    );
                    refresh(s);
                }
                Ok(None) => fail(s, "The sign-in expired. Start again."),
                Err(e) => fail(s, e),
            }
        },
    );
}

fn open_key_editor(s: &mut State, id: String) {
    let p = st(s);
    p.key_input.clear();
    p.key_input.focused = true;
    p.key_editor = Some(id);
}

fn open_pin_input(s: &mut State) {
    let p = st(s);
    p.pin_input.clear();
    p.pin_input.focused = true;
    p.pin_open = true;
}

fn save_key(s: &mut State, provider: LlmProviderView) {
    if st(s).key_editor.is_none() {
        return;
    }
    let input = &mut st(s).key_input;
    let key = input.text.trim().to_string();
    if key.is_empty() {
        return;
    }
    // Drop the plaintext from the input right away; the request owns it now.
    input.clear();
    jobs::run(
        s,
        "providers",
        move |c| async move { c.set_provider_key(&provider, &key).await },
        |s, result, _| match result {
            Ok(llm) => {
                let p = st(s);
                p.llm = Some(llm);
                p.key_editor = None;
                p.error = None;
                p.notice = Some("API key saved to the encrypted secret store.".into());
            }
            Err(e) => fail(s, e),
        },
    );
}

/// Connected providers (active or with a stored key), the active one first.
fn connected(llm: &LlmConfigSnapshot) -> Vec<(usize, &LlmProviderView)> {
    let mut rows: Vec<_> = llm
        .providers
        .iter()
        .enumerate()
        .filter(|(_, p)| p.active || p.api_key_set)
        .collect();
    rows.sort_by_key(|(_, p)| !p.active);
    rows
}

fn open_tile(s: &mut State, provider: &LlmProviderView) {
    let p = st(s);
    if p.adding.as_deref() == Some(provider.id.as_str()) {
        p.adding = None;
        p.key_editor = None;
    } else {
        p.adding = Some(provider.id.clone());
        p.key_editor = None;
        // An API-key provider's only step is the key: open it right away.
        if kind_of(&provider.id) == Kind::ApiKey && provider.accepts_api_key {
            open_key_editor(s, provider.id.clone());
        }
    }
}
