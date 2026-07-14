
#![allow(clippy::unwrap_used)]

use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicUsize, Ordering},
};

use imagegen_bridge_core::{
    Background, GeneratedImage, ImageAction, ImagePayload, InputCapabilities, InputFidelity,
    Moderation, OutputFormat, Quality, ResponseFormat, SessionMode, SizeCapabilities, SupportLevel,
};

use super::*;

struct FakeProvider {
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    fail_offset: Option<u8>,
    delay: Duration,
}

impl FakeProvider {
    fn new(fail_offset: Option<u8>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            fail_offset,
            delay: Duration::from_millis(20),
        }
    }
}

#[async_trait]
impl ImageProvider for FakeProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            name: "fanout-test".to_owned(),
            display_name: "Fan-out test".to_owned(),
            version: "1".to_owned(),
            experimental: false,
            models: vec!["test-image".to_owned()],
        }
    }

    async fn capabilities(
        &self,
        _model: Option<&str>,
    ) -> Result<ProviderCapabilities, BridgeError> {
        Ok(native_capabilities())
    }

    async fn execute(
        &self,
        request: ImageRequest,
        context: ProviderContext,
    ) -> Result<ImageResponse, BridgeError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_active.fetch_max(active, Ordering::AcqRel);
        let _active = ActiveGuard(&self.active);
        tokio::select! {
            () = tokio::time::sleep(self.delay) => {}
            () = context.cancellation.cancelled() => {
                return Err(BridgeError::new(ErrorCode::Cancelled, "fake cancelled"));
            }
        }
        let offset = context
            .request_id
            .rsplit(':')
            .next()
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or_default();
        if self.fail_offset == Some(offset) {
            return Err(BridgeError::new(ErrorCode::Upstream, "fake output failed").retryable(true));
        }
        if let Some(events) = &context.events {
            let _ = events
                .send(ProviderEvent::PartialImage {
                    index: 0,
                    partial_index: 0,
                    b64_json: "fixture".to_owned(),
                })
                .await;
        }
        let session = match request.session.mode {
            SessionMode::Isolated => None,
            SessionMode::Persistent => Some(SessionMetadata {
                key: request.session.key.clone(),
                thread_id: Some("thread-persistent".to_owned()),
                reused: true,
            }),
            SessionMode::Thread => Some(SessionMetadata {
                key: None,
                thread_id: request.session.thread_id.clone(),
                reused: true,
            }),
        };
        Ok(ImageResponse {
            id: context.request_id,
            created: 1,
            provider: "fanout-test".to_owned(),
            model: "test-image".to_owned(),
            requested: request.parameters.clone(),
            effective: request.parameters.clone(),
            normalizations: Vec::new(),
            data: (0..request.parameters.n)
                .map(|index| GeneratedImage {
                    index,
                    payload: ImagePayload::Metadata,
                    format: OutputFormat::Png,
                    width: 1,
                    height: 1,
                    bytes: 1,
                    sha256: "0".repeat(64),
                    generation_ms: None,
                    metadata_name: None,
                })
                .collect(),
            failures: Vec::new(),
            revised_prompt: Some(request.prompt),
            usage: Some(Usage {
                total_tokens: Some(1),
                ..Usage::default()
            }),
            session,
            timings: Timings::default(),
            warnings: Vec::new(),
        })
    }

    async fn check_ready(&self) -> Result<(), BridgeError> {
        Ok(())
    }
}

struct ActiveGuard<'a>(&'a AtomicUsize);

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn native_capabilities() -> ProviderCapabilities {
    let no_inputs = InputCapabilities {
        support: SupportLevel::Unsupported,
        max_count: 0,
        max_bytes_each: 0,
        max_bytes_total: 0,
    };
    ProviderCapabilities {
        provider: "fanout-test".to_owned(),
        implementation_version: "1".to_owned(),
        model: Some("test-image".to_owned()),
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
            allowed: BTreeSet::new(),
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
        revised_prompt: SupportLevel::Emulated,
        user_attribution: SupportLevel::Unsupported,
        input_fidelities: BTreeSet::from([InputFidelity::High]),
        actions: BTreeSet::from([ImageAction::Auto]),
        reference_images: no_inputs.clone(),
        edit_images: no_inputs.clone(),
        masks: no_inputs,
        partial_images: U8Range { min: 0, max: 0 },
        persistent_sessions: true,
        explicit_threads: true,
    }
}

fn provider(fake: Arc<FakeProvider>) -> OutputFanoutProvider {
    OutputFanoutProvider::new(
        fake,
        OutputFanoutConfig {
            max_outputs: 4,
            max_parallel_outputs: 2,
        },
    )
    .unwrap()
}

fn context(events: Option<mpsc::Sender<ProviderEvent>>) -> ProviderContext {
    ProviderContext {
        request_id: "batch".to_owned(),
        deadline: Instant::now() + Duration::from_secs(5),
        cancellation: CancellationToken::new(),
        events,
    }
}

#[tokio::test]
async fn advertises_effective_and_native_batch_limits() {
    let provider = provider(Arc::new(FakeProvider::new(None)));
    let capabilities = provider.capabilities(None).await.unwrap();
    assert_eq!(capabilities.count, U8Range { min: 1, max: 4 });
    assert_eq!(capabilities.batching.mode, BatchMode::FanOut);
    assert_eq!(
        capabilities.batching.native_count,
        U8Range { min: 1, max: 1 }
    );
    assert_eq!(capabilities.batching.max_parallel_outputs, 2);
}

#[tokio::test]
async fn best_effort_preserves_indices_and_maps_partial_events() {
    let fake = Arc::new(FakeProvider::new(Some(1)));
    let provider = provider(Arc::clone(&fake));
    let mut request = ImageRequest::generate("four outputs");
    request.parameters.n = 4;
    request.parameters.failure_policy = MultiImageFailurePolicy::BestEffort;
    request.output.response_format = ResponseFormat::Metadata;
    let (events, mut receiver) = mpsc::channel(8);
    let response = provider
        .execute(request, context(Some(events)))
        .await
        .unwrap();
    assert_eq!(
        response
            .data
            .iter()
            .map(|image| image.index)
            .collect::<Vec<_>>(),
        [0, 2, 3]
    );
    assert_eq!(response.failures[0].index, 1);
    assert_eq!(response.usage.unwrap().total_tokens, Some(3));
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| warning == "emulated_multi_image_fanout")
    );
    assert_eq!(fake.max_active.load(Ordering::Acquire), 2);
    let mut partial_indices = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let ProviderEvent::PartialImage { index, .. } = event {
            partial_indices.push(index);
        }
    }
    partial_indices.sort_unstable();
    assert_eq!(partial_indices, [0, 2, 3]);
}

#[tokio::test]
async fn fail_fast_stops_unscheduled_outputs_and_marks_ambiguous_batch() {
    let fake = Arc::new(FakeProvider::new(Some(0)));
    let provider = provider(Arc::clone(&fake));
    let mut request = ImageRequest::generate("fail first");
    request.parameters.n = 4;
    let error = provider.execute(request, context(None)).await.unwrap_err();
    assert!(fake.calls.load(Ordering::Acquire) <= 2);
    assert!(!error.retryable);
    assert_eq!(error.details["outcome"], "unknown");
    assert_eq!(error.details["retry_scope"], "do_not_retry_full_batch");
}

#[tokio::test]
async fn persistent_sessions_are_serialized_within_a_batch() {
    let fake = Arc::new(FakeProvider::new(None));
    let provider = provider(Arc::clone(&fake));
    let mut request = ImageRequest::generate("persistent variations");
    request.parameters.n = 3;
    request.session.mode = SessionMode::Persistent;
    request.session.key = Some("campaign".to_owned());
    let response = provider.execute(request, context(None)).await.unwrap();
    assert_eq!(response.data.len(), 3);
    assert_eq!(fake.max_active.load(Ordering::Acquire), 1);
    assert_eq!(response.session.unwrap().key.as_deref(), Some("campaign"));
}

#[tokio::test]
async fn provider_wide_parallelism_is_shared_across_batches() {
    let fake = Arc::new(FakeProvider::new(None));
    let provider = Arc::new(provider(Arc::clone(&fake)));
    let mut first = ImageRequest::generate("first batch");
    first.parameters.n = 4;
    let mut second = ImageRequest::generate("second batch");
    second.parameters.n = 4;
    let (first, second) = tokio::join!(
        provider.execute(first, context(None)),
        provider.execute(
            second,
            ProviderContext {
                request_id: "batch-two".to_owned(),
                ..context(None)
            }
        )
    );
    assert!(first.is_ok() && second.is_ok());
    assert_eq!(fake.max_active.load(Ordering::Acquire), 2);
}
