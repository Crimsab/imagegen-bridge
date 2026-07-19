use std::io::{Cursor, Read as _};

use flate2::read::GzDecoder;
use imagegen_bridge::core::{BridgeError, ErrorCode};
use sha2::{Digest as _, Sha256};

use super::github::MAX_DOWNLOAD_BYTES;

pub(super) fn verify_checksum(
    archive_name: &str,
    archive: &[u8],
    manifest: &[u8],
) -> Result<(), BridgeError> {
    let manifest =
        std::str::from_utf8(manifest).map_err(|_| protocol("SHA256SUMS is not valid UTF-8"))?;
    let expected = manifest.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        (name == archive_name && fields.next().is_none()).then_some(digest)
    });
    let expected =
        expected.ok_or_else(|| protocol("SHA256SUMS does not list the release asset"))?;
    let actual = base16ct::lower::encode_string(&Sha256::digest(archive));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(protocol("release asset checksum verification failed"));
    }
    Ok(())
}

pub(super) fn extract_binary(archive_name: &str, bytes: &[u8]) -> Result<Vec<u8>, BridgeError> {
    if archive_name.ends_with(".tar.gz") {
        extract_tar(bytes)
    } else if std::path::Path::new(archive_name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        extract_zip(bytes)
    } else {
        Err(protocol("unsupported release archive format"))
    }
}

fn extract_tar(bytes: &[u8]) -> Result<Vec<u8>, BridgeError> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .map_err(|_| protocol("release archive is invalid"))?
    {
        let mut entry = entry.map_err(|_| protocol("release archive is invalid"))?;
        let path = entry
            .path()
            .map_err(|_| protocol("release archive path is invalid"))?;
        if entry.header().entry_type().is_file() && is_expected_path(&path, "imagegen-bridge") {
            return read_bounded(&mut entry);
        }
    }
    Err(protocol("release archive does not contain imagegen-bridge"))
}

fn extract_zip(bytes: &[u8]) -> Result<Vec<u8>, BridgeError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| protocol("release archive is invalid"))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| protocol("release archive is invalid"))?;
        let Some(path) = entry.enclosed_name() else {
            continue;
        };
        if entry.is_file() && is_expected_path(&path, "imagegen-bridge.exe") {
            return read_bounded(&mut entry);
        }
    }
    Err(protocol(
        "release archive does not contain imagegen-bridge.exe",
    ))
}

fn is_expected_path(path: &std::path::Path, expected_name: &str) -> bool {
    let components = path.components().collect::<Vec<_>>();
    (1..=2).contains(&components.len())
        && components
            .iter()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && path.file_name() == Some(std::ffi::OsStr::new(expected_name))
}

fn read_bounded(reader: &mut impl std::io::Read) -> Result<Vec<u8>, BridgeError> {
    let mut output = Vec::new();
    reader
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut output)
        .map_err(|_| protocol("release binary could not be extracted"))?;
    if output.is_empty() || output.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(protocol("release binary has an invalid size"));
    }
    Ok(output)
}

fn protocol(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn checksum_requires_exact_asset_name() {
        let bytes = b"archive";
        let digest = base16ct::lower::encode_string(&Sha256::digest(bytes));
        let manifest = format!("{digest}  wanted.tar.gz\n");
        assert!(verify_checksum("wanted.tar.gz", bytes, manifest.as_bytes()).is_ok());
        assert!(verify_checksum("other.tar.gz", bytes, manifest.as_bytes()).is_err());
    }

    #[test]
    fn extracts_only_the_expected_tar_entry() -> Result<(), Box<dyn std::error::Error>> {
        let mut compressed = Vec::new();
        {
            let encoder =
                flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let payload = b"verified executable";
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(
                &mut header,
                "imagegen-bridge-v0.1.2-linux-x86_64/imagegen-bridge",
                payload.as_slice(),
            )?;
            builder.into_inner()?.finish()?;
        }
        assert_eq!(
            extract_binary("release.tar.gz", &compressed)?,
            b"verified executable"
        );
        Ok(())
    }

    #[test]
    fn zip_path_traversal_is_not_accepted() -> Result<(), Box<dyn std::error::Error>> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer.start_file(
                "../imagegen-bridge.exe",
                zip::write::SimpleFileOptions::default(),
            )?;
            writer.write_all(b"untrusted")?;
            writer.finish()?;
        }
        assert!(extract_binary("release.zip", bytes.get_ref()).is_err());
        Ok(())
    }
}
