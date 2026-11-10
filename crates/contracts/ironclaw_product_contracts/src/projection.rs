//! Projection read/subscription contracts.

use async_trait::async_trait;
use ironclaw_host_api::turn::{TurnActor, TurnScope};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::outbound::{ProductOutboundEnvelope, ProjectionCursor};
use ironclaw_host_api::product_adapter_error::ProductAdapterError;
use ironclaw_host_api::product_adapter_error::RedactedString;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectionReadRequest {
    pub actor: TurnActor,
    pub scope: TurnScope,
    pub after_cursor: Option<ProjectionCursor>,
    pub limit: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectionSubscriptionRequest {
    pub actor: TurnActor,
    pub scope: TurnScope,
    pub after_cursor: Option<ProjectionCursor>,
}

pub struct ProjectionStreamSubscription {
    receiver: mpsc::Receiver<Result<ProductOutboundEnvelope, ProductAdapterError>>,
}

impl ProjectionStreamSubscription {
    pub fn new(
        receiver: mpsc::Receiver<Result<ProductOutboundEnvelope, ProductAdapterError>>,
    ) -> Self {
        Self { receiver }
    }

    pub async fn next(&mut self) -> Option<Result<ProductOutboundEnvelope, ProductAdapterError>> {
        self.receiver.recv().await
    }
}

#[async_trait]
pub trait ProjectionStream: Send + Sync {
    async fn drain(
        &self,
        request: ProjectionSubscriptionRequest,
    ) -> Result<Vec<ProductOutboundEnvelope>, ProductAdapterError>;

    fn supports_subscription(&self) -> bool {
        false
    }

    async fn subscribe(
        &self,
        _request: ProjectionSubscriptionRequest,
    ) -> Result<ProjectionStreamSubscription, ProductAdapterError> {
        Err(ProductAdapterError::Internal {
            detail: RedactedString::new(
                "projection subscription is not supported by this ProjectionStream implementation",
            ),
        })
    }
}
