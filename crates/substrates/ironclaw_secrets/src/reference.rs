//! Secret references: a stored secret whose value is a reference
//! (`op://vault/item/field`) is read from the password manager each time it
//! is consumed, so the credential stays in 1Password and this store keeps
//! only where it lives.
//!
//! 1Password is read with its CLI (`op read`), signed in on the machine that
//! runs the backend — a desktop sign-in, a service account
//! (`OP_SERVICE_ACCOUNT_TOKEN`) or a Connect server (`OP_CONNECT_HOST` +
//! `OP_CONNECT_TOKEN`), all of which `op` reads from its environment. A
//! failed read names the provider and never carries a value.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::SecretMaterial;

/// Reads secret references of one kind.
#[async_trait]
pub trait SecretReferenceResolver: Send + Sync {
    /// Whether `raw` (a stored value, or a typed answer) is a reference.
    fn is_reference(&self, raw: &str) -> bool;

    /// The value `reference` names. The error is safe to show: it names the
    /// provider and what to do, never the value.
    async fn resolve(&self, reference: &str) -> Result<SecretMaterial, String>;
}

/// Longest wait for `op`: a desktop sign-in prompt the person never answers
/// must not hold a credential injection forever.
const OP_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest value accepted from `op`.
const MAX_VALUE_BYTES: usize = 64 * 1024;

/// [`SecretReferenceResolver`] for `op://` references via the 1Password CLI.
#[derive(Debug, Clone)]
pub struct OnePasswordReferenceResolver {
    /// The `op` binary; found on `PATH` (and the usual install folders) by default.
    binary: Option<PathBuf>,
}

impl Default for OnePasswordReferenceResolver {
    fn default() -> Self {
        Self { binary: find_op() }
    }
}

impl OnePasswordReferenceResolver {
    /// Use a specific `op` binary.
    pub fn with_binary(binary: PathBuf) -> Self {
        Self {
            binary: Some(binary),
        }
    }

    /// Whether an `op` binary was found on this machine.
    pub fn available(&self) -> bool {
        self.binary.is_some()
    }
}

/// `op://<vault>/<item>/<field>` (with an optional `/<section>` before the
/// field and `?attribute=` after it): three or more non-empty segments.
pub fn is_op_reference(raw: &str) -> bool {
    let Some(rest) = raw.trim().strip_prefix("op://") else {
        return false;
    };
    let path = rest.split('?').next().unwrap_or_default();
    let segments: Vec<&str> = path.split('/').collect();
    segments.len() >= 3
        && segments.iter().all(|s| !s.is_empty())
        && !raw.chars().any(char::is_control)
}

fn find_op() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.extend(
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
            .into_iter()
            .map(PathBuf::from),
    );
    dirs.into_iter()
        .map(|dir| dir.join("op"))
        .find(|candidate| candidate.is_file())
}

#[async_trait]
impl SecretReferenceResolver for OnePasswordReferenceResolver {
    fn is_reference(&self, raw: &str) -> bool {
        is_op_reference(raw)
    }

    async fn resolve(&self, reference: &str) -> Result<SecretMaterial, String> {
        let reference = reference.trim();
        if !is_op_reference(reference) {
            return Err("not a 1Password reference (op://vault/item/field)".into());
        }
        let Some(binary) = self.binary.as_ref() else {
            return Err(
                "1Password: the `op` CLI is not installed on the machine that runs ZLO".into(),
            );
        };
        let mut command = tokio::process::Command::new(binary);
        command
            .args(["read", "--no-newline", reference])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(OP_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(format!("1Password: could not run `op` ({error})")),
            Err(_) => {
                return Err(
                    "1Password did not answer in time: unlock it on the machine that runs ZLO, \
                     or sign `op` in with a service account"
                        .into(),
                );
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let hint = if stderr.contains("not signed in") || stderr.contains("sign in") {
                "1Password is locked or signed out on the machine that runs ZLO"
            } else if stderr.contains("isn't an item") || stderr.contains("not found") {
                "1Password has no such item or field"
            } else {
                "1Password could not read that reference"
            };
            return Err(hint.to_string());
        }
        if output.stdout.len() > MAX_VALUE_BYTES {
            return Err("1Password returned a value that is too large".into());
        }
        let value = String::from_utf8(output.stdout)
            .map_err(|_| "1Password returned a value that is not text".to_string())?;
        if value.is_empty() {
            return Err("1Password returned an empty value".into());
        }
        Ok(SecretMaterial::from(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_need_vault_item_and_field() {
        assert!(is_op_reference("op://Private/GitHub/token"));
        assert!(is_op_reference(
            "op://Private/GitHub/section/token?attribute=otp"
        ));
        assert!(!is_op_reference("op://Private/GitHub"));
        assert!(!is_op_reference("op:///item/field"));
        assert!(!is_op_reference("hunter2"));
        assert!(!is_op_reference("op://a/b/c\n"));
    }

    #[tokio::test]
    async fn a_missing_cli_names_the_provider() {
        let resolver = OnePasswordReferenceResolver { binary: None };
        let error = resolver.resolve("op://a/b/c").await.unwrap_err();
        assert!(error.contains("1Password"), "{error}");
    }
}
