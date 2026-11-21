//! A module resolver over bytes an installer already holds.
//!
//! An installer that reads a package's hook modules once and installs them
//! into many per-run dispatchers hands each install a [`PinnedHookModules`]
//! through [`crate::registrar::HookRegistrar::with_module_resolver`]: the
//! install compiles exactly the bytes the installer admitted, however the
//! installer's own cache changes meanwhile. The runtime's compiled-module
//! cache is still shared (keyed by content digest).

use std::collections::HashMap;
use std::sync::Arc;

use super::{WasmHookModuleRequest, WasmHookModuleResolver, WasmHookRuntimeError};

/// Immutable module bytes for one extension version's WASM hooks, keyed by
/// hook local id.
pub type HookModuleBytes = Arc<HashMap<String, Arc<[u8]>>>;

/// Resolves only the given extension version's hooks, from `modules`.
pub struct PinnedHookModules {
    extension_id: String,
    version: String,
    modules: HookModuleBytes,
}

impl PinnedHookModules {
    pub fn new(
        extension_id: impl Into<String>,
        version: impl Into<String>,
        modules: HookModuleBytes,
    ) -> Self {
        Self {
            extension_id: extension_id.into(),
            version: version.into(),
            modules,
        }
    }

    /// A resolver that refuses every module: for a runtime whose installs
    /// always bring their own pinned bytes.
    pub fn none() -> Self {
        Self::new(String::new(), String::new(), HookModuleBytes::default())
    }
}

impl WasmHookModuleResolver for PinnedHookModules {
    fn resolve_module(
        &self,
        request: &WasmHookModuleRequest<'_>,
    ) -> Result<Vec<u8>, WasmHookRuntimeError> {
        let pinned = request.extension_id.as_str() == self.extension_id
            && request.extension_version == self.version;
        pinned
            .then(|| self.modules.get(request.hook_local_id.as_str()))
            .flatten()
            .map(|bytes| bytes.to_vec())
            .ok_or_else(|| {
                WasmHookRuntimeError::Compilation(format!(
                    "no module bytes were admitted for hook `{}` of extension `{}` {}",
                    request.hook_local_id.as_str(),
                    request.extension_id.as_str(),
                    request.extension_version
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::HookLocalId;
    use crate::manifest::HookManifestKind;

    #[test]
    fn resolves_only_the_pinned_extension_version() {
        let modules: HookModuleBytes = Arc::new(HashMap::from([(
            "gate".to_string(),
            Arc::<[u8]>::from(&b"\0asm"[..]),
        )]));
        let pinned = PinnedHookModules::new("ext", "1.0.0", modules);
        let extension = ironclaw_host_api::ids::ExtensionId::new("ext").unwrap();
        let other = ironclaw_host_api::ids::ExtensionId::new("other").unwrap();
        let gate = HookLocalId::new("gate").unwrap();
        let missing = HookLocalId::new("missing").unwrap();
        let request = |extension_id, extension_version, hook_local_id| WasmHookModuleRequest {
            extension_id,
            extension_version,
            hook_local_id,
            kind: HookManifestKind::BeforeCapability,
            export: "evaluate",
        };
        assert_eq!(
            pinned
                .resolve_module(&request(&extension, "1.0.0", &gate))
                .unwrap(),
            b"\0asm"
        );
        assert!(
            pinned
                .resolve_module(&request(&extension, "1.0.1", &gate))
                .is_err()
        );
        assert!(
            pinned
                .resolve_module(&request(&other, "1.0.0", &gate))
                .is_err()
        );
        assert!(
            pinned
                .resolve_module(&request(&extension, "1.0.0", &missing))
                .is_err()
        );
        assert!(
            PinnedHookModules::none()
                .resolve_module(&request(&extension, "1.0.0", &gate))
                .is_err()
        );
    }
}
