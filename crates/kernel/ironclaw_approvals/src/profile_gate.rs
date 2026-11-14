use std::{borrow::Cow, sync::Arc};

use crate::{ToolPermissionOverride, permission_mode_allows_persistent_approval};
use async_trait::async_trait;
use ironclaw_authorization::{GrantAuthorizer, TrustAwareCapabilityDispatchAuthorizer};
use ironclaw_host_api::{
    Timestamp,
    action::Action,
    approval::ApprovalRequest,
    capability::{CapabilityDescriptor, CapabilityGrant, EffectKind, OriginGateMatrix},
    decision::{Decision, DenyReason},
    ids::{ApprovalRequestId, CapabilityId},
    invocation::InvocationOrigin,
    resource::{ResourceEstimate, ResourceScope},
    runtime_policy::ApprovalPolicy,
    scope::{ExecutionContext, Principal},
};
use ironclaw_trust::TrustDecision;

/// The origin→gate matrix's class-A contribution to one authorization (§5.2.1/S4).
///
/// Computed by [`ProfileApprovalGatePolicy::origin_gate_requirement`] from the
/// descriptor's [`OriginGateMatrix`] and the invocation's resolved
/// [`InvocationOrigin`]. The matrix maps onto the TWO existing gate tiers so its
/// §5.2.7 semantics are preserved exactly:
/// - [`OriginGateRequirement::Deny`] short-circuits to a hard deny ahead of the
///   class-B per-scope steps;
/// - [`OriginGateRequirement::GateHardFloor`] (`AskAlways`) composes with
///   `effects_force_approval` at the hard-floor step (ahead of class-B
///   auto-approve/always-allow), so "every invocation gates; persistent grants
///   never honored" holds — only a genuine one-shot approval lease satisfies it;
/// - [`OriginGateRequirement::GateSoft`] (`GatedUnlessGranted`) is OR'd into the
///   effect-based intrinsic gate at the soft step, so the same class-B
///   grant/always-allow machinery satisfies its "unless granted".
///
/// Class-B modulation (tool overrides, leases, auto-approve, always-allow) stays
/// entirely between the two tiers, exactly as for the effect gates it mirrors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginGateRequirement {
    /// The origin may not invoke this capability at all — a hard deny
    /// (fail-closed), not suppressible by any class-B grant/lease or by the
    /// Minimal (yolo) approval bypass.
    Deny,
    /// Hard-floor gate (`AskAlways`, §5.2.7): every invocation gates and no
    /// stored auto-approve/always-allow grant can bypass it — only a genuine
    /// one-shot approval lease satisfies it. Mirrors `effects_force_approval`:
    /// composed at the hard-floor step and NOT suppressed by the Minimal (yolo)
    /// bypass. A "Full access" thread never reaches this step (see
    /// [`ApprovalSettingsProvider::full_access`]).
    GateHardFloor,
    /// Soft gate (`GatedUnlessGranted`, §5.2.7): contributes an intrinsic
    /// approval gate whose "unless granted" is satisfied by the same class-B
    /// grant/always-allow machinery that satisfies the effect gate (no separate
    /// grant check here). Mirrors `effects_require_approval`: OR'd into the soft
    /// gate and suppressed under the Minimal-bypass guard so yolo stays
    /// "no prompts".
    GateSoft,
    /// The matrix adds no gate for this origin (`ConsentSufficient` / `Ungated`),
    /// or there is no matrix contribution to make (no resolvable origin — a
    /// test-only context that stamps neither `origin` nor `run_id`).
    None,
}

pub trait ProfileApprovalGatePolicy: Send + Sync {
    fn capability_exempt_from_approval(&self, _capability: &CapabilityId) -> bool {
        false
    }

    fn effects_require_approval(
        &self,
        approval_policy: ApprovalPolicy,
        effects: &[EffectKind],
    ) -> bool;

    /// Hard floor (#4776/#4959): effects that require an explicit approval
    /// gate and can never be auto-approved or satisfied by a stored
    /// always-allow grant, regardless of `ApprovalPolicy`. The reborn
    /// equivalent of v1's `ApprovalRequirement::Always`, expressed per-call
    /// over the invocation's effects. Defaults to "no floor". The one thing
    /// above it is "Full access" ([`ApprovalSettingsProvider::full_access`]),
    /// which the gate checks first and which skips the floor.
    fn effects_force_approval(&self, _effects: &[EffectKind]) -> bool {
        false
    }

    /// Class-A origin→gate matrix contribution (§5.2.1/S4): the intrinsic gate
    /// requirement implied by the descriptor's [`OriginGateMatrix`] for the
    /// invocation's resolved [`InvocationOrigin`].
    ///
    /// Composed at the SAME class-A points as the effect gates it mirrors, so
    /// class-B stays between the two tiers unchanged. Contract:
    /// - no resolvable `origin` (test-only contexts) → [`OriginGateRequirement::None`]
    ///   (no contribution — keeps pre-S4 behavior neutral);
    /// - `matrix` is `None` with an origin present → fail-closed
    ///   [`OriginGateRequirement::Deny`] (production descriptors always declare
    ///   one; only test fixtures are `None`, and those carry no origin);
    /// - `Forbidden` → [`OriginGateRequirement::Deny`] (not suppressed by the
    ///   Minimal bypass);
    /// - `AskAlways` → [`OriginGateRequirement::GateHardFloor`] (hard floor,
    ///   §5.2.7; NOT suppressed by the Minimal bypass — mirrors
    ///   `effects_force_approval`);
    /// - `GatedUnlessGranted` → [`OriginGateRequirement::GateSoft`], suppressed
    ///   to `None` under the SAME Minimal-bypass guard [`Self::effects_require_approval`]
    ///   uses;
    /// - `ConsentSufficient` / `Ungated` → [`OriginGateRequirement::None`].
    ///
    /// The default is `None` (no contribution), so a gate policy that does not
    /// consult the matrix leaves authorization exactly as it was pre-S4.
    fn origin_gate_requirement(
        &self,
        _approval_policy: ApprovalPolicy,
        _origin: Option<&InvocationOrigin>,
        _matrix: Option<&OriginGateMatrix>,
    ) -> OriginGateRequirement {
        OriginGateRequirement::None
    }
}

/// Resolves approval settings for one dispatch. Implementations read the
/// durable per-user stores; the authorizer queries these after the base grant
/// decision allows the candidate so settings apply without process restart
/// while non-runnable candidates do not spend approval-store reads.
#[async_trait]
pub trait ApprovalSettingsProvider: Send + Sync {
    async fn tool_override(
        &self,
        scope: &ResourceScope,
        capability_id: &CapabilityId,
    ) -> Option<ToolPermissionOverride>;

    async fn global_auto_approve(&self, scope: &ResourceScope) -> bool;

    async fn tool_always_allow(
        &self,
        scope: &ResourceScope,
        capability_id: &CapabilityId,
        grantee: &Principal,
    ) -> bool;

    /// Whether the dispatching thread runs on "Full access": every tool call
    /// runs without asking, hard floors and `ask_each_time` included; only an
    /// explicit per-tool `disabled` still denies. Off by default; composition's
    /// profile-aware provider answers it from the thread's profile mode, or the
    /// global auto-approve switch for a thread whose profile sets none.
    async fn full_access(&self, _scope: &ResourceScope) -> bool {
        false
    }

    /// The per-tool override, distinguishing "none set" from "could not be
    /// read". The approval gate refuses on [`ToolOverrideLookup::Unavailable`]
    /// before any approval or Full-access shortcut. Defaults to
    /// [`Self::tool_override`]; a provider whose store can fail overrides it.
    async fn tool_override_lookup(
        &self,
        scope: &ResourceScope,
        capability_id: &CapabilityId,
    ) -> ToolOverrideLookup {
        match self.tool_override(scope, capability_id).await {
            Some(state) => ToolOverrideLookup::Set(state),
            None => ToolOverrideLookup::Unset,
        }
    }

    /// Where [`Self::full_access`] comes from: an explicit Full-access choice
    /// for the thread, or the global auto-approve switch the thread inherits.
    /// A capability that opts out of global auto-approve accepts only the
    /// explicit kind. Defaults to treating `full_access` as explicit.
    async fn full_access_source(&self, scope: &ResourceScope) -> FullAccessSource {
        if self.full_access(scope).await {
            FullAccessSource::Explicit
        } else {
            FullAccessSource::Off
        }
    }
}

/// A per-tool override lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOverrideLookup {
    Unset,
    Set(ToolPermissionOverride),
    /// The override store could not be read: fail closed.
    Unavailable,
}

/// Why a thread runs on "Full access", if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullAccessSource {
    Off,
    /// The thread's profile explicitly chose Full access.
    Explicit,
    /// The thread follows the global switch, which is on.
    InheritedGlobal,
}

/// No stored overrides and global auto-approve off: the gate behaves exactly as
/// it did before #4959. Test-only — production wires composition's
/// `StoreApprovalSettingsProvider`.
///
/// Gated on `test-support` rather than `cfg(test)` because composition's own
/// capability-host tests drive the gate through this double; the crate-local
/// `cfg(test)` form it carried inside composition is unreachable across a crate
/// boundary.
#[cfg(any(test, feature = "test-support"))]
pub struct EmptyApprovalSettingsProvider;

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl ApprovalSettingsProvider for EmptyApprovalSettingsProvider {
    async fn tool_override(
        &self,
        _scope: &ResourceScope,
        _capability_id: &CapabilityId,
    ) -> Option<ToolPermissionOverride> {
        None
    }

    async fn global_auto_approve(&self, _scope: &ResourceScope) -> bool {
        false
    }

    async fn tool_always_allow(
        &self,
        _scope: &ResourceScope,
        _capability_id: &CapabilityId,
        _grantee: &Principal,
    ) -> bool {
        false
    }
}

pub fn profile_approval_authorizer(
    approval_policy: ApprovalPolicy,
    gate_policy: Arc<dyn ProfileApprovalGatePolicy>,
    settings: Arc<dyn ApprovalSettingsProvider>,
) -> Arc<dyn TrustAwareCapabilityDispatchAuthorizer> {
    Arc::new(ProfileApprovalPolicyAuthorizer::new(
        approval_policy,
        gate_policy,
        settings,
    ))
}

struct ProfileApprovalPolicyAuthorizer {
    inner: GrantAuthorizer,
    approval_policy: ApprovalPolicy,
    gate_policy: Arc<dyn ProfileApprovalGatePolicy>,
    settings: Arc<dyn ApprovalSettingsProvider>,
}

impl ProfileApprovalPolicyAuthorizer {
    fn new(
        approval_policy: ApprovalPolicy,
        gate_policy: Arc<dyn ProfileApprovalGatePolicy>,
        settings: Arc<dyn ApprovalSettingsProvider>,
    ) -> Self {
        Self {
            inner: GrantAuthorizer::new(),
            approval_policy,
            gate_policy,
            settings,
        }
    }
}

#[async_trait::async_trait]
impl TrustAwareCapabilityDispatchAuthorizer for ProfileApprovalPolicyAuthorizer {
    async fn authorize_dispatch_with_trust(
        &self,
        context: &ExecutionContext,
        descriptor: &CapabilityDescriptor,
        estimate: &ResourceEstimate,
        trust_decision: &TrustDecision,
    ) -> Decision {
        let decision = self
            .inner
            .authorize_dispatch_with_trust(context, descriptor, estimate, trust_decision)
            .await;
        require_approval_for_profile_policy(
            decision,
            context,
            descriptor,
            estimate,
            ProfileApprovalActionKind::Dispatch,
            self.approval_policy,
            self.gate_policy.as_ref(),
            self.settings.as_ref(),
        )
        .await
    }

    async fn authorize_spawn_with_trust(
        &self,
        context: &ExecutionContext,
        descriptor: &CapabilityDescriptor,
        estimate: &ResourceEstimate,
        trust_decision: &TrustDecision,
    ) -> Decision {
        let decision = self
            .inner
            .authorize_spawn_with_trust(context, descriptor, estimate, trust_decision)
            .await;
        require_approval_for_profile_policy(
            decision,
            context,
            descriptor,
            estimate,
            ProfileApprovalActionKind::SpawnCapability,
            self.approval_policy,
            self.gate_policy.as_ref(),
            self.settings.as_ref(),
        )
        .await
    }
}

#[derive(Clone, Copy, Debug)]
enum ProfileApprovalActionKind {
    Dispatch,
    SpawnCapability,
}

#[allow(clippy::too_many_arguments)]
// arch-exempt: too_many_args, gate decision needs context+descriptor+estimate+policy+gate+settings, plan #4776
async fn require_approval_for_profile_policy(
    decision: Decision,
    context: &ExecutionContext,
    descriptor: &CapabilityDescriptor,
    estimate: &ResourceEstimate,
    action_kind: ProfileApprovalActionKind,
    approval_policy: ApprovalPolicy,
    gate_policy: &dyn ProfileApprovalGatePolicy,
    settings: &dyn ApprovalSettingsProvider,
) -> Decision {
    // The profile approval gate only ever upgrades an underlying `Allow`; a
    // `Deny` / `RequireApproval` from the grant authorizer passes through
    // unchanged.
    let Decision::Allow { .. } = &decision else {
        return decision;
    };

    // A spawn exercises SpawnProcess even when the capability's own descriptor
    // does not declare it: the underlying GrantAuthorizer authorizes spawns
    // against `spawn_descriptor`, which adds EffectKind::SpawnProcess. Evaluate
    // the approval gate against the same elevated effect set so a dispatch-only
    // capability cannot be spawned as a live process without an approval gate.
    let gate_effects = approval_gate_effects(action_kind, descriptor);
    let effect_based_intrinsic_gate =
        gate_policy.effects_require_approval(approval_policy, &gate_effects);

    // Class-A origin→gate matrix contribution (§5.2.1/S4). Origin is resolved
    // through `ExecutionContext::resolved_origin` (the single definition of the
    // `run_id`-implies-`LoopRun` rule, shared with `seal_authorization`), so a
    // loop context that stamped only `run_id` still has its `LoopRun` column
    // consulted, and a context that stamps neither (test-only) resolves to
    // `None` — the matrix then contributes nothing, keeping pre-S4 decisions
    // neutral.
    let origin = context.resolved_origin();
    let origin_requirement = gate_policy.origin_gate_requirement(
        approval_policy,
        origin.as_ref(),
        descriptor.origin_gate_matrix.as_ref(),
    );
    let (origin_denied, origin_requires_approval, origin_forces_approval) = match origin_requirement
    {
        OriginGateRequirement::Deny => (true, false, false),
        OriginGateRequirement::GateHardFloor => (false, false, true),
        OriginGateRequirement::GateSoft => (false, true, false),
        OriginGateRequirement::None => (false, false, false),
    };

    // A `Forbidden` origin is denied regardless of any class-B grant/lease and
    // regardless of the Minimal (yolo) bypass — the origin may not invoke this
    // capability at all. Short-circuited to a sanitized, model-visible
    // `Decision::Deny` ahead of the class-B steps. The internal audit reason is
    // preserved in the debug log; the wire `DenyReason` carries no free-form
    // detail (`PolicyDenied`, matching the explicit-`disabled` deny path). No
    // production capability declares `Forbidden` for the live `LoopRun` origin,
    // and product/automation origins have no live producer, so this deny path is
    // unreachable in production today (fail-closed for the future).
    if origin_denied {
        tracing::debug!(
            capability = descriptor.id.as_str(),
            origin = origin.as_ref().map(InvocationOrigin::kind),
            "origin-gate matrix forbids this origin; denying dispatch (§5.2.1)"
        );
        return Decision::Deny {
            reason: DenyReason::PolicyDenied,
        };
    }

    // The soft class-A intrinsic gate is the OR of the effect-based gate and the
    // matrix's SOFT (`GatedUnlessGranted`) contribution: both live at the same
    // (step-9) precedence, so both are satisfied by the same class-B
    // grant/always-allow steps below and both are suppressed together under the
    // Minimal bypass. The matrix's HARD-FLOOR (`AskAlways`) contribution is NOT
    // folded here — it composes with `effects_force_approval` at step 3, ahead
    // of class-B (see below), so an always-allow/auto-approve cannot bypass it.
    let profile_requires_approval = effect_based_intrinsic_gate || origin_requires_approval;

    let expected_grantee = Principal::Extension(descriptor.provider.clone());
    let durable_auto_approval_eligible =
        permission_mode_allows_persistent_approval(descriptor.default_permission);
    let verdict = approval_gate_verdict(
        settings,
        ApprovalGateQuery {
            scope: &context.resource_scope,
            capability_id: &descriptor.id,
            grantee: &expected_grantee,
            // Fingerprinted approval leases are not ambient grants:
            // CapabilityHost's `resume_json` path excludes them from normal
            // grant loading, then validates the blocked run's
            // approval_request_id, approval request metadata, and invocation
            // fingerprint before injecting the matching lease grant into this
            // resume context. This predicate therefore recognizes the already
            // selected resume lease; it is not the lease lookup boundary.
            approved_for_this_invocation: has_matching_one_shot_approval_grant(
                context,
                descriptor,
                &gate_effects,
            ),
            // The matrix's `AskAlways` hard-floor gate composes here (§5.2.7),
            // with the same semantics as `effects_force_approval`.
            force_approval: gate_policy.effects_force_approval(&gate_effects)
                || origin_forces_approval,
            exempt_from_approval: gate_policy.capability_exempt_from_approval(&descriptor.id),
            global_auto_approve_eligible: durable_auto_approval_eligible,
            // Builtins treat an inherited Full access like an explicit one.
            inherited_full_access_eligible: true,
            always_allow_eligible: durable_auto_approval_eligible,
            approved_for_this_task: false,
            policy_requires_approval: profile_requires_approval,
        },
    )
    .await;
    match verdict {
        ApprovalGateVerdict::Allow => decision,
        ApprovalGateVerdict::RequireApproval => Decision::RequireApproval {
            request: approval_request(context, descriptor, estimate, action_kind),
        },
        ApprovalGateVerdict::Deny | ApprovalGateVerdict::SettingsUnavailable => Decision::Deny {
            reason: DenyReason::PolicyDenied,
        },
    }
}

/// What the approval gate concludes for one invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalGateVerdict {
    Allow,
    RequireApproval,
    Deny,
    /// A setting the decision needs could not be read: refuse (fail closed).
    SettingsUnavailable,
}

/// The inputs of the approval gate's decision for one invocation that the
/// settings provider does not answer itself.
///
/// The capability host derives these from a [`CapabilityDescriptor`]; callers
/// without one (synthetic loop capabilities) state them directly, so every
/// gated call shares [`approval_gate_verdict`]'s precedence instead of a copy.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalGateQuery<'a> {
    /// The invocation's full resource scope — thread included, since the
    /// thread's profile decides "Full access" and auto-approve.
    pub scope: &'a ResourceScope,
    pub capability_id: &'a CapabilityId,
    /// The principal a stored always-allow grant must name.
    pub grantee: &'a Principal,
    /// A verified one-shot approval of exactly this invocation.
    pub approved_for_this_invocation: bool,
    /// Hard floor: only a one-shot approval (or Full access) satisfies it.
    pub force_approval: bool,
    pub exempt_from_approval: bool,
    /// Whether the global auto-approve switch may let this capability run.
    pub global_auto_approve_eligible: bool,
    /// Whether a Full access the thread merely inherits from the global
    /// switch (no explicit profile choice) lets this capability run.
    pub inherited_full_access_eligible: bool,
    /// Whether a stored per-tool always-allow may let this capability run.
    pub always_allow_eligible: bool,
    /// An approval already given in this task that, by the capability's own
    /// contract, covers its later calls. Ranks with always-allow.
    pub approved_for_this_task: bool,
    /// Whether the runtime policy gates this capability's effects at all.
    pub policy_requires_approval: bool,
}

/// The approval gate's decision precedence (#4776), shared by the capability
/// host and every other gated caller. Settings errors are the provider's to
/// fail safe on (an unreadable setting never allows).
pub async fn approval_gate_verdict(
    settings: &dyn ApprovalSettingsProvider,
    query: ApprovalGateQuery<'_>,
) -> ApprovalGateVerdict {
    // Decision precedence (high → low), #4776:
    // 1. Explicit per-tool `disabled` → deny outright (strongest user intent),
    //    even over a one-shot approval already given.
    //    An unreadable override is refused here too, ahead of every
    //    shortcut below (a one-shot approval, Full access): the setting it
    //    could not read may be exactly the `disabled` that wins.
    let tool_override = match settings
        .tool_override_lookup(query.scope, query.capability_id)
        .await
    {
        ToolOverrideLookup::Unavailable => return ApprovalGateVerdict::SettingsUnavailable,
        ToolOverrideLookup::Set(ToolPermissionOverride::Disabled) => {
            return ApprovalGateVerdict::Deny;
        }
        ToolOverrideLookup::Set(state) => Some(state),
        ToolOverrideLookup::Unset => None,
    };
    // 2. A matching one-shot approval satisfies the current resume. This must
    //    beat explicit ask_each_time and hard-floor gates: those settings
    //    require a fresh human approval for each new invocation, but the lease
    //    is the durable proof that this invocation was just approved.
    if query.approved_for_this_invocation {
        return ApprovalGateVerdict::Allow;
    }
    // 2b. A "Full access" thread never asks: the person chose to run every
    //     call unattended, hard floors included.
    //     A Full access merely inherited from the global switch counts only
    //     for capabilities that honor global auto-approve.
    match settings.full_access_source(query.scope).await {
        FullAccessSource::Explicit => return ApprovalGateVerdict::Allow,
        FullAccessSource::InheritedGlobal if query.inherited_full_access_eligible => {
            return ApprovalGateVerdict::Allow;
        }
        FullAccessSource::InheritedGlobal | FullAccessSource::Off => {}
    }
    // 3. Hard floor: never auto-approve / never satisfiable by a stored grant
    //    (a Full access thread already returned at 2b).
    if query.force_approval {
        return ApprovalGateVerdict::RequireApproval;
    }
    // 4. Explicit per-tool `ask_each_time` → always gate fresh invocations,
    //    ignoring the global auto-approve setting and any stored always-allow
    //    grant.
    if matches!(tool_override, Some(ToolPermissionOverride::AskEachTime)) {
        return ApprovalGateVerdict::RequireApproval;
    }
    // 5. Capability deliberately exempt from the gate (in-turn consent).
    if query.exempt_from_approval {
        return ApprovalGateVerdict::Allow;
    }
    // 6. Global auto-approve bypasses an otherwise-gated eligible tool. Check
    //    this before per-tool always-allow because it is the default-on common
    //    path for new users; if enabled, both branches return the same Allow.
    if query.global_auto_approve_eligible && settings.global_auto_approve(query.scope).await {
        return ApprovalGateVerdict::Allow;
    }
    // 7. A settings-scope per-tool always-allow policy satisfies the gate.
    //    The provider verifies the active settings-page persistent policy
    //    directly, keyed to the capability provider rather than the caller
    //    extension, so this does not depend on whether that policy was also
    //    preloaded into this run's grants. Legacy prompt-created persistent
    //    grants are deliberately not enough when the tool row is "Follow
    //    global" and the global switch is off.
    if query.always_allow_eligible
        && settings
            .tool_always_allow(query.scope, query.capability_id, query.grantee)
            .await
    {
        return ApprovalGateVerdict::Allow;
    }
    // 7b. An approval earlier in this task that covers the task's later calls.
    if query.approved_for_this_task {
        return ApprovalGateVerdict::Allow;
    }
    // 8. With the global switch off, eligible tools default to asking even when
    //    the runtime profile would otherwise allow low-risk effects. This makes
    //    the Tools page switch authoritative for #4776; explicit always-allow
    //    grants above remain the per-tool opt-in.
    if query.global_auto_approve_eligible || query.always_allow_eligible {
        return ApprovalGateVerdict::RequireApproval;
    }
    // 9. Policy does not require a gate for this effect set.
    if !query.policy_requires_approval {
        return ApprovalGateVerdict::Allow;
    }
    ApprovalGateVerdict::RequireApproval
}

/// Effects the profile approval gate evaluates for `action_kind`.
///
/// Mirrors `ironclaw_authorization::spawn_descriptor`: a spawn always exercises
/// `SpawnProcess`, so it is added to the capability's declared effects when
/// gating a spawn. Dispatch evaluates the declared effects unchanged.
fn approval_gate_effects(
    action_kind: ProfileApprovalActionKind,
    descriptor: &CapabilityDescriptor,
) -> Cow<'_, [EffectKind]> {
    match action_kind {
        ProfileApprovalActionKind::Dispatch => Cow::Borrowed(descriptor.effects.as_slice()),
        ProfileApprovalActionKind::SpawnCapability => {
            if descriptor.effects.contains(&EffectKind::SpawnProcess) {
                Cow::Borrowed(descriptor.effects.as_slice())
            } else {
                let mut effects = descriptor.effects.clone();
                effects.push(EffectKind::SpawnProcess);
                Cow::Owned(effects)
            }
        }
    }
}

fn has_matching_one_shot_approval_grant(
    context: &ExecutionContext,
    descriptor: &CapabilityDescriptor,
    gate_effects: &[EffectKind],
) -> bool {
    let expected_grantee = Principal::Extension(context.extension_id.clone());
    let expected_user_approver = Principal::User(context.user_id.clone());
    let now = chrono::Utc::now();
    context.grants.grants.iter().any(|grant| {
        let grant_unexpired = grant_is_unexpired(grant, &now);
        let one_shot_approval_grant = grant.constraints.max_invocations == Some(1)
            && (grant.issued_by == Principal::HostRuntime
                || grant.issued_by == expected_user_approver)
            && grant_unexpired;
        grant.capability == descriptor.id
            && one_shot_approval_grant
            && grant.grantee == expected_grantee
            // Match against the spawn-elevated effect set so a one-shot lease
            // that does not cover SpawnProcess cannot satisfy a spawn gate.
            && gate_effects
                .iter()
                .all(|effect| grant.constraints.allowed_effects.contains(effect))
    })
}

fn grant_is_unexpired(grant: &CapabilityGrant, now: &Timestamp) -> bool {
    grant
        .constraints
        .expires_at
        .as_ref()
        .is_none_or(|expires_at| expires_at > now)
}

fn approval_request(
    context: &ExecutionContext,
    descriptor: &CapabilityDescriptor,
    estimate: &ResourceEstimate,
    action_kind: ProfileApprovalActionKind,
) -> ApprovalRequest {
    let action = match action_kind {
        ProfileApprovalActionKind::Dispatch => Action::Dispatch {
            capability: descriptor.id.clone(),
            estimated_resources: estimate.clone(),
        },
        ProfileApprovalActionKind::SpawnCapability => Action::SpawnCapability {
            capability: descriptor.id.clone(),
            estimated_resources: estimate.clone(),
        },
    };
    ApprovalRequest {
        id: ApprovalRequestId::new(),
        correlation_id: context.correlation_id,
        requested_by: Principal::Extension(context.extension_id.clone()),
        action: Box::new(action),
        invocation_fingerprint: None,
        reason: format!(
            "approval required for {:?} of {}",
            action_kind,
            descriptor.id.as_str()
        ),
        reusable_scope: None,
    }
}

#[cfg(test)]
mod tests {
    use crate::persistent_approval_grant_issuer;
    use ironclaw_host_api::{
        action::NetworkPolicy,
        capability::{
            CapabilityDescriptor, CapabilityGrant, CapabilitySet, EffectKind, GrantConstraints,
            PermissionMode,
        },
        ids::{CapabilityGrantId, CapabilityId, ExtensionId},
        mount::MountView,
        resource::ResourceEstimate,
        runtime::{RuntimeKind, TrustClass},
        scope::{ExecutionContext, Principal},
    };
    use ironclaw_trust::{AuthorityCeiling, EffectiveTrustClass, TrustDecision, TrustProvenance};
    use serde_json::json;

    use super::*;

    #[derive(Debug)]
    struct TestGatePolicy;

    impl ProfileApprovalGatePolicy for TestGatePolicy {
        fn effects_require_approval(
            &self,
            approval_policy: ApprovalPolicy,
            effects: &[EffectKind],
        ) -> bool {
            match approval_policy {
                ApprovalPolicy::Minimal => false,
                ApprovalPolicy::AskAlways => !effects.is_empty(),
                ApprovalPolicy::AskWrites | ApprovalPolicy::AskDestructive => {
                    effects.contains(&EffectKind::SpawnProcess)
                }
                ApprovalPolicy::OrgPolicy => !effects.is_empty(),
                _ => !effects.is_empty(),
            }
        }

        fn effects_force_approval(&self, effects: &[EffectKind]) -> bool {
            effects.contains(&EffectKind::Financial)
        }
    }

    /// Returns fixed settings so the gate's per-turn resolution can be driven
    /// deterministically (#4959).
    struct StubSettingsProvider {
        tool_override: Option<ToolPermissionOverride>,
        global_auto_approve: bool,
        tool_always_allow: bool,
    }

    #[async_trait]
    impl ApprovalSettingsProvider for StubSettingsProvider {
        async fn tool_override(
            &self,
            _scope: &ResourceScope,
            _capability_id: &CapabilityId,
        ) -> Option<ToolPermissionOverride> {
            self.tool_override
        }

        async fn global_auto_approve(&self, _scope: &ResourceScope) -> bool {
            self.global_auto_approve
        }

        async fn tool_always_allow(
            &self,
            _scope: &ResourceScope,
            _capability_id: &CapabilityId,
            _grantee: &Principal,
        ) -> bool {
            self.tool_always_allow
        }
    }

    /// Dispatch a `builtin.shell` capability carrying `effects`, with a granting
    /// lease and trust ceiling that make the underlying decision `Allow`, under
    /// the given approval policy + resolved settings.
    async fn dispatch_decision(
        approval_policy: ApprovalPolicy,
        effects: Vec<EffectKind>,
        settings: StubSettingsProvider,
    ) -> Decision {
        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let descriptor = test_descriptor_with_id(shell_id.clone(), effects.clone());
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: effects.clone(),
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let trust = TrustDecision {
            effective_trust: EffectiveTrustClass::user_trusted(),
            authority_ceiling: AuthorityCeiling {
                allowed_effects: effects,
                max_resource_ceiling: None,
            },
            provenance: TrustProvenance::AdminConfig,
            evaluated_at: chrono::Utc::now(),
        };
        profile_approval_authorizer(
            approval_policy,
            Arc::new(TestGatePolicy),
            Arc::new(settings),
        )
        .authorize_dispatch_with_trust(&ctx, &descriptor, &ResourceEstimate::default(), &trust)
        .await
    }

    #[tokio::test]
    async fn global_auto_approve_skips_gate_for_eligible_tool() {
        let decision = dispatch_decision(
            ApprovalPolicy::AskDestructive,
            vec![EffectKind::SpawnProcess],
            StubSettingsProvider {
                tool_override: None,
                global_auto_approve: true,
                tool_always_allow: false,
            },
        )
        .await;
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "global auto-approve should skip the gate for an eligible tool, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn explicit_ask_each_time_overrides_global_auto_approve() {
        let decision = dispatch_decision(
            ApprovalPolicy::AskDestructive,
            vec![EffectKind::SpawnProcess],
            StubSettingsProvider {
                tool_override: Some(ToolPermissionOverride::AskEachTime),
                global_auto_approve: true,
                tool_always_allow: false,
            },
        )
        .await;
        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "explicit ask_each_time must gate even with global auto-approve on, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn explicit_disabled_denies_dispatch() {
        let decision = dispatch_decision(
            ApprovalPolicy::AskDestructive,
            vec![EffectKind::SpawnProcess],
            StubSettingsProvider {
                tool_override: Some(ToolPermissionOverride::Disabled),
                global_auto_approve: true,
                tool_always_allow: false,
            },
        )
        .await;
        assert!(
            matches!(
                decision,
                Decision::Deny {
                    reason: DenyReason::PolicyDenied
                }
            ),
            "explicit disabled must deny, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn hard_floor_requires_approval_even_with_global_auto_approve() {
        let decision = dispatch_decision(
            ApprovalPolicy::AskDestructive,
            vec![EffectKind::Financial],
            StubSettingsProvider {
                tool_override: None,
                global_auto_approve: true,
                tool_always_allow: false,
            },
        )
        .await;
        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "hard-floor (Financial) must gate even with global auto-approve on, got {decision:?}"
        );
    }

    /// A "Full access" thread: ask-each-time on every tool, nothing else set.
    struct FullAccessSettings(Option<ToolPermissionOverride>);

    #[async_trait]
    impl ApprovalSettingsProvider for FullAccessSettings {
        async fn tool_override(
            &self,
            _scope: &ResourceScope,
            _capability_id: &CapabilityId,
        ) -> Option<ToolPermissionOverride> {
            self.0
        }
        async fn global_auto_approve(&self, _scope: &ResourceScope) -> bool {
            false
        }
        async fn tool_always_allow(
            &self,
            _scope: &ResourceScope,
            _capability_id: &CapabilityId,
            _grantee: &Principal,
        ) -> bool {
            false
        }
        async fn full_access(&self, _scope: &ResourceScope) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn full_access_never_asks_but_disabled_still_denies() {
        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let effects = vec![EffectKind::Financial, EffectKind::SpawnProcess];
        let descriptor = test_descriptor_with_id(shell_id.clone(), effects.clone());
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: effects.clone(),
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let trust = TrustDecision {
            effective_trust: EffectiveTrustClass::user_trusted(),
            authority_ceiling: AuthorityCeiling {
                allowed_effects: effects,
                max_resource_ceiling: None,
            },
            provenance: TrustProvenance::AdminConfig,
            evaluated_at: chrono::Utc::now(),
        };
        for (setting, allowed) in [
            (Some(ToolPermissionOverride::AskEachTime), true),
            (None, true),
            (Some(ToolPermissionOverride::Disabled), false),
        ] {
            let decision = profile_approval_authorizer(
                ApprovalPolicy::AskDestructive,
                Arc::new(TestGatePolicy),
                Arc::new(FullAccessSettings(setting)),
            )
            .authorize_dispatch_with_trust(&ctx, &descriptor, &ResourceEstimate::default(), &trust)
            .await;
            assert_eq!(
                matches!(decision, Decision::Allow { .. }),
                allowed,
                "{setting:?}: {decision:?}"
            );
        }
    }

    fn test_descriptor(effects: Vec<EffectKind>) -> CapabilityDescriptor {
        test_descriptor_with_id(CapabilityId::new("builtin.shell").unwrap(), effects)
    }

    fn test_descriptor_with_id(id: CapabilityId, effects: Vec<EffectKind>) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id,
            provider: ExtensionId::new("builtin").unwrap(),
            runtime: RuntimeKind::FirstParty,
            trust_ceiling: TrustClass::UserTrusted,
            description: "test".to_string(),
            parameters_schema: json!({}),
            effects,
            default_permission: PermissionMode::Allow,
            runtime_credentials: Vec::new(),
            network_targets: Vec::new(),
            max_egress_bytes: None,
            resource_profile: None,
            origin_gate_matrix: None,
            standard_op: None,
        }
    }

    fn test_context(grants: CapabilitySet) -> ExecutionContext {
        let ctx = ExecutionContext::local_default(
            ironclaw_host_api::ids::UserId::new("test-user").unwrap(),
            ExtensionId::new("builtin").unwrap(),
            RuntimeKind::FirstParty,
            TrustClass::UserTrusted,
            grants,
            MountView::default(),
        )
        .unwrap();
        ctx.validate().unwrap();
        ctx
    }

    fn test_trust_decision() -> TrustDecision {
        TrustDecision {
            effective_trust: EffectiveTrustClass::user_trusted(),
            authority_ceiling: AuthorityCeiling {
                allowed_effects: vec![EffectKind::SpawnProcess, EffectKind::DispatchCapability],
                max_resource_ceiling: None,
            },
            provenance: TrustProvenance::AdminConfig,
            evaluated_at: chrono::Utc::now(),
        }
    }

    fn test_authorizer(
        approval_policy: ApprovalPolicy,
    ) -> Arc<dyn TrustAwareCapabilityDispatchAuthorizer> {
        profile_approval_authorizer(
            approval_policy,
            Arc::new(TestGatePolicy),
            Arc::new(EmptyApprovalSettingsProvider),
        )
    }

    fn test_authorizer_with_settings(
        approval_policy: ApprovalPolicy,
        settings: StubSettingsProvider,
    ) -> Arc<dyn TrustAwareCapabilityDispatchAuthorizer> {
        profile_approval_authorizer(
            approval_policy,
            Arc::new(TestGatePolicy),
            Arc::new(settings),
        )
    }

    #[tokio::test]
    async fn dispatch_with_destructive_effect_requires_approval() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let descriptor = test_descriptor_with_id(shell_id.clone(), vec![EffectKind::SpawnProcess]);
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "destructive dispatch should require approval, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn spawn_with_dispatch_only_capability_requires_approval() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let echo_id = CapabilityId::new("builtin.echo").unwrap();
        let descriptor =
            test_descriptor_with_id(echo_id.clone(), vec![EffectKind::DispatchCapability]);
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: echo_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::DispatchCapability, EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_spawn_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "spawn of dispatch-only capability should require approval via SpawnProcess elevation, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn default_allow_dispatch_requires_approval_when_global_auto_approve_is_off() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let echo_id = CapabilityId::new("builtin.echo").unwrap();
        let descriptor =
            test_descriptor_with_id(echo_id.clone(), vec![EffectKind::DispatchCapability]);
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: echo_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::DispatchCapability],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "default-allow dispatch should still ask when global auto-approve is off, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn settings_always_allow_skips_default_allow_dispatch_gate_without_loaded_grant() {
        let authorizer = test_authorizer_with_settings(
            ApprovalPolicy::AskDestructive,
            StubSettingsProvider {
                tool_override: None,
                global_auto_approve: false,
                tool_always_allow: true,
            },
        );

        let echo_id = CapabilityId::new("builtin.echo").unwrap();
        let descriptor =
            test_descriptor_with_id(echo_id.clone(), vec![EffectKind::DispatchCapability]);
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: echo_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::DispatchCapability],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::Allow { .. }),
            "settings always-allow should skip the default-allow dispatch gate without a preloaded persistent grant, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn global_auto_approve_does_not_skip_manifest_ineligible_tool() {
        let authorizer = test_authorizer_with_settings(
            ApprovalPolicy::AskDestructive,
            StubSettingsProvider {
                tool_override: None,
                global_auto_approve: true,
                tool_always_allow: false,
            },
        );

        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let mut descriptor =
            test_descriptor_with_id(shell_id.clone(), vec![EffectKind::SpawnProcess]);
        descriptor.default_permission = PermissionMode::Deny;
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "global auto-approve must not bypass a tool whose manifest no longer permits durable approval, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn settings_persistent_grant_does_not_skip_manifest_ineligible_tool() {
        let authorizer = test_authorizer_with_settings(
            ApprovalPolicy::AskDestructive,
            StubSettingsProvider {
                tool_override: None,
                global_auto_approve: false,
                tool_always_allow: true,
            },
        );

        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let mut descriptor =
            test_descriptor_with_id(shell_id.clone(), vec![EffectKind::SpawnProcess]);
        descriptor.default_permission = PermissionMode::Deny;
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "settings persistent grant must not bypass a tool whose manifest no longer permits durable approval, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn legacy_persistent_grant_does_not_override_global_off() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let echo_id = CapabilityId::new("builtin.echo").unwrap();
        let provider = ExtensionId::new("builtin").unwrap();
        let descriptor =
            test_descriptor_with_id(echo_id.clone(), vec![EffectKind::DispatchCapability]);
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: echo_id,
                grantee: Principal::Extension(provider),
                issued_by: persistent_approval_grant_issuer(),
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::DispatchCapability],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "legacy persistent grant must not bypass Follow global when global auto-approve is off, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn minimal_policy_still_asks_when_global_auto_approve_is_off() {
        let authorizer = test_authorizer(ApprovalPolicy::Minimal);

        let descriptor = test_descriptor(vec![EffectKind::SpawnProcess]);
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: CapabilityId::new("builtin.shell").unwrap(),
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "Minimal policy should still ask when global auto-approve is off, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn user_issued_one_shot_approval_grant_allows_resume() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let descriptor = test_descriptor_with_id(shell_id.clone(), vec![EffectKind::SpawnProcess]);
        let base_ctx = test_context(CapabilitySet { grants: vec![] });
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(base_ctx.extension_id.clone()),
                issued_by: Principal::User(base_ctx.user_id.clone()),
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: Some(1),
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::Allow { .. }),
            "same-user one-shot approval lease should satisfy the standalone gate, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn persistent_approval_grant_allows_reuse() {
        let authorizer = test_authorizer_with_settings(
            ApprovalPolicy::AskDestructive,
            StubSettingsProvider {
                tool_override: None,
                global_auto_approve: false,
                tool_always_allow: true,
            },
        );

        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let descriptor = test_descriptor_with_id(shell_id.clone(), vec![EffectKind::SpawnProcess]);
        let base_ctx = test_context(CapabilitySet { grants: vec![] });
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(base_ctx.extension_id.clone()),
                issued_by: persistent_approval_grant_issuer(),
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::Allow { .. }),
            "settings persistent approval grant should satisfy the standalone gate, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn user_issued_persistent_like_grant_does_not_allow_reuse() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let descriptor = test_descriptor_with_id(shell_id.clone(), vec![EffectKind::SpawnProcess]);
        let base_ctx = test_context(CapabilitySet { grants: vec![] });
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(base_ctx.extension_id.clone()),
                issued_by: Principal::User(base_ctx.user_id.clone()),
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "standing user grant must not impersonate persistent approval replay, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn other_user_issued_persistent_like_grant_does_not_allow_reuse() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let descriptor = test_descriptor_with_id(shell_id.clone(), vec![EffectKind::SpawnProcess]);
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::User(
                    ironclaw_host_api::ids::UserId::new("other-user").unwrap(),
                ),
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: None,
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "different-user standing grant must not impersonate persistent approval replay, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn expired_persistent_approval_grant_does_not_allow_reuse() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let descriptor = test_descriptor_with_id(shell_id.clone(), vec![EffectKind::SpawnProcess]);
        let base_ctx = test_context(CapabilitySet { grants: vec![] });
        let ctx = test_context(CapabilitySet {
            grants: vec![
                CapabilityGrant {
                    id: CapabilityGrantId::new(),
                    capability: shell_id.clone(),
                    grantee: Principal::Extension(base_ctx.extension_id.clone()),
                    issued_by: Principal::HostRuntime,
                    constraints: GrantConstraints {
                        allowed_effects: vec![EffectKind::SpawnProcess],
                        mounts: MountView::default(),
                        network: NetworkPolicy::default(),
                        secrets: Vec::new(),
                        resource_ceiling: None,
                        expires_at: None,
                        max_invocations: None,
                    },
                },
                CapabilityGrant {
                    id: CapabilityGrantId::new(),
                    capability: shell_id,
                    grantee: Principal::Extension(base_ctx.extension_id.clone()),
                    issued_by: persistent_approval_grant_issuer(),
                    constraints: GrantConstraints {
                        allowed_effects: vec![EffectKind::SpawnProcess],
                        mounts: MountView::default(),
                        network: NetworkPolicy::default(),
                        secrets: Vec::new(),
                        resource_ceiling: None,
                        expires_at: Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
                        max_invocations: None,
                    },
                },
            ],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "expired persistent approval grant must not satisfy the standalone gate, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn other_user_issued_approval_grant_does_not_allow_resume() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let shell_id = CapabilityId::new("builtin.shell").unwrap();
        let descriptor = test_descriptor_with_id(shell_id.clone(), vec![EffectKind::SpawnProcess]);
        let ctx = test_context(CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: shell_id,
                grantee: Principal::Extension(ExtensionId::new("builtin").unwrap()),
                issued_by: Principal::User(
                    ironclaw_host_api::ids::UserId::new("other-user").unwrap(),
                ),
                constraints: GrantConstraints {
                    allowed_effects: vec![EffectKind::SpawnProcess],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: Some(1),
                },
            }],
        });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::RequireApproval { .. }),
            "different-user approval lease must not satisfy the standalone gate, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn deny_decision_passes_through_unchanged() {
        let authorizer = test_authorizer(ApprovalPolicy::AskDestructive);

        let descriptor = test_descriptor(vec![EffectKind::DispatchCapability]);
        let ctx = test_context(CapabilitySet { grants: vec![] });
        let decision = authorizer
            .authorize_dispatch_with_trust(
                &ctx,
                &descriptor,
                &ResourceEstimate::default(),
                &test_trust_decision(),
            )
            .await;

        assert!(
            matches!(decision, Decision::Deny { .. }),
            "ungranted capability should return Deny unchanged, got {decision:?}"
        );
    }

    #[test]
    fn approval_request_reason_includes_capability_id() {
        let descriptor = test_descriptor(vec![EffectKind::SpawnProcess]);
        let ctx = test_context(CapabilitySet { grants: vec![] });
        let req = approval_request(
            &ctx,
            &descriptor,
            &ResourceEstimate::default(),
            ProfileApprovalActionKind::Dispatch,
        );

        assert!(
            req.reason.contains("builtin.shell"),
            "reason should contain capability id, got: {:?}",
            req.reason
        );
        assert!(
            req.reason.contains("Dispatch"),
            "reason should contain action kind, got: {:?}",
            req.reason
        );
    }

    /// S4 (§5.2.1): the origin→gate matrix fold, driven through the REAL
    /// `RuntimeProfileApprovalGatePolicy` + `profile_approval_authorizer` (not a
    /// leaf). Proves (a) behavior-neutrality for `LoopRun` across profiles, and
    /// (b) that the fold demonstrably works with crafted matrices.
    mod origin_gate_matrix_fold {
        use ironclaw_host_api::{
            capability::OriginGatePolicy,
            ids::{ProductKind, RunId},
        };
        use ironclaw_runtime_policy::MinimalApprovalBypass;

        use super::*;
        use crate::profile_gate_policy::{
            RuntimeProfileApprovalGateEffectSets, RuntimeProfileApprovalGatePolicy,
        };

        /// The gated effect set today's `AskDestructive`/`AskWrites` gate uses
        /// (mirrors `builtin_capability_policy.toml`: everything except
        /// `read_filesystem` and `dispatch_capability`).
        fn gated_effect_set() -> Vec<EffectKind> {
            vec![
                EffectKind::WriteFilesystem,
                EffectKind::DeleteFilesystem,
                EffectKind::SpawnProcess,
                EffectKind::ExecuteCode,
                EffectKind::Network,
                EffectKind::UseSecret,
                EffectKind::ModifyExtension,
                EffectKind::ModifyApproval,
                EffectKind::ModifyBudget,
                EffectKind::ExternalWrite,
                EffectKind::Financial,
            ]
        }

        /// The production gate policy, so the fold is exercised through the real
        /// `origin_gate_requirement` impl (matrix mapping + Minimal-bypass
        /// suppression), not the test double.
        fn real_gate_policy(bypass: MinimalApprovalBypass) -> Arc<dyn ProfileApprovalGatePolicy> {
            let set = gated_effect_set();
            Arc::new(RuntimeProfileApprovalGatePolicy::new(
                bypass,
                RuntimeProfileApprovalGateEffectSets::new(set.clone(), set),
            ))
        }

        /// The four (ApprovalPolicy, MinimalApprovalBypass) pairs the neutrality
        /// claim covers: three non-Minimal profiles plus Minimal-yolo.
        fn profiles() -> Vec<(ApprovalPolicy, MinimalApprovalBypass)> {
            vec![
                (
                    ApprovalPolicy::AskDestructive,
                    MinimalApprovalBypass::Denied,
                ),
                (ApprovalPolicy::AskAlways, MinimalApprovalBypass::Denied),
                (ApprovalPolicy::AskWrites, MinimalApprovalBypass::Denied),
                (ApprovalPolicy::Minimal, MinimalApprovalBypass::Allowed),
            ]
        }

        fn descriptor(
            id: &str,
            effects: Vec<EffectKind>,
            permission: PermissionMode,
            matrix: Option<OriginGateMatrix>,
        ) -> CapabilityDescriptor {
            let mut d = test_descriptor_with_id(CapabilityId::new(id).unwrap(), effects);
            d.default_permission = permission;
            d.origin_gate_matrix = matrix;
            d
        }

        fn decision_kind(decision: &Decision) -> &'static str {
            match decision {
                Decision::Allow { .. } => "allow",
                Decision::Deny { .. } => "deny",
                Decision::RequireApproval { .. } => "require_approval",
            }
        }

        /// Drive the real authorizer for `descriptor` under `origin`, with a
        /// granting lease so the base decision is `Allow` (the gate only ever
        /// upgrades an `Allow`). Fixed default settings (global auto-approve off,
        /// no override, no always-allow) so any decision difference is
        /// attributable to the matrix fold, not class-B state.
        async fn decide(
            approval_policy: ApprovalPolicy,
            bypass: MinimalApprovalBypass,
            descriptor: &CapabilityDescriptor,
            origin: Option<InvocationOrigin>,
        ) -> Decision {
            decide_with_settings(
                approval_policy,
                bypass,
                descriptor,
                origin,
                StubSettingsProvider {
                    tool_override: None,
                    global_auto_approve: false,
                    tool_always_allow: false,
                },
            )
            .await
        }

        /// As [`decide`], but with caller-chosen class-B settings — used to prove
        /// the `AskAlways` hard-floor tier beats an always-allow/auto-approve that
        /// would otherwise resolve the decision to `Allow` at class-B.
        async fn decide_with_settings(
            approval_policy: ApprovalPolicy,
            bypass: MinimalApprovalBypass,
            descriptor: &CapabilityDescriptor,
            origin: Option<InvocationOrigin>,
            settings: StubSettingsProvider,
        ) -> Decision {
            let mut ctx = test_context(CapabilitySet {
                grants: vec![CapabilityGrant {
                    id: CapabilityGrantId::new(),
                    capability: descriptor.id.clone(),
                    grantee: Principal::Extension(descriptor.provider.clone()),
                    issued_by: Principal::HostRuntime,
                    constraints: GrantConstraints {
                        allowed_effects: descriptor.effects.clone(),
                        mounts: MountView::default(),
                        network: NetworkPolicy::default(),
                        secrets: Vec::new(),
                        resource_ceiling: None,
                        expires_at: None,
                        max_invocations: None,
                    },
                }],
            });
            ctx.origin = origin;
            let trust = TrustDecision {
                effective_trust: EffectiveTrustClass::user_trusted(),
                authority_ceiling: AuthorityCeiling {
                    allowed_effects: descriptor.effects.clone(),
                    max_resource_ceiling: None,
                },
                provenance: TrustProvenance::AdminConfig,
                evaluated_at: chrono::Utc::now(),
            };
            profile_approval_authorizer(
                approval_policy,
                real_gate_policy(bypass),
                Arc::new(settings),
            )
            .authorize_dispatch_with_trust(&ctx, descriptor, &ResourceEstimate::default(), &trust)
            .await
        }

        /// NEUTRALITY: for representative production caps, consulting the matrix
        /// for a `LoopRun` origin yields the IDENTICAL decision as not consulting
        /// it (an origin-less context, which reproduces the pre-S4 effect-only
        /// path), under AskDestructive, AskAlways, AskWrites, and Minimal-yolo.
        /// Covered per-cap at BOTH `default_permission` postures: the
        /// durable-eligible `Allow` (class-B dominated) and the ineligible `Deny`
        /// (reaches the step-9 effect/matrix compose), so the OR fold is proven
        /// neutral exactly where it can bite.
        #[tokio::test]
        async fn matrix_fold_is_behavior_neutral_for_loop_run() {
            // (id, effects, matrix loop_run policy) — matrices are the S3 seed:
            // read_file Ungated; write_file/http/gmail(read+write) GatedUnlessGranted.
            let caps: Vec<(&str, Vec<EffectKind>)> = vec![
                ("builtin.read_file", vec![EffectKind::ReadFilesystem]),
                ("builtin.write_file", vec![EffectKind::WriteFilesystem]),
                (
                    "builtin.http",
                    vec![EffectKind::DispatchCapability, EffectKind::Network],
                ),
                (
                    "gmail.messages_list",
                    vec![EffectKind::Network, EffectKind::UseSecret],
                ),
                (
                    "gmail.messages_send",
                    vec![
                        EffectKind::Network,
                        EffectKind::UseSecret,
                        EffectKind::ExternalWrite,
                    ],
                ),
            ];
            for (id, effects) in caps {
                let matrix = OriginGateMatrix::builtin_loop_run_seed(id);
                for permission in [PermissionMode::Allow, PermissionMode::Deny] {
                    for (approval_policy, bypass) in profiles() {
                        // With the matrix consulted (LoopRun origin, matrix declared).
                        let with = decide(
                            approval_policy,
                            bypass,
                            &descriptor(id, effects.clone(), permission, Some(matrix.clone())),
                            Some(InvocationOrigin::LoopRun(RunId::new())),
                        )
                        .await;
                        // Without the matrix consulted (no origin -> no contribution:
                        // exactly the pre-S4 effect-only decision).
                        let without = decide(
                            approval_policy,
                            bypass,
                            &descriptor(id, effects.clone(), permission, None),
                            None,
                        )
                        .await;
                        assert_eq!(
                            decision_kind(&with),
                            decision_kind(&without),
                            "matrix fold changed the decision for {id} \
                             (permission={permission:?}, policy={approval_policy:?}): \
                             with={with:?} without={without:?}"
                        );
                    }
                }
            }
        }

        /// MECHANISM (hard-floor tier — the key §5.2.7 proof): a crafted
        /// `loop_run: AskAlways` matrix gates EVEN when both `global_auto_approve`
        /// and per-tool `always_allow` are on (class-B would otherwise resolve the
        /// decision to `Allow`) under a non-Minimal profile. This proves the
        /// AskAlways gate composes at the hard-floor step, ahead of class-B, and
        /// is not bypassable by a stored auto-approve/always-allow.
        #[tokio::test]
        async fn ask_always_matrix_gates_even_when_class_b_would_allow() {
            let matrix = OriginGateMatrix {
                loop_run: OriginGatePolicy::AskAlways,
                product: OriginGatePolicy::Forbidden,
                automation: OriginGatePolicy::Forbidden,
            };
            // Durable-eligible (`Allow`) + auto-approve ON + always-allow ON: class-B
            // (steps 6/7) would return `Allow` for any soft-gated cap. NO gating
            // effect, so the effect gate is false — AskAlways is the only gate.
            let cap = descriptor(
                "builtin.effectless",
                vec![EffectKind::DispatchCapability],
                PermissionMode::Allow,
                Some(matrix),
            );
            let bypassing_settings = || StubSettingsProvider {
                tool_override: None,
                global_auto_approve: true,
                tool_always_allow: true,
            };
            let gated = decide_with_settings(
                ApprovalPolicy::AskDestructive,
                MinimalApprovalBypass::Denied,
                &cap,
                Some(InvocationOrigin::LoopRun(RunId::new())),
                bypassing_settings(),
            )
            .await;
            assert!(
                matches!(gated, Decision::RequireApproval { .. }),
                "AskAlways must gate even with auto-approve + always-allow on, got {gated:?}"
            );
            // Same class-B-bypassing settings, but no matrix consulted (origin-less):
            // class-B auto-approve/always-allow resolves it to `Allow`. Isolates the
            // AskAlways hard floor as the sole cause of the gate above.
            let allowed = decide_with_settings(
                ApprovalPolicy::AskDestructive,
                MinimalApprovalBypass::Denied,
                &descriptor(
                    "builtin.effectless",
                    vec![EffectKind::DispatchCapability],
                    PermissionMode::Allow,
                    None,
                ),
                None,
                bypassing_settings(),
            )
            .await;
            assert!(
                matches!(allowed, Decision::Allow { .. }),
                "without the AskAlways matrix, class-B auto-approve/always-allow allows, got {allowed:?}"
            );
        }

        /// MECHANISM (hard-floor tier is NOT yolo-suppressed): an `AskAlways`
        /// matrix gates even under Minimal-yolo, unlike the soft
        /// `GatedUnlessGranted` tier (see
        /// `minimal_yolo_suppresses_gated_unless_granted_matrix`). Mirrors
        /// `effects_force_approval`, which also fires under yolo.
        #[tokio::test]
        async fn ask_always_matrix_gates_under_minimal_yolo() {
            let matrix = OriginGateMatrix {
                loop_run: OriginGatePolicy::AskAlways,
                product: OriginGatePolicy::Forbidden,
                automation: OriginGatePolicy::Forbidden,
            };
            // NO gating effect, so the effect gate is false under Minimal-yolo —
            // AskAlways is the only possible gate, isolating "not suppressed".
            let cap = descriptor(
                "builtin.effectless",
                vec![EffectKind::DispatchCapability],
                PermissionMode::Allow,
                Some(matrix),
            );
            let decision = decide_with_settings(
                ApprovalPolicy::Minimal,
                MinimalApprovalBypass::Allowed,
                &cap,
                Some(InvocationOrigin::LoopRun(RunId::new())),
                // Even auto-approve + always-allow (the yolo default posture) must
                // not bypass the AskAlways hard floor.
                StubSettingsProvider {
                    tool_override: None,
                    global_auto_approve: true,
                    tool_always_allow: true,
                },
            )
            .await;
            assert!(
                matches!(decision, Decision::RequireApproval { .. }),
                "AskAlways is a hard floor and must gate even under Minimal-yolo, got {decision:?}"
            );
        }

        /// MECHANISM: a `product: Forbidden` matrix + a `Product` origin is a hard
        /// deny (sanitized `PolicyDenied`), even though the base decision is an
        /// `Allow`.
        #[tokio::test]
        async fn forbidden_product_origin_is_denied() {
            // The S3 builtin seed sets product = Forbidden for every cap.
            let cap = descriptor(
                "builtin.read_file",
                vec![EffectKind::ReadFilesystem],
                PermissionMode::Allow,
                Some(OriginGateMatrix::builtin_loop_run_seed("builtin.read_file")),
            );
            let decision = decide(
                ApprovalPolicy::AskDestructive,
                MinimalApprovalBypass::Denied,
                &cap,
                Some(InvocationOrigin::Product(
                    ProductKind::new("settings").unwrap(),
                )),
            )
            .await;
            assert!(
                matches!(
                    decision,
                    Decision::Deny {
                        reason: DenyReason::PolicyDenied
                    }
                ),
                "a Forbidden product origin must be denied, got {decision:?}"
            );
        }

        /// MECHANISM: `Ungated` (LoopRun) and `ConsentSufficient` (Product) add no
        /// gate — an effectless cap stays `Allow` under a non-Minimal profile.
        #[tokio::test]
        async fn ungated_and_consent_sufficient_add_no_gate() {
            let matrix = OriginGateMatrix {
                loop_run: OriginGatePolicy::Ungated,
                product: OriginGatePolicy::ConsentSufficient,
                automation: OriginGatePolicy::Forbidden,
            };
            let cap = descriptor(
                "builtin.effectless",
                vec![EffectKind::DispatchCapability],
                PermissionMode::Deny,
                Some(matrix),
            );
            for origin in [
                InvocationOrigin::LoopRun(RunId::new()),
                InvocationOrigin::Product(ProductKind::new("settings").unwrap()),
            ] {
                let decision = decide(
                    ApprovalPolicy::AskDestructive,
                    MinimalApprovalBypass::Denied,
                    &cap,
                    Some(origin.clone()),
                )
                .await;
                assert!(
                    matches!(decision, Decision::Allow { .. }),
                    "{} matrix policy must add no gate, got {decision:?}",
                    origin.kind()
                );
            }
        }

        /// MECHANISM (Minimal-bypass suppression, the critical S4 invariant):
        /// under Minimal-yolo a `GatedUnlessGranted` matrix does NOT re-introduce
        /// a prompt — the matrix gate is suppressed behind the same bypass guard
        /// the effect gate uses. The SAME descriptor under a non-Minimal profile
        /// DOES gate, proving the suppression (not the absence of a matrix) is
        /// what silences yolo.
        #[tokio::test]
        async fn minimal_yolo_suppresses_gated_unless_granted_matrix() {
            let matrix = OriginGateMatrix {
                loop_run: OriginGatePolicy::GatedUnlessGranted,
                product: OriginGatePolicy::Forbidden,
                automation: OriginGatePolicy::Forbidden,
            };
            // Effectless so the effect gate is false on both profiles: the matrix
            // is the only possible gate source, isolating the suppression.
            let cap = descriptor(
                "builtin.effectless",
                vec![EffectKind::DispatchCapability],
                PermissionMode::Deny,
                Some(matrix),
            );
            let origin = InvocationOrigin::LoopRun(RunId::new());
            let yolo = decide(
                ApprovalPolicy::Minimal,
                MinimalApprovalBypass::Allowed,
                &cap,
                Some(origin.clone()),
            )
            .await;
            assert!(
                matches!(yolo, Decision::Allow { .. }),
                "Minimal-yolo must suppress the GatedUnlessGranted matrix (no prompt), got {yolo:?}"
            );
            let non_minimal = decide(
                ApprovalPolicy::AskDestructive,
                MinimalApprovalBypass::Denied,
                &cap,
                Some(origin),
            )
            .await;
            assert!(
                matches!(non_minimal, Decision::RequireApproval { .. }),
                "the same GatedUnlessGranted matrix gates under a non-Minimal profile, got {non_minimal:?}"
            );
        }
    }

    /// Settings whose override store is unreadable or whose Full access comes
    /// from a given source.
    struct VerdictSettings {
        lookup: ToolOverrideLookup,
        source: FullAccessSource,
    }

    #[async_trait]
    impl ApprovalSettingsProvider for VerdictSettings {
        async fn tool_override(
            &self,
            _scope: &ResourceScope,
            _capability_id: &CapabilityId,
        ) -> Option<ToolPermissionOverride> {
            None
        }

        async fn tool_override_lookup(
            &self,
            _scope: &ResourceScope,
            _capability_id: &CapabilityId,
        ) -> ToolOverrideLookup {
            self.lookup
        }

        async fn global_auto_approve(&self, _scope: &ResourceScope) -> bool {
            true
        }

        async fn tool_always_allow(
            &self,
            _scope: &ResourceScope,
            _capability_id: &CapabilityId,
            _grantee: &Principal,
        ) -> bool {
            true
        }

        async fn full_access(&self, _scope: &ResourceScope) -> bool {
            self.source != FullAccessSource::Off
        }

        async fn full_access_source(&self, _scope: &ResourceScope) -> FullAccessSource {
            self.source
        }
    }

    async fn verdict(
        settings: VerdictSettings,
        approved: bool,
        inherited_full_access_eligible: bool,
    ) -> ApprovalGateVerdict {
        let scope = ResourceScope::local_default(
            ironclaw_host_api::ids::UserId::new("user-verdict").unwrap(),
            ironclaw_host_api::ids::InvocationId::new(),
        )
        .unwrap();
        let capability = CapabilityId::new("test.verdict").unwrap();
        approval_gate_verdict(
            &settings,
            ApprovalGateQuery {
                scope: &scope,
                capability_id: &capability,
                grantee: &Principal::HostRuntime,
                approved_for_this_invocation: approved,
                force_approval: true,
                exempt_from_approval: false,
                global_auto_approve_eligible: false,
                inherited_full_access_eligible,
                always_allow_eligible: false,
                approved_for_this_task: false,
                policy_requires_approval: true,
            },
        )
        .await
    }

    /// An unreadable override refuses ahead of every shortcut: an approved
    /// invocation and Full access both lose to it.
    #[tokio::test]
    async fn an_unreadable_override_refuses_before_approval_and_full_access() {
        for (approved, source) in [
            (true, FullAccessSource::Off),
            (false, FullAccessSource::Explicit),
            (true, FullAccessSource::InheritedGlobal),
        ] {
            assert_eq!(
                verdict(
                    VerdictSettings {
                        lookup: ToolOverrideLookup::Unavailable,
                        source,
                    },
                    approved,
                    true,
                )
                .await,
                ApprovalGateVerdict::SettingsUnavailable,
                "approved={approved}, full access={source:?}"
            );
        }
    }

    /// Full access inherited from the global switch counts only where the
    /// query allows it; an explicit Full access always does.
    #[tokio::test]
    async fn inherited_full_access_is_honored_only_where_eligible() {
        let settings = |source| VerdictSettings {
            lookup: ToolOverrideLookup::Unset,
            source,
        };
        assert_eq!(
            verdict(settings(FullAccessSource::InheritedGlobal), false, true).await,
            ApprovalGateVerdict::Allow
        );
        assert_eq!(
            verdict(settings(FullAccessSource::InheritedGlobal), false, false).await,
            ApprovalGateVerdict::RequireApproval
        );
        assert_eq!(
            verdict(settings(FullAccessSource::Explicit), false, false).await,
            ApprovalGateVerdict::Allow
        );
    }
}
