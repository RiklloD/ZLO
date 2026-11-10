use async_trait::async_trait;
use ironclaw_filesystem::RecordVersion;

use crate::{InboundTurnError, memory::InMemoryState};

pub(crate) struct PersistedConversationState {
    pub(crate) state: InMemoryState,
    pub(crate) revision: i64,
    /// Filesystem version of the loaded document; `None` when it was absent.
    pub(crate) version: Option<RecordVersion>,
}

#[async_trait]
pub(crate) trait ConversationStateRepository: Send + Sync {
    async fn load_state(&self) -> Result<PersistedConversationState, InboundTurnError>;
    /// Writes `state` only if the document is still the one that was loaded:
    /// `expected_revision` and `expected_version` both come from that load.
    /// Returns the revision and filesystem version of what it wrote.
    async fn save_state(
        &self,
        expected_revision: i64,
        expected_version: Option<RecordVersion>,
        state: &InMemoryState,
    ) -> Result<(i64, RecordVersion), InboundTurnError>;
}
