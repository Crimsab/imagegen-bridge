//! Durable `SQLite` job state with conservative crash recovery.

use std::path::Path;

use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImageJob, ImageJobProgress, ImageJobStatus, ImageJobSummary,
    ImageRequest, ImageResponse,
};
use sha2::{Digest as _, Sha256};
use tokio_rusqlite::{
    Connection, params,
    rusqlite::{
        OptionalExtension as _, TransactionBehavior, params_from_iter, types::Value as SqlValue,
    },
};

const CURRENT_MIGRATION: u32 = 3;
const ACTIVE_RESULT_RESERVE_BYTES: u64 = 256 * 1024;

/// Result of atomically submitting a durable job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteJobSubmission {
    /// Canonical durable job selected by the submission identity.
    pub job: ImageJob,
    /// Whether this call inserted and must schedule the job.
    pub created: bool,
}

/// Visibility constraint applied before durable history pagination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImageJobVisibility {
    /// Ordinary non-hidden history only.
    #[default]
    Active,
    /// Soft-deleted history only.
    Hidden,
    /// Both active and hidden history.
    All,
}

/// Server-side durable history filters applied before cursor pagination.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageJobListFilter {
    /// Exclusive newest-first `(created,id)` cursor.
    pub before: Option<(u64, String)>,
    /// Maximum rows returned.
    pub limit: usize,
    /// Visibility selection.
    pub visibility: ImageJobVisibility,
    /// Optional lifecycle status.
    pub status: Option<ImageJobStatus>,
    /// Optional favorite state.
    pub favorite: Option<bool>,
    /// Optional case-insensitive literal prompt substring.
    pub search: Option<String>,
}

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

/// Redaction-safe aggregate state for the durable job database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SqliteJobStatistics {
    /// Total retained job rows, including soft-deleted history.
    pub total: u64,
    /// Jobs accepted but not yet claimed.
    pub queued: u64,
    /// Jobs currently recorded as running.
    pub running: u64,
    /// Successfully completed jobs.
    pub succeeded: u64,
    /// Jobs completed with a structured failure.
    pub failed: u64,
    /// Jobs cancelled before or during execution.
    pub cancelled: u64,
    /// Jobs conservatively marked after uncertain shutdown completion.
    pub interrupted: u64,
    /// Soft-deleted history rows.
    pub hidden: u64,
    /// Main `SQLite` database pages multiplied by page size, excluding WAL files.
    pub database_bytes: u64,
    /// Logical job-row bytes used for admission and retention accounting.
    pub logical_bytes: u64,
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

enum CreateOutcome {
    Created(String),
    Replay(String),
    Conflict,
    Full,
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
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => return Err(job_error("job database must be a regular file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(job_error("could not inspect job database")),
        }
        let connection = Connection::open(path)
            .await
            .map_err(|_| job_error("could not open job database"))?;
        connection
            .call(|connection| {
                connection.execute_batch(
                    "PRAGMA journal_mode=WAL;
                     PRAGMA synchronous=FULL;
                     PRAGMA busy_timeout=5000;
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
                       prompt_search TEXT NOT NULL DEFAULT '',
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
                let transaction = connection.transaction()?;
                let has_prompt_search = transaction
                    .prepare("PRAGMA table_info(image_jobs)")?
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()?
                    .iter()
                    .any(|name| name == "prompt_search");
                if !has_prompt_search {
                    transaction.execute_batch(
                        "ALTER TABLE image_jobs ADD COLUMN prompt_search TEXT NOT NULL DEFAULT '';",
                    )?;
                }
                let prompts = {
                    let mut statement = transaction.prepare(
                        "SELECT id, COALESCE(json_extract(request_json, '$.prompt'), '')
                         FROM image_jobs WHERE prompt_search='' AND json_valid(request_json)",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                };
                for (id, prompt) in prompts {
                    transaction.execute(
                        "UPDATE image_jobs SET prompt_search=?2 WHERE id=?1",
                        params![id, prompt.to_lowercase()],
                    )?;
                }
                transaction.execute_batch(
                    "CREATE INDEX IF NOT EXISTS image_jobs_history_idx
                       ON image_jobs(deleted_at, favorite, created_at DESC, id DESC);
                     INSERT OR IGNORE INTO job_schema_migrations(version, applied_at)
                       VALUES (2, unixepoch());",
                )?;
                let columns = transaction
                    .prepare("PRAGMA table_info(image_jobs)")?
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()?;
                if !columns.iter().any(|name| name == "auth_scope") {
                    transaction.execute_batch(
                        "ALTER TABLE image_jobs ADD COLUMN auth_scope TEXT NOT NULL
                           DEFAULT 'legacy-unowned';",
                    )?;
                }
                if !columns.iter().any(|name| name == "submission_key_hash") {
                    transaction.execute_batch(
                        "ALTER TABLE image_jobs ADD COLUMN submission_key_hash TEXT;",
                    )?;
                }
                if !columns.iter().any(|name| name == "request_fingerprint") {
                    transaction.execute_batch(
                        "ALTER TABLE image_jobs ADD COLUMN request_fingerprint TEXT;",
                    )?;
                }
                transaction.execute_batch(
                    "CREATE INDEX IF NOT EXISTS image_jobs_scope_history_idx
                       ON image_jobs(auth_scope, deleted_at, favorite, created_at DESC, id DESC);
                     CREATE UNIQUE INDEX IF NOT EXISTS image_jobs_submission_idx
                       ON image_jobs(auth_scope, submission_key_hash)
                       WHERE submission_key_hash IS NOT NULL;
                     INSERT OR IGNORE INTO job_schema_migrations(version, applied_at)
                       VALUES (3, unixepoch());",
                )?;
                transaction.commit()?;
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
        auth_scope: &str,
        id: &str,
        request: &ImageRequest,
        now: u64,
        max_pending: usize,
        max_database_bytes: u64,
    ) -> Result<SqliteJobSubmission, BridgeError> {
        validate_auth_scope(auth_scope)?;
        let mut persisted_request = request.clone();
        persisted_request.idempotency_key = None;
        let request_json = serde_json::to_string(&persisted_request)
            .map_err(|_| job_error("could not encode job request"))?;
        let prompt_search = request.prompt.to_lowercase();
        if let Some(key) = request.idempotency_key.as_deref() {
            validate_submission_key(key)?;
        }
        let submission_key_hash = request
            .idempotency_key
            .as_deref()
            .map(|key| base16ct::lower::encode_string(&Sha256::digest(key.as_bytes())));
        let request_fingerprint = submission_key_hash
            .as_ref()
            .map(|_| durable_request_fingerprint(request))
            .transpose()?;
        let auth_scope = auth_scope.to_owned();
        let lookup_scope = auth_scope.clone();
        let id = id.to_owned();
        let now = to_i64(now)?;
        let max_pending = i64::try_from(max_pending).unwrap_or(i64::MAX);
        let max_database_bytes = i64::try_from(max_database_bytes).unwrap_or(i64::MAX);
        let new_job_bytes = logical_new_job_bytes(
            &id,
            &auth_scope,
            &request_json,
            &prompt_search,
            submission_key_hash.as_deref(),
            request_fingerprint.as_deref(),
        )?;
        let outcome = self
            .connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(key_hash) = submission_key_hash.as_deref() {
                    let existing: Option<(String, String)> = transaction
                        .query_row(
                            "SELECT id,request_fingerprint FROM image_jobs
                             WHERE auth_scope=?1 AND submission_key_hash=?2",
                            params![auth_scope, key_hash],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    if let Some((existing_id, existing_fingerprint)) = existing {
                        return Ok(
                            if request_fingerprint.as_deref() == Some(existing_fingerprint.as_str())
                            {
                                CreateOutcome::Replay(existing_id)
                            } else {
                                CreateOutcome::Conflict
                            },
                        );
                    }
                }
                let pending: i64 = transaction.query_row(
                    "SELECT COUNT(*) FROM image_jobs WHERE status = 'queued'",
                    [],
                    |row| row.get(0),
                )?;
                if pending >= max_pending {
                    return Ok(CreateOutcome::Full);
                }
                let logical_bytes: i64 =
                    transaction.query_row(logical_bytes_query(), [], |row| row.get(0))?;
                if logical_bytes.saturating_add(new_job_bytes) > max_database_bytes {
                    return Ok(CreateOutcome::Full);
                }
                transaction.execute(
                    "INSERT INTO image_jobs(
                       id,status,created_at,updated_at,request_json,prompt_search,auth_scope,
                       submission_key_hash,request_fingerprint
                     ) VALUES (?1,'queued',?2,?2,?3,?4,?5,?6,?7)",
                    params![
                        id,
                        now,
                        request_json,
                        prompt_search,
                        auth_scope,
                        submission_key_hash,
                        request_fingerprint
                    ],
                )?;
                transaction.commit()?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(CreateOutcome::Created(id))
            })
            .await
            .map_err(|_| job_error("could not submit durable job"))?;
        let (lookup_id, created) = match outcome {
            CreateOutcome::Created(id) => (id, true),
            CreateOutcome::Replay(id) => (id, false),
            CreateOutcome::Conflict => {
                return Err(BridgeError::new(
                    ErrorCode::IdempotencyConflict,
                    "idempotency key was already used for a different request",
                ));
            }
            CreateOutcome::Full => {
                return Err(
                    BridgeError::new(ErrorCode::Overloaded, "durable job queue is full")
                        .retryable(true),
                );
            }
        };
        Ok(SqliteJobSubmission {
            job: self.get(lookup_scope.as_str(), lookup_id.as_str()).await?,
            created,
        })
    }

    /// Returns bounded aggregate counters without request, prompt, or path data.
    pub async fn statistics(&self) -> Result<SqliteJobStatistics, BridgeError> {
        self.connection
            .call(|connection| {
                let counts: (i64, i64, i64, i64, i64, i64, i64, i64) = connection.query_row(
                    "SELECT COUNT(*),
                            COALESCE(SUM(status='queued'),0),
                            COALESCE(SUM(status='running'),0),
                            COALESCE(SUM(status='succeeded'),0),
                            COALESCE(SUM(status='failed'),0),
                            COALESCE(SUM(status='cancelled'),0),
                            COALESCE(SUM(status='interrupted'),0),
                            COALESCE(SUM(deleted_at IS NOT NULL),0)
                     FROM image_jobs",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )?;
                let logical_bytes: i64 =
                    connection.query_row(logical_bytes_query(), [], |row| row.get(0))?;
                let page_count: i64 =
                    connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
                let page_size: i64 =
                    connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>((
                    counts,
                    logical_bytes,
                    page_count,
                    page_size,
                ))
            })
            .await
            .map_err(|_| job_error("could not inspect job database statistics"))
            .and_then(|(counts, logical_bytes, page_count, page_size)| {
                let convert = |value: i64| {
                    u64::try_from(value)
                        .map_err(|_| job_error("job database returned invalid statistics"))
                };
                Ok(SqliteJobStatistics {
                    total: convert(counts.0)?,
                    queued: convert(counts.1)?,
                    running: convert(counts.2)?,
                    succeeded: convert(counts.3)?,
                    failed: convert(counts.4)?,
                    cancelled: convert(counts.5)?,
                    interrupted: convert(counts.6)?,
                    hidden: convert(counts.7)?,
                    database_bytes: convert(page_count)?.saturating_mul(convert(page_size)?),
                    logical_bytes: convert(logical_bytes)?,
                })
            })
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

    /// Returns oldest-first queued identities across ownership scopes for crash recovery.
    pub async fn queued_identities(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String)>, BridgeError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT auth_scope,id FROM image_jobs
                     WHERE status='queued'
                     ORDER BY created_at ASC,id ASC LIMIT ?1",
                )?;
                statement
                    .query_map([limit], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(|_| job_error("could not list queued durable jobs"))
    }

    /// Atomically claims one queued job for execution.
    pub async fn claim(&self, auth_scope: &str, id: &str, now: u64) -> Result<bool, BridgeError> {
        validate_auth_scope(auth_scope)?;
        let auth_scope = auth_scope.to_owned();
        let id = id.to_owned();
        let now = to_i64(now)?;
        self.connection
            .call(move |connection| {
                connection.execute(
                    "UPDATE image_jobs
                     SET status='running', started_at=?2, updated_at=?2, progress_stage='starting'
                     WHERE id=?1 AND auth_scope=?3 AND status='queued' AND cancel_requested=0",
                    params![id, now, auth_scope],
                )
            })
            .await
            .map(|changed| changed == 1)
            .map_err(|_| job_error("could not claim durable job"))
    }

    /// Stores only the latest safe progress label and partial count.
    pub async fn progress(
        &self,
        auth_scope: &str,
        id: &str,
        stage: &str,
        partial_images: u32,
        now: u64,
    ) -> Result<(), BridgeError> {
        validate_auth_scope(auth_scope)?;
        if stage.is_empty()
            || stage.len() > 64
            || !stage
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(job_error("job progress stage is invalid"));
        }
        let id = id.to_owned();
        let auth_scope = auth_scope.to_owned();
        let stage = stage.to_owned();
        let partial_images = i64::from(partial_images);
        let now = to_i64(now)?;
        self.connection
            .call(move |connection| {
                connection.execute(
                    "UPDATE image_jobs
                     SET progress_stage=?2, partial_images=?3, updated_at=?4
                     WHERE id=?1 AND auth_scope=?5 AND status='running'",
                    params![id, stage, partial_images, now, auth_scope],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| job_error("could not update durable job progress"))
    }

    /// Stores a verified terminal result.
    pub async fn succeed(
        &self,
        auth_scope: &str,
        id: &str,
        response: &ImageResponse,
        now: u64,
    ) -> Result<(), BridgeError> {
        let encoded = serde_json::to_string(response)
            .map_err(|_| job_error("could not encode job result"))?;
        if encoded.len() > usize::try_from(ACTIVE_RESULT_RESERVE_BYTES).unwrap_or(usize::MAX) {
            return Err(BridgeError::new(
                ErrorCode::Artifact,
                "durable job result metadata exceeds the storage reserve",
            ));
        }
        self.finish(
            auth_scope,
            id,
            ImageJobStatus::Succeeded,
            Some(encoded),
            None,
            now,
        )
        .await
    }

    /// Stores a structured terminal error or cancellation.
    pub async fn fail(
        &self,
        auth_scope: &str,
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
        let mut encoded =
            serde_json::to_string(error).map_err(|_| job_error("could not encode job error"))?;
        if encoded.len() > usize::try_from(ACTIVE_RESULT_RESERVE_BYTES).unwrap_or(usize::MAX) {
            encoded = serde_json::to_string(&BridgeError::new(
                ErrorCode::Internal,
                "durable job failed with oversized error metadata",
            ))
            .map_err(|_| job_error("could not encode bounded job error"))?;
        }
        self.finish(auth_scope, id, status, None, Some(encoded), now)
            .await
    }

    async fn finish(
        &self,
        auth_scope: &str,
        id: &str,
        status: ImageJobStatus,
        response: Option<String>,
        error: Option<String>,
        now: u64,
    ) -> Result<(), BridgeError> {
        validate_auth_scope(auth_scope)?;
        let auth_scope = auth_scope.to_owned();
        let id = id.to_owned();
        let status = status_name(status).to_owned();
        let now = to_i64(now)?;
        self.connection
            .call(move |connection| {
                let changed = connection.execute(
                    "UPDATE image_jobs
                     SET status=?2, updated_at=?3, completed_at=?3, progress_stage='completed',
                         response_json=?4, error_json=?5
                     WHERE id=?1 AND auth_scope=?6 AND status='running'",
                    params![id, status, now, response, error, auth_scope],
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
    pub async fn request_cancel(
        &self,
        auth_scope: &str,
        id: &str,
        now: u64,
    ) -> Result<ImageJob, BridgeError> {
        validate_auth_scope(auth_scope)?;
        let auth_scope = auth_scope.to_owned();
        let lookup_scope = auth_scope.clone();
        let id = id.to_owned();
        let lookup_id = id.clone();
        let now = to_i64(now)?;
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let status: Option<String> = transaction
                    .query_row(
                        "SELECT status FROM image_jobs WHERE id=?1 AND auth_scope=?2",
                        params![id, auth_scope],
                        |row| row.get(0),
                    )
                    .optional()?;
                match status.as_deref() {
                    Some("queued") => {
                        transaction.execute(
                            "UPDATE image_jobs SET status='cancelled', cancel_requested=1,
                             updated_at=?2, completed_at=?2, progress_stage='completed'
                             WHERE id=?1 AND auth_scope=?3",
                            params![id, now, auth_scope],
                        )?;
                    }
                    Some("running") => {
                        transaction.execute(
                            "UPDATE image_jobs SET cancel_requested=1, updated_at=?2
                             WHERE id=?1 AND auth_scope=?3",
                            params![id, now, auth_scope],
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
        self.get(lookup_scope.as_str(), lookup_id.as_str()).await
    }

    /// Returns one complete job record.
    pub async fn get(&self, auth_scope: &str, id: &str) -> Result<ImageJob, BridgeError> {
        validate_auth_scope(auth_scope)?;
        let auth_scope = auth_scope.to_owned();
        let id = id.to_owned();
        let row = self
            .connection
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT id,status,created_at,updated_at,started_at,completed_at,
                                request_json,progress_stage,partial_images,response_json,error_json,
                                cancel_requested,favorite,deleted_at
                         FROM image_jobs WHERE id=?1 AND auth_scope=?2",
                        params![id, auth_scope],
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
        auth_scope: &str,
        filter: ImageJobListFilter,
    ) -> Result<Vec<ImageJobSummary>, BridgeError> {
        validate_auth_scope(auth_scope)?;
        let auth_scope = auth_scope.to_owned();
        let before_created = filter
            .before
            .as_ref()
            .map(|(created, _)| to_i64(*created))
            .transpose()?;
        let before_id = filter.before.map(|(_, id)| id);
        let limit = i64::try_from(filter.limit).unwrap_or(i64::MAX);
        let status = filter.status.map(status_name).map(str::to_owned);
        let search = filter
            .search
            .map(|value| literal_like_pattern(&value.to_lowercase()));
        let visibility = filter.visibility;
        let favorite = filter.favorite;
        let rows = self
            .connection
            .call(move |connection| {
                let mut predicates = vec!["auth_scope=?"];
                let mut parameters = vec![SqlValue::Text(auth_scope)];
                match visibility {
                    ImageJobVisibility::Active => predicates.push("deleted_at IS NULL"),
                    ImageJobVisibility::Hidden => predicates.push("deleted_at IS NOT NULL"),
                    ImageJobVisibility::All => {}
                }
                if let Some(status) = status {
                    predicates.push("status=?");
                    parameters.push(SqlValue::Text(status));
                }
                if let Some(favorite) = favorite {
                    predicates.push("favorite=?");
                    parameters.push(SqlValue::Integer(i64::from(favorite)));
                }
                if let Some(search) = search {
                    predicates.push("prompt_search LIKE ? ESCAPE '\\'");
                    parameters.push(SqlValue::Text(search));
                }
                if let (Some(created), Some(id)) = (before_created, before_id) {
                    predicates.push("(created_at < ? OR (created_at=? AND id < ?))");
                    parameters.push(SqlValue::Integer(created));
                    parameters.push(SqlValue::Integer(created));
                    parameters.push(SqlValue::Text(id));
                }
                let condition = if predicates.is_empty() {
                    "1".to_owned()
                } else {
                    predicates.join(" AND ")
                };
                let query = format!(
                    "SELECT id,status,created_at,updated_at,started_at,completed_at,
                            progress_stage,partial_images,favorite,deleted_at
                     FROM image_jobs WHERE {condition}
                     ORDER BY created_at DESC, id DESC LIMIT ?"
                );
                parameters.push(SqlValue::Integer(limit));
                let mut statement = connection.prepare(&query)?;
                statement
                    .query_map(params_from_iter(parameters), read_summary)?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(|_| job_error("could not list durable jobs"))?;
        rows.into_iter().map(decode_summary).collect()
    }

    /// Updates favorite and soft-delete gallery state without removing job evidence.
    pub async fn update_history(
        &self,
        auth_scope: &str,
        id: &str,
        favorite: Option<bool>,
        deleted: Option<bool>,
        now: u64,
    ) -> Result<ImageJob, BridgeError> {
        validate_auth_scope(auth_scope)?;
        if favorite.is_none() && deleted.is_none() {
            return Err(BridgeError::new(
                ErrorCode::InvalidRequest,
                "history update must change favorite or deleted state",
            ));
        }
        let existing = self.get(auth_scope, id).await?;
        if deleted.is_some() && !existing.summary.status.terminal() {
            return Err(BridgeError::new(
                ErrorCode::InvalidRequest,
                "only terminal jobs can be deleted from history",
            )
            .with_detail("field", "deleted"));
        }
        let id = id.to_owned();
        let auth_scope = auth_scope.to_owned();
        let lookup_scope = auth_scope.clone();
        let lookup_id = id.clone();
        let now = to_i64(now)?;
        self.connection
            .call(move |connection| {
                let changed = connection.execute(
                    "UPDATE image_jobs
                     SET favorite=COALESCE(?2,favorite),
                         deleted_at=CASE WHEN ?3 IS NULL THEN deleted_at
                                         WHEN ?3 THEN ?4 ELSE NULL END,
                         updated_at=?4
                     WHERE id=?1 AND auth_scope=?5",
                    params![id, favorite, deleted, now, auth_scope],
                )?;
                if changed != 1 {
                    return Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows);
                }
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| job_error("could not update durable job history"))?;
        self.get(&lookup_scope, &lookup_id).await
    }

    /// Removes terminal records outside time/count retention bounds.
    pub async fn prune(
        &self,
        now: u64,
        retention_secs: u64,
        max_retained: usize,
        max_retained_bytes: u64,
    ) -> Result<usize, BridgeError> {
        let cutoff = to_i64(now.saturating_sub(retention_secs))?;
        let maximum = i64::try_from(max_retained).unwrap_or(i64::MAX);
        let maximum_bytes = i64::try_from(max_retained_bytes).unwrap_or(i64::MAX);
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let expired = transaction.execute(
                    "DELETE FROM image_jobs
                     WHERE status IN ('succeeded','failed','cancelled','interrupted')
                       AND favorite=0
                       AND completed_at <= ?1",
                    [cutoff],
                )?;
                let excess = transaction.execute(
                    "WITH retained AS (
                       SELECT id,
                         ROW_NUMBER() OVER (ORDER BY created_at DESC,id DESC) AS ordinal,
                         SUM(
                           length(CAST(id AS BLOB)) + length(CAST(auth_scope AS BLOB))
                           + length(CAST(request_json AS BLOB))
                           + length(CAST(prompt_search AS BLOB))
                           + length(CAST(COALESCE(submission_key_hash,'') AS BLOB))
                           + length(CAST(COALESCE(request_fingerprint,'') AS BLOB))
                           + length(CAST(COALESCE(response_json,'') AS BLOB))
                           + length(CAST(COALESCE(error_json,'') AS BLOB)) + 128
                         ) OVER (ORDER BY created_at DESC,id DESC) AS cumulative_bytes
                       FROM image_jobs
                       WHERE status IN ('succeeded','failed','cancelled','interrupted')
                         AND favorite=0
                     )
                     DELETE FROM image_jobs WHERE id IN (
                       SELECT id FROM retained WHERE ordinal > ?1 OR cumulative_bytes > ?2
                     )",
                    params![maximum, maximum_bytes],
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

fn literal_like_pattern(value: &str) -> String {
    let mut pattern = String::with_capacity(value.len().saturating_add(2));
    pattern.push('%');
    for character in value.chars() {
        if matches!(character, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

const fn logical_bytes_query() -> &'static str {
    "SELECT COALESCE(SUM(
       length(CAST(id AS BLOB)) + length(CAST(auth_scope AS BLOB))
       + length(CAST(request_json AS BLOB)) + length(CAST(prompt_search AS BLOB))
       + length(CAST(COALESCE(submission_key_hash,'') AS BLOB))
       + length(CAST(COALESCE(request_fingerprint,'') AS BLOB))
       + CASE WHEN status IN ('queued','running') THEN 262144 ELSE
           length(CAST(COALESCE(response_json,'') AS BLOB))
           + length(CAST(COALESCE(error_json,'') AS BLOB)) END
       + 128
     ),0) FROM image_jobs"
}

fn logical_new_job_bytes(
    id: &str,
    auth_scope: &str,
    request_json: &str,
    prompt_search: &str,
    submission_key_hash: Option<&str>,
    request_fingerprint: Option<&str>,
) -> Result<i64, BridgeError> {
    let bytes = id
        .len()
        .saturating_add(auth_scope.len())
        .saturating_add(request_json.len())
        .saturating_add(prompt_search.len())
        .saturating_add(submission_key_hash.map_or(0, str::len))
        .saturating_add(request_fingerprint.map_or(0, str::len))
        .saturating_add(usize::try_from(ACTIVE_RESULT_RESERVE_BYTES).unwrap_or(usize::MAX))
        .saturating_add(128);
    i64::try_from(bytes).map_err(|_| job_error("durable job is too large to account"))
}

fn durable_request_fingerprint(request: &ImageRequest) -> Result<String, BridgeError> {
    let mut canonical = request.clone();
    canonical.idempotency_key = None;
    canonical.timeout_ms = None;
    let encoded = serde_json::to_vec(&canonical)
        .map_err(|_| job_error("could not fingerprint durable job request"))?;
    Ok(base16ct::lower::encode_string(&Sha256::digest(encoded)))
}

fn validate_auth_scope(scope: &str) -> Result<(), BridgeError> {
    if scope.is_empty() || scope.len() > 256 || scope.chars().any(char::is_control) {
        Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "durable job authorization scope is invalid",
        ))
    } else {
        Ok(())
    }
}

fn validate_submission_key(key: &str) -> Result<(), BridgeError> {
    if key.trim().is_empty() || key.len() > 512 || key.chars().any(char::is_control) {
        Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "durable job idempotency key is invalid",
        ))
    } else {
        Ok(())
    }
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

    use std::sync::Arc;

    use super::*;

    const SCOPE: &str = "test-scope";
    const MAX_BYTES: u64 = 1024 * 1024 * 1024;

    fn fixture_response(id: &str) -> ImageResponse {
        ImageResponse {
            id: id.to_owned(),
            created: 14,
            provider: "test".to_owned(),
            model: "test".to_owned(),
            requested: imagegen_bridge_core::GenerationParameters::default(),
            effective: imagegen_bridge_core::GenerationParameters::default(),
            normalizations: Vec::new(),
            attempts: Vec::new(),
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
            .create(
                SCOPE,
                "019f-job-a",
                &ImageRequest::generate("first"),
                10,
                10,
                MAX_BYTES,
            )
            .await
            .unwrap();
        store
            .create(
                SCOPE,
                "019f-job-b",
                &ImageRequest::generate("second"),
                11,
                10,
                MAX_BYTES,
            )
            .await
            .unwrap();
        assert!(store.claim(SCOPE, "019f-job-a", 12).await.unwrap());
        store
            .progress(SCOPE, "019f-job-a", "provider", 2, 13)
            .await
            .unwrap();
        let response = fixture_response("019f-job-a");
        store
            .succeed(SCOPE, "019f-job-a", &response, 14)
            .await
            .unwrap();
        let first = store
            .list(
                SCOPE,
                ImageJobListFilter {
                    limit: 1,
                    ..ImageJobListFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(first[0].id, "019f-job-b");
        let second = store
            .list(
                SCOPE,
                ImageJobListFilter {
                    before: Some((first[0].created, first[0].id.clone())),
                    limit: 2,
                    ..ImageJobListFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(second[0].id, "019f-job-a");
        store.close().await.unwrap();

        let reopened = SqliteImageJobStore::open(&path).await.unwrap();
        let job = reopened.get(SCOPE, "019f-job-a").await.unwrap();
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
            .create(
                SCOPE,
                "019f-job",
                &ImageRequest::generate("test"),
                10,
                10,
                MAX_BYTES,
            )
            .await
            .unwrap();
        store.claim(SCOPE, "019f-job", 11).await.unwrap();
        assert_eq!(store.recover_interrupted(12).await.unwrap(), 1);
        let job = store.get(SCOPE, "019f-job").await.unwrap();
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
            .create(
                SCOPE,
                "019f-job",
                &ImageRequest::generate("test"),
                10,
                10,
                MAX_BYTES,
            )
            .await
            .unwrap();
        let job = store.request_cancel(SCOPE, "019f-job", 11).await.unwrap();
        assert_eq!(job.summary.status, ImageJobStatus::Cancelled);
        assert!(job.cancel_requested);
        assert!(!store.claim(SCOPE, "019f-job", 12).await.unwrap());

        let statistics = store.statistics().await.unwrap();
        assert_eq!(statistics.total, 1);
        assert_eq!(statistics.cancelled, 1);
        assert_eq!(statistics.queued, 0);
        assert_eq!(statistics.running, 0);
        assert_eq!(statistics.hidden, 0);
        assert!(statistics.database_bytes > 0);
    }

    #[tokio::test]
    async fn pending_capacity_rejects_without_persisting_extra_work() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteImageJobStore::open(&directory.path().join("jobs.sqlite3"))
            .await
            .unwrap();
        store
            .create(
                SCOPE,
                "019f-job-a",
                &ImageRequest::generate("first"),
                10,
                1,
                MAX_BYTES,
            )
            .await
            .unwrap();
        let error = store
            .create(
                SCOPE,
                "019f-job-b",
                &ImageRequest::generate("second"),
                11,
                1,
                MAX_BYTES,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Overloaded);
        assert!(error.retryable);
        assert!(store.get(SCOPE, "019f-job-b").await.is_err());
    }

    #[tokio::test]
    async fn logical_database_budget_rejects_before_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteImageJobStore::open(&directory.path().join("jobs.sqlite3"))
            .await
            .unwrap();
        let error = store
            .create(
                SCOPE,
                "019f-too-large",
                &ImageRequest::generate("bounded"),
                10,
                10,
                ACTIVE_RESULT_RESERVE_BYTES - 1,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Overloaded);
        assert_eq!(store.statistics().await.unwrap().total, 0);
    }

    #[tokio::test]
    async fn pruning_enforces_terminal_logical_byte_budget() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteImageJobStore::open(&directory.path().join("jobs.sqlite3"))
            .await
            .unwrap();
        store
            .create(
                SCOPE,
                "019f-job-a",
                &ImageRequest::generate("first retained request"),
                10,
                10,
                MAX_BYTES,
            )
            .await
            .unwrap();
        assert!(store.claim(SCOPE, "019f-job-a", 11).await.unwrap());
        store
            .succeed(SCOPE, "019f-job-a", &fixture_response("019f-job-a"), 12)
            .await
            .unwrap();
        let one_job_bytes = store.statistics().await.unwrap().logical_bytes;

        store
            .create(
                SCOPE,
                "019f-job-b",
                &ImageRequest::generate("second retained request"),
                13,
                10,
                MAX_BYTES,
            )
            .await
            .unwrap();
        assert!(store.claim(SCOPE, "019f-job-b", 14).await.unwrap());
        store
            .succeed(SCOPE, "019f-job-b", &fixture_response("019f-job-b"), 15)
            .await
            .unwrap();

        let two_job_bytes = store.statistics().await.unwrap().logical_bytes;
        let byte_budget = one_job_bytes.max(two_job_bytes.saturating_sub(one_job_bytes));
        assert_eq!(store.prune(15, 100, 10, byte_budget).await.unwrap(), 1);
        assert!(store.get(SCOPE, "019f-job-a").await.is_err());
        assert!(store.get(SCOPE, "019f-job-b").await.is_ok());
        assert!(store.statistics().await.unwrap().logical_bytes <= byte_budget);
    }

    #[tokio::test]
    async fn submission_idempotency_is_atomic_scoped_and_persistent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("jobs.sqlite3");
        let store = SqliteImageJobStore::open(&path).await.unwrap();
        let mut request = ImageRequest::generate("same request");
        request.idempotency_key = Some("durable-key".to_owned());

        let first = store
            .create("scope-a", "019f-job-a", &request, 10, 1, MAX_BYTES)
            .await
            .unwrap();
        assert!(first.created);
        assert_eq!(first.job.summary.id, "019f-job-a");
        assert_eq!(first.job.request.idempotency_key, None);

        let replay = store
            .create("scope-a", "019f-job-b", &request, 11, 1, MAX_BYTES)
            .await
            .unwrap();
        assert!(!replay.created);
        assert_eq!(replay.job.summary.id, "019f-job-a");

        let other_scope = store
            .create("scope-b", "019f-job-c", &request, 12, 2, MAX_BYTES)
            .await
            .unwrap();
        assert!(other_scope.created);
        assert_eq!(other_scope.job.summary.id, "019f-job-c");

        let mut conflict = request.clone();
        conflict.prompt = "different request".to_owned();
        let error = store
            .create("scope-a", "019f-job-d", &conflict, 13, 2, MAX_BYTES)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::IdempotencyConflict);
        store.close().await.unwrap();

        let reopened = SqliteImageJobStore::open(&path).await.unwrap();
        let replay = reopened
            .create("scope-a", "019f-job-e", &request, 14, 1, MAX_BYTES)
            .await
            .unwrap();
        assert!(!replay.created);
        assert_eq!(replay.job.summary.id, "019f-job-a");
    }

    #[tokio::test]
    async fn caller_operations_never_cross_authorization_scopes() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteImageJobStore::open(&directory.path().join("jobs.sqlite3"))
            .await
            .unwrap();
        store
            .create(
                "scope-a",
                "019f-owned",
                &ImageRequest::generate("private"),
                10,
                10,
                MAX_BYTES,
            )
            .await
            .unwrap();

        assert!(store.get("scope-b", "019f-owned").await.is_err());
        assert!(
            store
                .list(
                    "scope-b",
                    ImageJobListFilter {
                        limit: 10,
                        ..ImageJobListFilter::default()
                    },
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .request_cancel("scope-b", "019f-owned", 11)
                .await
                .is_err()
        );
        assert!(
            store
                .update_history("scope-b", "019f-owned", Some(true), None, 11)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .get("scope-a", "019f-owned")
                .await
                .unwrap()
                .request
                .prompt,
            "private"
        );
    }

    #[tokio::test]
    async fn concurrent_identical_submissions_converge_on_one_job() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteImageJobStore::open(&directory.path().join("jobs.sqlite3"))
                .await
                .unwrap(),
        );
        let mut request = ImageRequest::generate("concurrent request");
        request.idempotency_key = Some("concurrent-key".to_owned());
        let first = store.create("scope-a", "019f-first", &request, 10, 10, MAX_BYTES);
        let second = store.create("scope-a", "019f-second", &request, 10, 10, MAX_BYTES);
        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap();
        let second = second.unwrap();
        assert_ne!(first.created, second.created);
        assert_eq!(first.job.summary.id, second.job.summary.id);
        assert_eq!(store.statistics().await.unwrap().total, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn database_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.sqlite3");
        std::fs::write(&target, []).unwrap();
        let link = directory.path().join("jobs.sqlite3");
        symlink(target, &link).unwrap();
        let error = SqliteImageJobStore::open(&link).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
    }

    #[tokio::test]
    async fn history_updates_only_soft_delete_terminal_jobs() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteImageJobStore::open(&directory.path().join("jobs.sqlite3"))
            .await
            .unwrap();
        store
            .create(
                SCOPE,
                "019f-job",
                &ImageRequest::generate("test"),
                10,
                10,
                MAX_BYTES,
            )
            .await
            .unwrap();
        let error = store
            .update_history(SCOPE, "019f-job", None, Some(true), 11)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(store.claim(SCOPE, "019f-job", 12).await.unwrap());
        store
            .succeed(SCOPE, "019f-job", &fixture_response("019f-job"), 13)
            .await
            .unwrap();
        let deleted = store
            .update_history(SCOPE, "019f-job", Some(true), Some(true), 14)
            .await
            .unwrap();
        assert!(deleted.summary.favorite);
        assert_eq!(deleted.summary.deleted, Some(14));
        assert!(
            store
                .list(
                    SCOPE,
                    ImageJobListFilter {
                        limit: 10,
                        ..ImageJobListFilter::default()
                    }
                )
                .await
                .unwrap()
                .is_empty()
        );
        let restored = store
            .update_history(SCOPE, "019f-job", None, Some(false), 15)
            .await
            .unwrap();
        assert_eq!(restored.summary.deleted, None);
        assert_eq!(store.prune(100, 1, 1, MAX_BYTES).await.unwrap(), 0);
        store
            .update_history(SCOPE, "019f-job", Some(false), None, 101)
            .await
            .unwrap();
        assert_eq!(store.prune(102, 1, 1, MAX_BYTES).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn history_filters_apply_before_cursor_pagination() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteImageJobStore::open(&directory.path().join("jobs.sqlite3"))
            .await
            .unwrap();
        for (id, prompt, created) in [
            ("019f-job-a", "Red fox 100%", 10),
            ("019f-job-b", "BLUE_bird study", 11),
            ("019f-job-c", "ČERVENÁ liška", 12),
        ] {
            store
                .create(
                    SCOPE,
                    id,
                    &ImageRequest::generate(prompt),
                    created,
                    10,
                    MAX_BYTES,
                )
                .await
                .unwrap();
            assert!(store.claim(SCOPE, id, created + 1).await.unwrap());
            store
                .succeed(SCOPE, id, &fixture_response(id), created + 2)
                .await
                .unwrap();
        }
        store
            .update_history(SCOPE, "019f-job-a", Some(true), None, 20)
            .await
            .unwrap();
        store
            .update_history(SCOPE, "019f-job-b", None, Some(true), 21)
            .await
            .unwrap();

        let percent = store
            .list(
                SCOPE,
                ImageJobListFilter {
                    limit: 10,
                    search: Some("100%".to_owned()),
                    favorite: Some(true),
                    ..ImageJobListFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(percent.len(), 1);
        assert_eq!(percent[0].id, "019f-job-a");

        let underscore = store
            .list(
                SCOPE,
                ImageJobListFilter {
                    limit: 10,
                    visibility: ImageJobVisibility::Hidden,
                    search: Some("blue_".to_owned()),
                    ..ImageJobListFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(underscore.len(), 1);
        assert_eq!(underscore[0].id, "019f-job-b");

        let unicode_case = store
            .list(
                SCOPE,
                ImageJobListFilter {
                    limit: 10,
                    search: Some("červená".to_owned()),
                    ..ImageJobListFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(unicode_case.len(), 1);
        assert_eq!(unicode_case[0].id, "019f-job-c");

        let injection = store
            .list(
                SCOPE,
                ImageJobListFilter {
                    limit: 10,
                    visibility: ImageJobVisibility::All,
                    search: Some("' OR 1=1 --".to_owned()),
                    ..ImageJobListFilter::default()
                },
            )
            .await
            .unwrap();
        assert!(injection.is_empty());

        let newest_favorite = store
            .list(
                SCOPE,
                ImageJobListFilter {
                    limit: 1,
                    visibility: ImageJobVisibility::All,
                    favorite: Some(true),
                    ..ImageJobListFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(newest_favorite[0].id, "019f-job-a");
        let after_favorite = store
            .list(
                SCOPE,
                ImageJobListFilter {
                    before: Some((newest_favorite[0].created, newest_favorite[0].id.clone())),
                    limit: 1,
                    visibility: ImageJobVisibility::All,
                    favorite: Some(true),
                    ..ImageJobListFilter::default()
                },
            )
            .await
            .unwrap();
        assert!(after_favorite.is_empty());
    }

    #[tokio::test]
    async fn version_one_database_migrates_prompt_search_without_losing_jobs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("jobs.sqlite3");
        let connection = Connection::open(&path).await.unwrap();
        let request = serde_json::to_string(&ImageRequest::generate("legacy copper fox")).unwrap();
        connection
            .call(move |connection| {
                connection.execute_batch(
                    "CREATE TABLE job_schema_migrations (
                       version INTEGER PRIMARY KEY,
                       applied_at INTEGER NOT NULL
                     );
                     CREATE TABLE image_jobs (
                       id TEXT PRIMARY KEY,
                       status TEXT NOT NULL,
                       created_at INTEGER NOT NULL,
                       updated_at INTEGER NOT NULL,
                       started_at INTEGER,
                       completed_at INTEGER,
                       request_json TEXT NOT NULL,
                       progress_stage TEXT,
                       partial_images INTEGER NOT NULL DEFAULT 0,
                       response_json TEXT,
                       error_json TEXT,
                       cancel_requested INTEGER NOT NULL DEFAULT 0,
                       favorite INTEGER NOT NULL DEFAULT 0,
                       deleted_at INTEGER
                     );
                     INSERT INTO job_schema_migrations(version, applied_at) VALUES (1, 10);",
                )?;
                connection.execute(
                    "INSERT INTO image_jobs(id,status,created_at,updated_at,request_json)
                     VALUES ('019f-legacy','queued',10,10,?1)",
                    [request],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        connection.close().await.unwrap();

        let store = SqliteImageJobStore::open(&path).await.unwrap();
        let results = store
            .list(
                "legacy-unowned",
                ImageJobListFilter {
                    limit: 10,
                    search: Some("COPPER".to_owned()),
                    ..ImageJobListFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "019f-legacy");
        let status = inspect_sqlite_job_schema(&path).await.unwrap();
        assert_eq!(status.version, Some(3));
        assert_eq!(status.current_version, 3);
        store.close().await.unwrap();

        let connection = Connection::open(&path).await.unwrap();
        connection
            .call(|connection| {
                connection.execute("UPDATE image_jobs SET prompt_search=''", [])?;
                connection.execute("DELETE FROM job_schema_migrations WHERE version=2", [])?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        connection.close().await.unwrap();
        let repaired = SqliteImageJobStore::open(&path).await.unwrap();
        assert_eq!(
            repaired
                .list(
                    "legacy-unowned",
                    ImageJobListFilter {
                        limit: 10,
                        search: Some("legacy copper".to_owned()),
                        ..ImageJobListFilter::default()
                    }
                )
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
