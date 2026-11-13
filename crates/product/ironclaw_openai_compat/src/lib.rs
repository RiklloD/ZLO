#![deny(clippy::let_underscore_must_use)]
#![forbid(unsafe_code)]

//! Reborn-native OpenAI-compatible API contract surface.
//!
//! The crate owns DTOs, route descriptors, and a sanitized error envelope for
//! the OpenAI-compatible Chat Completions and Responses surfaces. It exposes
//! axum route fragments for host composition without routing through the v1
//! gateway. By default the router is
//! fail-closed; host composition can inject ProductSurface-backed Chat,
//! Responses, and projection-streaming services for the wired Reborn slices.

mod ack_helpers;
mod chat;
mod chat_workflow;
mod content_parts;
mod cost;
mod descriptors;
mod embeddings;
mod error;
mod external_tools;
mod handlers;
mod identity;
mod model_validation;
mod models;
mod models_catalog;
mod mount;
mod prepared_turn;
mod projection_helpers;
mod refs;
// Durable filesystem-backed ref store, over the universal `RootFilesystem`
// port. Unconditional: this crate declares no cargo features, and WS5
// collapsed the LibSql/Postgres ref-store newtypes onto that one
// backend-neutral form. (It carried a "gated behind `storage`" note until
// 2026-08-05; there has been no such feature here for some time.)
mod refs_storage;
mod responses;
mod responses_workflow;
mod router;
mod streaming;

pub use chat::{
    OpenAiChatChoice, OpenAiChatCompletionChunk, OpenAiChatCompletionRequest,
    OpenAiChatCompletionResponse, OpenAiChatDelta, OpenAiChatFinishReason, OpenAiChatFunction,
    OpenAiChatMessage, OpenAiChatMessageRole, OpenAiChatStreamChoice, OpenAiChatTool,
    OpenAiChatToolCall, OpenAiChatToolCallDelta, OpenAiChatToolCallFunction,
    OpenAiChatToolCallFunctionDelta, OpenAiChatToolKind, OpenAiPromptTokensDetails, OpenAiUsage,
};
pub use chat_workflow::{
    OPENAI_COMPAT_CONVERSATION_PREFIX, OpenAiChatCompletionProjection,
    OpenAiChatCompletionProjectionReader, OpenAiChatCompletionProjectionRequest,
    OpenAiChatCompletionsWorkflow, OpenAiChatModelOnlyTools, OpenAiCompatAuthenticatedCaller,
};
pub use cost::OpenAiCompatCost;
pub use descriptors::{
    OPENAI_COMPAT_PATTERN_CHAT_COMPLETIONS, OPENAI_COMPAT_PATTERN_EMBEDDINGS_API,
    OPENAI_COMPAT_PATTERN_EMBEDDINGS_V1, OPENAI_COMPAT_PATTERN_MODELS_API_LIST,
    OPENAI_COMPAT_PATTERN_MODELS_LIST, OPENAI_COMPAT_PATTERN_RESPONSES_API_CREATE,
    OPENAI_COMPAT_PATTERN_RESPONSES_API_ITEM, OPENAI_COMPAT_PATTERN_RESPONSES_API_ITEM_CANCEL,
    OPENAI_COMPAT_PATTERN_RESPONSES_V1_CREATE, OPENAI_COMPAT_PATTERN_RESPONSES_V1_ITEM,
    OPENAI_COMPAT_PATTERN_RESPONSES_V1_ITEM_CANCEL, OPENAI_COMPAT_ROUTE_CHAT_COMPLETIONS,
    OPENAI_COMPAT_ROUTE_EMBEDDINGS_API, OPENAI_COMPAT_ROUTE_EMBEDDINGS_V1,
    OPENAI_COMPAT_ROUTE_MODELS_API_LIST, OPENAI_COMPAT_ROUTE_MODELS_LIST,
    OPENAI_COMPAT_ROUTE_RESPONSES_API_CANCEL, OPENAI_COMPAT_ROUTE_RESPONSES_API_CREATE,
    OPENAI_COMPAT_ROUTE_RESPONSES_API_RETRIEVE, OPENAI_COMPAT_ROUTE_RESPONSES_V1_CANCEL,
    OPENAI_COMPAT_ROUTE_RESPONSES_V1_CREATE, OPENAI_COMPAT_ROUTE_RESPONSES_V1_RETRIEVE,
    openai_compat_routes,
};
pub use embeddings::{
    OpenAiCompatEmbeddingBatch, OpenAiCompatEmbeddingModel, OpenAiCompatEmbeddings,
    OpenAiEmbeddingEncodingFormat, OpenAiEmbeddingObject, OpenAiEmbeddingVector,
    OpenAiEmbeddingsInput, OpenAiEmbeddingsRequest, OpenAiEmbeddingsResponse,
    OpenAiEmbeddingsUsage,
};
pub use error::{
    OpenAiCompatError, OpenAiCompatErrorCode, OpenAiCompatErrorKind, OpenAiCompatErrorResponse,
    OpenAiCompatErrorType, OpenAiCompatHttpError,
};
pub use external_tools::{
    OpenAiCompatExternalToolResume, OpenAiCompatExternalToolResumeRequest,
    OpenAiCompatExternalToolSpec, OpenAiCompatExternalToolStore,
};
pub use handlers::{
    chat_completions, embeddings_create, models_list, responses_cancel, responses_create,
    responses_retrieve,
};
pub use identity::{
    OPENAI_COMPAT_ACTOR_KIND, OPENAI_COMPAT_ADAPTER_ID, OPENAI_COMPAT_INSTALLATION_ID,
};
pub use models::{OpenAiModelListResponse, OpenAiModelObject};
pub use models_catalog::{OpenAiCompatModelCatalog, OpenAiCompatModelEntry};
pub use mount::{
    OpenAiCompatRouteMountPorts, openai_compat_route_mount,
    product_surface_caller_from_openai_scope,
};
pub use prepared_turn::{OpenAiCompatPreparedTurnPort, OpenAiCompatPreparedTurnRequest};
pub use refs::{
    OpenAiChatCompletionId, OpenAiCompatActorScope, OpenAiCompatBindInternalRefs,
    OpenAiCompatIdempotencyConflict, OpenAiCompatIdempotencyKey, OpenAiCompatInternalRefs,
    OpenAiCompatMarkExternalToolResumeCompleted, OpenAiCompatProductActionRef,
    OpenAiCompatProjectionRef, OpenAiCompatPublicId, OpenAiCompatRecordAcceptedAck,
    OpenAiCompatRefError, OpenAiCompatRefLookup, OpenAiCompatRefOperation,
    OpenAiCompatRefReservation, OpenAiCompatRefReservationOutcome, OpenAiCompatRefStorePort,
    OpenAiCompatRequestFingerprint, OpenAiCompatResourceBinding, OpenAiCompatResourceKind,
    OpenAiCompatResourceMapping, OpenAiCompatRouteSurface, OpenAiCompatTurnRunRef,
    OpenAiResponseId, unix_timestamp_now,
};
pub use refs_storage::OpenAiCompatRefStore;
pub use responses::{
    OpenAiResponseErrorObject, OpenAiResponseInputTokensDetails, OpenAiResponseObject,
    OpenAiResponseOutputItem, OpenAiResponseOutputItemStatus, OpenAiResponseStatus,
    OpenAiResponseUsage, OpenAiResponsesCreateRequest, OpenAiResponsesInput,
    OpenAiResponsesInputItem, OpenAiResponsesMessageRole,
};
pub use responses_workflow::{
    OpenAiResponseProjection, OpenAiResponseReadRequest, OpenAiResponseWaitRequest,
    OpenAiResponsesProjectionReader, OpenAiResponsesWorkflow,
};
pub use router::{OpenAiCompatRouterState, openai_compat_router, openai_compat_router_with_state};
pub use streaming::{
    OpenAiChatProjectionStreamRequest, OpenAiCompatProjectionStreamer,
    OpenAiResponseProjectionStreamRequest,
};
