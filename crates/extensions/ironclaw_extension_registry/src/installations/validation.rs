use super::*;

pub(super) fn validate_installation_against_one_manifest(
    manifest: &ExtensionManifestRecord,
    installation: &ExtensionInstallation,
) -> Result<(), ExtensionInstallationError> {
    if manifest.extension_id() != installation.manifest_ref().extension_id() {
        return Err(ExtensionInstallationError::ManifestExtensionMismatch {
            extension_id: installation.extension_id().clone(),
            manifest_extension_id: installation.manifest_ref().extension_id().clone(),
        });
    }
    match (
        manifest.manifest_hash(),
        installation.manifest_ref().manifest_hash(),
    ) {
        (Some(registered), Some(referenced)) if registered != referenced => {
            return Err(ExtensionInstallationError::ManifestHashMismatch {
                extension_id: installation.extension_id().clone(),
            });
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(ExtensionInstallationError::ManifestHashMismatch {
                extension_id: installation.extension_id().clone(),
            });
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn validate_bindings_unique(
    credential_bindings: &[ExtensionCredentialBinding],
) -> Result<(), ExtensionInstallationError> {
    let mut seen = std::collections::BTreeSet::new();
    for binding in credential_bindings {
        if !seen.insert(binding.credential_handle.clone()) {
            return Err(ExtensionInstallationError::DuplicateCredentialBinding {
                handle: binding.credential_handle.clone(),
            });
        }
    }
    Ok(())
}

pub(super) fn validate_nonempty_noncontrol(
    field: &'static str,
    value: &str,
) -> Result<(), ExtensionInstallationError> {
    if value.is_empty() {
        return Err(ExtensionInstallationError::InvalidValue {
            field,
            reason: "must not be empty".to_string(),
        });
    }
    if value.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(ExtensionInstallationError::InvalidValue {
            field,
            reason: "must not contain control characters".to_string(),
        });
    }
    Ok(())
}

pub(super) fn validate_cleanup_id(
    value: String,
    label: &'static str,
) -> Result<String, ExtensionInstallationError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(value)
    } else {
        Err(ExtensionInstallationError::InvalidValue {
            field: label,
            reason: "must be a bounded lowercase identifier".to_string(),
        })
    }
}
