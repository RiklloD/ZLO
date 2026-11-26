//! `ProductSurface` decorator that answers Settings → Keys
//! (`ironclaw_product_contracts::service_keys`): the deployment's service API
//! keys, stored in the operator secret scope.
//!
//! A set is pre-validated: the key's verifier makes one authenticated call
//! first and a rejected key is never stored. A service that cannot be reached
//! stores the key marked unverified. Every write is read back from the store,
//! and the verifier is told the key changed so no stale copy stays cached.
//! The key value is never returned, logged or put in an error.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ids::SecretHandle;
use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
use ironclaw_product_contracts::service_keys::*;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse, ProductSurfaceValidationCode,
};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// One key the page manages: what it is, and how to prove a value works.
pub struct ServiceKey {
    pub descriptor: ServiceKeyDescriptor,
    pub verifier: Option<Arc<dyn ServiceKeyVerifier>>,
}

pub struct ServiceKeysSurface {
    inner: Arc<dyn ProductSurface>,
    keys: Vec<ServiceKey>,
    store: Arc<dyn OperatorSecretValueStore>,
    import_sources: Vec<Arc<dyn ServiceKeyImportSource>>,
}

impl ServiceKeysSurface {
    pub fn new(
        inner: Arc<dyn ProductSurface>,
        keys: Vec<ServiceKey>,
        store: Arc<dyn OperatorSecretValueStore>,
        import_sources: Vec<Arc<dyn ServiceKeyImportSource>>,
    ) -> Self {
        Self {
            inner,
            keys,
            store,
            import_sources,
        }
    }

    /// Production keys: Supermemory (verified through `supermemory`, whose
    /// cached key is dropped on every change), with copies looked up in
    /// Hermes under `$HOME`.
    pub fn for_host(
        inner: Arc<dyn ProductSurface>,
        supermemory: Arc<ironclaw_supermemory::Supermemory>,
        store: Arc<dyn OperatorSecretValueStore>,
    ) -> Arc<dyn ProductSurface> {
        let import_sources = crate::orchestration::ProfileHome::from_env()
            .map(|home| {
                Arc::new(crate::orchestration::HermesKeySource::new(&home))
                    as Arc<dyn ServiceKeyImportSource>
            })
            .into_iter()
            .collect();
        let keys = vec![ServiceKey {
            descriptor: ironclaw_supermemory::supermemory_service_key(),
            verifier: Some(Arc::new(ironclaw_supermemory::SupermemoryKeyVerifier::new(
                supermemory,
            ))),
        }];
        Arc::new(Self::new(inner, keys, store, import_sources))
    }

    fn key(&self, id: &str) -> Result<(&ServiceKey, SecretHandle), ProductSurfaceError> {
        let unknown =
            || ProductSurfaceError::validation("id", ProductSurfaceValidationCode::UnknownKey);
        let key = self
            .keys
            .iter()
            .find(|key| key.descriptor.id == id.trim())
            .ok_or_else(unknown)?;
        let handle = SecretHandle::new(key.descriptor.id.as_str()).map_err(|error| {
            tracing::debug!(%error, "service keys: descriptor id is not a secret handle");
            unknown()
        })?;
        Ok((key, handle))
    }

    /// The single copy another local tool holds, if any.
    async fn found(&self, key: &ServiceKey) -> Option<FoundServiceKey> {
        let env_var = key.descriptor.import_env_var.as_deref()?;
        for source in &self.import_sources {
            if let Some(found) = source.find(env_var).await {
                return Some(found);
            }
        }
        None
    }

    async fn list(&self) -> Result<ServiceKeysListResponse, ProductSurfaceError> {
        let mut keys = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            let (_, handle) = self.key(&key.descriptor.id)?;
            let stored = self
                .store
                .read(&handle)
                .await
                .map_err(ProductSurfaceError::internal_from)?;
            let import_from = match self.found(key).await {
                Some(found)
                    if stored.as_ref().is_none_or(|value| {
                        value.expose_secret() != found.value.expose_secret()
                    }) =>
                {
                    Some(found.source)
                }
                _ => None,
            };
            keys.push(ServiceKeyStatus {
                id: key.descriptor.id.clone(),
                label: key.descriptor.label.clone(),
                purpose: key.descriptor.purpose.clone(),
                docs_url: key.descriptor.docs_url.clone(),
                stored: stored.is_some(),
                import_from,
            });
        }
        Ok(ServiceKeysListResponse { keys })
    }

    /// Verify, store, read back, and tell the verifier.
    async fn store_value(
        &self,
        caller: &ProductSurfaceCaller,
        id: &str,
        value: SecretString,
        source: Option<String>,
    ) -> Result<ServiceKeySetResponse, ProductSurfaceError> {
        let (key, handle) = self.key(id)?;
        let check = match &key.verifier {
            None => ServiceKeyCheck::NotChecked,
            Some(verifier) => match verifier.verify(caller, &value).await {
                Ok(()) => ServiceKeyCheck::Verified,
                Err(ServiceKeyVerifyError::Rejected) => {
                    return Err(ProductSurfaceError::validation(
                        "value",
                        ProductSurfaceValidationCode::InvalidValue,
                    ));
                }
                Err(ServiceKeyVerifyError::Unreachable { reason }) => {
                    // A stored key switches the service on (Supermemory then
                    // replaces local memory), so an unproven key is not
                    // stored: the person tries again once it answers.
                    tracing::debug!(%reason, id = %key.descriptor.id, "service keys: could not verify; not stored");
                    return Err(ProductSurfaceError::unavailable(true));
                }
            },
        };
        self.store
            .put(&handle, value)
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        if let Some(verifier) = &key.verifier {
            verifier.key_changed().await;
        }
        let stored = self
            .store
            .contains(&handle)
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        Ok(ServiceKeySetResponse {
            id: key.descriptor.id.clone(),
            stored,
            check,
            source,
        })
    }

    async fn set(
        &self,
        caller: &ProductSurfaceCaller,
        body: ServiceKeySetRequest,
    ) -> Result<ServiceKeySetResponse, ProductSurfaceError> {
        let value = validate_value(&body.value)?;
        self.store_value(caller, &body.id, value, None).await
    }

    async fn import(
        &self,
        caller: &ProductSurfaceCaller,
        body: ServiceKeyIdRequest,
    ) -> Result<ServiceKeySetResponse, ProductSurfaceError> {
        let (key, _) = self.key(&body.id)?;
        let found = self
            .found(key)
            .await
            .ok_or_else(ProductSurfaceError::not_found)?;
        let value = validate_value(found.value.expose_secret())?;
        self.store_value(caller, &body.id, value, Some(found.source))
            .await
    }

    async fn remove(
        &self,
        body: ServiceKeyIdRequest,
    ) -> Result<ServiceKeyRemoveResponse, ProductSurfaceError> {
        let (key, handle) = self.key(&body.id)?;
        self.store
            .delete(&handle)
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        if let Some(verifier) = &key.verifier {
            verifier.key_changed().await;
        }
        let stored = self
            .store
            .contains(&handle)
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        Ok(ServiceKeyRemoveResponse {
            id: key.descriptor.id.clone(),
            stored,
        })
    }
}

#[async_trait]
impl ProductSurface for ServiceKeysSurface {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        let id = request.operation_id.as_str();
        let output = if id == SERVICE_KEY_SET_COMMAND_ID {
            let body = parse::<ServiceKeySetRequest>(request.input)?;
            to_value(self.set(&caller, body).await?)?
        } else if id == SERVICE_KEY_IMPORT_COMMAND_ID {
            let body = parse::<ServiceKeyIdRequest>(request.input)?;
            to_value(self.import(&caller, body).await?)?
        } else if id == SERVICE_KEY_REMOVE_COMMAND_ID {
            let body = parse::<ServiceKeyIdRequest>(request.input)?;
            to_value(self.remove(body).await?)?
        } else {
            return self.inner.invoke(caller, request).await;
        };
        Ok(ProductSurfaceInvokeResponse { output })
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        if request.view_id.as_str() != SERVICE_KEYS_VIEW.id {
            return self.inner.query(caller, request).await;
        }
        parse::<ServiceKeysListRequest>(request.input)?;
        Ok(ProductSurfaceQueryPage {
            items: vec![to_value(self.list().await?)?],
            next_cursor: None,
        })
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        self.inner.stream_events(caller, request).await
    }
}

/// Trimmed, non-empty, bounded, printable.
fn validate_value(raw: &str) -> Result<SecretString, ProductSurfaceError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(ProductSurfaceError::validation(
            "value",
            ProductSurfaceValidationCode::Blank,
        ));
    }
    if value.len() > MAX_SERVICE_KEY_BYTES {
        return Err(ProductSurfaceError::validation(
            "value",
            ProductSurfaceValidationCode::TooLong,
        ));
    }
    if value
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(ProductSurfaceError::validation(
            "value",
            ProductSurfaceValidationCode::InvalidControlCharacter,
        ));
    }
    Ok(SecretString::from(value.to_string()))
}

fn parse<T: DeserializeOwned>(input: serde_json::Value) -> Result<T, ProductSurfaceError> {
    serde_json::from_value(input).map_err(|error| {
        // serde's message can quote the offending value (a pasted key).
        tracing::debug!(
            line = error.line(),
            column = error.column(),
            "service keys: invalid input"
        );
        ProductSurfaceError::validation("input", ProductSurfaceValidationCode::InvalidValue)
    })
}

fn to_value<T: Serialize>(value: T) -> Result<serde_json::Value, ProductSurfaceError> {
    serde_json::to_value(value).map_err(ProductSurfaceError::internal_from)
}

#[cfg(test)]
#[path = "service_keys_tests.rs"]
mod tests;
