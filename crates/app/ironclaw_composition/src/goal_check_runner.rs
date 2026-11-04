//! The host half of goal checks (`ironclaw_orchestrator::goal_checks`).
//!
//! Gate: the same approval settings the capability gate reads for
//! `builtin.shell` on the thread — a per-tool `disabled` refuses, "Full
//! access" (the profile's mode, or the global switch a profile follows)
//! runs, `ask_each_time` asks before every run, anything else asks once per
//! command. Run: the deployment's runtime process port (the host process
//! port on a local single-user install, the user sandbox otherwise), in the
//! thread's folder, never a raw spawn. A deployment without a process port
//! has no shell, so its checks are refused, never run elsewhere.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_approvals::{ApprovalSettingsProvider, ToolPermissionOverride};
use ironclaw_host_api::{
    ids::{CapabilityId, InvocationId},
    process::CommandExecutionRequest,
    resource::ResourceScope,
};
use ironclaw_host_runtime::RuntimeProcessPort;
use ironclaw_orchestrator::goal_checks::{
    GoalCheckGate, GoalCheckOutput, GoalCheckRequest, GoalCheckRunner,
};
use ironclaw_orchestrator::model::ThreadRecord;

/// The shell capability whose approval settings a check follows.
const SHELL_CAPABILITY: &str = "builtin.shell";
/// Where a check runs when its thread has no folder of its own.
const DEFAULT_WORKDIR: &str = "/workspace";

pub(crate) struct HostGoalCheckRunner {
    // arch-exempt: optional_arc, a deployment without a process backend has no shell and refuses checks, plan #goal-checks
    process: Option<Arc<dyn RuntimeProcessPort>>,
    approvals: Arc<dyn ApprovalSettingsProvider>,
}

impl HostGoalCheckRunner {
    pub(crate) fn new(
        process: Option<Arc<dyn RuntimeProcessPort>>,
        approvals: Arc<dyn ApprovalSettingsProvider>,
    ) -> Self {
        Self { process, approvals }
    }
}

fn thread_scope(thread: &ThreadRecord) -> ResourceScope {
    ResourceScope {
        tenant_id: thread.scope.tenant_id.clone(),
        user_id: thread.scope.user_id.clone(),
        agent_id: thread.scope.agent_id.clone(),
        project_id: thread.scope.project_id.clone(),
        mission_id: None,
        thread_id: Some(thread.thread_id.clone()),
        invocation_id: InvocationId::new(),
    }
}

#[async_trait]
impl GoalCheckRunner for HostGoalCheckRunner {
    async fn gate(&self, thread: &ThreadRecord) -> GoalCheckGate {
        if self.process.is_none() {
            return GoalCheckGate::Refused(
                "This backend has no shell runtime, so the check cannot run.".into(),
            );
        }
        let scope = thread_scope(thread);
        let override_ = match CapabilityId::new(SHELL_CAPABILITY) {
            Ok(shell) => self.approvals.tool_override(&scope, &shell).await,
            Err(error) => {
                tracing::debug!(%error, "shell capability id rejected; asking");
                return GoalCheckGate::AskEachRun;
            }
        };
        if override_ == Some(ToolPermissionOverride::Disabled) {
            return GoalCheckGate::Refused(
                "Shell commands are switched off in tool settings, so the check cannot run.".into(),
            );
        }
        if self.approvals.full_access(&scope).await {
            return GoalCheckGate::Allowed;
        }
        if override_ == Some(ToolPermissionOverride::AskEachTime) {
            return GoalCheckGate::AskEachRun;
        }
        GoalCheckGate::AskFirst
    }

    async fn run(&self, request: GoalCheckRequest) -> Result<GoalCheckOutput, String> {
        let Some(process) = &self.process else {
            return Err("this backend has no shell runtime".into());
        };
        let workdir = match &request.cwd {
            Some(dir) => dir
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| "the thread's folder is not a UTF-8 path".to_string())?,
            None => DEFAULT_WORKDIR.to_string(),
        };
        let output = process
            .run_command(CommandExecutionRequest {
                scope: thread_scope(&request.thread),
                mounts: None,
                command: request.command,
                workdir: Some(workdir),
                timeout_secs: Some(request.timeout.as_secs().max(1)),
                extra_env: Default::default(),
            })
            .await
            .map_err(|error| {
                tracing::debug!(%error, "goal check did not run");
                error.to_string()
            })?;
        Ok(GoalCheckOutput {
            exit_code: output.exit_code,
            output: output.output,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ironclaw_host_api::{
        ids::{TenantId, ThreadId, UserId},
        process::{CommandExecutionOutput, RuntimeProcessError},
        scope::Principal,
    };
    use ironclaw_orchestrator::model::{RuntimeId, ThreadScope};

    use super::*;

    #[derive(Default)]
    struct Settings {
        shell: Option<ToolPermissionOverride>,
        full_access: bool,
    }

    #[async_trait]
    impl ApprovalSettingsProvider for Settings {
        async fn tool_override(
            &self,
            scope: &ResourceScope,
            capability_id: &CapabilityId,
        ) -> Option<ToolPermissionOverride> {
            assert_eq!(scope.thread_id.as_ref().map(|t| t.as_str()), Some("t1"));
            (capability_id.as_str() == SHELL_CAPABILITY)
                .then_some(self.shell)
                .flatten()
        }
        async fn global_auto_approve(&self, _scope: &ResourceScope) -> bool {
            self.full_access
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
            self.full_access
        }
    }

    #[derive(Default)]
    struct Port(Mutex<Vec<CommandExecutionRequest>>);

    #[async_trait]
    impl RuntimeProcessPort for Port {
        async fn run_command(
            &self,
            request: CommandExecutionRequest,
        ) -> Result<CommandExecutionOutput, RuntimeProcessError> {
            self.0.lock().unwrap().push(request);
            Ok(CommandExecutionOutput {
                output: "3 failed".into(),
                saved_output: None,
                exit_code: 101,
                sandboxed: false,
                duration: std::time::Duration::from_millis(5),
            })
        }
    }

    fn thread() -> ThreadRecord {
        ThreadRecord::new(
            ThreadId::new("t1").unwrap(),
            ThreadScope {
                tenant_id: TenantId::new("tenant").unwrap(),
                user_id: UserId::new("u1").unwrap(),
                agent_id: None,
                project_id: None,
            },
            RuntimeId::new("ironclaw").unwrap(),
        )
    }

    fn runner(settings: Settings, port: Option<Arc<Port>>) -> HostGoalCheckRunner {
        HostGoalCheckRunner::new(
            port.map(|p| p as Arc<dyn RuntimeProcessPort>),
            Arc::new(settings),
        )
    }

    #[tokio::test]
    async fn the_gate_follows_the_shell_tool_settings_of_the_thread() {
        let port = Some(Arc::new(Port::default()));
        let gate = |settings| async { runner(settings, port.clone()).gate(&thread()).await };
        assert_eq!(gate(Settings::default()).await, GoalCheckGate::AskFirst);
        assert_eq!(
            gate(Settings {
                full_access: true,
                ..Settings::default()
            })
            .await,
            GoalCheckGate::Allowed
        );
        assert_eq!(
            gate(Settings {
                shell: Some(ToolPermissionOverride::AskEachTime),
                ..Settings::default()
            })
            .await,
            GoalCheckGate::AskEachRun
        );
        assert!(matches!(
            gate(Settings {
                shell: Some(ToolPermissionOverride::Disabled),
                full_access: true,
            })
            .await,
            GoalCheckGate::Refused(_)
        ));
        assert!(matches!(
            runner(
                Settings {
                    full_access: true,
                    ..Settings::default()
                },
                None
            )
            .gate(&thread())
            .await,
            GoalCheckGate::Refused(_)
        ));
    }

    #[tokio::test]
    async fn runs_go_through_the_process_port_in_the_thread_folder() {
        let port = Arc::new(Port::default());
        let runner = runner(Settings::default(), Some(Arc::clone(&port)));
        let out = runner
            .run(GoalCheckRequest {
                thread: thread(),
                command: "cargo test".into(),
                cwd: Some("/Users/me/project".into()),
                timeout: std::time::Duration::from_secs(30),
            })
            .await
            .unwrap();
        assert_eq!((out.exit_code, out.output.as_str()), (101, "3 failed"));
        runner
            .run(GoalCheckRequest {
                thread: thread(),
                command: "true".into(),
                cwd: None,
                timeout: std::time::Duration::from_secs(1),
            })
            .await
            .unwrap();
        let sent = port.0.lock().unwrap();
        assert_eq!(sent[0].command, "cargo test");
        assert_eq!(sent[0].workdir.as_deref(), Some("/Users/me/project"));
        assert_eq!(sent[0].timeout_secs, Some(30));
        assert_eq!(
            sent[0].scope.thread_id.as_ref().map(|t| t.as_str()),
            Some("t1")
        );
        assert_eq!(sent[1].workdir.as_deref(), Some(DEFAULT_WORKDIR));
    }
}
