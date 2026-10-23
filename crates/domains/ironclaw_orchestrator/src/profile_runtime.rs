//! A profile's engine. A profile names a default provider and model; when
//! that provider belongs to a registered runtime (a CLI driver), the
//! profile's threads and delegations run there instead of on IronClaw's loop.
//! Providers no runtime advertises (IronClaw's own LLM providers) keep the
//! IronClaw loop and pass the model as a per-turn hint.
//!
//! A thread on another runtime has no IronClaw transcript: the product
//! surface sends its user turns, Stop and timeline here.

use ironclaw_host_api::ids::ThreadId;

use crate::engine::{Engine, Result};
use crate::error::OrchestratorError;
use crate::model::*;
use crate::profiles::ProfileRecord;
use crate::runtime::*;

/// What a user turn on an external-runtime thread did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserTurn {
    Started {
        run_id: Option<String>,
    },
    /// The thread was busy; the message waits in its queue.
    Queued {
        active_run_id: Option<String>,
    },
}

impl Engine {
    /// `(runtime, provider, model)` for a profile's defaults. `None` when the
    /// profile names no provider, or one no runtime advertises.
    pub async fn profile_route(
        &self,
        profile: &ProfileRecord,
        scope: &ThreadScope,
    ) -> Result<Option<(RuntimeId, Option<String>, Option<String>)>> {
        let settings = &profile.settings;
        let Some(provider) = settings
            .default_provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            return Ok(None);
        };
        let catalog = self.catalog(scope).await;
        let Some((rt, p)) = catalog.into_iter().find(|(_, p)| p.provider == provider) else {
            return Ok(None);
        };
        if !p.authenticated {
            return Err(OrchestratorError::request(
                "provider_unavailable",
                format!(
                    "Profile '{}' runs on '{provider}', which is not authenticated.",
                    profile.profile_id
                ),
            ));
        }
        let wanted = settings
            .default_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty());
        let model = match wanted {
            Some(m) if p.models.is_empty() || p.models.iter().any(|x| x == m) => {
                Some(m.to_string())
            }
            _ => p.models.first().cloned(),
        };
        Ok(Some((rt, Some(p.provider), model)))
    }

    /// Move a new, idle thread onto its profile's runtime. A thread that has
    /// already run a turn keeps the runtime it has. Returns whether it moved.
    pub async fn bind_profile_runtime(
        &self,
        thread_id: &ThreadId,
        profile: &ProfileRecord,
    ) -> Result<bool> {
        let Some(thread) = self.store.thread(thread_id).await? else {
            return Ok(false);
        };
        let Some((runtime_id, provider, model)) =
            self.profile_route(profile, &thread.scope).await?
        else {
            return Ok(false);
        };
        if runtime_id == thread.runtime_id
            || thread.turn_started_at.is_some()
            || thread.is_running()
        {
            return Ok(false);
        }
        let runtime = self.runtime(&runtime_id)?;
        let created = runtime
            .create_thread(CreateThread {
                scope: thread.scope.clone(),
                title: thread.title.clone(),
                provider: provider.clone(),
                model: model.clone(),
                idempotency_key: format!("profile-bind:{thread_id}"),
            })
            .await?;
        self.store
            .set_thread_runtime(
                thread_id,
                &runtime_id,
                created.provider.as_deref().or(provider.as_deref()),
                created.model.as_deref().or(model.as_deref()),
                created.native_session_id.as_deref(),
            )
            .await?;
        Ok(true)
    }

    /// The thread, when it runs on a runtime other than IronClaw's loop.
    pub async fn external_thread(&self, thread_id: &ThreadId) -> Result<Option<ThreadRecord>> {
        Ok(self
            .store
            .thread(thread_id)
            .await?
            .filter(|t| t.runtime_id != self.default_runtime))
    }

    /// A message the user typed into an external-runtime thread: start a
    /// turn, or queue it behind the running one.
    pub async fn user_turn(&self, thread: &ThreadRecord, text: &str) -> Result<UserTurn> {
        self.on_user_prompt(&thread.thread_id).await?;
        if self
            .deliver(
                &thread.thread_id,
                text.to_string(),
                TurnOrigin::UserPrompt,
                false,
            )
            .await?
        {
            let run_id = self
                .store
                .thread(&thread.thread_id)
                .await?
                .and_then(|t| t.active_run_id);
            return Ok(UserTurn::Started { run_id });
        }
        self.queue_add(&thread.thread_id, text, Author::User)
            .await?;
        let active_run_id = self
            .store
            .thread(&thread.thread_id)
            .await?
            .and_then(|t| t.active_run_id);
        Ok(UserTurn::Queued { active_run_id })
    }

    /// Stop on an external-runtime thread: the usual Stop (hold, cancel the
    /// subtree) plus cancelling the thread's own turn, which IronClaw's run
    /// cancel cannot reach.
    pub async fn user_stop_external(&self, thread: &ThreadRecord) -> Result<()> {
        self.on_user_stop(&thread.thread_id).await?;
        if let Some(current) = self.store.thread(&thread.thread_id).await?
            && current.is_running()
        {
            self.runtime(&current.runtime_id)?
                .cancel(&current, "user stop")
                .await?;
        }
        Ok(())
    }

    /// The transcript of an external-runtime thread, from its runtime.
    pub async fn external_history(&self, thread: &ThreadRecord) -> Result<Vec<HistoryMessage>> {
        self.history(thread).await
    }
}
