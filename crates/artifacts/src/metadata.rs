//! Bounded, pixel-preserving generation metadata embedding.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use imagegen_bridge_core::{BridgeError, ErrorCode, OutputFormat};

use crate::{ImageLimits, ImageMetadata, inspect_image};

/// Maximum uncompressed JSON bytes accepted by every supported XMP container.
pub const MAX_EMBEDDED_METADATA_BYTES: usize = 40 * 1024;
const MAX_XMP_BYTES: usize = 56 * 1024;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const PNG_XMP_KEYWORD: &[u8] = b"XML:com.adobe.xmp";
const JPEG_XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const XMP_OPEN: &[u8] = b"<igb:Metadata>";
const XMP_CLOSE: &[u8] = b"</igb:Metadata>";
const MAX_WEBP_CHUNKS: usize = 4_096;

/// Embeds one bounded JSON generation record without decoding or re-encoding pixels.
///
/// PNG uses an uncompressed XMP iTXt chunk, JPEG uses the standard XMP APP1
/// namespace, and WebP uses an XMP chunk in an extended container. Both the
/// source and result are completely decoded under the supplied limits.
pub fn embed_image_metadata(
    bytes: &[u8],
    expected_format: OutputFormat,
    encoded_json: &[u8],
    limits: ImageLimits,
) -> Result<(Vec<u8>, ImageMetadata), BridgeError> {
    validate_json(encoded_json)?;
    let source = inspect_image(bytes, limits).map_err(as_artifact_error)?;
    if source.format != expected_format {
        return Err(metadata_error(
            "embedded metadata target format does not match the image",
        ));
    }
    let xmp = xmp_packet(encoded_json);
    let embedded = match source.format {
        OutputFormat::Png => embed_png(bytes, &xmp)?,
        OutputFormat::Jpeg => embed_jpeg(bytes, &xmp)?,
        OutputFormat::Webp => embed_webp(bytes, &xmp, source.width, source.height)?,
    };
    let result = inspect_image(&embedded, limits).map_err(as_artifact_error)?;
    if result.format != source.format
        || result.width != source.width
        || result.height != source.height
    {
        return Err(metadata_error(
            "embedded metadata changed the verified image properties",
        ));
    }
    Ok((embedded, result))
}

/// Extracts the bridge JSON generation record from a PNG, JPEG, or WebP image.
///
/// `Ok(None)` means the image has no Imagegen Bridge record. The extracted
/// bytes are guaranteed to be a bounded JSON object.
pub fn extract_embedded_metadata(
    bytes: &[u8],
    limits: ImageLimits,
) -> Result<Option<Vec<u8>>, BridgeError> {
    let inspected = inspect_image(bytes, limits).map_err(as_artifact_error)?;
    let xmp = match inspected.format {
        OutputFormat::Png => extract_png_xmp(bytes)?,
        OutputFormat::Jpeg => extract_jpeg_xmp(bytes)?,
        OutputFormat::Webp => extract_webp_xmp(bytes)?,
    };
    let Some(xmp) = xmp else {
        return Ok(None);
    };
    let encoded = between(&xmp, XMP_OPEN, XMP_CLOSE)
        .ok_or_else(|| metadata_error("embedded XMP record is malformed"))?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| metadata_error("embedded XMP record is malformed"))?;
    validate_json(&decoded)?;
    Ok(Some(decoded))
}

fn validate_json(encoded: &[u8]) -> Result<(), BridgeError> {
    if encoded.is_empty()
        || encoded.len() > MAX_EMBEDDED_METADATA_BYTES
        || !matches!(
            serde_json::from_slice::<serde_json::Value>(encoded),
            Ok(serde_json::Value::Object(_))
        )
    {
        return Err(metadata_error(
            "embedded metadata must be a JSON object no larger than 40 KiB",
        ));
    }
    Ok(())
}

fn xmp_packet(encoded_json: &[u8]) -> Vec<u8> {
    let encoded = STANDARD.encode(encoded_json);
    let mut xmp = Vec::with_capacity(encoded.len() + 320);
    xmp.extend_from_slice(
        br#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="imagegen-bridge"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:igb="https://github.com/Crimsab/imagegen-bridge/ns/1.0/"><igb:Metadata>"#,
    );
    xmp.extend_from_slice(encoded.as_bytes());
    xmp.extend_from_slice(br"</igb:Metadata></rdf:Description></rdf:RDF></x:xmpmeta>");
    xmp
}

fn embed_png(bytes: &[u8], xmp: &[u8]) -> Result<Vec<u8>, BridgeError> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(metadata_error("PNG container is malformed"));
    }
    let mut output = Vec::with_capacity(bytes.len().saturating_add(xmp.len() + 64));
    output.extend_from_slice(PNG_SIGNATURE);
    let mut position = PNG_SIGNATURE.len();
    let mut inserted = false;
    while position < bytes.len() {
        let (kind, data, end) = png_chunk(bytes, position)?;
        if kind == b"IEND" && !inserted {
            append_png_xmp(&mut output, xmp)?;
            inserted = true;
        }
        if !(kind == b"iTXt" && is_png_xmp(data)) {
            output.extend_from_slice(&bytes[position..end]);
        }
        position = end;
    }
    if !inserted || position != bytes.len() {
        return Err(metadata_error("PNG container has no valid IEND chunk"));
    }
    Ok(output)
}

fn extract_png_xmp(bytes: &[u8]) -> Result<Option<Vec<u8>>, BridgeError> {
    let mut position = PNG_SIGNATURE.len();
    while position < bytes.len() {
        let (kind, data, end) = png_chunk(bytes, position)?;
        if kind == b"iTXt" && is_png_xmp(data) {
            let mut cursor = PNG_XMP_KEYWORD.len();
            if data.get(cursor) != Some(&0) || data.get(cursor + 1..cursor + 3) != Some(&[0, 0]) {
                return Err(metadata_error("PNG XMP chunk is malformed"));
            }
            cursor += 3;
            cursor = nul_terminated_end(data, cursor)?;
            cursor = nul_terminated_end(data, cursor)?;
            return bounded_xmp(&data[cursor..]).map(Some);
        }
        position = end;
    }
    Ok(None)
}

fn png_chunk(bytes: &[u8], position: usize) -> Result<(&[u8], &[u8], usize), BridgeError> {
    let header = bytes
        .get(position..position.saturating_add(8))
        .ok_or_else(|| metadata_error("PNG chunk header is truncated"))?;
    let length = usize::try_from(u32::from_be_bytes(
        header[..4]
            .try_into()
            .map_err(|_| metadata_error("PNG chunk length is malformed"))?,
    ))
    .map_err(|_| metadata_error("PNG chunk is too large"))?;
    let data_start = position
        .checked_add(8)
        .ok_or_else(|| metadata_error("PNG chunk is too large"))?;
    let data_end = data_start
        .checked_add(length)
        .ok_or_else(|| metadata_error("PNG chunk is too large"))?;
    let end = data_end
        .checked_add(4)
        .ok_or_else(|| metadata_error("PNG chunk is too large"))?;
    let data = bytes
        .get(data_start..data_end)
        .ok_or_else(|| metadata_error("PNG chunk data is truncated"))?;
    bytes
        .get(data_end..end)
        .ok_or_else(|| metadata_error("PNG chunk checksum is truncated"))?;
    Ok((&header[4..8], data, end))
}

fn append_png_xmp(output: &mut Vec<u8>, xmp: &[u8]) -> Result<(), BridgeError> {
    let mut data = Vec::with_capacity(PNG_XMP_KEYWORD.len() + xmp.len() + 5);
    data.extend_from_slice(PNG_XMP_KEYWORD);
    data.extend_from_slice(&[0, 0, 0, 0, 0]);
    data.extend_from_slice(xmp);
    append_png_chunk(output, *b"iTXt", &data)
}

fn append_png_chunk(output: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) -> Result<(), BridgeError> {
    let length =
        u32::try_from(data.len()).map_err(|_| metadata_error("PNG metadata chunk is too large"))?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(&kind);
    output.extend_from_slice(data);
    let mut checksum_input = Vec::with_capacity(kind.len() + data.len());
    checksum_input.extend_from_slice(&kind);
    checksum_input.extend_from_slice(data);
    output.extend_from_slice(&crc32(&checksum_input).to_be_bytes());
    Ok(())
}

fn embed_jpeg(bytes: &[u8], xmp: &[u8]) -> Result<Vec<u8>, BridgeError> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(metadata_error("JPEG container is malformed"));
    }
    let segment_bytes = JPEG_XMP_HEADER
        .len()
        .checked_add(xmp.len())
        .and_then(|value| value.checked_add(2))
        .ok_or_else(|| metadata_error("JPEG XMP record is too large"))?;
    let segment_length = u16::try_from(segment_bytes)
        .map_err(|_| metadata_error("JPEG XMP record exceeds the APP1 limit"))?;
    let mut output = Vec::with_capacity(bytes.len().saturating_add(xmp.len() + 64));
    output.extend_from_slice(&bytes[..2]);
    output.extend_from_slice(&[0xff, 0xe1]);
    output.extend_from_slice(&segment_length.to_be_bytes());
    output.extend_from_slice(JPEG_XMP_HEADER);
    output.extend_from_slice(xmp);
    output.extend_from_slice(&bytes[2..]);
    Ok(output)
}

fn extract_jpeg_xmp(bytes: &[u8]) -> Result<Option<Vec<u8>>, BridgeError> {
    let mut position = 2;
    while position < bytes.len() {
        if bytes.get(position) != Some(&0xff) {
            return Err(metadata_error("JPEG marker stream is malformed"));
        }
        while bytes.get(position) == Some(&0xff) {
            position += 1;
        }
        let marker = *bytes
            .get(position)
            .ok_or_else(|| metadata_error("JPEG marker stream is truncated"))?;
        position += 1;
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length_bytes = bytes
            .get(position..position.saturating_add(2))
            .ok_or_else(|| metadata_error("JPEG segment is truncated"))?;
        let length = usize::from(u16::from_be_bytes(
            length_bytes
                .try_into()
                .map_err(|_| metadata_error("JPEG segment is malformed"))?,
        ));
        if length < 2 {
            return Err(metadata_error("JPEG segment length is malformed"));
        }
        let data_start = position + 2;
        let end = position
            .checked_add(length)
            .ok_or_else(|| metadata_error("JPEG segment is too large"))?;
        let data = bytes
            .get(data_start..end)
            .ok_or_else(|| metadata_error("JPEG segment is truncated"))?;
        if marker == 0xe1 && data.starts_with(JPEG_XMP_HEADER) {
            return bounded_xmp(&data[JPEG_XMP_HEADER.len()..]).map(Some);
        }
        position = end;
    }
    Ok(None)
}

fn embed_webp(bytes: &[u8], xmp: &[u8], width: u32, height: u32) -> Result<Vec<u8>, BridgeError> {
    let mut has_vp8x = false;
    let mut flags = 0x04;
    visit_webp_chunks(bytes, |kind, data| {
        has_vp8x |= kind == b"VP8X";
        flags |= match kind {
            b"ICCP" => 0x20,
            b"ALPH" => 0x10,
            b"EXIF" => 0x08,
            b"ANIM" | b"ANMF" => 0x02,
            b"VP8L" if vp8l_has_alpha(data) => 0x10,
            _ => 0,
        };
        Ok(true)
    })?;
    let mut output = Vec::with_capacity(bytes.len().saturating_add(xmp.len() + 32));
    output.extend_from_slice(b"RIFF\0\0\0\0WEBP");
    if !has_vp8x {
        let mut vp8x = [0_u8; 10];
        vp8x[0] = flags;
        write_u24(&mut vp8x[4..7], width.saturating_sub(1))?;
        write_u24(&mut vp8x[7..10], height.saturating_sub(1))?;
        append_webp_chunk(&mut output, b"VP8X", &vp8x)?;
    }
    visit_webp_chunks(bytes, |kind, data| {
        if kind == b"XMP " {
            return Ok(true);
        }
        if kind == b"VP8X" {
            if data.len() != 10 {
                return Err(metadata_error("WebP VP8X chunk is malformed"));
            }
            let mut updated = data.to_vec();
            updated[0] |= 0x04;
            append_webp_chunk(&mut output, kind, &updated)?;
        } else {
            append_webp_chunk(&mut output, kind, data)?;
        }
        Ok(true)
    })?;
    append_webp_chunk(&mut output, b"XMP ", xmp)?;
    let riff_size = u32::try_from(output.len().saturating_sub(8))
        .map_err(|_| metadata_error("WebP container is too large"))?;
    output[4..8].copy_from_slice(&riff_size.to_le_bytes());
    Ok(output)
}

fn extract_webp_xmp(bytes: &[u8]) -> Result<Option<Vec<u8>>, BridgeError> {
    let mut found = None;
    visit_webp_chunks(bytes, |kind, data| {
        if kind == b"XMP " {
            found = Some(bounded_xmp(data)?);
            return Ok(false);
        }
        Ok(true)
    })?;
    Ok(found)
}

fn visit_webp_chunks<'a>(
    bytes: &'a [u8],
    mut visit: impl FnMut(&'a [u8], &'a [u8]) -> Result<bool, BridgeError>,
) -> Result<(), BridgeError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(metadata_error("WebP container is malformed"));
    }
    let advertised = usize::try_from(u32::from_le_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| metadata_error("WebP RIFF size is malformed"))?,
    ))
    .map_err(|_| metadata_error("WebP RIFF size is too large"))?;
    if advertised.checked_add(8) != Some(bytes.len()) {
        return Err(metadata_error("WebP RIFF size does not match the payload"));
    }
    let mut position = 12;
    let mut count = 0_usize;
    while position < bytes.len() {
        count = count
            .checked_add(1)
            .ok_or_else(|| metadata_error("WebP chunk count is too large"))?;
        if count > MAX_WEBP_CHUNKS {
            return Err(metadata_error("WebP chunk count exceeds the limit"));
        }
        let header = bytes
            .get(position..position.saturating_add(8))
            .ok_or_else(|| metadata_error("WebP chunk header is truncated"))?;
        let size = usize::try_from(u32::from_le_bytes(
            header[4..8]
                .try_into()
                .map_err(|_| metadata_error("WebP chunk size is malformed"))?,
        ))
        .map_err(|_| metadata_error("WebP chunk is too large"))?;
        let data_start = position + 8;
        let data_end = data_start
            .checked_add(size)
            .ok_or_else(|| metadata_error("WebP chunk is too large"))?;
        let padded_end = data_end
            .checked_add(size & 1)
            .ok_or_else(|| metadata_error("WebP chunk is too large"))?;
        let data = bytes
            .get(data_start..data_end)
            .ok_or_else(|| metadata_error("WebP chunk data is truncated"))?;
        bytes
            .get(data_end..padded_end)
            .ok_or_else(|| metadata_error("WebP chunk padding is truncated"))?;
        if !visit(&header[..4], data)? {
            return Ok(());
        }
        position = padded_end;
    }
    Ok(())
}

fn append_webp_chunk(output: &mut Vec<u8>, kind: &[u8], data: &[u8]) -> Result<(), BridgeError> {
    if kind.len() != 4 {
        return Err(metadata_error("WebP chunk type is malformed"));
    }
    let size = u32::try_from(data.len())
        .map_err(|_| metadata_error("WebP metadata chunk is too large"))?;
    output.extend_from_slice(kind);
    output.extend_from_slice(&size.to_le_bytes());
    output.extend_from_slice(data);
    if data.len() & 1 == 1 {
        output.push(0);
    }
    Ok(())
}

fn vp8l_has_alpha(data: &[u8]) -> bool {
    if data.len() < 5 || data[0] != 0x2f {
        return false;
    }
    let bits = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
    bits & (1 << 28) != 0
}

fn write_u24(output: &mut [u8], value: u32) -> Result<(), BridgeError> {
    if output.len() != 3 || value > 0x00ff_ffff {
        return Err(metadata_error("WebP canvas dimensions are too large"));
    }
    let encoded = value.to_le_bytes();
    output.copy_from_slice(&encoded[..3]);
    Ok(())
}

fn nul_terminated_end(bytes: &[u8], start: usize) -> Result<usize, BridgeError> {
    let offset = bytes
        .get(start..)
        .and_then(|tail| tail.iter().position(|byte| *byte == 0))
        .ok_or_else(|| metadata_error("PNG XMP text field is malformed"))?;
    start
        .checked_add(offset + 1)
        .ok_or_else(|| metadata_error("PNG XMP text field is too large"))
}

fn is_png_xmp(data: &[u8]) -> bool {
    data.starts_with(PNG_XMP_KEYWORD) && data.get(PNG_XMP_KEYWORD.len()) == Some(&0)
}

fn bounded_xmp(bytes: &[u8]) -> Result<Vec<u8>, BridgeError> {
    if bytes.is_empty() || bytes.len() > MAX_XMP_BYTES {
        return Err(metadata_error("embedded XMP record exceeds the size limit"));
    }
    Ok(bytes.to_vec())
}

fn between<'a>(bytes: &'a [u8], open: &[u8], close: &[u8]) -> Option<&'a [u8]> {
    let start = bytes
        .windows(open.len())
        .position(|window| window == open)?
        + open.len();
    let end = bytes[start..]
        .windows(close.len())
        .position(|window| window == close)?
        + start;
    Some(&bytes[start..end])
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn as_artifact_error(error: BridgeError) -> BridgeError {
    BridgeError {
        code: ErrorCode::Artifact,
        message: "image failed verification while embedding metadata".to_owned(),
        provider: error.provider,
        details: error.details,
        ..error
    }
}

fn metadata_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Artifact, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use image::{DynamicImage, ImageFormat};

    use super::*;

    const RECORD: &[u8] = br#"{"prompt":"a paper fox","model":"gpt-image-2"}"#;

    fn test_image(format: OutputFormat) -> Vec<u8> {
        let image = if format == OutputFormat::Jpeg {
            DynamicImage::new_rgb8(3, 2)
        } else {
            DynamicImage::new_rgba8(3, 2)
        };
        let image_format = match format {
            OutputFormat::Png => ImageFormat::Png,
            OutputFormat::Jpeg => ImageFormat::Jpeg,
            OutputFormat::Webp => ImageFormat::WebP,
        };
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, image_format).unwrap();
        output.into_inner()
    }

    #[test]
    fn embeds_and_extracts_all_supported_formats_without_changing_dimensions() {
        for format in [OutputFormat::Png, OutputFormat::Jpeg, OutputFormat::Webp] {
            let source = test_image(format);
            let source_metadata = inspect_image(&source, ImageLimits::default()).unwrap();
            assert_eq!(
                extract_embedded_metadata(&source, ImageLimits::default()).unwrap(),
                None
            );
            let (embedded, metadata) =
                embed_image_metadata(&source, format, RECORD, ImageLimits::default()).unwrap();
            assert_eq!(metadata.format, format);
            assert_eq!((metadata.width, metadata.height), (3, 2));
            assert_ne!(metadata.sha256, source_metadata.sha256);
            assert_eq!(
                image::load_from_memory(&embedded).unwrap().to_rgba8(),
                image::load_from_memory(&source).unwrap().to_rgba8()
            );
            assert_eq!(
                extract_embedded_metadata(&embedded, ImageLimits::default()).unwrap(),
                Some(RECORD.to_vec())
            );

            let (replaced, _) = embed_image_metadata(
                &embedded,
                format,
                br#"{"version":2}"#,
                ImageLimits::default(),
            )
            .unwrap();
            assert_eq!(
                extract_embedded_metadata(&replaced, ImageLimits::default()).unwrap(),
                Some(br#"{"version":2}"#.to_vec())
            );
        }
    }

    #[test]
    fn rejects_non_object_and_oversized_metadata() {
        let png = test_image(OutputFormat::Png);
        assert!(
            embed_image_metadata(&png, OutputFormat::Png, b"[]", ImageLimits::default()).is_err()
        );
        let oversized = vec![b'a'; MAX_EMBEDDED_METADATA_BYTES + 1];
        assert!(
            embed_image_metadata(&png, OutputFormat::Png, &oversized, ImageLimits::default())
                .is_err()
        );
    }

    #[test]
    fn maximum_metadata_record_fits_the_jpeg_app1_container() {
        let mut record = b"{\"value\":\"".to_vec();
        record.resize(MAX_EMBEDDED_METADATA_BYTES - 2, b'a');
        record.extend_from_slice(b"\"}");
        assert_eq!(record.len(), MAX_EMBEDDED_METADATA_BYTES);
        let jpeg = test_image(OutputFormat::Jpeg);
        let (embedded, _) =
            embed_image_metadata(&jpeg, OutputFormat::Jpeg, &record, ImageLimits::default())
                .unwrap();
        assert_eq!(
            extract_embedded_metadata(&embedded, ImageLimits::default()).unwrap(),
            Some(record)
        );
    }

    #[test]
    fn webp_chunk_cursor_enforces_a_work_limit_without_descriptor_storage() {
        fn container(chunks: usize) -> Vec<u8> {
            let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
            for _ in 0..chunks {
                bytes.extend_from_slice(b"JUNK\0\0\0\0");
            }
            let size = u32::try_from(bytes.len() - 8).unwrap();
            bytes[4..8].copy_from_slice(&size.to_le_bytes());
            bytes
        }

        let at_limit = container(MAX_WEBP_CHUNKS);
        let mut visited = 0_usize;
        visit_webp_chunks(&at_limit, |_, _| {
            visited += 1;
            Ok(true)
        })
        .unwrap();
        assert_eq!(visited, MAX_WEBP_CHUNKS);

        let over_limit = container(MAX_WEBP_CHUNKS + 1);
        let error = visit_webp_chunks(&over_limit, |_, _| Ok(true)).unwrap_err();
        assert!(error.message.contains("chunk count exceeds"));
    }

    #[test]
    fn webp_chunk_cursor_can_stop_before_unrelated_trailing_bytes() {
        let mut bytes = b"RIFF\0\0\0\0WEBPXMP \0\0\0\0x".to_vec();
        let size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
        let mut visited = 0;
        visit_webp_chunks(&bytes, |kind, _| {
            visited += 1;
            Ok(kind != b"XMP ")
        })
        .unwrap();
        assert_eq!(visited, 1);
    }
}
