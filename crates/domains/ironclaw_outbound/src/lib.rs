//! Outbound egress policy and delivery-attempt storage.
//!
//! This crate stores metadata-only Reborn outbound state: delivery attempts
//! and their status, reply publications, communication preferences and
//! delivered-gate routes. It never owns transport delivery, transcript content, projection
//! payloads, prompts, tool I/O, secrets, host paths, or backend detail strings.

mod communication_preferences;
mod delivered_gate_routes;
mod delivery_resolution;
mod delivery_targets;
mod error;
mod ids;
mod model_channel_delivery;
mod outbound_state_store;
mod reply_attachment_intents;
mod reply_publication;
mod resolution_engine;
mod service;
mod store;
mod triggered_run_delivery;
mod types;
mod validation;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use communication_preferences::{
    CommunicationPreferenceKey, CommunicationPreferenceRecord, CommunicationPreferenceRepository,
    CommunicationPreferenceVersion, DeliveryDefaultScope, NOTIFICATION_TARGETS_CAP,
    VersionedCommunicationPreferenceRecord, WriteCommunicationPreferenceRequest,
};
pub use delivered_gate_routes::{
    DELIVERED_GATE_ROUTE_TTL, DeliveredGateRouteRecord, DeliveredGateRouteStore,
    NoopDeliveredGateRouteStore,
};
pub use delivery_resolution::{
    CommunicationDeliveryCandidate, CommunicationDeliveryIntent, CommunicationDeliveryResolution,
    CommunicationDeliveryResolutionRequest, CommunicationModality, DeliveryTargetCapabilities,
    RequestedOutboundContext, RequestedOutboundKind, RunNotificationContext,
    RunNotificationEventKind, RunNotificationOrigin, SourceRouteContext, SystemEventReasonCode,
};
pub use delivery_targets::{
    MutableOutboundDeliveryTargetRegistry, OutboundDeliveryTargetChannel,
    OutboundDeliveryTargetDescription, OutboundDeliveryTargetDisplayName,
    OutboundDeliveryTargetEntry, OutboundDeliveryTargetId, OutboundDeliveryTargetOwner,
    OutboundDeliveryTargetProvider, OutboundDeliveryTargetRegistrationOutcome,
    OutboundDeliveryTargetRegistry, OutboundDeliveryTargetScope, OutboundDeliveryTargetSummary,
};
pub use error::OutboundError;
pub use ids::{OutboundDeliveryId, ProjectionUpdateRef};
pub use model_channel_delivery::{
    MODEL_DELIVERY_MAX_CONTENT_BYTES, MODEL_DELIVERY_PER_RUN_CAP, ModelChannelDelivery,
    ModelChannelDeliveryError, ModelChannelDeliveryEvidence, ModelChannelDeliveryRequest,
};
pub use outbound_state_store::OutboundStateStore;
pub use reply_attachment_intents::{
    ReplyAttachmentHandle, ReplyAttachmentIntent, ReplyAttachmentIntentPort,
};
pub use reply_publication::{
    AdvanceReplyPublicationRequest, ClaimReplyPublicationLeaseRequest, OpenReplyPublicationRequest,
    PublisherId, REPLY_PUBLICATION_IDENTIFIER_MAX_BYTES, ReleaseReplyPublicationLeaseRequest,
    ReplyPublicationClaim, ReplyPublicationEvidence, ReplyPublicationIdentifierError,
    ReplyPublicationLease, ReplyPublicationRecord, ReplyPublicationSettlement,
    ReplyPublicationState, ReplyPublicationStatus, ReplyPublicationTarget,
    ReplyPublicationTargetDescriptor, ReplyPublicationTargetKey, SettleReplyPublicationRequest,
};
pub use service::{OutboundPolicyService, ReplyTargetBindingValidator};
pub use store::OutboundStateStorePort;
pub use triggered_run_delivery::{
    TriggeredFireFailureDeliveryRequest, TriggeredRunDelivery, TriggeredRunDeliveryOutcomeKind,
    TriggeredRunDeliveryRecord, TriggeredRunDeliveryRequest, TriggeredRunDeliveryStore,
};
pub use types::{
    ClaimDeliveryAttemptForSendRequest, DeliveryFailureKind, OutboundDeliveryAttempt,
    OutboundDeliveryDecision, OutboundDeliveryStatus, OutboundPushCandidate, OutboundPushKind,
    PrepareCommunicationDeliveryRequest, PrepareOutboundDeliveryRequest,
    RecoverInterruptedDeliveryRequest, ReplyTargetBindingClaim, ReplyTargetValidationRequest,
    UpdateDeliveryStatusRequest, ValidatedReplyTargetBinding,
};
