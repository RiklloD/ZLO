mod account_policy;
mod credential;
mod handlers;
mod manifest;
mod network;

pub use account_policy::gsuite_google_account_visible_to_requester;
pub use credential::{
    GoogleCredential, GoogleCredentialError, GoogleCredentialResolver, google_provider_id,
};
pub use handlers::{
    CALENDAR_ADD_ATTENDEES_CAPABILITY_ID, CALENDAR_CREATE_EVENT_CAPABILITY_ID,
    CALENDAR_DELETE_EVENT_CAPABILITY_ID, CALENDAR_FIND_FREE_SLOTS_CAPABILITY_ID,
    CALENDAR_GET_EVENT_CAPABILITY_ID, CALENDAR_LIST_CALENDARS_CAPABILITY_ID,
    CALENDAR_LIST_EVENTS_CAPABILITY_ID, CALENDAR_SET_REMINDER_CAPABILITY_ID,
    CALENDAR_UPDATE_EVENT_CAPABILITY_ID, GMAIL_CREATE_DRAFT_CAPABILITY_ID,
    GMAIL_GET_MESSAGE_CAPABILITY_ID, GMAIL_LIST_MESSAGES_CAPABILITY_ID,
    GMAIL_REPLY_TO_MESSAGE_CAPABILITY_ID, GMAIL_SEND_MESSAGE_CAPABILITY_ID,
    GMAIL_TRASH_MESSAGE_CAPABILITY_ID, GsuiteCredentialDispatchReason, GsuiteCredentialStageError,
    GsuiteCredentialStageRequest, GsuiteCredentialStager, GsuiteDispatchError,
    GsuiteDispatchRequest, GsuiteDispatchResult, GsuiteExecutor,
};
/// The Google credential-authority provider id, re-exported so the assembling
/// binary can build the GSuite runtime-credential requirements and the
/// Google-account visibility policy without depending on `ironclaw_auth`
/// directly (extension-runtime DEL-7).
pub use ironclaw_auth::GOOGLE_PROVIDER_ID;
pub use manifest::{
    GSUITE_OUTPUT_BYTES_LIMIT, GSUITE_PROVIDER_SCOPES, GSUITE_REQUEST_BODY_LIMIT,
    GSUITE_RESPONSE_BODY_LIMIT, GsuiteCapabilityOperation, GsuiteCapabilitySpec, GsuitePackageSpec,
    find_gsuite_capability, gsuite_package_specs, gsuite_resource_profile, is_gsuite_extension_id,
};
pub use network::{google_api_network_policy, gsuite_network_policy_for};
