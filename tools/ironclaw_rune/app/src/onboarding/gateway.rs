//! The computer ZLO is connected to, as setup and the views name it.
//!
//! The saved computers themselves (T3 Code's environments) live in
//! `environments.json` with their tokens in the Keychain; `crate::remote`
//! owns that registry and the connect path. This is only the handle of the
//! one in use: this Mac's own backend, or a paired computer by its
//! permanent `environment_id`.

/// A paired computer: its permanent id and the name it gave itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRef {
    pub environment_id: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Computer {
    #[default]
    ThisMac,
    Remote(RemoteRef),
}

impl Computer {
    pub fn remote(environment_id: impl Into<String>, label: impl Into<String>) -> Self {
        Computer::Remote(RemoteRef {
            environment_id: environment_id.into(),
            label: label.into(),
        })
    }

    pub fn label(&self) -> String {
        match self {
            Computer::ThisMac => "This Mac".into(),
            Computer::Remote(r) => r.label.clone(),
        }
    }

    /// Stable key: `this-mac`, or the environment id (also the drafts and
    /// selected-thread scope).
    pub fn key(&self) -> String {
        match self {
            Computer::ThisMac => zlo_client::environments::THIS_MAC.into(),
            Computer::Remote(r) => r.environment_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computers_are_named_and_keyed() {
        assert_eq!(Computer::ThisMac.label(), "This Mac");
        assert_eq!(Computer::ThisMac.key(), "this-mac");
        let studio = Computer::remote("env-1", "Studio");
        assert_eq!(studio.label(), "Studio");
        assert_eq!(studio.key(), "env-1");
    }
}
