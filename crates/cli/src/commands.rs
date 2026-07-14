use std::{
    ffi::OsStr,
    io::{self, Write as _},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::SystemTime,
};

use clap::CommandFactory as _;
use imagegen_bridge::{
    BridgeApplication,
    config::{ConfigLoader, ConfigOverride, ConfigSource, ResolvedConfig},
    core::{
        BridgeError, ErrorCode, ImageInput, ImageOperation, ImageRequest, ImageSource, SessionMode,
        validate_request,
    },
    runtime::{ExecutionContext, ProviderReadinessStatus},
};
use imagegen_bridge_server::{ServerState, bind, router, serve};
use serde_json::json;
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;

use crate::{
    args::{
        ArtifactCommand, Cli, Command, ConfigCommand, EditArgs, GenerateArgs, ImageArgs,
        PresentationArgs, ProviderCommand, SchemaArgs, SchemaKind, SessionCommand,
    },
    dashboard, doctor,
    output::Output,
    presentation, setup,
};

pub(crate) async fn run(cli: Cli, output: &Output) -> Result<(), BridgeError> {
    match &cli.command {
        Command::Completions(args) => {
            completions(args.shell);
            return Ok(());
        }
        Command::Man(args) => return man(&args.output),
        Command::Schema(args) => return schema(args, output),
        _ => {}
    }
    if let Command::Setup(args) = &cli.command {
        if !cli.set.is_empty() || !cli.unset.is_empty() {
            return Err(invalid(
                "setup does not accept --set/--unset; use --state-root, --output-root, or edit the generated config",
            ));
        }
        return setup::run(cli.config.as_deref(), args, output).await;
    }
    let (resolved, config_path) = resolve_config(&cli)?;
    match cli.command {
        Command::Doctor(args) => doctor::run(config_path.as_deref(), resolved, &args, output).await,
        Command::Generate(args) => Box::pin(generate(args, &resolved, output)).await,
        Command::Edit(args) => Box::pin(edit(args, &resolved, output)).await,
        Command::Serve(args) => serve_command(args.bind, resolved, output).await,
        Command::Dashboard(args) => dashboard::run(args, resolved, output).await,
        Command::Providers(args) => providers(args.command, resolved, output).await,
        Command::Session(args) => session(args.command, resolved, output).await,
        Command::Config(args) => config(args.command, &resolved, output),
        Command::Artifacts(args) => artifacts(args.command, &resolved, output),
        Command::AuthDoctor(args) => auth_doctor(args.provider, resolved, output).await,
        Command::Setup(_) | Command::Completions(_) | Command::Man(_) | Command::Schema(_) => {
            unreachable!()
        }
    }
}

fn resolve_config(cli: &Cli) -> Result<(ResolvedConfig, Option<PathBuf>), BridgeError> {
    let file = setup::command_config_path(cli.config.as_deref())?;
    let mut overrides = Vec::with_capacity(cli.set.len() + cli.unset.len());
    for operation in &cli.set {
        let (key, value) = operation.split_once('=').ok_or_else(|| {
            invalid("--set must use KEY=VALUE syntax with a dotted configuration key")
        })?;
        if key.is_empty() {
            return Err(invalid("--set configuration key must not be empty"));
        }
        overrides.push(ConfigOverride::set(key, value));
    }
    overrides.extend(cli.unset.iter().cloned().map(ConfigOverride::unset));
    ConfigLoader::default()
        .resolve(file.as_deref(), &overrides)
        .map(|resolved| (resolved, file))
}

async fn generate(
    args: GenerateArgs,
    resolved: &ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    let presentation = args.presentation;
    let allow_implicit_artifact = args.request.is_none() && args.image.response_format.is_none();
    let mut request = if let Some(path) = args.request.as_deref() {
        ensure_request_only(
            args.prompt.is_some() || args.prompt_text.is_some() || !args.references.is_empty(),
            &args.image,
        )?;
        read_request(path, resolved.config.server.max_body_bytes).await?
    } else {
        let prompt = read_prompt(
            args.prompt
                .as_deref()
                .or(args.prompt_text.as_deref())
                .ok_or_else(|| invalid("prompt is required"))?,
            resolved.config.runtime.request.max_prompt_bytes,
        )
        .await?;
        let mut request = ImageRequest::generate(prompt);
        request.operation = ImageOperation::Generate {
            reference_images: file_inputs(args.references),
        };
        apply_image_args(&mut request, args.image, &resolved.config.artifacts.root)?;
        request
    };
    presentation::prepare_request(&mut request, presentation, allow_implicit_artifact, output)?;
    execute_or_preview(request, args.dry_run, presentation, resolved, output).await
}

async fn edit(
    args: EditArgs,
    resolved: &ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    let presentation = args.presentation;
    let allow_implicit_artifact = args.request.is_none() && args.image.response_format.is_none();
    let mut request = if let Some(path) = args.request.as_deref() {
        ensure_request_only(
            args.prompt.is_some()
                || args.prompt_text.is_some()
                || !args.images.is_empty()
                || args.mask.is_some()
                || !args.references.is_empty(),
            &args.image,
        )?;
        read_request(path, resolved.config.server.max_body_bytes).await?
    } else {
        let prompt = read_prompt(
            args.prompt
                .as_deref()
                .or(args.prompt_text.as_deref())
                .ok_or_else(|| invalid("prompt is required"))?,
            resolved.config.runtime.request.max_prompt_bytes,
        )
        .await?;
        let mut request = ImageRequest::generate(prompt);
        request.operation = ImageOperation::Edit {
            images: file_inputs(args.images),
            mask: args.mask.map(file_input).map(Box::new),
            reference_images: file_inputs(args.references),
        };
        apply_image_args(&mut request, args.image, &resolved.config.artifacts.root)?;
        request
    };
    presentation::prepare_request(&mut request, presentation, allow_implicit_artifact, output)?;
    execute_or_preview(request, args.dry_run, presentation, resolved, output).await
}

fn ensure_request_only(has_operation_fields: bool, image: &ImageArgs) -> Result<(), BridgeError> {
    if has_operation_fields || !image.is_empty() {
        Err(invalid(
            "--request cannot be combined with prompt, image, reference, or parameter flags",
        ))
    } else {
        Ok(())
    }
}

async fn execute_or_preview(
    request: ImageRequest,
    dry_run: bool,
    presentation_options: PresentationArgs,
    resolved: &ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    let runtime_config = resolved.config.runtime_config()?;
    validate_request(&request, runtime_config.request_limits)?;
    if dry_run {
        return output.value(&request);
    }

    let application = BridgeApplication::from_config(resolved.config.clone()).await?;
    let cancellation = CancellationToken::new();
    let context = ExecutionContext {
        cancellation: cancellation.clone(),
        idempotency_scope: "cli".to_owned(),
        ..ExecutionContext::default()
    };
    let operation = application.runtime().execute_with(request, context);
    tokio::pin!(operation);
    let result = tokio::select! {
        result = &mut operation => result,
        signal = shutdown_signal() => {
            signal.map_err(|_| internal("could not listen for termination signal"))?;
            cancellation.cancel();
            operation.await
        }
    };
    let shutdown = application.shutdown().await;
    match (result, shutdown) {
        (Ok(response), Ok(())) => {
            output.response(&response)?;
            presentation::present(
                &response,
                presentation_options,
                &resolved.config.artifacts.root,
                resolved.config.artifacts.image.max_encoded_bytes,
                output,
            )
        }
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn apply_image_args(
    request: &mut ImageRequest,
    args: ImageArgs,
    artifact_root: &Path,
) -> Result<(), BridgeError> {
    request.negative_prompt = args.negative_prompt;
    request.routing.provider = args.provider;
    request.routing.model = args.model;
    if let Some(value) = args.count {
        request.parameters.n = value;
    }
    if let Some(value) = args.size {
        request.parameters.size = value;
    }
    request.parameters.aspect_ratio = args.aspect_ratio;
    request.parameters.resolution = args.resolution;
    if let Some(value) = args.quality {
        request.parameters.quality = value;
    }
    if let Some(value) = args.format {
        request.parameters.output_format = value;
    }
    request.parameters.output_compression = args.compression;
    if let Some(value) = args.background {
        request.parameters.background = value;
    }
    if let Some(value) = args.moderation {
        request.parameters.moderation = value;
    }
    if let Some(value) = args.partial_images {
        request.parameters.partial_images = value;
    }
    if let Some(value) = args.failure_policy {
        request.parameters.failure_policy = value;
    }
    request.parameters.input_fidelity = args.input_fidelity;
    if let Some(value) = args.action {
        request.parameters.action = value;
    }
    let explicit_response_format = args.response_format;
    if let Some(value) = explicit_response_format {
        request.output.response_format = value;
    }
    request.output.filename_prefix = args.filename_prefix;
    apply_output_location(
        request,
        artifact_root,
        args.output_path.as_deref(),
        args.output_dir.as_deref(),
        explicit_response_format.is_none(),
    )?;
    if let Some(collision) = args.collision {
        request.output.collision = collision;
    }
    if let Some(metadata) = args.metadata {
        request.output.metadata = metadata;
        if metadata == imagegen_bridge::core::ArtifactMetadataPolicy::Sidecar
            && explicit_response_format.is_none()
            && request.output.response_format == imagegen_bridge::core::ResponseFormat::B64Json
        {
            request.output.response_format = imagegen_bridge::core::ResponseFormat::Artifact;
        }
    }
    if let Some(value) = args.compatibility {
        request.policies.compatibility = value;
    }
    if let Some(value) = args.negative_prompt_mode {
        request.policies.negative_prompt = value;
    }
    if let Some(value) = args.revised_prompt {
        request.policies.revised_prompt = value;
    }
    request.session.mode = args.session.unwrap_or_else(|| {
        if args.thread_id.is_some() {
            SessionMode::Thread
        } else if args.session_key.is_some() {
            SessionMode::Persistent
        } else {
            SessionMode::Isolated
        }
    });
    request.session.key = args.session_key;
    request.session.thread_id = args.thread_id;
    request.idempotency_key = args.idempotency_key;
    request.timeout_ms = args.timeout_ms;
    request.user = args.user;
    Ok(())
}

fn apply_output_location(
    request: &mut ImageRequest,
    artifact_root: &Path,
    output_path: Option<&Path>,
    output_dir: Option<&Path>,
    allow_implicit_artifact: bool,
) -> Result<(), BridgeError> {
    let Some(requested) = output_path.or(output_dir) else {
        return Ok(());
    };
    let root = lexical_absolute(artifact_root)?;
    let requested = if requested.is_absolute() {
        lexical_absolute(requested)?
    } else {
        lexical_absolute(&root.join(requested))?
    };
    let relative = requested
        .strip_prefix(&root)
        .map_err(|_| invalid("output path must remain below the configured artifact root"))?;
    if output_path.is_some() {
        let filename = relative
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| invalid("output path requires a portable UTF-8 filename"))?;
        request.output.filename = Some(filename.to_owned());
        request.output.directory = portable_parent(relative.parent())?;
    } else {
        request.output.directory = portable_path(relative)?;
    }
    if allow_implicit_artifact
        && request.output.response_format == imagegen_bridge::core::ResponseFormat::B64Json
    {
        request.output.response_format = imagegen_bridge::core::ResponseFormat::Artifact;
    }
    Ok(())
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, BridgeError> {
    let combined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let current =
            std::env::current_dir().map_err(|_| invalid("could not resolve output path"))?;
        current.join(path)
    };
    let mut output = PathBuf::new();
    for component in combined.components() {
        match component {
            std::path::Component::Normal(value) => output.push(value),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !output.pop() {
                    return Err(invalid("output path escapes its configured root"));
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                output.push(component.as_os_str());
            }
        }
    }
    Ok(output)
}

fn portable_parent(path: Option<&Path>) -> Result<Option<String>, BridgeError> {
    match path {
        Some(path) if !path.as_os_str().is_empty() => portable_relative(path).map(Some),
        Some(_) | None => Ok(None),
    }
}

fn portable_path(path: &Path) -> Result<Option<String>, BridgeError> {
    if path.as_os_str().is_empty() {
        Ok(None)
    } else {
        portable_relative(path).map(Some)
    }
}

fn portable_relative(path: &Path) -> Result<String, BridgeError> {
    let components = path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("output path must be portable UTF-8")),
            _ => Err(invalid("output path must be relative and portable")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if components.is_empty() {
        return Err(invalid("output directory is empty"));
    }
    Ok(components.join("/"))
}

fn file_inputs(paths: Vec<PathBuf>) -> Vec<ImageInput> {
    paths.into_iter().map(file_input).collect()
}

fn file_input(path: PathBuf) -> ImageInput {
    ImageInput {
        source: ImageSource::File { path },
        media_type: None,
        filename: None,
    }
}

async fn read_request(path: &Path, maximum: u64) -> Result<ImageRequest, BridgeError> {
    let bytes = read_bounded(path, maximum).await?;
    serde_json::from_slice(&bytes).map_err(|_| invalid("request file is not valid request JSON"))
}

async fn read_prompt(value: &str, maximum: usize) -> Result<String, BridgeError> {
    if value != "-" {
        return Ok(value.to_owned());
    }
    let bytes = read_stdin_bounded(u64::try_from(maximum).unwrap_or(u64::MAX)).await?;
    String::from_utf8(bytes).map_err(|_| invalid("stdin prompt is not valid UTF-8"))
}

async fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, BridgeError> {
    if path.as_os_str() == OsStr::new("-") {
        return read_stdin_bounded(maximum).await;
    }
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| input("could not inspect input file"))?;
    if !metadata.file_type().is_file() || metadata.len() > maximum {
        return Err(input("input must be a bounded regular file"));
    }
    tokio::fs::read(path)
        .await
        .map_err(|_| input("could not read input file"))
}

async fn read_stdin_bounded(maximum: u64) -> Result<Vec<u8>, BridgeError> {
    let limit = maximum.saturating_add(1);
    let mut bytes = Vec::new();
    tokio::io::stdin()
        .take(limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| input("could not read stdin"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum {
        return Err(input("stdin exceeds the configured byte limit"));
    }
    Ok(bytes)
}

async fn serve_command(
    bind_override: Option<String>,
    mut resolved: ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    if let Some(address) = bind_override {
        resolved.config.server.bind = address;
    }
    resolved.config.validate()?;
    dashboard::initialize_server_tracing(resolved.config.server.tracing.enabled);
    let address: SocketAddr = resolved
        .config
        .server
        .bind
        .parse()
        .map_err(|_| invalid("server bind must be a numeric socket address"))?;
    let application = BridgeApplication::from_config(resolved.config.clone()).await?;
    let listener = bind(address)
        .await
        .map_err(|_| internal("could not bind HTTP listener"))?;
    let local = listener
        .local_addr()
        .map_err(|_| internal("could not inspect HTTP listener"))?;
    let state = ServerState::from_resolved(application.runtime().clone(), &resolved).await?;
    let jobs = state.jobs.clone();
    output.status(&format!("listening on http://{local}"))?;
    if resolved.config.server.jobs.enabled {
        output.status(&format!("dashboard at http://{local}/dashboard"))?;
    }
    let result = serve(
        listener,
        router(state, &resolved.config.server),
        resolved.config.server.clone(),
        async {
            let _ = shutdown_signal().await;
        },
    )
    .await
    .map_err(|_| internal("HTTP server failed"));
    if let Some(jobs) = jobs {
        jobs.shutdown().await;
    }
    let shutdown = application.shutdown().await;
    result.and(shutdown)
}

#[cfg(unix)]
pub(crate) async fn shutdown_signal() -> io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        signal = terminate.recv() => signal.ok_or_else(|| {
            io::Error::other("SIGTERM listener closed before receiving a signal")
        }),
    }
}

#[cfg(not(unix))]
pub(crate) async fn shutdown_signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}

async fn providers(
    command: ProviderCommand,
    resolved: ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    let application = BridgeApplication::from_config(resolved.config).await?;
    let result = match command {
        ProviderCommand::List => output.value(&application.runtime().registry().descriptors()),
        ProviderCommand::Capabilities { provider, model } => {
            let capabilities = application
                .runtime()
                .registry()
                .capabilities(provider.as_deref(), model.as_deref())
                .await?;
            output.value(&capabilities)
        }
        ProviderCommand::Readiness => {
            let readiness = application.runtime().registry().readiness().await;
            output.value(&readiness)
        }
    };
    let shutdown = application.shutdown().await;
    result.and(shutdown)
}

async fn auth_doctor(
    provider: Option<String>,
    resolved: ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    resolved.config.validate()?;
    let application = BridgeApplication::from_config(resolved.config).await?;
    if let Some(name) = provider.as_deref() {
        application.runtime().registry().resolve(Some(name))?;
    }
    let mut readiness = application.runtime().registry().readiness().await;
    if let Some(name) = provider.as_deref() {
        readiness.retain(|check| check.provider == name);
    }
    let failure = readiness.iter().find_map(|check| match &check.status {
        ProviderReadinessStatus::Ready => None,
        ProviderReadinessStatus::NotReady { error } => Some(error.clone()),
    });
    let result = output.value(&json!({
        "authenticated": failure.is_none(),
        "providers": readiness,
    }));
    let shutdown = application.shutdown().await;
    result.and(shutdown)?;
    failure.map_or(Ok(()), Err)
}

async fn session(
    command: SessionCommand,
    resolved: ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    resolved.config.validate()?;
    if let SessionCommand::Delete {
        key,
        provider,
        dry_run: true,
        ..
    } = &command
    {
        return output.value(&json!({
            "action": "delete_session",
            "dry_run": true,
            "key": key,
            "provider": provider.as_deref().unwrap_or(&resolved.config.default_provider),
        }));
    }
    if let SessionCommand::Delete { force: false, .. } = &command {
        return Err(invalid("session deletion requires --force or --dry-run"));
    }
    let application = BridgeApplication::from_config(resolved.config).await?;
    let result = match command {
        SessionCommand::Get { key, provider } => {
            let session = application
                .runtime()
                .registry()
                .resolve(provider.as_deref())?
                .get_session(&key)
                .await?;
            output.value(&session)
        }
        SessionCommand::Delete {
            key,
            provider,
            dry_run: false,
            ..
        } => {
            application
                .runtime()
                .registry()
                .resolve(provider.as_deref())?
                .delete_session(&key)
                .await?;
            output.value(&json!({"deleted": true, "key": key}))
        }
        SessionCommand::Delete { dry_run: true, .. } => unreachable!(),
    };
    let shutdown = application.shutdown().await;
    result.and(shutdown)
}

fn config(
    command: ConfigCommand,
    resolved: &ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    match command {
        ConfigCommand::Check => {
            let issues = resolved.config.check();
            output.value(&json!({"valid": issues.is_empty(), "issues": issues}))?;
            if issues.is_empty() {
                Ok(())
            } else {
                Err(
                    BridgeError::new(ErrorCode::Configuration, "configuration validation failed")
                        .with_detail("issues", issues),
                )
            }
        }
        ConfigCommand::Show => output.value(&resolved.config),
        ConfigCommand::Origins => {
            let origins: Vec<_> = resolved
                .provenance()
                .iter()
                .map(|(field, origin)| {
                    json!({
                        "field": field,
                        "source": source_name(origin.source),
                        "key": origin.key,
                    })
                })
                .collect();
            output.value(&origins)
        }
    }
}

fn source_name(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::Default => "default",
        ConfigSource::File => "file",
        ConfigSource::Environment => "environment",
        ConfigSource::Override => "override",
    }
}

fn artifacts(
    command: ArtifactCommand,
    resolved: &ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    match command {
        ArtifactCommand::Cleanup { dry_run: true, .. } => {
            let policy = resolved.config.retention_policy()?;
            output.value(&json!({
                "action": "cleanup_artifacts",
                "dry_run": true,
                "root": resolved.config.artifacts.root,
                "max_age_seconds": policy.max_age.as_secs(),
                "max_artifacts": policy.max_artifacts,
                "max_scan_entries": policy.max_scan_entries,
            }))
        }
        ArtifactCommand::Cleanup { force: false, .. } => {
            Err(invalid("artifact cleanup requires --force or --dry-run"))
        }
        ArtifactCommand::Cleanup { force: true, .. } => {
            let store = resolved.config.artifact_store()?;
            let report = store.cleanup(resolved.config.retention_policy()?, SystemTime::now())?;
            output.value(&json!({
                "scanned": report.scanned,
                "deleted": report.deleted,
                "skipped": report.skipped,
                "scan_limit_reached": report.scan_limit_reached,
            }))
        }
        ArtifactCommand::Repair {
            dry_run: false,
            force: false,
        } => Err(invalid("artifact repair requires --force or --dry-run")),
        ArtifactCommand::Repair { dry_run, force } => {
            let store = resolved.config.artifact_store()?;
            let max_scan_entries = resolved.config.retention_policy()?.max_scan_entries;
            let mode = if force {
                imagegen_bridge::artifacts::ArtifactRepairMode::Apply
            } else {
                imagegen_bridge::artifacts::ArtifactRepairMode::Audit
            };
            let report = store.repair_orphans(max_scan_entries, mode)?;
            output.value(&json!({
                "action": "repair_artifacts",
                "dry_run": dry_run,
                "scanned": report.scanned,
                "healthy": report.healthy,
                "orphaned_records": report.orphaned_records,
                "missing_sidecars": report.missing_sidecars,
                "repaired": report.repaired,
                "skipped": report.skipped,
                "scan_limit_reached": report.scan_limit_reached,
            }))
        }
    }
}

fn completions(shell: clap_complete::Shell) {
    let mut command = Cli::command();
    let name = command.get_name().to_owned();
    clap_complete::generate(shell, &mut command, name, &mut io::stdout());
}

fn schema(args: &SchemaArgs, output: &Output) -> Result<(), BridgeError> {
    const MAX_SCHEMA_BYTES: u64 = 16 * 1024 * 1024;
    let mut rendered = match args.kind {
        SchemaKind::JsonSchema => {
            serde_json::to_vec_pretty(&imagegen_bridge::core::contract_schema())
        }
        SchemaKind::Openapi => {
            serde_json::to_vec_pretty(&imagegen_bridge_server::openapi_document())
        }
    }
    .map_err(|_| internal("could not encode schema document"))?;
    rendered.push(b'\n');
    if let Some(path) = args.check.as_deref() {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| input("could not inspect schema check file"))?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_SCHEMA_BYTES {
            return Err(input("schema check input must be a bounded regular file"));
        }
        let current = std::fs::read(path).map_err(|_| input("could not read schema check file"))?;
        if current != rendered {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "schema check file is stale",
            ));
        }
        return output.value(&json!({
            "current": true,
            "kind": match args.kind { SchemaKind::JsonSchema => "json_schema", SchemaKind::Openapi => "openapi" },
            "path": path,
        }));
    }
    if args.output.as_os_str() == OsStr::new("-") {
        io::stdout()
            .write_all(&rendered)
            .map_err(|_| internal("could not write schema output"))
    } else {
        std::fs::write(&args.output, rendered)
            .map_err(|_| input("could not write schema output file"))
    }
}

fn man(path: &Path) -> Result<(), BridgeError> {
    let manual = clap_mangen::Man::new(Cli::command());
    if path.as_os_str() == OsStr::new("-") {
        manual
            .render(&mut io::stdout())
            .map_err(|_| internal("could not render manual page"))
    } else {
        let mut file = std::fs::File::create(path)
            .map_err(|_| input("could not create manual page output"))?;
        manual
            .render(&mut file)
            .and_then(|()| file.flush())
            .map_err(|_| input("could not write manual page output"))
    }
}

fn invalid(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::InvalidRequest, message)
}

fn input(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Input, message)
}

fn internal(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Internal, message)
}
