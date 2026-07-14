//! Durable `SQLite` job state with conservative crash recovery.

use std::path::Path;

use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImageJob, ImageJobProgress, ImageJobStatus, ImageJobSummary,
    ImageRequest, ImageResponse,
};
use tokio_rusqlite::{Connection, params, rusqlite::OptionalExtension as _};

const CURRENT_MIGRATION: u32 = 1;

/// Read-only status of the durable job database schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteJobSchemaStatus {
    /// Whether the database and migration table exist.
    pub initialized: bool,
    /// Highest applied migration.
    pub version: Option<u32>,
    /// Migration expected by this build.
    pub current_version: u32,
}

/// Inspects job storage without creating or migrating it.
pub async fn inspect_sqlite_job_schema(path: &Path) -> Result<SqliteJobSchemaStatus, BridgeError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SqliteJobSchemaStatus {
                initialized: false,
                version: None,
                current_version: CURRENT_MIGRATION,
            });
        }
        Err(_) => return Err(job_error("could not inspect job database")),
    };
    if !metadata.file_type().is_file() {
        return Err(job_error("job database must be a regular file"));
    }
    let flags = tokio_rusqlite::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    let connection = Connection::open_with_flags(path, flags)
        .await
        .map_err(|_| job_error("could not open job database read-only"))?;
    let version = connection
        .call(|connection| {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='job_schema_migrations')",
                [],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(None);
            }
            connection.query_row(
                "SELECT MAX(version) FROM job_schema_migrations",
                [],
                |row| row.get(0),
            )
        })
        .await
        .map_err(|_| job_error("could not inspect job database schema"))?;
    connection
        .close()
        .await
        .map_err(|_| job_error("could not close job database"))?;
    Ok(SqliteJobSchemaStatus {
        initialized: version.is_some(),
        version,
        current_version: CURRENT_MIGRATION,
    })
}

/// Durable job state used by the HTTP job manager and history index.
pub struct SqliteImageJobStore {
    connection: Connection,
}

impl std::fmt::Debug for SqliteImageJobStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteImageJobStore")
            .finish_non_exhaustive()
    }
}

impl SqliteImageJobStore {
    /// Opens storage and applies idempotent migrations.
    pub async fn open(path: &Path) -> Result<Self, BridgeError> {
        let connection = Connection::open(path)
            .await
            .map_err(|_| job_error("could not open job database"))?;
        connection
            .call(|connection| {
                connection.execute_batch(
                    "PRAGMA journal_mode=WAL;
                     PRAGMA foreign_keys=ON;
                     CREATE TABLE IF NOT EXISTS job_schema_migrations (
                       version INTEGER PRIMARY KEY,
                       applied_at INTEGER NOT NULL
                     );
                     CREATE TABLE IF NOT EXISTS image_jobs (
                       id TEXT PRIMARY KEY,
                       status TEXT NOT NULL CHECK(status IN ('queued','running','succeeded','failed','cancelled','interrupted')),
                       created_at INTEGER NOT NULL,
                       updated_at INTEGER NOT NULL,
                       started_at INTEGER,
                       completed_at INTEGER,
                       request_json TEXT NOT NULL,
                       progress_stage TEXT,
                       partial_images INTEGER NOT NULL DEFAULT 0,
                       response_json TEXT,
                       error_json TEXT,
                       cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0,1)),
                       favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1)),
                       deleted_at INTEGER
                     );
                     CREATE INDEX IF NOT EXISTS image_jobs_created_idx
                       ON image_jobs(created_at DESC, id DESC);
                     CREATE INDEX IF NOT EXISTS image_jobs_status_idx
                       ON image_jobs(status, created_at);
                     INSERT OR IGNORE INTO job_schema_migrations(version, applied_at)
                       VALUES (1, unixepoch());",
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| job_error("could not migrate job database"))?;
        Ok(Self { connection })
    }

    /// Closes the `SQLite` worker after pending calls finish.
    pub async fn close(self) -> Result<(), BridgeError> {
        self.connection
            .close()
            .await
            .map_err(|_| job_error("could not close job database"))
    }

    /// Inserts a queued request if bounded pending capacity remains.
    pub async fn create(
        &self,
        id: &str,
        request: &ImageRequest,
        now: u64,
        max_pending: usize,
    ) -> Result<ImageJob, BridgeError> {
        let request_json = serde_json::to_string(request)
            .map_err(|_| job_error("could not encode job request"))?;
        let id = id.to_owned();
        let lookup_id = id.clone();
        let now = to_i64(now)?;
        let max_pending = i64::try_from(max_pending).unwrap_or(i64::MAX);
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let pending: i64 = transaction.query_row(
                    "SELECT COUNT(*) FROM image_jobs WHERE status = 'queued'",
                    [],
                    |row| row.get(0),
                )?;
                if pending >= max_pending {
                    return Err(tokio_rusqlite::rusqlite::Error::ExecuteReturnedResults);
                }
                transaction.execute(
                    "INSERT INTO image_jobs(id,status,created_at,updated_at,request_json)
                     VALUES (?1,'queued',?2,?2,?3)",
                    params![id, now, request_json],
                )?;
                transaction.commit()?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| {
                BridgeError::new(ErrorCode::Overloaded, "durable job queue is full").retryable(true)
            })?;
        self.get(lookup_id.as_str()).await
    }

    /// Marks previously running jobs interrupted without retrying paid work.
    pub async fn recover_interrupted(&self, now: u64) -> Result<usize, BridgeError> {
        let now = to_i64(now)?;
        let error = serde_json::to_string(
            &BridgeError::new(
                ErrorCode::Cancelled,
                "bridge stopped while provider completion was uncertain",
            )
            .with_detail("recovery", "inspect_history_before_retrying"),
        )
        .map_err(|_| job_error("could not encode interruption error"))?;
        self.connection
            .call(move |connection| {
                connection.execute(
                    "UPDATE image_jobs
                     SET status='interrupted', updated_at=?1, completed_at=?1, error_json=?2
                     WHERE status='running'",
                    params![now, error],
                )
            })
            .await
            .map_err(|_| job_error("could not recover interrupted jobs"))
    }

    /// Atomically claims one queued job for execution.
    pub async fn claim(&self, id: &str, now: u64) -> Result<bool, BridgeError> {
        let id = id.to_owned();
        let now = to_i64(now)?;
        self.connection
            .call(move |connection| {
                connection.execute(
                    "UPDATE image_jobs
                     SET status='running', started_at=?2, updated_at=?2, progress_stage='starting'
                     WHERE id=?1 AND status='queued' AND cancel_requested=0",
                    params![id, now],
                )
            })
            .await
            .map(|changed| changed == 1)
            .map_err(|_| job_error("could not claim durable job"))
    }

    /// Stores only the latest safe progress label and partial count.
    pub async fn progress(
        &self,
        id: &str,
        stage: &str,
        partial_images: u32,
        now: u64,
    ) -> Result<(), BridgeError> {
        if stage.is_empty()
            || stage.len() > 64
            || !stage
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(job_error("job progress stage is invalid"));
        }
        let id = id.to_owned();
        let stage = stage.to_owned();
        let partial_images = i64::from(partial_images);
        let now = to_i64(now)?;
        self.connection
            .call(move |connection| {
                connection.execute(
                    "UPDATE image_jobs
                     SET progress_stage=?2, partial_images=?3, updated_at=?4
                     WHERE id=?1 AND status='running'",
                    params![id, stage, partial_images, now],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| job_error("could not update durable job progress"))
    }

    /// Stores a verified terminal result.
    pub async fn succeed(
        &self,
        id: &str,
        response: &ImageResponse,
        now: u64,
    ) -> Result<(), BridgeError> {
        let encoded = serde_json::to_string(response)
            .map_err(|_| job_error("could not encode job result"))?;
        self.finish(id, ImageJobStatus::Succeeded, Some(encoded), None, now)
            .await
    }

    /// Stores a structured terminal error or cancellation.
    pub async fn fail(
        &self,
        id: &str,
        status: ImageJobStatus,
        error: &BridgeError,
        now: u64,
    ) -> Result<(), BridgeError> {
        if !matches!(
            status,
            ImageJobStatus::Failed | ImageJobStatus::Cancelled | ImageJobStatus::Interrupted
        ) {
            return Err(job_error("invalid terminal job failure status"));
        }
        let encoded =
            serde_json::to_string(error).map_err(|_| job_error("could not encode job error"))?;
        self.finish(id, status, None, Some(encoded), now).await
    }

    async fn finish(
        &self,
        id: &str,
        status: ImageJobStatus,
        response: Option<String>,
        error: Option<String>,
        now: u64,
    ) -> Result<(), BridgeError> {
        let id = id.to_owned();
        let status = status_name(status).to_owned();
        let now = to_i64(now)?;
        self.connection
            .call(move |connection| {
                let changed = connection.execute(
                    "UPDATE image_jobs
                     SET status=?2, updated_at=?3, completed_at=?3, progress_stage='completed',
                         response_json=?4, error_json=?5
                     WHERE id=?1 AND status='running'",
                    params![id, status, now, response, error],
                )?;
                if changed != 1 {
                    return Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows);
                }
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| job_error("could not finish durable job"))
    }

    /// Durably requests cancellation and immediately cancels queued work.
    pub async fn request_cancel(&self, id: &str, now: u64) -> Result<ImageJob, BridgeError> {
        let id = id.to_owned();
        let lookup_id = id.clone();
        let now = to_i64(now)?;
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let status: Option<String> = transaction
                    .query_row("SELECT status FROM image_jobs WHERE id=?1", [&id], |row| {
                        row.get(0)
                    })
                    .optional()?;
                match status.as_deref() {
                    Some("queued") => {
                        transaction.execute(
                            "UPDATE image_jobs SET status='cancelled', cancel_requested=1,
                             updated_at=?2, completed_at=?2, progress_stage='completed'
                             WHERE id=?1",
                            params![id, now],
                        )?;
                    }
                    Some("running") => {
                        transaction.execute(
                            "UPDATE image_jobs SET cancel_requested=1, updated_at=?2 WHERE id=?1",
                            params![id, now],
                        )?;
                    }
                    Some(_) => {}
                    None => return Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows),
                }
                transaction.commit()?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| not_found())?;
        self.get(lookup_id.as_str()).await
    }

    /// Returns one complete job record.
    pub async fn get(&self, id: &str) -> Result<ImageJob, BridgeError> {
        let id = id.to_owned();
        let row = self
            .connection
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT id,status,created_at,updated_at,started_at,completed_at,
                                request_json,progress_stage,partial_images,response_json,error_json,
                                cancel_requested,favorite,deleted_at
                         FROM image_jobs WHERE id=?1",
                        [id],
                        read_job_row,
                    )
                    .optional()
            })
            .await
            .map_err(|_| job_error("could not read durable job"))?
            .ok_or_else(not_found)?;
        decode_job(row)
    }

    /// Lists newest-first summaries using a stable `(created,id)` cursor.
    pub async fn list(
        &self,
        before: Option<(u64, String)>,
        limit: usize,
        include_deleted: bool,
        status: Option<ImageJobStatus>,
    ) -> Result<Vec<ImageJobSummary>, BridgeError> {
        let before_created = before
            .as_ref()
            .map(|(created, _)| to_i64(*created))
            .transpose()?;
        let before_id = before.map(|(_, id)| id);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let status = status.map(status_name).map(str::to_owned);
        let rows = self
            .connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT id,status,created_at,updated_at,started_at,completed_at,
                            progress_stage,partial_images,favorite,deleted_at
                     FROM image_jobs
                     WHERE (?1 OR deleted_at IS NULL)
                       AND (?2 IS NULL OR status=?2)
                       AND (?3 IS NULL OR created_at < ?3 OR (created_at=?3 AND id < ?4))
                     ORDER BY created_at DESC, id DESC LIMIT ?5",
                )?;
                statement
                    .query_map(
                        params![include_deleted, status, before_created, before_id, limit],
                        read_summary,
                    )?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(|_| job_error("could not list durable jobs"))?;
        rows.into_iter().map(decode_summary).collect()
    }

    /// Removes terminal records outside time/count retention bounds.
    pub async fn prune(
        &self,
        now: u64,
        retention_secs: u64,
        max_retained: usize,
    ) -> Result<usize, BridgeError> {
        let cutoff = to_i64(now.saturating_sub(retention_secs))?;
        let maximum = i64::try_from(max_retained).unwrap_or(i64::MAX);
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let expired = transaction.execute(
                    "DELETE FROM image_jobs
                     WHERE status IN ('succeeded','failed','cancelled','interrupted')
                       AND completed_at <= ?1",
                    [cutoff],
                )?;
                let excess = transaction.execute(
                    "DELETE FROM image_jobs WHERE id IN (
                       SELECT id FROM image_jobs
                       WHERE status IN ('succeeded','failed','cancelled','interrupted')
                       ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?1
                     )",
                    [maximum],
                )?;
                transaction.commit()?;
                Ok::<usize, tokio_rusqlite::rusqlite::Error>(expired + excess)
            })
            .await
            .map_err(|_| job_error("could not prune durable jobs"))
    }
}

struct JobRow {
    id: String,
    status: String,
    created: i64,
    updated: i64,
    started: Option<i64>,
    completed: Option<i64>,
    request: String,
    progress_stage: Option<String>,
    partial_images: i64,
    response: Option<String>,
    error: Option<String>,
    cancel_requested: bool,
    favorite: bool,
    deleted: Option<i64>,
}

struct SummaryRow {
    id: String,
    status: String,
    created: i64,
    updated: i64,
    started: Option<i64>,
    completed: Option<i64>,
    progress_stage: Option<String>,
    partial_images: i64,
    favorite: bool,
    deleted: Option<i64>,
}

fn read_job_row(
    row: &tokio_rusqlite::rusqlite::Row<'_>,
) -> tokio_rusqlite::rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: row.get(0)?,
        status: row.get(1)?,
        created: row.get(2)?,
        updated: row.get(3)?,
        started: row.get(4)?,
        completed: row.get(5)?,
        request: row.get(6)?,
        progress_stage: row.get(7)?,
        partial_images: row.get(8)?,
        response: row.get(9)?,
        error: row.get(10)?,
        cancel_requested: row.get(11)?,
        favorite: row.get(12)?,
        deleted: row.get(13)?,
    })
}

fn read_summary(
    row: &tokio_rusqlite::rusqlite::Row<'_>,
) -> tokio_rusqlite::rusqlite::Result<SummaryRow> {
    Ok(SummaryRow {
        id: row.get(0)?,
        status: row.get(1)?,
        created: row.get(2)?,
        updated: row.get(3)?,
        started: row.get(4)?,
        completed: row.get(5)?,
        progress_stage: row.get(6)?,
        partial_images: row.get(7)?,
        favorite: row.get(8)?,
        deleted: row.get(9)?,
    })
}

fn decode_job(row: JobRow) -> Result<ImageJob, BridgeError> {
    let summary = decode_summary(SummaryRow {
        id: row.id,
        status: row.status,
        created: row.created,
        updated: row.updated,
        started: row.started,
        completed: row.completed,
        progress_stage: row.progress_stage,
        partial_images: row.partial_images,
        favorite: row.favorite,
        deleted: row.deleted,
    })?;
    Ok(ImageJob {
        summary,
        request: serde_json::from_str(&row.request)
            .map_err(|_| job_error("stored job request is invalid"))?,
        result: row
            .response
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|_| job_error("stored job result is invalid"))?,
        error: row
            .error
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|_| job_error("stored job error is invalid"))?,
        cancel_requested: row.cancel_requested,
    })
}

fn decode_summary(row: SummaryRow) -> Result<ImageJobSummary, BridgeError> {
    let partial_images = u32::try_from(row.partial_images)
        .map_err(|_| job_error("stored partial image count is invalid"))?;
    Ok(ImageJobSummary {
        id: row.id,
        status: parse_status(&row.status)?,
        created: from_i64(row.created)?,
        updated: from_i64(row.updated)?,
        started: row.started.map(from_i64).transpose()?,
        completed: row.completed.map(from_i64).transpose()?,
        progress: row.progress_stage.map(|stage| ImageJobProgress {
            stage,
            partial_images,
        }),
        favorite: row.favorite,
        deleted: row.deleted.map(from_i64).transpose()?,
    })
}

const fn status_name(status: ImageJobStatus) -> &'static str {
    match status {
        ImageJobStatus::Queued => "queued",
        ImageJobStatus::Running => "running",
        ImageJobStatus::Succeeded => "succeeded",
        ImageJobStatus::Failed => "failed",
        ImageJobStatus::Cancelled => "cancelled",
        ImageJobStatus::Interrupted => "interrupted",
    }
}

fn parse_status(value: &str) -> Result<ImageJobStatus, BridgeError> {
    match value {
        "queued" => Ok(ImageJobStatus::Queued),
        "running" => Ok(ImageJobStatus::Running),
        "succeeded" => Ok(ImageJobStatus::Succeeded),
        "failed" => Ok(ImageJobStatus::Failed),
        "cancelled" => Ok(ImageJobStatus::Cancelled),
        "interrupted" => Ok(ImageJobStatus::Interrupted),
        _ => Err(job_error("stored job status is invalid")),
    }
}

fn to_i64(value: u64) -> Result<i64, BridgeError> {
    i64::try_from(value).map_err(|_| job_error("job timestamp is out of range"))
}

fn from_i64(value: i64) -> Result<u64, BridgeError> {
    u64::try_from(value).map_err(|_| job_error("stored job timestamp is invalid"))
}

fn not_found() -> BridgeError {
    BridgeError::new(ErrorCode::InvalidRequest, "durable job was not found")
        .with_detail("resource", "job")
}

fn job_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn fixture_response(id: &str) -> ImageResponse {
        ImageResponse {
            id: id.to_owned(),
            created: 14,
            provider: "test".to_owned(),
            model: "test".to_owned(),
            requested: imagegen_bridge_core::GenerationParameters::default(),
            effective: imagegen_bridge_core::GenerationParameters::default(),
            normalizations: Vec::new(),
            data: Vec::new(),
            failures: Vec::new(),
            revised_prompt: None,
            usage: None,
            session: None,
            timings: imagegen_bridge_core::Timings::default(),
            warnings: Vec::new(),
        }
    }

    #[tokio::test]
    async fn lifecycle_survives_reopen_and_is_cursor_stable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("jobs.sqlite3");
        let store = SqliteImageJobStore::open(&path).await.unwrap();
        store
            .create("019f-job-a", &ImageRequest::generate("first"), 10, 10)
            .await
            .unwrap();
        store
            .create("019f-job-b", &ImageRequest::generate("second"), 11, 10)
            .await
            .unwrap();
        assert!(store.claim("019f-job-a", 12).await.unwrap());
        store
            .progress("019f-job-a", "provider", 2, 13)
            .await
            .unwrap();
        let response = fixture_response("019f-job-a");
        store.succeed("019f-job-a", &response, 14).await.unwrap();
        let first = store.list(None, 1, false, None).await.unwrap();
        assert_eq!(first[0].id, "019f-job-b");
        let second = store
            .list(
                Some((first[0].created, first[0].id.clone())),
                2,
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(second[0].id, "019f-job-a");
        store.close().await.unwrap();

        let reopened = SqliteImageJobStore::open(&path).await.unwrap();
        let job = reopened.get("019f-job-a").await.unwrap();
        assert_eq!(job.summary.status, ImageJobStatus::Succeeded);
        assert_eq!(job.summary.progress.unwrap().partial_images, 2);
        assert_eq!(job.result.unwrap().id, "019f-job-a");
    }

    #[tokio::test]
    async fn recovery_never_retries_ambiguous_running_work() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteImageJobStore::open(&directory.path().join("jobs.sqlite3"))
            .await
            .unwrap();
        store
            .create("019f-job", &ImageRequest::generate("test"), 10, 10)
            .await
            .unwrap();
        store.claim("019f-job", 11).await.unwrap();
        assert_eq!(store.recover_interrupted(12).await.unwrap(), 1);
        let job = store.get("019f-job").await.unwrap();
        assert_eq!(job.summary.status, ImageJobStatus::Interrupted);
        assert!(!job.error.unwrap().retryable);
    }

    #[tokio::test]
    async fn queued_cancellation_is_immediate_and_durable() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteImageJobStore::open(&directory.path().join("jobs.sqlite3"))
            .await
            .unwrap();
        store
            .create("019f-job", &ImageRequest::generate("test"), 10, 10)
            .await
            .unwrap();
        let job = store.request_cancel("019f-job", 11).await.unwrap();
        assert_eq!(job.summary.status, ImageJobStatus::Cancelled);
        assert!(job.cancel_requested);
        assert!(!store.claim("019f-job", 12).await.unwrap());
    }
}
