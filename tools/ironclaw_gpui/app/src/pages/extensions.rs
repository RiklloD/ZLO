//! Extensions: what is installed, the catalog, install/remove, and setup.
//!
//! API keys and config fields are entered here. OAuth, pairing and device
//! linking redirect through the provider's site, so those continue in the
//! WebUI in the browser.

use std::collections::BTreeMap;

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::extension_state::LifecyclePublicState;
use ironclaw_gpui_client::product_wire::{
    RebornExtensionCredentialSetup, RebornExtensionRegistryEntry, RebornSetupExtensionResponse,
};
use ironclaw_gpui_client::wire::InstalledExtension;

use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

/// The open setup panel for one extension.
struct Setup {
    id: String,
    view: Option<RebornSetupExtensionResponse>,
    /// (secret or field name, is_secret, input)
    inputs: Vec<(String, bool, Entity<InputState>)>,
    submitting: bool,
}

pub struct ExtensionsView {
    backend: Backend,
    installed: Vec<InstalledExtension>,
    catalog: Vec<RebornExtensionRegistryEntry>,
    setup: Option<Setup>,
    busy: Option<String>,
    confirm_remove: Option<String>,
    loaded: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
}

impl EventEmitter<PageEvent> for ExtensionsView {}

impl ExtensionsView {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            installed: Vec::new(),
            catalog: Vec::new(),
            setup: None,
            busy: None,
            confirm_remove: None,
            loaded: false,
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
            |c| async move {
                let installed = c.extensions().await?;
                let catalog = c.extension_registry().await?;
                Ok((installed, catalog))
            },
            |this, result, cx| {
                match result {
                    Ok((installed, catalog)) => {
                        this.installed = installed.extensions;
                        this.catalog = catalog.entries;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                this.loaded = true;
                cx.notify();
            },
        );
    }

    fn browser_url(&self) -> String {
        format!("{}/extensions", self.backend.client().connection().base_url)
    }

    fn install(
        &mut self,
        entry: &RebornExtensionRegistryEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (package_ref, id) = (
            entry.package_ref.clone(),
            entry.package_ref.id.as_str().to_string(),
        );
        self.busy = Some(id.clone());
        self.notice = None;
        cx.notify();
        jobs::run_in(
            &self.backend,
            window,
            cx,
            move |c| async move { c.install_extension(&package_ref).await },
            move |this, result, window, cx| {
                this.busy = None;
                match result {
                    Ok(resp) => {
                        this.error = None;
                        this.notice = Some(resp.message.into());
                        // Installing usually needs a credential next.
                        this.open_setup(id.clone(), window, cx);
                        this.refresh(cx);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn remove(&mut self, id: String, cx: &mut Context<Self>) {
        self.busy = Some(id.clone());
        self.confirm_remove = None;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.remove_extension(&id).await },
            |this, result, cx| {
                this.busy = None;
                match result {
                    Ok(resp) => {
                        this.notice = Some(resp.message.into());
                        this.setup = None;
                        this.refresh(cx);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn open_setup(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.setup.as_ref().is_some_and(|s| s.id == id) {
            self.setup = None;
            cx.notify();
            return;
        }
        self.setup = Some(Setup {
            id: id.clone(),
            view: None,
            inputs: Vec::new(),
            submitting: false,
        });
        cx.notify();
        let job_id = id.clone();
        jobs::run_in(
            &self.backend,
            window,
            cx,
            move |c| async move { c.extension_setup(&job_id).await },
            move |this, result, window, cx| match result {
                Ok(view) => this.fill_setup(&id, view, window, cx),
                Err(e) => {
                    this.setup = None;
                    this.error = Some(e.to_string().into());
                    cx.notify();
                }
            },
        );
    }

    fn fill_setup(
        &mut self,
        id: &str,
        view: RebornSetupExtensionResponse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(setup) = self.setup.as_mut().filter(|s| s.id == id) else {
            return;
        };
        let mut inputs = Vec::new();
        for secret in &view.secrets {
            if matches!(secret.setup, RebornExtensionCredentialSetup::ManualToken) {
                let placeholder = if secret.provided {
                    "Saved. Paste a new value to replace it."
                } else {
                    "Paste value"
                };
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .masked(true)
                });
                inputs.push((secret.name.clone(), true, input));
            }
        }
        for field in &view.fields {
            let placeholder = field.placeholder.clone().unwrap_or_default();
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            inputs.push((field.name.clone(), false, input));
        }
        setup.inputs = inputs;
        setup.view = Some(view);
        cx.notify();
    }

    fn submit_setup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        let (mut secrets, mut fields) = (BTreeMap::new(), BTreeMap::new());
        for (name, secret, input) in &setup.inputs {
            let value = input.read(cx).value().trim().to_string();
            if value.is_empty() {
                continue;
            }
            if *secret {
                secrets.insert(name.clone(), value);
            } else {
                fields.insert(name.clone(), value);
            }
            // Secrets leave the input as soon as they are captured.
            if *secret {
                input.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }
        if secrets.is_empty() && fields.is_empty() {
            return;
        }
        setup.submitting = true;
        let id = setup.id.clone();
        cx.notify();
        jobs::run_in(
            &self.backend,
            window,
            cx,
            move |c| async move { c.submit_extension_setup(&id, secrets, fields).await },
            move |this, result, window, cx| {
                if let Some(s) = this.setup.as_mut() {
                    s.submitting = false;
                }
                match result {
                    Ok(view) => {
                        this.error = None;
                        if matches!(view.phase, LifecyclePublicState::Active) {
                            this.setup = None;
                            this.notice = Some("Extension is active.".into());
                        } else if let Some(id) = this.setup.as_ref().map(|s| s.id.clone()) {
                            this.fill_setup(&id, view, window, cx);
                        }
                        this.refresh(cx);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn render_setup(&self, setup: &Setup, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(view) = setup.view.as_ref() else {
            return h_flex()
                .gap_2()
                .text_sm()
                .child(Spinner::new())
                .child("Loading setup")
                .into_any_element();
        };
        let needs_browser = view.secrets.iter().any(|s| {
            !s.provided && !matches!(s.setup, RebornExtensionCredentialSetup::ManualToken)
        });
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

        let prompts: Vec<(String, bool, bool)> = view
            .secrets
            .iter()
            .filter(|s| matches!(s.setup, RebornExtensionCredentialSetup::ManualToken))
            .map(|s| (s.prompt.clone(), s.optional, s.provided))
            .chain(
                view.fields
                    .iter()
                    .map(|f| (f.prompt.clone(), f.optional, false)),
            )
            .collect();
        let rows = setup.inputs.iter().zip(prompts).map(
            |((_, _, input), (prompt, optional, provided))| {
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .font_medium()
                            .child(prompt)
                            .when(optional, |d| {
                                d.child(div().text_color(theme.muted_foreground).child("optional"))
                            })
                            .when(provided, |d| d.child(Tag::success().small().child("Saved"))),
                    )
                    .child(Input::new(input))
            },
        );

        let browser = self.browser_url();
        v_flex()
            .gap_3()
            .pt_2()
            .border_t_1()
            .border_color(theme.border)
            .when_some(instructions, |d, text| {
                d.child(div().text_sm().whitespace_normal().child(text))
            })
            .children(rows)
            .child(
                h_flex()
                    .gap_2()
                    .when(!setup.inputs.is_empty(), |d| {
                        d.child(
                            Button::new("submit-setup")
                                .label("Save")
                                .primary()
                                .small()
                                .loading(setup.submitting)
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.submit_setup(window, cx)
                                    }),
                                ),
                        )
                    })
                    .when(needs_browser, |d| {
                        d.child(
                            Button::new("setup-in-browser")
                                .icon(IconName::ExternalLink)
                                .label("Sign in via browser")
                                .outline()
                                .small()
                                .on_click(move |_, _, cx| cx.open_url(&browser)),
                        )
                    })
                    .when_some(setup_url, |d, url| {
                        d.child(
                            Button::new("setup-url")
                                .icon(IconName::ExternalLink)
                                .label("Where to get it")
                                .ghost()
                                .small()
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_installed(
        &self,
        ix: usize,
        ext: &InstalledExtension,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let id = ext.package_ref.id.as_str().to_string();
        let busy = self.busy.as_deref() == Some(id.as_str());
        let confirming = self.confirm_remove.as_deref() == Some(id.as_str());
        let setup = self.setup.as_ref().filter(|s| s.id == id);
        let tag = match ext.installation_state {
            LifecyclePublicState::Active => Tag::success().small().child("Active"),
            LifecyclePublicState::SetupNeeded => Tag::warning().small().child("Needs setup"),
            LifecyclePublicState::Uninstalled => Tag::secondary().small().child("Removed"),
        };
        let setup_id = id.clone();
        let remove_button = if confirming {
            let id = id.clone();
            Button::new(("confirm-remove", ix))
                .label("Remove?")
                .danger()
                .small()
                .on_click(cx.listener(move |this, _, _, cx| this.remove(id.clone(), cx)))
        } else {
            let id = id.clone();
            Button::new(("remove", ix))
                .icon(IconName::Delete)
                .ghost()
                .small()
                .tooltip("Remove")
                .accessibility_label("Remove")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.confirm_remove = Some(id.clone());
                    cx.notify();
                }))
        };

        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child(ext.display_name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(ext.description.clone()),
                            ),
                    )
                    .child(tag)
                    .child(
                        Button::new(("setup", ix))
                            .label(if setup.is_some() { "Close" } else { "Set up" })
                            .outline()
                            .small()
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_setup(setup_id.clone(), window, cx)
                            })),
                    )
                    .child(remove_button.disabled(busy)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{} · {} tools{}",
                        ext.runtime,
                        ext.tools.len(),
                        ext.version
                            .as_deref()
                            .map(|v| format!(" · v{v}"))
                            .unwrap_or_default()
                    )),
            )
            .when_some(ext.activation_error.clone(), |d, e| {
                d.child(error_line(&SharedString::from(e), cx))
            })
            .when_some(setup, |d, s| d.child(self.render_setup(s, cx)))
            .into_any_element()
    }

    fn render_catalog(
        &self,
        ix: usize,
        entry: &RebornExtensionRegistryEntry,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let busy = self.busy.as_deref() == Some(entry.package_ref.id.as_str());
        let e = entry.clone();
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
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(entry.display_name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .whitespace_normal()
                            .child(entry.description.clone()),
                    ),
            )
            .child(Tag::secondary().small().child(entry.runtime.clone()))
            .child(
                Button::new(("install", ix))
                    .label("Install")
                    .outline()
                    .small()
                    .loading(busy)
                    .on_click(cx.listener(move |this, _, window, cx| this.install(&e, window, cx))),
            )
            .into_any_element()
    }
}

impl Render for ExtensionsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = refresh_button("refresh-extensions")
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let installed: Vec<AnyElement> = self
            .installed
            .iter()
            .enumerate()
            .map(|(ix, e)| self.render_installed(ix, e, cx))
            .collect();
        let catalog: Vec<AnyElement> = self
            .catalog
            .iter()
            .filter(|e| !e.installed)
            .enumerate()
            .map(|(ix, e)| self.render_catalog(ix, e, cx))
            .collect();
        let body = v_flex()
            .child(header("Extensions", actions))
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .when_some(self.notice.clone(), |d, n| {
                d.child(
                    div()
                        .pb_3()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(n),
                )
            })
            .child(div().pb_2().font_semibold().child("Installed"))
            .when(self.loaded && installed.is_empty(), |d| {
                d.child(empty_state("No extensions installed yet.", cx))
            })
            .child(v_flex().gap_3().children(installed))
            .child(div().pt_6().pb_2().font_semibold().child("Available"))
            .child(v_flex().children(catalog));
        page_body("extensions-page", body)
    }
}
