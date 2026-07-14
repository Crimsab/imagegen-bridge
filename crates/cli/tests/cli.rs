#![allow(clippy::expect_used, clippy::panic, missing_docs)]

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use serde_json::Value;

#[test]
fn help_and_version_are_available_without_configuration() {
    cargo_bin_cmd!("imagegen-bridge")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "A bounded, provider-neutral image generation bridge",
        ))
        .stdout(predicate::str::contains("--local-artifact-paths"));
    cargo_bin_cmd!("imagegen-bridge")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("imagegen-bridge "));
    cargo_bin_cmd!("imagegen-bridge")
        .args(["dashboard", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--attach-only"))
        .stdout(predicate::str::contains("--no-open"));
}

#[test]
fn dashboard_rejects_public_listener_and_missing_attach_target() {
    cargo_bin_cmd!("imagegen-bridge")
        .args(["dashboard", "--bind", "0.0.0.0:8080", "--no-open"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "dashboard --bind must use a loopback IP address",
        ));

    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral listener");
    let address = probe.local_addr().expect("listener address");
    drop(probe);
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "dashboard",
            "--bind",
            &address.to_string(),
            "--attach-only",
            "--no-open",
            "--json",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "no Imagegen Bridge dashboard is listening",
        ));
}

#[test]
fn invalid_arguments_use_clap_exit_code_two() {
    cargo_bin_cmd!("imagegen-bridge")
        .args(["generate", "--quality", "impossible"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unsupported value `impossible`"));
}

#[test]
fn local_artifact_paths_are_explicit_json_only_generation_output() {
    cargo_bin_cmd!("imagegen-bridge")
        .args(["--local-artifact-paths", "generate", "test", "--dry-run"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--json"));
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--local-artifact-paths",
            "--json",
            "generate",
            "test",
            "--dry-run",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "requires a non-dry-run generate or edit command",
        ));
    cargo_bin_cmd!("imagegen-bridge")
        .args(["--local-artifact-paths", "--json", "config", "show"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "requires a non-dry-run generate or edit command",
        ));
}

#[test]
fn shipped_agent_skill_is_well_formed_and_uses_the_local_path_contract() {
    let skill = include_str!("../../../skills/generate-images-with-bridge/SKILL.md");
    assert!(skill.starts_with("---\nname: generate-images-with-bridge\n"));
    assert!(skill.contains("<objective>"));
    assert!(skill.contains("<quick_start>"));
    assert!(skill.contains("<success_criteria>"));
    assert!(skill.contains("providers capabilities --json"));
    assert!(skill.contains("--local-artifact-paths"));
    assert!(skill.contains("artifacts[].path"));
    let body = skill.splitn(3, "---").nth(2).expect("skill body");
    assert!(
        body.lines().all(|line| !line.trim_start().starts_with('#')),
        "skill body must use XML tags instead of Markdown headings"
    );
}

#[test]
fn config_check_is_non_mutating_and_machine_readable() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let output = cargo_bin_cmd!("imagegen-bridge")
        .current_dir(directory.path())
        .args(["config", "check", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("JSON output");
    assert_eq!(value["valid"], true);
    assert_eq!(value["issues"], serde_json::json!([]));
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("read directory")
            .count(),
        0
    );
}

#[test]
fn artifact_repair_audits_before_conservative_mutation() {
    use base64::Engine as _;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path().join("artifacts");
    let config = directory.path().join("bridge.toml");
    std::fs::write(&config, format!("[artifacts]\nroot = {root:?}\n")).expect("write config");
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "artifacts",
            "repair",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "artifact repair requires --force or --dry-run",
        ));
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "artifacts",
            "repair",
            "--force",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
    let store = imagegen_bridge::artifacts::ArtifactStore::new(
        &root,
        imagegen_bridge::artifacts::ImageLimits::default(),
    )
    .expect("artifact store");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .expect("PNG fixture");
    let artifact = store
        .publish(&bytes, Some("repair"), None)
        .expect("publish artifact");
    std::fs::remove_file(&artifact.path).expect("remove artifact");

    let output = cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "artifacts",
            "repair",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let audit: Value = serde_json::from_slice(&output).expect("audit JSON");
    assert_eq!(audit["orphaned_records"], 1);
    assert_eq!(audit["repaired"], 0);
    assert_eq!(
        store
            .repair_orphans(10, imagegen_bridge::artifacts::ArtifactRepairMode::Audit)
            .expect("repeat audit")
            .orphaned_records,
        1
    );

    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "artifacts",
            "repair",
            "--force",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"repaired\":1"));
    assert_eq!(
        store
            .repair_orphans(10, imagegen_bridge::artifacts::ArtifactRepairMode::Audit)
            .expect("post-repair audit")
            .scanned,
        0
    );
}

#[test]
fn checked_in_container_profile_remains_valid() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    cargo_bin_cmd!("imagegen-bridge")
        .current_dir(&root)
        .args([
            "--config",
            "deploy/imagegen-bridge.container.toml",
            "config",
            "check",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"valid\":true"));
}

#[test]
fn generation_dry_run_prints_complete_normalized_request() {
    let output = cargo_bin_cmd!("imagegen-bridge")
        .args([
            "generate",
            "--prompt",
            "red fox",
            "--size",
            "1024x1024",
            "--quality",
            "high",
            "--format",
            "webp",
            "--response-format",
            "artifact",
            "--session-key",
            "test-session",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("JSON output");
    assert_eq!(value["operation"], "generate");
    assert_eq!(value["prompt"], "red fox");
    assert_eq!(value["parameters"]["size"], "1024x1024");
    assert_eq!(value["parameters"]["quality"], "high");
    assert_eq!(value["parameters"]["output_format"], "webp");
    assert_eq!(value["output"]["response_format"], "artifact");
    assert_eq!(value["session"]["mode"], "persistent");
}

#[test]
fn generation_accepts_a_natural_positional_prompt() {
    let output = cargo_bin_cmd!("imagegen-bridge")
        .args(["generate", "a red-haired woman", "--dry-run", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("JSON output");
    assert_eq!(value["prompt"], "a red-haired woman");
}

#[test]
fn generation_maps_output_paths_below_the_artifact_root() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let output = cargo_bin_cmd!("imagegen-bridge")
        .current_dir(directory.path())
        .args([
            "generate",
            "red-haired woman",
            "--output",
            "portraits/woman.png",
            "--collision",
            "suffix",
            "--metadata",
            "sidecar",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("JSON output");
    assert_eq!(value["output"]["response_format"], "artifact");
    assert_eq!(value["output"]["directory"], "portraits");
    assert_eq!(value["output"]["filename"], "woman.png");
    assert_eq!(value["output"]["collision"], "suffix");
    assert_eq!(value["output"]["metadata"], "sidecar");
}

#[test]
fn generation_rejects_output_paths_outside_the_artifact_root() {
    let directory = tempfile::tempdir().expect("temporary directory");
    cargo_bin_cmd!("imagegen-bridge")
        .current_dir(directory.path())
        .args(["generate", "test", "--output", "/outside.png", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "output path must remain below the configured artifact root",
        ));
}

#[test]
fn explicit_incompatible_response_format_is_not_silently_overridden() {
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "generate",
            "test",
            "--output",
            "image.png",
            "--response-format",
            "b64_json",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("request validation failed"));
}

#[test]
fn metadata_sidecar_selects_artifact_delivery_by_default() {
    cargo_bin_cmd!("imagegen-bridge")
        .args(["generate", "test", "--metadata", "sidecar", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"response_format\": \"artifact\"",
        ));
}

#[test]
fn embedded_metadata_preserves_image_bearing_default_delivery() {
    let output = cargo_bin_cmd!("imagegen-bridge")
        .args([
            "generate",
            "test",
            "--metadata",
            "embedded",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("JSON output");
    assert_eq!(value["output"]["response_format"], "b64_json");
    assert_eq!(value["output"]["metadata"], "embedded");
}

#[test]
fn combined_metadata_selects_artifact_delivery_by_default() {
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "generate",
            "test",
            "--metadata",
            "sidecar_and_embedded",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"response_format\": \"artifact\"",
        ));
}

#[test]
fn preview_selects_artifact_delivery_for_natural_cli_requests() {
    cargo_bin_cmd!("imagegen-bridge")
        .args(["generate", "paper fox", "--preview", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"response_format\": \"artifact\"",
        ));
}

#[test]
fn preview_refuses_to_corrupt_machine_output() {
    cargo_bin_cmd!("imagegen-bridge")
        .args(["generate", "paper fox", "--preview", "--dry-run", "--json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--preview cannot be combined with machine output",
        ));
}

#[test]
fn request_file_is_lossless_and_exclusive() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let request = directory.path().join("request.json");
    std::fs::write(
        &request,
        r#"{"prompt":"from file","operation":"generate","parameters":{"n":1,"size":"auto","quality":"auto","output_format":"png","background":"auto","moderation":"auto","partial_images":0}}"#,
    )
    .expect("write request");
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "generate",
            "--request",
            request.to_str().expect("UTF-8 path"),
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"prompt\":\"from file\""));
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "generate",
            "--request",
            request.to_str().expect("UTF-8 path"),
            "--prompt",
            "conflict",
            "--dry-run",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used"));
}

#[test]
fn preset_cli_supports_crud_and_generate_overrides() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("jobs.sqlite3");
    let template = directory.path().join("preset.json");
    std::fs::write(
        &template,
        r#"{"prompt":"Stored preset prompt","operation":"generate","parameters":{"n":3,"quality":"low"},"output":{"response_format":"artifact","directory":"preset-output"}}"#,
    )
    .expect("write preset template");
    let database_override = format!("server.jobs.database={database:?}");

    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--set",
            &database_override,
            "--json",
            "preset",
            "create",
            "portrait-low",
            "--from",
            template.to_str().expect("UTF-8 path"),
            "--description",
            "Reusable portrait settings",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"name\":\"portrait-low\""));

    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--set",
            &database_override,
            "--json",
            "generate",
            "--preset",
            "portrait-low",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"prompt\":\"Stored preset prompt\"",
        ));

    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--set",
            &database_override,
            "--json",
            "generate",
            "A red-haired subject",
            "--preset",
            "portrait-low",
            "--quality",
            "high",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"n\":3"))
        .stdout(predicate::str::contains("\"quality\":\"high\""))
        .stdout(predicate::str::contains("\"directory\":\"preset-output\""));

    cargo_bin_cmd!("imagegen-bridge")
        .args(["--set", &database_override, "--json", "preset", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("portrait-low"));
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--set",
            &database_override,
            "preset",
            "delete",
            "portrait-low",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"dry_run\":true"));
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--set",
            &database_override,
            "preset",
            "delete",
            "portrait-low",
            "--force",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"deleted\":true"));
}

#[test]
fn stdin_is_bounded_by_effective_prompt_limit() {
    cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--set",
            "runtime.request.max_prompt_bytes=3",
            "generate",
            "--prompt",
            "-",
            "--dry-run",
        ])
        .write_stdin("four")
        .assert()
        .code(4)
        .stderr(predicate::str::contains(
            "exceeds the configured byte limit",
        ));
}

#[test]
fn config_output_never_resolves_referenced_secret_values() {
    cargo_bin_cmd!("imagegen-bridge")
        .env("BRIDGE_TOKEN", "value-that-must-not-appear")
        .args([
            "--set",
            "server.bearer_token_env=BRIDGE_TOKEN",
            "config",
            "show",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("BRIDGE_TOKEN"))
        .stdout(predicate::str::contains("value-that-must-not-appear").not());
}

#[test]
fn completions_and_manual_are_generated() {
    cargo_bin_cmd!("imagegen-bridge")
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("imagegen-bridge"));
    cargo_bin_cmd!("imagegen-bridge")
        .args(["man"])
        .assert()
        .success()
        .stdout(predicate::str::contains(".TH imagegen-bridge"));
}

#[test]
fn generated_schemas_match_checked_in_contracts() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    cargo_bin_cmd!("imagegen-bridge")
        .current_dir(&root)
        .args([
            "schema",
            "--kind",
            "json-schema",
            "--check",
            "schemas/imagegen-bridge-v1.schema.json",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"current\":true"));
    cargo_bin_cmd!("imagegen-bridge")
        .current_dir(root)
        .args([
            "schema",
            "--kind",
            "openapi",
            "--check",
            "schemas/imagegen-bridge-v1.openapi.json",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"current\":true"));
}

#[test]
fn setup_dry_run_is_machine_readable_and_does_not_mutate() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = directory.path().join("config/bridge.toml");
    let state = directory.path().join("state");
    let artifacts = directory.path().join("artifacts");
    let output = cargo_bin_cmd!("imagegen-bridge")
        .env("CODEX_HOME", directory.path().join("missing-codex-home"))
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "setup",
            "--state-root",
            state.to_str().expect("UTF-8 state"),
            "--output-root",
            artifacts.to_str().expect("UTF-8 artifacts"),
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let plan: Value = serde_json::from_slice(&output).expect("setup plan JSON");
    assert_eq!(plan["oauth_ready"], false);
    assert_eq!(plan["changes"].as_array().expect("changes").len(), 5);
    assert!(!config.exists());
    assert!(!state.exists());
    assert!(!artifacts.exists());
}

#[test]
fn setup_non_interactive_refuses_unconfirmed_changes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = directory.path().join("config.toml");
    cargo_bin_cmd!("imagegen-bridge")
        .env("CODEX_HOME", directory.path().join("missing-codex-home"))
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "setup",
            "--non-interactive",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"changes\""))
        .stderr(predicate::str::contains("confirmation is required"));
    assert!(!config.exists());
}

#[cfg(unix)]
#[test]
fn setup_repairs_once_then_doctor_passes_without_a_paid_probe() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("temporary directory");
    let codex = directory.path().join("codex");
    std::fs::write(
        &codex,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' 'codex-cli test'
  exit 0
fi
while IFS= read -r LINE; do
  case "$LINE" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}' ;;
  esac
done
"#,
    )
    .expect("write fake codex");
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700))
        .expect("codex permissions");
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&codex_home).expect("Codex home");
    let auth = codex_home.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"test-token","account_id":"test-account"}}"#,
    )
    .expect("write auth fixture");
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600))
        .expect("auth permissions");

    let config = directory.path().join("config/bridge.toml");
    let state = directory.path().join("state");
    let artifacts = directory.path().join("artifacts");
    let path = format!(
        "{}:{}",
        directory.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let arguments = [
        "--config",
        config.to_str().expect("UTF-8 config"),
        "setup",
        "--state-root",
        state.to_str().expect("UTF-8 state"),
        "--output-root",
        artifacts.to_str().expect("UTF-8 artifacts"),
        "--yes",
        "--non-interactive",
        "--json",
    ];
    let first = cargo_bin_cmd!("imagegen-bridge")
        .env("CODEX_HOME", &codex_home)
        .env("PATH", &path)
        .args(arguments)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first: Value = serde_json::from_slice(&first).expect("first setup result");
    assert_eq!(first["complete"], true);
    assert_eq!(first["changed"], true);
    assert_eq!(
        first["applied_changes"].as_array().expect("changes").len(),
        5
    );

    let second = cargo_bin_cmd!("imagegen-bridge")
        .env("CODEX_HOME", &codex_home)
        .env("PATH", &path)
        .args(arguments)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let second: Value = serde_json::from_slice(&second).expect("second setup result");
    assert_eq!(second["changed"], false);
    assert_eq!(second["applied_changes"], serde_json::json!([]));

    let doctor = cargo_bin_cmd!("imagegen-bridge")
        .env("CODEX_HOME", &codex_home)
        .env("PATH", &path)
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "doctor",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doctor: Value = serde_json::from_slice(&doctor).expect("doctor result");
    assert_eq!(doctor["ok"], true);
    assert_eq!(doctor["summary"]["failed"], 0);
    assert!(
        doctor["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .any(|check| { check["name"] == "live_probe" && check["status"] == "skip" })
    );
}

#[cfg(unix)]
#[test]
fn daemon_starts_serves_health_and_stops_on_sigint() {
    use std::{
        io::{Read as _, Write as _},
        os::unix::fs::PermissionsExt as _,
        process::Stdio,
        time::Duration,
    };

    let directory = tempfile::tempdir().expect("temporary directory");
    let script = directory.path().join("fake-codex");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r LINE; do
  case "$LINE" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
  esac
done
"#,
    )
    .expect("write fake codex");
    let mut permissions = std::fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).expect("script permissions");

    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral listener");
    let address = probe.local_addr().expect("listener address");
    drop(probe);
    let config = directory.path().join("bridge.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[inputs]
local_roots = ["{root}"]

[artifacts]
root = "{root}/artifacts"

[providers.codex_app_server]
executable = "{script}"
cwd = "{root}"
session_database = "{root}/state.sqlite3"
restart_backoff_ms = 0

[server]
bind = "{address}"

[server.jobs]
database = "{root}/jobs.sqlite3"
"#,
            root = directory.path().display(),
            script = script.display(),
        ),
    )
    .expect("write config");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_imagegen-bridge"))
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "serve",
            "--quiet",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start daemon");

    let mut response = None;
    for _ in 0..100 {
        if let Ok(mut stream) = std::net::TcpStream::connect(address) {
            let written = stream
                .write_all(
                    b"GET /health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .is_ok();
            let mut body = String::new();
            if written
                && stream.read_to_string(&mut body).is_ok()
                && body.contains("200 OK")
                && body.contains("\"status\":\"live\"")
            {
                response = Some(body);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        response
            .as_deref()
            .is_some_and(|body| body.contains("200 OK") && body.contains("\"status\":\"live\"")),
        "daemon never served liveness"
    );

    let mut dashboard_stream = std::net::TcpStream::connect(address).expect("dashboard connection");
    dashboard_stream
        .write_all(b"GET /dashboard HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write dashboard request");
    let mut dashboard = String::new();
    dashboard_stream
        .read_to_string(&mut dashboard)
        .expect("read dashboard response");
    assert!(dashboard.contains("200 OK"));
    assert!(dashboard.contains("<title>Imagegen Bridge</title>"));

    let attached = cargo_bin_cmd!("imagegen-bridge")
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "dashboard",
            "--attach-only",
            "--no-open",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let attached: Value = serde_json::from_slice(&attached).expect("dashboard connection JSON");
    assert_eq!(attached["mode"], "attached");
    assert_eq!(attached["url"], format!("http://{address}/dashboard"));
    assert_eq!(attached["api_base_url"], format!("http://{address}"));
    assert_eq!(attached["bind"], address.to_string());
    assert_eq!(attached["authentication"], "unknown");
    assert_eq!(attached["opened"], false);
    assert!(attached.get("pid").is_none());

    let request_body =
        r#"{"prompt":"trace-secret prompt","operation":"generate","parameters":{"n":0}}"#;
    let mut stream = std::net::TcpStream::connect(address).expect("generation connection");
    write!(
        stream,
        "POST /v1/images HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        request_body.len(),
        request_body
    )
    .expect("write generation request");
    let mut generation = String::new();
    stream
        .read_to_string(&mut generation)
        .expect("read generation response");
    assert!(generation.contains("422 Unprocessable Entity"));

    std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    for _ in 0..100 {
        if let Some(status) = child.try_wait().expect("poll daemon") {
            assert!(status.success(), "daemon exited unsuccessfully: {status}");
            let mut diagnostics = String::new();
            child
                .stderr
                .take()
                .expect("daemon stderr")
                .read_to_string(&mut diagnostics)
                .expect("read daemon diagnostics");
            assert!(
                diagnostics.contains("server tracing initialized"),
                "missing tracing initialization event: {diagnostics}"
            );
            assert!(
                diagnostics.contains("image operation failed"),
                "missing trace event: {diagnostics}"
            );
            assert!(diagnostics.contains("\"provider\":\"codex-app-server\""));
            assert!(diagnostics.contains("\"request_id\":"));
            assert!(diagnostics.contains("\"error_code\":\"InvalidRequest\""));
            assert!(!diagnostics.contains("trace-secret prompt"));
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    child.kill().expect("kill stuck daemon");
    panic!("daemon did not stop after SIGINT");
}

#[cfg(unix)]
#[test]
fn dashboard_launcher_starts_serves_and_stops_on_sigint() {
    use std::{
        io::{BufRead as _, Read as _, Write as _},
        os::unix::fs::PermissionsExt as _,
        process::Stdio,
        sync::mpsc,
        time::Duration,
    };

    let directory = tempfile::tempdir().expect("temporary directory");
    let script = directory.path().join("fake-codex");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r LINE; do
  case "$LINE" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
  esac
done
"#,
    )
    .expect("write fake codex");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("script permissions");

    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("occupied listener");
    let configured_address = occupied.local_addr().expect("listener address");
    let config = directory.path().join("bridge.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[inputs]
local_roots = ["{root}"]

[artifacts]
root = "{root}/artifacts"

[providers.codex_app_server]
executable = "{script}"
cwd = "{root}"
session_database = "{root}/state.sqlite3"
restart_backoff_ms = 0

[server]
bind = "{configured_address}"

[server.jobs]
database = "{root}/jobs.sqlite3"
"#,
            root = directory.path().display(),
            script = script.display(),
        ),
    )
    .expect("write config");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_imagegen-bridge"))
        .args([
            "--config",
            config.to_str().expect("UTF-8 config"),
            "dashboard",
            "--no-open",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start dashboard launcher");

    let stdout = child.stdout.take().expect("dashboard stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .map(|_| line);
        let _ = sender.send(result);
    });
    let connection_line = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("dashboard connection output timed out")
        .expect("read dashboard connection output");
    let connection: Value =
        serde_json::from_str(connection_line.trim()).expect("dashboard connection JSON");
    let address: std::net::SocketAddr = connection["bind"]
        .as_str()
        .expect("dashboard bind")
        .parse()
        .expect("dashboard socket address");
    assert_ne!(address, configured_address);
    drop(occupied);

    let mut page = None;
    for _ in 0..100 {
        if let Ok(mut stream) = std::net::TcpStream::connect(address) {
            let written = stream
                .write_all(
                    b"GET /dashboard HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .is_ok();
            let mut body = String::new();
            if written
                && stream.read_to_string(&mut body).is_ok()
                && body.contains("200 OK")
                && body.contains("<title>Imagegen Bridge</title>")
            {
                page = Some(body);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(page.is_some(), "dashboard launcher never served its UI");

    std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    for _ in 0..100 {
        if let Some(status) = child.try_wait().expect("poll dashboard launcher") {
            assert!(
                status.success(),
                "dashboard exited unsuccessfully: {status}"
            );
            assert_eq!(connection["mode"], "started");
            assert_eq!(connection["url"], format!("http://{address}/dashboard"));
            assert_eq!(connection["bind"], address.to_string());
            assert_eq!(connection["authentication"], "none");
            assert_eq!(connection["opened"], false);
            assert_eq!(connection["pid"], child.id());
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    child.kill().expect("kill stuck dashboard launcher");
    panic!("dashboard launcher did not stop after SIGINT");
}
