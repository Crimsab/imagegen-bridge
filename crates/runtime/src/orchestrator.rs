//! Shared request orchestration used by every transport and SDK facade.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use imagegen_bridge_core::{
    BridgeError, ErrorCode, FallbackPolicy, ImageRequest, ImageResponse, Normalization,
    ProviderAttempt, ProviderAttemptOutcome, ProviderContext, ProviderEvent, ProviderRoute,
    RequestLimits, RevisedPromptPolicy, negotiate_request, validate_request,
};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::{
    CircuitBreakerConfig, CircuitBreakerSnapshot,
    admission::AdmissionGate,
    circuit_breaker::CircuitBreaker,
    idempotency::{IdempotencyAction, IdempotencyConfig, IdempotencyCoordinator},
    materialize::{MaterializationConfig, OutputMaterializer},
    registry::ProviderRegistry,
    transparency::prepare_transparency,
};

/// One concurrency pool and its bounded waiting room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrencyLimit {
    /// Maximum operations executing concurrently.
    pub max_concurrent: usize,
    /// Maximum operations waiting for this pool.
    pub max_queued: usize,
}

impl Default for ConcurrencyLimit {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            max_queued: 16,
        }
    }
}

/// Complete shared runtime policy.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Intrinsic request limits applied before provider selection.
    pub request_limits: RequestLimits,
    /// Deadline used when a request does not provide one.
    pub default_timeout: Duration,
    /// Cooperative cleanup window after cancellation or deadline expiry.
    pub cancellation_grace: Duration,
    /// Maximum time graceful shutdown waits for active calls to unwind.
    pub shutdown_grace: Duration,
    /// Global execution and queue bound.
    pub global_limit: ConcurrencyLimit,
    /// Default bound for each registered provider.
    pub default_provider_limit: ConcurrencyLimit,
    /// Per-provider limit overrides.
    pub provider_limits: BTreeMap<String, ConcurrencyLimit>,
    /// Default per-provider circuit-breaker policy.
    pub default_circuit_breaker: CircuitBreakerConfig,
    /// Per-provider circuit-breaker overrides.
    pub circuit_breakers: BTreeMap<String, CircuitBreakerConfig>,
    /// Scoped replay coordination policy.
    pub idempotency: IdempotencyConfig,
    /// Output verification and delivery policy.
    pub materialization: MaterializationConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            request_limits: RequestLimits::default(),
            default_timeout: Duration::from_secs(5 * 60),
            cancellation_grace: Duration::from_secs(1),
            shutdown_grace: Duration::from_secs(10),
            global_limit: ConcurrencyLimit {
                max_concurrent: 16,
                max_queued: 64,
            },
            default_provider_limit: ConcurrencyLimit::default(),
            provider_limits: BTreeMap::new(),
            default_circuit_breaker: CircuitBreakerConfig::default(),
            circuit_breakers: BTreeMap::new(),
            idempotency: IdempotencyConfig::default(),
            materialization: MaterializationConfig::default(),
        }
    }
}

/// Per-call transport context that is not forwarded as generation content.
#[derive(Clone)]
pub struct ExecutionContext {
    /// Optional caller-generated safe request ID.
    pub request_id: Option<String>,
    /// Tenant/user boundary for an idempotency key.
    pub idempotency_scope: String,
    /// Transport disconnect or caller cancellation signal.
    pub cancellation: CancellationToken,
}

impl std::fmt::Debug for ExecutionContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecutionContext")
            .field("request_id", &self.request_id)
            .field("idempotency_scope", &"[REDACTED SCOPE]")
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish()
    }
}

impl Default for ExecutionContext {
    fn default() -> Self {
        Self {
            request_id: None,
            idempotency_scope: "default".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }
}

/// Queue counters suitable for bounded operational metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeQueueSnapshot {
    /// Callers waiting for the global pool.
    pub global_queued: usize,
    /// Callers waiting per provider.
    pub providers_queued: BTreeMap<String, usize>,
}

/// Provider-neutral execution engine shared by CLI, HTTP, and libraries.
pub struct ImagegenRuntime {
    registry: ProviderRegistry,
    config: RuntimeConfig,
    global_gate: Arc<AdmissionGate>,
    provider_gates: BTreeMap<String, Arc<AdmissionGate>>,
    circuit_breakers: BTreeMap<String, Arc<CircuitBreaker>>,
    idempotency: IdempotencyCoordinator,
    materializer: OutputMaterializer,
    shutdown: CancellationToken,
    activity: Arc<ActivityTracker>,
    shutdown_result: Mutex<Option<Result<(), BridgeError>>>,
}

struct RouteExecution {
    response: ImageResponse,
    effective_request: ImageRequest,
}

impl std::fmt::Debug for ImagegenRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImagegenRuntime")
            .field("registry", &self.registry)
            .field("config", &self.config)
            .field("shutdown", &self.shutdown.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl ImagegenRuntime {
    /// Validates configuration and constructs all fixed-capacity runtime state.
    pub fn new(registry: ProviderRegistry, config: RuntimeConfig) -> Result<Self, BridgeError> {
        if config.default_timeout.is_zero()
            || config.default_timeout > Duration::from_millis(config.request_limits.max_timeout_ms)
        {
            return Err(configuration_error(
                "default timeout must be within the configured request limit",
            ));
        }
        if config.cancellation_grace.is_zero()
            || config.cancellation_grace > Duration::from_secs(30)
        {
            return Err(configuration_error(
                "cancellation grace must be greater than zero and at most 30 seconds",
            ));
        }
        if config.shutdown_grace.is_zero() || config.shutdown_grace > Duration::from_secs(5 * 60) {
            return Err(configuration_error(
                "shutdown grace must be greater than zero and at most five minutes",
            ));
        }
        for name in config.provider_limits.keys() {
            registry.resolve(Some(name))?;
        }
        for name in config.circuit_breakers.keys() {
            registry.resolve(Some(name))?;
        }
        let global_gate = Arc::new(AdmissionGate::new(
            config.global_limit.max_concurrent,
            config.global_limit.max_queued,
            "global",
        )?);
        let mut provider_gates = BTreeMap::new();
        let mut circuit_breakers = BTreeMap::new();
        for (name, _) in registry.entries() {
            let limit = config
                .provider_limits
                .get(name)
                .copied()
                .unwrap_or(config.default_provider_limit);
            provider_gates.insert(
                name.to_owned(),
                Arc::new(AdmissionGate::new(
                    limit.max_concurrent,
                    limit.max_queued,
                    format!("provider:{name}"),
                )?),
            );
            let breaker = config
                .circuit_breakers
                .get(name)
                .copied()
                .unwrap_or(config.default_circuit_breaker);
            circuit_breakers.insert(name.to_owned(), Arc::new(CircuitBreaker::new(breaker)?));
        }
        let idempotency = IdempotencyCoordinator::new(config.idempotency)?;
        let materializer = OutputMaterializer::new(config.materialization.clone())?;
        Ok(Self {
            registry,
            config,
            global_gate,
            provider_gates,
            circuit_breakers,
            idempotency,
            materializer,
            shutdown: CancellationToken::new(),
            activity: Arc::new(ActivityTracker::default()),
            shutdown_result: Mutex::new(None),
        })
    }

    /// Returns the immutable provider registry.
    #[must_use]
    pub const fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }

    /// Validates intrinsic request bounds without starting provider work.
    pub fn validate_request(&self, request: &ImageRequest) -> Result<(), BridgeError> {
        validate_request(request, self.config.request_limits)
    }

    /// Whether verified bridge-owned artifact delivery is available.
    #[must_use]
    pub const fn has_artifact_store(&self) -> bool {
        self.materializer.has_artifact_store()
    }

    /// Reads one ownership-verified artifact for authenticated delivery.
    pub fn read_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<imagegen_bridge_artifacts::StoredArtifactContent, BridgeError> {
        self.materializer.read_artifact(artifact_id)
    }

    /// Creates a bounded PNG thumbnail from one ownership-verified artifact.
    pub fn read_artifact_thumbnail(
        &self,
        artifact_id: &str,
        maximum_edge: u32,
    ) -> Result<Vec<u8>, BridgeError> {
        self.materializer.read_thumbnail(artifact_id, maximum_edge)
    }

    /// Executes with generated request metadata and the default idempotency scope.
    pub async fn execute(&self, request: ImageRequest) -> Result<ImageResponse, BridgeError> {
        self.execute_with(request, ExecutionContext::default())
            .await
    }

    /// Executes the complete validation, negotiation, admission, provider, and
    /// materialization pipeline.
    pub async fn execute_with(
        &self,
        request: ImageRequest,
        context: ExecutionContext,
    ) -> Result<ImageResponse, BridgeError> {
        self.execute_with_optional_events(request, context, None)
            .await
    }

    /// Executes the complete pipeline and forwards bounded provider events.
    pub async fn execute_with_events(
        &self,
        request: ImageRequest,
        context: ExecutionContext,
        events: mpsc::Sender<ProviderEvent>,
    ) -> Result<ImageResponse, BridgeError> {
        self.execute_with_optional_events(request, context, Some(events))
            .await
    }

    async fn execute_with_optional_events(
        &self,
        request: ImageRequest,
        context: ExecutionContext,
        events: Option<mpsc::Sender<ProviderEvent>>,
    ) -> Result<ImageResponse, BridgeError> {
        if self.shutdown.is_cancelled() {
            return Err(cancelled_error("runtime is shutting down"));
        }
        let _activity = self.activity.enter();
        if self.shutdown.is_cancelled() {
            return Err(cancelled_error("runtime is shutting down"));
        }
        validate_request(&request, self.config.request_limits)?;
        let request_id = validated_request_id(context.request_id)?;
        let timeout = request
            .timeout_ms
            .map_or(self.config.default_timeout, Duration::from_millis);
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| configuration_error("request deadline overflowed"))?;
        let operation = self.shutdown.child_token();

        let mut token = None;
        if let Some(key) = request.idempotency_key.as_deref() {
            match self
                .idempotency
                .begin(&context.idempotency_scope, key, &request)
                .await?
            {
                IdempotencyAction::Leader(leader) => token = Some(leader),
                IdempotencyAction::Cached(response) => return Ok(*response),
                IdempotencyAction::Wait(receiver) => {
                    return self
                        .await_controlled(
                            IdempotencyAction::wait(receiver, deadline, &operation),
                            deadline,
                            &context.cancellation,
                            &operation,
                            "request was cancelled while waiting for idempotent result",
                        )
                        .await;
                }
            }
        }

        let result = self
            .execute_leader(
                request,
                request_id,
                deadline,
                &context.cancellation,
                &operation,
                events,
            )
            .await;
        match (token, &result) {
            (Some(token), Ok(response)) => {
                self.idempotency.complete(token, response.clone()).await;
            }
            (Some(token), Err(error)) => {
                self.idempotency.fail(token, error.clone()).await;
            }
            (None, _) => {}
        }
        result
    }

    /// Returns current bounded queue depth without request identifiers.
    #[must_use]
    pub fn queue_snapshot(&self) -> RuntimeQueueSnapshot {
        RuntimeQueueSnapshot {
            global_queued: self.global_gate.queued(),
            providers_queued: self
                .provider_gates
                .iter()
                .map(|(name, gate)| (name.clone(), gate.queued()))
                .collect(),
        }
    }

    /// Returns redaction-safe per-provider circuit state.
    #[must_use]
    pub fn circuit_breaker_snapshot(&self) -> BTreeMap<String, CircuitBreakerSnapshot> {
        self.circuit_breakers
            .iter()
            .map(|(name, breaker)| (name.clone(), breaker.snapshot()))
            .collect()
    }

    /// Stops admissions, cancels active operations, and releases providers.
    pub async fn shutdown(&self) -> Result<(), BridgeError> {
        let mut stored_result = self.shutdown_result.lock().await;
        if let Some(result) = stored_result.as_ref() {
            return result.clone();
        }
        self.shutdown.cancel();
        self.global_gate.close();
        for gate in self.provider_gates.values() {
            gate.close();
        }
        let mut first_error =
            if tokio::time::timeout(self.config.shutdown_grace, self.activity.wait_until_idle())
                .await
                .is_err()
            {
                Some(
                    BridgeError::new(
                        ErrorCode::Timeout,
                        "runtime shutdown grace elapsed with active operations",
                    )
                    .retryable(true),
                )
            } else {
                None
            };
        for (_, provider) in self.registry.entries() {
            if let Err(error) = provider.shutdown().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        let result = first_error.map_or(Ok(()), Err);
        *stored_result = Some(result.clone());
        result
    }

    async fn execute_leader(
        &self,
        request: ImageRequest,
        request_id: String,
        deadline: Instant,
        external_cancellation: &CancellationToken,
        operation: &CancellationToken,
        events: Option<mpsc::Sender<ProviderEvent>>,
    ) -> Result<ImageResponse, BridgeError> {
        let total_started = Instant::now();
        let queue_started = Instant::now();
        let global_permit = self
            .acquire_controlled(
                &self.global_gate,
                deadline,
                external_cancellation,
                operation,
            )
            .await?;
        let global_queue_ms = elapsed_ms(queue_started);
        let primary_provider = request
            .routing
            .provider
            .clone()
            .unwrap_or_else(|| self.registry.default_name().to_owned());
        let routes = std::iter::once(ProviderRoute {
            provider: primary_provider.clone(),
            model: request.routing.model.clone(),
        })
        .chain(request.routing.fallbacks.iter().cloned())
        .collect::<Vec<_>>();
        let mut unique_routes = BTreeSet::new();
        for (index, route) in routes.iter().enumerate() {
            if !unique_routes.insert((route.provider.as_str(), route.model.as_deref())) {
                return Err(BridgeError::new(
                    ErrorCode::InvalidRequest,
                    "provider routing contains a duplicate resolved route",
                )
                .with_detail("field", format!("routing.fallbacks[{}]", index - 1))
                .with_detail("provider", &route.provider)
                .with_detail("model", route.model.as_deref()));
            }
        }
        let mut attempts = Vec::with_capacity(routes.len());
        let mut final_error = None;

        for (index, route) in routes.iter().enumerate() {
            let attempt_started = Instant::now();
            let attempt_span = tracing::info_span!(
                "imagegen_bridge.provider_attempt",
                request_id = %request_id,
                provider = %route.provider,
                attempt_index = index
            );
            match self
                .execute_route(
                    &request,
                    &request_id,
                    route,
                    deadline,
                    external_cancellation,
                    operation,
                    events.clone(),
                )
                .instrument(attempt_span)
                .await
            {
                Ok(execution) => {
                    let mut response = execution.response;
                    attempts.push(ProviderAttempt {
                        provider: response.provider.clone(),
                        model: Some(response.model.clone()),
                        outcome: ProviderAttemptOutcome::Succeeded,
                        error_code: None,
                        duration_ms: elapsed_ms(attempt_started),
                    });
                    if index > 0 {
                        response.normalizations.insert(
                            0,
                            Normalization {
                                field: "routing.provider".to_owned(),
                                requested: Some(serde_json::json!(primary_provider)),
                                effective: Some(serde_json::json!(response.provider)),
                                reason: "provider_fallback".to_owned(),
                            },
                        );
                        response.warnings.push("provider_fallback_used".to_owned());
                    }
                    if !request.routing.fallbacks.is_empty() {
                        response.attempts = attempts;
                    }
                    response.timings.queue_ms =
                        response.timings.queue_ms.saturating_add(global_queue_ms);
                    response.timings.total_ms = elapsed_ms(total_started);
                    self.materializer.attach_metadata(
                        &request,
                        &execution.effective_request,
                        &mut response,
                    )?;
                    drop(global_permit);
                    return Ok(response);
                }
                Err(error) => {
                    attempts.push(ProviderAttempt {
                        provider: route.provider.clone(),
                        model: route.model.clone(),
                        outcome: ProviderAttemptOutcome::Failed,
                        error_code: Some(error.code),
                        duration_ms: elapsed_ms(attempt_started),
                    });
                    let has_next = index + 1 < routes.len();
                    if !has_next || !should_fallback(&error, request.routing.fallback_policy) {
                        final_error = Some(error);
                        break;
                    }
                    final_error = Some(error);
                }
            }
        }
        drop(global_permit);
        Err(final_error
            .unwrap_or_else(|| configuration_error("provider routing produced no attempts"))
            .with_detail("attempts", attempts))
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_route(
        &self,
        request: &ImageRequest,
        request_id: &str,
        route: &ProviderRoute,
        deadline: Instant,
        external_cancellation: &CancellationToken,
        operation: &CancellationToken,
        events: Option<mpsc::Sender<ProviderEvent>>,
    ) -> Result<RouteExecution, BridgeError> {
        let provider = self.registry.resolve(Some(&route.provider))?;
        let descriptor = provider.descriptor();
        let breaker = self
            .circuit_breakers
            .get(&descriptor.name)
            .ok_or_else(|| configuration_error("provider has no configured circuit breaker"))?;
        let breaker_permit = breaker.acquire(&descriptor.name)?;
        let provider_gate = self
            .provider_gates
            .get(&descriptor.name)
            .ok_or_else(|| configuration_error("provider has no configured admission gate"))?;
        let queue_started = Instant::now();
        let provider_permit = self
            .acquire_controlled(provider_gate, deadline, external_cancellation, operation)
            .await;
        let _provider_permit = match provider_permit {
            Ok(permit) => permit,
            Err(error) => {
                drop(breaker_permit);
                return Err(error);
            }
        };
        let queue_ms = elapsed_ms(queue_started);
        let result = self
            .execute_route_admitted(
                request,
                request_id,
                route,
                deadline,
                external_cancellation,
                operation,
                events,
                provider,
                descriptor,
                queue_ms,
            )
            .await;
        breaker_permit.finish(&result);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_route_admitted(
        &self,
        request: &ImageRequest,
        request_id: &str,
        route: &ProviderRoute,
        deadline: Instant,
        external_cancellation: &CancellationToken,
        operation: &CancellationToken,
        events: Option<mpsc::Sender<ProviderEvent>>,
        provider: Arc<dyn imagegen_bridge_core::ImageProvider>,
        descriptor: imagegen_bridge_core::ProviderDescriptor,
        queue_ms: u64,
    ) -> Result<RouteExecution, BridgeError> {
        let capabilities = self
            .await_controlled(
                provider.capabilities(route.model.as_deref()),
                deadline,
                external_cancellation,
                operation,
                "request was cancelled during capability discovery",
            )
            .await
            .map_err(|error| attach_provider(error, &descriptor.name))?;
        if capabilities.provider != descriptor.name {
            return Err(protocol_error(
                "provider capability identity does not match its registry identity",
            )
            .with_provider(descriptor.name));
        }
        let mut routed_request = request.clone();
        routed_request.routing.provider = Some(route.provider.clone());
        routed_request.routing.model.clone_from(&route.model);
        routed_request.routing.fallbacks.clear();
        let prepared = prepare_transparency(&routed_request, &capabilities)?;
        validate_request(&prepared.provider_request, self.config.request_limits)?;
        let negotiated = negotiate_request(&prepared.provider_request, &capabilities)?;
        let effective_request = negotiated.effective_request;

        let provider_started = Instant::now();
        let provider_result = self
            .await_provider_execution(
                provider.execute(
                    effective_request.clone(),
                    ProviderContext {
                        request_id: request_id.to_owned(),
                        deadline,
                        cancellation: operation.clone(),
                        events,
                    },
                ),
                deadline,
                external_cancellation,
                operation,
            )
            .await;
        let provider_ms = elapsed_ms(provider_started);
        let mut response =
            provider_result.map_err(|error| attach_provider(error, &descriptor.name))?;
        if let Some(expected_model) = capabilities.model.as_deref()
            && response.model != expected_model
        {
            return Err(protocol_error(
                "provider response model does not match discovered capabilities",
            )
            .with_provider(descriptor.name)
            .with_detail("expected_model", expected_model)
            .with_detail("actual_model", response.model));
        }

        response.id = request_id.to_owned();
        response.provider = descriptor.name;
        response.requested = request.parameters.clone();
        response.effective = effective_request.parameters.clone();
        if prepared.plan.is_some() {
            response.effective.background = imagegen_bridge_core::Background::Transparent;
            response
                .warnings
                .push("transparent_background_postprocessed".to_owned());
        }
        response.normalizations.splice(
            0..0,
            prepared
                .normalizations
                .into_iter()
                .chain(negotiated.normalizations),
        );
        if effective_request.policies.revised_prompt == RevisedPromptPolicy::Omit {
            response.revised_prompt = None;
        }
        response.timings.queue_ms = queue_ms;
        response.timings.provider_ms = provider_ms;
        let artifact_started = Instant::now();
        response = self
            .await_controlled(
                self.materializer
                    .materialize(response, request, &effective_request, prepared.plan),
                deadline,
                external_cancellation,
                operation,
                "request was cancelled during output materialization",
            )
            .await?;
        response.timings.artifact_ms = elapsed_ms(artifact_started);
        Ok(RouteExecution {
            response,
            effective_request,
        })
    }

    async fn acquire_controlled(
        &self,
        gate: &AdmissionGate,
        deadline: Instant,
        external: &CancellationToken,
        operation: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, BridgeError> {
        self.await_controlled(
            gate.acquire(deadline, operation),
            deadline,
            external,
            operation,
            "request was cancelled while waiting for capacity",
        )
        .await
    }

    async fn await_provider_execution<T, F>(
        &self,
        future: F,
        deadline: Instant,
        external: &CancellationToken,
        operation: &CancellationToken,
    ) -> Result<T, BridgeError>
    where
        F: Future<Output = Result<T, BridgeError>>,
    {
        if deadline <= Instant::now() {
            operation.cancel();
            return Err(timeout_error());
        }
        tokio::pin!(future);
        let interrupted = tokio::select! {
            result = &mut future => return classify_provider_execution_result(result),
            () = external.cancelled() => {
                operation.cancel();
                (ErrorCode::Cancelled, "request was cancelled during provider execution")
            }
            () = self.shutdown.cancelled() => {
                operation.cancel();
                (ErrorCode::Cancelled, "runtime shut down during provider execution")
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                operation.cancel();
                (ErrorCode::Timeout, "request deadline elapsed during provider execution")
            }
        };
        match tokio::time::timeout(self.config.cancellation_grace, &mut future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) if !matches!(error.code, ErrorCode::Timeout | ErrorCode::Cancelled) => {
                Err(error)
            }
            Ok(Err(_)) | Err(_) => Err(unknown_outcome_error(interrupted.0, interrupted.1)),
        }
    }

    async fn await_controlled<T, F>(
        &self,
        future: F,
        deadline: Instant,
        external: &CancellationToken,
        operation: &CancellationToken,
        cancellation_message: &'static str,
    ) -> Result<T, BridgeError>
    where
        F: Future<Output = Result<T, BridgeError>>,
    {
        if deadline <= Instant::now() {
            operation.cancel();
            return Err(timeout_error());
        }
        tokio::pin!(future);
        tokio::select! {
            result = &mut future => result,
            () = external.cancelled() => {
                operation.cancel();
                let _ = tokio::time::timeout(self.config.cancellation_grace, &mut future).await;
                Err(cancelled_error(cancellation_message))
            }
            () = self.shutdown.cancelled() => {
                operation.cancel();
                let _ = tokio::time::timeout(self.config.cancellation_grace, &mut future).await;
                Err(cancelled_error("runtime is shutting down"))
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                operation.cancel();
                let _ = tokio::time::timeout(self.config.cancellation_grace, &mut future).await;
                Err(timeout_error())
            }
        }
    }
}

#[derive(Default)]
struct ActivityTracker {
    active: AtomicUsize,
    idle: Notify,
}

impl ActivityTracker {
    fn enter(self: &Arc<Self>) -> ActivityGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        ActivityGuard {
            tracker: Arc::clone(self),
        }
    }

    async fn wait_until_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

struct ActivityGuard {
    tracker: Arc<ActivityTracker>,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if self.tracker.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.tracker.idle.notify_waiters();
        }
    }
}

fn validated_request_id(value: Option<String>) -> Result<String, BridgeError> {
    let Some(value) = value else {
        return Ok(Uuid::now_v7().to_string());
    };
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'));
    if valid {
        Ok(value)
    } else {
        Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "request ID is invalid",
        ))
    }
}

fn attach_provider(mut error: BridgeError, provider: &str) -> BridgeError {
    if error.provider.is_none() {
        error.provider = Some(provider.to_owned());
    }
    error
}

fn should_fallback(error: &BridgeError, policy: FallbackPolicy) -> bool {
    if error
        .details
        .get("outcome")
        .and_then(serde_json::Value::as_str)
        == Some("unknown")
    {
        return false;
    }
    if matches!(
        error.code,
        ErrorCode::InvalidRequest
            | ErrorCode::PermissionDenied
            | ErrorCode::SafetyRejected
            | ErrorCode::Cancelled
            | ErrorCode::Session
            | ErrorCode::IdempotencyConflict
    ) {
        return false;
    }
    if matches!(
        error.code,
        ErrorCode::Configuration
            | ErrorCode::Authentication
            | ErrorCode::UnsupportedCapability
            | ErrorCode::RateLimited
            | ErrorCode::Overloaded
    ) {
        return true;
    }
    policy == FallbackPolicy::OnError
        && matches!(
            error.code,
            ErrorCode::Upstream
                | ErrorCode::Protocol
                | ErrorCode::Artifact
                | ErrorCode::Input
                | ErrorCode::Internal
        )
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn timeout_error() -> BridgeError {
    BridgeError::new(ErrorCode::Timeout, "request deadline elapsed").retryable(true)
}

fn unknown_outcome_error(code: ErrorCode, message: impl Into<String>) -> BridgeError {
    BridgeError::new(code, message)
        .retryable(false)
        .with_detail("outcome", "unknown")
}

fn classify_provider_execution_result<T>(result: Result<T, BridgeError>) -> Result<T, BridgeError> {
    match result {
        Err(error)
            if matches!(error.code, ErrorCode::Timeout | ErrorCode::Cancelled)
                && error
                    .details
                    .get("outcome")
                    .and_then(serde_json::Value::as_str)
                    != Some("unknown") =>
        {
            Err(error.retryable(false).with_detail("outcome", "unknown"))
        }
        result => result,
    }
}

fn cancelled_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Cancelled, message)
}

fn configuration_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Configuration, message)
}

fn protocol_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use std::{
        collections::BTreeSet,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use imagegen_bridge_artifacts::{ArtifactStore, ImageLimits, inspect_image};
    use imagegen_bridge_core::{
        Background, BatchCapabilities, BatchMode, CompatibilityMode, GeneratedImage,
        GenerationParameters, ImageAction, ImagePayload, ImageProvider, ImageSize,
        InputCapabilities, InputFidelity, Moderation, OutputFormat, ProviderCapabilities,
        ProviderDescriptor, Quality, ResponseFormat, SizeCapabilities, SupportLevel, Timings,
        U8Range, Usage,
    };

    use super::*;
    use crate::CircuitState;

    const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    struct FakeProvider {
        calls: AtomicUsize,
        delay: Duration,
        saw_cancellation: AtomicBool,
        corrupt_metadata: bool,
    }

    impl FakeProvider {
        fn new(delay: Duration) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                delay,
                saw_cancellation: AtomicBool::new(false),
                corrupt_metadata: false,
            }
        }

        fn corrupting() -> Self {
            Self {
                corrupt_metadata: true,
                ..Self::new(Duration::ZERO)
            }
        }
    }

    #[async_trait]
    impl ImageProvider for FakeProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                name: "fake".to_owned(),
                display_name: "Fake".to_owned(),
                version: "test".to_owned(),
                experimental: false,
                models: vec!["test-image".to_owned()],
            }
        }

        async fn capabilities(
            &self,
            model: Option<&str>,
        ) -> Result<ProviderCapabilities, BridgeError> {
            Ok(fake_capabilities(model))
        }

        async fn execute(
            &self,
            request: ImageRequest,
            context: ProviderContext,
        ) -> Result<ImageResponse, BridgeError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            tokio::select! {
                () = tokio::time::sleep(self.delay) => {}
                () = context.cancellation.cancelled() => {
                    self.saw_cancellation.store(true, Ordering::Release);
                    return Err(BridgeError::new(ErrorCode::Cancelled, "fake cancelled"));
                }
            }
            let bytes = STANDARD.decode(ONE_PIXEL_PNG).unwrap();
            let metadata = inspect_image(&bytes, ImageLimits::default()).unwrap();
            let image = GeneratedImage {
                index: 0,
                payload: ImagePayload::B64Json {
                    b64_json: ONE_PIXEL_PNG.to_owned(),
                },
                format: metadata.format,
                width: metadata.width,
                height: metadata.height,
                bytes: metadata.bytes,
                sha256: if self.corrupt_metadata {
                    "0".repeat(64)
                } else {
                    metadata.sha256
                },
                generation_ms: None,
                metadata_name: None,
            };
            Ok(ImageResponse {
                id: context.request_id,
                created: 0,
                provider: "fake".to_owned(),
                model: request
                    .routing
                    .model
                    .clone()
                    .unwrap_or_else(|| "fake-image".to_owned()),
                requested: request.parameters.clone(),
                effective: request.parameters.clone(),
                normalizations: Vec::new(),
                attempts: Vec::new(),
                data: (0..request.parameters.n)
                    .map(|index| GeneratedImage {
                        index,
                        ..image.clone()
                    })
                    .collect(),
                failures: Vec::new(),
                revised_prompt: Some("safe revised prompt".to_owned()),
                usage: Some(Usage::default()),
                session: None,
                timings: Timings::default(),
                warnings: Vec::new(),
            })
        }

        async fn check_ready(&self) -> Result<(), BridgeError> {
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum RouteFailure {
        Authentication,
        KnownUpstream,
        Safety,
        UnknownOutcome,
    }

    struct RouteProvider {
        name: &'static str,
        failure: Option<RouteFailure>,
        execute_calls: AtomicUsize,
    }

    impl RouteProvider {
        fn new(name: &'static str, failure: Option<RouteFailure>) -> Self {
            Self {
                name,
                failure,
                execute_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ImageProvider for RouteProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                name: self.name.to_owned(),
                display_name: self.name.to_owned(),
                version: "test".to_owned(),
                experimental: false,
                models: vec!["route-image".to_owned()],
            }
        }

        async fn capabilities(
            &self,
            model: Option<&str>,
        ) -> Result<ProviderCapabilities, BridgeError> {
            if matches!(self.failure, Some(RouteFailure::Authentication)) {
                return Err(BridgeError::new(
                    ErrorCode::Authentication,
                    "test provider is unavailable",
                ));
            }
            let mut capabilities = fake_capabilities(model);
            capabilities.provider = self.name.to_owned();
            Ok(capabilities)
        }

        async fn execute(
            &self,
            request: ImageRequest,
            context: ProviderContext,
        ) -> Result<ImageResponse, BridgeError> {
            self.execute_calls.fetch_add(1, Ordering::AcqRel);
            match self.failure {
                Some(RouteFailure::Safety) => {
                    return Err(BridgeError::safety_rejected("test safety rejection"));
                }
                Some(RouteFailure::UnknownOutcome) => {
                    return Err(
                        BridgeError::new(ErrorCode::Upstream, "test ambiguous failure")
                            .with_detail("outcome", "unknown"),
                    );
                }
                Some(RouteFailure::KnownUpstream) => {
                    return Err(BridgeError::new(ErrorCode::Upstream, "test known failure")
                        .with_detail("outcome", "failed"));
                }
                Some(RouteFailure::Authentication) | None => {}
            }
            let bytes = STANDARD.decode(ONE_PIXEL_PNG).unwrap();
            let metadata = inspect_image(&bytes, ImageLimits::default()).unwrap();
            Ok(ImageResponse {
                id: context.request_id,
                created: 0,
                provider: self.name.to_owned(),
                model: request
                    .routing
                    .model
                    .clone()
                    .unwrap_or_else(|| "route-image".to_owned()),
                requested: request.parameters.clone(),
                effective: request.parameters.clone(),
                normalizations: Vec::new(),
                attempts: Vec::new(),
                data: vec![GeneratedImage {
                    index: 0,
                    payload: ImagePayload::B64Json {
                        b64_json: ONE_PIXEL_PNG.to_owned(),
                    },
                    format: metadata.format,
                    width: metadata.width,
                    height: metadata.height,
                    bytes: metadata.bytes,
                    sha256: metadata.sha256,
                    generation_ms: None,
                    metadata_name: None,
                }],
                failures: Vec::new(),
                revised_prompt: None,
                usage: None,
                session: None,
                timings: Timings::default(),
                warnings: Vec::new(),
            })
        }

        async fn check_ready(&self) -> Result<(), BridgeError> {
            Ok(())
        }
    }

    fn fake_capabilities(model: Option<&str>) -> ProviderCapabilities {
        let unsupported_inputs = InputCapabilities {
            support: SupportLevel::Unsupported,
            max_count: 0,
            max_bytes_each: 0,
            max_bytes_total: 0,
        };
        ProviderCapabilities {
            provider: "fake".to_owned(),
            implementation_version: "test".to_owned(),
            model: model.map(str::to_owned),
            experimental: false,
            generation: true,
            edits: false,
            count: U8Range { min: 1, max: 1 },
            batching: BatchCapabilities {
                mode: BatchMode::Native,
                native_count: U8Range { min: 1, max: 1 },
                max_parallel_outputs: 1,
            },
            sizes: SizeCapabilities {
                auto: true,
                allowed: BTreeSet::from([ImageSize::exact(2, 2).unwrap()]),
                arbitrary: false,
                min_edge: None,
                max_edge: None,
                edge_multiple: None,
                min_pixels: None,
                max_pixels: None,
                max_aspect_ratio: None,
            },
            aspect_ratio: SupportLevel::Unsupported,
            resolution: SupportLevel::Unsupported,
            qualities: BTreeSet::from([Quality::Auto]),
            output_formats: BTreeSet::from([OutputFormat::Png]),
            backgrounds: BTreeSet::from([Background::Auto]),
            transparent_background: SupportLevel::Emulated,
            moderation: BTreeSet::from([Moderation::Auto]),
            negative_prompt: SupportLevel::Emulated,
            revised_prompt: SupportLevel::Native,
            user_attribution: SupportLevel::Native,
            input_fidelities: BTreeSet::from([InputFidelity::Low, InputFidelity::High]),
            actions: BTreeSet::from([ImageAction::Auto, ImageAction::Generate, ImageAction::Edit]),
            reference_images: unsupported_inputs.clone(),
            edit_images: unsupported_inputs.clone(),
            masks: unsupported_inputs,
            partial_images: U8Range { min: 0, max: 0 },
            persistent_sessions: false,
            explicit_threads: false,
        }
    }

    fn runtime(
        provider: &Arc<FakeProvider>,
        mutate: impl FnOnce(&mut RuntimeConfig),
    ) -> ImagegenRuntime {
        let registry =
            ProviderRegistry::new([Arc::clone(provider) as Arc<dyn ImageProvider>], "fake")
                .unwrap();
        let mut config = RuntimeConfig::default();
        mutate(&mut config);
        ImagegenRuntime::new(registry, config).unwrap()
    }

    fn routing_runtime(
        primary: &Arc<RouteProvider>,
        fallback: &Arc<RouteProvider>,
    ) -> ImagegenRuntime {
        routing_runtime_with(primary, fallback, |_| {})
    }

    fn routing_runtime_with(
        primary: &Arc<RouteProvider>,
        fallback: &Arc<RouteProvider>,
        mutate: impl FnOnce(&mut RuntimeConfig),
    ) -> ImagegenRuntime {
        let registry = ProviderRegistry::new(
            [
                Arc::clone(primary) as Arc<dyn ImageProvider>,
                Arc::clone(fallback) as Arc<dyn ImageProvider>,
            ],
            primary.name,
        )
        .unwrap();
        let mut config = RuntimeConfig::default();
        mutate(&mut config);
        ImagegenRuntime::new(registry, config).unwrap()
    }

    #[tokio::test]
    async fn negotiates_then_independently_verifies_and_projects_metadata() {
        let provider = Arc::new(FakeProvider::new(Duration::ZERO));
        let runtime = runtime(&provider, |_| {});
        let mut request = ImageRequest::generate("test");
        request.parameters = GenerationParameters {
            n: 2,
            ..GenerationParameters::default()
        };
        request.policies.compatibility = CompatibilityMode::Normalize;
        request.policies.revised_prompt = RevisedPromptPolicy::Omit;
        request.output.response_format = ResponseFormat::Metadata;
        let response = runtime
            .execute_with(
                request,
                ExecutionContext {
                    request_id: Some("request-1".to_owned()),
                    ..ExecutionContext::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(response.id, "request-1");
        assert_eq!(response.requested.n, 2);
        assert_eq!(response.effective.n, 1);
        assert_eq!(response.data.len(), 1);
        assert!(matches!(response.data[0].payload, ImagePayload::Metadata));
        assert!(response.revised_prompt.is_none());
        assert_eq!(provider.calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn strict_mode_reports_verified_dimension_mismatch() {
        let provider = Arc::new(FakeProvider::new(Duration::ZERO));
        let runtime = runtime(&provider, |_| {});
        let mut request = ImageRequest::generate("test");
        request.parameters.size = ImageSize::exact(2, 2).unwrap();
        let response = runtime.execute(request).await.unwrap();
        assert_eq!(response.effective.size, ImageSize::exact(1, 1).unwrap());
        assert!(response.normalizations.iter().any(|entry| {
            entry.field == "parameters.size"
                && entry.requested == Some(serde_json::json!("2x2"))
                && entry.effective == Some(serde_json::json!("1x1"))
                && entry.reason == "provider_output_dimensions_differed"
        }));
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning == "provider_output_dimensions_differed")
        );
    }

    #[tokio::test]
    async fn normalize_mode_reports_verified_dimension_mismatch() {
        let provider = Arc::new(FakeProvider::new(Duration::ZERO));
        let runtime = runtime(&provider, |_| {});
        let mut request = ImageRequest::generate("test");
        request.parameters.size = ImageSize::exact(2, 2).unwrap();
        request.policies.compatibility = CompatibilityMode::Normalize;
        let response = runtime.execute(request).await.unwrap();
        assert_eq!(response.effective.size, ImageSize::exact(1, 1).unwrap());
        assert!(response.normalizations.iter().any(|entry| {
            entry.field == "parameters.size"
                && entry.reason == "provider_output_dimensions_differed"
        }));
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning == "provider_output_dimensions_differed")
        );
    }

    #[tokio::test]
    async fn replays_idempotent_responses_without_a_second_provider_call() {
        let provider = Arc::new(FakeProvider::new(Duration::ZERO));
        let runtime = runtime(&provider, |_| {});
        let mut request = ImageRequest::generate("test");
        request.idempotency_key = Some("stable-key".to_owned());
        let first = runtime.execute(request.clone()).await.unwrap();
        let second = runtime.execute(request).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(provider.calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn publishes_bridge_owned_artifacts_only_after_verification() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(ArtifactStore::new(directory.path(), ImageLimits::default()).unwrap());
        let provider = Arc::new(FakeProvider::new(Duration::ZERO));
        let runtime = runtime(&provider, |config| {
            config.materialization.artifact_store = Some(store);
        });
        let mut request = ImageRequest::generate("test");
        request.output.response_format = ResponseFormat::Artifact;
        request.output.filename_prefix = Some("Result".to_owned());
        let response = runtime.execute(request).await.unwrap();
        assert!(matches!(
            &response.data[0].payload,
            ImagePayload::Artifact { name: Some(name), .. } if name.starts_with("result-")
        ));
    }

    #[tokio::test]
    async fn deadline_keeps_an_unknown_idempotency_tombstone() {
        let provider = Arc::new(FakeProvider::new(Duration::from_secs(5)));
        let runtime = runtime(&provider, |_| {});
        let mut request = ImageRequest::generate("test");
        request.timeout_ms = Some(100);
        request.idempotency_key = Some("retryable-key".to_owned());
        let first = runtime.execute(request.clone()).await.unwrap_err();
        assert_eq!(first.code, ErrorCode::Timeout);
        assert!(!first.retryable);
        assert_eq!(first.details["outcome"], "unknown");
        tokio::task::yield_now().await;
        assert!(provider.saw_cancellation.load(Ordering::Acquire));
        let second = runtime.execute(request).await.unwrap_err();
        assert_eq!(second.code, ErrorCode::Timeout);
        assert!(!second.retryable);
        assert_eq!(second.details["outcome"], "unknown");
        assert_eq!(provider.calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn rejects_provider_metadata_that_does_not_match_decoded_output() {
        let provider = Arc::new(FakeProvider::corrupting());
        let runtime = runtime(&provider, |_| {});
        let error = runtime
            .execute(ImageRequest::generate("test"))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);
    }

    #[tokio::test]
    async fn runtime_queue_rejects_work_beyond_its_explicit_bound() {
        let provider = Arc::new(FakeProvider::new(Duration::from_millis(40)));
        let runtime = Arc::new(runtime(&provider, |config| {
            config.global_limit = ConcurrencyLimit {
                max_concurrent: 1,
                max_queued: 1,
            };
            config.default_provider_limit = ConcurrencyLimit {
                max_concurrent: 1,
                max_queued: 1,
            };
        }));
        let first_runtime = Arc::clone(&runtime);
        let first =
            tokio::spawn(
                async move { first_runtime.execute(ImageRequest::generate("first")).await },
            );
        while provider.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        let second_runtime = Arc::clone(&runtime);
        let second = tokio::spawn(async move {
            second_runtime
                .execute(ImageRequest::generate("second"))
                .await
        });
        while runtime.queue_snapshot().global_queued == 0 {
            tokio::task::yield_now().await;
        }
        let error = runtime
            .execute(ImageRequest::generate("rejected"))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Overloaded);
        assert!(first.await.unwrap().is_ok());
        assert!(second.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn concurrent_idempotent_callers_share_one_provider_operation() {
        let provider = Arc::new(FakeProvider::new(Duration::from_millis(20)));
        let runtime = Arc::new(runtime(&provider, |_| {}));
        let mut request = ImageRequest::generate("same");
        request.idempotency_key = Some("shared-key".to_owned());
        let first_runtime = Arc::clone(&runtime);
        let first_request = request.clone();
        let first = tokio::spawn(async move { first_runtime.execute(first_request).await });
        while provider.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        let second = runtime.execute(request).await.unwrap();
        let first = first.await.unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(provider.calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn unavailable_primary_falls_back_in_order_and_records_attempts() {
        let primary = Arc::new(RouteProvider::new(
            "primary",
            Some(RouteFailure::Authentication),
        ));
        let fallback = Arc::new(RouteProvider::new("fallback", None));
        let runtime = routing_runtime(&primary, &fallback);
        let mut request = ImageRequest::generate("test routing");
        request.routing.fallbacks.push(ProviderRoute {
            provider: "fallback".to_owned(),
            model: None,
        });
        let response = runtime.execute(request).await.unwrap();
        assert_eq!(response.provider, "fallback");
        assert_eq!(response.attempts.len(), 2);
        assert_eq!(
            response.attempts[0].error_code,
            Some(ErrorCode::Authentication)
        );
        assert_eq!(
            response.attempts[1].outcome,
            ProviderAttemptOutcome::Succeeded
        );
        assert!(
            response
                .warnings
                .contains(&"provider_fallback_used".to_owned())
        );
        assert_eq!(primary.execute_calls.load(Ordering::Acquire), 0);
        assert_eq!(fallback.execute_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn open_primary_circuit_fails_fast_and_preserves_explicit_fallback() {
        let primary = Arc::new(RouteProvider::new(
            "primary",
            Some(RouteFailure::KnownUpstream),
        ));
        let fallback = Arc::new(RouteProvider::new("fallback", None));
        let runtime = routing_runtime_with(&primary, &fallback, |config| {
            config.default_circuit_breaker.failure_threshold = 1;
        });
        let mut request = ImageRequest::generate("test breaker fallback");
        request.routing.fallback_policy = FallbackPolicy::OnError;
        request.routing.fallbacks.push(ProviderRoute {
            provider: "fallback".to_owned(),
            model: None,
        });
        assert_eq!(
            runtime.execute(request.clone()).await.unwrap().provider,
            "fallback"
        );
        assert_eq!(
            runtime.circuit_breaker_snapshot()["primary"].state,
            CircuitState::Open
        );
        let second = runtime.execute(request).await.unwrap();
        assert_eq!(second.provider, "fallback");
        assert_eq!(primary.execute_calls.load(Ordering::Acquire), 1);
        assert_eq!(fallback.execute_calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn unknown_outcome_opens_only_for_later_calls_without_retrying_current_call() {
        let primary = Arc::new(RouteProvider::new(
            "primary",
            Some(RouteFailure::UnknownOutcome),
        ));
        let fallback = Arc::new(RouteProvider::new("fallback", None));
        let runtime = routing_runtime_with(&primary, &fallback, |config| {
            config.default_circuit_breaker.failure_threshold = 1;
        });
        let mut request = ImageRequest::generate("test unknown outcome");
        request.routing.fallback_policy = FallbackPolicy::OnError;
        request.routing.fallbacks.push(ProviderRoute {
            provider: "fallback".to_owned(),
            model: None,
        });
        let first = runtime.execute(request.clone()).await.unwrap_err();
        assert_eq!(first.details["outcome"], "unknown");
        assert_eq!(fallback.execute_calls.load(Ordering::Acquire), 0);
        assert_eq!(
            runtime.circuit_breaker_snapshot()["primary"].state,
            CircuitState::Open
        );
        assert_eq!(runtime.execute(request).await.unwrap().provider, "fallback");
        assert_eq!(primary.execute_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn fallback_never_bypasses_safety_or_unknown_outcomes() {
        for failure in [RouteFailure::Safety, RouteFailure::UnknownOutcome] {
            let primary = Arc::new(RouteProvider::new("primary", Some(failure)));
            let fallback = Arc::new(RouteProvider::new("fallback", None));
            let runtime = routing_runtime(&primary, &fallback);
            let mut request = ImageRequest::generate("test routing guard");
            request.routing.fallback_policy = FallbackPolicy::OnError;
            request.routing.fallbacks.push(ProviderRoute {
                provider: "fallback".to_owned(),
                model: None,
            });
            let error = runtime.execute(request).await.unwrap_err();
            assert_eq!(fallback.execute_calls.load(Ordering::Acquire), 0);
            assert_eq!(error.details["attempts"].as_array().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn on_error_expands_but_does_not_replace_unavailable_policy() {
        for (policy, expected_calls) in [
            (FallbackPolicy::OnUnavailable, 0),
            (FallbackPolicy::OnError, 1),
        ] {
            let primary = Arc::new(RouteProvider::new(
                "primary",
                Some(RouteFailure::KnownUpstream),
            ));
            let fallback = Arc::new(RouteProvider::new("fallback", None));
            let runtime = routing_runtime(&primary, &fallback);
            let mut request = ImageRequest::generate("test known outcome policy");
            request.routing.fallback_policy = policy;
            request.routing.fallbacks.push(ProviderRoute {
                provider: "fallback".to_owned(),
                model: None,
            });
            let result = runtime.execute(request).await;
            assert_eq!(
                fallback.execute_calls.load(Ordering::Acquire),
                expected_calls
            );
            assert_eq!(result.is_ok(), policy == FallbackPolicy::OnError);
        }
    }

    #[tokio::test]
    async fn implicit_primary_cannot_be_repeated_as_a_fallback() {
        let primary = Arc::new(RouteProvider::new("primary", None));
        let fallback = Arc::new(RouteProvider::new("fallback", None));
        let runtime = routing_runtime(&primary, &fallback);
        let mut request = ImageRequest::generate("test duplicate route");
        request.routing.fallbacks.push(ProviderRoute {
            provider: "primary".to_owned(),
            model: None,
        });
        let error = runtime.execute(request).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(primary.execute_calls.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn shutdown_cancels_and_drains_active_work_before_provider_release() {
        let provider = Arc::new(FakeProvider::new(Duration::from_secs(5)));
        let runtime = Arc::new(runtime(&provider, |_| {}));
        let executing_runtime = Arc::clone(&runtime);
        let executing = tokio::spawn(async move {
            executing_runtime
                .execute(ImageRequest::generate("active"))
                .await
        });
        while provider.calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        runtime.shutdown().await.unwrap();
        runtime.shutdown().await.unwrap();
        let error = executing.await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert!(provider.saw_cancellation.load(Ordering::Acquire));
        let error = runtime
            .execute(ImageRequest::generate("after shutdown"))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
    }
}
