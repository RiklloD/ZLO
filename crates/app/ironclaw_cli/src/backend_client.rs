//! Talks to the running `zlo serve` over loopback with the master
//! WebUI token, for `pair`, `devices` and `remote status`.
//!
//! The origin comes from `[webui]` (listen host/port, the same defaults
//! `serve` uses; a wildcard bind host is reached through 127.0.0.1), and the
//! token from the same resolution `serve` performs (`[webui].env_token_var`,
//! else `<reborn_home>/webui-token`). Only loopback origins are used, so the
//! master token never leaves the machine.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context as _;
use ironclaw_product_contracts::environment_access::{
    DEVICES_PATH, DeviceList, ENVIRONMENT_DESCRIPTOR_PATH, EnvironmentDescriptor,
    PAIRING_CODES_PATH, PairingCodeIssued,
};
use secrecy::{ExposeSecret, SecretString};

use crate::context::RebornCliContext;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct RunningBackend {
    origin: String,
    token: SecretString,
    http: reqwest::Client,
}

impl RunningBackend {
    pub(crate) fn from_context(context: &RebornCliContext) -> anyhow::Result<Self> {
        let home = context.boot_config().home();
        let config_file = ironclaw_config::RebornConfigFile::load(&home.config_file_path())
            .map_err(anyhow::Error::from)?;
        let webui = config_file.as_ref().and_then(|file| file.webui.as_ref());
        let origin = loopback_origin(
            webui.and_then(|section| section.listen_host.as_deref()),
            webui.and_then(|section| section.listen_port),
        )?;
        let env_var = crate::webui_token::resolve_env_token_var_name(config_file.as_ref());
        let token = crate::webui_token::resolve_webui_token(
            env_var,
            crate::commands::serve::present_unicode_env_var(env_var)?.as_deref(),
            home.path(),
        )?;
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .no_proxy()
            .build()
            .context("could not build the loopback HTTP client")?;
        Ok(Self {
            origin,
            token: SecretString::from(token.value),
            http,
        })
    }

    /// `http://127.0.0.1:<port>`, the listener this CLI talks to.
    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    pub(crate) async fn descriptor(&self) -> anyhow::Result<EnvironmentDescriptor> {
        let response = self
            .http
            .get(self.url(ENVIRONMENT_DESCRIPTOR_PATH))
            .send()
            .await
            .map_err(|error| self.unreachable(error))?;
        decode(response).await
    }

    pub(crate) async fn issue_pairing_code(&self) -> anyhow::Result<PairingCodeIssued> {
        let response = self
            .http
            .post(self.url(PAIRING_CODES_PATH))
            .bearer_auth(self.token.expose_secret())
            .send()
            .await
            .map_err(|error| self.unreachable(error))?;
        decode(response).await
    }

    pub(crate) async fn list_devices(&self) -> anyhow::Result<DeviceList> {
        let response = self
            .http
            .get(self.url(DEVICES_PATH))
            .bearer_auth(self.token.expose_secret())
            .send()
            .await
            .map_err(|error| self.unreachable(error))?;
        decode(response).await
    }

    pub(crate) async fn revoke_device(&self, device_id: &str) -> anyhow::Result<()> {
        let response = self
            .http
            .delete(self.url(&format!("{DEVICES_PATH}/{device_id}")))
            .bearer_auth(self.token.expose_secret())
            .send()
            .await
            .map_err(|error| self.unreachable(error))?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(());
        }
        Err(http_error(response).await)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    fn unreachable(&self, error: reqwest::Error) -> anyhow::Error {
        anyhow::anyhow!(
            "could not reach the IronClaw backend at {} ({error}). Is `zlo serve` running?",
            self.origin
        )
    }
}

async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> anyhow::Result<T> {
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    response
        .json::<T>()
        .await
        .context("the backend answered with an unexpected body")
}

async fn http_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default(); // silent-ok: the body only enriches an error that is already being returned
    let code = body
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match status {
        reqwest::StatusCode::UNAUTHORIZED => anyhow::anyhow!(
            "the backend rejected the WebUI token (401). Check {} or the webui-token file.",
            crate::webui_token::default_env_token_var()
        ),
        reqwest::StatusCode::NOT_FOUND if code == "not_found" => {
            anyhow::anyhow!("no such device")
        }
        reqwest::StatusCode::NOT_FOUND => anyhow::anyhow!(
            "the running backend does not have remote-environment routes; restart `zlo serve` \
             with this build"
        ),
        _ => anyhow::anyhow!("the backend answered {status} {code}"),
    }
}

/// The loopback origin for a `[webui]` listen host/port.
pub(crate) fn loopback_origin(host: Option<&str>, port: Option<u16>) -> anyhow::Result<String> {
    let port = port.unwrap_or(crate::commands::serve::DEFAULT_SERVE_PORT);
    let ip = match host {
        Some(raw) => IpAddr::from_str(raw)
            .map_err(|error| anyhow::anyhow!("[webui].listen_host `{raw}` invalid: {error}"))?,
        None => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    let ip = if ip.is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        ip
    };
    if !ip.is_loopback() {
        anyhow::bail!(
            "the backend listens on {ip}, not loopback; `pair` and `devices` only send the master \
             token to a loopback listener"
        );
    }
    Ok(format!("http://{}", SocketAddr::new(ip, port)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_is_always_loopback() {
        assert_eq!(
            loopback_origin(None, None).expect("default"),
            "http://127.0.0.1:3000"
        );
        assert_eq!(
            loopback_origin(Some("0.0.0.0"), Some(3900)).expect("wildcard"),
            "http://127.0.0.1:3900"
        );
        assert_eq!(
            loopback_origin(Some("::1"), Some(3900)).expect("v6"),
            "http://[::1]:3900"
        );
        assert!(loopback_origin(Some("192.168.1.5"), None).is_err());
    }
}
