//! Continuous configuration sync service and `ProductSurface` decorator.
//!
//! Each syncable domain implements [`ConfigSyncDomainHandler`] in its own
//! submodule (`config`, `orchestrator_settings`, `prompts`, `approvals`,
//! `profiles`, `skills`, `skill_usage`, `memory`), so a later phase only adds
//! a domain module and registers it at composition.
//!
//! Invariants:
//! - Every domain reaches its state through the owning service/port, never raw
//!   SQL from the product layer.
//! - `apply` validates the entire snapshot (schema version, total size,
//!   unknown domains, payload variants, and per-domain validation) BEFORE
//!   writing anything.
//! - Each domain apply is idempotent and atomic per domain.
//! - Canonical hashes are computed over sorted-key JSON with `$HOME` /
//!   `$ZLO_HOME` path normalization so macOS/Linux and libSQL/Postgres produce
//!   identical digests for identical content.
//! - Threads, turns, runs, events, inbox, and device/identity records are never
//!   touched.

pub mod approvals;
pub mod cli_home;
pub mod config;
pub mod items;
pub mod memory;
pub mod memory_base;
pub mod orchestrator_settings;
pub mod profiles;
pub mod prompts;
pub mod skill_usage;
pub mod skills;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_config::HomePathRewrite;
use ironclaw_product_contracts::config_sync::{
    CONFIG_SYNC_APPLY_COMMAND_ID, CONFIG_SYNC_EXPORT_VIEW_ID, CONFIG_SYNC_MANIFEST_VIEW_ID,
    CONFIG_SYNC_MAX_SNAPSHOT_BYTES, CONFIG_SYNC_SCHEMA_VERSION, ConfigSnapshotV1,
    ConfigSyncApplyOutcome, ConfigSyncApplyReport, ConfigSyncApplyRequest, ConfigSyncDomain,
    ConfigSyncDomainDigest, ConfigSyncDomainPayload, ConfigSyncExportOutcome,
    ConfigSyncExportRequest, ConfigSyncManifest, DomainApplyOutcome, DomainApplyStatus,
};
use ironclaw_product_contracts::environment_access::EnvironmentAccessService;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// Typed errors produced by [`ConfigSyncService`] and domain handlers.
#[derive(Debug, Error)]
pub enum ConfigSyncError {
    #[error("validation failed for `{field}`: {reason}")]
    Validation { field: String, reason: String },
    #[error("config sync store error in `{domain}`: {reason}")]
    Store {
        domain: ConfigSyncDomain,
        reason: String,
    },
    #[error("config sync I/O error in `{domain}`: {source}")]
    Io {
        domain: ConfigSyncDomain,
        #[source]
        source: std::io::Error,
    },
    #[error("config sync serialization error in `{domain}`: {source}")]
    Serialization {
        domain: ConfigSyncDomain,
        #[source]
        source: serde_json::Error,
    },
    #[error("config sync unavailable: {reason}")]
    Unavailable { reason: String },
}

impl ConfigSyncError {
    pub fn validation(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

/// Host-supplied context passed to every [`ConfigSyncDomainHandler`] call.
#[derive(Debug, Clone)]
pub struct ConfigSyncContext {
    pub caller: ProductSurfaceCaller,
    pub data_folder: String,
    pub home_dir: String,
    /// Export: how an itemized domain's page is cut.
    pub page: items::PageRequest,
    /// Apply: the return leg of a two-way domain (see
    /// `ConfigSyncApplyRequest::merge_back`).
    pub merge_back: bool,
    /// Apply: the installation the snapshot came from.
    pub peer_environment_id: Option<String>,
}

impl ConfigSyncContext {
    /// Rewrite host absolute paths into `$ZLO_HOME` / `$HOME` tokens for export.
    pub fn to_tokens_rewrite(&self) -> HomePathRewrite {
        HomePathRewrite::to_tokens(&self.data_folder, &self.home_dir)
    }

    /// Expand `$ZLO_HOME` / `$HOME` tokens into this host's paths for apply.
    pub fn from_tokens_rewrite(&self) -> HomePathRewrite {
        HomePathRewrite::from_tokens(&self.data_folder, &self.home_dir)
    }
}

/// Per-domain apply result returned by [`ConfigSyncDomainHandler::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainApplyResult {
    pub outcome: DomainApplyOutcome,
    /// Dotted `config.toml` keys/sections that changed on disk and require a
    /// backend restart to take effect.
    pub restart_required: Vec<String>,
}

impl DomainApplyResult {
    pub fn applied(applied_count: u64, skipped_count: u64) -> Self {
        Self {
            outcome: DomainApplyOutcome {
                status: DomainApplyStatus::Applied,
                reason: None,
                applied_count,
                skipped_count,
                warnings: Vec::new(),
            },
            restart_required: Vec::new(),
        }
    }

    pub fn unchanged(skipped_count: u64) -> Self {
        Self {
            outcome: DomainApplyOutcome {
                status: DomainApplyStatus::SkippedUnchanged,
                reason: None,
                applied_count: 0,
                skipped_count,
                warnings: Vec::new(),
            },
            restart_required: Vec::new(),
        }
    }

    pub fn skipped_with_reason(reason: impl Into<String>, skipped_count: u64) -> Self {
        Self {
            outcome: DomainApplyOutcome {
                status: DomainApplyStatus::SkippedWithReason,
                reason: Some(reason.into()),
                applied_count: 0,
                skipped_count,
                warnings: Vec::new(),
            },
            restart_required: Vec::new(),
        }
    }
}

/// Extension point implemented by each configuration sync domain module.
#[async_trait]
pub trait ConfigSyncDomainHandler: Send + Sync {
    /// Which sync domain this handler owns.
    fn domain(&self) -> ConfigSyncDomain;

    /// Compute the lightweight manifest digest for this domain.
    async fn manifest(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError>;

    /// Export this domain's portable payload (paths normalized to
    /// `$ZLO_HOME` / `$HOME`).
    async fn export(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError>;

    /// Validate an incoming payload without mutating any state.
    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError>;

    /// Atomically apply an already-validated payload to this host's store.
    async fn apply(
        &self,
        ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError>;
}

/// Product-layer orchestrator for configuration sync across registered domain
/// handlers.
pub struct ConfigSyncService {
    environment_access: Arc<dyn EnvironmentAccessService>,
    data_folder: String,
    home_dir: String,
    handlers: BTreeMap<ConfigSyncDomain, Arc<dyn ConfigSyncDomainHandler>>,
}

impl ConfigSyncService {
    pub fn new(
        environment_access: Arc<dyn EnvironmentAccessService>,
        data_folder: impl Into<String>,
        home_dir: impl Into<String>,
    ) -> Self {
        Self {
            environment_access,
            data_folder: data_folder.into(),
            home_dir: home_dir.into(),
            handlers: BTreeMap::new(),
        }
    }

    /// Register a domain handler. Replacing an existing handler for the same
    /// domain overwrites the previous registration.
    pub fn with_handler(mut self, handler: Arc<dyn ConfigSyncDomainHandler>) -> Self {
        self.handlers.insert(handler.domain(), handler);
        self
    }

    fn context(&self, caller: ProductSurfaceCaller) -> ConfigSyncContext {
        ConfigSyncContext {
            caller,
            data_folder: self.data_folder.clone(),
            home_dir: self.home_dir.clone(),
            page: items::PageRequest::default(),
            merge_back: false,
            peer_environment_id: None,
        }
    }

    async fn environment_id(&self) -> Result<String, ConfigSyncError> {
        let env = self
            .environment_access
            .environment()
            .await
            .map_err(|error| ConfigSyncError::Unavailable {
                reason: error.to_string(),
            })?;
        Ok(env.environment_id.as_str().to_string())
    }

    /// Compute the per-domain manifest for every registered domain.
    pub async fn manifest(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<ConfigSyncManifest, ConfigSyncError> {
        let environment_id = self.environment_id().await?;
        let ctx = self.context(caller);
        let mut domains = BTreeMap::new();
        for (domain, handler) in &self.handlers {
            // A domain that cannot be read here is left out, so the others
            // still sync; the carrier only moves domains both sides list.
            let digest = match handler.manifest(&ctx).await {
                Ok(digest) => digest,
                Err(error) => {
                    tracing::debug!(domain = domain.as_str(), %error, "config sync: domain left out of the manifest");
                    continue;
                }
            };
            tracing::debug!(
                domain = domain.as_str(),
                hash = %digest.hash,
                item_count = digest.item_count,
                "config sync manifest computed"
            );
            domains.insert(*domain, digest);
        }
        Ok(ConfigSyncManifest {
            schema_version: CONFIG_SYNC_SCHEMA_VERSION,
            environment_id,
            os: std::env::consts::OS.to_string(),
            domains,
        })
    }

    /// Export the requested domains as a [`ConfigSnapshotV1`]. An empty
    /// `domains` means every registered domain that is not itemized; an
    /// itemized one is asked for by name, alone, one page at a time.
    pub async fn export(
        &self,
        caller: ProductSurfaceCaller,
        request: ConfigSyncExportRequest,
    ) -> Result<ConfigSnapshotV1, ConfigSyncError> {
        let environment_id = self.environment_id().await?;
        let mut ctx = self.context(caller);
        ctx.page = items::PageRequest {
            index_only: request.index_only,
            have: request.have,
            after: request.after,
            until: request.until,
        };
        let selected: BTreeSet<ConfigSyncDomain> = if request.domains.is_empty() {
            self.handlers
                .keys()
                .copied()
                .filter(|domain| !domain.itemized())
                .collect()
        } else {
            request.domains.into_iter().collect()
        };

        let mut domains = BTreeMap::new();
        for domain in selected {
            if let Some(handler) = self.handlers.get(&domain) {
                let payload = handler.export(&ctx).await?;
                tracing::debug!(
                    domain = domain.as_str(),
                    hash = %payload.hash(),
                    "config sync domain exported"
                );
                domains.insert(domain, payload);
            }
        }

        Ok(ConfigSnapshotV1 {
            schema_version: CONFIG_SYNC_SCHEMA_VERSION,
            source_environment_id: environment_id,
            source_os: std::env::consts::OS.to_string(),
            generated_at: Utc::now(),
            domains,
        })
    }

    /// Validate the entire apply request before mutating any domain, then apply
    /// each selected domain atomically in canonical order.
    pub async fn apply(
        &self,
        caller: ProductSurfaceCaller,
        request: ConfigSyncApplyRequest,
    ) -> Result<ConfigSyncApplyReport, ConfigSyncError> {
        // Pre-write validation across the whole snapshot.
        self.validate_apply_request(&request)?;

        let target_environment_id = self.environment_id().await?;
        let mut ctx = self.context(caller);
        ctx.merge_back = request.merge_back;
        ctx.peer_environment_id = Some(request.snapshot.source_environment_id.clone());
        let selected: Vec<ConfigSyncDomain> = if request.domains.is_empty() {
            request.snapshot.domains.keys().copied().collect()
        } else {
            let mut set = BTreeSet::new();
            for domain in &request.domains {
                set.insert(*domain);
            }
            set.into_iter().collect()
        };

        let mut outcomes = BTreeMap::new();
        let mut restart_set = BTreeSet::new();

        for domain in selected {
            let Some(payload) = request.snapshot.domains.get(&domain) else {
                outcomes.insert(
                    domain,
                    DomainApplyOutcome {
                        status: DomainApplyStatus::SkippedWithReason,
                        reason: Some(format!(
                            "domain `{domain}` was requested but absent from snapshot"
                        )),
                        applied_count: 0,
                        skipped_count: 0,
                        warnings: Vec::new(),
                    },
                );
                continue;
            };
            let Some(handler) = self.handlers.get(&domain) else {
                outcomes.insert(
                    domain,
                    DomainApplyOutcome {
                        status: DomainApplyStatus::SkippedWithReason,
                        reason: Some(format!("domain `{domain}` is not enabled on this backend")),
                        applied_count: 0,
                        skipped_count: 0,
                        warnings: Vec::new(),
                    },
                );
                continue;
            };

            // The snapshot was validated as a whole above; a domain whose
            // store fails now is reported and the others still apply.
            let result = match handler.apply(&ctx, payload).await {
                Ok(result) => result,
                Err(error @ ConfigSyncError::Validation { .. }) => return Err(error),
                Err(error) => {
                    tracing::debug!(domain = domain.as_str(), %error, "config sync: domain not applied");
                    DomainApplyResult::skipped_with_reason(error.to_string(), 0)
                }
            };
            tracing::debug!(
                domain = domain.as_str(),
                status = result.outcome.status.as_str(),
                applied = result.outcome.applied_count,
                skipped = result.outcome.skipped_count,
                restart_required = ?result.restart_required,
                "config sync domain applied"
            );
            for section in result.restart_required {
                restart_set.insert(section);
            }
            outcomes.insert(domain, result.outcome);
        }

        Ok(ConfigSyncApplyReport {
            schema_version: CONFIG_SYNC_SCHEMA_VERSION,
            target_environment_id,
            domains: outcomes,
            restart_required: restart_set.into_iter().collect(),
        })
    }

    /// Validate schema version, serialized size, domain-key/payload matching,
    /// and every domain handler's payload constraints without writing anything.
    pub fn validate_apply_request(
        &self,
        request: &ConfigSyncApplyRequest,
    ) -> Result<(), ConfigSyncError> {
        if request.snapshot.schema_version != CONFIG_SYNC_SCHEMA_VERSION {
            return Err(ConfigSyncError::validation(
                "schema_version",
                format!(
                    "unsupported schema_version {}; expected {}",
                    request.snapshot.schema_version, CONFIG_SYNC_SCHEMA_VERSION
                ),
            ));
        }

        let encoded = serde_json::to_vec(&request.snapshot).map_err(|error| {
            ConfigSyncError::validation(
                "snapshot",
                format!("could not serialize snapshot: {error}"),
            )
        })?;
        if encoded.len() > CONFIG_SYNC_MAX_SNAPSHOT_BYTES {
            return Err(ConfigSyncError::validation(
                "snapshot",
                format!(
                    "snapshot size {} bytes exceeds maximum {} bytes",
                    encoded.len(),
                    CONFIG_SYNC_MAX_SNAPSHOT_BYTES
                ),
            ));
        }

        for (domain, payload) in &request.snapshot.domains {
            if payload.domain() != *domain {
                return Err(ConfigSyncError::validation(
                    "domains",
                    format!(
                        "domain `{domain}` carries mismatched payload kind `{}`",
                        payload.domain()
                    ),
                ));
            }
            if let Some(handler) = self.handlers.get(domain) {
                handler.validate(payload)?;
            }
        }

        Ok(())
    }
}

/// `ProductSurface` decorator exposing the three config-sync operations
/// (`operator.config_sync.manifest`, `operator.config_sync.export`,
/// `operator.config_sync.apply`). All other calls pass through to `inner`.
pub struct ConfigSyncSurface {
    inner: Arc<dyn ProductSurface>,
    service: Arc<ConfigSyncService>,
}

impl ConfigSyncSurface {
    pub fn new(inner: Arc<dyn ProductSurface>, service: Arc<ConfigSyncService>) -> Self {
        Self { inner, service }
    }
}

#[async_trait]
impl ProductSurface for ConfigSyncSurface {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        if request.operation_id.as_str() != CONFIG_SYNC_APPLY_COMMAND_ID {
            return self.inner.invoke(caller, request).await;
        }
        let parsed = match parse_raw_apply_request(request.input) {
            Ok(req) => req,
            Err((field, reason)) => {
                let output =
                    serde_json::to_value(ConfigSyncApplyOutcome::Rejected { field, reason })
                        .map_err(ProductSurfaceError::internal_from)?;
                return Ok(ProductSurfaceInvokeResponse { output });
            }
        };
        let outcome = match self.service.apply(caller, parsed).await {
            Ok(report) => ConfigSyncApplyOutcome::Completed(report),
            Err(ConfigSyncError::Validation { field, reason }) => {
                ConfigSyncApplyOutcome::Rejected { field, reason }
            }
            Err(error) => return Err(map_sync_error(error)),
        };
        Ok(ProductSurfaceInvokeResponse {
            output: serde_json::to_value(outcome).map_err(ProductSurfaceError::internal_from)?,
        })
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        if request.view_id == CONFIG_SYNC_MANIFEST_VIEW_ID {
            let manifest = self
                .service
                .manifest(caller)
                .await
                .map_err(map_sync_error)?;
            let item =
                serde_json::to_value(manifest).map_err(ProductSurfaceError::internal_from)?;
            return Ok(ProductSurfaceQueryPage {
                items: vec![item],
                next_cursor: None,
            });
        }
        if request.view_id == CONFIG_SYNC_EXPORT_VIEW_ID {
            let parsed = match parse_raw_export_request(request.input) {
                Ok(req) => req,
                Err((field, reason)) => {
                    let item =
                        serde_json::to_value(ConfigSyncExportOutcome::Rejected { field, reason })
                            .map_err(ProductSurfaceError::internal_from)?;
                    return Ok(ProductSurfaceQueryPage {
                        items: vec![item],
                        next_cursor: None,
                    });
                }
            };
            let snapshot = self
                .service
                .export(caller, parsed)
                .await
                .map_err(map_sync_error)?;
            let item = serde_json::to_value(ConfigSyncExportOutcome::Exported(snapshot))
                .map_err(ProductSurfaceError::internal_from)?;
            return Ok(ProductSurfaceQueryPage {
                items: vec![item],
                next_cursor: None,
            });
        }
        self.inner.query(caller, request).await
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        self.inner.stream_events(caller, request).await
    }
}

fn map_sync_error(error: ConfigSyncError) -> ProductSurfaceError {
    match error {
        ConfigSyncError::Unavailable { reason } => {
            tracing::debug!(%reason, "config sync unavailable");
            ProductSurfaceError::service_unavailable(true)
        }
        other => ProductSurfaceError::internal_from(other),
    }
}

/// Inspect raw export JSON so unknown domain names return a structured,
/// user-visible rejection instead of a generic serde failure.
fn parse_raw_export_request(raw: Value) -> Result<ConfigSyncExportRequest, (String, String)> {
    let Some(obj) = raw.as_object() else {
        return Err((
            "input".to_string(),
            "request body must be a JSON object".to_string(),
        ));
    };
    for key in obj.keys() {
        if !matches!(
            key.as_str(),
            "domains" | "index_only" | "have" | "after" | "until"
        ) {
            return Err((
                key.clone(),
                format!("unknown field `{key}` in export request"),
            ));
        }
    }
    // A page belongs to one itemized domain.
    let paged = ["index_only", "have", "after", "until"].iter().any(|key| {
        obj.get(*key)
            .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
    });
    let names: Vec<&str> = obj
        .get("domains")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let itemized = names
        .iter()
        .filter(|name| ConfigSyncDomain::from_wire(name).is_some_and(ConfigSyncDomain::itemized))
        .count();
    if (paged || itemized > 0) && (names.len() != 1 || itemized != 1) {
        return Err((
            "domains".to_string(),
            "an itemized domain is exported alone, one page at a time".to_string(),
        ));
    }
    if let Some(domains_val) = obj.get("domains") {
        let Some(arr) = domains_val.as_array() else {
            return Err((
                "domains".to_string(),
                "`domains` must be an array of domain names".to_string(),
            ));
        };
        for item in arr {
            let Some(name) = item.as_str() else {
                return Err((
                    "domains".to_string(),
                    "each entry in `domains` must be a string".to_string(),
                ));
            };
            if ConfigSyncDomain::from_wire(name).is_none() {
                return Err((
                    "domains".to_string(),
                    format!("unknown config-sync domain `{name}`"),
                ));
            }
        }
    }
    serde_json::from_value(raw).map_err(|error| ("input".to_string(), error.to_string()))
}

/// Inspect raw apply JSON before typed deserialization so schema version,
/// total size, and unknown domain names produce clear, deterministic reasons.
fn parse_raw_apply_request(raw: Value) -> Result<ConfigSyncApplyRequest, (String, String)> {
    let encoded = serde_json::to_vec(&raw)
        .map_err(|error| ("snapshot".to_string(), format!("invalid JSON: {error}")))?;
    if encoded.len() > CONFIG_SYNC_MAX_SNAPSHOT_BYTES {
        return Err((
            "snapshot".to_string(),
            format!(
                "snapshot size {} bytes exceeds maximum {} bytes",
                encoded.len(),
                CONFIG_SYNC_MAX_SNAPSHOT_BYTES
            ),
        ));
    }

    let Some(obj) = raw.as_object() else {
        return Err((
            "input".to_string(),
            "request body must be a JSON object".to_string(),
        ));
    };
    for key in obj.keys() {
        if !matches!(key.as_str(), "snapshot" | "domains" | "merge_back") {
            return Err((
                key.clone(),
                format!("unknown field `{key}` in apply request"),
            ));
        }
    }

    if let Some(filter_val) = obj.get("domains") {
        let Some(arr) = filter_val.as_array() else {
            return Err((
                "domains".to_string(),
                "`domains` must be an array of domain names".to_string(),
            ));
        };
        for item in arr {
            let Some(name) = item.as_str() else {
                return Err((
                    "domains".to_string(),
                    "each entry in `domains` must be a string".to_string(),
                ));
            };
            if ConfigSyncDomain::from_wire(name).is_none() {
                return Err((
                    "domains".to_string(),
                    format!("unknown config-sync domain `{name}`"),
                ));
            }
        }
    }

    let Some(snapshot_obj) = obj.get("snapshot").and_then(Value::as_object) else {
        return Err((
            "snapshot".to_string(),
            "missing or invalid `snapshot` object".to_string(),
        ));
    };

    match snapshot_obj.get("schema_version").and_then(Value::as_u64) {
        Some(v) if v == u64::from(CONFIG_SYNC_SCHEMA_VERSION) => {}
        Some(v) => {
            return Err((
                "schema_version".to_string(),
                format!("unsupported schema_version {v}; expected {CONFIG_SYNC_SCHEMA_VERSION}"),
            ));
        }
        None => {
            return Err((
                "schema_version".to_string(),
                format!("missing or invalid schema_version; expected {CONFIG_SYNC_SCHEMA_VERSION}"),
            ));
        }
    }

    if let Some(domains_val) = snapshot_obj.get("domains") {
        let Some(domains_obj) = domains_val.as_object() else {
            return Err((
                "domains".to_string(),
                "`snapshot.domains` must be an object".to_string(),
            ));
        };
        for key in domains_obj.keys() {
            if ConfigSyncDomain::from_wire(key).is_none() {
                return Err((
                    "domains".to_string(),
                    format!("unknown config-sync domain `{key}`"),
                ));
            }
        }
    }

    serde_json::from_value(raw).map_err(|error| ("snapshot".to_string(), error.to_string()))
}

/// Recursively canonicalize JSON object key order and compute a `sha256:<hex>`
/// digest that is identical across platforms and database backends.
pub fn canonical_json_sha256(
    domain: ConfigSyncDomain,
    value: &Value,
) -> Result<String, ConfigSyncError> {
    let canonical = canonicalize_json(value);
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|source| ConfigSyncError::Serialization { domain, source })?;
    let digest = Sha256::digest(&bytes);
    Ok(format!("sha256:{}", hex_lower(&digest)))
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            let mut out = Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k.clone(), canonicalize_json(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Recursively rewrite all string values inside a JSON value using `rewrite`.
pub(crate) fn rewrite_json_strings(value: &Value, rewrite: &HomePathRewrite) -> Value {
    if rewrite.is_identity() {
        return value.clone();
    }
    match value {
        Value::String(s) => Value::String(rewrite.rewrite_or_keep(s)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| rewrite_json_strings(item, rewrite))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), rewrite_json_strings(v, rewrite));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_hash_is_independent_of_object_key_order() {
        let a = json!({ "b": 2, "a": { "z": true, "m": "hello" } });
        let b = json!({ "a": { "m": "hello", "z": true }, "b": 2 });
        let hash_a = canonical_json_sha256(ConfigSyncDomain::Config, &a).expect("hash a");
        let hash_b = canonical_json_sha256(ConfigSyncDomain::Config, &b).expect("hash b");
        assert_eq!(hash_a, hash_b);
        assert!(hash_a.starts_with("sha256:"));
        assert_eq!(hash_a.len(), "sha256:".len() + 64);
    }

    #[test]
    fn raw_apply_parser_rejects_bad_version_unknown_domain_and_oversized_payload() {
        let bad_ver = json!({
            "snapshot": {
                "schema_version": 99,
                "source_environment_id": "env-1",
                "source_os": "macos",
                "generated_at": "2026-11-26T12:00:00Z",
                "domains": {}
            }
        });
        let (field, reason) = parse_raw_apply_request(bad_ver).expect_err("bad version");
        assert_eq!(field, "schema_version");
        assert!(reason.contains("99"));

        let unknown_domain = json!({
            "snapshot": {
                "schema_version": 1,
                "source_environment_id": "env-1",
                "source_os": "macos",
                "generated_at": "2026-11-26T12:00:00Z",
                "domains": {
                    "not_a_real_domain": { "hash": "sha256:abc" }
                }
            }
        });
        let (field, reason) = parse_raw_apply_request(unknown_domain).expect_err("unknown domain");
        assert_eq!(field, "domains");
        assert!(reason.contains("not_a_real_domain"));

        let oversized = json!({
            "snapshot": {
                "schema_version": 1,
                "source_environment_id": "env-1",
                "source_os": "macos",
                "generated_at": "2026-11-26T12:00:00Z",
                "domains": {
                    "prompts": {
                        "kind": "prompts",
                        "hash": "sha256:0",
                        "system_md": "x".repeat(CONFIG_SYNC_MAX_SNAPSHOT_BYTES + 128)
                    }
                }
            }
        });
        let (field, reason) = parse_raw_apply_request(oversized).expect_err("oversized");
        assert_eq!(field, "snapshot");
        assert!(reason.contains("exceeds maximum"));
    }
}
