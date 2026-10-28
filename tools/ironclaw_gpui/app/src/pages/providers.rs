//! Settings → Providers (t3code's provider settings): the providers IronClaw
//! can run on, in two kinds. Accounts and API keys are IronClaw's own LLM
//! backends (ChatGPT plan sign-in, API keys, local servers); CLI agents are
//! coding-agent runtimes that run a thread in their own harness.
//!
//! Connected providers sit on top; "Add a provider" is a searchable grid.
//! "Background tasks" picks one model per background job (thread titles
//! today); it applies to every profile.

mod catalog;

use std::collections::HashMap;

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::operator_llm::{LlmConfigSnapshot, LlmProviderView, UserModelCatalog};
use ironclaw_gpui_client::orchestration::{PickerPrefs, RouteProvider};
use ironclaw_gpui_client::task_models::{TaskModelChoice, TaskModels};

use super::{PageEvent, error_line, header, page_body, refresh_button};
use crate::design::brands::provider_logo;
use crate::jobs;
use crate::ui::*;
use catalog::{KNOWN_CLIS, Kind, display_name, kind_of};

const CODEX_ADAPTER: &str = "openai_codex";
/// Device codes and browser sign-ins live up to 15 minutes; poll until then.
const CODEX_POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
const CODEX_POLL_FOR: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const TILE_WIDTH: f32 = 196.;

/// Expanded model list for one provider.
enum Models {
    Loading,
    Loaded(Vec<String>),
    Failed(SharedString),
}

/// A running ChatGPT sign-in: the code to type at `uri`.
struct CodexLogin {
    provider_id: String,
    code: SharedString,
    uri: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Accounts,
    Cli,
}

pub struct ProvidersView {
    backend: Backend,
    llm: Option<LlmConfigSnapshot>,
    models: HashMap<String, Models>,
    /// Models offered by the chat's quick switcher.
    catalog: Option<UserModelCatalog>,
    /// The orchestrator's catalog (CLI runtimes and their sign-in state),
    /// read through any thread; `None` until read or without threads.
    runtimes: Option<Vec<RouteProvider>>,
    /// The picker preferences read with the runtimes; removing a CLI agent
    /// hides it there.
    picker: Option<PickerPrefs>,
    catalog_thread: Option<String>,
    pin_input: Option<Entity<InputState>>,
    key_editor: Option<(String, Entity<InputState>)>,
    /// Connection test results by provider id (`None` while running).
    probes: HashMap<String, Option<(bool, SharedString)>>,
    confirm_delete: Option<String>,
    codex: Option<CodexLogin>,
    tab: Tab,
    search: Entity<InputState>,
    /// The tile opened from "Add a provider".
    adding: Option<String>,
    /// Background-task models; `None` until read (or an older gateway).
    task_models: Option<TaskModels>,
    /// The thread-titles model list is open.
    titles_open: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    _search: Subscription,
}

impl EventEmitter<PageEvent> for ProvidersView {}

impl ProvidersView {
    pub fn new(
        backend: Backend,
        catalog_thread: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search providers"));
        let _search = cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify());
        let mut this = Self {
            backend,
            llm: None,
            models: HashMap::new(),
            catalog: None,
            runtimes: None,
            picker: None,
            catalog_thread,
            pin_input: None,
            key_editor: None,
            probes: HashMap::new(),
            confirm_delete: None,
            codex: None,
            tab: Tab::Accounts,
            search,
            adding: None,
            task_models: None,
            titles_open: false,
            error: None,
            notice: None,
            _search,
        };
        this.refresh(cx);
        this
    }

    pub fn set_catalog_thread(&mut self, thread: Option<String>) {
        if thread.is_some() {
            self.catalog_thread = thread;
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.llm_config().await },
            |this, result, cx| {
                match result {
                    Ok(llm) => this.llm = Some(llm),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.model_catalog().await },
            |this, result, cx| {
                // silent-ok: an older gateway without the route just has no switcher.
                this.catalog = result.ok();
                cx.notify();
            },
        );
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.task_models().await },
            |this, result, cx| {
                // silent-ok: an older gateway has no task models; the section hides.
                this.task_models = result.ok();
                cx.notify();
            },
        );
        if let Some(thread) = self.catalog_thread.clone() {
            jobs::run(
                &self.backend,
                cx,
                move |c| async move { c.thread_route(&thread).await },
                |this, result, cx| {
                    // silent-ok: without the route the CLI tab says it cannot tell.
                    let route = result.ok();
                    this.picker = route.as_ref().map(|r| r.picker.clone());
                    this.runtimes = route.map(|r| r.providers);
                    cx.notify();
                },
            );
        }
    }

    fn set_title_model(&mut self, choice: Option<TaskModelChoice>, cx: &mut Context<Self>) {
        let models = TaskModels {
            thread_titles: choice,
        };
        self.titles_open = false;
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_task_models(&models).await },
            |this, result, cx| match result {
                Ok(saved) => {
                    this.task_models = Some(saved);
                    cx.notify();
                }
                Err(e) => this.fail(e, cx),
            },
        );
        cx.notify();
    }

    /// API providers a task can run on, with their models: the routable
    /// providers of the orchestrator catalog, or the connected ones with
    /// their default model when no thread exists to read the catalog.
    fn task_choices(&self) -> Vec<(String, String, Vec<String>)> {
        let active = self
            .llm
            .as_ref()
            .and_then(|l| l.active.as_ref())
            .map(|a| a.provider_id.clone());
        let mut out: Vec<(String, String, Vec<String>)> = Vec::new();
        for r in self.runtimes.iter().flatten() {
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
            out.push((id.clone(), display_name(&id).to_string(), r.models.clone()));
        }
        if out.is_empty()
            && let Some(llm) = &self.llm
        {
            for (_, p) in self.connected(llm) {
                let model = p
                    .active_model
                    .clone()
                    .unwrap_or_else(|| p.default_model.clone());
                out.push((p.id.clone(), display_name(&p.id).to_string(), vec![model]));
            }
        }
        out
    }

    fn render_task_models(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let current = self
            .task_models
            .as_ref()
            .and_then(|m| m.thread_titles.clone());
        let label = current.as_ref().map_or_else(
            || "Off".to_string(),
            |c| format!("{} · {}", display_name(&c.provider_id), c.model),
        );
        let picker = self.titles_open.then(|| {
            let mut list = v_flex().gap_2().pt_2().child(
                h_flex().child(
                    Button::new("title-model-off")
                        .label("Off")
                        .small()
                        .when(current.is_none(), |b| b.primary())
                        .when(current.is_some(), |b| b.outline())
                        .on_click(cx.listener(|this, _, _, cx| this.set_title_model(None, cx))),
                ),
            );
            for (pix, (provider, name, models)) in self.task_choices().into_iter().enumerate() {
                let buttons = models.into_iter().enumerate().map(|(mix, model)| {
                    let selected = current
                        .as_ref()
                        .is_some_and(|c| c.provider_id == provider && c.model == model);
                    let choice = TaskModelChoice {
                        provider_id: provider.clone(),
                        model: model.clone(),
                    };
                    Button::new(("title-model", pix * 1000 + mix))
                        .label(model)
                        .small()
                        .when(selected, |b| b.primary())
                        .when(!selected, |b| b.outline())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_title_model(Some(choice.clone()), cx)
                        }))
                });
                list = list.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(name),
                        )
                        .child(h_flex().flex_wrap().gap_1().children(buttons)),
                );
            }
            list
        });
        card(cx)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child("Thread titles"))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                "Names a thread after its first reply. \
                                         One model for every profile.",
                            )),
                    )
                    .child(
                        Button::new("title-model")
                            .label(label)
                            .icon(if self.titles_open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .outline()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.titles_open = !this.titles_open;
                                cx.notify();
                            })),
                    ),
            )
            .when_some(picker, |d, p| d.child(p))
            .into_any_element()
    }

    fn fail(&mut self, e: impl ToString, cx: &mut Context<Self>) {
        self.error = Some(e.to_string().into());
        cx.notify();
    }

    fn set_active(&mut self, provider: String, model: Option<String>, cx: &mut Context<Self>) {
        self.notice = None;
        // With a quick-switch list saved, the gateway runs chats on that
        // list's default (or the person's pick), not the active model. Keep
        // a model pick honest: make the chosen model the default and clear the pick.
        let switcher = self
            .catalog
            .as_ref()
            .filter(|c| c.selection_enabled)
            .map(|c| c.models.clone());
        jobs::run(
            &self.backend,
            cx,
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
            |this, result, cx| match result {
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
                    this.llm = Some(llm);
                    this.error = None;
                    this.notice = active.map(Into::into);
                    this.refresh(cx);
                }
                Err(e) => this.fail(e, cx),
            },
        );
    }

    /// Add or remove a model from the quick switcher. The default stays the
    /// active model when it is pinned, else the first pinned one.
    fn toggle_pin(&mut self, model: String, active: Option<String>, cx: &mut Context<Self>) {
        let mut allowed = self
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
            self.fail("Keep at least one model in the quick switcher.", cx);
            return;
        };
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_model_policy(default, allowed).await },
            |this, result, cx| match result {
                Ok(catalog) => {
                    this.catalog = Some(catalog);
                    this.error = None;
                    cx.notify();
                }
                Err(e) => this.fail(e, cx),
            },
        );
    }

    fn toggle_models(&mut self, p: &LlmProviderView, cx: &mut Context<Self>) {
        if self.models.remove(&p.id).is_some() {
            cx.notify();
            return;
        }
        self.models.insert(p.id.clone(), Models::Loading);
        cx.notify();
        let (provider, id) = (p.clone(), p.id.clone());
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.list_models(&provider).await },
            move |this, result, cx| {
                let models = match result {
                    Ok(r) if r.ok => Models::Loaded(r.models),
                    Ok(r) => Models::Failed(r.message.into()),
                    Err(e) => Models::Failed(e.to_string().into()),
                };
                // Only fill the list if it is still open.
                if let Some(slot) = this.models.get_mut(&id) {
                    *slot = models;
                    cx.notify();
                }
            },
        );
    }

    fn test_connection(&mut self, p: LlmProviderView, cx: &mut Context<Self>) {
        let id = p.id.clone();
        self.probes.insert(id.clone(), None);
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.test_llm_connection(&p).await },
            move |this, result, cx| {
                let outcome = match result {
                    Ok(r) => (r.ok, r.message.into()),
                    Err(e) => (false, e.to_string().into()),
                };
                this.probes.insert(id, Some(outcome));
                cx.notify();
            },
        );
    }

    fn delete_provider(&mut self, id: String, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.delete_llm_provider(&id).await.map(|l| (id, l)) },
            |this, result, cx| match result {
                Ok((id, llm)) => {
                    this.llm = Some(llm);
                    this.error = None;
                    this.notice =
                        Some(format!("Removed {} and its stored key.", display_name(&id)).into());
                    cx.notify();
                }
                Err(e) => this.fail(e, cx),
            },
        );
    }

    /// Remove a CLI agent from Numoh, or bring it back. The binary stays
    /// installed; the agent is hidden from the model picker and this list.
    fn toggle_cli_hidden(&mut self, provider: String, cx: &mut Context<Self>) {
        let (Some(thread), Some(prefs)) = (self.catalog_thread.clone(), self.picker.as_mut())
        else {
            self.fail("Start one thread first, then remove CLI agents here.", cx);
            return;
        };
        prefs.toggle_provider(&provider);
        let prefs = prefs.clone();
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_model_picker(&thread, &prefs).await },
            |this, result, cx| match result {
                Ok(()) => this.refresh(cx),
                Err(e) => {
                    this.fail(e, cx);
                    this.refresh(cx);
                }
            },
        );
    }

    /// Show the device code, open the sign-in page, then wait for the gateway
    /// to make Codex active.
    fn start_codex(&mut self, provider_id: String, cx: &mut Context<Self>) {
        self.notice = None;
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.start_codex_login().await },
            move |this, result, cx| match result {
                Ok(start) => {
                    cx.open_url(&start.verification_uri);
                    this.codex = Some(CodexLogin {
                        provider_id: provider_id.clone(),
                        code: start.user_code.into(),
                        uri: start.verification_uri,
                    });
                    this.error = None;
                    cx.notify();
                    this.wait_for_codex(provider_id, cx);
                }
                Err(e) => this.fail(e, cx),
            },
        );
    }

    fn wait_for_codex(&mut self, provider_id: String, cx: &mut Context<Self>) {
        let target = provider_id.clone();
        jobs::run(
            &self.backend,
            cx,
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
            move |this, result, cx| {
                // Cancelled or replaced while waiting: drop the result.
                if this.codex.as_ref().map(|l| &l.provider_id) != Some(&provider_id) {
                    return;
                }
                this.codex = None;
                match result {
                    Ok(Some(llm)) => {
                        this.llm = Some(llm);
                        this.notice = Some(
                            "Signed in with ChatGPT. Your plan is now the active provider.".into(),
                        );
                        this.refresh(cx);
                    }
                    Ok(None) => this.fail("The sign-in expired. Start again.", cx),
                    Err(e) => this.fail(e, cx),
                }
            },
        );
    }

    fn open_key_editor(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste the new API key")
                .masked(true)
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        self.key_editor = Some((id, input));
        cx.notify();
    }

    fn save_key(&mut self, p: LlmProviderView, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, input)) = self.key_editor.as_ref() else {
            return;
        };
        let key = input.read(cx).value().trim().to_string();
        if key.is_empty() {
            return;
        }
        // Drop the plaintext from the input right away; the request owns it now.
        input.update(cx, |s, cx| s.set_value("", window, cx));
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_provider_key(&p, &key).await },
            |this, result, cx| match result {
                Ok(llm) => {
                    this.llm = Some(llm);
                    this.key_editor = None;
                    this.error = None;
                    this.notice = Some("API key saved to the encrypted secret store.".into());
                    cx.notify();
                }
                Err(e) => this.fail(e, cx),
            },
        );
    }

    fn render_provider(&self, ix: usize, p: &LlmProviderView, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let usable = p.api_key_set || !p.api_key_required;
        let editing = self.key_editor.as_ref().filter(|(id, _)| *id == p.id);
        let status = if p.active {
            Tag::success().small().child(format!(
                "Active · {}",
                p.active_model
                    .clone()
                    .unwrap_or_else(|| p.default_model.clone())
            ))
        } else if p.api_key_set {
            Tag::secondary().small().child("Key saved")
        } else if p.api_key_required {
            Tag::warning().small().child("Needs key")
        } else {
            Tag::secondary().small().child("No key needed")
        };

        let actions = h_flex()
            .gap_1()
            .when(p.can_list_models && usable, |d| {
                let provider = p.clone();
                d.child(
                    Button::new(("models", ix))
                        .label("Models")
                        .ghost()
                        .small()
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle_models(&provider, cx)),
                        ),
                )
            })
            .when(usable && p.adapter != CODEX_ADAPTER, |d| {
                let provider = p.clone();
                d.child(
                    Button::new(("test", ix))
                        .label("Test")
                        .ghost()
                        .small()
                        .loading(self.probes.get(&p.id).is_some_and(Option::is_none))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.test_connection(provider.clone(), cx)
                        })),
                )
            })
            .when(p.adapter == CODEX_ADAPTER && !p.active, |d| {
                let id = p.id.clone();
                d.child(
                    Button::new(("codex-login", ix))
                        .label("Sign in with ChatGPT")
                        .outline()
                        .small()
                        .disabled(self.codex.is_some())
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.start_codex(id.clone(), cx)),
                        ),
                )
            })
            .when(p.accepts_api_key, |d| {
                let id = p.id.clone();
                d.child(
                    Button::new(("set-key", ix))
                        .label(if p.api_key_set {
                            "Replace key"
                        } else {
                            "Add key"
                        })
                        .ghost()
                        .small()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_key_editor(id.clone(), window, cx)
                        })),
                )
            })
            // Remove sits last. A built-in stays in the catalog: removing it
            // drops its stored key and settings, and it returns to the list below.
            .when(p.active || p.api_key_set || !p.builtin, |d| {
                let id = p.id.clone();
                d.child(
                    Button::new(("delete-provider", ix))
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall()
                        .disabled(p.active)
                        .tooltip(if p.active {
                            "Switch to another provider before removing this one"
                        } else {
                            "Remove provider"
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_delete = Some(id.clone());
                            cx.notify();
                        })),
                )
            });

        let models = self.models.get(&p.id).map(|m| match m {
            Models::Loading => h_flex()
                .gap_2()
                .text_xs()
                .child(Spinner::new())
                .child("Loading models")
                .into_any_element(),
            Models::Failed(e) => error_line(e, cx).into_any_element(),
            Models::Loaded(list) => {
                let current = p.active.then(|| p.active_model.clone()).flatten();
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(list.iter().enumerate().map(|(mix, model)| {
                        let selected = current.as_deref() == Some(model.as_str());
                        let (id, m) = (p.id.clone(), model.clone());
                        Button::new(("model", ix * 1000 + mix))
                            .label(model.clone())
                            .small()
                            .when(selected, |b| b.primary())
                            .when(!selected, |b| b.outline())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_active(id.clone(), Some(m.clone()), cx)
                            }))
                    }))
                    .into_any_element()
            }
        });

        card(cx)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(provider_logo(&[p.id.as_str(), p.adapter.as_str()], 22., cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child(display_name(&p.id)))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(p.description.clone()),
                            ),
                    )
                    .child(status)
                    .child(actions),
            )
            .when_some(models, |d, m| d.child(m))
            .when_some(self.probes.get(&p.id).cloned().flatten(), |d, (ok, msg)| {
                d.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .child(if ok {
                            Tag::success().small().child("Connected")
                        } else {
                            Tag::danger().small().child("Failed")
                        })
                        .child(div().flex_1().whitespace_normal().child(msg)),
                )
            })
            .when(self.confirm_delete.as_deref() == Some(p.id.as_str()), |d| {
                let id = p.id.clone();
                d.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_sm()
                        .child(
                            div()
                                .flex_1()
                                .child(format!(
                                    "Remove {} and its stored API key?",
                                    display_name(&p.id)
                                )),
                        )
                        .child(
                            Button::new(("confirm-delete", ix))
                                .label("Remove")
                                .danger()
                                .small()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_provider(id.clone(), cx)
                                })),
                        )
                        .child(
                            Button::new(("cancel-delete", ix))
                                .label("Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_delete = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(
                self.codex.as_ref().filter(|l| l.provider_id == p.id),
                |d, login| {
                    let uri = login.uri.clone();
                    d.child(
                        v_flex()
                            .gap_1p5()
                            .pt_2()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(if login.code.is_empty() {
                                        "Finish signing in on the ChatGPT page that just opened. Allow IronClaw to use your plan."
                                    } else {
                                        "Enter this code on the ChatGPT page that just opened:"
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .font_family(theme.mono_font_family.clone())
                                            .text_lg()
                                            .font_semibold()
                                            .child(login.code.clone()),
                                    )
                                    .child(Spinner::new().small())
                                    .child(
                                        Button::new(("codex-open", ix))
                                            .label("Open page")
                                            .ghost()
                                            .small()
                                            .on_click(move |_, _, cx| cx.open_url(&uri)),
                                    )
                                    .child(
                                        Button::new(("codex-cancel", ix))
                                            .label("Cancel")
                                            .ghost()
                                            .small()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.codex = None;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                },
            )
            .when(p.active, |d| d.child(self.render_quick_switch(p, cx)))
            .when_some(editing, |d, (_, input)| {
                let provider = p.clone();
                d.child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(input)))
                        .child(
                            Button::new(("save-key", ix))
                                .label("Save")
                                .primary()
                                .small()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.save_key(provider.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(("cancel-key", ix))
                                .label("Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.key_editor = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .into_any_element()
    }

    /// Models the chat's switcher offers. The gateway binds the list to the
    /// active provider, so it lives on that provider's card.
    fn render_quick_switch(&self, p: &LlmProviderView, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let pinned = self
            .catalog
            .as_ref()
            .map(|c| c.models.clone())
            .unwrap_or_default();
        let active = p.active_model.clone();
        v_flex()
            .gap_1p5()
            .pt_2()
            .border_t_1()
            .border_color(theme.border)
            .child(div().text_xs().font_medium().child("Quick switch in chat"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(if pinned.is_empty() {
                        "Add models here to switch between them from the chat composer."
                    } else {
                        "Click a model to remove it."
                    }),
            )
            .when(!pinned.is_empty(), |d| {
                d.child(
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .children(pinned.iter().enumerate().map(|(pix, model)| {
                            let (m, a) = (model.clone(), active.clone());
                            Button::new(("pinned", pix))
                                .label(model.clone())
                                .icon(IconName::Close)
                                .outline()
                                .xsmall()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_pin(m.clone(), a.clone(), cx)
                                }))
                        })),
                )
            })
            .when(self.pin_input.is_none(), |d| {
                d.child(
                    h_flex().child(
                        Button::new("pin-open")
                            .label("Add model")
                            .icon(IconName::Plus)
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, window, cx| {
                                let input = cx.new(|cx| {
                                    InputState::new(window, cx)
                                        .placeholder("Model id, e.g. glm-5.3")
                                });
                                input.update(cx, |s, cx| s.focus(window, cx));
                                this.pin_input = Some(input);
                                cx.notify();
                            })),
                    ),
                )
            })
            .when_some(self.pin_input.clone(), |d, input| {
                d.child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&input).small()))
                        .child(
                            Button::new("pin-add")
                                .label("Add")
                                .outline()
                                .small()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let model = input.read(cx).value().trim().to_string();
                                    if !model.is_empty() {
                                        input.update(cx, |s, cx| s.set_value("", window, cx));
                                        let active = this
                                            .llm
                                            .as_ref()
                                            .and_then(|l| l.active.as_ref())
                                            .and_then(|a| a.model.clone());
                                        this.toggle_pin(model, active, cx);
                                    }
                                })),
                        ),
                )
            })
            .into_any_element()
    }

    fn connected<'a>(&self, llm: &'a LlmConfigSnapshot) -> Vec<(usize, &'a LlmProviderView)> {
        let mut rows: Vec<_> = llm
            .providers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.active || p.api_key_set)
            .collect();
        rows.sort_by_key(|(_, p)| !p.active);
        rows
    }

    fn open_tile(&mut self, p: &LlmProviderView, window: &mut Window, cx: &mut Context<Self>) {
        if self.adding.as_deref() == Some(p.id.as_str()) {
            self.adding = None;
            self.key_editor = None;
        } else {
            self.adding = Some(p.id.clone());
            self.key_editor = None;
            // An API-key provider's only step is the key: open it right away.
            if kind_of(&p.id) == Kind::ApiKey && p.accepts_api_key {
                self.open_key_editor(p.id.clone(), window, cx);
            }
        }
        cx.notify();
    }

    fn render_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, label: &'static str, which: Tab| {
            Button::new(id)
                .label(label)
                .small()
                .when(self.tab == which, |b| b.primary())
                .when(self.tab != which, |b| b.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = which;
                    this.adding = None;
                    cx.notify();
                }))
        };
        h_flex()
            .gap_2()
            .items_center()
            .child(tab("tab-accounts", "Accounts & API keys", Tab::Accounts))
            .child(tab("tab-cli", "CLI agents", Tab::Cli))
            .child(div().flex_1())
            .child(div().w(px(220.)).child(Input::new(&self.search).small()))
    }

    fn render_tile(
        &self,
        ix: usize,
        p: &LlmProviderView,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let open = self.adding.as_deref() == Some(p.id.as_str());
        let provider = p.clone();
        div()
            .id(("tile", ix))
            .w(px(TILE_WIDTH))
            .p_3()
            .rounded(px(12.))
            .border_1()
            .border_color(if open { theme.primary } else { theme.border })
            .bg(theme.secondary.opacity(if open { 0.9 } else { 0.4 }))
            .hover(|d| d.bg(theme.secondary))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.open_tile(&provider, window, cx)))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(provider_logo(&[p.id.as_str(), p.adapter.as_str()], 20., cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_medium()
                            .child(display_name(&p.id)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(kind_of(&p.id).label()),
                    ),
            )
            .child(
                div()
                    .pt_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(p.description.clone()),
            )
    }

    fn render_accounts(&self, cx: &Context<Self>) -> AnyElement {
        let Some(llm) = self.llm.as_ref() else {
            return Spinner::new().into_any_element();
        };
        let query = self.search.read(cx).value().trim().to_lowercase();
        let open = self.adding.as_ref().and_then(|id| {
            llm.providers
                .iter()
                .enumerate()
                .find(|(_, p)| p.id == *id && !(p.active || p.api_key_set))
        });
        let mut groups = Vec::new();
        for kind in [Kind::Account, Kind::ApiKey, Kind::Local] {
            let tiles: Vec<_> = llm
                .providers
                .iter()
                .enumerate()
                .filter(|(_, p)| !(p.active || p.api_key_set) && kind_of(&p.id) == kind)
                .filter(|(_, p)| {
                    query.is_empty()
                        || p.id.contains(&query)
                        || display_name(&p.id).to_lowercase().contains(&query)
                        || p.description.to_lowercase().contains(&query)
                })
                .map(|(ix, p)| self.render_tile(ix, p, cx).into_any_element())
                .collect();
            if tiles.is_empty() {
                continue;
            }
            groups.push(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child(match kind {
                                Kind::Account => "Use a subscription or an existing login",
                                Kind::ApiKey => "Paste an API key",
                                Kind::Local => "Run on this Mac",
                            }),
                    )
                    .child(h_flex().flex_wrap().gap_2().children(tiles))
                    .into_any_element(),
            );
        }
        v_flex()
            .gap_4()
            .when_some(open, |d, (ix, p)| d.child(self.render_provider(ix, p, cx)))
            .when(groups.is_empty(), |d| {
                d.child(super::empty_state("No provider matches.", cx))
            })
            .children(groups)
            .into_any_element()
    }

    fn render_cli(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtimes = self.runtimes.clone().unwrap_or_default();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let mut rows: Vec<AnyElement> = Vec::new();
        let row = |id: SharedString,
                   logo_ids: Vec<String>,
                   name: String,
                   blurb: String,
                   status: Tag,
                   command: Option<(&'static str, &'static str)>,
                   remove: Option<(String, bool)>| {
            let ids: Vec<&str> = logo_ids.iter().map(String::as_str).collect();
            let hidden = remove.as_ref().is_some_and(|(_, h)| *h);
            let remove = remove.map(|(provider, hidden)| {
                let button = Button::new(SharedString::from(format!("cli-remove-{id}")));
                let button = if hidden {
                    button.label("Add back").outline().xsmall()
                } else {
                    button
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall()
                        .tooltip("Remove from Numoh (the CLI stays installed)")
                };
                button.on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_cli_hidden(provider.clone(), cx)),
                )
            });
            card(cx).when(hidden, |d| d.opacity(0.6)).child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(provider_logo(&ids, 22., cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child(name))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .whitespace_normal()
                                    .child(blurb),
                            )
                            .when_some(command, |d, (label, cmd)| {
                                d.child(
                                    h_flex()
                                        .pt_1()
                                        .gap_2()
                                        .items_center()
                                        .text_xs()
                                        .child(
                                            div().text_color(theme.muted_foreground).child(label),
                                        )
                                        .child(
                                            div()
                                                .font_family(theme.mono_font_family.clone())
                                                .child(cmd),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!("copy-{id}")))
                                                .icon(IconName::Copy)
                                                .ghost()
                                                .xsmall()
                                                .tooltip("Copy command")
                                                .on_click(move |_, _, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(cmd.to_string()),
                                                    )
                                                }),
                                        ),
                                )
                            }),
                    )
                    .child(status)
                    .children(remove),
            )
        };
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
                    Tag::secondary().small().child("Driver coming next")
                } else if self.runtimes.is_none() {
                    Tag::secondary().small().child("Unknown")
                } else {
                    Tag::warning().small().child("Not detected")
                };
                rows.push(
                    row(
                        known.runtime.into(),
                        vec![known.runtime.to_string()],
                        known.name.to_string(),
                        known.blurb.to_string(),
                        status,
                        known.install.map(|c| ("Install", c)),
                        None,
                    )
                    .into_any_element(),
                );
                continue;
            }
            for r in found {
                let hidden = self
                    .picker
                    .as_ref()
                    .is_some_and(|p| p.provider_hidden(&r.provider));
                let status = if hidden {
                    Tag::secondary().small().child("Removed")
                } else if r.authenticated {
                    Tag::success()
                        .small()
                        .child(format!("Ready · {} models", r.models.len()))
                } else {
                    Tag::warning().small().child("Not signed in")
                };
                let command = (!r.authenticated)
                    .then_some(known.login)
                    .flatten()
                    .map(|c| ("Sign in", c));
                rows.push(
                    row(
                        r.provider.clone().into(),
                        vec![r.provider.clone(), r.runtime.clone()],
                        r.name.clone(),
                        known.blurb.to_string(),
                        status,
                        command,
                        self.picker.is_some().then(|| (r.provider.clone(), hidden)),
                    )
                    .into_any_element(),
                );
            }
        }
        v_flex()
            .gap_3()
            .when(self.runtimes.is_none(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .whitespace_normal()
                        .child(
                            "Start one thread to let IronClaw report which CLI agents it found.",
                        ),
                )
            })
            .children(rows)
            .into_any_element()
    }
}

fn section_title(text: &'static str) -> impl IntoElement {
    div().pt_6().pb_2().font_semibold().child(text)
}

impl Render for ProvidersView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = refresh_button("refresh-providers")
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let connected: Vec<AnyElement> = self
            .llm
            .as_ref()
            .map(|llm| {
                self.connected(llm)
                    .into_iter()
                    .map(|(ix, p)| self.render_provider(ix, p, cx))
                    .collect()
            })
            .unwrap_or_default();
        let body = v_flex()
            .child(header("Providers", actions))
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when_some(self.notice.clone(), |d, n| {
                d.child(
                    div()
                        .pb_3()
                        .text_sm()
                        .text_color(cx.theme().success)
                        .child(n),
                )
            })
            .child(section_title("Connected"))
            .child(if self.llm.is_none() {
                Spinner::new().into_any_element()
            } else if connected.is_empty() {
                super::empty_state("Nothing connected yet. Add a provider below.", cx)
                    .into_any_element()
            } else {
                v_flex().gap_3().children(connected).into_any_element()
            })
            .when(self.task_models.is_some(), |d| {
                d.child(section_title("Background tasks"))
                    .child(self.render_task_models(cx))
            })
            .child(section_title("Add a provider"))
            .child(self.render_tabs(cx))
            .child(div().pt_3().child(match self.tab {
                Tab::Accounts => self.render_accounts(cx),
                Tab::Cli => self.render_cli(cx),
            }));
        page_body("providers-page", body)
    }
}
