//! Device tokens, one per saved computer: the macOS Keychain (service
//! `ZLO`, account `<environment_id>`), behind [`TokenStore`] so tests use
//! [`MemoryTokens`]. Calls may block (the Keychain can ask the user): run
//! them off the UI thread.

use std::collections::HashMap;
use std::sync::Mutex;

/// Keychain service name of ZLO's device tokens.
pub const SERVICE: &str = "ZLO";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Keychain: {0}")]
pub struct KeychainError(pub String);

pub trait TokenStore: Send + Sync {
    /// `Ok(None)` when no token is saved for the computer.
    fn get(&self, environment_id: &str) -> Result<Option<String>, KeychainError>;
    fn set(&self, environment_id: &str, token: &str) -> Result<(), KeychainError>;
    /// Deleting a missing token is fine.
    fn delete(&self, environment_id: &str) -> Result<(), KeychainError>;
}

/// The login Keychain.
pub struct Keychain;

#[cfg(target_os = "macos")]
mod mac {
    use security_framework::base::Error;
    use security_framework::passwords;

    use super::{KeychainError, SERVICE};

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    fn err(e: Error) -> KeychainError {
        KeychainError(e.to_string())
    }

    pub fn get(account: &str) -> Result<Option<String>, KeychainError> {
        match passwords::get_generic_password(SERVICE, account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| KeychainError(format!("saved token is not text: {e}"))),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    pub fn set(account: &str, token: &str) -> Result<(), KeychainError> {
        passwords::set_generic_password(SERVICE, account, token.as_bytes()).map_err(err)
    }

    pub fn delete(account: &str) -> Result<(), KeychainError> {
        match passwords::delete_generic_password(SERVICE, account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == NOT_FOUND => Ok(()),
            Err(e) => Err(err(e)),
        }
    }
}

#[cfg(target_os = "macos")]
impl TokenStore for Keychain {
    fn get(&self, environment_id: &str) -> Result<Option<String>, KeychainError> {
        mac::get(environment_id)
    }
    fn set(&self, environment_id: &str, token: &str) -> Result<(), KeychainError> {
        mac::set(environment_id, token)
    }
    fn delete(&self, environment_id: &str) -> Result<(), KeychainError> {
        mac::delete(environment_id)
    }
}

#[cfg(not(target_os = "macos"))]
impl TokenStore for Keychain {
    fn get(&self, _: &str) -> Result<Option<String>, KeychainError> {
        Err(KeychainError("only available on macOS".into()))
    }
    fn set(&self, _: &str, _: &str) -> Result<(), KeychainError> {
        Err(KeychainError("only available on macOS".into()))
    }
    fn delete(&self, _: &str) -> Result<(), KeychainError> {
        Err(KeychainError("only available on macOS".into()))
    }
}

/// In-memory tokens for tests and headless runs.
#[derive(Default)]
pub struct MemoryTokens(Mutex<HashMap<String, String>>);

impl MemoryTokens {
    fn map(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, String>>, KeychainError> {
        self.0
            .lock()
            .map_err(|e| KeychainError(format!("token map poisoned: {e}")))
    }
}

impl TokenStore for MemoryTokens {
    fn get(&self, environment_id: &str) -> Result<Option<String>, KeychainError> {
        Ok(self.map()?.get(environment_id).cloned())
    }
    fn set(&self, environment_id: &str, token: &str) -> Result<(), KeychainError> {
        self.map()?
            .insert(environment_id.to_string(), token.to_string());
        Ok(())
    }
    fn delete(&self, environment_id: &str) -> Result<(), KeychainError> {
        self.map()?.remove(environment_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_tokens_behave_like_the_keychain() {
        let t = MemoryTokens::default();
        assert_eq!(t.get("e"), Ok(None));
        t.set("e", "icd_x_y").unwrap();
        assert_eq!(t.get("e"), Ok(Some("icd_x_y".into())));
        t.delete("e").unwrap();
        t.delete("e").unwrap();
        assert_eq!(t.get("e"), Ok(None));
    }
}
