//! Which scope a paired device needs for a route.
//!
//! The bearer middleware checks every request a device token makes against
//! [`required_scope`]; the master token and sessions keep full access. The
//! classification is by path family and method, so a new route inherits a
//! sensible scope (reads need `orchestration:read`, other mutations
//! `orchestration:operate`) instead of being reachable by every device.

use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use ironclaw_product_contracts::environment_access::{DeviceGrant, DeviceScope};
use serde_json::json;

/// The authenticated caller's device grant (`None` for non-device bearers,
/// which have full access). Inserted on every protected request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerGrant(pub Option<DeviceGrant>);

impl CallerGrant {
    /// Whether the caller may use `scope`.
    pub fn allows(&self, scope: DeviceScope) -> bool {
        self.0.as_ref().is_none_or(|grant| grant.allows(scope))
    }
}

const SETTINGS_FAMILIES: &[&str] = &[
    "settings",
    "extensions",
    "llm",
    "skills",
    "profiles",
    "admin",
    "channels",
    "hooks",
    "providers",
    "notifications",
    "session-imports",
    "ironhub",
];

/// Operator sub-paths that change configuration rather than run maintenance.
/// `config-sync` reads and writes configuration (and, when asked, provider
/// keys): its `export` and `apply` are POSTs, so both need
/// `settings:manage`; only the hash manifest is a read.
const OPERATOR_SETTINGS: &[&str] = &["config", "config-sync", "extension-configuration", "setup"];

/// The scope a device needs for `method path`, or `None` when the route
/// checks access itself (the peer routes).
pub fn required_scope(method: &Method, path: &str) -> Option<DeviceScope> {
    let read = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if path.starts_with("/api/peer/") || path == "/api/mcp" {
        return None;
    }
    if path.starts_with("/api/auth/") || path.starts_with("/api/peer-links") {
        return Some(DeviceScope::AccessManage);
    }
    if path.starts_with("/api/agent-requests") {
        return Some(if read {
            DeviceScope::OrchestrationRead
        } else {
            DeviceScope::OrchestrationOperate
        });
    }
    if path.starts_with("/api/operator/") {
        return Some(DeviceScope::EnvironmentManage);
    }
    if let Some(rest) = path.strip_prefix("/api/webchat/v2/") {
        let mut parts = rest.split('/');
        let family = parts.next().unwrap_or_default();
        match family {
            "operator" => {
                let sub = parts.next().unwrap_or_default();
                return Some(if OPERATOR_SETTINGS.contains(&sub) && !read {
                    DeviceScope::SettingsManage
                } else if OPERATOR_SETTINGS.contains(&sub) {
                    DeviceScope::OrchestrationRead
                } else {
                    DeviceScope::EnvironmentManage
                });
            }
            "fs" => {
                return Some(if read {
                    DeviceScope::FilesystemRead
                } else {
                    DeviceScope::FilesystemWrite
                });
            }
            "threads" if rest.split('/').nth(2) == Some("console") => {
                return Some(if read {
                    DeviceScope::OrchestrationRead
                } else {
                    DeviceScope::TerminalOperate
                });
            }
            family if SETTINGS_FAMILIES.contains(&family) && !read => {
                return Some(DeviceScope::SettingsManage);
            }
            _ => {}
        }
    }
    Some(if read {
        DeviceScope::OrchestrationRead
    } else {
        DeviceScope::OrchestrationOperate
    })
}

/// `403 scope_required`, naming the missing scope.
pub fn scope_forbidden(scope: DeviceScope) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({
            "error": "scope_required",
            "scope": scope.as_str(),
            "message": format!(
                "This device was paired without {scope}. Pair it again with that permission."
            ),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_map_to_the_scope_that_guards_them() {
        let cases = [
            (
                Method::GET,
                "/api/webchat/v2/threads",
                DeviceScope::OrchestrationRead,
            ),
            (
                Method::GET,
                "/api/webchat/v2/inbox/events",
                DeviceScope::OrchestrationRead,
            ),
            (
                Method::POST,
                "/api/webchat/v2/threads/t1/messages",
                DeviceScope::OrchestrationOperate,
            ),
            (
                Method::POST,
                "/api/webchat/v2/threads/t1/console",
                DeviceScope::TerminalOperate,
            ),
            (
                Method::GET,
                "/api/webchat/v2/fs/list",
                DeviceScope::FilesystemRead,
            ),
            (
                Method::PUT,
                "/api/webchat/v2/fs/content",
                DeviceScope::FilesystemWrite,
            ),
            (
                Method::PUT,
                "/api/webchat/v2/operator/config/provider.default",
                DeviceScope::SettingsManage,
            ),
            (
                Method::GET,
                "/api/webchat/v2/operator/logs",
                DeviceScope::EnvironmentManage,
            ),
            (
                Method::POST,
                "/api/webchat/v2/extensions/slack/install",
                DeviceScope::SettingsManage,
            ),
            (
                Method::GET,
                "/api/webchat/v2/extensions",
                DeviceScope::OrchestrationRead,
            ),
            (
                Method::POST,
                "/api/auth/pairing-codes",
                DeviceScope::AccessManage,
            ),
            (Method::POST, "/api/peer-links", DeviceScope::AccessManage),
            (
                Method::POST,
                "/api/operator/update",
                DeviceScope::EnvironmentManage,
            ),
            (
                Method::POST,
                "/api/agent-requests/r1",
                DeviceScope::OrchestrationOperate,
            ),
        ];
        for (method, path, scope) in cases {
            assert_eq!(
                required_scope(&method, path),
                Some(scope),
                "{method} {path}"
            );
        }
        assert_eq!(
            required_scope(&Method::POST, "/api/peer/v1/tools/t3_task"),
            None
        );
    }

    #[test]
    fn non_device_callers_keep_full_access() {
        assert!(CallerGrant(None).allows(DeviceScope::AccessManage));
    }
}
