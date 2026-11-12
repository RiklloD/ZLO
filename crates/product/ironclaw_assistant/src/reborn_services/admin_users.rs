//! The admin user-management wire contract + the fail-closed default.
//!
//! The [`AdminUserService`] port and its record vocabulary moved to
//! `ironclaw_product_contracts::admin_users` (PROPOSAL §6.1.3): its only
//! production implementation is a `ironclaw_composition` adapter over
//! the identity user-directory, and `ironclaw_extension_host` reads the same
//! directory to resolve a channel actor's admin role. What stays here is
//! product's own surface: the `Reborn*` request/response types the WebChat v2
//! admin routes serialize, and the fail-closed default `RebornServices` wires
//! before composition installs the real adapter.

use super::*;

use std::collections::BTreeMap;

use async_trait::async_trait;
use ironclaw_host_api::ids::{SecretHandle, TenantId, UserId};
use ironclaw_product_contracts::admin_users::{
    AdminCreateUserFields, AdminCreatedUser, AdminUserError, AdminUserRecord, AdminUserRole,
    AdminUserSecretMeta, AdminUserService, AdminUserStatus,
};
use secrecy::SecretString;

pub use ironclaw_product_contracts::admin_users::{
    RebornAdminCreateUserRequest, RebornAdminDeleteSecretProductRequest,
    RebornAdminPutSecretProductRequest, RebornAdminPutSecretRequest,
    RebornAdminSecretDeletedResponse, RebornAdminSecretResponse, RebornAdminSetRoleProductRequest,
    RebornAdminSetRoleRequest, RebornAdminSetStatusProductRequest, RebornAdminSetStatusRequest,
    RebornAdminUpdateUserProductRequest, RebornAdminUpdateUserRequest,
    RebornAdminUserCreatedResponse, RebornAdminUserDeletedResponse, RebornAdminUserListQuery,
    RebornAdminUserListResponse, RebornAdminUserRequest, RebornAdminUserResponse,
    RebornAdminUserSecretsListResponse,
};

/// Fail-closed default wired into `RebornServices` before composition installs
/// the real adapter. Every operation reports the service unavailable, so a
/// deployment that never wires the admin surface serves 503s rather than
/// panicking or silently succeeding. Mirrors the `Rejecting*` default pattern
/// used for the other optional-but-live services on `RebornServices`.
pub(crate) struct RejectingAdminUserService;

#[async_trait]
impl AdminUserService for RejectingAdminUserService {
    async fn list_users(
        &self,
        _tenant: &TenantId,
        _status: Option<AdminUserStatus>,
        _after: Option<&UserId>,
        _limit: usize,
    ) -> Result<Vec<AdminUserRecord>, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn get_user(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
    ) -> Result<Option<AdminUserRecord>, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn create_user(
        &self,
        _tenant: &TenantId,
        _actor: &UserId,
        _fields: AdminCreateUserFields,
    ) -> Result<AdminCreatedUser, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn update_profile(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
        _display_name: Option<String>,
        _metadata: Option<BTreeMap<String, String>>,
    ) -> Result<AdminUserRecord, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn set_status(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
        _status: AdminUserStatus,
    ) -> Result<AdminUserRecord, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn set_role(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
        _role: AdminUserRole,
    ) -> Result<AdminUserRecord, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn delete_user(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
    ) -> Result<(), AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn count_active_admins(&self, _tenant: &TenantId) -> Result<usize, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn list_secrets(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
    ) -> Result<Vec<AdminUserSecretMeta>, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn put_secret(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
        _handle: SecretHandle,
        _material: SecretString,
    ) -> Result<AdminUserSecretMeta, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }

    async fn delete_secret(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
        _handle: SecretHandle,
    ) -> Result<bool, AdminUserError> {
        Err(AdminUserError::Unavailable)
    }
}

pub const ADMIN_USER_UPDATE_CAPABILITY_ID: &str = "builtin.admin_user_update";

pub const ADMIN_USER_UPDATE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(ADMIN_USER_UPDATE_CAPABILITY_ID);

pub const ADMIN_USER_SET_STATUS_CAPABILITY_ID: &str = "builtin.admin_user_set_status";

pub const ADMIN_USER_SET_STATUS_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(ADMIN_USER_SET_STATUS_CAPABILITY_ID);

pub const ADMIN_USER_SET_ROLE_CAPABILITY_ID: &str = "builtin.admin_user_set_role";

pub const ADMIN_USER_SET_ROLE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(ADMIN_USER_SET_ROLE_CAPABILITY_ID);

pub const ADMIN_USER_DELETE_CAPABILITY_ID: &str = "builtin.admin_user_delete";

pub const ADMIN_USER_DELETE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(ADMIN_USER_DELETE_CAPABILITY_ID);

pub const ADMIN_USER_PUT_SECRET_CAPABILITY_ID: &str = "builtin.admin_user_put_secret";

pub const ADMIN_USER_PUT_SECRET_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(ADMIN_USER_PUT_SECRET_CAPABILITY_ID);

pub const ADMIN_USER_DELETE_SECRET_CAPABILITY_ID: &str = "builtin.admin_user_delete_secret";

pub const ADMIN_USER_DELETE_SECRET_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(ADMIN_USER_DELETE_SECRET_CAPABILITY_ID);

pub const ADMIN_USER_CREATE_COMMAND_ID: &str = "admin.user.create";

pub const ADMIN_USER_CREATE_COMMAND: ProductSurfaceCommandDescriptor<
    RebornAdminCreateUserRequest,
    RebornAdminUserCreatedResponse,
> = ProductSurfaceCommandDescriptor::new(ADMIN_USER_CREATE_COMMAND_ID);

pub const ADMIN_USER_DELETE_SECRET_COMMAND_ID: &str = "admin.user.delete_secret";

pub const ADMIN_USER_DELETE_SECRET_COMMAND: ProductSurfaceCommandDescriptor<
    RebornAdminDeleteSecretProductRequest,
    RebornAdminSecretDeletedResponse,
> = ProductSurfaceCommandDescriptor::new(ADMIN_USER_DELETE_SECRET_COMMAND_ID);

pub const ADMIN_USERS_VIEW: ProductView<RebornAdminUserListQuery, RebornAdminUserListResponse> =
    ProductView::paginated("admin_users");

pub const ADMIN_USER_VIEW: ProductView<RebornAdminUserRequest, RebornAdminUserResponse> =
    ProductView::unpaginated("admin_user");

pub const ADMIN_USER_SECRETS_VIEW: ProductView<
    RebornAdminUserRequest,
    RebornAdminUserSecretsListResponse,
> = ProductView::unpaginated("admin_user_secrets");

/// Map the coarse admin-port error into the sanitized WebUI wire taxonomy.
pub(super) fn map_admin_user_error(error: AdminUserError) -> ProductSurfaceError {
    match error {
        AdminUserError::NotFound => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false)
        }
        // Client-supplied value is malformed (e.g. a bad secret handle) — a 400,
        // never a 500: the input is at fault, not the backend.
        AdminUserError::InvalidInput => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::InvalidRequest, 400, false)
        }
        // Transient backend failure — the browser may retry.
        AdminUserError::Unavailable => ProductSurfaceError::service_unavailable(true),
        AdminUserError::Internal => ProductSurfaceError::internal(),
    }
}

/// Stable last-admin-protection error: a 409 conflict carrying a `last_admin`
/// marker so the UI can render a specific message and tests can pin it.
pub(super) fn last_admin_error() -> ProductSurfaceError {
    ProductSurfaceError {
        code: ProductSurfaceErrorCode::Conflict,
        kind: ProductSurfaceErrorKind::Conflict,
        status_code: 409,
        retryable: false,
        field: Some("last_admin".to_string()),
        validation_code: None,
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    /// Authorize the caller for admin operations. An env-bearer operator is an
    /// implicit owner; otherwise the caller's persisted role must be admin or
    /// owner. The role is read from the directory on EVERY call (never cached),
    /// so a demoted admin loses access immediately — see
    /// this crate's `AGENTS.md` ("No caching. Caching the authz result is
    /// explicitly forbidden").
    pub(super) async fn authorize_admin(
        &self,
        caller: &ProductSurfaceCaller,
    ) -> Result<(), ProductSurfaceError> {
        if caller.operator_config {
            return Ok(());
        }
        let record = self
            .admin_users
            .get_user(&caller.tenant_id, &caller.user_id)
            .await
            .map_err(map_admin_user_error)?;
        match record {
            // Admin/owner role AND an active account. A suspended admin keeps
            // the role field but must not act: status gates authorization, so
            // suspending an admin immediately revokes their admin API access
            // (same "read on every call, never cache" contract as role).
            Some(user) if user.role.is_admin() && user.status == AdminUserStatus::Active => Ok(()),
            // "No record", "not admin", and "suspended admin" are all a 403: the
            // caller is authenticated but not authorized. Never leak which.
            _ => Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::Forbidden,
                403,
                false,
            )),
        }
    }

    /// Fetch the target user, mapping absence to a sanitized 404.
    pub(super) async fn require_admin_target(
        &self,
        tenant: &TenantId,
        user_id: &UserId,
    ) -> Result<AdminUserRecord, ProductSurfaceError> {
        self.admin_users
            .get_user(tenant, user_id)
            .await
            .map_err(map_admin_user_error)?
            .ok_or_else(|| {
                ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false)
            })
    }

    /// Reject a mutation that would strand the tenant without an admin.
    /// `target` is the user's CURRENT record; `still_admin_after` is whether the
    /// user remains an active admin once the mutation lands. Re-reads the
    /// active-admin count immediately before the decision as a TOCTOU guard
    /// (mirrors the `blocked_gate_state` re-read pattern).
    pub(super) async fn ensure_not_last_admin(
        &self,
        tenant: &TenantId,
        target: &AdminUserRecord,
        still_admin_after: bool,
    ) -> Result<(), ProductSurfaceError> {
        // Only a mutation that drops a currently-active admin below the line can
        // strand the tenant. If the target is not now an active admin, or stays
        // one, there is nothing to protect.
        if still_admin_after || target.status != AdminUserStatus::Active || !target.role.is_admin()
        {
            return Ok(());
        }
        let active_admins = self
            .admin_users
            .count_active_admins(tenant)
            .await
            .map_err(map_admin_user_error)?;
        if active_admins <= 1 {
            return Err(last_admin_error());
        }
        Ok(())
    }
    pub async fn list_admin_users(
        &self,
        caller: ProductSurfaceCaller,
        query: RebornAdminUserListQuery,
    ) -> Result<RebornAdminUserListResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        // Bound the page: clamp the caller's `limit` and parse the opaque
        // cursor into a `UserId`. A malformed cursor is caller input at fault
        // (it should only ever be a value we minted), so it is a 400.
        let limit = query
            .limit
            .map(|value| value as usize)
            .unwrap_or(ADMIN_USER_LIST_DEFAULT_LIMIT)
            .clamp(1, ADMIN_USER_LIST_MAX_LIMIT);
        let after = match query.cursor.as_deref() {
            Some(raw) => Some(UserId::new(raw).map_err(|_| {
                ProductSurfaceError::from_status(
                    ProductSurfaceErrorCode::InvalidRequest,
                    400,
                    false,
                )
            })?),
            None => None,
        };
        let users = self
            .admin_users
            .list_users(&caller.tenant_id, query.status, after.as_ref(), limit)
            .await
            .map_err(map_admin_user_error)?;
        // A full page means there may be more rows past it; hand back the last
        // id as the next cursor. A short page is the end of the tenant's users.
        let next_cursor = (users.len() == limit)
            .then(|| users.last().map(|user| user.user_id.as_str().to_string()))
            .flatten();
        Ok(RebornAdminUserListResponse { users, next_cursor })
    }

    pub async fn get_admin_user(
        &self,
        caller: ProductSurfaceCaller,
        user_id: UserId,
    ) -> Result<RebornAdminUserResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        let user = self
            .require_admin_target(&caller.tenant_id, &user_id)
            .await?;
        Ok(RebornAdminUserResponse { user })
    }

    pub async fn create_admin_user(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornAdminCreateUserRequest,
    ) -> Result<RebornAdminUserCreatedResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        let created = self
            .admin_users
            .create_user(
                &caller.tenant_id,
                &caller.user_id,
                AdminCreateUserFields {
                    email: request.email,
                    display_name: request.display_name,
                    role: request.role,
                },
            )
            .await
            .map_err(map_admin_user_error)?;
        Ok(RebornAdminUserCreatedResponse {
            user: created.record,
            // Exposed exactly once, here. The DTO carries it in no other path.
            api_token: created.api_token.expose_secret().to_string(),
        })
    }

    pub async fn update_admin_user(
        &self,
        caller: ProductSurfaceCaller,
        user_id: UserId,
        request: RebornAdminUpdateUserRequest,
    ) -> Result<RebornAdminUserResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        // Surface a 404 before attempting the mutation.
        self.require_admin_target(&caller.tenant_id, &user_id)
            .await?;
        let user = self
            .admin_users
            .update_profile(
                &caller.tenant_id,
                &user_id,
                request.display_name,
                request.metadata,
            )
            .await
            .map_err(map_admin_user_error)?;
        Ok(RebornAdminUserResponse { user })
    }

    pub async fn set_admin_user_status(
        &self,
        caller: ProductSurfaceCaller,
        user_id: UserId,
        request: RebornAdminSetStatusRequest,
    ) -> Result<RebornAdminUserResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        // Serialize with concurrent role/status/delete on this tenant so the
        // last-admin count read below reflects any in-flight demotion.
        let _admin_guard = self.lock_admin_mutation(&caller.tenant_id).await;
        let target = self
            .require_admin_target(&caller.tenant_id, &user_id)
            .await?;
        // Activating keeps/raises an admin; suspending drops one.
        let still_admin_after = matches!(request.status, AdminUserStatus::Active);
        self.ensure_not_last_admin(&caller.tenant_id, &target, still_admin_after)
            .await?;
        let user = self
            .admin_users
            .set_status(&caller.tenant_id, &user_id, request.status)
            .await
            .map_err(map_admin_user_error)?;
        Ok(RebornAdminUserResponse { user })
    }

    pub async fn set_admin_user_role(
        &self,
        caller: ProductSurfaceCaller,
        user_id: UserId,
        request: RebornAdminSetRoleRequest,
    ) -> Result<RebornAdminUserResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        // Serialize with concurrent role/status/delete on this tenant so the
        // last-admin count read below reflects any in-flight demotion.
        let _admin_guard = self.lock_admin_mutation(&caller.tenant_id).await;
        let target = self
            .require_admin_target(&caller.tenant_id, &user_id)
            .await?;
        let still_admin_after = request.role.is_admin();
        self.ensure_not_last_admin(&caller.tenant_id, &target, still_admin_after)
            .await?;
        let user = self
            .admin_users
            .set_role(&caller.tenant_id, &user_id, request.role)
            .await
            .map_err(map_admin_user_error)?;
        Ok(RebornAdminUserResponse { user })
    }

    pub async fn delete_admin_user(
        &self,
        caller: ProductSurfaceCaller,
        user_id: UserId,
    ) -> Result<RebornAdminUserDeletedResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        // Serialize with concurrent role/status/delete on this tenant so the
        // last-admin count read below reflects any in-flight demotion.
        let _admin_guard = self.lock_admin_mutation(&caller.tenant_id).await;
        let target = self
            .require_admin_target(&caller.tenant_id, &user_id)
            .await?;
        // Deletion always removes the user, so it can never leave them an admin.
        self.ensure_not_last_admin(&caller.tenant_id, &target, false)
            .await?;
        self.admin_users
            .delete_user(&caller.tenant_id, &user_id)
            .await
            .map_err(map_admin_user_error)?;
        Ok(RebornAdminUserDeletedResponse {
            user_id,
            deleted: true,
        })
    }

    pub async fn list_admin_user_secrets(
        &self,
        caller: ProductSurfaceCaller,
        user_id: UserId,
    ) -> Result<RebornAdminUserSecretsListResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        self.require_admin_target(&caller.tenant_id, &user_id)
            .await?;
        let secrets = self
            .admin_users
            .list_secrets(&caller.tenant_id, &user_id)
            .await
            .map_err(map_admin_user_error)?;
        Ok(RebornAdminUserSecretsListResponse { secrets })
    }

    pub async fn put_admin_user_secret(
        &self,
        caller: ProductSurfaceCaller,
        user_id: UserId,
        handle: SecretHandle,
        request: RebornAdminPutSecretRequest,
    ) -> Result<RebornAdminSecretResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        self.require_admin_target(&caller.tenant_id, &user_id)
            .await?;
        let secret = self
            .admin_users
            .put_secret(
                &caller.tenant_id,
                &user_id,
                handle,
                SecretString::from(request.value),
            )
            .await
            .map_err(map_admin_user_error)?;
        Ok(RebornAdminSecretResponse { secret })
    }

    pub async fn delete_admin_user_secret(
        &self,
        caller: ProductSurfaceCaller,
        user_id: UserId,
        handle: SecretHandle,
    ) -> Result<RebornAdminSecretDeletedResponse, ProductSurfaceError> {
        self.authorize_admin(&caller).await?;
        self.require_admin_target(&caller.tenant_id, &user_id)
            .await?;
        // Echo the parsed, canonical handle back on the wire as a plain string.
        let handle_str = handle.as_str().to_string();
        let deleted = self
            .admin_users
            .delete_secret(&caller.tenant_id, &user_id, handle)
            .await
            .map_err(map_admin_user_error)?;
        Ok(RebornAdminSecretDeletedResponse {
            handle: handle_str,
            deleted,
        })
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker,
    V: RebornViewProvider,
{
    /// Per-tenant lock serializing admin mutations that affect the active-admin
    /// count (role/status/delete). `ensure_not_last_admin` re-reads the count
    /// then mutates; without serialization two concurrent demotions each see
    /// "2 admins", both pass, and both land — stranding the tenant with zero
    /// admins (a TOCTOU race). Holding this across the check+mutation makes the
    /// count read authoritative. Reuses the same weak-ref keyed registry as
    /// `thread_operation_lock`, namespaced so the keyspaces cannot collide.
    ///
    /// Scope of the guarantee: this lock lives in the current `RebornServices`
    /// instance, so it serializes every admin mutation within one process. The
    /// standalone `ironclaw-reborn serve` binary is single-process, so last-
    /// admin protection is airtight there. It does NOT span multiple runtime
    /// instances sharing one identity filesystem (a not-yet-supported multi-
    /// replica deployment): two processes each hold their own lock and could
    /// both read `active_admins > 1` before demoting different admins. Closing
    /// that requires a durable per-tenant lease (a CAS-guarded lock record in
    /// the identity store) shared by all instances — deferred until a multi-
    /// replica deployment mode exists, since a hand-rolled filesystem lease adds
    /// crash-recovery/stale-takeover risk that outweighs the bounded race it
    /// would replace in the single-process product shipping today.
    pub(super) async fn lock_admin_mutation(&self, tenant: &TenantId) -> OwnedMutexGuard<()> {
        let key = format!("admin-mutation:{}", tenant.as_str());
        let lock = {
            let mut locks = match self.thread_operation_locks.lock() {
                Ok(locks) => locks,
                Err(poisoned) => poisoned.into_inner(),
            };
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(AsyncMutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
}
