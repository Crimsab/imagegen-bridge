//! Image provider and persistent Codex thread semantics.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use imagegen_bridge_artifacts::{
    ArtifactStore, ImageLimits, InputLoader, RemoteImageFetcher, inspect_image,
};
use imagegen_bridge_core::{
    Background, BatchCapabilities, BatchMode, BridgeError, ErrorCode, GeneratedImage, ImageAction,
    ImageOperation, ImagePayload, ImageProvider, ImageRequest, ImageResponse, InputCapabilities,
    InputFidelity, Moderation, OutputFormat, ProviderCapabilities, ProviderContext,
    ProviderDescriptor, Quality, RequestLimits, SessionMetadata, SessionMode, SizeCapabilities,
    SupportLevel, Timings, U8Range, Usage, negotiate_request, validate_request,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::{AppServerRpc, CodexProcess};

const TURN_CLEANUP_GRACE: Duration = Duration::from_secs(2);
const MAX_REVISED_PROMPT_BYTES: usize = 128 * 1024;

/// Durable binding interface implemented by memory and runtime `SQLite` stores.
#[async_trait]
pub trait SessionBindingStore: Send + Sync {
    /// Looks up a provider thread for a caller session key.
    async fn get(&self, key: &str) -> Result<Option<String>, BridgeError>;
    /// Atomically creates or replaces a session binding.
    async fn put(&self, key: &str, thread_id: &str) -> Result<(), BridgeError>;
    /// Deletes a binding.
    async fn delete(&self, key: &str) -> Result<(), BridgeError>;
}

/// In-memory session store for embedded use and tests.
#[derive(Debug, Default)]
pub struct MemorySessionBindingStore {
    bindings: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl SessionBindingStore for MemorySessionBindingStore {
    async fn get(&self, key: &str) -> Result<Option<String>, BridgeError> {
        Ok(self.bindings.lock().await.get(key).cloned())
    }

    async fn put(&self, key: &str, thread_id: &str) -> Result<(), BridgeError> {
        self.bindings
            .lock()
            .await
            .insert(key.to_owned(), thread_id.to_owned());
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), BridgeError> {
        self.bindings.lock().await.remove(key);
        Ok(())
    }
}

/// App-server image provider settings.
#[derive(Debug, Clone)]
pub struct AppServerProviderConfig {
    /// Optional Codex chat model used to orchestrate the image tool.
    pub codex_model: Option<String>,
    /// Working directory visible to Codex.
    pub cwd: PathBuf,
    /// Limits used to verify returned base64 images.
    pub image_limits: ImageLimits,
}

/// Secure input loading and bridge-owned staging for app-server references.
#[derive(Clone)]
pub struct AppServerReferenceInputs {
    loader: Arc<InputLoader>,
    remote_fetcher: Option<RemoteImageFetcher>,
    staging_store: Arc<ArtifactStore>,
    max_aggregate_bytes: u64,
}

impl std::fmt::Debug for AppServerReferenceInputs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppServerReferenceInputs")
            .field("loader", &"[CAPABILITY ROOTS]")
            .field("remote_fetcher", &self.remote_fetcher.is_some())
            .field("staging_store", &"[BRIDGE-OWNED ROOT]")
            .field("max_aggregate_bytes", &self.max_aggregate_bytes)
            .finish()
    }
}

impl AppServerReferenceInputs {
    /// Creates a reference loader with a per-request aggregate decoded-byte bound.
    pub fn new(
        loader: Arc<InputLoader>,
        remote_fetcher: Option<RemoteImageFetcher>,
        staging_store: Arc<ArtifactStore>,
        max_aggregate_bytes: u64,
    ) -> Result<Self, BridgeError> {
        if max_aggregate_bytes == 0 {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "app-server reference byte limit must be greater than zero",
            ));
        }
        Ok(Self {
            loader,
            remote_fetcher,
            staging_store,
            max_aggregate_bytes,
        })
    }
}

/// Provider backed by one initialized app-server connection.
pub struct AppServerImageProvider {
    fixed_rpc: Option<Arc<AppServerRpc>>,
    process: Option<Arc<CodexProcess>>,
    sessions: Arc<dyn SessionBindingStore>,
    thread_locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    config: AppServerProviderConfig,
    reference_inputs: Option<AppServerReferenceInputs>,
}

impl std::fmt::Debug for AppServerImageProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppServerImageProvider")
            .field("has_fixed_rpc", &self.fixed_rpc.is_some())
            .field("has_process", &self.process.is_some())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AppServerImageProvider {
    /// Creates a provider over an existing initialized connection.
    #[must_use]
    pub fn new(
        rpc: Arc<AppServerRpc>,
        sessions: Arc<dyn SessionBindingStore>,
        config: AppServerProviderConfig,
    ) -> Self {
        Self {
            fixed_rpc: Some(rpc),
            process: None,
            sessions,
            thread_locks: Mutex::new(HashMap::new()),
            config,
            reference_inputs: None,
        }
    }

    /// Creates a provider over an existing connection with secure references.
    #[must_use]
    pub fn new_with_inputs(
        rpc: Arc<AppServerRpc>,
        sessions: Arc<dyn SessionBindingStore>,
        config: AppServerProviderConfig,
        reference_inputs: AppServerReferenceInputs,
    ) -> Self {
        Self {
            fixed_rpc: Some(rpc),
            process: None,
            sessions,
            thread_locks: Mutex::new(HashMap::new()),
            config,
            reference_inputs: Some(reference_inputs),
        }
    }

    /// Creates a provider that owns and shuts down its Codex child process.
    #[must_use]
    pub fn with_process(
        process: Arc<CodexProcess>,
        sessions: Arc<dyn SessionBindingStore>,
        config: AppServerProviderConfig,
    ) -> Self {
        Self {
            fixed_rpc: None,
            process: Some(process),
            sessions,
            thread_locks: Mutex::new(HashMap::new()),
            config,
            reference_inputs: None,
        }
    }

    /// Creates an owned provider with secure bridge-staged reference inputs.
    #[must_use]
    pub fn with_process_and_inputs(
        process: Arc<CodexProcess>,
        sessions: Arc<dyn SessionBindingStore>,
        config: AppServerProviderConfig,
        reference_inputs: AppServerReferenceInputs,
    ) -> Self {
        Self {
            fixed_rpc: None,
            process: Some(process),
            sessions,
            thread_locks: Mutex::new(HashMap::new()),
            config,
            reference_inputs: Some(reference_inputs),
        }
    }

    async fn execute_inner(
        &self,
        request: ImageRequest,
        context: ProviderContext,
    ) -> Result<ImageResponse, BridgeError> {
        validate_request(&request, RequestLimits::default())?;
        let negotiated = negotiate_request(&request, &capabilities())?;
        let request = negotiated.effective_request;
        let rpc = self.connection().await?;
        let lock_key = request
            .session
            .key
            .clone()
            .or_else(|| request.session.thread_id.clone())
            .unwrap_or_else(|| context.request_id.clone());
        let thread_lock = {
            let mut locks = self.thread_locks.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(&lock_key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(lock_key, Arc::downgrade(&lock));
                lock
            }
        };
        let remaining = context.deadline.saturating_duration_since(Instant::now());
        let _guard = tokio::select! {
            guard = thread_lock.lock_owned() => guard,
            () = context.cancellation.cancelled() => {
                return Err(BridgeError::new(
                    ErrorCode::Cancelled,
                    "request was cancelled while waiting for the session",
                ));
            }
            () = tokio::time::sleep(remaining) => {
                return Err(BridgeError::new(
                    ErrorCode::Timeout,
                    "request timed out while waiting for the session",
                ).retryable(true));
            }
        };
        let started = Instant::now();
        let (thread_id, reused) = self.resolve_thread(&rpc, &request, &context).await?;
        let result = self.run_turn(&rpc, &thread_id, &request, &context).await;
        if request.session.mode == SessionMode::Isolated {
            let cleanup_deadline = Instant::now() + TURN_CLEANUP_GRACE;
            let _ = rpc
                .request_side_effecting_until(
                    "thread/archive",
                    json!({"threadId": thread_id}),
                    cleanup_deadline,
                    tokio_util::sync::CancellationToken::new(),
                )
                .await;
        }
        let turn = result?;
        let mut images = turn
            .images
            .into_iter()
            .map(|encoded| self.normalized_image(&encoded))
            .collect::<Result<Vec<_>, BridgeError>>()?;
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        for image in &mut images {
            image.generation_ms = Some(elapsed);
        }
        Ok(ImageResponse {
            id: context.request_id,
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
            provider: "codex-app-server".to_owned(),
            model: "gpt-image-2".to_owned(),
            requested: negotiated.requested,
            effective: request.parameters,
            normalizations: negotiated.normalizations,
            data: images,
            failures: Vec::new(),
            revised_prompt: turn.revised_prompt,
            usage: None::<Usage>,
            session: Some(SessionMetadata {
                key: request.session.key,
                thread_id: Some(thread_id),
                reused,
            }),
            timings: Timings {
                provider_ms: elapsed,
                total_ms: elapsed,
                ..Timings::default()
            },
            warnings: Vec::new(),
        })
    }

    async fn connection(&self) -> Result<Arc<AppServerRpc>, BridgeError> {
        if let Some(process) = &self.process {
            process.rpc().await
        } else {
            self.fixed_rpc.clone().ok_or_else(|| {
                BridgeError::new(
                    ErrorCode::Internal,
                    "app-server provider has no connection source",
                )
            })
        }
    }

    async fn resolve_thread(
        &self,
        rpc: &AppServerRpc,
        request: &ImageRequest,
        context: &ProviderContext,
    ) -> Result<(String, bool), BridgeError> {
        match request.session.mode {
            SessionMode::Isolated => self
                .start_thread(rpc, true, context)
                .await
                .map(|id| (id, false)),
            SessionMode::Persistent => {
                let key = request.session.key.as_deref().ok_or_else(|| {
                    BridgeError::new(ErrorCode::Session, "persistent session key is missing")
                })?;
                if let Some(thread_id) = self.sessions.get(key).await? {
                    self.resume_thread(rpc, &thread_id, context).await?;
                    Ok((thread_id, true))
                } else {
                    let thread_id = self.start_thread(rpc, false, context).await?;
                    self.sessions.put(key, &thread_id).await?;
                    Ok((thread_id, false))
                }
            }
            SessionMode::Thread => {
                let thread_id = request.session.thread_id.as_deref().ok_or_else(|| {
                    BridgeError::new(ErrorCode::Session, "explicit thread ID is missing")
                })?;
                self.resume_thread(rpc, thread_id, context).await?;
                Ok((thread_id.to_owned(), true))
            }
        }
    }

    async fn start_thread(
        &self,
        rpc: &AppServerRpc,
        ephemeral: bool,
        context: &ProviderContext,
    ) -> Result<String, BridgeError> {
        let result = rpc
            .request_until(
                "thread/start",
                json!({
                    "model": self.config.codex_model,
                    "cwd": self.config.cwd,
                    "approvalPolicy": "never",
                    "sandbox": "read-only",
                    "ephemeral": ephemeral,
                    "developerInstructions": "For each request, call image_gen.imagegen exactly once and do not use unrelated tools."
                }),
                context.deadline,
                context.cancellation.clone(),
            )
            .await?;
        string_at(
            &result,
            &["thread", "id"],
            "thread/start returned no thread ID",
        )
    }

    async fn resume_thread(
        &self,
        rpc: &AppServerRpc,
        thread_id: &str,
        context: &ProviderContext,
    ) -> Result<(), BridgeError> {
        rpc.request_until(
            "thread/resume",
            json!({"threadId": thread_id}),
            context.deadline,
            context.cancellation.clone(),
        )
        .await?;
        Ok(())
    }

    async fn run_turn(
        &self,
        rpc: &AppServerRpc,
        thread_id: &str,
        request: &ImageRequest,
        context: &ProviderContext,
    ) -> Result<TurnImages, BridgeError> {
        let paths = self.reference_paths(request).await?;
        let mut input = vec![json!({
            "type": "text",
            "text": turn_prompt(&request.prompt, &paths),
            "textElements": []
        })];
        input.extend(
            paths
                .iter()
                .map(|path| json!({"type": "localImage", "path": path, "detail": "original"})),
        );
        let mut notifications = rpc.subscribe();
        let result = rpc
            .request_side_effecting_until(
                "turn/start",
                json!({"threadId": thread_id, "input": input}),
                context.deadline,
                context.cancellation.clone(),
            )
            .await?;
        let turn_id = string_at(&result, &["turn", "id"], "turn/start returned no turn ID")?;
        let max_images = usize::from(request.parameters.n);
        let encoded_each = self
            .config
            .image_limits
            .max_encoded_bytes
            .saturating_add(2)
            .saturating_div(3)
            .saturating_mul(4);
        let max_result_bytes = usize::try_from(encoded_each)
            .unwrap_or(usize::MAX)
            .saturating_mul(max_images);
        let mut accumulator = TurnAccumulator {
            images: Vec::with_capacity(max_images),
            revised_prompt: None,
            completed_item_ids: HashSet::with_capacity(max_images),
            result_bytes: 0,
            max_images,
            max_result_bytes,
        };
        loop {
            let remaining = context.deadline.saturating_duration_since(Instant::now());
            let notification = tokio::select! {
                result = notifications.recv() => result.map_err(|error| notification_error(&error))?,
                () = context.cancellation.cancelled() => {
                    return Err(self
                        .interrupt_outcome(
                            rpc,
                            &mut notifications,
                            thread_id,
                            &turn_id,
                            ErrorCode::Cancelled,
                            "image generation was cancelled",
                        )
                        .await);
                }
                () = tokio::time::sleep(remaining) => {
                    return Err(self
                        .interrupt_outcome(
                            rpc,
                            &mut notifications,
                            thread_id,
                            &turn_id,
                            ErrorCode::Timeout,
                            "image generation timed out",
                        )
                        .await);
                }
            };
            if notification.method == "item/completed"
                && belongs_to(&notification.params, thread_id, &turn_id)
                && notification.params["item"]["type"] == "imageGeneration"
            {
                let item = &notification.params["item"];
                if item["status"] == "completed"
                    && let Err(error) = accumulator.capture(item)
                {
                    return Err(self
                        .protocol_failure_outcome(
                            rpc,
                            &mut notifications,
                            thread_id,
                            &turn_id,
                            error,
                        )
                        .await);
                }
            }
            if notification.method == "turn/completed"
                && belongs_to(&notification.params, thread_id, &turn_id)
            {
                if notification.params["turn"]["status"] != "completed" {
                    return Err(turn_failure_error(&notification.params["turn"], request));
                }
                if accumulator.images.is_empty() {
                    return Err(BridgeError::new(
                        ErrorCode::Upstream,
                        "Codex turn completed without an image",
                    )
                    .with_provider("codex-app-server"));
                }
                return Ok(TurnImages {
                    images: accumulator.images,
                    revised_prompt: accumulator.revised_prompt,
                });
            }
        }
    }

    async fn reference_paths(&self, request: &ImageRequest) -> Result<Vec<PathBuf>, BridgeError> {
        let inputs = request_images(request);
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let materializer = self.reference_inputs.as_ref().ok_or_else(|| {
            BridgeError::new(
                ErrorCode::Input,
                "app-server reference inputs require a configured secure staging store",
            )
        })?;
        let mut total = 0_u64;
        let mut paths = Vec::with_capacity(inputs.len());
        for input in inputs {
            let loaded = match &input.source {
                imagegen_bridge_core::ImageSource::Url { url } => {
                    materializer
                        .remote_fetcher
                        .as_ref()
                        .ok_or_else(|| {
                            BridgeError::new(ErrorCode::Input, "remote image inputs are disabled")
                        })?
                        .fetch(url)
                        .await?
                }
                _ => materializer.loader.load(&input)?,
            };
            total = total.checked_add(loaded.metadata.bytes).ok_or_else(|| {
                BridgeError::new(ErrorCode::Input, "reference image bytes overflowed")
            })?;
            if total > materializer.max_aggregate_bytes {
                return Err(BridgeError::new(
                    ErrorCode::Input,
                    "aggregate reference images exceed the configured byte limit",
                ));
            }
            let stored = materializer.staging_store.publish(
                &loaded.bytes,
                input.filename.as_deref().or(Some("reference")),
                Some(loaded.metadata.format),
            )?;
            paths.push(stored.path);
        }
        Ok(paths)
    }

    async fn interrupt_and_confirm(
        &self,
        rpc: &AppServerRpc,
        notifications: &mut broadcast::Receiver<crate::RpcNotification>,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<String, BridgeError> {
        let cleanup_deadline = Instant::now() + TURN_CLEANUP_GRACE;
        rpc.request_side_effecting_until(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
            cleanup_deadline,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        loop {
            let remaining = cleanup_deadline.saturating_duration_since(Instant::now());
            let notification = tokio::time::timeout(remaining, notifications.recv())
                .await
                .map_err(|_| {
                    unknown_app_server_outcome(
                        ErrorCode::Timeout,
                        "turn interruption was not confirmed",
                    )
                })?
                .map_err(|error| notification_error(&error))?;
            if notification.method == "turn/completed"
                && belongs_to(&notification.params, thread_id, turn_id)
            {
                return Ok(notification.params["turn"]["status"]
                    .as_str()
                    .and_then(safe_failure_label)
                    .unwrap_or("unknown")
                    .to_owned());
            }
        }
    }

    async fn interrupt_outcome(
        &self,
        rpc: &AppServerRpc,
        notifications: &mut broadcast::Receiver<crate::RpcNotification>,
        thread_id: &str,
        turn_id: &str,
        code: ErrorCode,
        message: &str,
    ) -> BridgeError {
        match self
            .interrupt_and_confirm(rpc, notifications, thread_id, turn_id)
            .await
        {
            Ok(status) if matches!(status.as_str(), "interrupted" | "cancelled" | "failed") => {
                BridgeError::new(code, message)
                    .with_provider("codex-app-server")
                    .retryable(code == ErrorCode::Timeout)
                    .with_detail("outcome", "cancelled")
                    .with_detail("terminal_status", status)
            }
            Ok(status) => {
                unknown_app_server_outcome(code, message).with_detail("terminal_status", status)
            }
            Err(_) => unknown_app_server_outcome(code, message),
        }
    }

    async fn protocol_failure_outcome(
        &self,
        rpc: &AppServerRpc,
        notifications: &mut broadcast::Receiver<crate::RpcNotification>,
        thread_id: &str,
        turn_id: &str,
        error: BridgeError,
    ) -> BridgeError {
        match self
            .interrupt_and_confirm(rpc, notifications, thread_id, turn_id)
            .await
        {
            Ok(status) if matches!(status.as_str(), "interrupted" | "cancelled" | "failed") => {
                error.with_detail("terminal_status", status)
            }
            Ok(status) => error
                .retryable(false)
                .with_detail("outcome", "unknown")
                .with_detail("terminal_status", status),
            Err(_) => error.retryable(false).with_detail("outcome", "unknown"),
        }
    }

    fn normalized_image(&self, encoded: &str) -> Result<GeneratedImage, BridgeError> {
        let bytes = STANDARD.decode(encoded.trim()).map_err(|_| {
            BridgeError::new(
                ErrorCode::Protocol,
                "Codex returned malformed base64 image data",
            )
        })?;
        let metadata =
            inspect_image(&bytes, self.config.image_limits).map_err(|error| BridgeError {
                code: ErrorCode::Protocol,
                provider: Some("codex-app-server".to_owned()),
                ..error
            })?;
        Ok(GeneratedImage {
            index: 0,
            payload: ImagePayload::B64Json {
                b64_json: encoded.to_owned(),
            },
            format: metadata.format,
            width: metadata.width,
            height: metadata.height,
            bytes: metadata.bytes,
            sha256: metadata.sha256,
            generation_ms: None,
            metadata_name: None,
        })
    }
}

#[async_trait]
impl ImageProvider for AppServerImageProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            name: "codex-app-server".to_owned(),
            display_name: "Codex app-server".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            experimental: false,
            models: vec!["gpt-image-2".to_owned()],
        }
    }

    async fn capabilities(&self, model: Option<&str>) -> Result<ProviderCapabilities, BridgeError> {
        let capabilities = capabilities();
        if let Some(model) = model
            && capabilities.model.as_deref() != Some(model)
        {
            return Err(BridgeError::new(
                ErrorCode::UnsupportedCapability,
                "Codex app-server image generation only exposes gpt-image-2",
            )
            .with_provider("codex-app-server")
            .with_detail("field", "routing.model")
            .with_detail("requested_model", model)
            .with_detail("effective_model", "gpt-image-2"));
        }
        Ok(capabilities)
    }

    async fn execute(
        &self,
        request: ImageRequest,
        context: ProviderContext,
    ) -> Result<ImageResponse, BridgeError> {
        self.execute_inner(request, context).await
    }

    async fn check_ready(&self) -> Result<(), BridgeError> {
        let account = self
            .connection()
            .await?
            .request("account/read", json!({}))
            .await?;
        if account["account"].is_null() {
            return Err(BridgeError::new(
                ErrorCode::Authentication,
                "Codex app-server requires authentication",
            )
            .with_provider("codex-app-server"));
        }
        Ok(())
    }

    async fn get_session(&self, key: &str) -> Result<SessionMetadata, BridgeError> {
        let thread_id = self.sessions.get(key).await?.ok_or_else(|| {
            BridgeError::new(ErrorCode::Session, "persistent session was not found")
                .with_provider("codex-app-server")
        })?;
        Ok(SessionMetadata {
            key: Some(key.to_owned()),
            thread_id: Some(thread_id),
            reused: true,
        })
    }

    async fn delete_session(&self, key: &str) -> Result<(), BridgeError> {
        if self.sessions.get(key).await?.is_none() {
            return Err(
                BridgeError::new(ErrorCode::Session, "persistent session was not found")
                    .with_provider("codex-app-server"),
            );
        }
        self.sessions.delete(key).await
    }

    fn restart_count(&self) -> Option<u64> {
        self.process
            .as_ref()
            .map(|process| process.generation().saturating_sub(1))
    }

    async fn shutdown(&self) -> Result<(), BridgeError> {
        if let Some(process) = &self.process {
            process.shutdown().await?;
        }
        Ok(())
    }
}

fn capabilities() -> ProviderCapabilities {
    let no_input = InputCapabilities {
        support: SupportLevel::Unsupported,
        max_count: 0,
        max_bytes_each: 0,
        max_bytes_total: 0,
    };
    let references = InputCapabilities {
        support: SupportLevel::Native,
        max_count: 5,
        max_bytes_each: 32 * 1024 * 1024,
        max_bytes_total: 64 * 1024 * 1024,
    };
    ProviderCapabilities {
        provider: "codex-app-server".to_owned(),
        implementation_version: env!("CARGO_PKG_VERSION").to_owned(),
        model: Some("gpt-image-2".to_owned()),
        experimental: false,
        generation: true,
        edits: true,
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
        reference_images: references.clone(),
        edit_images: references,
        masks: no_input,
        partial_images: U8Range { min: 0, max: 0 },
        persistent_sessions: true,
        explicit_threads: true,
    }
}

fn request_images(request: &ImageRequest) -> Vec<imagegen_bridge_core::ImageInput> {
    match &request.operation {
        ImageOperation::Generate { reference_images } => reference_images.clone(),
        ImageOperation::Edit {
            images,
            reference_images,
            ..
        } => images.iter().chain(reference_images).cloned().collect(),
    }
}

fn turn_prompt(prompt: &str, paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        format!("Generate this image now with image_gen.imagegen: {prompt}")
    } else {
        let paths = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Generate or edit this image now with image_gen.imagegen. Use referenced_image_paths exactly as listed: [{paths}]. Prompt: {prompt}"
        )
    }
}

fn string_at(value: &Value, path: &[&str], message: &str) -> Result<String, BridgeError> {
    let mut value = value;
    for component in path {
        value = &value[*component];
    }
    value.as_str().map(str::to_owned).ok_or_else(|| {
        BridgeError::new(ErrorCode::Protocol, message).with_provider("codex-app-server")
    })
}

fn belongs_to(params: &Value, thread_id: &str, turn_id: &str) -> bool {
    params["threadId"] == thread_id
        && (params["turnId"] == turn_id || params["turn"]["id"] == turn_id)
}

fn notification_error(error: &broadcast::error::RecvError) -> BridgeError {
    match error {
        broadcast::error::RecvError::Closed => {
            BridgeError::new(ErrorCode::Protocol, "app-server notification stream closed")
        }
        broadcast::error::RecvError::Lagged(_) => BridgeError::new(
            ErrorCode::Overloaded,
            "app-server notification consumer lagged behind",
        ),
    }
    .with_provider("codex-app-server")
}

fn turn_failure_error(turn: &Value, request: &ImageRequest) -> BridgeError {
    let upstream_code = turn["error"]["code"]
        .as_str()
        .or_else(|| turn["errorCode"].as_str())
        .and_then(safe_failure_label);
    let upstream_status = turn["status"].as_str().and_then(safe_failure_label);
    let safety_rejected = upstream_code.is_some_and(is_safety_failure_label)
        || upstream_status.is_some_and(is_safety_failure_label);
    let mut error = if safety_rejected {
        BridgeError::safety_rejected("Codex app-server rejected the image request")
            .with_detail("requested_moderation", request.parameters.moderation)
            .with_detail("input_images_present", !request_images(request).is_empty())
    } else {
        BridgeError::new(
            ErrorCode::Upstream,
            "Codex image generation turn did not complete successfully",
        )
    }
    .with_provider("codex-app-server");
    if let Some(code) = upstream_code {
        error = error.with_detail("upstream_code", code);
    }
    if let Some(status) = upstream_status {
        error = error.with_detail("upstream_status", status);
    }
    error
}

fn safe_failure_label(value: &str) -> Option<&str> {
    (!value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    .then_some(value)
}

fn is_safety_failure_label(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("safety")
        || lower.contains("content_policy")
        || lower.contains("moderation")
        || lower.contains("refusal")
}

fn unknown_app_server_outcome(code: ErrorCode, message: &str) -> BridgeError {
    BridgeError::new(code, message)
        .with_provider("codex-app-server")
        .retryable(false)
        .with_detail("outcome", "unknown")
}

struct TurnAccumulator {
    images: Vec<String>,
    revised_prompt: Option<String>,
    completed_item_ids: HashSet<String>,
    result_bytes: usize,
    max_images: usize,
    max_result_bytes: usize,
}

impl TurnAccumulator {
    fn capture(&mut self, item: &Value) -> Result<(), BridgeError> {
        let item_id = item["id"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| app_server_protocol_error("completed image has no item ID"))?;
        if self.completed_item_ids.contains(item_id) {
            return Err(app_server_protocol_error(
                "app-server returned a duplicate completed image item",
            ));
        }
        if self.images.len() >= self.max_images {
            return Err(app_server_protocol_error(
                "app-server returned more images than negotiated",
            ));
        }
        let result = item["result"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| app_server_protocol_error("completed image has no result"))?;
        let next_result_bytes = self
            .result_bytes
            .checked_add(result.len())
            .ok_or_else(|| app_server_protocol_error("aggregate image result size overflowed"))?;
        if next_result_bytes > self.max_result_bytes {
            return Err(app_server_protocol_error(
                "aggregate image results exceed the byte limit",
            ));
        }
        let revised = item["revisedPrompt"]
            .as_str()
            .or_else(|| item["revised_prompt"].as_str())
            .filter(|value| !value.is_empty());
        if revised.is_some_and(|value| value.len() > MAX_REVISED_PROMPT_BYTES) {
            return Err(app_server_protocol_error(
                "Codex revised prompt exceeds the configured limit",
            ));
        }
        self.completed_item_ids.insert(item_id.to_owned());
        self.result_bytes = next_result_bytes;
        self.images.push(result.to_owned());
        if let Some(revised) = revised {
            self.revised_prompt = Some(revised.to_owned());
        }
        Ok(())
    }
}

fn app_server_protocol_error(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message).with_provider("codex-app-server")
}

struct TurnImages {
    images: Vec<String>,
    revised_prompt: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::{fs, time::Duration};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio::io::duplex;
    use tokio_util::{
        codec::{Framed, LinesCodec},
        sync::CancellationToken,
    };

    use super::*;
    use crate::RpcConfig;

    const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn turn_safety_failures_are_classified_without_reflecting_messages() {
        let mut request = ImageRequest::generate("safe fixture");
        request.parameters.moderation = Moderation::Auto;
        let error = turn_failure_error(
            &json!({
                "status": "failed",
                "error": {
                    "code": "content_policy_violation",
                    "message": "secret upstream prompt detail"
                }
            }),
            &request,
        );
        assert_eq!(error.code, ErrorCode::SafetyRejected);
        assert_eq!(error.details["recovery"], "revise_prompt_or_inputs");
        assert_eq!(error.details["requested_moderation"], "auto");
        assert!(!error.message.contains("secret"));
    }

    #[test]
    fn turn_accumulator_bounds_count_identity_bytes_and_revised_prompt() {
        fn accumulator(max_images: usize, max_result_bytes: usize) -> TurnAccumulator {
            TurnAccumulator {
                images: Vec::new(),
                revised_prompt: None,
                completed_item_ids: HashSet::new(),
                result_bytes: 0,
                max_images,
                max_result_bytes,
            }
        }

        let first = json!({
            "id": "image-1",
            "result": "1234",
            "revisedPrompt": "safe"
        });
        let mut bounded = accumulator(1, 4);
        bounded.capture(&first).unwrap();
        assert_eq!(bounded.images, ["1234"]);
        assert!(
            bounded
                .capture(&first)
                .unwrap_err()
                .message
                .contains("duplicate")
        );

        let second = json!({"id": "image-2", "result": "1"});
        assert!(
            bounded
                .capture(&second)
                .unwrap_err()
                .message
                .contains("more images")
        );

        let mut bytes = accumulator(2, 3);
        assert!(
            bytes
                .capture(&first)
                .unwrap_err()
                .message
                .contains("byte limit")
        );

        let mut revised = accumulator(1, 4);
        let revised_item = json!({
            "id": "image-1",
            "result": "1",
            "revisedPrompt": "x".repeat(MAX_REVISED_PROMPT_BYTES + 1)
        });
        assert!(
            revised
                .capture(&revised_item)
                .unwrap_err()
                .message
                .contains("revised prompt")
        );
    }

    async fn rpc_and_server() -> (
        Arc<AppServerRpc>,
        Framed<tokio::io::DuplexStream, LinesCodec>,
    ) {
        let (client, server) = duplex(128 * 1024);
        let (reader, writer) = tokio::io::split(client);
        let mut server = Framed::new(server, LinesCodec::new());
        let connection = tokio::spawn(AppServerRpc::connect(
            reader,
            writer,
            RpcConfig {
                max_message_bytes: 128 * 1024,
                max_notification_bytes: 128 * 1024,
                request_timeout: Duration::from_secs(2),
                notification_capacity: 16,
            },
        ));
        let initialize = next_message(&mut server).await;
        server
            .send(json!({"id": initialize["id"], "result": {}}).to_string())
            .await
            .unwrap();
        let rpc = connection.await.unwrap().unwrap();
        assert_eq!(next_message(&mut server).await["method"], "initialized");
        (rpc, server)
    }

    async fn next_message(server: &mut Framed<tokio::io::DuplexStream, LinesCodec>) -> Value {
        serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap()
    }

    fn request() -> ImageRequest {
        let mut request = ImageRequest::generate("a tiny blue square");
        request.session.mode = SessionMode::Persistent;
        request.session.key = Some("gallery".to_owned());
        request
    }

    fn context(id: &str) -> ProviderContext {
        ProviderContext {
            request_id: id.to_owned(),
            deadline: Instant::now() + Duration::from_secs(2),
            cancellation: CancellationToken::new(),
            events: None,
        }
    }

    async fn complete_turn(
        server: &mut Framed<tokio::io::DuplexStream, LinesCodec>,
        thread_id: &str,
        turn_id: &str,
    ) {
        server
            .send(
                json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "completedAtMs": 1,
                        "item": {
                            "type": "imageGeneration",
                            "id": "image-1",
                            "status": "completed",
                            "revisedPrompt": "a tiny blue square",
                            "result": ONE_PIXEL_PNG
                        }
                    }
                })
                .to_string(),
            )
            .await
            .unwrap();
        server
            .send(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": thread_id,
                        "turn": {"id": turn_id, "status": "completed", "items": []}
                    }
                })
                .to_string(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn persistent_requests_start_once_then_resume_the_same_thread() {
        let (rpc, mut server) = rpc_and_server().await;
        let provider = Arc::new(AppServerImageProvider::new(
            rpc,
            Arc::new(MemorySessionBindingStore::default()),
            AppServerProviderConfig {
                codex_model: None,
                cwd: PathBuf::from("/tmp"),
                image_limits: ImageLimits::default(),
            },
        ));

        let first = tokio::spawn({
            let provider = Arc::clone(&provider);
            async move { provider.execute(request(), context("request-1")).await }
        });
        let start = next_message(&mut server).await;
        assert_eq!(start["method"], "thread/start");
        assert_eq!(start["params"]["ephemeral"], false);
        server
            .send(json!({"id": start["id"], "result": {"thread": {"id": "thread-1"}}}).to_string())
            .await
            .unwrap();
        let turn = next_message(&mut server).await;
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(turn["params"]["threadId"], "thread-1");
        server
            .send(json!({"id": turn["id"], "result": {"turn": {"id": "turn-1"}}}).to_string())
            .await
            .unwrap();
        complete_turn(&mut server, "thread-1", "turn-1").await;
        let first = first.await.unwrap().unwrap();
        assert!(!first.session.unwrap().reused);
        assert_eq!(first.data.len(), 1);

        let second = tokio::spawn({
            let provider = Arc::clone(&provider);
            async move { provider.execute(request(), context("request-2")).await }
        });
        let resume = next_message(&mut server).await;
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "thread-1");
        server
            .send(json!({"id": resume["id"], "result": {"thread": {"id": "thread-1"}}}).to_string())
            .await
            .unwrap();
        let turn = next_message(&mut server).await;
        server
            .send(json!({"id": turn["id"], "result": {"turn": {"id": "turn-2"}}}).to_string())
            .await
            .unwrap();
        complete_turn(&mut server, "thread-1", "turn-2").await;
        let second = second.await.unwrap().unwrap();
        assert!(second.session.unwrap().reused);
    }

    #[tokio::test]
    async fn stages_inline_references_under_bridge_owned_storage() {
        let (rpc, _server) = rpc_and_server().await;
        let directory = tempfile::tempdir().unwrap();
        let input_root = directory.path().join("inputs");
        let staging_root = directory.path().join("staging");
        fs::create_dir(&input_root).unwrap();
        let limits = ImageLimits::default();
        let reference_inputs = AppServerReferenceInputs::new(
            Arc::new(InputLoader::new([input_root], limits).unwrap()),
            None,
            Arc::new(ArtifactStore::new(&staging_root, limits).unwrap()),
            1024 * 1024,
        )
        .unwrap();
        let provider = AppServerImageProvider::new_with_inputs(
            rpc,
            Arc::new(MemorySessionBindingStore::default()),
            AppServerProviderConfig {
                codex_model: None,
                cwd: directory.path().to_owned(),
                image_limits: limits,
            },
            reference_inputs,
        );
        let mut request = request();
        request.operation = ImageOperation::Generate {
            reference_images: vec![imagegen_bridge_core::ImageInput {
                source: imagegen_bridge_core::ImageSource::Base64 {
                    data: ONE_PIXEL_PNG.to_owned(),
                },
                media_type: Some("image/png".to_owned()),
                filename: Some("visual reference.png".to_owned()),
            }],
        };

        let paths = provider.reference_paths(&request).await.unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].starts_with(staging_root.canonicalize().unwrap()));
        assert!(paths[0].is_file());
        assert!(!paths[0].to_string_lossy().contains("visual reference"));
    }

    #[tokio::test]
    async fn rejects_references_when_secure_staging_is_not_configured() {
        let (rpc, _server) = rpc_and_server().await;
        let provider = AppServerImageProvider::new(
            rpc,
            Arc::new(MemorySessionBindingStore::default()),
            AppServerProviderConfig {
                codex_model: None,
                cwd: PathBuf::from("/tmp"),
                image_limits: ImageLimits::default(),
            },
        );
        let mut request = request();
        request.operation = ImageOperation::Generate {
            reference_images: vec![imagegen_bridge_core::ImageInput {
                source: imagegen_bridge_core::ImageSource::File {
                    path: PathBuf::from("outside.png"),
                },
                media_type: None,
                filename: None,
            }],
        };
        let error = provider.reference_paths(&request).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Input);
    }

    #[tokio::test]
    async fn session_lifecycle_is_visible_without_exposing_other_state() {
        let (rpc, _server) = rpc_and_server().await;
        let sessions = Arc::new(MemorySessionBindingStore::default());
        sessions.put("gallery", "thread-1").await.unwrap();
        let provider = AppServerImageProvider::new(
            rpc,
            sessions,
            AppServerProviderConfig {
                codex_model: None,
                cwd: PathBuf::from("/tmp"),
                image_limits: ImageLimits::default(),
            },
        );
        let session = provider.get_session("gallery").await.unwrap();
        assert_eq!(session.key.as_deref(), Some("gallery"));
        assert_eq!(session.thread_id.as_deref(), Some("thread-1"));
        provider.delete_session("gallery").await.unwrap();
        assert_eq!(
            provider.get_session("gallery").await.unwrap_err().code,
            ErrorCode::Session
        );
    }

    #[tokio::test]
    async fn waiting_on_the_same_session_is_cancellable_without_starting_a_turn() {
        let (rpc, mut server) = rpc_and_server().await;
        let provider = Arc::new(AppServerImageProvider::new(
            rpc,
            Arc::new(MemorySessionBindingStore::default()),
            AppServerProviderConfig {
                codex_model: None,
                cwd: PathBuf::from("/tmp"),
                image_limits: ImageLimits::default(),
            },
        ));
        let first = tokio::spawn({
            let provider = Arc::clone(&provider);
            async move { provider.execute(request(), context("first")).await }
        });
        let start = next_message(&mut server).await;
        server
            .send(json!({"id": start["id"], "result": {"thread": {"id": "thread-1"}}}).to_string())
            .await
            .unwrap();
        let turn = next_message(&mut server).await;
        server
            .send(json!({"id": turn["id"], "result": {"turn": {"id": "turn-1"}}}).to_string())
            .await
            .unwrap();

        let cancellation = CancellationToken::new();
        let second = tokio::spawn({
            let provider = Arc::clone(&provider);
            let cancellation = cancellation.clone();
            async move {
                provider
                    .execute(
                        request(),
                        ProviderContext {
                            request_id: "second".to_owned(),
                            deadline: Instant::now() + Duration::from_secs(2),
                            cancellation,
                            events: None,
                        },
                    )
                    .await
            }
        });
        tokio::task::yield_now().await;
        cancellation.cancel();
        let error = second.await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);

        complete_turn(&mut server, "thread-1", "turn-1").await;
        assert!(first.await.unwrap().is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_restart_resumes_the_durable_thread_without_starting_a_new_chat() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("fake-codex");
        let state = directory.path().join("started-once");
        let source = format!(
            r#"#!/bin/sh
STATE='{}'
if [ -e "$STATE" ]; then
  RUN=2
else
  : > "$STATE"
  RUN=1
fi
while IFS= read -r LINE; do
  case "$LINE" in
    *'"method":"initialize"'*)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-1"}}}}}}'
      ;;
    *'"method":"turn/start"'*)
      if [ "$RUN" -eq 1 ]; then
        exit 17
      fi
      printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"turn-2"}}}}}}'
      printf '%s\n' '{{"method":"item/completed","params":{{"threadId":"thread-1","turnId":"turn-2","item":{{"type":"imageGeneration","id":"image-1","status":"completed","revisedPrompt":"resumed prompt","result":"{}"}}}}}}'
      printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"thread-1","turn":{{"id":"turn-2","status":"completed","items":[]}}}}}}'
      ;;
  esac
done
"#,
            state.display(),
            ONE_PIXEL_PNG
        );
        fs::write(&script, source).unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).unwrap();

        let process = Arc::new(
            CodexProcess::spawn(crate::CodexProcessConfig {
                executable: script,
                cwd: Some(directory.path().to_owned()),
                rpc: RpcConfig {
                    request_timeout: Duration::from_secs(2),
                    ..RpcConfig::default()
                },
                restart_backoff: Duration::ZERO,
                ..crate::CodexProcessConfig::default()
            })
            .await
            .unwrap(),
        );
        let sessions = Arc::new(MemorySessionBindingStore::default());
        sessions.put("gallery", "thread-1").await.unwrap();
        let provider = AppServerImageProvider::with_process(
            Arc::clone(&process),
            sessions,
            AppServerProviderConfig {
                codex_model: None,
                cwd: directory.path().to_owned(),
                image_limits: ImageLimits::default(),
            },
        );

        let first = provider
            .execute(request(), context("crashing-request"))
            .await
            .unwrap_err();
        assert_eq!(first.code, ErrorCode::Protocol);
        let second = provider
            .execute(request(), context("resumed-request"))
            .await
            .unwrap();
        let session = second.session.unwrap();
        assert!(session.reused);
        assert_eq!(session.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(process.generation(), 2);
        provider.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn deadline_interrupts_an_active_turn() {
        let (rpc, mut server) = rpc_and_server().await;
        let provider = Arc::new(AppServerImageProvider::new(
            rpc,
            Arc::new(MemorySessionBindingStore::default()),
            AppServerProviderConfig {
                codex_model: None,
                cwd: PathBuf::from("/tmp"),
                image_limits: ImageLimits::default(),
            },
        ));
        let execution = tokio::spawn({
            let provider = Arc::clone(&provider);
            async move {
                provider
                    .execute(
                        ImageRequest::generate("timeout"),
                        ProviderContext {
                            request_id: "timeout".to_owned(),
                            deadline: Instant::now() + Duration::from_millis(20),
                            cancellation: CancellationToken::new(),
                            events: None,
                        },
                    )
                    .await
            }
        });
        let start = next_message(&mut server).await;
        assert_eq!(start["method"], "thread/start");
        server
            .send(
                json!({"id": start["id"], "result": {"thread": {"id": "thread-timeout"}}})
                    .to_string(),
            )
            .await
            .unwrap();
        let turn = next_message(&mut server).await;
        server
            .send(json!({"id": turn["id"], "result": {"turn": {"id": "turn-timeout"}}}).to_string())
            .await
            .unwrap();
        let interrupt = next_message(&mut server).await;
        assert_eq!(interrupt["method"], "turn/interrupt");
        server
            .send(json!({"id": interrupt["id"], "result": {}}).to_string())
            .await
            .unwrap();
        server
            .send(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-timeout",
                        "turn": {"id": "turn-timeout", "status": "interrupted"}
                    }
                })
                .to_string(),
            )
            .await
            .unwrap();
        let archive = next_message(&mut server).await;
        assert_eq!(archive["method"], "thread/archive");
        server
            .send(json!({"id": archive["id"], "result": {}}).to_string())
            .await
            .unwrap();
        let error = execution.await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout);
        assert!(error.retryable);
        assert_eq!(error.details["outcome"], "cancelled");
    }

    #[tokio::test]
    async fn cancellation_interrupts_an_active_turn() {
        let (rpc, mut server) = rpc_and_server().await;
        let provider = Arc::new(AppServerImageProvider::new(
            rpc,
            Arc::new(MemorySessionBindingStore::default()),
            AppServerProviderConfig {
                codex_model: None,
                cwd: PathBuf::from("/tmp"),
                image_limits: ImageLimits::default(),
            },
        ));
        let cancellation = CancellationToken::new();
        let execution = tokio::spawn({
            let provider = Arc::clone(&provider);
            let cancellation = cancellation.clone();
            async move {
                provider
                    .execute(
                        ImageRequest::generate("cancel"),
                        ProviderContext {
                            request_id: "cancel".to_owned(),
                            deadline: Instant::now() + Duration::from_secs(2),
                            cancellation,
                            events: None,
                        },
                    )
                    .await
            }
        });
        let start = next_message(&mut server).await;
        server
            .send(
                json!({"id": start["id"], "result": {"thread": {"id": "thread-cancel"}}})
                    .to_string(),
            )
            .await
            .unwrap();
        let turn = next_message(&mut server).await;
        server
            .send(json!({"id": turn["id"], "result": {"turn": {"id": "turn-cancel"}}}).to_string())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        cancellation.cancel();
        let interrupt = next_message(&mut server).await;
        assert_eq!(interrupt["method"], "turn/interrupt");
        assert_eq!(interrupt["params"]["threadId"], "thread-cancel");
        server
            .send(json!({"id": interrupt["id"], "result": {}}).to_string())
            .await
            .unwrap();
        server
            .send(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-cancel",
                        "turn": {"id": "turn-cancel", "status": "interrupted"}
                    }
                })
                .to_string(),
            )
            .await
            .unwrap();
        let archive = next_message(&mut server).await;
        assert_eq!(archive["method"], "thread/archive");
        server
            .send(json!({"id": archive["id"], "result": {}}).to_string())
            .await
            .unwrap();
        let error = execution.await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert!(!error.retryable);
        assert_eq!(error.details["outcome"], "cancelled");
    }

    #[tokio::test]
    async fn completed_turn_without_a_valid_image_is_rejected() {
        let (rpc, mut server) = rpc_and_server().await;
        let provider = Arc::new(AppServerImageProvider::new(
            rpc,
            Arc::new(MemorySessionBindingStore::default()),
            AppServerProviderConfig {
                codex_model: None,
                cwd: PathBuf::from("/tmp"),
                image_limits: ImageLimits::default(),
            },
        ));
        let execution = tokio::spawn({
            let provider = Arc::clone(&provider);
            async move {
                provider
                    .execute(ImageRequest::generate("missing"), context("missing"))
                    .await
            }
        });
        let start = next_message(&mut server).await;
        server
            .send(
                json!({"id": start["id"], "result": {"thread": {"id": "thread-missing"}}})
                    .to_string(),
            )
            .await
            .unwrap();
        let turn = next_message(&mut server).await;
        server
            .send(json!({"id": turn["id"], "result": {"turn": {"id": "turn-missing"}}}).to_string())
            .await
            .unwrap();
        server
            .send(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-missing",
                        "turn": {"id": "turn-missing", "status": "completed", "items": []}
                    }
                })
                .to_string(),
            )
            .await
            .unwrap();
        let archive = next_message(&mut server).await;
        server
            .send(json!({"id": archive["id"], "result": {}}).to_string())
            .await
            .unwrap();
        let error = execution.await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Upstream);
    }

    #[tokio::test]
    #[ignore = "requires authenticated Codex OAuth and performs a real image generation"]
    async fn live_codex_generates_a_verified_image() {
        if std::env::var("IMAGEGEN_BRIDGE_LIVE_CODEX").as_deref() != Ok("1") {
            return;
        }
        let process = Arc::new(
            CodexProcess::spawn(crate::CodexProcessConfig {
                rpc: RpcConfig {
                    request_timeout: Duration::from_secs(30),
                    ..RpcConfig::default()
                },
                ..crate::CodexProcessConfig::default()
            })
            .await
            .unwrap(),
        );
        let provider = AppServerImageProvider::with_process(
            Arc::clone(&process),
            Arc::new(MemorySessionBindingStore::default()),
            AppServerProviderConfig {
                codex_model: None,
                cwd: std::env::current_dir().unwrap(),
                image_limits: ImageLimits::default(),
            },
        );
        provider.check_ready().await.unwrap();
        let mut live_request = ImageRequest::generate(
            "A single cobalt-blue circle centered on a plain white background",
        );
        live_request.session.mode = SessionMode::Persistent;
        live_request.session.key = Some("live-persistent-session".to_owned());
        let first = provider
            .execute(
                live_request.clone(),
                ProviderContext {
                    request_id: "live-test-1".to_owned(),
                    deadline: Instant::now() + Duration::from_secs(240),
                    cancellation: CancellationToken::new(),
                    events: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(first.data.len(), 1);
        assert!(first.data[0].bytes > 0);
        let first_session = first.session.unwrap();
        assert!(!first_session.reused);

        let second = provider
            .execute(
                live_request,
                ProviderContext {
                    request_id: "live-test-2".to_owned(),
                    deadline: Instant::now() + Duration::from_secs(240),
                    cancellation: CancellationToken::new(),
                    events: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(second.data.len(), 1);
        assert!(second.data[0].bytes > 0);
        let second_session = second.session.unwrap();
        assert!(second_session.reused);
        assert_eq!(first_session.thread_id, second_session.thread_id);
        provider.shutdown().await.unwrap();
    }
}
