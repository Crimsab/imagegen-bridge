//! Durable asynchronous job execution over the shared runtime.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use imagegen_bridge_artifacts::{ImageLimits, inspect_image};
use imagegen_bridge_config::JobSettings;
use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImageJob, ImageJobStatus, ImageJobSummary, ImageRequest, OutputFormat,
    ProviderEvent, ResponseFormat,
};
use imagegen_bridge_runtime::{
    ExecutionContext, ImageJobListFilter, ImagegenRuntime, SqliteImageJobStore,
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
    active: Mutex<BTreeMap<String, CancellationToken>>,
    partials: Mutex<BTreeMap<String, PartialPreview>>,
    active_changed: Notify,
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
        });
        let mut queued = manager
            .store
            .list(ImageJobListFilter {
                limit: manager.settings.max_pending,
                status: Some(ImageJobStatus::Queued),
                ..ImageJobListFilter::default()
            })
            .await?;
        queued.reverse();
        for job in queued {
            manager.schedule(job.id);
        }
        Ok(manager)
    }

    /// Validates, normalizes for durable artifact delivery, persists, and schedules a request.
    pub async fn submit(
        self: &Arc<Self>,
        mut request: ImageRequest,
    ) -> Result<ImageJob, BridgeError> {
        if self.shutdown.is_cancelled() {
            return Err(BridgeError::new(
                ErrorCode::Overloaded,
                "durable job manager is shutting down",
            )
            .retryable(true));
        }
        request.output.response_format = ResponseFormat::Artifact;
        self.runtime.validate_request(&request)?;
        let id = Uuid::now_v7().to_string();
        let job = self
            .store
            .create(&id, &request, unix_timestamp(), self.settings.max_pending)
            .await?;
        self.schedule(id);
        Ok(job)
    }

    /// Returns a complete durable job.
    pub async fn get(&self, id: &str) -> Result<ImageJob, BridgeError> {
        self.store.get(id).await
    }

    /// Returns the latest verified preview retained only while a job is active.
    pub(crate) async fn partial_preview(&self, id: &str) -> Option<PartialPreview> {
        self.partials.lock().await.get(id).cloned()
    }

    /// Returns newest-first durable job summaries.
    pub async fn list(
        &self,
        filter: ImageJobListFilter,
    ) -> Result<Vec<ImageJobSummary>, BridgeError> {
        self.store.list(filter).await
    }

    /// Requests durable cancellation and signals active provider work.
    pub async fn cancel(&self, id: &str) -> Result<ImageJob, BridgeError> {
        let job = self.store.request_cancel(id, unix_timestamp()).await?;
        if let Some(cancellation) = self.active.lock().await.get(id).cloned() {
            cancellation.cancel();
        }
        Ok(job)
    }

    /// Updates favorite and reversible history visibility fields.
    pub async fn update_history(
        &self,
        id: &str,
        update: imagegen_bridge_core::ImageJobUpdate,
    ) -> Result<ImageJob, BridgeError> {
        self.store
            .update_history(id, update.favorite, update.deleted, unix_timestamp())
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

    fn schedule(self: &Arc<Self>, id: String) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = manager.run(id.clone()).await {
                tracing::warn!(job_id = %id, error_code = ?error.code, "durable job worker failed");
            }
        });
    }

    async fn run(self: &Arc<Self>, id: String) -> Result<(), BridgeError> {
        let permit = tokio::select! {
            permit = Arc::clone(&self.permits).acquire_owned() => permit.map_err(|_| {
                BridgeError::new(ErrorCode::Internal, "durable job worker pool closed")
            })?,
            () = self.shutdown.cancelled() => return Ok(()),
        };
        if !self.store.claim(&id, unix_timestamp()).await? {
            return Ok(());
        }

        let cancellation = self.shutdown.child_token();
        self.active
            .lock()
            .await
            .insert(id.clone(), cancellation.clone());
        self.active_changed.notify_waiters();
        let result = self.execute_claimed(&id, cancellation.clone()).await;
        self.active.lock().await.remove(&id);
        self.partials.lock().await.remove(&id);
        self.active_changed.notify_waiters();
        drop(permit);

        match result {
            Ok(response) => self.store.succeed(&id, &response, unix_timestamp()).await,
            Err(error) => {
                let status = if self.shutdown.is_cancelled() {
                    ImageJobStatus::Interrupted
                } else if cancellation.is_cancelled() {
                    ImageJobStatus::Cancelled
                } else {
                    ImageJobStatus::Failed
                };
                self.store.fail(&id, status, &error, unix_timestamp()).await
            }
        }
    }

    async fn execute_claimed(
        &self,
        id: &str,
        cancellation: CancellationToken,
    ) -> Result<imagegen_bridge_core::ImageResponse, BridgeError> {
        let job = self.store.get(id).await?;
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
                    Some(event) => self.handle_job_event(id, event, &mut partial_images).await,
                    None => return execution.await,
                },
                result = &mut execution => {
                    while let Ok(event) = receiver.try_recv() {
                        self.handle_job_event(id, event, &mut partial_images).await;
                    }
                    return result;
                },
            }
        }
    }

    async fn handle_job_event(&self, id: &str, event: ProviderEvent, partial_images: &mut u32) {
        match event {
            ProviderEvent::Started => self.progress(id, "provider", *partial_images).await,
            ProviderEvent::Progress { .. } => {
                self.progress(id, "provider_progress", *partial_images)
                    .await;
            }
            ProviderEvent::PartialImage {
                index,
                partial_index,
                b64_json,
            } => {
                *partial_images = partial_images.saturating_add(1);
                if let Err(error) = self.cache_partial(id, index, partial_index, b64_json).await {
                    tracing::warn!(job_id = %id, error_code = ?error.code, "ignored invalid partial preview");
                }
                self.progress(id, "partial_image", *partial_images).await;
            }
            ProviderEvent::Completed { .. } => {
                self.progress(id, "materializing", *partial_images).await;
            }
        }
    }

    async fn progress(&self, id: &str, stage: &str, partial_images: u32) {
        if let Err(error) = self
            .store
            .progress(id, stage, partial_images, unix_timestamp())
            .await
        {
            tracing::warn!(job_id = %id, error_code = ?error.code, "could not persist job progress");
        }
    }

    async fn cache_partial(
        &self,
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
        self.partials.lock().await.insert(id.to_owned(), preview);
        Ok(())
    }
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
