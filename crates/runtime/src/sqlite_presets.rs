//! Durable `SQLite` storage for reusable request presets.

use std::path::Path;

use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImagePreset, ImagePresetCreate, ImagePresetTemplate, ImagePresetWrite,
    validate_preset_name, validate_preset_write,
};
use tokio_rusqlite::{Connection, params, rusqlite::OptionalExtension as _};

type PresetRow = (String, Option<String>, String, u64, u64);

/// Durable named preset store sharing the configured server state database.
pub struct SqlitePresetStore {
    connection: Connection,
}

impl std::fmt::Debug for SqlitePresetStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqlitePresetStore")
            .finish_non_exhaustive()
    }
}

impl SqlitePresetStore {
    /// Opens the preset store and applies its idempotent schema.
    pub async fn open(path: &Path) -> Result<Self, BridgeError> {
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => {
                return Err(preset_storage_error(
                    "preset database must be a regular file",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(preset_storage_error("could not inspect preset database")),
        }
        let connection = Connection::open(path)
            .await
            .map_err(|_| preset_storage_error("could not open preset database"))?;
        connection
            .call(|connection| {
                connection.execute_batch(
                    "PRAGMA journal_mode=WAL;
                     PRAGMA synchronous=FULL;
                     PRAGMA busy_timeout=5000;
                     CREATE TABLE IF NOT EXISTS image_presets (
                       name TEXT PRIMARY KEY,
                       description TEXT,
                       template_json TEXT NOT NULL,
                       created_at INTEGER NOT NULL,
                       updated_at INTEGER NOT NULL
                     );
                     CREATE INDEX IF NOT EXISTS image_presets_updated_idx
                       ON image_presets(updated_at DESC, name ASC);",
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .map_err(|_| preset_storage_error("could not initialize preset database"))?;
        Ok(Self { connection })
    }

    /// Creates a preset and returns a conflict when the name already exists.
    pub async fn create(
        &self,
        create: ImagePresetCreate,
        now: u64,
    ) -> Result<ImagePreset, BridgeError> {
        validate_preset_name(&create.name)?;
        let write = ImagePresetWrite {
            description: create.description,
            template: create.template,
        };
        validate_preset_write(&write)?;
        let template = encode_template(&write.template)?;
        let name = create.name;
        let description = write.description;
        let stored_name = name.clone();
        let stored_description = description.clone();
        let inserted = self
            .connection
            .call(move |connection| {
                connection.execute(
                    "INSERT OR IGNORE INTO image_presets(name,description,template_json,created_at,updated_at)
                     VALUES (?1,?2,?3,?4,?4)",
                    params![stored_name, stored_description, template, now],
                )
            })
            .await
            .map_err(|_| preset_storage_error("could not create preset"))?;
        if inserted == 0 {
            return Err(BridgeError::new(
                ErrorCode::IdempotencyConflict,
                "a preset with this name already exists",
            )
            .with_detail("field", "name")
            .with_detail("resource", "preset"));
        }
        Ok(ImagePreset {
            name,
            description,
            template: write.template,
            created: now,
            updated: now,
        })
    }

    /// Returns one preset by exact name.
    pub async fn get(&self, name: &str) -> Result<ImagePreset, BridgeError> {
        validate_preset_name(name)?;
        let name = name.to_owned();
        let row = self
            .connection
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT name,description,template_json,created_at,updated_at
                         FROM image_presets WHERE name=?1",
                        [name],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            ))
                        },
                    )
                    .optional()
            })
            .await
            .map_err(|_| preset_storage_error("could not read preset"))?
            .ok_or_else(preset_not_found)?;
        decode_row(row)
    }

    /// Lists presets in stable name order after an optional exclusive cursor.
    pub async fn list(
        &self,
        after: Option<String>,
        limit: usize,
    ) -> Result<Vec<ImagePreset>, BridgeError> {
        let rows = self
            .connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT name,description,template_json,created_at,updated_at
                     FROM image_presets
                     WHERE (?1 IS NULL OR name > ?1)
                     ORDER BY name ASC LIMIT ?2",
                )?;
                statement
                    .query_map(params![after, limit], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    })?
                    .collect::<Result<Vec<PresetRow>, _>>()
            })
            .await
            .map_err(|_| preset_storage_error("could not list presets"))?;
        rows.into_iter().map(decode_row).collect()
    }

    /// Replaces an existing preset without changing its name or creation time.
    pub async fn replace(
        &self,
        name: &str,
        write: ImagePresetWrite,
        now: u64,
    ) -> Result<ImagePreset, BridgeError> {
        validate_preset_name(name)?;
        validate_preset_write(&write)?;
        let template = encode_template(&write.template)?;
        let stored_name = name.to_owned();
        let stored_description = write.description.clone();
        let updated = self
            .connection
            .call(move |connection| {
                connection.execute(
                    "UPDATE image_presets
                     SET description=?2,template_json=?3,updated_at=?4 WHERE name=?1",
                    params![stored_name, stored_description, template, now],
                )
            })
            .await
            .map_err(|_| preset_storage_error("could not update preset"))?;
        if updated == 0 {
            return Err(preset_not_found());
        }
        self.get(name).await
    }

    /// Deletes a preset, returning not-found when it does not exist.
    pub async fn delete(&self, name: &str) -> Result<(), BridgeError> {
        validate_preset_name(name)?;
        let name = name.to_owned();
        let deleted = self
            .connection
            .call(move |connection| {
                connection.execute("DELETE FROM image_presets WHERE name=?1", [name])
            })
            .await
            .map_err(|_| preset_storage_error("could not delete preset"))?;
        if deleted == 0 {
            return Err(preset_not_found());
        }
        Ok(())
    }
}

fn encode_template(template: &ImagePresetTemplate) -> Result<String, BridgeError> {
    serde_json::to_string(template).map_err(|_| preset_storage_error("could not encode preset"))
}

fn decode_row(row: PresetRow) -> Result<ImagePreset, BridgeError> {
    let (name, description, template, created, updated) = row;
    let template = serde_json::from_str(&template)
        .map_err(|_| preset_storage_error("stored preset is invalid"))?;
    Ok(ImagePreset {
        name,
        description,
        template,
        created,
        updated,
    })
}

fn preset_not_found() -> BridgeError {
    BridgeError::new(ErrorCode::InvalidRequest, "preset was not found")
        .with_detail("resource", "preset")
}

fn preset_storage_error(message: &'static str) -> BridgeError {
    BridgeError::new(ErrorCode::Internal, message).with_detail("stage", "preset_storage")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preset_crud_is_durable_and_conflict_safe() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state.sqlite3");
        let store = SqlitePresetStore::open(&path).await?;
        let created = store
            .create(
                ImagePresetCreate {
                    name: "portrait".to_owned(),
                    description: Some("Editorial portrait".to_owned()),
                    template: ImagePresetTemplate::default(),
                },
                10,
            )
            .await?;
        assert_eq!(created.name, "portrait");
        assert_eq!(store.list(None, 10).await?.len(), 1);
        let conflict = store
            .create(
                ImagePresetCreate {
                    name: "portrait".to_owned(),
                    description: None,
                    template: ImagePresetTemplate::default(),
                },
                11,
            )
            .await;
        assert!(matches!(
            conflict,
            Err(ref error) if error.code == ErrorCode::IdempotencyConflict
        ));
        let updated = store
            .replace(
                "portrait",
                ImagePresetWrite {
                    description: Some("Updated".to_owned()),
                    template: ImagePresetTemplate::default(),
                },
                12,
            )
            .await?;
        assert_eq!(updated.created, 10);
        assert_eq!(updated.updated, 12);
        store.delete("portrait").await?;
        assert!(store.get("portrait").await.is_err());
        Ok(())
    }
}
