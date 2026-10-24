//! A profile's engine. A profile names a default provider and model; when
//! that provider belongs to a registered runtime (a CLI driver), the
//! profile's threads and delegations run there instead of on IronClaw's loop.
//! Providers no runtime advertises (IronClaw's own LLM providers) keep the
//! IronClaw loop and pass the model as a per-turn hint.
//!
//! A thread on another runtime has no IronClaw transcript: the product
//! surface sends its user turns, Stop and timeline here.

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};

use crate::delegation::Target;
use crate::engine::{CallContext, Engine, Result};
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

    /// The thread's route and every provider it could switch to.
    pub async fn route_view(&self, thread: &ThreadRecord) -> Result<Value> {
        let providers: Vec<Value> = self
            .catalog(&thread.scope)
            .await
            .into_iter()
            .map(|(rt, p)| {
                json!({
                    "provider": p.provider, "name": p.name, "runtime": rt.as_str(),
                    "authenticated": p.authenticated, "models": p.models,
                    "efforts": p.efforts, "modelEfforts": p.model_efforts,
                })
            })
            .collect();
        Ok(json!({
            "threadId": thread.thread_id.as_str(),
            "runtime": thread.runtime_id.as_str(),
            "provider": thread.provider,
            "model": thread.model,
            "effort": self.store.thread_effort(&thread.thread_id).await?,
            "fresh": is_fresh(thread),
            "providers": providers,
            "picker": self.store.settings().await?.model_picker,
        }))
    }

    /// Replace the model picker's preferences (the whole document).
    pub async fn set_model_picker(&self, picker: Value) -> Result<Value> {
        let prefs: crate::model::ModelPickerPrefs = serde_json::from_value(picker)
            .map_err(|e| OrchestratorError::invalid(format!("invalid picker: {e}")))?;
        let prefs = prefs.validated().map_err(OrchestratorError::invalid)?;
        let settings = self
            .store
            .set_settings(&json!({ "model_picker": prefs }))
            .await?;
        Ok(json!({ "ok": true, "picker": settings.model_picker }))
    }

    /// Switch the provider/model and reasoning effort of a thread's next
    /// turns. An effort alone (`default` clears it) changes nothing else; a
    /// switch keeps only the effort it carries, on the thread that continues.
    pub async fn set_route(&self, thread: &ThreadRecord, target: &Target) -> Result<Value> {
        let effort = target
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty() && *e != "default")
            .map(str::to_string);
        if target.reasoning_effort.is_some() && target.provider.is_none() && target.model.is_none()
        {
            self.store
                .set_thread_effort(&thread.thread_id, effort.as_deref())
                .await?;
            return Ok(json!({
                "ok": true, "threadId": thread.thread_id.as_str(), "moved": "in_place",
                "forkThreadId": null, "provider": thread.provider, "model": thread.model,
                "effort": effort,
            }));
        }
        let mut out = self.switch_route(thread, target).await?;
        if out["ok"] == json!(true) {
            let id = match out["forkThreadId"].as_str() {
                Some(fork) => {
                    ThreadId::new(fork).map_err(|e| OrchestratorError::invalid(e.to_string()))?
                }
                None => thread.thread_id.clone(),
            };
            self.store.set_thread_effort(&id, effort.as_deref()).await?;
            out["effort"] = json!(effort);
        }
        Ok(out)
    }

    /// Switch the provider/model of a thread's next turns. An unused thread
    /// moves to the new runtime; a thread with history stays the same thread
    /// and opens a new segment (`crate::segments`), whose first turn carries
    /// a context handoff when the runtime changed.
    /// Model `default` clears the pick (IronClaw's loop then uses the
    /// profile's model).
    async fn switch_route(&self, thread: &ThreadRecord, target: &Target) -> Result<Value> {
        if thread.is_running() {
            return Err(OrchestratorError::request(
                "thread_busy",
                "Stop or wait for the running turn before switching model.",
            ));
        }
        let ctx = CallContext {
            thread_id: thread.thread_id.clone(),
            scope: thread.scope.clone(),
            provider: thread.provider.clone(),
            model: thread.model.clone(),
        };
        let wants_default = target.model.as_deref().map(str::trim) == Some("default");
        // "Default" exists only on IronClaw's loop.
        let (runtime_id, provider, model) = if wants_default {
            (self.default_runtime.clone(), target.provider.clone(), None)
        } else {
            let probe = Target {
                model: target.model.clone().filter(|_| !wants_default),
                ..target.clone()
            };
            self.resolve_target(&probe, &ctx, &thread.runtime_id)
                .await?
        };
        let id = &thread.thread_id;
        let done =
            |moved: &str, fork: Option<&str>, provider: Option<&str>, model: Option<&str>| {
                json!({
                    "ok": true, "threadId": id.as_str(), "moved": moved,
                    "forkThreadId": fork, "provider": provider, "model": model,
                })
            };
        // A thread with history keeps its id: the switch opens a segment.
        if !is_fresh(thread) {
            return self
                .switch_in_place(thread, runtime_id, provider, model)
                .await;
        }
        if runtime_id == thread.runtime_id {
            self.store
                .set_thread_runtime(
                    id,
                    &runtime_id,
                    provider.as_deref(),
                    model.as_deref(),
                    thread.native_session_id.as_deref(),
                )
                .await?;
            return Ok(done(
                "in_place",
                None,
                provider.as_deref(),
                model.as_deref(),
            ));
        }
        let (provider, model, session) = if runtime_id == self.default_runtime {
            (provider, model, None)
        } else {
            let created = self
                .runtime(&runtime_id)?
                .create_thread(CreateThread {
                    scope: thread.scope.clone(),
                    title: thread.title.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                    idempotency_key: format!("route:{id}:{runtime_id}"),
                })
                .await?;
            (
                created.provider.or(provider),
                created.model.or(model),
                created.native_session_id,
            )
        };
        self.store
            .set_thread_runtime(
                id,
                &runtime_id,
                provider.as_deref(),
                model.as_deref(),
                session.as_deref(),
            )
            .await?;
        Ok(done("rebound", None, provider.as_deref(), model.as_deref()))
    }
}

/// No user turn has reached the thread yet.
fn is_fresh(thread: &ThreadRecord) -> bool {
    thread.last_user_at.is_none() && thread.turn_started_at.is_none()
}
