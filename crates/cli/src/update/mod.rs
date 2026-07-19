mod archive;
mod cache;
mod github;
mod install;

use std::io::IsTerminal as _;

use imagegen_bridge::core::{BridgeError, ErrorCode};
use semver::Version;
use serde::Serialize;

use crate::{args::UpdateCommand, output::Output};

#[derive(Debug, Serialize)]
struct CheckResult {
    current_version: String,
    latest_version: String,
    update_available: bool,
    release_url: String,
}

pub(crate) async fn run(command: &UpdateCommand, output: &Output) -> Result<(), BridgeError> {
    match command {
        UpdateCommand::Check => {
            let release = refresh_cache(false).await?;
            show_check(&release, output)
        }
        UpdateCommand::Install { dry_run, yes } => {
            require_confirmation(*dry_run, *yes)?;
            let release = refresh_cache(false).await?;
            ensure_newer(&release)?;
            let result = install::standalone(&release, *dry_run, *yes).await?;
            output.value(&result)
        }
        UpdateCommand::Docker {
            compose_file,
            env_file,
            dry_run,
            yes,
            active_passive,
            coordination_file,
            slot_file,
            readiness_timeout_secs,
        } => {
            require_confirmation(*dry_run, *yes)?;
            let release = refresh_cache(false).await?;
            ensure_newer(&release)?;
            let result = install::docker(
                &release,
                compose_file,
                env_file,
                *dry_run,
                *yes,
                *active_passive,
                coordination_file,
                slot_file,
                *readiness_timeout_secs,
            )
            .await?;
            output.value(&result)
        }
        UpdateCommand::Rollback { dry_run, yes } => {
            output.value(&install::rollback(*dry_run, *yes)?)
        }
    }
}

pub(crate) async fn notify_if_available(output: &Output) {
    if output.is_quiet()
        || !output.is_human()
        || !std::io::stderr().is_terminal()
        || std::env::var_os("CI").is_some()
        || truthy("IMAGEGEN_BRIDGE_NO_UPDATE_CHECK")
    {
        return;
    }
    let mut cache = cache::Cache::load();
    let release = if cache.fresh() {
        cache.release.clone()
    } else {
        match github::latest(true).await {
            Ok(release) => {
                cache.checked_at = cache::now();
                cache.release = Some(release.clone());
                cache.store();
                Some(release)
            }
            Err(_) => None,
        }
    };
    let Some(release) = release else { return };
    let Ok(version) = release.version() else {
        return;
    };
    if is_newer(&version) && cache.should_notify(&version.to_string()) {
        let _ = output.notice(&format!(
            "Imagegen Bridge {version} is available (current {}). Run `imagegen-bridge update check`.",
            env!("CARGO_PKG_VERSION")
        ));
        cache.notified_version = Some(version.to_string());
        cache.notified_at = Some(cache::now());
        cache.store();
    }
}

async fn refresh_cache(passive: bool) -> Result<github::Release, BridgeError> {
    let release = github::latest(passive).await?;
    let mut cache = cache::Cache::load();
    cache.checked_at = cache::now();
    cache.release = Some(release.clone());
    cache.store();
    Ok(release)
}

fn show_check(release: &github::Release, output: &Output) -> Result<(), BridgeError> {
    let latest = release.version()?;
    let result = CheckResult {
        current_version: env!("CARGO_PKG_VERSION").into(),
        latest_version: latest.to_string(),
        update_available: is_newer(&latest),
        release_url: release.html_url.clone(),
    };
    if output.is_human() {
        if result.update_available {
            output.text(&format!(
                "Imagegen Bridge {} → {} available",
                result.current_version, result.latest_version
            ))?;
            output.text(&format!("Release: {}", result.release_url))?;
            output.text("Preview: imagegen-bridge update install --dry-run")
        } else {
            output.text(&format!(
                "Imagegen Bridge {} is up to date.",
                result.current_version
            ))
        }
    } else {
        output.value(&result)
    }
}

fn ensure_newer(release: &github::Release) -> Result<(), BridgeError> {
    if is_newer(&release.version()?) {
        Ok(())
    } else {
        Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "the installed version is already current",
        ))
    }
}

fn require_confirmation(dry_run: bool, yes: bool) -> Result<(), BridgeError> {
    if dry_run || yes {
        Ok(())
    } else {
        Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "this command changes the installation; review with --dry-run, then pass --yes",
        ))
    }
}

fn is_newer(latest: &Version) -> bool {
    Version::parse(env!("CARGO_PKG_VERSION")).is_ok_and(|current| latest > &current)
}

fn truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_not_newer() -> Result<(), Box<dyn std::error::Error>> {
        assert!(!is_newer(&Version::parse(env!("CARGO_PKG_VERSION"))?));
        Ok(())
    }
}
