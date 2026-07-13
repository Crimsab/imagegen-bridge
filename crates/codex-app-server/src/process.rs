//! Supervised `codex app-server` child process.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

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
}

impl Default for CodexProcessConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("codex"),
            args: Vec::new(),
            cwd: None,
            rpc: RpcConfig::default(),
            shutdown_timeout: Duration::from_secs(5),
        }
    }
}

/// Initialized RPC client plus owned child lifecycle.
#[derive(Debug)]
pub struct CodexProcess {
    rpc: Arc<AppServerRpc>,
    child: Mutex<Option<Child>>,
    shutdown_timeout: Duration,
}

impl CodexProcess {
    /// Spawns `codex app-server` using inherited OAuth/config state.
    pub async fn spawn(config: CodexProcessConfig) -> Result<Self, BridgeError> {
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
        Ok(Self {
            rpc,
            child: Mutex::new(Some(child)),
            shutdown_timeout: config.shutdown_timeout,
        })
    }

    /// Returns the initialized RPC connection.
    #[must_use]
    pub fn rpc(&self) -> Arc<AppServerRpc> {
        Arc::clone(&self.rpc)
    }

    /// Terminates and reaps the owned child process.
    pub async fn shutdown(&self) -> Result<(), BridgeError> {
        let Some(mut child) = self.child.lock().await.take() else {
            return Ok(());
        };
        child.start_kill().map_err(|_| {
            BridgeError::new(ErrorCode::Internal, "could not stop codex app-server")
        })?;
        tokio::time::timeout(self.shutdown_timeout, child.wait())
            .await
            .map_err(|_| BridgeError::new(ErrorCode::Timeout, "codex app-server did not stop"))?
            .map_err(|_| {
                BridgeError::new(ErrorCode::Internal, "could not reap codex app-server")
            })?;
        Ok(())
    }
}
