//! Settings: which model IronClaw uses, provider API keys and logins, and
//! what tools may do without asking.

use std::collections::HashMap;

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::operator_llm::{LlmConfigSnapshot, LlmProviderView, UserModelCatalog};
use ironclaw_gpui_client::product_wire::SettingsToolPermissionState;
use ironclaw_gpui_client::wire::ToolPermission;

use super::{PageEvent, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

const AUTO_APPROVE_KEY: &str = "agent.auto_approve_tools";
const TOOL_KEY_PREFIX: &str = "tool.";
const CODEX_ADAPTER: &str = "openai_codex";
/// Device codes live about 15 minutes; poll the snapshot until then.
const CODEX_POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
const CODEX_POLL_FOR: std::time::Duration = std::time::Duration::from_secs(15 * 60);

const STATES: [(SettingsToolPermissionState, &str); 4] = [
    (SettingsToolPermissionState::Default, "Default"),
    (SettingsToolPermissionState::AlwaysAllow, "Allow"),
    (SettingsToolPermissionState::AskEachTime, "Ask"),
    (SettingsToolPermissionState::Disabled, "Off"),
];

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

pub struct SettingsView {
    backend: Backend,
    llm: Option<LlmConfigSnapshot>,
    auto_approve: Option<bool>,
    tools: Vec<(String, ToolPermission)>,
    models: HashMap<String, Models>,
    /// Models offered by the chat's quick switcher.
    catalog: Option<UserModelCatalog>,
    pin_input: Option<Entity<InputState>>,
    key_editor: Option<(String, Entity<InputState>)>,
    show_all_providers: bool,
    /// Connection test results by provider id (`None` while running).
    probes: HashMap<String, Option<(bool, SharedString)>>,
    confirm_delete: Option<String>,
    codex: Option<CodexLogin>,
    error: Option<SharedString>,
    notice: Option<SharedString>,
}

impl EventEmitter<PageEvent> for SettingsView {}

impl SettingsView {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            llm: None,
            auto_approve: None,
            tools: Vec::new(),
            models: HashMap::new(),
            catalog: None,
            pin_input: None,
            key_editor: None,
            show_all_providers: false,
            probes: HashMap::new(),
            confirm_delete: None,
            codex: None,
            error: None,
            notice: None,
        };
        this.refresh(cx);
        this
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
            |c| async move { c.tool_settings().await },
            |this, result, cx| {
                match result {
                    Ok(list) => {
                        let mut tools = Vec::new();
                        for entry in list.entries {
                            if entry.key == AUTO_APPROVE_KEY {
                                this.auto_approve = entry.value.as_bool();
                            } else if let Some(id) = entry.key.strip_prefix(TOOL_KEY_PREFIX) {
                                // silent-ok: a row the gateway shapes differently is skipped, not fatal.
                                if let Ok(tool) =
                                    serde_json::from_value::<ToolPermission>(entry.value)
                                {
                                    tools.push((id.to_string(), tool));
                                }
                            }
                        }
                        tools.sort_by(|a, b| a.0.cmp(&b.0));
                        this.tools = tools;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn fail(&mut self, e: impl ToString, cx: &mut Context<Self>) {
        self.error = Some(e.to_string().into());
        cx.notify();
    }

    fn set_active(&mut self, provider: String, model: Option<String>, cx: &mut Context<Self>) {
        self.notice = None;
        // With a quick-switch list saved, the gateway runs chats on that
        // list's default (or the person's pick), not the active model. Keep
        // "Use" honest: make the chosen model the default and clear the pick.
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
                    this.notice = Some(format!("Deleted provider {id} and its stored key.").into());
                    cx.notify();
                }
                Err(e) => this.fail(e, cx),
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
                        this.notice = Some("Signed in with ChatGPT. Codex is now active.".into());
                        this.refresh(cx);
                    }
                    Ok(None) => this.fail("The sign-in code expired. Start again.", cx),
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

    fn set_auto_approve(&mut self, enabled: bool, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_auto_approve(enabled).await },
            |this, result, cx| match result {
                // Show the stored value the gateway read back, not the click.
                Ok(resp) => {
                    this.auto_approve = resp.entry.value.as_bool();
                    this.error = None;
                    // Tools without their own setting follow the switch.
                    this.refresh(cx);
                }
                Err(e) => this.fail(e, cx),
            },
        );
    }

    fn set_tool(&mut self, id: String, state: SettingsToolPermissionState, cx: &mut Context<Self>) {
        let job_id = id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_tool_permission(&job_id, state).await },
            move |this, result, cx| match result {
                Ok(resp) => {
                    if let Ok(tool) = serde_json::from_value::<ToolPermission>(resp.entry.value)
                        && let Some(row) = this.tools.iter_mut().find(|(k, _)| *k == id)
                    {
                        row.1 = tool;
                    }
                    this.error = None;
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
            .when(!p.active && usable, |d| {
                let id = p.id.clone();
                d.child(
                    Button::new(("use-provider", ix))
                        .label("Use")
                        .outline()
                        .small()
                        .on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.set_active(id.clone(), None, cx)
                            }),
                        ),
                )
            })
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
            .when(!p.builtin && !p.active, |d| {
                let id = p.id.clone();
                d.child(
                    Button::new(("delete-provider", ix))
                        .icon(IconName::Delete)
                        .ghost()
                        .small()
                        .tooltip("Delete provider")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_delete = Some(id.clone());
                            cx.notify();
                        })),
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
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child(p.id.clone()))
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
                                .child(format!("Delete {} and its stored API key?", p.id)),
                        )
                        .child(
                            Button::new(("confirm-delete", ix))
                                .label("Delete")
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
                                    .child("Enter this code on the ChatGPT page that just opened:"),
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

    fn render_models(&self, cx: &Context<Self>) -> AnyElement {
        let Some(llm) = self.llm.as_ref() else {
            return Spinner::new().into_any_element();
        };
        // Configured providers first; the long builtin catalog stays folded.
        let (mut shown, hidden): (Vec<_>, Vec<_>) = llm
            .providers
            .iter()
            .enumerate()
            .partition(|(_, p)| p.active || p.api_key_set);
        shown.sort_by_key(|(_, p)| !p.active);
        let hidden_count = hidden.len();
        let rows: Vec<AnyElement> = shown
            .into_iter()
            .chain(hidden.into_iter().filter(|_| self.show_all_providers))
            .map(|(ix, p)| self.render_provider(ix, p, cx))
            .collect();
        v_flex()
            .gap_3()
            .children(rows)
            .when(hidden_count > 0, |d| {
                d.child(
                    Button::new("toggle-providers")
                        .label(if self.show_all_providers {
                            "Hide other providers".to_string()
                        } else {
                            format!("Show {hidden_count} other providers")
                        })
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_all_providers = !this.show_all_providers;
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    fn render_desktop(&self, cx: &Context<Self>) -> impl IntoElement {
        let bundled = crate::login_item::bundle_path().is_some();
        card(cx).child(
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_semibold().child("Open at login"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .whitespace_normal()
                                .child(if bundled {
                                    "Starts IronClaw Desktop when you log in to your Mac."
                                } else {
                                    "Available when IronClaw runs from IronClaw.app (tools/ironclaw_gpui/bundle.sh)."
                                }),
                        ),
                )
                .child(
                    Switch::new("open-at-login")
                        .checked(crate::login_item::is_enabled())
                        .disabled(!bundled)
                        .accessibility_label("Open at login")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            match crate::login_item::set_enabled(*checked) {
                                Ok(()) => this.error = None,
                                Err(e) => this.error = Some(e.into()),
                            }
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_tools(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let auto = self.auto_approve.unwrap_or(false);
        let toggle = card(cx).child(
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_semibold().child("Approve tools automatically"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(if auto { theme.warning } else { theme.muted_foreground })
                                .whitespace_normal()
                                .child(if auto {
                                    "On: shell commands, file writes and network calls run without asking you. Tools set to Ask below still ask."
                                } else {
                                    "Off: IronClaw asks before shell commands, file writes and network calls, unless a tool is set to Allow."
                                }),
                        ),
                )
                .child(
                    Switch::new("auto-approve")
                        .checked(auto)
                        .disabled(self.auto_approve.is_none())
                        .accessibility_label("Approve tools automatically")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_auto_approve(*checked, cx)
                        })),
                ),
        );

        let rows = self.tools.iter().enumerate().map(|(ix, (id, tool))| {
            let segments = h_flex().gap_0p5().children(STATES.iter().enumerate().map(
                |(six, (state, label))| {
                    let selected = tool.state == *state;
                    let id = id.clone();
                    let state = *state;
                    Button::new(("tool-state", ix * 10 + six))
                        .label(*label)
                        .xsmall()
                        .when(selected, |b| b.primary())
                        .when(!selected, |b| b.ghost())
                        .disabled(tool.locked)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_tool(id.clone(), state, cx)),
                        )
                },
            ));
            h_flex()
                .gap_3()
                .py_2()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_medium().child(tool.name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(tool.description.clone()),
                        ),
                )
                .when(tool.locked, |d| {
                    d.child(Tag::secondary().small().child("Locked"))
                })
                .when(!tool.locked && !tool.has_override(), |d| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("follows switch"),
                    )
                })
                .child(segments)
        });

        v_flex()
            .gap_3()
            .child(toggle)
            .child(v_flex().children(rows))
    }
}

fn section_title(text: &'static str) -> impl IntoElement {
    div().pt_6().pb_2().font_semibold().child(text)
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = refresh_button("refresh-settings")
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let body = v_flex()
            .child(header("Settings", actions))
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
            .child(section_title("Model"))
            .child(self.render_models(cx))
            .child(section_title("Desktop app"))
            .child(self.render_desktop(cx))
            .child(section_title("Tool permissions"))
            .child(self.render_tools(cx));
        page_body("settings-page", body)
    }
}
