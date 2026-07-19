use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

#[cfg(test)]
static TEST_DOCKER_PROGRAM: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

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
    pub strategy: &'static str,
    pub active_slot: Option<String>,
    pub dry_run: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn docker(
    release: &Release,
    compose_file: &Path,
    env_file: &Path,
    dry_run: bool,
    yes: bool,
    active_passive: bool,
    coordination_file: &Path,
    slot_file: &Path,
    readiness_timeout_secs: u64,
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
    if active_passive && readiness_timeout_secs == 0 {
        return Err(invalid("readiness timeout must be greater than zero"));
    }
    let current_slot = if active_passive {
        read_slot(slot_file)?
    } else {
        "blue"
    };
    let target_slot = if current_slot == "blue" {
        "green"
    } else {
        "blue"
    };
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
        strategy: if active_passive {
            "active_passive"
        } else {
            "in_place"
        },
        active_slot: active_passive.then(|| target_slot.to_owned()),
        dry_run,
    };
    if dry_run {
        return Ok(result);
    }

    if active_passive {
        docker_handoff(
            compose_file,
            env_file,
            coordination_file,
            slot_file,
            current_slot,
            target_slot,
            &image,
            Duration::from_secs(readiness_timeout_secs),
        )
        .await?;
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

#[allow(clippy::too_many_arguments)]
async fn docker_handoff(
    compose_file: &Path,
    env_file: &Path,
    coordination_file: &Path,
    slot_file: &Path,
    current_slot: &str,
    target_slot: &str,
    image: &str,
    readiness_timeout: Duration,
) -> Result<(), BridgeError> {
    let original = match fs::read(env_file) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(invalid("environment file could not be read safely")),
    };
    let variable = format!("IMAGEGEN_BRIDGE_{}_IMAGE", target_slot.to_ascii_uppercase());
    let updated = update_env_named_pin(original.as_deref().unwrap_or_default(), &variable, image)?;
    atomic_write(env_file, &updated)?;
    if let Err(error) = compose(compose_file, env_file, &["pull", target_slot]).await {
        restore_file(env_file, original.as_deref());
        return Err(error);
    }

    if let Err(error) = atomic_write(coordination_file, b"hold\n") {
        restore_file(env_file, original.as_deref());
        return Err(error);
    }
    let cutover = async {
        compose(compose_file, env_file, &["stop", current_slot]).await?;
        compose(
            compose_file,
            env_file,
            &["up", "-d", "--no-deps", target_slot],
        )
        .await?;
        wait_healthy(compose_file, env_file, target_slot, readiness_timeout).await
    }
    .await;
    if let Err(error) = cutover {
        let rollback = rollback_handoff(
            compose_file,
            env_file,
            coordination_file,
            slot_file,
            current_slot,
            target_slot,
            original.as_deref(),
            readiness_timeout,
        )
        .await;
        return Err(BridgeError::new(
            ErrorCode::Upstream,
            format!(
                "active/passive handoff failed; previous slot rollback {}: {}",
                if rollback.is_ok() {
                    "succeeded"
                } else {
                    "failed"
                },
                error.message
            ),
        ));
    }
    if let Err(error) = atomic_write(slot_file, format!("{target_slot}\n").as_bytes())
        .and_then(|()| atomic_write(coordination_file, format!("{target_slot}\n").as_bytes()))
    {
        let rollback = rollback_handoff(
            compose_file,
            env_file,
            coordination_file,
            slot_file,
            current_slot,
            target_slot,
            original.as_deref(),
            readiness_timeout,
        )
        .await;
        return Err(BridgeError::new(
            ErrorCode::Upstream,
            format!(
                "handoff state commit failed; previous slot rollback {}: {}",
                if rollback.is_ok() {
                    "succeeded"
                } else {
                    "failed"
                },
                error.message
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn rollback_handoff(
    compose_file: &Path,
    env_file: &Path,
    coordination_file: &Path,
    slot_file: &Path,
    current_slot: &str,
    target_slot: &str,
    original_env: Option<&[u8]>,
    readiness_timeout: Duration,
) -> Result<(), BridgeError> {
    let _ = compose(compose_file, env_file, &["stop", target_slot]).await;
    restore_file(env_file, original_env);
    compose(
        compose_file,
        env_file,
        &["up", "-d", "--no-deps", current_slot],
    )
    .await?;
    wait_healthy(compose_file, env_file, current_slot, readiness_timeout).await?;
    atomic_write(slot_file, format!("{current_slot}\n").as_bytes())?;
    atomic_write(coordination_file, format!("{current_slot}\n").as_bytes())
}

async fn wait_healthy(
    compose_file: &Path,
    env_file: &Path,
    service: &str,
    timeout: Duration,
) -> Result<(), BridgeError> {
    let deadline = Instant::now() + timeout;
    loop {
        let output = tokio::process::Command::new(docker_program())
            .args(["compose", "--env-file"])
            .arg(env_file)
            .arg("-f")
            .arg(compose_file)
            .args(["ps", "--format", "json", service])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|_| invalid("Docker Compose is not available on this host"))?;
        if output.status.success() && compose_reports_healthy(&output.stdout) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BridgeError::new(
                ErrorCode::Timeout,
                "new deployment slot did not become provider-ready before the deadline",
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn compose_reports_healthy(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    if serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| match value {
        serde_json::Value::Array(items) => items.iter().any(healthy_value),
        value => healthy_value(&value),
    }) {
        return true;
    }
    text.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| healthy_value(&value))
    })
}

fn healthy_value(value: &serde_json::Value) -> bool {
    value.get("Health").and_then(serde_json::Value::as_str) == Some("healthy")
}

fn read_slot(path: &Path) -> Result<&'static str, BridgeError> {
    let value = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("blue"),
        Err(_) => return Err(invalid("active slot file could not be read safely")),
    };
    match value.trim() {
        "blue" => Ok("blue"),
        "green" => Ok("green"),
        _ => Err(invalid("active slot file must contain blue or green")),
    }
}

fn restore_file(path: &Path, original: Option<&[u8]>) {
    if let Some(original) = original {
        let _ = atomic_write(path, original);
    } else {
        let _ = fs::remove_file(path);
    }
}

async fn compose(compose_file: &Path, env_file: &Path, args: &[&str]) -> Result<(), BridgeError> {
    let status = tokio::process::Command::new(docker_program())
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

fn docker_program() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_DOCKER_PROGRAM
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return path;
    }
    PathBuf::from("docker")
}

fn update_env_pin(original: &[u8], image: &str) -> Result<Vec<u8>, BridgeError> {
    update_env_named_pin(original, "IMAGEGEN_BRIDGE_IMAGE", image)
}

fn update_env_named_pin(
    original: &[u8],
    variable: &str,
    image: &str,
) -> Result<Vec<u8>, BridgeError> {
    let text = std::str::from_utf8(original)
        .map_err(|_| invalid("environment file is not valid UTF-8"))?;
    let mut found = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line
            .trim_start()
            .strip_prefix(variable)
            .is_some_and(|rest| rest.starts_with('='))
        {
            if !found {
                lines.push(format!("{variable}={image}"));
                found = true;
            }
        } else {
            lines.push(line.to_owned());
        }
    }
    if !found {
        lines.push(format!("{variable}={image}"));
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
    fn handoff_pin_and_compose_health_are_strict_and_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        let output = update_env_named_pin(
            b"SECRET=keep\nIMAGEGEN_BRIDGE_BLUE_IMAGE=old\n",
            "IMAGEGEN_BRIDGE_GREEN_IMAGE",
            "new",
        )?;
        assert_eq!(
            String::from_utf8(output)?,
            "SECRET=keep\nIMAGEGEN_BRIDGE_BLUE_IMAGE=old\nIMAGEGEN_BRIDGE_GREEN_IMAGE=new\n"
        );
        assert!(compose_reports_healthy(br#"[{"Health":"healthy"}]"#));
        assert!(!compose_reports_healthy(br#"[{"Health":"starting"}]"#));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn handoff_executes_a_complete_fake_docker_cutover()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let fake = directory.path().join("docker");
        let log = directory.path().join("docker.log");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in *' ps --format json '*) printf '%s\\n' '[{{\"Health\":\"healthy\"}}]' ;; esac\n",
                log.display()
            ),
        )?;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755))?;
        *TEST_DOCKER_PROGRAM
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fake);

        let compose_file = directory.path().join("compose.yaml");
        let env_file = directory.path().join(".env");
        let coordination = directory.path().join("active-slot");
        let slot = directory.path().join("slot");
        fs::write(&compose_file, b"services: {}\n")?;
        fs::write(&env_file, b"IMAGEGEN_BRIDGE_BLUE_IMAGE=old\n")?;
        fs::write(&coordination, b"blue\n")?;
        fs::write(&slot, b"blue\n")?;
        let original = fs::read(&env_file)?;
        docker_handoff(
            &compose_file,
            &env_file,
            &coordination,
            &slot,
            "blue",
            "green",
            "image:new",
            Duration::from_secs(1),
        )
        .await?;
        *TEST_DOCKER_PROGRAM
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        assert_eq!(fs::read_to_string(&coordination)?, "green\n");
        assert_eq!(fs::read_to_string(&slot)?, "green\n");
        assert!(fs::read_to_string(&env_file)?.contains("IMAGEGEN_BRIDGE_GREEN_IMAGE=image:new"));
        let calls = fs::read_to_string(log)?;
        assert!(calls.contains("pull green"));
        assert!(calls.contains("stop blue"));
        assert!(calls.contains("up -d --no-deps green"));
        assert_ne!(fs::read(&env_file)?, original);
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
