//! Shared request orchestration used by every transport and SDK facade.

use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImageRequest, ImageResponse, ProviderContext, ProviderEvent,
    RequestLimits, RevisedPromptPolicy, negotiate_request, validate_request,
};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    admission::AdmissionGate,
    idempotency::{IdempotencyAction, IdempotencyConfig, IdempotencyCoordinator},
    materialize::{MaterializationConfig, OutputMaterializer},
    registry::ProviderRegistry,
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
    idempotency: IdempotencyCoordinator,
    materializer: OutputMaterializer,
    shutdown: CancellationToken,
    activity: Arc<ActivityTracker>,
    shutdown_result: Mutex<Option<Result<(), BridgeError>>>,
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
        let global_gate = Arc::new(AdmissionGate::new(
            config.global_limit.max_concurrent,
            config.global_limit.max_queued,
            "global",
        )?);
        let mut provider_gates = BTreeMap::new();
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
        }
        let idempotency = IdempotencyCoordinator::new(config.idempotency)?;
        let materializer = OutputMaterializer::new(config.materialization.clone())?;
        Ok(Self {
            registry,
            config,
            global_gate,
            provider_gates,
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
        let provider = self.registry.resolve(request.routing.provider.as_deref())?;
        let descriptor = provider.descriptor();

        let queue_started = Instant::now();
        let global_permit = self
            .acquire_controlled(
                &self.global_gate,
                deadline,
                external_cancellation,
                operation,
            )
            .await?;
        let provider_gate = self
            .provider_gates
            .get(&descriptor.name)
            .ok_or_else(|| configuration_error("provider has no configured admission gate"))?;
        let provider_permit = self
            .acquire_controlled(provider_gate, deadline, external_cancellation, operation)
            .await?;
        let queue_ms = elapsed_ms(queue_started);

        let capabilities = self
            .await_controlled(
                provider.capabilities(request.routing.model.as_deref()),
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
        let negotiated = negotiate_request(&request, &capabilities)?;
        let effective_request = negotiated.effective_request;

        let provider_started = Instant::now();
        let provider_result = self
            .await_provider_execution(
                provider.execute(
                    effective_request.clone(),
                    ProviderContext {
                        request_id: request_id.clone(),
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

        response.id = request_id;
        response.provider = descriptor.name;
        response.requested = negotiated.requested;
        response.effective = effective_request.parameters.clone();
        response
            .normalizations
            .splice(0..0, negotiated.normalizations);
        if effective_request.policies.revised_prompt == RevisedPromptPolicy::Omit {
            response.revised_prompt = None;
        }
        response.timings.queue_ms = queue_ms;
        response.timings.provider_ms = provider_ms;
        let artifact_started = Instant::now();
        response = self
            .await_controlled(
                self.materializer
                    .materialize(response, &request, &effective_request),
                deadline,
                external_cancellation,
                operation,
                "request was cancelled during output materialization",
            )
            .await?;
        response.timings.artifact_ms = elapsed_ms(artifact_started);
        response.timings.total_ms = elapsed_ms(total_started);
        self.materializer
            .attach_metadata(&request, &effective_request, &mut response)?;
        drop(provider_permit);
        drop(global_permit);
        Ok(response)
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
    async fn strict_mode_rejects_verified_dimension_mismatch() {
        let provider = Arc::new(FakeProvider::new(Duration::ZERO));
        let runtime = runtime(&provider, |_| {});
        let mut request = ImageRequest::generate("test");
        request.parameters.size = ImageSize::exact(2, 2).unwrap();
        let error = runtime.execute(request).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);
        assert_eq!(error.details["expected"], "2x2");
        assert_eq!(error.details["actual"], "1x1");
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
