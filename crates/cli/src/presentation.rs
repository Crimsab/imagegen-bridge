use std::{
    env, fs,
    io::{self, IsTerminal as _, Write as _},
    path::{Path, PathBuf},
    process::Command,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use imagegen_bridge::core::{
    BridgeError, ErrorCode, ImagePayload, ImageRequest, ImageResponse, ResponseFormat,
};
use sha2::{Digest as _, Sha256};

use crate::{args::PresentationArgs, output::Output};

pub(crate) fn prepare_request(
    request: &mut ImageRequest,
    presentation: PresentationArgs,
    allow_implicit_artifact: bool,
    output: &Output,
) -> Result<(), BridgeError> {
    if !presentation.requested() {
        return Ok(());
    }
    if presentation.preview && output.is_machine() {
        return Err(invalid("--preview cannot be combined with machine output"));
    }
    match request.output.response_format {
        ResponseFormat::Artifact => Ok(()),
        ResponseFormat::Url if presentation.open && !presentation.preview => Ok(()),
        ResponseFormat::B64Json if allow_implicit_artifact => {
            request.output.response_format = ResponseFormat::Artifact;
            Ok(())
        }
        ResponseFormat::B64Json => Err(invalid(
            "--open/--preview requires artifact output; omit an explicit response format or select artifact",
        )),
        ResponseFormat::Url => Err(invalid("terminal preview requires artifact output")),
        ResponseFormat::Metadata => Err(invalid(
            "--open/--preview cannot display metadata-only output",
        )),
    }
}

pub(crate) fn present(
    response: &ImageResponse,
    presentation: PresentationArgs,
    artifact_root: &Path,
    max_encoded_bytes: u64,
    output: &Output,
) -> Result<(), BridgeError> {
    if !presentation.requested() {
        return Ok(());
    }
    let mut artifacts = Vec::new();
    let mut urls = Vec::new();
    for image in &response.data {
        match &image.payload {
            ImagePayload::Artifact {
                name: Some(name), ..
            } => artifacts.push(resolve_local_file(
                artifact_root,
                name,
                max_encoded_bytes,
                Some(&image.sha256),
            )?),
            ImagePayload::Url { url } => urls.push(url.clone()),
            ImagePayload::Artifact { name: None, .. }
            | ImagePayload::Metadata
            | ImagePayload::B64Json { .. } => {
                return Err(artifact_error(
                    "generated output cannot be opened from this response",
                ));
            }
        }
    }
    if presentation.preview {
        preview(&artifacts, max_encoded_bytes, output)?;
    }
    if presentation.open {
        for artifact in &artifacts {
            open_path(artifact)?;
        }
        for url in &urls {
            open_url(url)?;
        }
    }
    Ok(())
}

pub(crate) fn resolve_local_file(
    root: &Path,
    name: &str,
    max_bytes: u64,
    expected_sha256: Option<&str>,
) -> Result<PathBuf, BridgeError> {
    let root = fs::canonicalize(root)
        .map_err(|_| artifact_error("could not open the configured artifact root"))?;
    let candidate = fs::canonicalize(root.join(name))
        .map_err(|_| artifact_error("generated artifact is unavailable"))?;
    let metadata = fs::symlink_metadata(&candidate)
        .map_err(|_| artifact_error("could not inspect generated artifact"))?;
    if !candidate.starts_with(&root)
        || !metadata.file_type().is_file()
        || metadata.len() > max_bytes
    {
        return Err(artifact_error("generated artifact path is not trusted"));
    }
    if let Some(expected) = expected_sha256 {
        let bytes = fs::read(&candidate)
            .map_err(|_| artifact_error("could not re-read generated artifact"))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes
            || format!("{:x}", Sha256::digest(&bytes)) != expected
        {
            return Err(artifact_error(
                "generated artifact no longer matches its verified checksum",
            ));
        }
    }
    Ok(candidate)
}

fn preview(paths: &[PathBuf], max_bytes: u64, output: &Output) -> Result<(), BridgeError> {
    if paths.is_empty() {
        return Err(artifact_error("terminal preview requires artifact output"));
    }
    if !io::stdout().is_terminal() {
        return output.status("terminal preview unavailable: stdout is not a terminal");
    }
    let protocol = detect_protocol(
        env::var("TERM").ok().as_deref(),
        env::var("TERM_PROGRAM").ok().as_deref(),
        env::var("LC_TERMINAL").ok().as_deref(),
        env::var_os("KITTY_WINDOW_ID").is_some(),
    );
    let Some(protocol) = protocol else {
        return output.status(
            "terminal preview unavailable: use --open or a Kitty/iTerm2-compatible terminal",
        );
    };
    let mut stdout = io::stdout().lock();
    for path in paths {
        match protocol {
            PreviewProtocol::Kitty => {
                if path.extension().and_then(|value| value.to_str()) != Some("png") {
                    output.status(
                        "Kitty terminal preview supports PNG artifacts; use --format png or --open",
                    )?;
                    continue;
                }
                let path = path
                    .to_str()
                    .ok_or_else(|| artifact_error("artifact path is not portable UTF-8"))?;
                writeln!(
                    stdout,
                    "\x1b_Ga=T,f=100,t=f;{}\x1b\\",
                    STANDARD.encode(path)
                )
                .map_err(|_| output_error("could not write Kitty image preview"))?;
            }
            PreviewProtocol::Iterm2 => {
                let bytes = fs::read(path)
                    .map_err(|_| artifact_error("could not read generated artifact"))?;
                if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
                    return Err(artifact_error("generated artifact exceeds preview limits"));
                }
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("image");
                writeln!(
                    stdout,
                    "\x1b]1337;File=name={};size={};inline=1:{}\x07",
                    STANDARD.encode(name),
                    bytes.len(),
                    STANDARD.encode(bytes)
                )
                .map_err(|_| output_error("could not write iTerm2 image preview"))?;
            }
        }
    }
    stdout
        .flush()
        .map_err(|_| output_error("could not flush terminal preview"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewProtocol {
    Kitty,
    Iterm2,
}

fn detect_protocol(
    term: Option<&str>,
    term_program: Option<&str>,
    lc_terminal: Option<&str>,
    kitty_window: bool,
) -> Option<PreviewProtocol> {
    let term = term.unwrap_or_default().to_ascii_lowercase();
    if kitty_window || term.contains("kitty") || term.contains("ghostty") {
        return Some(PreviewProtocol::Kitty);
    }
    let program = term_program
        .or(lc_terminal)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if program.contains("iterm") || program.contains("wezterm") {
        return Some(PreviewProtocol::Iterm2);
    }
    None
}

#[cfg(target_os = "macos")]
fn open_path(path: &Path) -> Result<(), BridgeError> {
    spawn_viewer(Command::new("open").arg(path))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_path(path: &Path) -> Result<(), BridgeError> {
    spawn_viewer(Command::new("xdg-open").arg(path))
}

#[cfg(target_os = "windows")]
fn open_path(path: &Path) -> Result<(), BridgeError> {
    spawn_viewer(Command::new("cmd").args(["/C", "start", ""]).arg(path))
}

#[cfg(not(any(unix, target_os = "windows")))]
fn open_path(_path: &Path) -> Result<(), BridgeError> {
    Err(artifact_error("system image opening is unsupported"))
}

#[cfg(target_os = "macos")]
pub(crate) fn open_url(url: &str) -> Result<(), BridgeError> {
    spawn_viewer(Command::new("open").arg(url))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn open_url(url: &str) -> Result<(), BridgeError> {
    spawn_viewer(Command::new("xdg-open").arg(url))
}

#[cfg(target_os = "windows")]
pub(crate) fn open_url(url: &str) -> Result<(), BridgeError> {
    spawn_viewer(Command::new("cmd").args(["/C", "start", "", url]))
}

#[cfg(not(any(unix, target_os = "windows")))]
pub(crate) fn open_url(_url: &str) -> Result<(), BridgeError> {
    Err(artifact_error("system URL opening is unsupported"))
}

fn spawn_viewer(command: &mut Command) -> Result<(), BridgeError> {
    command
        .spawn()
        .map(|_| ())
        .map_err(|_| artifact_error("could not start the system viewer"))
}

fn invalid(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::InvalidRequest, message)
}

fn artifact_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Artifact, message)
}

fn output_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_detection_is_explicit_and_predictable() {
        assert_eq!(
            detect_protocol(Some("xterm-kitty"), None, None, false),
            Some(PreviewProtocol::Kitty)
        );
        assert_eq!(
            detect_protocol(None, Some("iTerm.app"), None, false),
            Some(PreviewProtocol::Iterm2)
        );
        assert_eq!(
            detect_protocol(None, Some("WezTerm"), None, false),
            Some(PreviewProtocol::Iterm2)
        );
        assert_eq!(detect_protocol(Some("xterm"), None, None, false), None);
    }
}
