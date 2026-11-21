//! Wiring for several machines working together: the receiving side of peer
//! links, agent requests (secrets, machines to link), and where answered
//! secrets are kept. Behavior lives in `ironclaw_orchestrator::peer` and
//! `ironclaw_orchestrator::agent_requests`; the HTTP peer client in
//! `ironclaw_webui`. This module only assembles them.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ids::SecretHandle;
use ironclaw_orchestrator::agent_requests::SecretVault;
use ironclaw_orchestrator::model::ThreadScope as OrchestratorScope;
use ironclaw_orchestrator::peer::PeerReceiver;
use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
use ironclaw_product_contracts::peer::{
    AgentRequestService, PeerLinkRecords, PeerLinkService, PeerToolHandler, SecretReferenceReader,
};
use ironclaw_secrets::reference::{OnePasswordReferenceResolver, SecretReferenceResolver};
use secrecy::SecretString;

use crate::runtime::RebornRuntime;

/// What the transport serves once multi-machine work is installed.
#[derive(Clone, Default)]
pub struct MultiMachineParts {
    /// Runs linked environments' tool calls here (`None` without an engine).
    pub receiver: Option<Arc<dyn PeerToolHandler>>,
    /// Secret and link requests agents raised (`None` without an engine).
    pub requests: Option<Arc<dyn AgentRequestService>>,
}

/// [`SecretVault`] over the encrypted operator secret store.
struct OperatorVault {
    store: Arc<dyn OperatorSecretValueStore>,
    reader: Arc<dyn SecretReferenceReader>,
    files_dir: PathBuf,
}

fn handle(raw: &str) -> Result<SecretHandle, String> {
    SecretHandle::new(raw).map_err(|e| format!("invalid secret handle: {e}"))
}

#[async_trait]
impl SecretVault for OperatorVault {
    async fn put(&self, name: &str, value: SecretString) -> Result<(), String> {
        self.store
            .put(&handle(name)?, value)
            .await
            .map_err(|e| e.stable_reason().to_string())
    }

    async fn take(&self, name: &str) -> Result<Option<SecretString>, String> {
        let handle = handle(name)?;
        let value = self
            .store
            .read(&handle)
            .await
            .map_err(|e| e.stable_reason().to_string())?;
        if value.is_some() {
            self.store
                .delete(&handle)
                .await
                .map_err(|e| e.stable_reason().to_string())?;
        }
        Ok(value)
    }

    fn reference_reader(&self) -> Option<Arc<dyn SecretReferenceReader>> {
        Some(Arc::clone(&self.reader))
    }

    fn files_dir(&self) -> PathBuf {
        self.files_dir.clone()
    }
}

/// [`SecretReferenceReader`] over the 1Password resolver.
struct OnePasswordReader(OnePasswordReferenceResolver);

#[async_trait]
impl SecretReferenceReader for OnePasswordReader {
    fn is_reference(&self, raw: &str) -> bool {
        self.0.is_reference(raw)
    }

    async fn read(&self, reference: &str) -> Result<SecretString, String> {
        self.0.resolve(reference).await
    }
}

impl RebornRuntime {
    /// The peer links this installation holds (records only; tokens live in
    /// the operator secret store).
    pub fn peer_link_records(&self) -> Arc<dyn PeerLinkRecords> {
        Arc::new(self.environment_access_store())
    }

    /// The encrypted operator secret store (system scope).
    pub fn operator_secret_values(&self) -> Arc<dyn OperatorSecretValueStore> {
        crate::RuntimeOperatorSecretValueStore::shared(self.secret_store())
    }

    /// Install multi-machine work: `links` reaches the other environments,
    /// `secret_files` is the owner-only folder for `delivery: file` secrets.
    /// Without an orchestrator engine (libSQL off) nothing is installed.
    pub fn install_multi_machine(
        &self,
        links: Arc<dyn PeerLinkService>,
        secret_files: PathBuf,
    ) -> MultiMachineParts {
        let Some(engine) = self.orchestrator.as_ref() else {
            return MultiMachineParts::default();
        };
        engine.set_peer_links(links);
        engine.set_secret_vault(Arc::new(OperatorVault {
            store: self.operator_secret_values(),
            reader: Arc::new(OnePasswordReader(OnePasswordReferenceResolver::default())),
            files_dir: secret_files,
        }));
        let scope = OrchestratorScope {
            tenant_id: self.thread_scope.tenant_id.clone(),
            user_id: self.actor_user_id.clone(),
            agent_id: Some(self.thread_scope.agent_id.clone()),
            project_id: self.thread_scope.project_id.clone(),
        };
        MultiMachineParts {
            receiver: Some(Arc::new(PeerReceiver::new(Arc::clone(engine), scope))),
            requests: Some(Arc::clone(engine) as Arc<dyn AgentRequestService>),
        }
    }
}
