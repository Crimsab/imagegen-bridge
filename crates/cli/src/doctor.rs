use std::{
    fs::{self, OpenOptions},
    net::{SocketAddr, TcpListener},
    path::Path,
    time::Instant,
};

use imagegen_bridge::{
    BridgeApplication,
    config::{BridgeConfig, ResolvedConfig},
    core::{BridgeError, ErrorCode, ImageRequest, OutputFormat, ResponseFormat},
    runtime::{
        ExecutionContext, ProviderReadiness, ProviderReadinessStatus, inspect_sqlite_session_schema,
    },
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{args::DoctorArgs, output::Output, setup};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: &'static str,
    status: CheckStatus,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    ok: bool,
    version: &'static str,
    checks: Vec<DoctorCheck>,
    summary: DoctorSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    live_probe: Option<LiveProbeResult>,
}

#[derive(Debug, Serialize)]
struct DoctorSummary {
    passed: usize,
    warnings: usize,
    failed: usize,
    skipped: usize,
}

/// Redaction-safe result of one explicitly requested paid generation probe.
#[derive(Debug, Serialize)]
pub(crate) struct LiveProbeResult {
    provider: String,
    duration_ms: u64,
    images: usize,
    width: u32,
    height: u32,
    format: OutputFormat,
}

pub(crate) async fn run(
    config_path: Option<&Path>,
    resolved: ResolvedConfig,
    args: &DoctorArgs,
    output: &Output,
) -> Result<(), BridgeError> {
    let mut checks = vec![pass(
        "bridge_executable",
        format!("imagegen-bridge {} is running", env!("CARGO_PKG_VERSION")),
    )];
    if let Some(path) = config_path {
        checks.push(pass(
            "configuration_file",
            format!("configuration loaded from {}", path.display()),
        ));
    } else {
        checks.push(fail(
            "configuration_file",
            "no project or user configuration file exists; run `imagegen-bridge setup`",
        ));
    }
    let issues = resolved.config.check();
    if issues.is_empty() {
        checks.push(pass("configuration", "effective configuration is valid"));
    } else {
        checks.push(DoctorCheck {
            name: "configuration",
            status: CheckStatus::Fail,
            message: "effective configuration has validation issues".to_owned(),
            details: Some(json!({"issues": issues})),
        });
    }

    let codex = setup::probe_codex(&resolved.config.providers.codex_app_server.executable).await;
    if codex.available {
        checks.push(DoctorCheck {
            name: "codex_executable",
            status: CheckStatus::Pass,
            message: "Codex CLI is executable".to_owned(),
            details: Some(json!({"version": codex.version})),
        });
    } else {
        checks.push(fail(
            "codex_executable",
            "Codex CLI could not be executed within five seconds",
        ));
    }

    match setup::probe_oauth().await {
        Ok(()) => checks.push(pass(
            "codex_oauth",
            "Codex is logged in with ChatGPT OAuth and the auth file is protected",
        )),
        Err(error) => checks.push(DoctorCheck {
            name: "codex_oauth",
            status: CheckStatus::Fail,
            message: error.message,
            details: None,
        }),
    }

    checks.push(directory_check(
        "artifact_storage",
        &resolved.config.artifacts.root,
    ));
    let state_database = &resolved.config.providers.codex_app_server.session_database;
    let state_root = state_database.parent().unwrap_or_else(|| Path::new("."));
    checks.push(directory_check("state_storage", state_root));
    match inspect_sqlite_session_schema(state_database).await {
        Ok(schema) if schema.initialized && schema.version == Some(schema.current_version) => {
            checks.push(DoctorCheck {
                name: "database_migrations",
                status: CheckStatus::Pass,
                message: "session database schema is current".to_owned(),
                details: Some(json!({"version": schema.version})),
            });
        }
        Ok(schema) => checks.push(DoctorCheck {
            name: "database_migrations",
            status: CheckStatus::Fail,
            message: "session database is absent or requires migration; rerun setup".to_owned(),
            details: Some(json!({
                "initialized": schema.initialized,
                "version": schema.version,
                "required_version": schema.current_version,
            })),
        }),
        Err(error) => checks.push(fail("database_migrations", error.message)),
    }
    checks.push(port_check(&resolved.config.server.bind));

    let prerequisites_ready = config_path.is_some()
        && checks
            .iter()
            .filter(|check| {
                matches!(
                    check.name,
                    "configuration"
                        | "codex_executable"
                        | "codex_oauth"
                        | "artifact_storage"
                        | "state_storage"
                        | "database_migrations"
                )
            })
            .all(|check| check.status == CheckStatus::Pass);
    if prerequisites_ready {
        provider_checks(&resolved.config, args.provider.as_deref(), &mut checks).await;
    } else {
        checks.push(skip(
            "provider_readiness",
            "provider process checks skipped until local prerequisites pass",
        ));
        checks.push(skip(
            "provider_capabilities",
            "capability discovery skipped until local prerequisites pass",
        ));
    }

    let live_probe = if args.live_probe {
        if !setup::confirm(
            "Run one paid Codex OAuth image generation now?",
            args.yes,
            args.non_interactive || output.is_machine(),
        )? {
            return Err(BridgeError::new(
                ErrorCode::Cancelled,
                "live probe was not confirmed",
            ));
        }
        if prerequisites_ready {
            match run_live_probe(resolved.config.clone(), args.provider.as_deref()).await {
                Ok(result) => {
                    checks.push(pass(
                        "live_probe",
                        "paid OAuth generation returned a verified image",
                    ));
                    Some(result)
                }
                Err(error) => {
                    checks.push(DoctorCheck {
                        name: "live_probe",
                        status: CheckStatus::Fail,
                        message: error.message,
                        details: None,
                    });
                    None
                }
            }
        } else {
            checks.push(skip(
                "live_probe",
                "live probe skipped because local prerequisites failed",
            ));
            None
        }
    } else {
        checks.push(skip(
            "live_probe",
            "not requested; no paid generation was performed",
        ));
        None
    };

    let summary = summarize(&checks);
    let report = DoctorReport {
        ok: summary.failed == 0,
        version: env!("CARGO_PKG_VERSION"),
        checks,
        summary,
        live_probe,
    };
    let ok = report.ok;
    output.value(&report)?;
    if ok {
        Ok(())
    } else {
        Err(
            BridgeError::new(ErrorCode::Configuration, "one or more doctor checks failed")
                .with_detail("failed", report.summary.failed),
        )
    }
}

async fn provider_checks(
    config: &BridgeConfig,
    provider: Option<&str>,
    checks: &mut Vec<DoctorCheck>,
) {
    let application = match BridgeApplication::from_config(config.clone()).await {
        Ok(application) => application,
        Err(error) => {
            checks.push(fail("provider_readiness", error.message));
            checks.push(skip(
                "provider_capabilities",
                "capability discovery skipped because provider startup failed",
            ));
            return;
        }
    };
    let registry = application.runtime().registry();
    let readiness = if let Some(name) = provider {
        let selected = match registry.resolve(Some(name)) {
            Ok(selected) => selected,
            Err(error) => {
                checks.push(fail("provider_readiness", error.message));
                checks.push(skip(
                    "provider_capabilities",
                    "capability discovery skipped because provider selection failed",
                ));
                let _ = application.shutdown().await;
                return;
            }
        };
        let status = match selected.check_ready().await {
            Ok(()) => ProviderReadinessStatus::Ready,
            Err(error) => ProviderReadinessStatus::NotReady { error },
        };
        vec![ProviderReadiness {
            provider: name.to_owned(),
            status,
        }]
    } else {
        registry.readiness().await
    };
    let not_ready = readiness
        .iter()
        .filter(|check| matches!(check.status, ProviderReadinessStatus::NotReady { .. }))
        .count();
    if not_ready == 0 && !readiness.is_empty() {
        checks.push(DoctorCheck {
            name: "provider_readiness",
            status: CheckStatus::Pass,
            message: "all selected providers are ready".to_owned(),
            details: Some(json!({
                "providers": readiness.iter().map(|check| &check.provider).collect::<Vec<_>>()
            })),
        });
    } else {
        checks.push(DoctorCheck {
            name: "provider_readiness",
            status: CheckStatus::Fail,
            message: "one or more selected providers are not ready".to_owned(),
            details: Some(json!({"not_ready": not_ready})),
        });
    }
    match registry.capabilities(provider, None).await {
        Ok(capabilities) => checks.push(DoctorCheck {
            name: "provider_capabilities",
            status: CheckStatus::Pass,
            message: "dynamic provider capabilities are available".to_owned(),
            details: Some(json!({
                "provider": capabilities.provider,
                "model": capabilities.model,
            })),
        }),
        Err(error) => checks.push(fail("provider_capabilities", error.message)),
    }
    if let Err(error) = application.shutdown().await {
        checks.push(DoctorCheck {
            name: "provider_shutdown",
            status: CheckStatus::Warn,
            message: error.message,
            details: None,
        });
    }
}

pub(crate) async fn run_live_probe(
    config: BridgeConfig,
    provider: Option<&str>,
) -> Result<LiveProbeResult, BridgeError> {
    let application = BridgeApplication::from_config(config).await?;
    let mut request = ImageRequest::generate(
        "A single cobalt-blue circle centered on a plain warm-white background",
    );
    request.routing.provider = provider.map(str::to_owned);
    request.output.response_format = ResponseFormat::Metadata;
    let started = Instant::now();
    let result = application
        .runtime()
        .execute_with(request, ExecutionContext::default())
        .await;
    let shutdown = application.shutdown().await;
    let response = result?;
    shutdown?;
    let first = response
        .data
        .first()
        .ok_or_else(|| BridgeError::new(ErrorCode::Protocol, "live probe returned no image"))?;
    Ok(LiveProbeResult {
        provider: response.provider,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        images: response.data.len(),
        width: first.width,
        height: first.height,
        format: first.format,
    })
}

fn directory_check(name: &'static str, path: &Path) -> DoctorCheck {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return fail(name, "configured directory does not exist; rerun setup");
    };
    if !metadata.file_type().is_dir() {
        return fail(name, "configured path is not a directory");
    }
    let probe = path.join(format!(".imagegen-bridge-doctor-{}", std::process::id()));
    let writable = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|file| file.sync_all())
        .and_then(|()| fs::remove_file(&probe));
    if writable.is_ok() {
        pass(name, format!("{} is writable", path.display()))
    } else {
        let _ = fs::remove_file(probe);
        fail(name, "configured directory is not safely writable")
    }
}

fn port_check(value: &str) -> DoctorCheck {
    let Ok(address) = value.parse::<SocketAddr>() else {
        return fail("server_port", "configured server bind address is invalid");
    };
    match TcpListener::bind(address) {
        Ok(listener) => {
            let local = listener.local_addr().ok();
            drop(listener);
            DoctorCheck {
                name: "server_port",
                status: CheckStatus::Pass,
                message: "configured loopback listener is available".to_owned(),
                details: Some(json!({"address": local})),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => DoctorCheck {
            name: "server_port",
            status: CheckStatus::Warn,
            message: "configured server port is already in use; a bridge may already be running"
                .to_owned(),
            details: Some(json!({"address": address})),
        },
        Err(_) => fail("server_port", "configured server address cannot be bound"),
    }
}

fn summarize(checks: &[DoctorCheck]) -> DoctorSummary {
    DoctorSummary {
        passed: checks
            .iter()
            .filter(|check| check.status == CheckStatus::Pass)
            .count(),
        warnings: checks
            .iter()
            .filter(|check| check.status == CheckStatus::Warn)
            .count(),
        failed: checks
            .iter()
            .filter(|check| check.status == CheckStatus::Fail)
            .count(),
        skipped: checks
            .iter()
            .filter(|check| check.status == CheckStatus::Skip)
            .count(),
    }
}

fn pass(name: &'static str, message: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name,
        status: CheckStatus::Pass,
        message: message.into(),
        details: None,
    }
}

fn fail(name: &'static str, message: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name,
        status: CheckStatus::Fail,
        message: message.into(),
        details: None,
    }
}

fn skip(name: &'static str, message: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name,
        status: CheckStatus::Skip,
        message: message.into(),
        details: None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn summary_counts_every_status() {
        let checks = vec![
            pass("a", "pass"),
            DoctorCheck {
                name: "b",
                status: CheckStatus::Warn,
                message: "warn".to_owned(),
                details: None,
            },
            fail("c", "fail"),
            skip("d", "skip"),
        ];
        let summary = summarize(&checks);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.warnings, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1);
    }

    #[test]
    fn directory_probe_is_reversible() {
        let directory = tempfile::tempdir().unwrap();
        let check = directory_check("storage", directory.path());
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
