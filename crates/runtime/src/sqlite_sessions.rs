//! Durable session-to-thread bindings with a deliberately minimal schema.

use std::path::Path;

use async_trait::async_trait;
use imagegen_bridge_codex_app_server::SessionBindingStore;
use imagegen_bridge_core::{BridgeError, ErrorCode};
use tokio_rusqlite::{Connection, params};

const MAX_IDENTIFIER_BYTES: usize = 512;

/// `SQLite`-backed provider session bindings.
#[derive(Debug)]
pub struct SqliteSessionBindingStore {
    connection: Connection,
    provider: String,
}

impl SqliteSessionBindingStore {
    /// Opens the database and applies idempotent schema migrations.
    pub async fn open(path: impl AsRef<Path>, provider: &str) -> Result<Self, BridgeError> {
        validate_identifier(provider, "provider")?;
        let connection = Connection::open(path)
            .await
            .map_err(|_| session_error("could not open session database"))?;
        connection
            .call(|connection| {
                connection.execute_batch(
                    "PRAGMA foreign_keys = ON;
                     PRAGMA journal_mode = WAL;
                     PRAGMA synchronous = FULL;
                     CREATE TABLE IF NOT EXISTS schema_migrations (
                       version INTEGER PRIMARY KEY,
                       applied_at INTEGER NOT NULL
                     );
                     CREATE TABLE IF NOT EXISTS session_bindings (
                       provider TEXT NOT NULL,
                       session_key TEXT NOT NULL,
                       thread_id TEXT NOT NULL,
                       created_at INTEGER NOT NULL,
                       updated_at INTEGER NOT NULL,
                       PRIMARY KEY (provider, session_key)
                     );
                     INSERT OR IGNORE INTO schema_migrations(version, applied_at)
                     VALUES (1, unixepoch());",
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| session_error("could not migrate session database"))?;
        Ok(Self {
            connection,
            provider: provider.to_owned(),
        })
    }

    /// Closes the database worker after pending calls complete.
    pub async fn close(self) -> Result<(), BridgeError> {
        self.connection
            .close()
            .await
            .map_err(|_| session_error("could not close session database"))
    }
}

#[async_trait]
impl SessionBindingStore for SqliteSessionBindingStore {
    async fn get(&self, key: &str) -> Result<Option<String>, BridgeError> {
        validate_identifier(key, "session key")?;
        let provider = self.provider.clone();
        let key = key.to_owned();
        self.connection
            .call(
                move |connection| -> tokio_rusqlite::rusqlite::Result<Option<String>> {
                    let mut statement = connection.prepare_cached(
                        "SELECT thread_id FROM session_bindings
                     WHERE provider = ?1 AND session_key = ?2",
                    )?;
                    let mut rows = statement.query(params![provider, key])?;
                    rows.next()?.map(|row| row.get(0)).transpose()
                },
            )
            .await
            .map_err(|_| session_error("could not read session binding"))
    }

    async fn put(&self, key: &str, thread_id: &str) -> Result<(), BridgeError> {
        validate_identifier(key, "session key")?;
        validate_identifier(thread_id, "thread ID")?;
        let provider = self.provider.clone();
        let key = key.to_owned();
        let thread_id = thread_id.to_owned();
        self.connection
            .call(move |connection| {
                connection.execute(
                    "INSERT INTO session_bindings(
                       provider, session_key, thread_id, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, unixepoch(), unixepoch())
                     ON CONFLICT(provider, session_key) DO UPDATE SET
                       thread_id = excluded.thread_id,
                       updated_at = unixepoch()",
                    params![provider, key, thread_id],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| session_error("could not persist session binding"))
    }

    async fn delete(&self, key: &str) -> Result<(), BridgeError> {
        validate_identifier(key, "session key")?;
        let provider = self.provider.clone();
        let key = key.to_owned();
        self.connection
            .call(move |connection| {
                connection.execute(
                    "DELETE FROM session_bindings WHERE provider = ?1 AND session_key = ?2",
                    params![provider, key],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| session_error("could not delete session binding"))
    }
}

fn validate_identifier(value: &str, label: &str) -> Result<(), BridgeError> {
    if value.trim().is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(session_error(format!("invalid {label}")));
    }
    Ok(())
}

fn session_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Session, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn binding_survives_database_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let store = SqliteSessionBindingStore::open(&path, "codex-app-server")
            .await
            .unwrap();
        store.put("gallery", "thread-1").await.unwrap();
        store.close().await.unwrap();

        let reopened = SqliteSessionBindingStore::open(&path, "codex-app-server")
            .await
            .unwrap();
        assert_eq!(
            reopened.get("gallery").await.unwrap().as_deref(),
            Some("thread-1")
        );
        reopened.delete("gallery").await.unwrap();
        assert_eq!(reopened.get("gallery").await.unwrap(), None);
    }

    #[tokio::test]
    async fn providers_have_independent_namespaces() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let first = SqliteSessionBindingStore::open(&path, "first")
            .await
            .unwrap();
        let second = SqliteSessionBindingStore::open(&path, "second")
            .await
            .unwrap();
        first.put("same-key", "thread-a").await.unwrap();
        second.put("same-key", "thread-b").await.unwrap();
        assert_eq!(
            first.get("same-key").await.unwrap().as_deref(),
            Some("thread-a")
        );
        assert_eq!(
            second.get("same-key").await.unwrap().as_deref(),
            Some("thread-b")
        );
    }

    #[tokio::test]
    async fn rejects_empty_or_control_character_identifiers() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteSessionBindingStore::open(
            directory.path().join("sessions.sqlite3"),
            "codex-app-server",
        )
        .await
        .unwrap();
        assert!(store.put("", "thread").await.is_err());
        assert!(store.put("key", "thread\nsecret").await.is_err());
    }
}
