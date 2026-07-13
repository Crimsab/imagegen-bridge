//! Capability-scoped local and inline image input loading.

use std::{
    io::Read,
    path::{Component, Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cap_std::{ambient_authority, fs::Dir};
use imagegen_bridge_core::{BridgeError, ErrorCode, ImageInput, ImageSource};

use crate::{ImageLimits, ImageMetadata, inspect_image};

/// Loaded and verified image input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage {
    /// Encoded image body.
    pub bytes: Vec<u8>,
    /// Verified metadata.
    pub metadata: ImageMetadata,
    /// Optional safe logical filename supplied by the caller.
    pub filename: Option<String>,
}

struct AllowedRoot {
    canonical_path: PathBuf,
    dir: Dir,
}

/// Loads inputs without granting arbitrary filesystem authority.
pub struct InputLoader {
    roots: Vec<AllowedRoot>,
    limits: ImageLimits,
}

impl std::fmt::Debug for InputLoader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputLoader")
            .field("root_count", &self.roots.len())
            .field("limits", &self.limits)
            .finish()
    }
}

impl InputLoader {
    /// Opens configured roots as directory capabilities.
    pub fn new(
        roots: impl IntoIterator<Item = PathBuf>,
        limits: ImageLimits,
    ) -> Result<Self, BridgeError> {
        let roots = roots
            .into_iter()
            .map(|root| {
                let canonical_path = std::fs::canonicalize(&root).map_err(|error| {
                    input_error(format!("could not open an allowed input root: {error}"))
                })?;
                let dir = Dir::open_ambient_dir(&canonical_path, ambient_authority()).map_err(
                    |error| input_error(format!("could not open an allowed input root: {error}")),
                )?;
                Ok(AllowedRoot {
                    canonical_path,
                    dir,
                })
            })
            .collect::<Result<Vec<_>, BridgeError>>()?;
        Ok(Self { roots, limits })
    }

    /// Loads and verifies one image input.
    pub fn load(&self, input: &ImageInput) -> Result<LoadedImage, BridgeError> {
        let bytes = match &input.source {
            ImageSource::File { path } => self.load_file(path)?,
            ImageSource::Base64 { data } => self.decode_base64(data)?,
            ImageSource::DataUrl { data_url } => self.decode_data_url(data_url)?,
            ImageSource::Url { .. } => {
                return Err(input_error(
                    "remote URL input requires an explicitly configured remote fetcher",
                ));
            }
        };
        let metadata = inspect_image(&bytes, self.limits)?;
        if let Some(expected) = input.media_type.as_deref() {
            let actual = media_type(metadata.format);
            if expected != actual {
                return Err(input_error(format!(
                    "declared media type does not match image bytes; detected {actual}"
                )));
            }
        }
        Ok(LoadedImage {
            bytes,
            metadata,
            filename: input.filename.clone(),
        })
    }

    fn load_file(&self, path: &Path) -> Result<Vec<u8>, BridgeError> {
        if self.roots.is_empty() {
            return Err(input_error("local file inputs are disabled"));
        }
        validate_lexical_path(path)?;

        let mut last_error = None;
        for root in &self.roots {
            let relative = if path.is_absolute() {
                match path.strip_prefix(&root.canonical_path) {
                    Ok(relative) => relative,
                    Err(_) => continue,
                }
            } else {
                path
            };
            match root.dir.open(relative) {
                Ok(mut file) => {
                    let metadata = file.metadata().map_err(|error| {
                        input_error(format!("could not inspect input: {error}"))
                    })?;
                    if !metadata.is_file() {
                        return Err(input_error("local input is not a regular file"));
                    }
                    if metadata.len() > self.limits.max_encoded_bytes {
                        return Err(input_error("local input exceeds the configured byte limit"));
                    }
                    return read_bounded(&mut file, self.limits.max_encoded_bytes);
                }
                Err(error) => last_error = Some(error),
            }
        }
        let suffix = last_error
            .map(|error| format!(": {error}"))
            .unwrap_or_default();
        Err(input_error(format!(
            "local input is outside allowed roots or cannot be opened{suffix}"
        )))
    }

    fn decode_base64(&self, data: &str) -> Result<Vec<u8>, BridgeError> {
        let maximum_encoded = self
            .limits
            .max_encoded_bytes
            .saturating_mul(4)
            .div_ceil(3)
            .saturating_add(4);
        if u64::try_from(data.len()).unwrap_or(u64::MAX) > maximum_encoded {
            return Err(input_error(
                "base64 input exceeds the configured byte limit",
            ));
        }
        let decoded = STANDARD
            .decode(data)
            .map_err(|_| input_error("base64 input is malformed"))?;
        if u64::try_from(decoded.len()).unwrap_or(u64::MAX) > self.limits.max_encoded_bytes {
            return Err(input_error(
                "decoded input exceeds the configured byte limit",
            ));
        }
        Ok(decoded)
    }

    fn decode_data_url(&self, data_url: &str) -> Result<Vec<u8>, BridgeError> {
        let (header, encoded) = data_url
            .split_once(',')
            .ok_or_else(|| input_error("data URL is malformed"))?;
        if !header.starts_with("data:image/") || !header.ends_with(";base64") {
            return Err(input_error(
                "data URL must contain a supported image media type and base64 encoding",
            ));
        }
        self.decode_base64(encoded)
    }
}

fn validate_lexical_path(path: &Path) -> Result<(), BridgeError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(input_error(
            "local input path contains a forbidden component",
        ));
    }
    Ok(())
}

fn read_bounded(reader: &mut impl Read, maximum: u64) -> Result<Vec<u8>, BridgeError> {
    let mut bytes = Vec::new();
    reader
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| input_error(format!("could not read local input: {error}")))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum {
        return Err(input_error("input grew beyond the configured byte limit"));
    }
    Ok(bytes)
}

const fn media_type(format: imagegen_bridge_core::OutputFormat) -> &'static str {
    match format {
        imagegen_bridge_core::OutputFormat::Png => "image/png",
        imagegen_bridge_core::OutputFormat::Jpeg => "image/jpeg",
        imagegen_bridge_core::OutputFormat::Webp => "image/webp",
    }
}

fn input_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Input, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;

    use super::*;
    use crate::inspect::test_png;

    fn input(source: ImageSource) -> ImageInput {
        ImageInput {
            source,
            media_type: Some("image/png".to_owned()),
            filename: Some("input.png".to_owned()),
        }
    }

    #[test]
    fn loads_bounded_base64_and_verifies_type() {
        let bytes = test_png(2, 2);
        let loader = InputLoader::new(Vec::new(), ImageLimits::default()).unwrap();
        let image = loader
            .load(&input(ImageSource::Base64 {
                data: STANDARD.encode(&bytes),
            }))
            .unwrap();
        assert_eq!(image.bytes, bytes);
        assert_eq!(image.metadata.width, 2);
    }

    #[test]
    fn loads_file_beneath_allowed_root() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("input.png"), test_png(2, 3)).unwrap();
        let loader = InputLoader::new([root.path().to_path_buf()], ImageLimits::default()).unwrap();
        let image = loader
            .load(&input(ImageSource::File {
                path: PathBuf::from("input.png"),
            }))
            .unwrap();
        assert_eq!((image.metadata.width, image.metadata.height), (2, 3));
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let root = tempfile::tempdir().unwrap();
        let loader = InputLoader::new([root.path().to_path_buf()], ImageLimits::default()).unwrap();
        assert!(
            loader
                .load(&input(ImageSource::File {
                    path: PathBuf::from("../escape.png"),
                }))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_that_escapes_allowed_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("outside.png"), test_png(1, 1)).unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let loader = InputLoader::new([root.path().to_path_buf()], ImageLimits::default()).unwrap();
        assert!(
            loader
                .load(&input(ImageSource::File {
                    path: PathBuf::from("escape/outside.png"),
                }))
                .is_err()
        );
    }
}
