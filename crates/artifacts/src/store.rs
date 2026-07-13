//! Atomic, collision-safe artifact publication.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use imagegen_bridge_core::{BridgeError, ErrorCode, OutputFormat};
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::{ImageLimits, ImageMetadata, inspect_image};

/// Bridge-owned artifact returned to the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtifact {
    /// Opaque public identifier.
    pub id: String,
    /// Safe single-component filename.
    pub name: String,
    /// Internal absolute path; never expose this directly to remote clients.
    pub path: PathBuf,
    /// Verified image metadata.
    pub metadata: ImageMetadata,
}

/// Publishes verified images beneath one owned output root.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
    limits: ImageLimits,
}

impl ArtifactStore {
    /// Creates or opens an artifact root.
    pub fn new(root: impl Into<PathBuf>, limits: ImageLimits) -> Result<Self, BridgeError> {
        let root = root.into();
        fs::create_dir_all(&root)
            .map_err(|error| artifact_error(format!("could not create artifact root: {error}")))?;
        let root = fs::canonicalize(root)
            .map_err(|error| artifact_error(format!("could not open artifact root: {error}")))?;
        if !root.is_dir() {
            return Err(artifact_error("artifact root is not a directory"));
        }
        Ok(Self { root, limits })
    }

    /// Verifies and atomically publishes one image without overwriting a file.
    pub fn publish(
        &self,
        bytes: &[u8],
        filename_prefix: Option<&str>,
        expected_format: Option<OutputFormat>,
    ) -> Result<StoredArtifact, BridgeError> {
        let metadata = inspect_image(bytes, self.limits).map_err(|error| BridgeError {
            code: ErrorCode::Artifact,
            ..error
        })?;
        if expected_format.is_some_and(|expected| expected != metadata.format) {
            return Err(artifact_error(
                "generated image format does not match the effective request",
            ));
        }

        let prefix = sanitize_prefix(filename_prefix.unwrap_or("image"));
        let id = Uuid::now_v7().to_string();
        let name = format!("{prefix}-{id}.{}", extension(metadata.format));
        let destination = self.root.join(&name);
        let mut temporary = NamedTempFile::new_in(&self.root).map_err(|error| {
            artifact_error(format!("could not create temporary artifact: {error}"))
        })?;
        temporary
            .write_all(bytes)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| artifact_error(format!("could not write artifact: {error}")))?;
        temporary.persist_noclobber(&destination).map_err(|error| {
            artifact_error(format!(
                "could not publish artifact without overwrite: {error}"
            ))
        })?;
        sync_directory(&self.root)?;

        Ok(StoredArtifact {
            id,
            name,
            path: destination,
            metadata,
        })
    }

    /// Returns the private artifact root for trusted runtime code.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn sanitize_prefix(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                Some(character.to_ascii_lowercase())
            } else if character.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .take(64)
        .collect();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "image".to_owned()
    } else {
        sanitized.to_owned()
    }
}

const fn extension(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Png => "png",
        OutputFormat::Jpeg => "jpg",
        OutputFormat::Webp => "webp",
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), BridgeError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| artifact_error(format!("could not sync artifact directory: {error}")))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), BridgeError> {
    Ok(())
}

fn artifact_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Artifact, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;

    use super::*;
    use crate::inspect::test_png;

    #[test]
    fn publishes_verified_artifacts_without_name_reuse() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), ImageLimits::default()).unwrap();
        let bytes = test_png(2, 2);
        let first = store
            .publish(&bytes, Some("My portrait"), Some(OutputFormat::Png))
            .unwrap();
        let second = store
            .publish(&bytes, Some("My portrait"), Some(OutputFormat::Png))
            .unwrap();
        assert_ne!(first.name, second.name);
        assert!(first.name.starts_with("my-portrait-"));
        assert_eq!(fs::read(first.path).unwrap(), bytes);
    }

    #[test]
    fn rejects_mismatched_effective_format_before_write() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), ImageLimits::default()).unwrap();
        let error = store
            .publish(&test_png(1, 1), None, Some(OutputFormat::Jpeg))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Artifact);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
