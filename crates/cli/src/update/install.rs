use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
};

use imagegen_bridge::core::{BridgeError, ErrorCode};
use serde::Serialize;

use super::{
    archive,
    github::{self, Release},
};

#[derive(Debug, Serialize)]
pub(super) struct InstallResult {
    pub current_version: String,
    pub target_version: String,
    pub action: &'static str,
    pub backup: Option<PathBuf>,
    pub dry_run: bool,
}

pub(super) async fn standalone(
    release: &Release,
    dry_run: bool,
    yes: bool,
) -> Result<InstallResult, BridgeError> {
    ensure_confirmation(dry_run, yes)?;
    let target_version = release.version()?.to_string();
    let executable =
        std::env::current_exe().map_err(|_| internal("could not locate the running executable"))?;
    let backup = backup_path(&executable)?;
    if dry_run {
        return Ok(InstallResult {
            current_version: env!("CARGO_PKG_VERSION").into(),
            target_version,
            action: "install",
            backup: Some(backup),
            dry_run: true,
        });
    }
    if is_container() {
        return Err(invalid(
            "standalone self-update is disabled inside containers; run `imagegen-bridge update docker` on the Docker host",
        ));
    }
    let archive_name = asset_name(&target_version)?;
    let archive_asset = release.asset(&archive_name)?;
    let checksum_asset = release.asset("SHA256SUMS")?;
    let (archive_bytes, checksum_bytes) = tokio::try_join!(
        github::download(archive_asset),
        github::download(checksum_asset)
    )?;
    archive::verify_checksum(&archive_name, &archive_bytes, &checksum_bytes)?;
    let binary = archive::extract_binary(&archive_name, &archive_bytes)?;
    replace(&executable, &backup, &binary)?;
    Ok(InstallResult {
        current_version: env!("CARGO_PKG_VERSION").into(),
        target_version,
        action: "installed",
        backup: Some(backup),
        dry_run: false,
    })
}

pub(super) fn rollback(dry_run: bool, yes: bool) -> Result<InstallResult, BridgeError> {
    ensure_confirmation(dry_run, yes)?;
    if is_container() {
        return Err(invalid(
            "standalone rollback is disabled inside containers; roll back the Docker image pin on the host",
        ));
    }
    let executable =
        std::env::current_exe().map_err(|_| internal("could not locate the running executable"))?;
    let backup = backup_path(&executable)?;
    if !backup.is_file() {
        return Err(invalid(
            "no previous standalone binary is available for rollback",
        ));
    }
    if !dry_run {
        let bytes = fs::read(&backup).map_err(|_| internal("previous binary could not be read"))?;
        replace(&executable, &backup, &bytes)?;
    }
    Ok(InstallResult {
        current_version: env!("CARGO_PKG_VERSION").into(),
        target_version: "previous".into(),
        action: if dry_run { "rollback" } else { "rolled_back" },
        backup: Some(backup),
        dry_run,
    })
}

#[derive(Debug, Serialize)]
pub(super) struct DockerResult {
    pub current_version: String,
    pub target_version: String,
    pub image: String,
    pub compose_file: PathBuf,
    pub env_file: PathBuf,
    pub action: &'static str,
    pub dry_run: bool,
}

pub(super) async fn docker(
    release: &Release,
    compose_file: &Path,
    env_file: &Path,
    dry_run: bool,
    yes: bool,
) -> Result<DockerResult, BridgeError> {
    ensure_confirmation(dry_run, yes)?;
    if is_container() {
        return Err(invalid(
            "Docker deployment updates must run on the Docker host, not inside the container",
        ));
    }
    if !compose_file.is_file() {
        return Err(invalid("Compose file is not a readable regular file"));
    }
    let target_version = release.version()?.to_string();
    let image = format!("ghcr.io/crimsab/imagegen-bridge:{target_version}");
    let result = DockerResult {
        current_version: env!("CARGO_PKG_VERSION").into(),
        target_version,
        image: image.clone(),
        compose_file: compose_file.to_path_buf(),
        env_file: env_file.to_path_buf(),
        action: if dry_run {
            "docker_update"
        } else {
            "docker_updated"
        },
        dry_run,
    };
    if dry_run {
        return Ok(result);
    }

    let original = match fs::read(env_file) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(invalid("environment file could not be read safely")),
    };
    let updated = update_env_pin(original.as_deref().unwrap_or_default(), &image)?;
    atomic_write(env_file, &updated)?;
    let deployment = match compose(compose_file, env_file, &["pull", "imagegen-bridge"]).await {
        Ok(()) => {
            compose(
                compose_file,
                env_file,
                &["up", "-d", "--no-deps", "imagegen-bridge"],
            )
            .await
        }
        Err(error) => Err(error),
    };
    if let Err(error) = deployment {
        if let Some(original) = original.as_deref() {
            let _ = atomic_write(env_file, original);
        } else {
            let _ = fs::remove_file(env_file);
        }
        let _ = compose(
            compose_file,
            env_file,
            &["up", "-d", "--no-deps", "imagegen-bridge"],
        )
        .await;
        return Err(BridgeError::new(
            ErrorCode::Upstream,
            format!(
                "Docker update failed and the previous image pin was restored: {}",
                error.message
            ),
        ));
    }
    Ok(result)
}

async fn compose(compose_file: &Path, env_file: &Path, args: &[&str]) -> Result<(), BridgeError> {
    let status = tokio::process::Command::new("docker")
        .args(["compose", "--env-file"])
        .arg(env_file)
        .arg("-f")
        .arg(compose_file)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|_| invalid("Docker Compose is not available on this host"))?;
    if !status.success() {
        return Err(BridgeError::new(
            ErrorCode::Upstream,
            "Docker Compose command failed",
        ));
    }
    Ok(())
}

fn update_env_pin(original: &[u8], image: &str) -> Result<Vec<u8>, BridgeError> {
    let text = std::str::from_utf8(original)
        .map_err(|_| invalid("environment file is not valid UTF-8"))?;
    let mut found = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("IMAGEGEN_BRIDGE_IMAGE=") {
            if !found {
                lines.push(format!("IMAGEGEN_BRIDGE_IMAGE={image}"));
                found = true;
            }
        } else {
            lines.push(line.to_owned());
        }
    }
    if !found {
        lines.push(format!("IMAGEGEN_BRIDGE_IMAGE={image}"));
    }
    let mut output = lines.join("\n").into_bytes();
    output.push(b'\n');
    Ok(output)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), BridgeError> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|_| internal("update directory could not be created"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| internal("temporary update file could not be created"))?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.flush())
        .map_err(|_| internal("temporary update file could not be written"))?;
    temporary
        .persist(path)
        .map_err(|_| internal("update file could not be published atomically"))?;
    Ok(())
}

fn replace(executable: &Path, backup: &Path, binary: &[u8]) -> Result<(), BridgeError> {
    #[cfg(windows)]
    {
        let _ = (executable, backup, binary);
        return Err(invalid(
            "automatic replacement of a running Windows executable is not supported yet; download the verified release archive from GitHub",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let parent = executable
            .parent()
            .ok_or_else(|| internal("running executable has no parent directory"))?;
        let current = fs::read(executable)
            .map_err(|_| internal("running executable could not be backed up"))?;
        atomic_write(backup, &current)?;
        fs::set_permissions(backup, fs::Permissions::from_mode(0o755))
            .map_err(|_| internal("backup executable permissions could not be set"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|_| internal("temporary executable could not be created"))?;
        temporary
            .write_all(binary)
            .and_then(|()| temporary.flush())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|_| internal("temporary executable could not be written"))?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|_| internal("temporary executable permissions could not be set"))?;
        temporary
            .persist(executable)
            .map_err(|_| internal("updated executable could not be published atomically"))?;
        Ok(())
    }
}

fn backup_path(executable: &Path) -> Result<PathBuf, BridgeError> {
    let name = executable
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| internal("running executable name is invalid"))?;
    Ok(executable.with_file_name(format!(".{name}.previous")))
}

fn asset_name(version: &str) -> Result<String, BridgeError> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64.tar.gz",
        ("linux", "aarch64") => "linux-aarch64.tar.gz",
        ("macos", "x86_64") => "macos-x86_64.tar.gz",
        ("macos", "aarch64") => "macos-aarch64.tar.gz",
        ("windows", "x86_64") => "windows-x86_64.zip",
        _ => {
            return Err(BridgeError::new(
                ErrorCode::UnsupportedCapability,
                "no prebuilt release asset exists for this platform",
            ));
        }
    };
    Ok(format!("imagegen-bridge-v{version}-{target}"))
}

fn ensure_confirmation(dry_run: bool, yes: bool) -> Result<(), BridgeError> {
    if !dry_run && !yes {
        return Err(invalid(
            "this command changes the installation; review with --dry-run, then pass --yes",
        ));
    }
    Ok(())
}

fn is_container() -> bool {
    Path::new("/.dockerenv").exists() || std::env::var_os("container").is_some()
}
fn invalid(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::InvalidRequest, message)
}
fn internal(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_pin_preserves_unrelated_values() -> Result<(), Box<dyn std::error::Error>> {
        let output = update_env_pin(b"SECRET=keep\nIMAGEGEN_BRIDGE_IMAGE=old\n", "new")?;
        assert_eq!(
            String::from_utf8(output)?,
            "SECRET=keep\nIMAGEGEN_BRIDGE_IMAGE=new\n"
        );
        Ok(())
    }

    #[test]
    fn mutations_require_confirmation() {
        assert!(ensure_confirmation(false, false).is_err());
        assert!(ensure_confirmation(true, false).is_ok());
        assert!(ensure_confirmation(false, true).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn replacement_retains_one_working_rollback_binary() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("imagegen-bridge");
        let backup = directory.path().join(".imagegen-bridge.previous");
        fs::write(&executable, b"old binary")?;

        replace(&executable, &backup, b"new binary")?;
        assert_eq!(fs::read(&executable)?, b"new binary");
        assert_eq!(fs::read(&backup)?, b"old binary");

        let previous = fs::read(&backup)?;
        replace(&executable, &backup, &previous)?;
        assert_eq!(fs::read(&executable)?, b"old binary");
        assert_eq!(fs::read(&backup)?, b"new binary");
        Ok(())
    }
}
