//! Script runner contracts for IronClaw Reborn.
//!
//! `ironclaw_sandbox` executes declared script/CLI capabilities through a
//! host-selected backend. Extension manifests describe the command metadata, but
//! extensions do not receive raw Docker flags, host paths, ambient environment,
//! secrets, or network by default.

pub mod docker;
mod orphans;

use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;

use futures_util::FutureExt as _;
use ironclaw_extension_contracts::runtime::ExtensionRuntime;
use ironclaw_host_api::{
    capability::CapabilityDescriptor,
    http::{
        CapabilityHostHttpRequest, RuntimeHttpEgress, RuntimeHttpEgressError,
        RuntimeHttpEgressResponse,
    },
    ids::{CapabilityId, ExtensionId, ResourceReservationId},
    mount::MountView,
    resource::{
        CapabilityHostResult, ResourceEstimate, ResourceReceipt, ResourceReservation,
        ResourceScope, ResourceUsage, RuntimeResourceBudget, RuntimeResourceError,
        RuntimeResourceErrorKind,
    },
    runtime::RuntimeKind,
};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Script runner limits owned by the host runtime, not by extension manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRuntimeConfig {
    pub max_stdout_bytes: u64,
    pub max_stderr_bytes: u64,
    pub max_wall_clock_ms: u64,
    /// Extra time, past `max_wall_clock_ms`, the runtime waits for the backend
    /// to tear a timed-out script down before abandoning the backend call,
    /// releasing the reservation, and reporting the timeout. Must exceed the
    /// backend's own bounded teardown (the sum of `docker::TeardownBounds`).
    pub teardown_grace_ms: u64,
    /// Upper bound on script executions the lane owns at once. A slot is held
    /// by the worker until the backend really returns (past a timeout), and
    /// by an orphaned container until its removal is verified. A call that
    /// finds no free slot is refused with a resource limit error, never
    /// queued.
    pub max_concurrent_executions: usize,
}

impl Default for ScriptRuntimeConfig {
    fn default() -> Self {
        Self {
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
            max_wall_clock_ms: 30_000,
            teardown_grace_ms: 20_000,
            max_concurrent_executions: 8,
        }
    }
}

impl ScriptRuntimeConfig {
    pub fn for_testing() -> Self {
        Self {
            max_stdout_bytes: 64 * 1024,
            max_stderr_bytes: 16 * 1024,
            max_wall_clock_ms: 5_000,
            teardown_grace_ms: 200,
            max_concurrent_executions: 4,
        }
    }
}

/// JSON invocation passed to a script capability.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptInvocation {
    pub input: Value,
}

/// Full resource-governed script execution request.
#[derive(Debug)]
pub struct ScriptExecutionRequest<'a> {
    /// The extension whose manifest declares this lane.
    ///
    /// The lane deliberately does **not** receive the `ExtensionPackage`: it
    /// read only the id, the capability descriptors, and the runtime stanza,
    /// and taking the package forced a `runtimes -> loops` dependency on the
    /// registry crate (the W7 `ironclaw_sandbox -> ironclaw_extension_registry` exception).
    /// The caller, which owns the package, projects those three.
    ///
    /// **Caller obligation (the cost of that carve-out).** These three are
    /// independent borrows, so the type no longer *structurally* guarantees
    /// they came from one package the way `&ExtensionPackage` did, and nothing
    /// in an `&ExtensionRuntime` identifies its owning extension. **Always
    /// project all three from the same `ExtensionPackage` in one expression.**
    /// Identical shape and identical obligation to
    /// `ironclaw_mcp::McpExecutionRequest`, whose field doc carries the full
    /// rationale.
    pub extension: &'a ExtensionId,
    pub capabilities: &'a [CapabilityDescriptor],
    pub runtime: &'a ExtensionRuntime,
    pub capability_id: &'a CapabilityId,
    pub scope: ResourceScope,
    pub estimate: ResourceEstimate,
    pub mounts: Option<MountView>,
    pub resource_reservation: Option<ResourceReservation>,
    pub invocation: ScriptInvocation,
}

/// Host-normalized request handed to the configured backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptBackendRequest {
    pub provider: ExtensionId,
    pub capability_id: CapabilityId,
    pub scope: ResourceScope,
    pub runner: String,
    pub image: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub stdin_json: String,
    pub max_stdout_bytes: u64,
    pub max_stderr_bytes: u64,
    pub max_wall_clock_ms: u64,
}

/// Raw backend output before the script runtime parses stdout as JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptBackendOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wall_clock_ms: u64,
}

impl ScriptBackendOutput {
    pub fn json(value: Value) -> Self {
        Self {
            exit_code: 0,
            stdout: serde_json::to_vec(&value).unwrap_or_else(|_| b"null".to_vec()),
            stderr: Vec::new(),
            wall_clock_ms: 0,
        }
    }
}

/// One unit of the script lane's bounded concurrency.
///
/// The runtime admits a call only with a free slot and hands the slot to the
/// backend with the request. Dropping it frees the unit. A backend that still
/// owns work after returning (an orphaned container it is still removing)
/// keeps the slot alive until that work is verified gone, so admission stays
/// bounded by what is really running.
#[derive(Debug)]
pub struct ScriptExecutionSlot {
    _permit: Option<OwnedSemaphorePermit>,
}

impl ScriptExecutionSlot {
    /// A slot outside any lane's admission control, for driving a backend
    /// directly (tests, tooling). The runtime never uses it.
    pub fn unmetered() -> Self {
        Self { _permit: None }
    }

    fn admitted(permit: OwnedSemaphorePermit) -> Self {
        Self {
            _permit: Some(permit),
        }
    }
}

/// Backend interface for sandboxed script execution.
pub trait ScriptBackend: Send + Sync {
    /// Run one script. `slot` is this execution's lane capacity; drop it when
    /// the work is done, or keep it with any work that outlives this call.
    fn execute(
        &self,
        request: ScriptBackendRequest,
        slot: ScriptExecutionSlot,
    ) -> Result<ScriptBackendOutput, String>;
}

/// Full resource-governed script execution result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptExecutionResult {
    pub result: CapabilityHostResult,
    pub receipt: ResourceReceipt,
}

pub type ScriptHostHttpResponse = RuntimeHttpEgressResponse;

#[derive(Debug, Error)]
pub enum ScriptHostHttpError {
    #[error("script host HTTP error: {reason}")]
    Egress { reason: String },
}

#[derive(Debug, Clone)]
pub struct ScriptRuntimeHttpAdapter<E> {
    egress: E,
}

impl<E> ScriptRuntimeHttpAdapter<E>
where
    E: RuntimeHttpEgress,
{
    pub fn new(egress: E) -> Self {
        Self { egress }
    }

    pub async fn request(
        &self,
        request: CapabilityHostHttpRequest,
    ) -> Result<ScriptHostHttpResponse, ScriptHostHttpError> {
        AssertUnwindSafe(
            self.egress
                .execute(request.into_runtime_request(RuntimeKind::Script)),
        )
        .catch_unwind()
        .await
        .map_err(|_| ScriptHostHttpError::Egress {
            reason: "runtime_http_egress_panicked".to_string(),
        })?
        .map_err(script_http_error)
    }
}

fn script_http_error(error: RuntimeHttpEgressError) -> ScriptHostHttpError {
    ScriptHostHttpError::Egress {
        reason: error.stable_runtime_reason().to_string(),
    }
}

/// Script runtime failures.
#[derive(Debug, Error)]
pub enum ScriptError {
    #[error("resource governor error: {0}")]
    Resource(RuntimeResourceError),
    #[error("script backend error: {reason}")]
    Backend { reason: String },
    #[error("unsupported script runner {runner}")]
    UnsupportedRunner { runner: String },
    #[error("extension {extension} uses runtime {actual:?}, not RuntimeKind::Script")]
    ExtensionRuntimeMismatch {
        extension: ExtensionId,
        actual: RuntimeKind,
    },
    #[error("capability {capability} is not declared by this extension package")]
    CapabilityNotDeclared { capability: CapabilityId },
    #[error("script descriptor mismatch: {reason}")]
    DescriptorMismatch { reason: String },
    #[error("invalid script invocation: {reason}")]
    InvalidInvocation { reason: String },
    #[error("script exited with code {code}: {stderr}")]
    ExitFailure { code: i32, stderr: String },
    #[error("script output limit exceeded: limit {limit}, actual {actual}")]
    OutputLimitExceeded { limit: u64, actual: u64 },
    #[error("script timed out after {limit_ms} ms")]
    Timeout { limit_ms: u64 },
    #[error("script stdout is invalid JSON: {reason}")]
    InvalidOutput { reason: String },
}

impl From<RuntimeResourceError> for ScriptError {
    fn from(error: RuntimeResourceError) -> Self {
        Self::Resource(error)
    }
}

/// Runtime for executing manifest-declared script capabilities.
///
/// The backend is synchronous (it waits on a host child process), so the
/// runtime runs it on the Tokio blocking pool and never on an async worker,
/// and bounds the wait at `max_wall_clock_ms + teardown_grace_ms`. A backend
/// still running at that deadline is abandoned on its blocking thread; the
/// reservation is released and the call reports [`ScriptError::Timeout`].
///
/// Admission is bounded by `max_concurrent_executions` slots. The slot moves
/// into the blocking job and is released only when the backend returns (or
/// later, by work the backend keeps), never at the timeout, so stuck
/// backends cannot be replaced by fresh ones without bound. A job that only
/// gets a blocking thread after its call timed out refuses to start.
#[derive(Debug, Clone)]
pub struct ScriptRuntime<B> {
    config: ScriptRuntimeConfig,
    backend: Arc<B>,
    slots: Arc<Semaphore>,
}

impl<B> ScriptRuntime<B>
where
    B: ScriptBackend + 'static,
{
    pub fn new(config: ScriptRuntimeConfig, backend: B) -> Self {
        let slots = Arc::new(Semaphore::new(config.max_concurrent_executions));
        Self {
            config,
            backend: Arc::new(backend),
            slots,
        }
    }

    pub fn config(&self) -> &ScriptRuntimeConfig {
        &self.config
    }

    pub async fn execute_extension_json<Budget>(
        &self,
        budget: &Budget,
        request: ScriptExecutionRequest<'_>,
    ) -> Result<ScriptExecutionResult, ScriptError>
    where
        Budget: RuntimeResourceBudget + ?Sized,
    {
        let backend_request = self.prepare_backend_request(&request)?;
        // Admission before reservation: a full lane is refused outright and
        // never holds budget it cannot use.
        let slot = self.admit()?;
        let reservation = reserve_or_use_existing(
            budget,
            request.scope.clone(),
            request.estimate.clone(),
            request.resource_reservation.clone(),
        )?;

        let output = match self.execute_backend_off_worker(backend_request, slot).await {
            Ok(output) => output,
            Err(error) => return Err(release_after_failure(budget, reservation.id, error)),
        };

        if output.stdout.len() as u64 > self.config.max_stdout_bytes {
            return Err(release_after_failure(
                budget,
                reservation.id,
                ScriptError::OutputLimitExceeded {
                    limit: self.config.max_stdout_bytes,
                    actual: output.stdout.len() as u64,
                },
            ));
        }

        if output.exit_code != 0 {
            return Err(release_after_failure(
                budget,
                reservation.id,
                ScriptError::ExitFailure {
                    code: output.exit_code,
                    stderr: bounded_lossy(&output.stderr, self.config.max_stderr_bytes),
                },
            ));
        }

        let parsed = match serde_json::from_slice::<Value>(&output.stdout) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Err(release_after_failure(
                    budget,
                    reservation.id,
                    ScriptError::InvalidOutput {
                        reason: error.to_string(),
                    },
                ));
            }
        };

        let output_bytes = output.stdout.len() as u64;
        let usage = ResourceUsage::default()
            .set_wall_clock_ms(output.wall_clock_ms)
            .set_output_bytes(output_bytes)
            .set_process_count(1);
        let receipt = budget.reconcile(reservation.id, usage.clone())?;
        Ok(ScriptExecutionResult {
            result: CapabilityHostResult {
                output: parsed,
                reservation_id: reservation.id,
                usage,
                output_bytes,
            },
            receipt,
        })
    }

    fn admit(&self) -> Result<ScriptExecutionSlot, ScriptError> {
        Arc::clone(&self.slots)
            .try_acquire_owned()
            .map(ScriptExecutionSlot::admitted)
            .map_err(|_full| {
                ScriptError::Resource(RuntimeResourceError::new(
                    RuntimeResourceErrorKind::LimitExceeded,
                    format!(
                        "script lane is at its concurrency limit of {}",
                        self.config.max_concurrent_executions
                    ),
                ))
            })
    }

    /// Run the synchronous backend on the blocking pool with an outer deadline.
    async fn execute_backend_off_worker(
        &self,
        request: ScriptBackendRequest,
        slot: ScriptExecutionSlot,
    ) -> Result<ScriptBackendOutput, ScriptError> {
        let limit_ms = request.max_wall_clock_ms;
        let deadline =
            Duration::from_millis(limit_ms.saturating_add(self.config.teardown_grace_ms));
        let start_by = Instant::now() + deadline;
        let abandoned = Arc::new(AtomicBool::new(false));
        let backend = Arc::clone(&self.backend);
        let job_abandoned = Arc::clone(&abandoned);
        let task = tokio::task::spawn_blocking(move || {
            // The job may only get a thread after the call gave up on it
            // (a saturated blocking pool). It must not start a script whose
            // reservation is already released; dropping `slot` frees it.
            if job_abandoned.load(Ordering::SeqCst) || Instant::now() >= start_by {
                tracing::debug!("refusing to start a script job whose call already timed out");
                return Err("script job expired before it started".to_string());
            }
            backend.execute(request, slot)
        });
        let outcome = tokio::time::timeout(deadline, task).await;
        if outcome.is_err() {
            abandoned.store(true, Ordering::SeqCst);
        }
        match outcome {
            Ok(Ok(Ok(output))) => Ok(output),
            Ok(Ok(Err(reason))) => Err(ScriptError::Backend { reason }),
            Ok(Err(join_error)) => Err(ScriptError::Backend {
                reason: format!("script backend task failed: {join_error}"),
            }),
            Err(_elapsed) => {
                // Dropping the join handle detaches the blocking thread. The
                // job keeps its slot until the backend really returns, and a
                // job that has not started yet will refuse to.
                tracing::debug!(
                    limit_ms,
                    deadline_ms = deadline.as_millis() as u64,
                    "abandoning a script backend call that outlived its teardown deadline"
                );
                Err(ScriptError::Timeout { limit_ms })
            }
        }
    }

    fn prepare_backend_request(
        &self,
        request: &ScriptExecutionRequest<'_>,
    ) -> Result<ScriptBackendRequest, ScriptError> {
        let descriptor = request
            .capabilities
            .iter()
            .find(|descriptor| &descriptor.id == request.capability_id)
            .cloned()
            .ok_or_else(|| ScriptError::CapabilityNotDeclared {
                capability: request.capability_id.clone(),
            })?;

        if descriptor.runtime != RuntimeKind::Script {
            return Err(ScriptError::ExtensionRuntimeMismatch {
                extension: request.extension.clone(),
                actual: descriptor.runtime,
            });
        }
        if descriptor.provider != *request.extension {
            return Err(ScriptError::DescriptorMismatch {
                reason: format!(
                    "descriptor {} provider {} does not match package {}",
                    descriptor.id, descriptor.provider, *request.extension
                ),
            });
        }

        let (runner, image, command, args) = match request.runtime {
            ExtensionRuntime::Script {
                runner,
                image,
                command,
                args,
            } => (runner, image, command, args),
            other => {
                return Err(ScriptError::ExtensionRuntimeMismatch {
                    extension: request.extension.clone(),
                    actual: other.kind(),
                });
            }
        };
        if runner == "docker" && image.is_none() {
            return Err(ScriptError::UnsupportedRunner {
                runner: runner.clone(),
            });
        }

        let stdin_json = serde_json::to_string(&request.invocation.input).map_err(|error| {
            ScriptError::InvalidInvocation {
                reason: error.to_string(),
            }
        })?;

        Ok(ScriptBackendRequest {
            provider: request.extension.clone(),
            capability_id: request.capability_id.clone(),
            scope: request.scope.clone(),
            runner: runner.clone(),
            image: image.clone(),
            command: command.clone(),
            args: args.clone(),
            stdin_json,
            max_stdout_bytes: self.config.max_stdout_bytes,
            max_stderr_bytes: self.config.max_stderr_bytes,
            max_wall_clock_ms: self.config.max_wall_clock_ms,
        })
    }
}

/// Object-safe script executor interface used by the kernel composition layer.
#[async_trait]
pub trait ScriptExecutor: Send + Sync {
    async fn execute_extension_json(
        &self,
        budget: &dyn RuntimeResourceBudget,
        request: ScriptExecutionRequest<'_>,
    ) -> Result<ScriptExecutionResult, ScriptError>;
}

#[async_trait]
impl<B> ScriptExecutor for ScriptRuntime<B>
where
    B: ScriptBackend + 'static,
{
    async fn execute_extension_json(
        &self,
        budget: &dyn RuntimeResourceBudget,
        request: ScriptExecutionRequest<'_>,
    ) -> Result<ScriptExecutionResult, ScriptError> {
        ScriptRuntime::execute_extension_json(self, budget, request).await
    }
}

fn reserve_or_use_existing<Budget>(
    budget: &Budget,
    scope: ResourceScope,
    estimate: ResourceEstimate,
    reservation: Option<ResourceReservation>,
) -> Result<ResourceReservation, ScriptError>
where
    Budget: RuntimeResourceBudget + ?Sized,
{
    if let Some(reservation) = reservation {
        if reservation.scope != scope || reservation.estimate != estimate {
            return Err(ScriptError::Resource(
                RuntimeResourceError::reservation_mismatch(reservation.id),
            ));
        }
        return Ok(reservation);
    }
    budget.reserve(scope, estimate).map_err(ScriptError::from)
}

fn release_after_failure<Budget>(
    budget: &Budget,
    reservation_id: ResourceReservationId,
    original: ScriptError,
) -> ScriptError
where
    Budget: RuntimeResourceBudget + ?Sized,
{
    if let Err(error) = budget.release(reservation_id) {
        tracing::debug!(error = %error, "releasing the script resource reservation after failure failed");
    }
    original
}

fn bounded_lossy(bytes: &[u8], limit: u64) -> String {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let end = bytes.len().min(limit);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
