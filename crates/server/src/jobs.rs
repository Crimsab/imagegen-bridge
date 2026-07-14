//! Durable asynchronous job execution over the shared runtime.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use imagegen_bridge_config::JobSettings;
use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImageJob, ImageJobStatus, ImageJobSummary, ImageRequest, ProviderEvent,
    ResponseFormat,
};
use imagegen_bridge_runtime::{ExecutionContext, ImagegenRuntime, SqliteImageJobStore};
use tokio::sync::{Mutex, Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Owns the bounded durable queue and its active cancellation handles.
pub struct JobManager {
    runtime: Arc<ImagegenRuntime>,
    store: Arc<SqliteImageJobStore>,
    settings: JobSettings,
    permits: Arc<Semaphore>,
    shutdown: CancellationToken,
    active: Mutex<BTreeMap<String, CancellationToken>>,
    active_changed: Notify,
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
            active_changed: Notify::new(),
        });
        let mut queued = manager
            .store
            .list(
                None,
                manager.settings.max_pending,
                false,
                Some(ImageJobStatus::Queued),
            )
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

    /// Returns newest-first durable job summaries.
    pub async fn list(
        &self,
        before: Option<(u64, String)>,
        limit: usize,
        include_deleted: bool,
        status: Option<ImageJobStatus>,
    ) -> Result<Vec<ImageJobSummary>, BridgeError> {
        self.store
            .list(before, limit, include_deleted, status)
            .await
    }

    /// Requests durable cancellation and signals active provider work.
    pub async fn cancel(&self, id: &str) -> Result<ImageJob, BridgeError> {
        let job = self.store.request_cancel(id, unix_timestamp()).await?;
        if let Some(cancellation) = self.active.lock().await.get(id).cloned() {
            cancellation.cancel();
        }
        Ok(job)
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
                result = &mut execution => return result,
                event = receiver.recv() => match event {
                    Some(ProviderEvent::Started) => {
                        self.progress(id, "provider", partial_images).await;
                    }
                    Some(ProviderEvent::Progress { .. }) => {
                        self.progress(id, "provider_progress", partial_images).await;
                    }
                    Some(ProviderEvent::PartialImage { .. }) => {
                        partial_images = partial_images.saturating_add(1);
                        self.progress(id, "partial_image", partial_images).await;
                    }
                    Some(ProviderEvent::Completed { .. }) => {
                        self.progress(id, "materializing", partial_images).await;
                    }
                    None => return execution.await,
                }
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
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
