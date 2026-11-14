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
use ironclaw_approvals::{ApprovalSettingsProvider, ToolOverrideLookup, ToolPermissionOverride};
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
        let lookup = match CapabilityId::new(SHELL_CAPABILITY) {
            Ok(shell) => self.approvals.tool_override_lookup(&scope, &shell).await,
            Err(error) => {
                tracing::debug!(%error, "shell capability id rejected; asking");
                return GoalCheckGate::AskEachRun;
            }
        };
        // An unreadable shell setting refuses before Full access or an earlier
        // approval can stand in for it: it may be exactly the `disabled`.
        let override_ = match lookup {
            ToolOverrideLookup::Unavailable => {
                return GoalCheckGate::Refused(
                    "Tool settings could not be read, so the check did not run; try again \
                     once settings are available."
                        .into(),
                );
            }
            ToolOverrideLookup::Set(state) => Some(state),
            ToolOverrideLookup::Unset => None,
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
    use ironclaw_orchestrator::engine::Engine;

    #[derive(Default)]
    struct Settings {
        shell: Option<ToolPermissionOverride>,
        full_access: bool,
    }

    #[async_trait]
    impl ApprovalSettingsProvider for Settings {
        async fn tool_override_lookup(
            &self,
            scope: &ResourceScope,
            capability_id: &CapabilityId,
        ) -> ironclaw_approvals::ToolOverrideLookup {
            ironclaw_approvals::ToolOverrideLookup::from_setting({
                assert_eq!(scope.thread_id.as_ref().map(|t| t.as_str()), Some("t1"));
                (capability_id.as_str() == SHELL_CAPABILITY)
                    .then_some(self.shell)
                    .flatten()
            })
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

    /// Through the goal-check caller: the orchestrator engine asks
    /// `HostGoalCheckRunner` for the gate, then launches the command on the
    /// process port. With the production settings providers over an override
    /// store that cannot be read, a check never runs — not under Full access
    /// (inherited from global auto-approve), and not when the person already
    /// approved it.
    #[tokio::test]
    async fn an_unreadable_shell_override_refuses_goal_checks() {
        use crate::synthetic_approval_gate_tests::{FlakyOverrides, engine, production_settings};
        use ironclaw_orchestrator::runtime::{TurnEventSink as _, TurnOutcome};
        use ironclaw_product_contracts::inbox::{GoalCheckAction, GoalCheckSpec, GoalCheckStatus};

        async fn armed_check(
            engine: &Engine,
            thread: &ThreadId,
            scope: &ThreadScope,
        ) -> ironclaw_product_contracts::inbox::GoalCheckRecord {
            engine.ensure_thread(thread, scope, None).await.unwrap();
            let now = ironclaw_orchestrator::model::now_ms();
            engine
                .store()
                .set_goal(thread, Some("tests pass"), None, now)
                .await
                .unwrap();
            engine
                .store()
                .set_goal_check(
                    thread,
                    Some(&GoalCheckSpec {
                        command: "cargo test".into(),
                        max_attempts: Some(3),
                        timeout_secs: Some(30),
                    }),
                    now,
                )
                .await
                .unwrap()
                .unwrap()
                .check
                .unwrap()
        }

        async fn complete_turn(engine: &Engine, thread: &ThreadId) {
            let run = format!("run-{}", ironclaw_orchestrator::model::now_ms());
            engine
                .store()
                .set_turn_running(thread, Some(&run), ironclaw_orchestrator::model::now_ms())
                .await
                .unwrap();
            engine
                .turn_finished(
                    thread,
                    Some(run),
                    TurnOutcome::completed(None),
                    ironclaw_orchestrator::model::now_ms(),
                )
                .await;
        }

        async fn status(engine: &Engine, thread: &ThreadId) -> GoalCheckStatus {
            engine
                .store()
                .goal(thread)
                .await
                .unwrap()
                .unwrap()
                .check
                .unwrap()
                .status
        }

        let scope = ThreadScope {
            tenant_id: TenantId::new("tenant").unwrap(),
            user_id: UserId::new("u1").unwrap(),
            agent_id: None,
            project_id: None,
        };

        // Full access (the thread follows global auto-approve, on by
        // default) with the override store unreadable.
        let (_dir, engine_a) = engine().await;
        let overrides = FlakyOverrides::new();
        overrides
            .failing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let port = Arc::new(Port::default());
        engine_a.set_goal_check_runner(Arc::new(HostGoalCheckRunner::new(
            Some(Arc::clone(&port) as Arc<dyn RuntimeProcessPort>),
            production_settings(
                overrides,
                Arc::new(
                    ironclaw_approvals::test_support::in_memory_backed_auto_approve_setting_store(),
                ),
                Some(Arc::clone(&engine_a)),
            ),
        )));
        let full_access = ThreadId::new("t-full-access").unwrap();
        armed_check(&engine_a, &full_access, &scope).await;
        complete_turn(&engine_a, &full_access).await;
        engine_a.tick().await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            port.0.lock().unwrap().is_empty(),
            "Full access must not run a check whose shell setting cannot be read"
        );
        assert_ne!(
            status(&engine_a, &full_access).await,
            GoalCheckStatus::Running
        );

        // An already-approved check (global auto-approve off), then the store
        // becomes unreadable before it runs.
        let (_dir_b, engine_b) = engine().await;
        let overrides = FlakyOverrides::new();
        let auto_approve = Arc::new(
            ironclaw_approvals::test_support::in_memory_backed_auto_approve_setting_store(),
        );
        ironclaw_approvals::AutoApproveSettingStorePort::set(
            auto_approve.as_ref(),
            ironclaw_approvals::AutoApproveSettingInput {
                updated_by: Principal::User(scope.user_id.clone()),
                scope: ResourceScope {
                    tenant_id: scope.tenant_id.clone(),
                    user_id: scope.user_id.clone(),
                    agent_id: None,
                    project_id: None,
                    mission_id: None,
                    thread_id: None,
                    invocation_id: InvocationId::new(),
                },
                enabled: false,
            },
        )
        .await
        .unwrap();
        let port = Arc::new(Port::default());
        engine_b.set_goal_check_runner(Arc::new(HostGoalCheckRunner::new(
            Some(Arc::clone(&port) as Arc<dyn RuntimeProcessPort>),
            production_settings(overrides.clone(), auto_approve, Some(Arc::clone(&engine_b))),
        )));
        let approved = ThreadId::new("t-approved").unwrap();
        let check = armed_check(&engine_b, &approved, &scope).await;
        complete_turn(&engine_b, &approved).await;
        engine_b.tick().await;
        assert_eq!(
            status(&engine_b, &approved).await,
            GoalCheckStatus::AwaitingApproval,
            "with auto-approve off the check waits for the person"
        );
        engine_b
            .store()
            .goal_check_action(
                &approved,
                GoalCheckAction::RunNow,
                Some(check.revision),
                ironclaw_orchestrator::model::now_ms(),
            )
            .await
            .unwrap();
        overrides
            .failing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        engine_b.tick().await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            port.0.lock().unwrap().is_empty(),
            "an approved check must not run when its shell setting cannot be read"
        );
    }
}
