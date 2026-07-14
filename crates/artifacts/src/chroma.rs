//! Deterministic chroma-key removal with bounded image decoding.

use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};
use imagegen_bridge_core::{BridgeError, ErrorCode, OutputFormat};

use crate::{ImageLimits, ImageMetadata, inspect_image};

const KEY_DOMINANCE_THRESHOLD: i16 = 16;
const ALPHA_NOISE_FLOOR: u8 = 8;
const ALPHA_SCALE: u64 = 65_535;

/// RGB color used as a generated solid background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromaKey(pub u8, pub u8, pub u8);

impl ChromaKey {
    /// Parses `#RRGGBB` or `RRGGBB`.
    pub fn parse(value: &str) -> Result<Self, BridgeError> {
        let value = value.strip_prefix('#').unwrap_or(value);
        if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(chroma_error(
                "chroma key must be a hex RGB color like #00ff00",
            ));
        }
        let channel = |offset| {
            u8::from_str_radix(&value[offset..offset + 2], 16)
                .map_err(|_| chroma_error("chroma key contains an invalid RGB channel"))
        };
        Ok(Self(channel(0)?, channel(2)?, channel(4)?))
    }

    /// Lowercase CSS-compatible hexadecimal representation.
    #[must_use]
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
}

/// Controls for a soft chroma matte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromaKeyOptions {
    /// Known key color requested from the image model.
    pub key: ChromaKey,
    /// Distance at or below which a pixel becomes fully transparent.
    pub transparent_threshold: u8,
    /// Distance at or above which a pixel becomes fully opaque.
    pub opaque_threshold: u8,
    /// Remove key-colored spill from partially transparent edges.
    pub despill: bool,
}

impl ChromaKeyOptions {
    fn validate(self) -> Result<(), BridgeError> {
        if self.transparent_threshold >= self.opaque_threshold {
            return Err(chroma_error(
                "transparent threshold must be lower than opaque threshold",
            ));
        }
        Ok(())
    }
}

impl Default for ChromaKeyOptions {
    fn default() -> Self {
        Self {
            key: ChromaKey(0, 255, 0),
            transparent_threshold: 12,
            opaque_threshold: 96,
            despill: true,
        }
    }
}

/// Alpha validation counters returned after background removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlphaSummary {
    /// Total decoded pixels.
    pub total_pixels: u64,
    /// Fully transparent pixels.
    pub transparent_pixels: u64,
    /// Partially transparent antialiasing pixels.
    pub partial_pixels: u64,
    /// Fully opaque pixels.
    pub opaque_pixels: u64,
    /// Transparent pixels among the four corners.
    pub transparent_corners: u8,
}

/// Re-encoded alpha image and its independently verified metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromaKeyResult {
    /// PNG or WebP bytes with an alpha channel.
    pub bytes: Vec<u8>,
    /// Metadata verified after encoding.
    pub metadata: ImageMetadata,
    /// Matte validation counters.
    pub alpha: AlphaSummary,
}

/// Removes a known solid chroma background and validates a usable alpha matte.
pub fn remove_chroma_key(
    bytes: &[u8],
    output_format: OutputFormat,
    options: ChromaKeyOptions,
    limits: ImageLimits,
) -> Result<ChromaKeyResult, BridgeError> {
    options.validate()?;
    if !matches!(output_format, OutputFormat::Png | OutputFormat::Webp) {
        return Err(chroma_error(
            "transparent output requires PNG or WebP encoding",
        ));
    }
    let source = inspect_image(bytes, limits)?;
    let decoder_format = image_format(source.format);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), decoder_format);
    let mut decode_limits = Limits::default();
    decode_limits.max_image_width = Some(limits.max_edge);
    decode_limits.max_image_height = Some(limits.max_edge);
    decode_limits.max_alloc = Some(limits.max_decode_alloc);
    reader.limits(decode_limits);
    let mut rgba = reader
        .decode()
        .map_err(|_| chroma_error("image could not be decoded for background removal"))?
        .to_rgba8();

    let key = [options.key.0, options.key.1, options.key.2];
    for pixel in rgba.pixels_mut() {
        let rgb = [pixel[0], pixel[1], pixel[2]];
        let distance = channel_distance(rgb, key);
        let key_like = looks_key_colored(rgb, key, distance);
        let matte_alpha = if key_like {
            soft_alpha(
                distance,
                options.transparent_threshold,
                options.opaque_threshold,
            )
            .min(dominance_alpha(rgb, key))
        } else {
            255
        };
        let mut alpha = u8::try_from((u16::from(matte_alpha) * u16::from(pixel[3]) + 127) / 255)
            .unwrap_or(u8::MAX);
        if alpha <= ALPHA_NOISE_FLOOR {
            alpha = 0;
        }
        if alpha == 0 {
            *pixel = image::Rgba([0, 0, 0, 0]);
            continue;
        }
        if options.despill && key_like {
            let cleaned = cleanup_spill(rgb, key, alpha);
            pixel[0] = cleaned[0];
            pixel[1] = cleaned[1];
            pixel[2] = cleaned[2];
        }
        pixel[3] = alpha;
    }

    let alpha = summarize_alpha(&rgba);
    validate_alpha(alpha, true)?;
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut encoded, image_format(output_format))
        .map_err(|_| chroma_error("transparent image could not be encoded"))?;
    let bytes = encoded.into_inner();
    let metadata = inspect_image(&bytes, limits)?;
    Ok(ChromaKeyResult {
        bytes,
        metadata,
        alpha,
    })
}

/// Samples the median RGB color from a bounded band around the image border.
pub fn detect_border_chroma_key(
    bytes: &[u8],
    limits: ImageLimits,
) -> Result<ChromaKey, BridgeError> {
    let source = inspect_image(bytes, limits)?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image_format(source.format));
    let mut decode_limits = Limits::default();
    decode_limits.max_image_width = Some(limits.max_edge);
    decode_limits.max_image_height = Some(limits.max_edge);
    decode_limits.max_alloc = Some(limits.max_decode_alloc);
    reader.limits(decode_limits);
    let rgba = reader
        .decode()
        .map_err(|_| chroma_error("image could not be decoded for key detection"))?
        .to_rgba8();
    let band = rgba.width().min(rgba.height()).clamp(1, 6);
    let step = (rgba.width().min(rgba.height()) / 256).max(1);
    let mut red = Vec::new();
    let mut green = Vec::new();
    let mut blue = Vec::new();
    let mut sample = |pixel: &image::Rgba<u8>| {
        red.push(pixel[0]);
        green.push(pixel[1]);
        blue.push(pixel[2]);
    };
    for x in (0..rgba.width()).step_by(step as usize) {
        for offset in 0..band {
            sample(rgba.get_pixel(x, offset));
            sample(rgba.get_pixel(x, rgba.height() - 1 - offset));
        }
    }
    for y in (0..rgba.height()).step_by(step as usize) {
        for offset in 0..band {
            sample(rgba.get_pixel(offset, y));
            sample(rgba.get_pixel(rgba.width() - 1 - offset, y));
        }
    }
    red.sort_unstable();
    green.sort_unstable();
    blue.sort_unstable();
    let middle = red.len() / 2;
    if red.is_empty() {
        return Err(chroma_error(
            "image border did not contain sampleable pixels",
        ));
    }
    Ok(ChromaKey(red[middle], green[middle], blue[middle]))
}

/// Verifies that an encoded image contains a plausible transparent-background matte.
pub fn inspect_transparent_alpha(
    bytes: &[u8],
    limits: ImageLimits,
) -> Result<AlphaSummary, BridgeError> {
    let source = inspect_image(bytes, limits)?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image_format(source.format));
    let mut decode_limits = Limits::default();
    decode_limits.max_image_width = Some(limits.max_edge);
    decode_limits.max_image_height = Some(limits.max_edge);
    decode_limits.max_alloc = Some(limits.max_decode_alloc);
    reader.limits(decode_limits);
    let rgba = reader
        .decode()
        .map_err(|_| chroma_error("image could not be decoded for alpha validation"))?
        .to_rgba8();
    let summary = summarize_alpha(&rgba);
    validate_alpha(summary, false)?;
    Ok(summary)
}

fn image_format(format: OutputFormat) -> ImageFormat {
    match format {
        OutputFormat::Png => ImageFormat::Png,
        OutputFormat::Jpeg => ImageFormat::Jpeg,
        OutputFormat::Webp => ImageFormat::WebP,
    }
}

fn channel_distance(rgb: [u8; 3], key: [u8; 3]) -> u8 {
    rgb.into_iter()
        .zip(key)
        .map(|(left, right)| left.abs_diff(right))
        .max()
        .unwrap_or(0)
}

fn spill_channels(key: [u8; 3]) -> [bool; 3] {
    let maximum = key.into_iter().max().unwrap_or(0);
    if maximum < 128 {
        return [false; 3];
    }
    key.map(|value| value >= maximum.saturating_sub(16) && value >= 128)
}

fn key_dominance(rgb: [u8; 3], key: [u8; 3]) -> i16 {
    let spill = spill_channels(key);
    if !spill.iter().any(|value| *value) {
        return 0;
    }
    let key_strength = rgb
        .iter()
        .enumerate()
        .filter(|(index, _)| spill[*index])
        .map(|(_, value)| *value)
        .min()
        .unwrap_or(0);
    let non_key_strength = rgb
        .iter()
        .enumerate()
        .filter(|(index, _)| !spill[*index])
        .map(|(_, value)| *value)
        .max()
        .unwrap_or(0);
    i16::from(key_strength) - i16::from(non_key_strength)
}

fn looks_key_colored(rgb: [u8; 3], key: [u8; 3], distance: u8) -> bool {
    distance <= 32 || key_dominance(rgb, key) >= KEY_DOMINANCE_THRESHOLD
}

fn soft_alpha(distance: u8, transparent: u8, opaque: u8) -> u8 {
    if distance <= transparent {
        return 0;
    }
    if distance >= opaque {
        return 255;
    }
    let ratio = u64::from(distance - transparent) * ALPHA_SCALE / u64::from(opaque - transparent);
    let smooth = ratio * ratio * (3 * ALPHA_SCALE - 2 * ratio) / (ALPHA_SCALE * ALPHA_SCALE);
    u8::try_from((smooth * 255 + ALPHA_SCALE / 2) / ALPHA_SCALE).unwrap_or(u8::MAX)
}

fn dominance_alpha(rgb: [u8; 3], key: [u8; 3]) -> u8 {
    let dominance = key_dominance(rgb, key);
    if dominance <= 0 {
        return 255;
    }
    let spill = spill_channels(key);
    let non_key_strength = rgb
        .iter()
        .enumerate()
        .filter(|(index, _)| !spill[*index])
        .map(|(_, value)| *value)
        .max()
        .unwrap_or(0);
    let denominator =
        (i16::from(key.into_iter().max().unwrap_or(0)) - i16::from(non_key_strength)).max(1);
    let remaining = denominator.saturating_sub(dominance.min(denominator));
    u8::try_from((i32::from(remaining) * 255 + i32::from(denominator) / 2) / i32::from(denominator))
        .unwrap_or(u8::MAX)
}

fn cleanup_spill(rgb: [u8; 3], key: [u8; 3], alpha: u8) -> [u8; 3] {
    if alpha >= 252 {
        return rgb;
    }
    let spill = spill_channels(key);
    let anchor = rgb
        .iter()
        .enumerate()
        .filter(|(index, _)| !spill[*index])
        .map(|(_, value)| *value)
        .max()
        .unwrap_or(0)
        .saturating_sub(1);
    let mut output = rgb;
    for (index, value) in output.iter_mut().enumerate() {
        if spill[index] {
            *value = (*value).min(anchor);
        }
    }
    output
}

fn summarize_alpha(image: &image::RgbaImage) -> AlphaSummary {
    let mut transparent_pixels = 0_u64;
    let mut partial_pixels = 0_u64;
    let mut opaque_pixels = 0_u64;
    for pixel in image.pixels() {
        match pixel[3] {
            0 => transparent_pixels += 1,
            255 => opaque_pixels += 1,
            _ => partial_pixels += 1,
        }
    }
    let maximum_x = image.width().saturating_sub(1);
    let maximum_y = image.height().saturating_sub(1);
    let transparent_corners = u8::try_from(
        [
            (0, 0),
            (maximum_x, 0),
            (0, maximum_y),
            (maximum_x, maximum_y),
        ]
        .into_iter()
        .filter(|(x, y)| image.get_pixel(*x, *y)[3] == 0)
        .count(),
    )
    .unwrap_or(4);
    AlphaSummary {
        total_pixels: u64::from(image.width()) * u64::from(image.height()),
        transparent_pixels,
        partial_pixels,
        opaque_pixels,
        transparent_corners,
    }
}

fn validate_alpha(
    summary: AlphaSummary,
    require_transparent_corners: bool,
) -> Result<(), BridgeError> {
    let minimum_region = (summary.total_pixels / 1_000).max(1);
    if summary.transparent_pixels < minimum_region {
        return Err(
            chroma_error("background removal produced no meaningful transparent region")
                .with_detail("transparent_pixels", summary.transparent_pixels)
                .with_detail("total_pixels", summary.total_pixels),
        );
    }
    if summary.opaque_pixels + summary.partial_pixels < minimum_region {
        return Err(chroma_error(
            "background removal erased the entire visible subject",
        ));
    }
    if require_transparent_corners && summary.transparent_corners < 2 {
        return Err(chroma_error(
            "background removal left an opaque border around the generated subject",
        )
        .with_detail("transparent_corners", summary.transparent_corners));
    }
    Ok(())
}

fn chroma_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Artifact, message).with_detail("stage", "transparent_background")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn keyed_fixture() -> Vec<u8> {
        let mut image = image::RgbaImage::from_pixel(20, 20, image::Rgba([0, 255, 0, 255]));
        for y in 5..15 {
            for x in 5..15 {
                image.put_pixel(x, y, image::Rgba([220, 30, 20, 255]));
            }
        }
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn removes_key_preserves_subject_and_normalizes_transparent_rgb() {
        let result = remove_chroma_key(
            &keyed_fixture(),
            OutputFormat::Png,
            ChromaKeyOptions::default(),
            ImageLimits::default(),
        )
        .unwrap();
        assert_eq!(result.alpha.transparent_pixels, 300);
        assert_eq!(result.alpha.opaque_pixels, 100);
        assert_eq!(result.alpha.transparent_corners, 4);
        let decoded = image::load_from_memory(&result.bytes).unwrap().to_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(decoded.get_pixel(10, 10).0, [220, 30, 20, 255]);
    }

    #[test]
    fn rejects_invalid_thresholds_and_non_keyed_outputs() {
        let invalid = ChromaKeyOptions {
            transparent_threshold: 96,
            opaque_threshold: 12,
            ..ChromaKeyOptions::default()
        };
        assert!(
            remove_chroma_key(
                &keyed_fixture(),
                OutputFormat::Png,
                invalid,
                ImageLimits::default()
            )
            .is_err()
        );

        let white = image::RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(white)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        assert!(
            remove_chroma_key(
                &bytes.into_inner(),
                OutputFormat::Png,
                ChromaKeyOptions::default(),
                ImageLimits::default()
            )
            .is_err()
        );
    }
}
