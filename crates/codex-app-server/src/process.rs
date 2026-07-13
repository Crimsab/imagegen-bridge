//! Supervised `codex app-server` child process.

use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use imagegen_bridge_core::{BridgeError, ErrorCode};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

use crate::{AppServerRpc, RpcConfig};

/// Child process configuration.
#[derive(Debug, Clone)]
pub struct CodexProcessConfig {
    /// Codex executable path or command name.
    pub executable: PathBuf,
    /// Extra arguments after `app-server`.
    pub args: Vec<String>,
    /// Optional process working directory.
    pub cwd: Option<PathBuf>,
    /// JSONL connection limits.
    pub rpc: RpcConfig,
    /// Grace period before forceful termination.
    pub shutdown_timeout: Duration,
    /// Minimum interval between child starts to bound restart storms.
    pub restart_backoff: Duration,
}

impl Default for CodexProcessConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("codex"),
            args: Vec::new(),
            cwd: None,
            rpc: RpcConfig::default(),
            shutdown_timeout: Duration::from_secs(5),
            restart_backoff: Duration::from_millis(250),
        }
    }
}

/// Initialized RPC client plus owned child lifecycle.
#[derive(Debug)]
pub struct CodexProcess {
    config: CodexProcessConfig,
    running: Mutex<Option<RunningProcess>>,
    shutting_down: AtomicBool,
    generation: AtomicU64,
}

#[derive(Debug)]
struct RunningProcess {
    rpc: Arc<AppServerRpc>,
    child: Child,
    started_at: Instant,
}

impl CodexProcess {
    /// Spawns `codex app-server` using inherited OAuth/config state.
    pub async fn spawn(config: CodexProcessConfig) -> Result<Self, BridgeError> {
        if config.shutdown_timeout.is_zero() {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "codex app-server shutdown timeout must be greater than zero",
            ));
        }
        if config.restart_backoff > Duration::from_secs(30) {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "codex app-server restart backoff must not exceed 30 seconds",
            ));
        }
        let running = spawn_running(&config).await?;
        Ok(Self {
            config,
            running: Mutex::new(Some(running)),
            shutting_down: AtomicBool::new(false),
            generation: AtomicU64::new(1),
        })
    }

    /// Returns a healthy initialized connection, restarting a failed child once.
    pub async fn rpc(&self) -> Result<Arc<AppServerRpc>, BridgeError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(BridgeError::new(
                ErrorCode::Cancelled,
                "codex app-server supervisor is shutting down",
            ));
        }
        let mut running = self.running.lock().await;
        let healthy = match running.as_mut() {
            Some(process) if !process.rpc.is_closed() => match process.child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) | Err(_) => false,
            },
            Some(_) | None => false,
        };
        if healthy {
            return Ok(Arc::clone(
                &running.as_ref().ok_or_else(supervisor_state_error)?.rpc,
            ));
        }
        let previous_started_at = running.as_ref().map(|process| process.started_at);
        if let Some(process) = running.take() {
            stop_running(process, self.config.shutdown_timeout).await?;
        }
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(BridgeError::new(
                ErrorCode::Cancelled,
                "codex app-server supervisor is shutting down",
            ));
        }
        if let Some(started_at) = previous_started_at {
            tokio::time::sleep(
                self.config
                    .restart_backoff
                    .saturating_sub(started_at.elapsed()),
            )
            .await;
        }
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(BridgeError::new(
                ErrorCode::Cancelled,
                "codex app-server supervisor is shutting down",
            ));
        }
        let replacement = spawn_running(&self.config).await?;
        let rpc = Arc::clone(&replacement.rpc);
        *running = Some(replacement);
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(rpc)
    }

    /// Current one-based child generation, useful for safe operational metrics.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Terminates and reaps the owned child process. Repeated calls are safe.
    pub async fn shutdown(&self) -> Result<(), BridgeError> {
        self.shutting_down.store(true, Ordering::Release);
        let Some(process) = self.running.lock().await.take() else {
            return Ok(());
        };
        stop_running(process, self.config.shutdown_timeout).await
    }
}

async fn spawn_running(config: &CodexProcessConfig) -> Result<RunningProcess, BridgeError> {
    let mut command = Command::new(&config.executable);
    command
        .arg("app-server")
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|_| {
        BridgeError::new(ErrorCode::Configuration, "could not spawn codex app-server")
    })?;
    let stdin = child.stdin.take().ok_or_else(|| {
        BridgeError::new(ErrorCode::Internal, "codex app-server stdin is unavailable")
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        BridgeError::new(
            ErrorCode::Internal,
            "codex app-server stdout is unavailable",
        )
    })?;
    let rpc = match AppServerRpc::connect(stdout, stdin, config.rpc).await {
        Ok(rpc) => rpc,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    Ok(RunningProcess {
        rpc,
        child,
        started_at: Instant::now(),
    })
}

async fn stop_running(
    mut process: RunningProcess,
    shutdown_timeout: Duration,
) -> Result<(), BridgeError> {
    match process.child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(_) => {
            return Err(BridgeError::new(
                ErrorCode::Internal,
                "could not inspect codex app-server process",
            ));
        }
    }
    process
        .child
        .start_kill()
        .map_err(|_| BridgeError::new(ErrorCode::Internal, "could not stop codex app-server"))?;
    tokio::time::timeout(shutdown_timeout, process.child.wait())
        .await
        .map_err(|_| BridgeError::new(ErrorCode::Timeout, "codex app-server did not stop"))?
        .map_err(|_| BridgeError::new(ErrorCode::Internal, "could not reap codex app-server"))?;
    Ok(())
}

fn supervisor_state_error() -> BridgeError {
    BridgeError::new(
        ErrorCode::Internal,
        "codex app-server supervisor has no active process",
    )
}
