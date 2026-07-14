//! Atomic, collision-safe artifact publication.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use imagegen_bridge_core::{ArtifactCollisionPolicy, BridgeError, ErrorCode, OutputFormat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::{ImageLimits, ImageMetadata, inspect_image};

const OWNERSHIP_DIRECTORY: &str = ".imagegen-bridge-ownership";
const MAX_MARKER_BYTES: u64 = 4 * 1024;

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

/// Per-request artifact placement below the configured owned root.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactPublication<'a> {
    /// Portable relative directory, or the root when absent.
    pub directory: Option<&'a str>,
    /// Exact single-image filename, or a generated UUID name when absent.
    pub filename: Option<&'a str>,
    /// Behavior when the explicit filename exists.
    pub collision: ArtifactCollisionPolicy,
}

/// Publishes verified images beneath one owned output root.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
    ownership_root: PathBuf,
    limits: ImageLimits,
}

/// Bounded policy for deleting bridge-owned artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Delete artifacts at least this old.
    pub max_age: Duration,
    /// Optional maximum retained artifact count, keeping newest valid records.
    pub max_artifacts: Option<usize>,
    /// Hard bound on ownership records inspected per cleanup pass.
    pub max_scan_entries: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(7 * 24 * 60 * 60),
            max_artifacts: None,
            max_scan_entries: 100_000,
        }
    }
}

/// Safe aggregate result from one retention pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Ownership entries inspected.
    pub scanned: usize,
    /// Verified owned artifacts and markers removed.
    pub deleted: usize,
    /// Invalid, changed, missing, or otherwise non-deletable records.
    pub skipped: usize,
    /// Whether the scan stopped at its configured entry bound.
    pub scan_limit_reached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipRecord {
    version: u8,
    id: String,
    name: String,
    created_at: u64,
    sha256: String,
}

struct OwnedCandidate {
    marker: PathBuf,
    artifact: PathBuf,
    record: OwnershipRecord,
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
        let ownership_path = root.join(OWNERSHIP_DIRECTORY);
        fs::create_dir_all(&ownership_path).map_err(|error| {
            artifact_error(format!("could not create artifact ownership root: {error}"))
        })?;
        if fs::symlink_metadata(&ownership_path)
            .map_err(|error| {
                artifact_error(format!(
                    "could not inspect artifact ownership root: {error}"
                ))
            })?
            .file_type()
            .is_symlink()
        {
            return Err(artifact_error(
                "artifact ownership root must not be a symbolic link",
            ));
        }
        let ownership_root = fs::canonicalize(ownership_path).map_err(|error| {
            artifact_error(format!("could not open artifact ownership root: {error}"))
        })?;
        if !ownership_root.is_dir()
            || ownership_root.parent() != Some(root.as_path())
            || !ownership_root.starts_with(&root)
            || ownership_root.file_name().and_then(|value| value.to_str())
                != Some(OWNERSHIP_DIRECTORY)
        {
            return Err(artifact_error(
                "artifact ownership root must be a real child directory",
            ));
        }
        Ok(Self {
            root,
            ownership_root,
            limits,
        })
    }

    /// Verifies and atomically publishes one image without overwriting a file.
    pub fn publish(
        &self,
        bytes: &[u8],
        filename_prefix: Option<&str>,
        expected_format: Option<OutputFormat>,
    ) -> Result<StoredArtifact, BridgeError> {
        self.publish_with_options(
            bytes,
            filename_prefix,
            expected_format,
            ArtifactPublication::default(),
        )
    }

    /// Verifies and atomically publishes one image at a constrained relative location.
    pub fn publish_with_options(
        &self,
        bytes: &[u8],
        filename_prefix: Option<&str>,
        expected_format: Option<OutputFormat>,
        publication: ArtifactPublication<'_>,
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

        let id = Uuid::now_v7().to_string();
        let (directory, portable_directory) = self.publication_directory(publication.directory)?;
        if let Some(filename) = publication.filename
            && !safe_filename(filename, metadata.format)
        {
            return Err(artifact_error(
                "output filename must be a safe component with a matching image extension",
            ));
        }
        let requested_name = publication.filename.map_or_else(
            || {
                let prefix = sanitize_prefix(filename_prefix.unwrap_or("image"));
                format!("{prefix}-{id}.{}", extension(metadata.format))
            },
            |filename| filename_with_extension(filename, metadata.format),
        );
        let (destination, filename) = publish_noclobber(
            &directory,
            &requested_name,
            bytes,
            publication.collision,
            publication.filename.is_some(),
        )?;
        if let Err(error) = sync_directory(&directory) {
            let _ = fs::remove_file(&destination);
            return Err(error);
        }

        let name = portable_directory.map_or_else(
            || filename.clone(),
            |directory| format!("{directory}/{filename}"),
        );

        let record = OwnershipRecord {
            version: 1,
            id: id.clone(),
            name: name.clone(),
            created_at: unix_timestamp(SystemTime::now())?,
            sha256: metadata.sha256.clone(),
        };
        if let Err(error) = self.publish_ownership(&record) {
            let _ = fs::remove_file(&destination);
            let _ = sync_directory(&self.root);
            return Err(error);
        }

        Ok(StoredArtifact {
            id,
            name,
            path: destination,
            metadata,
        })
    }

    fn publication_directory(
        &self,
        relative: Option<&str>,
    ) -> Result<(PathBuf, Option<String>), BridgeError> {
        let Some(relative) = relative else {
            return Ok((self.root.clone(), None));
        };
        if !safe_relative(relative) {
            return Err(artifact_error(
                "output directory is not a safe relative path",
            ));
        }
        let mut directory = self.root.clone();
        for component in relative.split('/') {
            directory.push(component);
            match fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(artifact_error(
                        "output directory component must not be a file or symlink",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&directory).map_err(|_| {
                        artifact_error("could not create output directory component")
                    })?;
                }
                Err(_) => return Err(artifact_error("could not inspect output directory")),
            }
        }
        let canonical = fs::canonicalize(&directory)
            .map_err(|_| artifact_error("could not open output directory"))?;
        if !canonical.starts_with(&self.root) || canonical == self.ownership_root {
            return Err(artifact_error("output directory escapes the artifact root"));
        }
        Ok((canonical, Some(relative.to_owned())))
    }

    /// Returns the private artifact root for trusted runtime code.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Deletes only verified artifacts with bridge-created ownership records.
    pub fn cleanup(
        &self,
        policy: RetentionPolicy,
        now: SystemTime,
    ) -> Result<CleanupReport, BridgeError> {
        if policy.max_scan_entries == 0 {
            return Err(artifact_error(
                "retention scan limit must be greater than zero",
            ));
        }
        let now = unix_timestamp(now)?;
        let cutoff = now.saturating_sub(policy.max_age.as_secs());
        let mut report = CleanupReport::default();
        let mut candidates = Vec::new();
        for entry in fs::read_dir(&self.ownership_root)
            .map_err(|error| artifact_error(format!("could not scan ownership records: {error}")))?
        {
            if report.scanned >= policy.max_scan_entries {
                report.scan_limit_reached = true;
                break;
            }
            report.scanned += 1;
            let Ok(entry) = entry else {
                report.skipped += 1;
                continue;
            };
            match self.read_candidate(&entry.path()) {
                Ok(candidate) if self.verify_candidate(&candidate).is_ok() => {
                    candidates.push(candidate);
                }
                Err(()) | Ok(_) => report.skipped += 1,
            }
        }

        candidates.sort_by(|left, right| {
            right
                .record
                .created_at
                .cmp(&left.record.created_at)
                .then_with(|| right.record.id.cmp(&left.record.id))
        });
        for (index, candidate) in candidates.into_iter().enumerate() {
            let exceeds_count = policy.max_artifacts.is_some_and(|maximum| index >= maximum);
            let expired = candidate.record.created_at <= cutoff;
            if !expired && !exceeds_count {
                continue;
            }
            if self.remove_verified(&candidate).is_ok() {
                report.deleted += 1;
            } else {
                report.skipped += 1;
            }
        }
        Ok(report)
    }

    fn publish_ownership(&self, record: &OwnershipRecord) -> Result<(), BridgeError> {
        let encoded = serde_json::to_vec(record)
            .map_err(|_| artifact_error("could not encode artifact ownership record"))?;
        if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_MARKER_BYTES {
            return Err(artifact_error("artifact ownership record is too large"));
        }
        let destination = self.ownership_root.join(format!("{}.json", record.id));
        let mut temporary = NamedTempFile::new_in(&self.ownership_root).map_err(|error| {
            artifact_error(format!("could not create ownership record: {error}"))
        })?;
        temporary
            .write_all(&encoded)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| {
                artifact_error(format!("could not write ownership record: {error}"))
            })?;
        temporary.persist_noclobber(destination).map_err(|error| {
            artifact_error(format!("could not publish ownership record: {error}"))
        })?;
        sync_directory(&self.ownership_root)
    }

    fn read_candidate(&self, marker: &Path) -> Result<OwnedCandidate, ()> {
        let marker_metadata = fs::symlink_metadata(marker).map_err(|_| ())?;
        if !marker_metadata.file_type().is_file() || marker_metadata.len() > MAX_MARKER_BYTES {
            return Err(());
        }
        let marker_name = marker
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(())?;
        let encoded = fs::read(marker).map_err(|_| ())?;
        let record: OwnershipRecord = serde_json::from_slice(&encoded).map_err(|_| ())?;
        if record.version != 1
            || marker_name != format!("{}.json", record.id)
            || Uuid::parse_str(&record.id).is_err()
            || !safe_relative(&record.name)
            || record.sha256.len() != 64
            || !record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(());
        }
        Ok(OwnedCandidate {
            marker: marker.to_owned(),
            artifact: self.root.join(&record.name),
            record,
        })
    }

    fn remove_verified(&self, candidate: &OwnedCandidate) -> Result<(), ()> {
        self.verify_candidate(candidate)?;
        fs::remove_file(&candidate.artifact).map_err(|_| ())?;
        fs::remove_file(&candidate.marker).map_err(|_| ())?;
        sync_directory(&self.root).map_err(|_| ())?;
        sync_directory(&self.ownership_root).map_err(|_| ())
    }

    fn verify_candidate(&self, candidate: &OwnedCandidate) -> Result<(), ()> {
        let metadata = fs::symlink_metadata(&candidate.artifact).map_err(|_| ())?;
        if !metadata.file_type().is_file() || metadata.len() > self.limits.max_encoded_bytes {
            return Err(());
        }
        let bytes = fs::read(&candidate.artifact).map_err(|_| ())?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if digest != candidate.record.sha256 || inspect_image(&bytes, self.limits).is_err() {
            return Err(());
        }
        Ok(())
    }
}

fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && component.len() <= 160
                && !component.starts_with('.')
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn safe_filename(value: &str, format: OutputFormat) -> bool {
    if value.is_empty()
        || value.len() > 160
        || value.starts_with('.')
        || value.contains(['/', '\\'])
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return false;
    }
    let Some((_, extension)) = value.rsplit_once('.') else {
        return true;
    };
    match format {
        OutputFormat::Png => extension.eq_ignore_ascii_case("png"),
        OutputFormat::Jpeg => {
            extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg")
        }
        OutputFormat::Webp => extension.eq_ignore_ascii_case("webp"),
    }
}

fn filename_with_extension(filename: &str, format: OutputFormat) -> String {
    if Path::new(filename).extension().is_some() {
        filename.to_owned()
    } else {
        format!("{filename}.{}", extension(format))
    }
}

fn publish_noclobber(
    directory: &Path,
    requested_name: &str,
    bytes: &[u8],
    collision: ArtifactCollisionPolicy,
    explicit_filename: bool,
) -> Result<(PathBuf, String), BridgeError> {
    let attempts = if explicit_filename && collision == ArtifactCollisionPolicy::Suffix {
        10_000
    } else {
        1
    };
    for attempt in 0..attempts {
        let name = if attempt == 0 {
            requested_name.to_owned()
        } else {
            suffixed_filename(requested_name, attempt + 1)
        };
        let destination = directory.join(&name);
        let mut temporary = NamedTempFile::new_in(directory)
            .map_err(|_| artifact_error("could not create temporary artifact"))?;
        temporary
            .write_all(bytes)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|_| artifact_error("could not write artifact"))?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => return Ok((destination, name)),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => {
                return Err(artifact_error(
                    "could not publish artifact without overwrite",
                ));
            }
        }
    }
    Err(artifact_error(
        "artifact collision suffix limit was reached",
    ))
}

fn suffixed_filename(filename: &str, suffix: usize) -> String {
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("image");
    path.extension()
        .and_then(|value| value.to_str())
        .map_or_else(
            || format!("{stem}-{suffix}"),
            |extension| format!("{stem}-{suffix}.{extension}"),
        )
}

fn unix_timestamp(time: SystemTime) -> Result<u64, BridgeError> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| artifact_error("system time is before the Unix epoch"))
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
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name() != OWNERSHIP_DIRECTORY)
                .count(),
            0
        );
    }

    #[test]
    fn cleanup_deletes_only_verified_bridge_owned_artifacts() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), ImageLimits::default()).unwrap();
        let owned = store
            .publish(&test_png(2, 2), Some("owned"), Some(OutputFormat::Png))
            .unwrap();
        let unowned = root.path().join("unowned.png");
        fs::write(&unowned, test_png(2, 2)).unwrap();
        let report = store
            .cleanup(
                RetentionPolicy {
                    max_age: Duration::ZERO,
                    ..RetentionPolicy::default()
                },
                SystemTime::now() + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!owned.path.exists());
        assert!(unowned.exists());
    }

    #[test]
    fn cleanup_does_not_delete_an_owned_path_after_content_replacement() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), ImageLimits::default()).unwrap();
        let owned = store
            .publish(&test_png(2, 2), None, Some(OutputFormat::Png))
            .unwrap();
        fs::write(&owned.path, test_png(3, 3)).unwrap();
        let report = store
            .cleanup(
                RetentionPolicy {
                    max_age: Duration::ZERO,
                    ..RetentionPolicy::default()
                },
                SystemTime::now() + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped, 1);
        assert!(owned.path.exists());
    }

    #[test]
    fn cleanup_scan_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), ImageLimits::default()).unwrap();
        for index in 0..3 {
            fs::write(
                store.ownership_root.join(format!("invalid-{index}.json")),
                b"{}",
            )
            .unwrap();
        }
        let report = store
            .cleanup(
                RetentionPolicy {
                    max_scan_entries: 2,
                    ..RetentionPolicy::default()
                },
                SystemTime::now(),
            )
            .unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.skipped, 2);
        assert!(report.scan_limit_reached);
    }

    #[test]
    fn publishes_nested_exact_names_and_cleans_them_up() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), ImageLimits::default()).unwrap();
        let bytes = test_png(2, 2);
        let owned = store
            .publish_with_options(
                &bytes,
                None,
                Some(OutputFormat::Png),
                ArtifactPublication {
                    directory: Some("portraits/people"),
                    filename: Some("alice"),
                    collision: ArtifactCollisionPolicy::Error,
                },
            )
            .unwrap();
        assert_eq!(owned.name, "portraits/people/alice.png");
        assert_eq!(fs::read(&owned.path).unwrap(), bytes);

        let report = store
            .cleanup(
                RetentionPolicy {
                    max_age: Duration::ZERO,
                    ..RetentionPolicy::default()
                },
                SystemTime::now() + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!owned.path.exists());
    }

    #[test]
    fn explicit_collision_can_error_or_select_a_suffix() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), ImageLimits::default()).unwrap();
        let bytes = test_png(2, 2);
        let exact = ArtifactPublication {
            filename: Some("portrait.png"),
            ..ArtifactPublication::default()
        };
        store
            .publish_with_options(&bytes, None, Some(OutputFormat::Png), exact)
            .unwrap();
        assert!(
            store
                .publish_with_options(&bytes, None, Some(OutputFormat::Png), exact)
                .is_err()
        );
        let suffixed = store
            .publish_with_options(
                &bytes,
                None,
                Some(OutputFormat::Png),
                ArtifactPublication {
                    collision: ArtifactCollisionPolicy::Suffix,
                    ..exact
                },
            )
            .unwrap();
        assert_eq!(suffixed.name, "portrait-2.png");
    }

    #[test]
    fn direct_store_calls_reject_filename_traversal_and_symlink_directories() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), ImageLimits::default()).unwrap();
        let bytes = test_png(2, 2);
        assert!(
            store
                .publish_with_options(
                    &bytes,
                    None,
                    Some(OutputFormat::Png),
                    ArtifactPublication {
                        filename: Some("../escape.png"),
                        ..ArtifactPublication::default()
                    },
                )
                .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
            assert!(
                store
                    .publish_with_options(
                        &bytes,
                        None,
                        Some(OutputFormat::Png),
                        ArtifactPublication {
                            directory: Some("linked"),
                            ..ArtifactPublication::default()
                        },
                    )
                    .is_err()
            );
        }
    }
}
