pub(crate) use chrono::{Duration, Utc};
pub(crate) use ironclaw_auth::{
    AuthChallenge, AuthContinuationRef, AuthErrorCode, AuthFlowKind, AuthFlowManager,
    AuthFlowStatus, AuthGateRef, AuthInteractionService, AuthProductError, AuthProductScope,
    AuthProviderClient, AuthProviderId, AuthSessionId, AuthSurface, AuthorizationCodeHash,
    CredentialAccount, CredentialAccountChoiceRequest, CredentialAccountId, CredentialAccountLabel,
    CredentialAccountListRequest, CredentialAccountLookupRequest, CredentialAccountMutation,
    CredentialAccountProjection, CredentialAccountSelectionRequest, CredentialAccountService,
    CredentialAccountStatus, CredentialAccountUpdate, CredentialAccountUpdateBinding,
    CredentialOwnership, CredentialRecoveryKind, CredentialRecoveryProjection,
    CredentialRecoveryReason, CredentialRecoveryRequest, CredentialRefreshRequest,
    CredentialSelectionInput, CredentialSetupService, InMemoryAuthProductServices,
    LifecyclePackageRef, ManualTokenSetupRequest, NewAuthFlow, NewCredentialAccount,
    OAuthAuthorizationCode, OAuthAuthorizationUrl, OAuthCallbackInput,
    OAuthProviderCallbackRequest, OAuthProviderExchange, OAuthProviderExchangeContext,
    OAuthProviderRefreshRequest, OpaqueStateHash, PkceVerifierHash, PkceVerifierSecret,
    ProviderCallbackOutcome, ProviderScope, SecretCleanupAction, SecretCleanupQuarantineReason,
    SecretCleanupRequest, SecretCleanupService, SecretSubmitRequest, SecretSubmitResult,
    TurnRunRef,
};
pub(crate) use ironclaw_host_api::{
    ids::{ExtensionId, InvocationId, SecretHandle, ThreadId, UserId},
    resource::ResourceScope,
};
pub(crate) use secrecy::SecretString;

pub(crate) fn scope(user: &str) -> AuthProductScope {
    AuthProductScope::new(
        ResourceScope::local_default(UserId::new(user).expect("valid user"), InvocationId::new())
            .expect("valid scope"),
        AuthSurface::Web,
    )
    .with_session_id(AuthSessionId::new(format!("session-{user}")).expect("valid session"))
}

/// A scope for the same owner/session/surface as [`scope`] but carrying a
/// distinct `thread_id` and a fresh `invocation_id`. Models an OAuth /
/// manual-token reconnect that arrives from a different chat thread (and a new
/// per-flow invocation) than the one the bound account was created in — the
/// #4935 defect-A shape. At durable owner granularity this is the same owner,
/// so the reconnect must update the existing account; full scope equality would
/// reject it.
pub(crate) fn reconnect_scope(user: &str, thread: &str) -> AuthProductScope {
    let mut scope = scope(user);
    scope.resource.thread_id = Some(ThreadId::new(thread).expect("valid thread"));
    scope
}

pub(crate) fn provider() -> AuthProviderId {
    AuthProviderId::new("github").expect("valid provider")
}

pub(crate) fn label(value: &str) -> CredentialAccountLabel {
    CredentialAccountLabel::new(value).expect("valid label")
}

pub(crate) fn state_hash(value: &str) -> OpaqueStateHash {
    OpaqueStateHash::new(fake_digest(value)).expect("valid state hash")
}

pub(crate) fn pkce_hash(value: &str) -> PkceVerifierHash {
    PkceVerifierHash::new(fake_digest(value)).expect("valid pkce hash")
}

pub(crate) fn code_hash(value: &str) -> AuthorizationCodeHash {
    AuthorizationCodeHash::new(fake_digest(value)).expect("valid code hash")
}

pub(crate) fn fake_digest(value: &str) -> String {
    format!(
        "{:064x}",
        value.bytes().fold(0_u64, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(u64::from(byte))
        })
    )
}

pub(crate) fn authorization_url(value: &str) -> OAuthAuthorizationUrl {
    OAuthAuthorizationUrl::new(value).expect("valid authorization url")
}

pub(crate) fn provider_scope(value: &str) -> ProviderScope {
    ProviderScope::new(value).expect("valid provider scope")
}

pub(crate) fn provider_scopes(values: &[&str]) -> Vec<ProviderScope> {
    values.iter().map(|value| provider_scope(value)).collect()
}

pub(crate) fn secret(value: &str) -> SecretString {
    SecretString::from(value.to_string())
}

pub(crate) fn account_request(
    owner: AuthProductScope,
    label_value: &str,
    status: CredentialAccountStatus,
) -> NewCredentialAccount {
    NewCredentialAccount {
        scope: owner,
        provider: provider(),
        label: label(label_value),
        status,
        ownership: CredentialOwnership::UserReusable,
        owner_extension: None,
        granted_extensions: Vec::new(),
        access_secret: None,
        refresh_secret: None,
        scopes: Vec::new(),
    }
}

/// The sanctioned shape a device link mints: extension-owned by exactly one
/// extension, no grants (PROPOSAL §4.5). `bump_link_revision` refuses anything
/// else, so this is the only fixture that can become a linked device.
pub(crate) fn linked_device_account_request(
    owner: AuthProductScope,
    label_value: &str,
    owner_extension: &ExtensionId,
) -> NewCredentialAccount {
    NewCredentialAccount::for_linked_device(
        owner,
        provider(),
        label(label_value),
        owner_extension.clone(),
        SecretHandle::new("linked-device-session").expect("valid secret handle"),
    )
}

pub(crate) fn account_ids(accounts: &[CredentialAccountProjection]) -> Vec<CredentialAccountId> {
    let mut ids = accounts
        .iter()
        .map(|account| account.id)
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

pub(crate) fn update_binding(account: &CredentialAccount) -> CredentialAccountUpdateBinding {
    CredentialAccountUpdateBinding {
        account_id: account.id,
        ownership: account.ownership,
        owner_extension: account.owner_extension.clone(),
        granted_extensions: account.granted_extensions.clone(),
    }
}

pub(crate) async fn oauth_flow(
    services: &InMemoryAuthProductServices,
    owner: AuthProductScope,
) -> ironclaw_auth::AuthFlowRecord {
    services
        .create_flow(NewAuthFlow {
            requested_scopes: Vec::new(),
            id: None,
            scope: owner,
            kind: AuthFlowKind::IntegrationCredential,
            provider: provider(),
            requester_extension: None,
            challenge: AuthChallenge::OAuthUrl {
                authorization_url: authorization_url("https://provider.example/oauth"),
                expires_at: Utc::now() + Duration::minutes(5),
            },
            continuation: AuthContinuationRef::LifecycleActivation {
                package_ref: LifecyclePackageRef::new("github-extension").expect("valid package"),
            },
            update_binding: None,
            opaque_state_hash: Some(state_hash("state-hash")),
            pkce_verifier_hash: Some(pkce_hash("pkce-hash")),
            expires_at: Utc::now() + Duration::minutes(5),
        })
        .await
        .expect("flow")
}

pub(crate) async fn oauth_update_flow(
    services: &InMemoryAuthProductServices,
    owner: AuthProductScope,
    account: &CredentialAccount,
) -> ironclaw_auth::AuthFlowRecord {
    try_oauth_update_flow(services, owner, account)
        .await
        .expect("update flow")
}

pub(crate) async fn try_oauth_update_flow(
    services: &InMemoryAuthProductServices,
    owner: AuthProductScope,
    account: &CredentialAccount,
) -> Result<ironclaw_auth::AuthFlowRecord, AuthProductError> {
    services
        .create_flow(NewAuthFlow {
            requested_scopes: Vec::new(),
            id: None,
            scope: owner,
            kind: AuthFlowKind::IntegrationCredential,
            provider: provider(),
            requester_extension: None,
            challenge: AuthChallenge::OAuthUrl {
                authorization_url: authorization_url("https://provider.example/oauth"),
                expires_at: Utc::now() + Duration::minutes(5),
            },
            continuation: AuthContinuationRef::LifecycleActivation {
                package_ref: LifecyclePackageRef::new("github-extension").expect("valid package"),
            },
            update_binding: Some(update_binding(account)),
            opaque_state_hash: Some(state_hash("state-hash")),
            pkce_verifier_hash: Some(pkce_hash("pkce-hash")),
            expires_at: Utc::now() + Duration::minutes(5),
        })
        .await
}
