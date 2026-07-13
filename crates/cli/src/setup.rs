use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, IsTerminal as _, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use imagegen_bridge::{
    codex_responses::{CodexAuthFile, CodexCredentialSource as _},
    config::{BridgeConfig, ConfigLoader},
    core::{BridgeError, ErrorCode},
    runtime::{SqliteSessionBindingStore, inspect_sqlite_session_schema},
};
use serde::Serialize;
use tokio::{process::Command, time::timeout};

use crate::{args::SetupArgs, doctor, output::Output};

#[derive(Debug, Clone, Serialize)]
struct SetupChange {
    action: &'static str,
    target: PathBuf,
}

#[derive(Debug, Serialize)]
struct SetupPlan {
    config_path: PathBuf,
    state_root: PathBuf,
    output_root: PathBuf,
    session_database: PathBuf,
    codex: CodexStatus,
    oauth_ready: bool,
    changes: Vec<SetupChange>,
    live_probe: bool,
}

#[derive(Debug, Serialize)]
struct SetupResult {
    complete: bool,
    changed: bool,
    config_path: PathBuf,
    state_root: PathBuf,
    output_root: PathBuf,
    codex: CodexStatus,
    oauth_ready: bool,
    database_schema: u32,
    applied_changes: Vec<SetupChange>,
    live_probe: Option<doctor::LiveProbeResult>,
    next_steps: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexStatus {
    pub(crate) available: bool,
    pub(crate) version: Option<String>,
}

pub(crate) async fn run(
    explicit_config: Option<&Path>,
    args: &SetupArgs,
    output: &Output,
) -> Result<(), BridgeError> {
    let config_path = setup_config_path(explicit_config)?;
    let config_exists = path_is_regular_file(&config_path)?;
    let original = if config_exists {
        Some(
            ConfigLoader::default()
                .resolve(Some(&config_path), &[])?
                .config,
        )
    } else {
        None
    };
    let mut config = original.clone().unwrap_or_default();
    let state_root = absolute_path(args.state_root.as_deref().unwrap_or(&default_state_root()?))?;
    let output_root = absolute_path(
        args.output_root
            .as_deref()
            .unwrap_or(&default_output_root()?),
    )?;

    if original.is_none() || args.state_root.is_some() {
        config.providers.codex_app_server.session_database = state_root.join("sessions.sqlite3");
    }
    if original.is_none() || args.output_root.is_some() {
        config.artifacts.root = output_root.clone();
    }
    config.validate()?;

    let effective_state_root = config
        .providers
        .codex_app_server
        .session_database
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| input("session database requires a parent directory"))?;
    let effective_output_root = config.artifacts.root.clone();
    let config_changed = original.as_ref() != Some(&config);
    let schema =
        inspect_sqlite_session_schema(&config.providers.codex_app_server.session_database).await?;
    let mut changes = Vec::new();
    if config_changed {
        changes.push(SetupChange {
            action: if config_exists {
                "update_config"
            } else {
                "create_config"
            },
            target: config_path.clone(),
        });
    }
    if !effective_state_root.is_dir() {
        changes.push(SetupChange {
            action: "create_private_state_directory",
            target: effective_state_root.clone(),
        });
    }
    if !effective_output_root.is_dir() {
        changes.push(SetupChange {
            action: "create_output_directory",
            target: effective_output_root.clone(),
        });
    }
    if !schema.initialized || schema.version != Some(schema.current_version) {
        changes.push(SetupChange {
            action: "migrate_session_database",
            target: config.providers.codex_app_server.session_database.clone(),
        });
    }

    let codex = probe_codex(&config.providers.codex_app_server.executable).await;
    let oauth_ready = probe_oauth().await.is_ok();
    let plan = SetupPlan {
        config_path: config_path.clone(),
        state_root: effective_state_root.clone(),
        output_root: effective_output_root.clone(),
        session_database: config.providers.codex_app_server.session_database.clone(),
        codex: codex.clone(),
        oauth_ready,
        changes,
        live_probe: args.live_probe,
    };
    if args.dry_run {
        output.value(&plan)?;
        return Ok(());
    }

    if output.is_human() {
        output.value(&plan)?;
    }

    let needs_confirmation = !plan.changes.is_empty();
    let confirmed = if needs_confirmation {
        match confirm(
            "Apply this setup plan?",
            args.yes,
            args.non_interactive || output.is_machine(),
        ) {
            Ok(confirmed) => confirmed,
            Err(error) => {
                if output.is_machine() {
                    output.value(&plan)?;
                }
                return Err(error);
            }
        }
    } else {
        true
    };
    if !confirmed {
        return Err(BridgeError::new(
            ErrorCode::Cancelled,
            "setup was not applied",
        ));
    }

    if config_changed {
        write_config_atomic(&config_path, &config)?;
    }
    create_directory(&effective_state_root, true)?;
    create_directory(&effective_output_root, false)?;
    let store = SqliteSessionBindingStore::open(
        &config.providers.codex_app_server.session_database,
        "codex-app-server",
    )
    .await?;
    store.close().await?;

    let live_probe = if args.live_probe {
        if !confirm(
            "Run one paid Codex OAuth image generation now?",
            args.yes,
            args.non_interactive || output.is_machine(),
        )? {
            return Err(BridgeError::new(
                ErrorCode::Cancelled,
                "live probe was not confirmed",
            ));
        }
        Some(doctor::run_live_probe(config.clone(), None).await?)
    } else {
        None
    };

    let mut next_steps = Vec::new();
    if !codex.available {
        next_steps.push("install the Codex CLI and ensure `codex` is on PATH");
    }
    if !oauth_ready {
        next_steps.push("run `codex login`, then rerun `imagegen-bridge doctor`");
    }
    if codex.available && oauth_ready {
        next_steps.push("run `imagegen-bridge generate \"a red-haired woman\"`");
    }
    output.value(&SetupResult {
        complete: codex.available && oauth_ready,
        changed: config_changed || !plan.changes.is_empty(),
        config_path,
        state_root: effective_state_root,
        output_root: effective_output_root,
        codex: codex.clone(),
        oauth_ready,
        database_schema: schema.current_version,
        applied_changes: plan.changes.clone(),
        live_probe,
        next_steps,
    })?;
    if !codex.available {
        return Err(BridgeError::new(
            ErrorCode::Configuration,
            "Codex CLI was not found; install it and rerun setup",
        ));
    }
    if !oauth_ready {
        return Err(BridgeError::new(
            ErrorCode::Authentication,
            "Codex ChatGPT OAuth is not ready; run `codex login` and rerun setup",
        ));
    }
    Ok(())
}

pub(crate) fn command_config_path(explicit: Option<&Path>) -> Result<Option<PathBuf>, BridgeError> {
    if let Some(path) = explicit {
        return absolute_path(path).map(Some);
    }
    let project = absolute_path(Path::new("imagegen-bridge.toml"))?;
    if project.is_file() {
        return Ok(Some(project));
    }
    let user = default_config_path()?;
    Ok(user.is_file().then_some(user))
}

fn setup_config_path(explicit: Option<&Path>) -> Result<PathBuf, BridgeError> {
    if let Some(path) = explicit {
        return absolute_path(path);
    }
    let project = absolute_path(Path::new("imagegen-bridge.toml"))?;
    if project.is_file() {
        return Ok(project);
    }
    default_config_path()
}

fn default_config_path() -> Result<PathBuf, BridgeError> {
    Ok(xdg_or_home("XDG_CONFIG_HOME", ".config")?
        .join("imagegen-bridge")
        .join("config.toml"))
}

fn default_state_root() -> Result<PathBuf, BridgeError> {
    Ok(xdg_or_home("XDG_STATE_HOME", ".local/state")?.join("imagegen-bridge"))
}

fn default_output_root() -> Result<PathBuf, BridgeError> {
    Ok(xdg_or_home("XDG_DATA_HOME", ".local/share")?
        .join("imagegen-bridge")
        .join("artifacts"))
}

fn xdg_or_home(variable: &str, fallback: &str) -> Result<PathBuf, BridgeError> {
    if let Some(path) = env::var_os(variable).map(PathBuf::from)
        && path.is_absolute()
    {
        return Ok(path);
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| input("could not determine a user configuration directory"))?;
    Ok(home.join(fallback))
}

fn absolute_path(path: &Path) -> Result<PathBuf, BridgeError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir()
            .map(|current| current.join(path))
            .map_err(|_| input("could not resolve an absolute path"))
    }
}

fn path_is_regular_file(path: &Path) -> Result<bool, BridgeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(input("configuration path must be a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(input("could not inspect configuration path")),
    }
}

fn write_config_atomic(path: &Path, config: &BridgeConfig) -> Result<(), BridgeError> {
    let parent = path
        .parent()
        .ok_or_else(|| input("configuration path requires a parent directory"))?;
    create_directory(parent, true)?;
    let rendered =
        toml::to_string_pretty(config).map_err(|_| input("could not encode configuration TOML"))?;
    let temporary = temporary_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| input("could not create temporary configuration file"))?;
    let write_result = file
        .write_all(rendered.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, path));
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(input("could not atomically write configuration file"));
    }
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    name.push(format!(".tmp-{}-{nonce}", std::process::id()));
    path.with_file_name(name)
}

fn create_directory(path: &Path, private: bool) -> Result<(), BridgeError> {
    let existed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => true,
        Ok(_) => return Err(input("setup directory path must not be a file or symlink")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => return Err(input("could not inspect setup directory")),
    };
    fs::create_dir_all(path).map_err(|_| input("could not create setup directory"))?;
    if private && !existed {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|_| input("could not protect setup directory permissions"))?;
        }
    }
    Ok(())
}

pub(crate) async fn probe_codex(executable: &Path) -> CodexStatus {
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let result = timeout(Duration::from_secs(5), command.output()).await;
    let Ok(Ok(result)) = result else {
        return CodexStatus {
            available: false,
            version: None,
        };
    };
    if !result.status.success() {
        return CodexStatus {
            available: false,
            version: None,
        };
    }
    let source = if result.stdout.is_empty() {
        result.stderr
    } else {
        result.stdout
    };
    let version = String::from_utf8_lossy(&source)
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.chars()
                .filter(|value| !value.is_control())
                .take(200)
                .collect()
        });
    CodexStatus {
        available: true,
        version,
    }
}

pub(crate) async fn probe_oauth() -> Result<(), BridgeError> {
    CodexAuthFile::discover()?.load().await.map(|_| ())
}

pub(crate) fn confirm(
    question: &str,
    yes: bool,
    non_interactive: bool,
) -> Result<bool, BridgeError> {
    if yes {
        return Ok(true);
    }
    if non_interactive || !io::stdin().is_terminal() {
        return Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "confirmation is required; rerun with --yes or use --dry-run",
        ));
    }
    eprint!("{question} [y/N] ");
    io::stderr()
        .flush()
        .map_err(|_| input("could not write confirmation prompt"))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|_| input("could not read confirmation response"))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn input(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Input, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn machine_confirmation_requires_yes() {
        let error = confirm("apply?", false, true).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(confirm("apply?", true, true).unwrap());
    }

    #[test]
    fn explicit_config_paths_are_made_absolute() {
        let path = command_config_path(Some(Path::new("custom.toml")))
            .unwrap()
            .unwrap();
        assert!(path.is_absolute());
        assert!(path.ends_with("custom.toml"));
    }

    #[cfg(unix)]
    #[test]
    fn setup_directory_rejects_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = create_directory(&link, true).unwrap_err();
        assert_eq!(error.code, ErrorCode::Input);
    }
}
