//! Extensions: what is installed, the catalog, install/remove, and setup.
//!
//! API keys, config fields and channel pairing are handled here, as are
//! custom MCP servers and tool-bundle imports. OAuth and device linking
//! report back to a browser popup, so those continue in the WebUI.
//!
//! Port of GPUI `pages/extensions.rs`: state and gateway calls here, the
//! rendering in `extensions/view.rs`.

pub mod fixtures;
mod view;

use std::collections::BTreeMap;
use std::time::Duration;

use ironclaw_gpui_client::ClientError;
use ironclaw_gpui_client::extension_state::LifecyclePublicState;
use ironclaw_gpui_client::hosted_mcp::HostedMcpAuthSelection;
use ironclaw_gpui_client::product_wire::{
    RebornExtensionCredentialSetup, RebornExtensionRegistryEntry, RebornSetupExtensionResponse,
};
use ironclaw_gpui_client::wire::{InstalledExtension, PairingStatus};
use rune::{Cx, El, FileDialogOptions, TextEdit, Tx};
use rune_kit::Kit;

use crate::jobs;
use crate::state::State;

/// The open setup panel for one extension.
pub struct Setup {
    pub id: String,
    pub view: Option<RebornSetupExtensionResponse>,
    /// (secret or field name, is_secret, input)
    pub inputs: Vec<(String, bool, TextEdit)>,
    pub submitting: bool,
}

/// The open pairing panel for one channel extension.
pub struct Pairing {
    pub id: String,
    pub status: Option<PairingStatus>,
    pub busy: bool,
}

/// The "add a custom MCP server" form.
pub struct McpForm {
    pub name: TextEdit,
    pub endpoint: TextEdit,
    pub auth: usize,
    pub submitting: bool,
}

pub const MCP_AUTH: [&str; 4] = ["Detect", "None", "API token", "OAuth"];

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

pub struct ExtensionsState {
    pub pairing: Option<Pairing>,
    pub mcp: Option<McpForm>,
    pub importing: bool,
    pub installed: Vec<InstalledExtension>,
    pub catalog: Vec<RebornExtensionRegistryEntry>,
    pub setup: Option<Setup>,
    pub busy: Option<String>,
    pub confirm_remove: Option<String>,
    pub loaded: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// What a field's edit accessor gets when its form closed meanwhile
    /// (Rune's accessors must always return a field).
    scratch: TextEdit,
}

impl Default for ExtensionsState {
    fn default() -> Self {
        Self {
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
            scratch: TextEdit::new(""),
        }
    }
}

fn st(s: &mut State) -> &mut ExtensionsState {
    &mut s.pages.extensions
}

/// The setup panel's `i`th input (or the scratch field when it is gone).
fn setup_input(s: &mut State, i: usize) -> &mut TextEdit {
    let ext = st(s);
    match ext.setup.as_mut().and_then(|x| x.inputs.get_mut(i)) {
        Some(input) => &mut input.2,
        None => &mut ext.scratch,
    }
}

fn mcp_input(s: &mut State, endpoint: bool) -> &mut TextEdit {
    let ext = st(s);
    match ext.mcp.as_mut() {
        Some(f) if endpoint => &mut f.endpoint,
        Some(f) => &mut f.name,
        None => &mut ext.scratch,
    }
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

pub fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    view::page(s, cx, k, layer)
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "extensions",
        |c| async move {
            let installed = c.extensions().await?;
            let catalog = c.extension_registry().await?;
            Ok((installed, catalog))
        },
        |s, result, _| {
            let ext = st(s);
            match result {
                Ok((installed, catalog)) => {
                    ext.installed = installed.extensions;
                    ext.catalog = catalog.entries;
                    ext.error = None;
                }
                Err(e) => ext.error = Some(e.to_string()),
            }
            ext.loaded = true;
        },
    );
}

/// The WebUI's extensions page, for sign-ins that report to a browser.
fn browser_url(s: &State) -> String {
    let base =
        s.rt.backend
            .as_ref()
            .map(|b| b.client().connection().base_url.to_string())
            .unwrap_or_default();
    format!("{base}/extensions")
}

fn install(s: &mut State, entry: &RebornExtensionRegistryEntry) {
    let (package_ref, id) = (
        entry.package_ref.clone(),
        entry.package_ref.id.as_str().to_string(),
    );
    let ext = st(s);
    ext.busy = Some(id.clone());
    ext.notice = None;
    jobs::run(
        s,
        "extensions",
        move |c| async move { c.install_extension(&package_ref).await },
        move |s, result, _| {
            st(s).busy = None;
            match result {
                Ok(resp) => {
                    let ext = st(s);
                    ext.error = None;
                    ext.notice = Some(resp.message);
                    // Installing usually needs a credential next.
                    open_setup(s, id.clone());
                    refresh(s);
                }
                Err(e) => st(s).error = Some(e.to_string()),
            }
        },
    );
}

fn remove(s: &mut State, id: String) {
    let ext = st(s);
    ext.busy = Some(id.clone());
    ext.confirm_remove = None;
    jobs::run(
        s,
        "extensions",
        move |c| async move { c.remove_extension(&id).await },
        |s, result, _| {
            st(s).busy = None;
            match result {
                Ok(resp) => {
                    let ext = st(s);
                    ext.notice = Some(resp.message);
                    ext.setup = None;
                    refresh(s);
                }
                Err(e) => st(s).error = Some(e.to_string()),
            }
        },
    );
}

fn open_setup(s: &mut State, id: String) {
    let ext = st(s);
    if ext.setup.as_ref().is_some_and(|x| x.id == id) {
        ext.setup = None;
        return;
    }
    ext.setup = Some(Setup {
        id: id.clone(),
        view: None,
        inputs: Vec::new(),
        submitting: false,
    });
    let job_id = id.clone();
    jobs::run(
        s,
        "extensions",
        move |c| async move { c.extension_setup(&job_id).await },
        move |s, result, _| match result {
            Ok(view) => fill_setup(s, &id, view),
            Err(e) => {
                let ext = st(s);
                ext.setup = None;
                ext.error = Some(e.to_string());
            }
        },
    );
}

/// One input per manual secret, then per config field, in the gateway's order.
fn setup_inputs(view: &RebornSetupExtensionResponse) -> Vec<(String, bool, TextEdit)> {
    let mut inputs = Vec::new();
    for secret in &view.secrets {
        if matches!(secret.setup, RebornExtensionCredentialSetup::ManualToken) {
            inputs.push((secret.name.clone(), true, TextEdit::new("").secure()));
        }
    }
    for field in &view.fields {
        inputs.push((field.name.clone(), false, TextEdit::new("")));
    }
    inputs
}

fn fill_setup(s: &mut State, id: &str, view: RebornSetupExtensionResponse) {
    let Some(setup) = st(s).setup.as_mut().filter(|x| x.id == id) else {
        return;
    };
    setup.inputs = setup_inputs(&view);
    setup.view = Some(view);
}

fn submit_setup(s: &mut State) {
    let Some(setup) = st(s).setup.as_mut() else {
        return;
    };
    let (mut secrets, mut fields) = (BTreeMap::new(), BTreeMap::new());
    for (name, secret, input) in &mut setup.inputs {
        let value = input.text.trim().to_string();
        if value.is_empty() {
            continue;
        }
        if *secret {
            secrets.insert(name.clone(), value);
            // Secrets leave the input as soon as they are captured.
            input.clear();
        } else {
            fields.insert(name.clone(), value);
        }
    }
    if secrets.is_empty() && fields.is_empty() {
        return;
    }
    setup.submitting = true;
    let id = setup.id.clone();
    jobs::run(
        s,
        "extensions",
        move |c| async move { c.submit_extension_setup(&id, secrets, fields).await },
        |s, result, _| {
            if let Some(x) = st(s).setup.as_mut() {
                x.submitting = false;
            }
            match result {
                Ok(view) => {
                    st(s).error = None;
                    if matches!(view.phase, LifecyclePublicState::Active) {
                        let ext = st(s);
                        ext.setup = None;
                        ext.notice = Some("Extension is active.".into());
                    } else if let Some(id) = st(s).setup.as_ref().map(|x| x.id.clone()) {
                        fill_setup(s, &id, view);
                    }
                    refresh(s);
                }
                Err(e) => st(s).error = Some(e.to_string()),
            }
        },
    );
}

// --- pairing ---

fn toggle_pairing(s: &mut State, id: String) {
    let ext = st(s);
    if ext.pairing.as_ref().is_some_and(|p| p.id == id) {
        ext.pairing = None;
        return;
    }
    ext.pairing = Some(Pairing {
        id: id.clone(),
        status: None,
        busy: true,
    });
    poll_pairing(s, id, None);
}

/// Read the pairing state; while a code is pending, check again every few
/// seconds until the account connects or the panel closes.
fn poll_pairing(s: &mut State, id: String, delay: Option<Duration>) {
    let job_id = id.clone();
    jobs::run(
        s,
        "extensions",
        move |c| async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            c.pairing_status(&job_id).await
        },
        move |s, result, _| {
            let ext = st(s);
            let Some(p) = ext.pairing.as_mut().filter(|p| p.id == id) else {
                return;
            };
            p.busy = false;
            match result {
                Ok(status) => {
                    let waiting = !status.connected && status.pending.is_some();
                    p.status = Some(status);
                    if waiting {
                        poll_pairing(s, id, Some(Duration::from_secs(3)));
                    }
                }
                Err(e) => ext.error = Some(e.to_string()),
            }
        },
    );
}

fn mint_pairing(s: &mut State) {
    let Some(p) = st(s).pairing.as_mut() else {
        return;
    };
    p.busy = true;
    let id = p.id.clone();
    let job_id = id.clone();
    jobs::run(
        s,
        "extensions",
        move |c| async move { c.mint_pairing_code(&job_id).await },
        move |s, result, _| {
            let ext = st(s);
            let Some(p) = ext.pairing.as_mut().filter(|p| p.id == id) else {
                return;
            };
            p.busy = false;
            match result {
                Ok(code) => {
                    p.status = Some(PairingStatus {
                        connected: false,
                        pending: Some(code),
                    });
                    poll_pairing(s, id, Some(Duration::from_secs(3)));
                }
                Err(e) => ext.error = Some(e.to_string()),
            }
        },
    );
}

fn unpair(s: &mut State) {
    let Some(p) = st(s).pairing.as_mut() else {
        return;
    };
    p.busy = true;
    let id = p.id.clone();
    let job_id = id.clone();
    jobs::run(
        s,
        "extensions",
        move |c| async move { c.unpair_extension(&job_id).await },
        move |s, result, _| match result {
            Ok(()) => poll_pairing(s, id, None),
            Err(e) => {
                let ext = st(s);
                if let Some(p) = ext.pairing.as_mut() {
                    p.busy = false;
                }
                ext.error = Some(e.to_string());
            }
        },
    );
}

// --- custom MCP and bundle import ---

fn toggle_mcp(s: &mut State) {
    let ext = st(s);
    ext.mcp = match ext.mcp.take() {
        Some(_) => None,
        None => {
            let mut name = TextEdit::new("");
            name.focused = true;
            Some(McpForm {
                name,
                endpoint: TextEdit::new(""),
                auth: 0,
                submitting: false,
            })
        }
    };
}

fn submit_mcp(s: &mut State) {
    let ext = st(s);
    let Some(form) = ext.mcp.as_mut() else {
        return;
    };
    let name = form.name.text.trim().to_string();
    let endpoint = form.endpoint.text.trim().to_string();
    let slug = mcp_slug(&name);
    if slug.is_empty() || endpoint.is_empty() {
        ext.error = Some("Enter a name and the server URL.".into());
        return;
    }
    let auth = mcp_auth(form.auth);
    form.submitting = true;
    ext.error = None;
    jobs::run(
        s,
        "extensions",
        move |c| async move { c.register_hosted_mcp(&slug, &name, &endpoint, auth).await },
        |s, result, _| match result {
            Ok(done) => {
                let ext = st(s);
                ext.mcp = None;
                ext.notice = Some(done.message);
                // Bearer and OAuth servers need their credential next.
                open_setup(s, done.package_ref.id.as_str().to_string());
                refresh(s);
            }
            Err(e) => {
                let ext = st(s);
                if let Some(f) = ext.mcp.as_mut() {
                    f.submitting = false;
                }
                ext.error = Some(e.to_string());
            }
        },
    );
}

fn import_bundle(_: &mut State, tx: &mut Tx) {
    let options = FileDialogOptions {
        title: "Import".into(),
        ..Default::default()
    };
    tx.open_file(options, |s: &mut State, picked, _| {
        let path = match picked {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "file dialog failed");
                st(s).error = Some(e.to_string());
                return;
            }
        };
        let ext = st(s);
        ext.importing = true;
        ext.error = None;
        jobs::run(
            s,
            "extensions",
            move |c| async move {
                let bundle = tokio::fs::read(&path)
                    .await
                    .map_err(|e| ClientError::Config(format!("cannot read the file: {e}")))?;
                c.import_extension(bundle).await
            },
            |s, result, _| {
                st(s).importing = false;
                match result {
                    Ok(resp) => {
                        st(s).notice = Some(resp.message);
                        refresh(s);
                    }
                    Err(e) => st(s).error = Some(e.to_string()),
                }
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::mcp_slug;

    #[test]
    fn slugs_drop_punctuation_and_the_mcp_prefix() {
        assert_eq!(mcp_slug("My Server"), "my-server");
        assert_eq!(mcp_slug("  Linear!! "), "linear");
        assert_eq!(mcp_slug("mcp-github"), "github");
    }
}
