//! Image type, integrity, dimension, and checksum inspection.

use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};
use imagegen_bridge_core::{BridgeError, ErrorCode, OutputFormat};
use sha2::{Digest, Sha256};

/// Limits enforced while inspecting and decoding an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    /// Maximum encoded image bytes.
    pub max_encoded_bytes: u64,
    /// Maximum width or height.
    pub max_edge: u32,
    /// Maximum decoded pixel count.
    pub max_pixels: u64,
    /// Maximum memory that the decoder may allocate.
    pub max_decode_alloc: u64,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 32 * 1024 * 1024,
            max_edge: 16_384,
            max_pixels: 64 * 1024 * 1024,
            max_decode_alloc: 256 * 1024 * 1024,
        }
    }
}

/// Verified metadata for encoded image bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageMetadata {
    /// Detected encoded format.
    pub format: OutputFormat,
    /// Decoded width in pixels.
    pub width: u32,
    /// Decoded height in pixels.
    pub height: u32,
    /// Encoded byte length.
    pub bytes: u64,
    /// Lowercase hexadecimal SHA-256 digest.
    pub sha256: String,
}

/// Fully verifies an encoded PNG, JPEG, or WebP image under bounded limits.
///
/// This checks magic bytes, probes dimensions without allocating a pixel
/// buffer, applies dimension/pixel/allocation limits, then performs a complete
/// decode so truncated payloads do not pass as valid artifacts.
pub fn inspect_image(bytes: &[u8], limits: ImageLimits) -> Result<ImageMetadata, BridgeError> {
    let encoded_len = u64::try_from(bytes.len()).map_err(|_| image_error("image is too large"))?;
    if bytes.is_empty() || encoded_len > limits.max_encoded_bytes {
        return Err(image_error(
            "encoded image exceeds the configured size limit",
        ));
    }

    let image_type = imagesize::image_type(bytes)
        .map_err(|_| image_error("input is not a supported PNG, JPEG, or WebP image"))?;
    let (format, decoder_format) = match image_type {
        imagesize::ImageType::Png => (OutputFormat::Png, ImageFormat::Png),
        imagesize::ImageType::Jpeg => (OutputFormat::Jpeg, ImageFormat::Jpeg),
        imagesize::ImageType::Webp => (OutputFormat::Webp, ImageFormat::WebP),
        _ => return Err(image_error("input image format is not enabled")),
    };
    let dimensions =
        imagesize::blob_size(bytes).map_err(|_| image_error("could not read image dimensions"))?;
    let width = u32::try_from(dimensions.width)
        .map_err(|_| image_error("image width is outside the supported range"))?;
    let height = u32::try_from(dimensions.height)
        .map_err(|_| image_error("image height is outside the supported range"))?;
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > limits.max_edge
        || height > limits.max_edge
        || pixels > limits.max_pixels
    {
        return Err(image_error("image dimensions exceed configured limits"));
    }

    let mut reader = ImageReader::with_format(Cursor::new(bytes), decoder_format);
    let mut decode_limits = Limits::default();
    decode_limits.max_image_width = Some(limits.max_edge);
    decode_limits.max_image_height = Some(limits.max_edge);
    decode_limits.max_alloc = Some(limits.max_decode_alloc);
    reader.limits(decode_limits);
    let decoded = reader
        .decode()
        .map_err(|_| image_error("image payload is malformed or incomplete"))?;
    if decoded.width() != width || decoded.height() != height {
        return Err(image_error("image headers and decoded dimensions disagree"));
    }

    let sha256 = format!("{:x}", Sha256::digest(bytes));
    Ok(ImageMetadata {
        format,
        width,
        height,
        bytes: encoded_len,
        sha256,
    })
}

fn image_error(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Input, message)
}

#[cfg(test)]
pub(crate) fn test_png(width: u32, height: u32) -> Vec<u8> {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    let image = image::DynamicImage::new_rgba8(width, height);
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Png).unwrap();
    output.into_inner()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn validates_complete_png_and_checksum() {
        let bytes = test_png(3, 2);
        let metadata = inspect_image(&bytes, ImageLimits::default()).unwrap();
        assert_eq!(metadata.format, OutputFormat::Png);
        assert_eq!((metadata.width, metadata.height), (3, 2));
        assert_eq!(metadata.sha256.len(), 64);
    }

    #[test]
    fn rejects_truncated_image_after_header_probe() {
        let mut bytes = test_png(3, 2);
        bytes.truncate(bytes.len() - 8);
        assert!(inspect_image(&bytes, ImageLimits::default()).is_err());
    }

    #[test]
    fn enforces_dimension_limits_before_publish() {
        let bytes = test_png(3, 2);
        let limits = ImageLimits {
            max_edge: 2,
            ..ImageLimits::default()
        };
        assert!(inspect_image(&bytes, limits).is_err());
    }
}
