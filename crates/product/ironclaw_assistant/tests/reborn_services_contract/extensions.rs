//! Extension listing, setup, import, hosted-MCP registration, and the IronHub link.

use super::*;

#[derive(Default)]
struct RecordingExtensionCredentialSetupService {
    status_requests: Mutex<Vec<ExtensionCredentialStatusRequest>>,
    submit_requests: Mutex<Vec<ExtensionCredentialSubmitRequest>>,
}

impl RecordingExtensionCredentialSetupService {
    fn status_count(&self) -> usize {
        self.status_requests.lock().expect("lock").len()
    }

    fn submit_count(&self) -> usize {
        self.submit_requests.lock().expect("lock").len()
    }
}

#[async_trait]
impl ExtensionCredentialSetupService for RecordingExtensionCredentialSetupService {
    async fn credential_status(
        &self,
        request: ExtensionCredentialStatusRequest,
    ) -> Result<Option<CredentialAccountProjection>, ProductSurfaceError> {
        self.status_requests.lock().expect("lock").push(request);
        Ok(None)
    }

    async fn submit_manual_token(
        &self,
        request: ExtensionCredentialSubmitRequest,
    ) -> Result<CredentialAccountId, ProductSurfaceError> {
        self.submit_requests.lock().expect("lock").push(request);
        Ok(CredentialAccountId::new())
    }
}

#[tokio::test]
async fn setup_extension_projects_through_configured_lifecycle_service() {
    let lifecycle_service = Arc::new(RecordingLifecycleService::new());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(lifecycle_service.clone());

    let response = query_extension_setup(&services, caller(), "github")
        .await
        .expect("setup extension response");

    assert_eq!(
        response.package_ref,
        LifecyclePackageRef::new(LifecyclePackageKind::Extension, "github")
            .expect("valid package ref")
    );
    assert_eq!(response.phase, LifecyclePublicState::SetupNeeded);
    assert!(response.blockers.iter().any(|blocker| matches!(
        blocker,
        LifecycleReadinessBlocker::Runtime { ref_id: Some(ref_id) }
            if ref_id.as_str() == "extension_lifecycle_store_unwired"
    )));
    assert_eq!(
        lifecycle_service.package_refs(),
        vec![
            LifecyclePackageRef::new(LifecyclePackageKind::Extension, "github")
                .expect("valid package ref")
        ]
    );
}

#[tokio::test]
async fn extension_setup_is_available_as_product_view() {
    let lifecycle_service = Arc::new(RecordingLifecycleService::new());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(lifecycle_service.clone());

    let response = query_extension_setup(&services, caller(), "github")
        .await
        .expect("extension setup view response");

    assert_eq!(
        response.package_ref,
        LifecyclePackageRef::new(LifecyclePackageKind::Extension, "github")
            .expect("valid package ref")
    );
    assert_eq!(response.phase, LifecyclePublicState::SetupNeeded);
    assert_eq!(
        lifecycle_service.package_refs(),
        vec![
            LifecyclePackageRef::new(LifecyclePackageKind::Extension, "github")
                .expect("valid package ref")
        ]
    );
}

#[tokio::test]
async fn list_extensions_projects_onboarding_payload_through_reborn_services() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(Arc::new(ListingLifecycleService {
        extension: LifecycleInstalledExtensionSummary {
            summary: extension_summary(
                "github",
                vec![manual_credential_requirement("github_runtime_token", true)],
                Some(onboarding_fixture()),
            ),
            phase: InstallationState::Installed,
            install_scope: None,
        },
    }));

    let response = query_extensions(&services, caller())
        .await
        .expect("extension list response");
    let extension = response.extensions.first().expect("one extension");

    assert_eq!(extension.tools, vec!["github.read", "github.write"]);
    assert_eq!(
        extension.installation_state,
        LifecyclePublicState::SetupNeeded
    );
    let onboarding = extension.onboarding.as_ref().expect("onboarding payload");
    assert_eq!(
        onboarding.credential_instructions.as_deref(),
        Some("Paste the GitHub token IronClaw should use.")
    );
    assert_eq!(
        onboarding.credential_next_step.as_deref(),
        Some("After saving the token, activate GitHub to publish its tools.")
    );
}

#[tokio::test]
async fn a_directory_classified_session_channel_is_hidden_from_the_extension_list() {
    // The fail-open arm (no directory wired → everything listed) is covered
    // by every other listing test passing `None`. This pins the PRESENT arm:
    // the deployment's session channel is host infrastructure, not an
    // installable row — a regression that inverts `is_some_and` or drops the
    // filter must fail here while an unrelated extension still survives.
    struct TwoExtensionListingService {
        session_channel: LifecycleInstalledExtensionSummary,
        unrelated: LifecycleInstalledExtensionSummary,
    }

    #[async_trait]
    impl LifecycleProductService for TwoExtensionListingService {
        async fn execute(
            &self,
            _context: LifecycleProductContext,
            action: LifecycleProductAction,
        ) -> Result<LifecycleProductResponse, ProductSurfaceError> {
            assert!(matches!(action, LifecycleProductAction::ExtensionList));
            Ok(LifecycleProductResponse {
                package_ref: None,
                phase: self.session_channel.phase,
                blockers: Vec::new(),
                message: None,
                payload: Some(LifecycleProductPayload::ExtensionList {
                    extensions: vec![self.session_channel.clone(), self.unrelated.clone()],
                    count: 2,
                }),
            })
        }

        async fn project_package(
            &self,
            _context: LifecycleProductContext,
            _package_ref: LifecyclePackageRef,
        ) -> Result<LifecycleProductResponse, ProductSurfaceError> {
            panic!("list_extensions should execute the list action, not project one package")
        }
    }

    let installed = |id: &str| LifecycleInstalledExtensionSummary {
        summary: extension_summary(id, Vec::new(), None),
        phase: InstallationState::Installed,
        install_scope: None,
    };
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(Arc::new(TwoExtensionListingService {
        session_channel: installed("web-app"),
        unrelated: installed("github"),
    }))
    .with_session_channel_directory(Arc::new(StaticSessionChannelDirectory {
        session_channels: vec!["web-app"],
    }));

    let response = query_extensions(&services, caller())
        .await
        .expect("extension list response");
    let listed: Vec<&str> = response
        .extensions
        .iter()
        .map(|extension| extension.package_ref.id.as_str())
        .collect();
    assert_eq!(
        listed,
        vec!["github"],
        "the session channel is hidden; unrelated extensions survive"
    );
}

#[tokio::test]
async fn list_extensions_projects_channel_surface_with_directions_and_connection() {
    // Channel discovery is extension-surface data: an installed extension
    // whose summary declares an inbound+outbound channel surface projects a
    // typed `channel` surface with its connect affordance — there is no
    // separate connectable-channel registry or route.
    let mut summary = extension_summary("slack", Vec::new(), None);
    summary.surface_kinds = vec![CapabilitySurfaceKind::Channel];
    summary.channel_directions = Some(LifecycleChannelDirections {
        inbound: true,
        outbound: true,
    });
    summary.channel_connection = Some(ChannelConnectionRequirement {
        channel: "slack".to_string(),
        display_name: "Slack".to_string(),
        strategy: RebornChannelConnectStrategy::OAuth,
        instructions: "Connect Slack with OAuth.".to_string(),
        input_placeholder: String::new(),
        submit_label: "Connect Slack".to_string(),
        error_message: "Slack OAuth connection failed.".to_string(),
    });
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(Arc::new(ListingLifecycleService {
        extension: LifecycleInstalledExtensionSummary {
            summary,
            phase: InstallationState::Active,
            install_scope: None,
        },
    }))
    // The caller has connected this OAuth channel. Without it the extension is
    // correctly `setup_needed` (that is the unpaired-channel contract), so the
    // connected signal is what this test's `active` assertion depends on.
    .with_channel_connection_service(Arc::new(ConnectedChannelConnectionService {
        connections: std::collections::HashMap::from([(extension_id("slack"), true)]),
    }));

    let response = query_extensions(&services, caller())
        .await
        .expect("extensions response");

    let info = response
        .extensions
        .iter()
        .find(|extension| extension.package_ref.id.as_str() == "slack")
        .expect("channel extension listed");
    let channel = info
        .surfaces
        .iter()
        .find_map(|surface| match surface {
            RebornExtensionSurface::Channel {
                inbound,
                outbound,
                connection,
                ..
            } => Some((inbound, outbound, connection.clone())),
            _ => None,
        })
        .expect("channel surface projected");
    assert!(channel.0, "inbound direction must project");
    assert!(channel.1, "outbound direction must project");
    let connection = channel.2.expect("connect affordance carried");
    assert_eq!(connection.strategy, RebornChannelConnectStrategy::OAuth);
    assert_eq!(connection.submit_label, "Connect Slack");
    // S5 wire gap: the connect affordance now carries the manifest display name
    // so the frontend never derives a label from the channel id.
    assert_eq!(connection.display_name, "Slack");
    // §6.1 installation-state enum replaces the activation_status string.
    assert_eq!(info.installation_state, LifecyclePublicState::Active);
}

/// A caller-scoped channel-connection service that reports a fixed set of
/// connected channels (mirrors the production port shape the composition crate
/// wires; the default `StaticChannelConnectionService` reports none).
struct ConnectedChannelConnectionService {
    connections: std::collections::HashMap<ExtensionId, bool>,
}

#[async_trait]
impl ChannelConnectionService for ConnectedChannelConnectionService {
    async fn caller_channel_connections(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<std::collections::HashMap<ExtensionId, bool>, ProductSurfaceError> {
        Ok(self.connections.clone())
    }
}

/// Golden wire fixture (UI-1 / UI-2 / AUTH-9): an arbitrary channel on a
/// multi-surface extension freezes the exact shape the frontend renders — the
/// §6.1 installation-state enum, the per-vendor accounts list carrying the §6.3
/// auth-account state enum (`account_id` / `label` / `state` / `is_default`),
/// each surface's `resolved_account_id` + binding source, and the connection
/// requirement's `display_name`. Vendor-neutral on purpose: `acme` proves no
/// concrete product is needed. The retired stopgap fields (the
/// `activation_status` string and the `connected` bool) are gone from the wire.
#[tokio::test]
async fn list_extensions_golden_wire_multi_surface_extension_freezes_accounts_list() {
    let mut summary = extension_summary(
        "acme",
        vec![LifecycleExtensionCredentialRequirement {
            name: "acme_oauth_token".to_string(),
            provider: "acme".to_string(),
            required: true,
            setup: LifecycleExtensionCredentialSetup::ManualToken,
        }],
        None,
    );
    summary.surface_kinds = vec![
        CapabilitySurfaceKind::Tool,
        CapabilitySurfaceKind::Channel,
        CapabilitySurfaceKind::Auth,
    ];
    summary.channel_directions = Some(LifecycleChannelDirections {
        inbound: true,
        outbound: true,
    });
    summary.channel_connection = Some(ChannelConnectionRequirement {
        channel: "acme".to_string(),
        display_name: "Acme Messenger".to_string(),
        strategy: RebornChannelConnectStrategy::OAuth,
        instructions: "Connect Acme Messenger with OAuth.".to_string(),
        input_placeholder: String::new(),
        submit_label: "Connect Acme Messenger".to_string(),
        error_message: "Acme Messenger OAuth connection failed.".to_string(),
    });

    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(Arc::new(ListingLifecycleService {
        extension: LifecycleInstalledExtensionSummary {
            summary,
            phase: InstallationState::Active,
            install_scope: None,
        },
    }))
    .with_channel_connection_service(Arc::new(ConnectedChannelConnectionService {
        connections: std::collections::HashMap::from([(extension_id("acme"), true)]),
    }));

    let response = query_extensions(&services, caller())
        .await
        .expect("extensions response");
    let info = response
        .extensions
        .iter()
        .find(|extension| extension.package_ref.id.as_str() == "acme")
        .expect("multi-surface extension listed");

    // §6.1 installation-state enum on the wire (replaces the activation_status string).
    assert_eq!(info.installation_state, LifecyclePublicState::Active);

    // §6.4 / ADR 0001 accounts list — the frozen shape, named field for field.
    // A live grant backfills to `connected` (MIG-1); one account per vendor,
    // is_default (list length ≤ 1, shape only).
    let accounts = serde_json::to_value(&info.auth_accounts).expect("auth_accounts serialize");
    assert_eq!(
        accounts,
        json!([{
            "vendor": "acme",
            "accounts": [{
                "account_id": "acme",
                "label": "acme",
                "state": "connected",
                "is_default": true
            }]
        }]),
        "the per-vendor accounts list freezes the account_id/label/state/is_default shape",
    );

    // Surface keys: tool / channel-with-resolved-account / auth, in declared order.
    let surfaces = serde_json::to_value(&info.surfaces).expect("surfaces serialize");
    assert_eq!(
        surfaces,
        json!([
            { "kind": "tool" },
            {
                "kind": "channel",
                "inbound": true,
                "outbound": true,
                "resolved_account_id": "acme",
                "binding_source": "default",
                "connection": {
                    "channel": "acme",
                    "display_name": "Acme Messenger",
                    "strategy": "oauth",
                    "instructions": "Connect Acme Messenger with OAuth.",
                    "input_placeholder": "",
                    "submit_label": "Connect Acme Messenger",
                    "error_message": "Acme Messenger OAuth connection failed."
                }
            },
            { "kind": "auth" }
        ]),
        "surface keys + the channel surface's resolved account + binding source + display_name are frozen",
    );

    // The retired stopgap fields are gone from the wire.
    let info_json = serde_json::to_value(info).expect("info serialize");
    assert!(
        info_json.get("activation_status").is_none(),
        "the activation_status string stopgap must be gone",
    );
    let channel_json = &surfaces.as_array().expect("surfaces array")[1];
    assert!(
        channel_json.get("connected").is_none(),
        "the connected bool stopgap must be gone from the channel surface",
    );
}

/// A lifecycle service that lists one installed extension in a caller-chosen
/// installation state and reports a redacted per-extension activation error —
/// drives the terminal `Failed` installation-state (§6.1) and `activation_error`
/// projection through the real descriptor-backed `EXTENSIONS_VIEW` seam.
struct FailedStateLifecycleService {
    extension: LifecycleInstalledExtensionSummary,
    activation_errors: std::collections::HashMap<ExtensionId, String>,
}

#[async_trait]
impl LifecycleProductService for FailedStateLifecycleService {
    async fn execute(
        &self,
        _context: LifecycleProductContext,
        action: LifecycleProductAction,
    ) -> Result<LifecycleProductResponse, ProductSurfaceError> {
        assert!(matches!(action, LifecycleProductAction::ExtensionList));
        Ok(LifecycleProductResponse {
            package_ref: None,
            phase: self.extension.phase,
            blockers: Vec::new(),
            message: None,
            payload: Some(LifecycleProductPayload::ExtensionList {
                extensions: vec![self.extension.clone()],
                count: 1,
            }),
        })
    }

    async fn project_package(
        &self,
        _context: LifecycleProductContext,
        _package_ref: LifecyclePackageRef,
    ) -> Result<LifecycleProductResponse, ProductSurfaceError> {
        panic!("list_extensions should execute the list action, not project one package")
    }

    async fn installed_activation_errors(
        &self,
        _context: LifecycleProductContext,
    ) -> Result<std::collections::HashMap<ExtensionId, String>, ProductSurfaceError> {
        Ok(self.activation_errors.clone())
    }
}

/// A channel-connection service that also reports the caller's durable
/// auth-account status per vendor, so the extensions wire projects real §6.3
/// states (expired / refresh-failed) instead of the connected/disconnected
/// collapse the connection bool alone permits.
struct AccountStatusConnectionService {
    connections: std::collections::HashMap<ExtensionId, bool>,
    account_states: std::collections::HashMap<ExtensionId, ChannelAuthAccountState>,
}

#[async_trait]
impl ChannelConnectionService for AccountStatusConnectionService {
    async fn caller_channel_connections(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<std::collections::HashMap<ExtensionId, bool>, ProductSurfaceError> {
        Ok(self.connections.clone())
    }

    async fn caller_channel_account_states(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<std::collections::HashMap<ExtensionId, ChannelAuthAccountState>, ProductSurfaceError>
    {
        Ok(self.account_states.clone())
    }
}

/// Un-collapse regression (G1/G2/G3) driven through the real
/// descriptor-backed `EXTENSIONS_VIEW` seam. Before the projection fix
/// this shape was unrepresentable/collapsed: a `Failed` extension read as
/// `Installed`, a live-grant account read as `connected` with no error, and
/// `activation_error` was hard-coded `None`. The service must now project all
/// three distinctly.
#[tokio::test]
async fn list_extensions_surfaces_failed_state_expired_account_and_activation_error() {
    let mut summary = extension_summary(
        "acme",
        vec![LifecycleExtensionCredentialRequirement {
            name: "acme_oauth_token".to_string(),
            provider: "acme".to_string(),
            required: true,
            setup: LifecycleExtensionCredentialSetup::ManualToken,
        }],
        None,
    );
    summary.surface_kinds = vec![CapabilitySurfaceKind::Channel];
    summary.channel_directions = Some(LifecycleChannelDirections {
        inbound: true,
        outbound: true,
    });
    summary.channel_connection = Some(ChannelConnectionRequirement {
        channel: "acme".to_string(),
        display_name: "Acme Messenger".to_string(),
        strategy: RebornChannelConnectStrategy::OAuth,
        instructions: "Connect Acme Messenger with OAuth.".to_string(),
        input_placeholder: String::new(),
        submit_label: "Connect Acme Messenger".to_string(),
        error_message: "Acme Messenger OAuth connection failed.".to_string(),
    });

    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(Arc::new(FailedStateLifecycleService {
        extension: LifecycleInstalledExtensionSummary {
            summary,
            // An enabled extension whose activation failed for a non-auth
            // reason — the terminal `Failed` state, distinct from a pristine
            // `Installed`, carrying its reason.
            phase: InstallationState::Failed,
            install_scope: None,
        },
        activation_errors: std::collections::HashMap::from([
            (
                ExtensionId::new("acme").expect("valid extension id"),
                "activation failed: runtime credential rejected".to_string(),
            ),
            // A *different* extension's failure sits in the same map. The key
            // is an `ExtensionId`, so the only way this reason could land on
            // the acme card is a genuine lookup bug — which is exactly what a
            // `String` key made indistinguishable from a package id, a display
            // name, or an installation id being keyed in by mistake.
            (
                ExtensionId::new("other-extension").expect("valid extension id"),
                "activation failed: a different extension entirely".to_string(),
            ),
        ]),
    }))
    .with_channel_connection_service(Arc::new(AccountStatusConnectionService {
        // The caller still holds a binding (connected), yet the durable
        // credential-account status says the grant's refresh failed. The real
        // status must win over the connected backfill.
        connections: std::collections::HashMap::from([(extension_id("acme"), true)]),
        account_states: std::collections::HashMap::from([(
            extension_id("acme"),
            ChannelAuthAccountState {
                account_status: Some(CredentialAccountStatus::RefreshFailed),
                active_flow_status: None,
            },
        )]),
    }));

    let response = query_extensions(&services, caller())
        .await
        .expect("extensions response");
    let info = response
        .extensions
        .iter()
        .find(|extension| extension.package_ref.id.as_str() == "acme")
        .expect("extension listed");

    // (a) An internal activation failure stays a redacted diagnostic attached
    // to the one caller-visible setup state; it never creates a third public
    // lifecycle state (overview.md §6.1). The distinguishing signal is
    // `activation_error` below, not a bespoke wire state.
    assert_eq!(
        info.installation_state,
        LifecyclePublicState::SetupNeeded,
        "a failed internal activation checkpoint must remain setup_needed",
    );
    assert_ne!(info.installation_state, LifecyclePublicState::Active);

    // (c) The redacted activation error reaches the DTO with its reason (the
    // frontend card renders this slot; it was fed `None` before).
    assert_eq!(
        info.activation_error.as_deref(),
        Some("activation failed: runtime credential rejected"),
        "the installation record's last_error must reach the projected DTO",
    );
    assert_ne!(
        info.activation_error.as_deref(),
        Some("activation failed: a different extension entirely"),
        "each card carries its OWN extension's reason; the map is keyed by \
         ExtensionId precisely so a neighbouring failure cannot land here",
    );

    // (b) The auth account projects its real §6.3 state + typed last error,
    // not the connected/disconnected collapse.
    let account = info
        .auth_accounts
        .first()
        .and_then(|vendor| vendor.accounts.first())
        .expect("vendor account projected");
    assert_eq!(
        account.state,
        AuthAccountState::Expired,
        "a refresh-failed credential account must project `expired`, not `connected`",
    );
    assert_eq!(
        account.last_error,
        Some(AuthAccountLastError::RefreshFailed),
        "the account's typed last_error must reach the projected DTO",
    );
}

#[tokio::test]
async fn setup_extension_returns_post_setup_onboarding_payload() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(Arc::new(
        RecordingLifecycleService::with_credential_requirements_and_onboarding(
            vec![manual_credential_requirement("github_runtime_token", true)],
            onboarding_fixture(),
        ),
    ));

    let response = query_extension_setup(&services, caller(), "github")
        .await
        .expect("setup extension response");

    let onboarding = response.onboarding.as_ref().expect("onboarding payload");
    assert_eq!(response.phase, LifecyclePublicState::SetupNeeded);
    assert_eq!(
        onboarding.credential_instructions.as_deref(),
        Some("github is installed. Activate it to make its tools available.")
    );
    assert_eq!(
        onboarding.credential_next_step.as_deref(),
        Some("After saving the token, activate GitHub to publish its tools.")
    );
}

#[tokio::test]
async fn setup_extension_dispatches_one_typed_hosted_mcp_auth_selection() {
    let lifecycle = Arc::new(RecordingLifecycleService::with_credential_requirements(
        vec![manual_credential_requirement(
            "hosted_mcp_bearer_token",
            true,
        )],
    ));
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(lifecycle.clone());

    invoke_extension_setup_submit(
        &services,
        caller(),
        "mcp-custom",
        ProductSetupExtensionRequest {
            client_action_id: None,
            action: Some("select_auth".to_string()),
            payload: Some(json!({
                "auth_selection": { "kind": "bearer" }
            })),
        },
    )
    .await
    .expect("typed hosted MCP auth selection dispatches");

    assert_eq!(
        lifecycle.actions(),
        vec![LifecycleProductAction::ExtensionSelectHostedMcpAuth {
            package_ref: LifecyclePackageRef::new(LifecyclePackageKind::Extension, "mcp-custom",)
                .expect("package ref"),
            auth_selection: HostedMcpAuthSelection::Bearer,
        }],
        "the setup boundary dispatches selection exactly once and does not run a second activation",
    );
    assert_eq!(
        lifecycle.package_refs(),
        vec![
            LifecyclePackageRef::new(LifecyclePackageKind::Extension, "mcp-custom",)
                .expect("package ref")
        ],
        "setup re-projects the package after applying the auth selection",
    );
}

#[tokio::test]
async fn setup_extension_rejects_auto_as_a_recovery_selection() {
    let services = setup_services_with_requirements(Vec::new());
    let err = invoke_extension_setup_submit(
        &services,
        caller(),
        "mcp-custom",
        ProductSetupExtensionRequest {
            client_action_id: None,
            action: Some("select_auth".to_string()),
            payload: Some(json!({
                "auth_selection": { "kind": "auto" }
            })),
        },
    )
    .await
    .expect_err("automatic detection cannot be retried as an explicit recovery choice");

    assert_setup_validation(err, "payload", ProductSurfaceValidationCode::InvalidValue);
}

#[tokio::test]
async fn setup_extension_rejects_blank_required_manual_secret() {
    let credentials = Arc::new(RecordingExtensionCredentialSetupService::default());
    let services =
        setup_services_with_requirements(vec![manual_credential_requirement("api_token", true)])
            .with_extension_credentials(credentials.clone());

    let err = invoke_extension_setup_submit(
        &services,
        caller(),
        "github",
        ProductSetupExtensionRequest {
            client_action_id: None,
            action: Some("submit".to_string()),
            payload: Some(json!({
                "secrets": {
                    "api_token": "   "
                }
            })),
        },
    )
    .await
    .expect_err("blank required token is rejected");

    assert_setup_validation(err, "secrets", ProductSurfaceValidationCode::Blank);
    assert_eq!(credentials.status_count(), 1);
    assert_eq!(credentials.submit_count(), 0);
}

#[tokio::test]
async fn setup_extension_rejects_unknown_secret_name() {
    let credentials = Arc::new(RecordingExtensionCredentialSetupService::default());
    let services =
        setup_services_with_requirements(vec![manual_credential_requirement("api_token", true)])
            .with_extension_credentials(credentials.clone());

    let err = invoke_extension_setup_submit(
        &services,
        caller(),
        "github",
        ProductSetupExtensionRequest {
            client_action_id: None,
            action: Some("submit".to_string()),
            payload: Some(json!({
                "secrets": {
                    "unknown_name": "value"
                }
            })),
        },
    )
    .await
    .expect_err("unknown secret name is rejected");

    assert_setup_validation(err, "secrets", ProductSurfaceValidationCode::InvalidValue);
    assert_eq!(credentials.status_count(), 0);
    assert_eq!(credentials.submit_count(), 0);
}

#[tokio::test]
async fn setup_extension_rejects_oauth_secret_via_manual_submit() {
    let credentials = Arc::new(RecordingExtensionCredentialSetupService::default());
    let services =
        setup_services_with_requirements(vec![oauth_credential_requirement("google_oauth", true)])
            .with_extension_credentials(credentials.clone());

    let err = invoke_extension_setup_submit(
        &services,
        caller(),
        "google",
        ProductSetupExtensionRequest {
            client_action_id: None,
            action: Some("submit".to_string()),
            payload: Some(json!({
                "secrets": {
                    "google_oauth": "value"
                }
            })),
        },
    )
    .await
    .expect_err("oauth credential cannot be submitted as a manual token");

    assert_setup_validation(err, "secrets", ProductSurfaceValidationCode::InvalidValue);
    assert_eq!(credentials.status_count(), 0);
    assert_eq!(credentials.submit_count(), 0);
}

/// Recording fake of the channel-config configure port: serves a fixed
/// field-status projection and records every save.
#[derive(Default)]
struct RecordingChannelConfigProductService {
    fields: Vec<RebornChannelConfigField>,
    saves: Mutex<Vec<RecordedChannelConfigSave>>,
}

impl RecordingChannelConfigProductService {
    fn with_fields(fields: Vec<RebornChannelConfigField>) -> Self {
        Self {
            fields,
            saves: Mutex::new(Vec::new()),
        }
    }

    fn saves(&self) -> Vec<RecordedChannelConfigSave> {
        self.saves.lock().expect("saves lock").clone()
    }
}

#[async_trait]
impl ChannelConfigProductService for RecordingChannelConfigProductService {
    async fn field_status(
        &self,
        _extension_id: &ExtensionId,
    ) -> Result<Vec<RebornChannelConfigField>, ProductSurfaceError> {
        Ok(self.fields.clone())
    }

    async fn save_values(
        &self,
        extension_id: &ExtensionId,
        values: Vec<(String, String)>,
    ) -> Result<(), ProductSurfaceError> {
        self.saves
            .lock()
            .expect("saves lock")
            .push((extension_id.as_str().to_string(), values));
        Ok(())
    }
}

fn channel_config_field(name: &str, label: &str, secret: bool) -> RebornChannelConfigField {
    RebornChannelConfigField {
        name: name.to_string(),
        label: label.to_string(),
        secret,
        provided: false,
    }
}

/// The setup service renders manifest-declared channel-config fields (the
/// non-secret descriptors in `fields`, the secret ones in the existing
/// `secrets` shape, presence only) and routes submitted values to the
/// configure port while credential secrets keep the credential path.
#[tokio::test]
async fn setup_extension_projects_and_routes_channel_config_values() {
    let credentials = Arc::new(RecordingExtensionCredentialSetupService::default());
    let channel_config = Arc::new(RecordingChannelConfigProductService::with_fields(vec![
        channel_config_field("bot_token", "Bot token", true),
        channel_config_field("public_url", "Public webhook URL", false),
    ]));
    let lifecycle_service = Arc::new(RecordingLifecycleService::with_credential_requirements(
        vec![manual_credential_requirement("api_token", false)],
    ));
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(lifecycle_service.clone())
    .with_extension_credentials(credentials.clone())
    .with_channel_config_product_service(channel_config.clone());

    // View: fields from the non-secret descriptors, secret channel fields in
    // the secrets list (presence only, manual-token shape).
    let view = query_extension_setup(&services, caller(), "github")
        .await
        .expect("setup view");
    assert_eq!(view.fields.len(), 1);
    assert_eq!(view.fields[0].name, "public_url");
    assert_eq!(view.fields[0].prompt, "Public webhook URL");
    assert!(view.fields[0].placeholder.is_none());
    let bot_token = view
        .secrets
        .iter()
        .find(|secret| secret.name == "bot_token")
        .expect("secret channel field surfaces in the secrets shape");
    assert!(!bot_token.provided);
    assert!(
        view.secrets.iter().any(|secret| secret.name == "api_token"),
        "credential requirements keep their own entry"
    );

    // Submit: channel values route to the configure port; the credential
    // secret stays on the credential path.
    let response = submit_extension_setup_and_query(
        &services,
        caller(),
        "github",
        ProductSetupExtensionRequest {
            client_action_id: None,
            action: Some("submit".to_string()),
            payload: Some(json!({
                "secrets": {
                    "bot_token": "xbt-123",
                    "api_token": "cred-456"
                },
                "fields": {
                    "public_url": "https://hooks.example.test/updates"
                }
            })),
        },
    )
    .await
    .expect("setup submit");
    assert_eq!(response.fields.len(), 1);
    let saves = channel_config.saves();
    assert_eq!(saves.len(), 1);
    assert_eq!(saves[0].0, "github");
    assert!(
        saves[0]
            .1
            .contains(&("bot_token".to_string(), "xbt-123".to_string()))
    );
    assert!(saves[0].1.contains(&(
        "public_url".to_string(),
        "https://hooks.example.test/updates".to_string()
    )));
    assert!(
        !saves[0].1.iter().any(|(name, _)| name == "api_token"),
        "credential secrets must not leak into the channel-config port"
    );
    assert_eq!(
        credentials.submit_count(),
        1,
        "the credential secret still reaches the credential path"
    );
    assert_eq!(
        lifecycle_service.actions(),
        vec![LifecycleProductAction::ExtensionActivate {
            package_ref: LifecyclePackageRef::new(LifecyclePackageKind::Extension, "github")
                .expect("package ref")
        }],
        "setup submit should activate after storing credentials and channel config"
    );
}

/// A submitted `fields` value that matches no declared non-secret handle is
/// rejected before anything is stored.
#[tokio::test]
async fn setup_extension_rejects_unknown_channel_config_field() {
    let credentials = Arc::new(RecordingExtensionCredentialSetupService::default());
    let channel_config = Arc::new(RecordingChannelConfigProductService::with_fields(vec![
        channel_config_field("public_url", "Public webhook URL", false),
    ]));
    let services = setup_services_with_requirements(Vec::new())
        .with_extension_credentials(credentials.clone())
        .with_channel_config_product_service(channel_config.clone());

    let err = invoke_extension_setup_submit(
        &services,
        caller(),
        "github",
        ProductSetupExtensionRequest {
            client_action_id: None,
            action: Some("submit".to_string()),
            payload: Some(json!({
                "fields": {
                    "unknown_field": "value"
                }
            })),
        },
    )
    .await
    .expect_err("unknown field handle is rejected");

    assert_setup_validation(err, "fields", ProductSurfaceValidationCode::InvalidValue);
    assert!(channel_config.saves().is_empty());
    assert_eq!(credentials.submit_count(), 0);
}

fn setup_services_with_requirements(
    requirements: Vec<LifecycleExtensionCredentialRequirement>,
) -> RebornServices {
    session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(Arc::new(
        RecordingLifecycleService::with_credential_requirements(requirements),
    ))
}

async fn query_extensions<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
) -> Result<RebornExtensionListResponse, ProductSurfaceError> {
    let page = query_product_surface_page(
        services,
        caller,
        RebornViewQuery {
            view_id: EXTENSIONS_VIEW.id.to_string(),
            params: json!({}),
            cursor: None,
        },
    )
    .await?;
    serde_json::from_value(page.payload).map_err(ProductSurfaceError::internal_from)
}

async fn query_extension_setup<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
    package_id: &str,
) -> Result<RebornSetupExtensionResponse, ProductSurfaceError> {
    let page = query_product_surface_page(
        services,
        caller,
        RebornViewQuery {
            view_id: EXTENSION_SETUP_VIEW.id.to_string(),
            params: json!({ "package_id": package_id }),
            cursor: None,
        },
    )
    .await?;
    serde_json::from_value(page.payload).map_err(ProductSurfaceError::internal_from)
}

async fn invoke_extension_setup_submit<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
    package_id: &str,
    request: ProductSetupExtensionRequest,
) -> Result<Resolution, ProductSurfaceError> {
    let mut input = serde_json::to_value(request).map_err(ProductSurfaceError::internal_from)?;
    input
        .as_object_mut()
        .expect("setup request serializes as object")
        .insert(
            "extension_id".to_string(),
            serde_json::Value::String(package_id.to_string()),
        );
    let response = services
        .invoke(
            caller,
            ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(EXTENSION_SETUP_SUBMIT_CAPABILITY_ID)
                    .expect("capability id"),
                input,
                activity_id: ActivityId::new(),
            },
        )
        .await?;
    serde_json::from_value(response.output).map_err(ProductSurfaceError::internal_from)
}

async fn submit_extension_setup_and_query<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
    package_id: &str,
    request: ProductSetupExtensionRequest,
) -> Result<RebornSetupExtensionResponse, ProductSurfaceError> {
    let caller_for_query = caller.clone();
    invoke_extension_setup_submit(services, caller, package_id, request).await?;
    query_extension_setup(services, caller_for_query, package_id).await
}

#[tokio::test]
async fn extension_import_is_available_as_product_capability() {
    let lifecycle_service = Arc::new(RecordingLifecycleService::new());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(lifecycle_service.clone());
    let bundle: Vec<u8> = b"PK\x03\x04\x00\xff\xfe binary zip bytes".to_vec();

    let resolution = services
        .invoke(
            caller(),
            CapabilityId::new(EXTENSION_IMPORT_CAPABILITY_ID).expect("capability id"),
            json!({ "bundle_base64": STANDARD.encode(&bundle) }),
            ActivityId::new(),
        )
        .await
        .expect("extension import");

    assert!(matches!(
        resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));
    assert_eq!(lifecycle_service.imported_bundles(), vec![bundle]);
}

/// The other half of the import capability: what a runtime that never wired a
/// bundle importer answers. Driven **through the surface**, not against the
/// trait default, because that is where the status the browser sees is
/// decided — `RebornServices` is constructed exactly as composition
/// constructs it and then simply not given a lifecycle service, which leaves
/// the `UnsupportedLifecycleProductService` default in place.
///
/// It must be `Unavailable`/503, never `InvalidRequest`/400: the caller
/// uploaded a bundle nothing ever looked at, so there is nothing about their
/// request to fault. (Flipped 2026-08-02 by the CHECKLIST WS2 "four WS2.1
/// follow-ups" row; the code used to be 400.)
#[tokio::test]
async fn webui_extension_import_reports_unavailable_when_no_service_is_wired() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let bundle: Vec<u8> = b"PK\x03\x04\x00\xff\xfe binary zip bytes".to_vec();

    let error = services
        .invoke(
            caller(),
            CapabilityId::new(EXTENSION_IMPORT_CAPABILITY_ID).expect("capability id"),
            json!({ "bundle_base64": STANDARD.encode(&bundle) }),
            ActivityId::new(),
        )
        .await
        .expect_err("a runtime with no bundle importer must refuse the import");

    assert_eq!(error.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(error.status_code, 503);
    assert!(
        !error.retryable,
        "an unwired capability does not become wired by retrying"
    );
    // The request itself was never faulted: a validation error would carry the
    // offending field, and this one has none.
    assert!(error.field.is_none());
    assert!(error.validation_code.is_none());
}

#[tokio::test]
async fn hosted_mcp_registration_auth_selection_blocker_is_sanitized_surface_validation_error() {
    let lifecycle = Arc::new(RecordingLifecycleService::new());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_lifecycle_product_service(lifecycle.clone());

    let error = ProductSurface::invoke(
        &services,
        caller(),
        ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(EXTENSION_REGISTER_HOSTED_MCP_CAPABILITY_ID)
                .expect("hosted MCP registration capability id"),
            input: json!({
                "desired_id": "calendar",
                "desired_name": "Calendar",
                "endpoint": "https://mcp.example.test"
            }),
            activity_id: ActivityId::new(),
        },
    )
    .await
    .expect_err("auth-selection blocker must reject hosted MCP registration");

    assert!(matches!(
        lifecycle.actions().as_slice(),
        [LifecycleProductAction::ExtensionRegisterHostedMcp { request }]
            if request.desired_id.as_str() == "calendar"
    ));
    assert_eq!(error.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(error.kind, ProductSurfaceErrorKind::Validation);
    assert_eq!(error.status_code, 400);
    assert!(!error.retryable);
    assert_eq!(error.field.as_deref(), Some("auth_selection"));
    assert_eq!(
        error.validation_code,
        Some(ProductSurfaceValidationCode::AuthSelectionRequired)
    );
    let rendered = serde_json::to_value(&error).expect("surface error serializes");
    assert_eq!(
        rendered,
        json!({
            "code": "invalid_request",
            "kind": "validation",
            "status_code": 400,
            "retryable": false,
            "field": "auth_selection",
            "validation_code": "auth_selection_required"
        })
    );
    assert!(
        !rendered
            .to_string()
            .contains("internal lifecycle diagnostic"),
        "the lifecycle response message must not cross the surface boundary"
    );
}

fn onboarding_fixture() -> LifecycleExtensionOnboarding {
    LifecycleExtensionOnboarding {
        instructions: "GitHub needs a token before its tools can run.".to_string(),
        credential_instructions: Some("Paste the GitHub token IronClaw should use.".to_string()),
        setup_url: Some("https://github.com/settings/personal-access-tokens/new".to_string()),
        credential_next_step: Some(
            "After saving the token, activate GitHub to publish its tools.".to_string(),
        ),
    }
}

fn manual_credential_requirement(
    name: &str,
    required: bool,
) -> LifecycleExtensionCredentialRequirement {
    LifecycleExtensionCredentialRequirement {
        name: name.to_string(),
        provider: "github".to_string(),
        required,
        setup: LifecycleExtensionCredentialSetup::ManualToken,
    }
}

fn oauth_credential_requirement(
    name: &str,
    required: bool,
) -> LifecycleExtensionCredentialRequirement {
    LifecycleExtensionCredentialRequirement {
        name: name.to_string(),
        provider: "google".to_string(),
        required,
        setup: LifecycleExtensionCredentialSetup::OAuth {
            scopes: vec!["https://www.googleapis.com/auth/gmail.readonly".to_string()],
        },
    }
}

#[derive(Default)]
struct RecordingIronhubLinkService {
    deliveries: Mutex<Vec<(ProductSurfaceCaller, IronhubInstallDeliveryRequest)>>,
}

#[async_trait]
impl IronhubLinkService for RecordingIronhubLinkService {
    async fn register(&self, _request: IronhubRegisterRequest) -> Result<(), IronhubLinkError> {
        Ok(())
    }

    async fn deliver_install(
        &self,
        caller: ProductSurfaceCaller,
        request: IronhubInstallDeliveryRequest,
    ) -> Result<IronhubInstallDeliveryResult, IronhubLinkError> {
        let slug = request.slug.clone();
        self.deliveries
            .lock()
            .expect("lock")
            .push((caller, request));
        Ok(IronhubInstallDeliveryResult {
            installed: true,
            slug,
            message: "installed".to_string(),
        })
    }
}

#[tokio::test]
async fn ironhub_delivery_command_forwards_authenticated_product_surface_caller() {
    let link = Arc::new(RecordingIronhubLinkService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_ironhub_link_service(link.clone());
    let authenticated_caller =
        caller_for_user_with_project("user-ironhub", Some("project-ironhub"));

    let response = ProductSurface::invoke(
        &services,
        authenticated_caller.clone(),
        ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(IRONHUB_DELIVER_INSTALL_COMMAND_ID)
                .expect("operation id"),
            input: serde_json::json!({
                "slug": "private-skill",
                "version": "1.2.3",
                "uid": "body-user-is-not-authority",
                "aid": "agent-alpha",
                "ts": 1_700_000_000_u64,
                "nonce": "nonce-1",
                "artifact_digest": "sha256:deadbeef",
                "sig": "signature",
                "private_manifest_url": "https://catalog.example/private/repo?token=rotating"
            }),
            activity_id: ActivityId::new(),
        },
    )
    .await
    .expect("delivery command");

    let output: IronhubInstallDeliveryResult =
        serde_json::from_value(response.output).expect("typed output");
    assert!(output.installed);
    assert_eq!(output.slug, "private-skill");

    let deliveries = link.deliveries.lock().expect("lock");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].0, authenticated_caller);
    assert_eq!(deliveries[0].1.uid, "body-user-is-not-authority");
}

#[tokio::test]
async fn ironhub_delivery_command_fails_closed_when_link_service_is_unwired() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let error = ProductSurface::invoke(
        &services,
        caller(),
        ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(IRONHUB_DELIVER_INSTALL_COMMAND_ID)
                .expect("operation id"),
            input: serde_json::json!({
                "slug": "private-skill",
                "version": "1.2.3",
                "uid": "body-user-is-not-authority",
                "aid": "agent-alpha",
                "ts": 1_700_000_000_u64,
                "nonce": "nonce-1",
                "artifact_digest": "sha256:deadbeef",
                "sig": "signature"
            }),
            activity_id: ActivityId::new(),
        },
    )
    .await
    .expect_err("unwired IronHub link service must fail closed");

    assert_eq!(error.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(error.kind, ProductSurfaceErrorKind::ServiceUnavailable);
    assert_eq!(error.status_code, 503);
    assert!(!error.retryable);
}
