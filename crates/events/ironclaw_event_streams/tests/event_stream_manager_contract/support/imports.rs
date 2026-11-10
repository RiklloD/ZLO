use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_event_projections::{
    CapabilityActivityProjection, CapabilityActivityStatus, EventProjectionService,
    ProjectionCursor, ProjectionError, ProjectionReplay, ProjectionRequest, ProjectionScope,
    ProjectionSnapshot, RunProjectionStatus, RunStatusProjection, ThreadTimeline, TimelineEntry,
    TimelineEntryKind,
};
use ironclaw_event_streams::{
    AllowAllProjectionAccessPolicy, EventStreamManager, InMemoryProjectionStreamAdmissionPolicy,
    InMemoryProjectionUpdateSource, LagReason, NoExposureProjectionRedactionValidator,
    ProductProjectionEnvelope, ProjectionAccessPolicy, ProjectionAccessRequest,
    ProjectionFetchRequest, ProjectionLiveUpdateRequest, ProjectionRedactionValidator,
    ProjectionStreamAdmissionPolicy, ProjectionStreamAdmissionRequest, ProjectionStreamError,
    ProjectionStreamItem, ProjectionStreamLimits, ProjectionSubscribeRequest, ProjectionTarget,
    ProjectionUpdateSource, ProjectionViewClass,
    SubscriberCapabilities, ThreadLiveProjectionItem, ThreadLiveProjectionUpdate, keep_alive_item,
};
use ironclaw_event_log::{EventCursor, EventStreamKey, ReadScope};
use ironclaw_host_api::{ids::{CapabilityId, ExtensionId, InvocationId, MissionId, ProjectId, TenantId, ThreadId, UserId}, runtime::RuntimeKind};
use ironclaw_host_api::turn::{TurnActor, TurnRunId};
use tokio::{
    sync::Barrier,
    time::{Duration, timeout},
};
