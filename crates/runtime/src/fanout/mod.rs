//! Reusable bounded multi-output emulation for providers with a lower native count.

use std::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::FuturesUnordered};
use imagegen_bridge_core::{
    BatchCapabilities, BatchExecution, BatchMode, BridgeError, ErrorCode, ImageFailure,
    ImageProvider, ImageRequest, ImageResponse, MultiImageFailurePolicy, ProviderCapabilities,
    ProviderContext, ProviderDescriptor, ProviderEvent, RequestLimits, SessionMetadata, Timings,
    U8Range, Usage, negotiate_request, validate_request,
};
use sha2::{Digest as _, Sha256};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

const FAILED_SIBLING_GRACE: Duration = Duration::from_secs(1);

/// Bounded output fan-out policy shared by provider adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFanoutConfig {
    /// Effective maximum output count advertised by the decorated provider.
    pub max_outputs: u8,
    /// Provider-wide maximum simultaneous upstream operations.
    pub max_parallel_outputs: u8,
}

impl OutputFanoutConfig {
    fn validate(self) -> Result<Self, BridgeError> {
        if self.max_outputs == 0
            || self.max_parallel_outputs == 0
            || self.max_parallel_outputs > self.max_outputs
        {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "output fan-out limits are invalid",
            ));
        }
        Ok(self)
    }
}

/// Provider decorator that turns one logical multi-output request into bounded calls.
pub struct OutputFanoutProvider {
    inner: Arc<dyn ImageProvider>,
    config: OutputFanoutConfig,
    permits: Arc<Semaphore>,
}

impl std::fmt::Debug for OutputFanoutProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutputFanoutProvider")
            .field("provider", &self.inner.descriptor().name)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OutputFanoutProvider {
    /// Decorates a provider without changing its registry identity.
    pub fn new(
        inner: Arc<dyn ImageProvider>,
        config: OutputFanoutConfig,
    ) -> Result<Self, BridgeError> {
        let config = config.validate()?;
        Ok(Self {
            inner,
            config,
            permits: Arc::new(Semaphore::new(usize::from(config.max_parallel_outputs))),
        })
    }

    fn effective_capabilities(
        &self,
        mut capabilities: ProviderCapabilities,
    ) -> Result<ProviderCapabilities, BridgeError> {
        capabilities.validate()?;
        if self.config.max_outputs <= capabilities.count.max {
            return Ok(capabilities);
        }
        let native_count = capabilities.count;
        capabilities.count.max = self.config.max_outputs;
        capabilities.batching = BatchCapabilities {
            mode: BatchMode::FanOut,
            native_count,
            max_parallel_outputs: self.config.max_parallel_outputs,
        };
        capabilities.validate()?;
        Ok(capabilities)
    }

    async fn execute_fanout(
        &self,
        request: ImageRequest,
        context: ProviderContext,
        native_count: U8Range,
    ) -> Result<ImageResponse, BridgeError> {
        let started = Instant::now();
        let requested_count = request.parameters.n;
        let failure_policy = request.parameters.failure_policy;
        let batch_parallel = match request.policies.batch_execution {
            BatchExecution::Parallel => usize::from(self.config.max_parallel_outputs),
            BatchExecution::Auto
                if request.session.mode == imagegen_bridge_core::SessionMode::Isolated =>
            {
                usize::from(self.config.max_parallel_outputs)
            }
            BatchExecution::Sequential | BatchExecution::Auto => 1,
        };
        let batch_cancellation = context.cancellation.child_token();
        let mut chunks = chunks(requested_count, native_count.max);
        let mut active = FuturesUnordered::<BoxFuture<'static, ChunkResult>>::new();
        let mut results = Vec::new();
        for _ in 0..batch_parallel {
            push_next(
                &mut chunks,
                &mut active,
                &self.inner,
                &self.permits,
                &request,
                &context,
                &batch_cancellation,
            );
        }

        while let Some(result) = active.next().await {
            if failure_policy == MultiImageFailurePolicy::FailFast
                && let Err(error) = &result.result
            {
                let siblings_may_have_run = !active.is_empty() || !results.is_empty();
                batch_cancellation.cancel();
                let _ = tokio::time::timeout(FAILED_SIBLING_GRACE, async {
                    while active.next().await.is_some() {}
                })
                .await;
                let mut error = error
                    .clone()
                    .with_detail("output_index", result.offset)
                    .with_detail("generation_ms", result.elapsed_ms);
                if siblings_may_have_run {
                    error = error
                        .retryable(false)
                        .with_detail("outcome", "unknown")
                        .with_detail("retry_scope", "do_not_retry_full_batch");
                }
                return Err(error);
            }
            results.push(result);
            push_next(
                &mut chunks,
                &mut active,
                &self.inner,
                &self.permits,
                &request,
                &context,
                &batch_cancellation,
            );
        }
        results.sort_by_key(|result| result.offset);
        aggregate(
            request,
            context.request_id,
            requested_count,
            failure_policy,
            results,
            started,
        )
    }
}

#[async_trait]
impl ImageProvider for OutputFanoutProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.inner.descriptor()
    }

    async fn capabilities(&self, model: Option<&str>) -> Result<ProviderCapabilities, BridgeError> {
        self.effective_capabilities(self.inner.capabilities(model).await?)
    }

    async fn execute(
        &self,
        request: ImageRequest,
        context: ProviderContext,
    ) -> Result<ImageResponse, BridgeError> {
        validate_request(&request, RequestLimits::default())?;
        let native = self
            .inner
            .capabilities(request.routing.model.as_deref())
            .await?;
        let effective = self.effective_capabilities(native.clone())?;
        let negotiated = negotiate_request(&request, &effective)?;
        let request = negotiated.effective_request;
        if request.parameters.n <= native.count.max {
            let _permit = acquire(&self.permits, &context).await?;
            let mut response = self.inner.execute(request.clone(), context).await?;
            response.requested = negotiated.requested;
            response.effective = request.parameters;
            response
                .normalizations
                .splice(0..0, negotiated.normalizations);
            return Ok(response);
        }
        let mut response = self
            .execute_fanout(request.clone(), context, native.count)
            .await?;
        response.requested = negotiated.requested;
        response.effective = request.parameters;
        response
            .normalizations
            .splice(0..0, negotiated.normalizations);
        Ok(response)
    }

    async fn check_ready(&self) -> Result<(), BridgeError> {
        self.inner.check_ready().await
    }

    async fn get_session(&self, key: &str) -> Result<SessionMetadata, BridgeError> {
        self.inner.get_session(key).await
    }

    async fn delete_session(&self, key: &str) -> Result<(), BridgeError> {
        self.inner.delete_session(key).await
    }

    fn restart_count(&self) -> Option<u64> {
        self.inner.restart_count()
    }

    async fn shutdown(&self) -> Result<(), BridgeError> {
        self.inner.shutdown().await
    }
}

struct ChunkSpec {
    offset: u8,
    count: u8,
}

struct ChunkResult {
    offset: u8,
    count: u8,
    elapsed_ms: u64,
    result: Result<ImageResponse, BridgeError>,
}

fn chunks(total: u8, native_max: u8) -> VecDeque<ChunkSpec> {
    let mut chunks = VecDeque::new();
    let mut offset = 0_u8;
    while offset < total {
        let count = native_max.min(total - offset);
        chunks.push_back(ChunkSpec { offset, count });
        offset += count;
    }
    chunks
}

#[allow(clippy::too_many_arguments)]
fn push_next(
    chunks: &mut VecDeque<ChunkSpec>,
    active: &mut FuturesUnordered<BoxFuture<'static, ChunkResult>>,
    inner: &Arc<dyn ImageProvider>,
    permits: &Arc<Semaphore>,
    request: &ImageRequest,
    context: &ProviderContext,
    cancellation: &CancellationToken,
) {
    let Some(chunk) = chunks.pop_front() else {
        return;
    };
    let mut chunk_request = request.clone();
    chunk_request.parameters.n = chunk.count;
    let chunk_context = ProviderContext {
        request_id: subrequest_id(&context.request_id, chunk.offset),
        deadline: context.deadline,
        cancellation: cancellation.clone(),
        events: context.events.clone(),
    };
    active.push(
        execute_chunk(
            Arc::clone(inner),
            Arc::clone(permits),
            chunk_request,
            chunk_context,
            chunk,
        )
        .boxed(),
    );
}

async fn execute_chunk(
    inner: Arc<dyn ImageProvider>,
    permits: Arc<Semaphore>,
    request: ImageRequest,
    mut context: ProviderContext,
    chunk: ChunkSpec,
) -> ChunkResult {
    let started = Instant::now();
    let result = acquire(&permits, &context).await;
    let result = match result {
        Ok(_permit) => {
            let outer_events = context.events.take();
            if let Some(outer_events) = outer_events {
                let (sender, mut receiver) = mpsc::channel(4);
                context.events = Some(sender);
                let execution = inner.execute(request, context);
                tokio::pin!(execution);
                loop {
                    tokio::select! {
                        result = &mut execution => {
                            while let Ok(event) = receiver.try_recv() {
                                if let Some(event) = map_event(event, chunk.offset, chunk.count) {
                                    let _ = outer_events.send(event).await;
                                }
                            }
                            break result;
                        },
                        event = receiver.recv() => {
                            let Some(event) = event else { continue; };
                            if let Some(event) = map_event(event, chunk.offset, chunk.count) {
                                let _ = outer_events.send(event).await;
                            }
                        }
                    }
                }
            } else {
                inner.execute(request, context).await
            }
        }
        Err(error) => Err(error),
    };
    ChunkResult {
        offset: chunk.offset,
        count: chunk.count,
        elapsed_ms: elapsed_ms(started),
        result,
    }
}

async fn acquire(
    permits: &Arc<Semaphore>,
    context: &ProviderContext,
) -> Result<tokio::sync::OwnedSemaphorePermit, BridgeError> {
    let remaining = context.deadline.saturating_duration_since(Instant::now());
    tokio::select! {
        permit = Arc::clone(permits).acquire_owned() => permit.map_err(|_| {
            BridgeError::new(ErrorCode::Cancelled, "output fan-out is shutting down")
        }),
        () = context.cancellation.cancelled() => Err(BridgeError::new(
            ErrorCode::Cancelled,
            "output fan-out was cancelled while waiting for capacity",
        )),
        () = tokio::time::sleep(remaining) => Err(BridgeError::new(
            ErrorCode::Timeout,
            "output fan-out timed out while waiting for capacity",
        ).retryable(true)),
    }
}

fn map_event(event: ProviderEvent, offset: u8, count: u8) -> Option<ProviderEvent> {
    match event {
        ProviderEvent::Progress { stage } => Some(ProviderEvent::Progress { stage }),
        ProviderEvent::PartialImage {
            index,
            partial_index,
            b64_json,
        } if index < count => Some(ProviderEvent::PartialImage {
            index: offset + index,
            partial_index,
            b64_json,
        }),
        ProviderEvent::Started
        | ProviderEvent::Completed { .. }
        | ProviderEvent::PartialImage { .. } => None,
    }
}

fn aggregate(
    request: ImageRequest,
    request_id: String,
    requested_count: u8,
    failure_policy: MultiImageFailurePolicy,
    results: Vec<ChunkResult>,
    started: Instant,
) -> Result<ImageResponse, BridgeError> {
    let mut successful = Vec::new();
    let mut failures = Vec::new();
    for result in results {
        match result.result {
            Ok(mut response) => {
                validate_chunk_indices(&response, result.offset, result.count)?;
                for image in &mut response.data {
                    image.index += result.offset;
                    image.generation_ms.get_or_insert(result.elapsed_ms);
                }
                for failure in &mut response.failures {
                    failure.index += result.offset;
                }
                successful.push(response);
            }
            Err(error) => {
                for index in result.offset..result.offset + result.count {
                    failures.push(ImageFailure {
                        index,
                        error: error.clone().with_detail("output_index", index),
                        generation_ms: result.elapsed_ms,
                    });
                }
            }
        }
    }
    if successful.is_empty() {
        let mut error = failures.first().map_or_else(
            || BridgeError::new(ErrorCode::Upstream, "all fan-out outputs failed"),
            |failure| failure.error.clone(),
        );
        error = error
            .with_detail("all_outputs_failed", true)
            .with_detail("failed_outputs", requested_count);
        return Err(error);
    }

    let provider = successful[0].provider.clone();
    let model = successful[0].model.clone();
    if successful
        .iter()
        .any(|response| response.provider != provider || response.model != model)
    {
        return Err(BridgeError::new(
            ErrorCode::Protocol,
            "fan-out provider identity changed within one batch",
        ));
    }
    let mut data = Vec::new();
    let mut warnings = BTreeSet::from(["emulated_multi_image_fanout".to_owned()]);
    if request.policies.batch_execution == BatchExecution::Sequential {
        warnings.insert("sequential_multi_image_fanout".to_owned());
    }
    let mut usage = Usage::default();
    let mut has_usage = false;
    let mut revised_prompt = None;
    let session = merge_sessions(&successful);
    for response in successful {
        data.extend(response.data);
        failures.extend(response.failures);
        warnings.extend(response.warnings);
        if revised_prompt.is_none() {
            revised_prompt = response.revised_prompt;
        }
        if let Some(next) = response.usage {
            merge_usage(&mut usage, next);
            has_usage = true;
        }
    }
    data.sort_by_key(|image| image.index);
    failures.sort_by_key(|failure| failure.index);
    if !failures.is_empty() {
        if failure_policy != MultiImageFailurePolicy::BestEffort {
            return Err(BridgeError::new(
                ErrorCode::Protocol,
                "fan-out provider returned partial failures under fail-fast policy",
            ));
        }
        warnings.insert("partial_output_failure".to_owned());
    }
    Ok(ImageResponse {
        id: request_id,
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs()),
        provider,
        model,
        requested: request.parameters.clone(),
        effective: request.parameters,
        normalizations: Vec::new(),
        attempts: Vec::new(),
        data,
        failures,
        revised_prompt,
        usage: has_usage.then_some(usage),
        session,
        timings: Timings {
            provider_ms: elapsed_ms(started),
            total_ms: elapsed_ms(started),
            ..Timings::default()
        },
        warnings: warnings.into_iter().collect(),
    })
}

fn validate_chunk_indices(
    response: &ImageResponse,
    offset: u8,
    count: u8,
) -> Result<(), BridgeError> {
    let mut indices = response
        .data
        .iter()
        .map(|image| image.index)
        .chain(response.failures.iter().map(|failure| failure.index))
        .collect::<Vec<_>>();
    indices.sort_unstable();
    let expected = (0..count).collect::<Vec<_>>();
    if indices != expected {
        return Err(BridgeError::new(
            ErrorCode::Protocol,
            "fan-out subrequest returned an invalid output set",
        )
        .with_detail("output_offset", offset));
    }
    Ok(())
}

fn merge_usage(target: &mut Usage, value: Usage) {
    target.input_tokens = sum_option(target.input_tokens, value.input_tokens);
    target.output_tokens = sum_option(target.output_tokens, value.output_tokens);
    target.total_tokens = sum_option(target.total_tokens, value.total_tokens);
    for (key, value) in value.provider {
        let entry = target.provider.entry(key).or_default();
        *entry = entry.saturating_add(value);
    }
}

fn sum_option(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(
            left.unwrap_or_default()
                .saturating_add(right.unwrap_or_default()),
        ),
    }
}

fn merge_sessions(responses: &[ImageResponse]) -> Option<SessionMetadata> {
    let first = responses.first()?.session.clone()?;
    responses
        .iter()
        .skip(1)
        .all(|response| {
            response.session.as_ref().is_some_and(|session| {
                session.key == first.key && session.thread_id == first.thread_id
            })
        })
        .then_some(first)
}

fn subrequest_id(request_id: &str, offset: u8) -> String {
    let suffix = format!(":output:{offset}");
    if request_id.len().saturating_add(suffix.len()) <= 128 {
        return format!("{request_id}{suffix}");
    }
    let digest = format!("{:x}", Sha256::digest(request_id.as_bytes()));
    format!("fanout:{}:{offset}", &digest[..32])
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
