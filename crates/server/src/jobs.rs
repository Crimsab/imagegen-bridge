//! Durable asynchronous job execution over the shared runtime.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use imagegen_bridge_artifacts::{ImageLimits, inspect_image};
use imagegen_bridge_config::JobSettings;
use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImageJob, ImageJobStatus, ImageJobSummary, ImageRequest, OutputFormat,
    ProviderEvent, ResponseFormat,
};
use imagegen_bridge_runtime::{
    ExecutionContext, ImageJobListFilter, ImagegenRuntime, SqliteImageJobStore, SqliteJobSubmission,
};
use serde::Serialize;
use tokio::sync::{Mutex, Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_PARTIAL_PREVIEW_BYTES: usize = 16 * 1024 * 1024;
const MAX_PARTIAL_PREVIEW_BYTES_U64: u64 = 16 * 1024 * 1024;
const MAX_PARTIAL_PREVIEW_BASE64_CHARS: usize =
    (MAX_PARTIAL_PREVIEW_BYTES.saturating_add(2) / 3).saturating_mul(4);

/// Latest verified transient preview for one running durable job.
#[derive(Debug, Clone)]
pub(crate) struct PartialPreview {
    pub(crate) bytes: Bytes,
    pub(crate) content_type: &'static str,
    pub(crate) output_index: u8,
    pub(crate) partial_index: u8,
}

/// Owns the bounded durable queue and its active cancellation handles.
pub struct JobManager {
    runtime: Arc<ImagegenRuntime>,
    store: Arc<SqliteImageJobStore>,
    settings: JobSettings,
    permits: Arc<Semaphore>,
    shutdown: CancellationToken,
    active: Mutex<BTreeMap<(String, String), CancellationToken>>,
    partials: Mutex<BTreeMap<(String, String), PartialPreview>>,
    active_changed: Notify,
    retention_healthy: AtomicBool,
}

/// Redaction-safe durable queue diagnostics for operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct JobManagerDiagnostics {
    /// Current aggregate durable job state.
    #[serde(flatten)]
    pub statistics: imagegen_bridge_runtime::SqliteJobStatistics,
    /// Active worker tasks tracked by this process.
    pub active_workers: usize,
    /// Maximum retained queued work accepted by admission.
    pub max_pending: usize,
    /// Maximum simultaneous durable workers.
    pub max_running: usize,
    /// Completed job retention window in seconds.
    pub retention_secs: u64,
    /// Maximum terminal rows retained after pruning.
    pub max_retained: usize,
}

impl std::fmt::Debug for JobManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JobManager")
            .field("settings", &self.settings)
            .field("shutdown", &self.shutdown.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl JobManager {
    /// Opens the durable queue, conservatively recovers active work, and resumes queued jobs.
    pub async fn open(
        runtime: Arc<ImagegenRuntime>,
        settings: JobSettings,
    ) -> Result<Arc<Self>, BridgeError> {
        let store = Arc::new(SqliteImageJobStore::open(&settings.database).await?);
        store.recover_interrupted(unix_timestamp()).await?;
        store
            .prune(
                unix_timestamp(),
                settings.retention_secs,
                settings.max_retained,
            )
            .await?;
        let manager = Arc::new(Self {
            runtime,
            store,
            permits: Arc::new(Semaphore::new(settings.max_running)),
            settings,
            shutdown: CancellationToken::new(),
            active: Mutex::new(BTreeMap::new()),
            partials: Mutex::new(BTreeMap::new()),
            active_changed: Notify::new(),
            retention_healthy: AtomicBool::new(true),
        });
        let queued = manager
            .store
            .queued_identities(manager.settings.max_pending)
            .await?;
        for (scope, id) in queued {
            manager.schedule(scope, id);
        }
        manager.spawn_retention_loop();
        Ok(manager)
    }

    /// Validates, normalizes for durable artifact delivery, persists, and schedules a request.
    pub async fn submit(
        self: &Arc<Self>,
        auth_scope: &str,
        mut request: ImageRequest,
    ) -> Result<SqliteJobSubmission, BridgeError> {
        if self.shutdown.is_cancelled() {
            return Err(BridgeError::new(
                ErrorCode::Overloaded,
                "durable job manager is shutting down",
            )
            .retryable(true));
        }
        if !self.retention_healthy.load(Ordering::Acquire) {
            return Err(BridgeError::new(
                ErrorCode::Overloaded,
                "durable job retention is temporarily unavailable",
            )
            .retryable(true));
        }
        request.output.response_format = ResponseFormat::Artifact;
        self.runtime.validate_request(&request)?;
        let id = Uuid::now_v7().to_string();
        let job = self
            .store
            .create(
                auth_scope,
                &id,
                &request,
                unix_timestamp(),
                self.settings.max_pending,
            )
            .await?;
        if job.created {
            self.schedule(auth_scope.to_owned(), id);
        }
        Ok(job)
    }

    /// Returns a complete durable job.
    pub async fn get(&self, auth_scope: &str, id: &str) -> Result<ImageJob, BridgeError> {
        self.store.get(auth_scope, id).await
    }

    /// Returns the latest verified preview retained only while a job is active.
    pub(crate) async fn partial_preview(
        &self,
        auth_scope: &str,
        id: &str,
    ) -> Option<PartialPreview> {
        self.partials
            .lock()
            .await
            .get(&(auth_scope.to_owned(), id.to_owned()))
            .cloned()
    }

    /// Returns newest-first durable job summaries.
    pub async fn list(
        &self,
        auth_scope: &str,
        filter: ImageJobListFilter,
    ) -> Result<Vec<ImageJobSummary>, BridgeError> {
        self.store.list(auth_scope, filter).await
    }

    /// Requests durable cancellation and signals active provider work.
    pub async fn cancel(&self, auth_scope: &str, id: &str) -> Result<ImageJob, BridgeError> {
        let job = self
            .store
            .request_cancel(auth_scope, id, unix_timestamp())
            .await?;
        if let Some(cancellation) = self
            .active
            .lock()
            .await
            .get(&(auth_scope.to_owned(), id.to_owned()))
            .cloned()
        {
            cancellation.cancel();
        }
        Ok(job)
    }

    /// Updates favorite and reversible history visibility fields.
    pub async fn update_history(
        &self,
        auth_scope: &str,
        id: &str,
        update: imagegen_bridge_core::ImageJobUpdate,
    ) -> Result<ImageJob, BridgeError> {
        self.store
            .update_history(
                auth_scope,
                id,
                update.favorite,
                update.deleted,
                unix_timestamp(),
            )
            .await
    }

    /// Returns aggregate queue and storage state without user content or paths.
    pub async fn diagnostics(&self) -> Result<JobManagerDiagnostics, BridgeError> {
        Ok(JobManagerDiagnostics {
            statistics: self.store.statistics().await?,
            active_workers: self.active.lock().await.len(),
            max_pending: self.settings.max_pending,
            max_running: self.settings.max_running,
            retention_secs: self.settings.retention_secs,
            max_retained: self.settings.max_retained,
        })
    }

    /// Stops queued dispatch and waits a bounded interval for active state to settle.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let wait = async {
            loop {
                let notified = self.active_changed.notified();
                if self.active.lock().await.is_empty() {
                    break;
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(15), wait).await;
    }

    fn spawn_retention_loop(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        let interval_secs = self.settings.retention_secs.saturating_div(2).clamp(1, 60);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(error) = manager.enforce_retention().await {
                            tracing::error!(error_code = ?error.code, "durable job retention failed");
                        }
                    }
                    () = manager.shutdown.cancelled() => break,
                }
            }
        });
    }

    async fn enforce_retention(&self) -> Result<(), BridgeError> {
        match self
            .store
            .prune(
                unix_timestamp(),
                self.settings.retention_secs,
                self.settings.max_retained,
            )
            .await
        {
            Ok(_) => {
                self.retention_healthy.store(true, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.retention_healthy.store(false, Ordering::Release);
                Err(error)
            }
        }
    }

    fn schedule(self: &Arc<Self>, auth_scope: String, id: String) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = manager.run(auth_scope, id.clone()).await {
                tracing::warn!(job_id = %id, error_code = ?error.code, "durable job worker failed");
            }
        });
    }

    async fn run(self: &Arc<Self>, auth_scope: String, id: String) -> Result<(), BridgeError> {
        let permit = tokio::select! {
            permit = Arc::clone(&self.permits).acquire_owned() => permit.map_err(|_| {
                BridgeError::new(ErrorCode::Internal, "durable job worker pool closed")
            })?,
            () = self.shutdown.cancelled() => return Ok(()),
        };
        if !self.store.claim(&auth_scope, &id, unix_timestamp()).await? {
            return Ok(());
        }

        let cancellation = self.shutdown.child_token();
        let key = (auth_scope.clone(), id.clone());
        self.active
            .lock()
            .await
            .insert(key.clone(), cancellation.clone());
        self.active_changed.notify_waiters();
        let result = self
            .execute_claimed(&auth_scope, &id, cancellation.clone())
            .await;
        self.active.lock().await.remove(&key);
        self.partials.lock().await.remove(&key);
        self.active_changed.notify_waiters();
        drop(permit);

        match result {
            Ok(response) => {
                self.store
                    .succeed(&auth_scope, &id, &response, unix_timestamp())
                    .await?;
                self.enforce_retention().await
            }
            Err(error) => {
                let status = if self.shutdown.is_cancelled() {
                    ImageJobStatus::Interrupted
                } else if cancellation.is_cancelled() {
                    ImageJobStatus::Cancelled
                } else {
                    ImageJobStatus::Failed
                };
                self.store
                    .fail(&auth_scope, &id, status, &error, unix_timestamp())
                    .await?;
                self.enforce_retention().await
            }
        }
    }

    async fn execute_claimed(
        &self,
        auth_scope: &str,
        id: &str,
        cancellation: CancellationToken,
    ) -> Result<imagegen_bridge_core::ImageResponse, BridgeError> {
        let job = self.store.get(auth_scope, id).await?;
        if job.cancel_requested {
            cancellation.cancel();
        }
        let context = ExecutionContext {
            request_id: Some(id.to_owned()),
            idempotency_scope: format!("job:{id}"),
            cancellation,
        };
        let (events, mut receiver) = mpsc::channel(16);
        let execution = self
            .runtime
            .execute_with_events(job.request, context, events);
        tokio::pin!(execution);
        let mut partial_images = 0_u32;
        loop {
            tokio::select! {
                biased;
                event = receiver.recv() => match event {
                    Some(event) => self.handle_job_event(auth_scope, id, event, &mut partial_images).await,
                    None => return execution.await,
                },
                result = &mut execution => {
                    while let Ok(event) = receiver.try_recv() {
                        self.handle_job_event(auth_scope, id, event, &mut partial_images).await;
                    }
                    return result;
                },
            }
        }
    }

    async fn handle_job_event(
        &self,
        auth_scope: &str,
        id: &str,
        event: ProviderEvent,
        partial_images: &mut u32,
    ) {
        match event {
            ProviderEvent::Started => {
                self.progress(auth_scope, id, "provider", *partial_images)
                    .await;
            }
            ProviderEvent::Progress { .. } => {
                self.progress(auth_scope, id, "provider_progress", *partial_images)
                    .await;
            }
            ProviderEvent::PartialImage {
                index,
                partial_index,
                b64_json,
            } => {
                *partial_images = partial_images.saturating_add(1);
                if let Err(error) = self
                    .cache_partial(auth_scope, id, index, partial_index, b64_json)
                    .await
                {
                    tracing::warn!(job_id = %id, error_code = ?error.code, "ignored invalid partial preview");
                }
                self.progress(auth_scope, id, "partial_image", *partial_images)
                    .await;
            }
            ProviderEvent::Completed { .. } => {
                self.progress(auth_scope, id, "materializing", *partial_images)
                    .await;
            }
        }
    }

    async fn progress(&self, auth_scope: &str, id: &str, stage: &str, partial_images: u32) {
        if let Err(error) = self
            .store
            .progress(auth_scope, id, stage, partial_images, unix_timestamp())
            .await
        {
            tracing::warn!(job_id = %id, error_code = ?error.code, "could not persist job progress");
        }
    }

    async fn cache_partial(
        &self,
        auth_scope: &str,
        id: &str,
        output_index: u8,
        partial_index: u8,
        b64_json: String,
    ) -> Result<(), BridgeError> {
        if b64_json.len() > MAX_PARTIAL_PREVIEW_BASE64_CHARS {
            return Err(BridgeError::new(
                ErrorCode::Input,
                "partial preview exceeds the in-memory preview limit",
            ));
        }
        let preview = tokio::task::spawn_blocking(move || {
            let bytes = STANDARD.decode(b64_json).map_err(|_| {
                BridgeError::new(ErrorCode::Input, "partial preview is not valid base64")
            })?;
            let metadata = inspect_image(
                &bytes,
                ImageLimits {
                    max_encoded_bytes: MAX_PARTIAL_PREVIEW_BYTES_U64,
                    ..ImageLimits::default()
                },
            )?;
            let content_type = match metadata.format {
                OutputFormat::Png => "image/png",
                OutputFormat::Jpeg => "image/jpeg",
                OutputFormat::Webp => "image/webp",
            };
            Ok::<_, BridgeError>(PartialPreview {
                bytes: Bytes::from(bytes),
                content_type,
                output_index,
                partial_index,
            })
        })
        .await
        .map_err(|_| {
            BridgeError::new(
                ErrorCode::Internal,
                "partial preview validation task failed",
            )
        })??;
        self.partials
            .lock()
            .await
            .insert((auth_scope.to_owned(), id.to_owned()), preview);
        Ok(())
    }
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
