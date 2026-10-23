//! Extensions: what is installed, the catalog, install/remove, and setup.
//!
//! API keys, config fields and channel pairing are handled here, as are
//! custom MCP servers and tool-bundle imports. OAuth and device linking
//! report back to a browser popup, so those continue in the WebUI.

use std::collections::BTreeMap;
use std::time::Duration;

use ironclaw_gpui_client::extension_state::LifecyclePublicState;
use ironclaw_gpui_client::hosted_mcp::HostedMcpAuthSelection;
use ironclaw_gpui_client::product_wire::{
    RebornExtensionCredentialSetup, RebornExtensionRegistryEntry, RebornSetupExtensionResponse,
};
use ironclaw_gpui_client::wire::{InstalledExtension, PairingStatus};
use ironclaw_gpui_client::{Backend, ClientError};

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

/// The open pairing panel for one channel extension.
struct Pairing {
    id: String,
    status: Option<PairingStatus>,
    busy: bool,
}

/// The "add a custom MCP server" form.
struct McpForm {
    name: Entity<InputState>,
    endpoint: Entity<InputState>,
    auth: usize,
    submitting: bool,
}

const MCP_AUTH: [&str; 4] = ["Detect", "None", "API token", "OAuth"];

fn mcp_auth(ix: usize) -> HostedMcpAuthSelection {
    match ix {
        0 => HostedMcpAuthSelection::Auto,
        2 => HostedMcpAuthSelection::Bearer,
        3 => HostedMcpAuthSelection::OAuth {
            client_profile_id: None,
        },
        _ => HostedMcpAuthSelection::NoAuth,
    }
}

/// "My Server" -> "my-server"; the gateway prefixes `mcp-`.
fn mcp_slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-')
        .trim_start_matches("mcp-")
        .to_string()
}

pub struct ExtensionsView {
    backend: Backend,
    pairing: Option<Pairing>,
    mcp: Option<McpForm>,
    importing: bool,
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
            pairing: None,
            mcp: None,
            importing: false,
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

    // --- pairing ---

    fn toggle_pairing(&mut self, id: String, cx: &mut Context<Self>) {
        if self.pairing.as_ref().is_some_and(|p| p.id == id) {
            self.pairing = None;
            cx.notify();
            return;
        }
        self.pairing = Some(Pairing {
            id: id.clone(),
            status: None,
            busy: true,
        });
        cx.notify();
        self.poll_pairing(id, None, cx);
    }

    /// Read the pairing state; while a code is pending, check again every
    /// few seconds until the account connects or the panel closes.
    fn poll_pairing(&mut self, id: String, delay: Option<Duration>, cx: &mut Context<Self>) {
        let job_id = id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move {
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                c.pairing_status(&job_id).await
            },
            move |this, result, cx| {
                let Some(p) = this.pairing.as_mut().filter(|p| p.id == id) else {
                    return;
                };
                p.busy = false;
                match result {
                    Ok(status) => {
                        let waiting = !status.connected && status.pending.is_some();
                        p.status = Some(status);
                        if waiting {
                            this.poll_pairing(id, Some(Duration::from_secs(3)), cx);
                        }
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn mint_pairing(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.pairing.as_mut() else {
            return;
        };
        p.busy = true;
        let id = p.id.clone();
        cx.notify();
        let job_id = id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.mint_pairing_code(&job_id).await },
            move |this, result, cx| {
                let Some(p) = this.pairing.as_mut().filter(|p| p.id == id) else {
                    return;
                };
                p.busy = false;
                match result {
                    Ok(code) => {
                        p.status = Some(PairingStatus {
                            connected: false,
                            pending: Some(code),
                        });
                        this.poll_pairing(id, Some(Duration::from_secs(3)), cx);
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn unpair(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.pairing.as_mut() else {
            return;
        };
        p.busy = true;
        let id = p.id.clone();
        cx.notify();
        let job_id = id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.unpair_extension(&job_id).await },
            move |this, result, cx| {
                match result {
                    Ok(()) => this.poll_pairing(id, None, cx),
                    Err(e) => {
                        if let Some(p) = this.pairing.as_mut() {
                            p.busy = false;
                        }
                        this.error = Some(e.to_string().into());
                    }
                }
                cx.notify();
            },
        );
    }

    fn render_pairing(&self, p: &Pairing, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let body = match &p.status {
            None => h_flex()
                .gap_2()
                .text_sm()
                .child(Spinner::new())
                .child("Checking connection")
                .into_any_element(),
            Some(s) if s.connected => h_flex()
                .gap_2()
                .items_center()
                .child(Tag::success().small().child("Connected"))
                .child(div().flex_1())
                .child(
                    Button::new("unpair")
                        .label("Disconnect")
                        .outline()
                        .small()
                        .loading(p.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.unpair(cx))),
                )
                .into_any_element(),
            Some(s) => match &s.pending {
                Some(code) => {
                    let link = code.deep_link.clone();
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .whitespace_normal()
                                .child("Send this code to the bot from your chat app:"),
                        )
                        .child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .child(
                                    div()
                                        .text_xl()
                                        .font_semibold()
                                        .font_family("monospace")
                                        .child(code.code.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("expires {}", jobs::when(code.expires_at))),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .when_some(link, |d, url| {
                                    d.child(
                                        Button::new("pair-link")
                                            .icon(IconName::ExternalLink)
                                            .label("Open in chat app")
                                            .outline()
                                            .small()
                                            .on_click(move |_, _, cx| cx.open_url(&url)),
                                    )
                                })
                                .child(
                                    Button::new("pair-new")
                                        .label("New code")
                                        .ghost()
                                        .small()
                                        .loading(p.busy)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.mint_pairing(cx)),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Waiting for the bot"),
                                ),
                        )
                        .into_any_element()
                }
                None => h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child("Not connected."))
                    .child(
                        Button::new("pair-mint")
                            .label("Get a pairing code")
                            .primary()
                            .small()
                            .loading(p.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.mint_pairing(cx))),
                    )
                    .into_any_element(),
            },
        };
        v_flex()
            .gap_2()
            .pt_2()
            .border_t_1()
            .border_color(theme.border)
            .child(body)
            .into_any_element()
    }

    // --- custom MCP and bundle import ---

    fn toggle_mcp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mcp = match self.mcp.take() {
            Some(_) => None,
            None => Some(McpForm {
                name: cx.new(|cx| InputState::new(window, cx).placeholder("Name, e.g. Linear")),
                endpoint: cx
                    .new(|cx| InputState::new(window, cx).placeholder("https://example.com/mcp")),
                auth: 0,
                submitting: false,
            }),
        };
        cx.notify();
    }

    fn submit_mcp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.mcp.as_mut() else {
            return;
        };
        let name = form.name.read(cx).value().trim().to_string();
        let endpoint = form.endpoint.read(cx).value().trim().to_string();
        let slug = mcp_slug(&name);
        if slug.is_empty() || endpoint.is_empty() {
            self.error = Some("Enter a name and the server URL.".into());
            cx.notify();
            return;
        }
        let auth = mcp_auth(form.auth);
        form.submitting = true;
        self.error = None;
        cx.notify();
        jobs::run_in(
            &self.backend,
            window,
            cx,
            move |c| async move { c.register_hosted_mcp(&slug, &name, &endpoint, auth).await },
            |this, result, window, cx| {
                match result {
                    Ok(done) => {
                        this.mcp = None;
                        this.notice = Some(done.message.into());
                        // Bearer and OAuth servers need their credential next.
                        this.open_setup(done.package_ref.id.as_str().to_string(), window, cx);
                        this.refresh(cx);
                    }
                    Err(e) => {
                        if let Some(f) = this.mcp.as_mut() {
                            f.submitting = false;
                        }
                        this.error = Some(e.to_string().into());
                    }
                }
                cx.notify();
            },
        );
    }

    fn import_bundle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.importing = true;
                this.error = None;
                cx.notify();
                jobs::run(
                    &this.backend,
                    cx,
                    move |c| async move {
                        let bundle = tokio::fs::read(&path).await.map_err(|e| {
                            ClientError::Config(format!("cannot read the file: {e}"))
                        })?;
                        c.import_extension(bundle).await
                    },
                    |this, result, cx| {
                        this.importing = false;
                        match result {
                            Ok(resp) => {
                                this.notice = Some(resp.message.into());
                                this.refresh(cx);
                            }
                            Err(e) => this.error = Some(e.to_string().into()),
                        }
                        cx.notify();
                    },
                );
            });
        })
        .detach();
    }

    fn render_mcp(&self, form: &McpForm, cx: &Context<Self>) -> AnyElement {
        let auth = MCP_AUTH.iter().enumerate().map(|(ix, label)| {
            let button = Button::new(("mcp-auth", ix)).label(*label).small();
            let button = if ix == form.auth {
                button.primary()
            } else {
                button.outline()
            };
            button.on_click(cx.listener(move |this, _, _, cx| {
                if let Some(f) = this.mcp.as_mut() {
                    f.auth = ix;
                }
                cx.notify();
            }))
        });
        card(cx)
            .child(div().font_semibold().child("Custom MCP server"))
            .child(Input::new(&form.name))
            .child(Input::new(&form.endpoint))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_xs().font_medium().child("Sign-in"))
                    .children(auth),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("mcp-add")
                            .label("Add server")
                            .primary()
                            .small()
                            .loading(form.submitting)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_mcp(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("mcp-cancel")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_mcp(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
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
        let pairing = self.pairing.as_ref().filter(|p| p.id == id);
        let pair_id = id.clone();
        let channel =
            ext.is_channel() && matches!(ext.installation_state, LifecyclePublicState::Active);
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
                    .when(channel, |d| {
                        d.child(
                            Button::new(("pair", ix))
                                .label(if pairing.is_some() {
                                    "Close"
                                } else {
                                    "Connect"
                                })
                                .outline()
                                .small()
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_pairing(pair_id.clone(), cx)
                                })),
                        )
                    })
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
            .when_some(pairing, |d, p| d.child(self.render_pairing(p, cx)))
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
        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("add-mcp")
                    .icon(IconName::Plus)
                    .label("MCP server")
                    .outline()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_mcp(window, cx))),
            )
            .child(
                Button::new("import-bundle")
                    .label("Import bundle")
                    .outline()
                    .small()
                    .tooltip("Upload a tool bundle .zip (operator only)")
                    .loading(self.importing)
                    .on_click(cx.listener(|this, _, window, cx| this.import_bundle(window, cx))),
            )
            .child(
                refresh_button("refresh-extensions")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
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
            .when_some(self.mcp.as_ref(), |d, f| {
                d.child(div().pb_4().child(self.render_mcp(f, cx)))
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
