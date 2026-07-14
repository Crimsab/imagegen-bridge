use std::{
    io::{self, IsTerminal as _, Write as _},
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use imagegen_bridge::{
    BridgeApplication,
    config::ResolvedConfig,
    core::{BridgeError, ErrorCode},
};
use imagegen_bridge_server::{ServerState, bind, router, serve};
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};
use tokio_util::sync::CancellationToken;

use crate::{args::DashboardArgs, commands::shutdown_signal, output::Output, presentation};

#[derive(Debug, Serialize)]
struct DashboardConnection {
    mode: &'static str,
    url: String,
    api_base_url: String,
    bind: String,
    authentication: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    opened: bool,
}

pub(crate) async fn run(
    args: DashboardArgs,
    mut resolved: ResolvedConfig,
    output: &Output,
) -> Result<(), BridgeError> {
    resolved.config.validate()?;
    let explicit_bind = args.bind.is_some();
    let requested = dashboard_address(args.bind.as_deref(), &resolved.config.server.bind)?;
    if requested.port() != 0 && dashboard_is_live(requested).await {
        let url = format!("http://{requested}/dashboard");
        let opened = open_dashboard(&url, &args, output)?;
        return print_dashboard_connection(
            output,
            &DashboardConnection {
                mode: "attached",
                api_base_url: format!("http://{requested}"),
                bind: requested.to_string(),
                authentication: "unknown",
                pid: None,
                opened,
                url,
            },
        );
    }
    if args.attach_only {
        return Err(invalid(
            "no Imagegen Bridge dashboard is listening at the selected address",
        ));
    }
    if !resolved.config.server.jobs.enabled {
        return Err(invalid(
            "dashboard startup requires server.jobs.enabled=true",
        ));
    }

    let listener = match bind(requested).await {
        Ok(listener) => listener,
        Err(error)
            if !explicit_bind
                && requested.port() != 0
                && error.kind() == io::ErrorKind::AddrInUse =>
        {
            bind(SocketAddr::new(requested.ip(), 0))
                .await
                .map_err(|_| internal("could not select an available dashboard port"))?
        }
        Err(_) => return Err(internal("could not bind the requested dashboard address")),
    };
    let local = listener
        .local_addr()
        .map_err(|_| internal("could not inspect dashboard listener"))?;
    resolved.config.server.bind = local.to_string();
    initialize_server_tracing(resolved.config.server.tracing.enabled);

    let application = BridgeApplication::from_config(resolved.config.clone()).await?;
    let state = ServerState::from_resolved(application.runtime().clone(), &resolved).await?;
    let jobs = state.jobs.clone();
    let shutdown = CancellationToken::new();
    let server_settings = resolved.config.server.clone();
    let server_shutdown = shutdown.clone();
    let mut server_task = tokio::spawn(serve(
        listener,
        router(state, &resolved.config.server),
        server_settings,
        async move { server_shutdown.cancelled().await },
    ));
    tokio::task::yield_now().await;

    let url = format!("http://{local}/dashboard");
    let opened = match open_dashboard(&url, &args, output) {
        Ok(opened) => opened,
        Err(error) => {
            shutdown.cancel();
            let _ = server_task.await;
            if let Some(jobs) = jobs {
                jobs.shutdown().await;
            }
            let _ = application.shutdown().await;
            return Err(error);
        }
    };
    let connection_result = print_dashboard_connection(
        output,
        &DashboardConnection {
            mode: "started",
            api_base_url: format!("http://{local}"),
            bind: local.to_string(),
            authentication: auth_mode(&resolved),
            pid: Some(std::process::id()),
            opened,
            url,
        },
    )
    .and_then(|()| output.status("dashboard server is running; press Ctrl-C to stop"));
    if let Err(error) = connection_result {
        shutdown.cancel();
        let _ = server_task.await;
        if let Some(jobs) = jobs {
            jobs.shutdown().await;
        }
        let _ = application.shutdown().await;
        return Err(error);
    }

    let result = tokio::select! {
        signal = shutdown_signal() => {
            shutdown.cancel();
            let server_result = server_task.await.map_err(|_| internal("dashboard server task failed"))?
                .map_err(|_| internal("dashboard HTTP server failed"));
            signal.map_err(|_| internal("dashboard signal handler failed")).and(server_result)
        }
        server_result = &mut server_task => server_result
            .map_err(|_| internal("dashboard server task failed"))?
            .map_err(|_| internal("dashboard HTTP server failed")),
    };
    if let Some(jobs) = jobs {
        jobs.shutdown().await;
    }
    result.and(application.shutdown().await)
}

fn dashboard_address(
    bind_override: Option<&str>,
    configured: &str,
) -> Result<SocketAddr, BridgeError> {
    let configured = configured
        .parse::<SocketAddr>()
        .map_err(|_| invalid("server bind must be a numeric socket address"))?;
    let selected = bind_override
        .map(str::parse::<SocketAddr>)
        .transpose()
        .map_err(|_| invalid("dashboard bind must be a numeric socket address"))?
        .unwrap_or(configured);
    if bind_override.is_some() && !selected.ip().is_loopback() {
        return Err(invalid("dashboard --bind must use a loopback IP address"));
    }
    if selected.ip().is_loopback() {
        Ok(selected)
    } else {
        Ok(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            selected.port(),
        ))
    }
}

async fn dashboard_is_live(address: SocketAddr) -> bool {
    const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
    let probe = async {
        let mut stream = TcpStream::connect(address).await.ok()?;
        stream
            .write_all(b"GET /dashboard HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut response = Vec::new();
        stream
            .take(MAX_RESPONSE_BYTES)
            .read_to_end(&mut response)
            .await
            .ok()?;
        let response = String::from_utf8_lossy(&response);
        Some(
            response.starts_with("HTTP/1.1 200")
                && response.contains("<title>Imagegen Bridge</title>"),
        )
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), probe)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

fn open_dashboard(url: &str, args: &DashboardArgs, output: &Output) -> Result<bool, BridgeError> {
    let automatic = !args.no_open && output.is_human() && io::stdout().is_terminal();
    if !args.open && !automatic {
        return Ok(false);
    }
    match presentation::open_url(url) {
        Ok(()) => Ok(true),
        Err(error) if args.open => Err(error),
        Err(_) => {
            output.status("system browser could not be opened; use the printed dashboard URL")?;
            Ok(false)
        }
    }
}

fn print_dashboard_connection(
    output: &Output,
    connection: &DashboardConnection,
) -> Result<(), BridgeError> {
    if output.is_json() {
        return output.value(connection);
    }
    let mut stdout = io::stdout().lock();
    let result = (|| -> io::Result<()> {
        if output.is_plain() {
            writeln!(stdout, "mode={}", connection.mode)?;
            writeln!(stdout, "url={}", connection.url)?;
            writeln!(stdout, "api_base_url={}", connection.api_base_url)?;
            writeln!(stdout, "bind={}", connection.bind)?;
            writeln!(stdout, "authentication={}", connection.authentication)?;
            if let Some(pid) = connection.pid {
                writeln!(stdout, "pid={pid}")?;
            }
            writeln!(stdout, "opened={}", connection.opened)?;
        } else {
            writeln!(stdout, "Dashboard")?;
            writeln!(stdout, "  mode            {}", connection.mode)?;
            writeln!(stdout, "  url             {}", connection.url)?;
            writeln!(stdout, "  bind            {}", connection.bind)?;
            writeln!(stdout, "  authentication  {}", connection.authentication)?;
            writeln!(stdout, "  browser opened  {}", connection.opened)?;
        }
        Ok(())
    })();
    result.map_err(|_| internal("could not write dashboard connection details"))
}

fn auth_mode(resolved: &ResolvedConfig) -> &'static str {
    if resolved.config.server.bearer_token_env.is_some() {
        "bearer"
    } else {
        "none"
    }
}

pub(crate) fn initialize_server_tracing(enabled: bool) {
    if !enabled {
        return;
    }
    let installed = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_current_span(true)
        .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
        .try_init()
        .is_ok();
    if installed {
        tracing::info!(
            event = "server_tracing_initialized",
            "server tracing initialized"
        );
    }
}

fn invalid(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::InvalidRequest, message)
}

fn internal(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Internal, message)
}
