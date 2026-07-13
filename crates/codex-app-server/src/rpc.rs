//! Bounded JSONL request/response correlation for Codex app-server.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::StreamExt as _;
use imagegen_bridge_core::{BridgeError, ErrorCode};
use parking_lot::Mutex as ParkingMutex;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _},
    sync::{Mutex, broadcast, oneshot, watch},
};
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    sync::CancellationToken,
};

type PendingSender = oneshot::Sender<Result<Value, BridgeError>>;

/// Bounded connection settings.
#[derive(Debug, Clone, Copy)]
pub struct RpcConfig {
    /// Maximum incoming or outgoing JSONL message bytes.
    pub max_message_bytes: usize,
    /// Default request timeout.
    pub request_timeout: Duration,
    /// Notification ring capacity.
    pub notification_capacity: usize,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: 64 * 1024 * 1024,
            request_timeout: Duration::from_secs(60),
            notification_capacity: 32,
        }
    }
}

/// One app-server notification with its method and parsed params.
#[derive(Debug, Clone)]
pub struct RpcNotification {
    /// Notification method.
    pub method: String,
    /// Parsed params, or null when absent.
    pub params: Value,
}

/// Initialized app-server connection.
pub struct AppServerRpc {
    writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: Arc<ParkingMutex<HashMap<u64, PendingSender>>>,
    notifications: broadcast::Sender<RpcNotification>,
    closed: watch::Receiver<Option<BridgeError>>,
    closed_sender: watch::Sender<Option<BridgeError>>,
    next_id: AtomicU64,
    config: RpcConfig,
}

impl std::fmt::Debug for AppServerRpc {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppServerRpc")
            .field("pending_count", &self.pending.lock().len())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AppServerRpc {
    /// Connects to app-server and completes initialize/initialized exactly once.
    pub async fn connect<R, W>(
        reader: R,
        writer: W,
        config: RpcConfig,
    ) -> Result<Arc<Self>, BridgeError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        if config.max_message_bytes == 0 || config.notification_capacity == 0 {
            return Err(protocol_error("RPC limits must be greater than zero"));
        }
        let (notifications, _) = broadcast::channel(config.notification_capacity);
        let (closed_sender, closed) = watch::channel(None);
        let pending = Arc::new(ParkingMutex::new(HashMap::new()));
        tokio::spawn(read_loop(
            reader,
            config.max_message_bytes,
            Arc::clone(&pending),
            notifications.clone(),
            closed_sender.clone(),
        ));
        let rpc = Arc::new(Self {
            writer: Mutex::new(Box::new(writer)),
            pending,
            notifications,
            closed,
            closed_sender,
            next_id: AtomicU64::new(1),
            config,
        });
        rpc.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "imagegen-bridge",
                    "title": "Imagegen Bridge",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": false,
                    "requestAttestation": false,
                    "mcpServerOpenaiFormElicitation": false
                }
            }),
        )
        .await?;
        rpc.notify("initialized", None).await?;
        Ok(rpc)
    }

    /// Subscribes before starting a turn so no notification is missed.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RpcNotification> {
        self.notifications.subscribe()
    }

    /// Returns whether the read or write side has failed permanently.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    /// Sends a request using the default timeout.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, BridgeError> {
        self.request_until(
            method,
            params,
            Instant::now() + self.config.request_timeout,
            CancellationToken::new(),
        )
        .await
    }

    /// Sends a request bounded by an absolute deadline and cancellation token.
    pub async fn request_until(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<Value, BridgeError> {
        if let Some(error) = self.closed.borrow().clone() {
            return Err(error);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"method": method, "id": id, "params": params});
        let rendered = render_message(&message, self.config.max_message_bytes)?;
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(id, sender);
        if let Err(error) = self.write(&rendered).await {
            self.pending.lock().remove(&id);
            return Err(error);
        }

        let timeout = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            result = receiver => result
                .map_err(|_| protocol_error("app-server response channel closed"))?,
            () = cancellation.cancelled() => {
                self.pending.lock().remove(&id);
                Err(BridgeError::new(ErrorCode::Cancelled, "app-server request was cancelled"))
            }
            () = tokio::time::sleep(timeout) => {
                self.pending.lock().remove(&id);
                Err(BridgeError::new(ErrorCode::Timeout, "app-server request timed out").retryable(true))
            }
        }
    }

    /// Sends a JSONL notification.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), BridgeError> {
        let mut message = json!({"method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        let rendered = render_message(&message, self.config.max_message_bytes)?;
        self.write(&rendered).await
    }

    async fn write(&self, rendered: &[u8]) -> Result<(), BridgeError> {
        let mut writer = self.writer.lock().await;
        if let Err(_error) = writer.write_all(rendered).await {
            let error = protocol_error("could not write to app-server");
            fail_connection(error.clone(), &self.pending, &self.closed_sender);
            return Err(error);
        }
        if let Err(_error) = writer.flush().await {
            let error = protocol_error("could not flush app-server input");
            fail_connection(error.clone(), &self.pending, &self.closed_sender);
            return Err(error);
        }
        Ok(())
    }
}

fn render_message(message: &Value, maximum: usize) -> Result<Vec<u8>, BridgeError> {
    let mut rendered = serde_json::to_vec(message)
        .map_err(|_| protocol_error("could not serialize app-server message"))?;
    if rendered.len() > maximum {
        return Err(protocol_error("outgoing app-server message exceeds limit"));
    }
    rendered.push(b'\n');
    Ok(rendered)
}

async fn read_loop<R>(
    reader: R,
    maximum: usize,
    pending: Arc<ParkingMutex<HashMap<u64, PendingSender>>>,
    notifications: broadcast::Sender<RpcNotification>,
    closed: watch::Sender<Option<BridgeError>>,
) where
    R: AsyncRead + Send + Unpin + 'static,
{
    let mut lines = FramedRead::new(reader, LinesCodec::new_with_max_length(maximum));
    while let Some(line) = lines.next().await {
        let result = match line {
            Ok(line) => dispatch_message(&line, &pending, &notifications),
            Err(_) => Err(protocol_error(
                "app-server message exceeds limit or is not valid UTF-8",
            )),
        };
        if let Err(error) = result {
            fail_connection(error, &pending, &closed);
            return;
        }
    }
    fail_connection(
        protocol_error("app-server connection closed"),
        &pending,
        &closed,
    );
}

fn dispatch_message(
    line: &str,
    pending: &ParkingMutex<HashMap<u64, PendingSender>>,
    notifications: &broadcast::Sender<RpcNotification>,
) -> Result<(), BridgeError> {
    let message: Value =
        serde_json::from_str(line).map_err(|_| protocol_error("app-server sent invalid JSON"))?;
    let object = message
        .as_object()
        .ok_or_else(|| protocol_error("app-server message is not an object"))?;
    if let Some(id) = object.get("id").and_then(Value::as_u64) {
        if let Some(sender) = pending.lock().remove(&id) {
            let result = if let Some(error) = object.get("error") {
                Err(rpc_response_error(error))
            } else {
                Ok(object.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = sender.send(result);
        }
        return Ok(());
    }
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_error("app-server notification has no method"))?;
    let notification = RpcNotification {
        method: method.to_owned(),
        params: object.get("params").cloned().unwrap_or(Value::Null),
    };
    let _ = notifications.send(notification);
    Ok(())
}

fn rpc_response_error(value: &Value) -> BridgeError {
    let mut error = BridgeError::new(ErrorCode::Upstream, "Codex app-server request failed")
        .with_provider("codex-app-server");
    if let Some(code) = value.get("code").and_then(Value::as_i64) {
        error = error.with_detail("rpc_code", code);
    } else if let Some(code) = value.get("code").and_then(Value::as_str).filter(|code| {
        !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    }) {
        error = error.with_detail("rpc_code", code);
    }
    error
}

fn fail_connection(
    error: BridgeError,
    pending: &ParkingMutex<HashMap<u64, PendingSender>>,
    closed: &watch::Sender<Option<BridgeError>>,
) {
    for (_, sender) in pending.lock().drain() {
        let _ = sender.send(Err(error.clone()));
    }
    closed.send_replace(Some(error));
}

fn protocol_error(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message).with_provider("codex-app-server")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio::io::duplex;
    use tokio_util::codec::{Framed, LinesCodec};

    use super::*;

    async fn initialized_connection() -> (
        Arc<AppServerRpc>,
        Framed<tokio::io::DuplexStream, LinesCodec>,
    ) {
        let (client, server) = duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client);
        let mut server = Framed::new(server, LinesCodec::new());
        let connect = tokio::spawn(AppServerRpc::connect(
            client_read,
            client_write,
            RpcConfig {
                max_message_bytes: 4096,
                request_timeout: Duration::from_secs(1),
                notification_capacity: 8,
            },
        ));
        let initialize: Value =
            serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(initialize["method"], "initialize");
        server
            .send(json!({"id": initialize["id"], "result": {"userAgent": "codex/0.0"}}).to_string())
            .await
            .unwrap();
        let rpc = connect.await.unwrap().unwrap();
        let initialized: Value =
            serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(initialized["method"], "initialized");
        (rpc, server)
    }

    #[tokio::test]
    async fn correlates_concurrent_responses_by_id() {
        let (rpc, mut server) = initialized_connection().await;
        let first = tokio::spawn({
            let rpc = Arc::clone(&rpc);
            async move { rpc.request("one", json!({})).await.unwrap() }
        });
        let second = tokio::spawn({
            let rpc = Arc::clone(&rpc);
            async move { rpc.request("two", json!({})).await.unwrap() }
        });
        let request_a: Value =
            serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
        let request_b: Value =
            serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
        server
            .send(json!({"id": request_b["id"], "result": request_b["method"]}).to_string())
            .await
            .unwrap();
        server
            .send(json!({"id": request_a["id"], "result": request_a["method"]}).to_string())
            .await
            .unwrap();
        let values = [first.await.unwrap(), second.await.unwrap()];
        assert!(values.contains(&json!("one")));
        assert!(values.contains(&json!("two")));
    }

    #[tokio::test]
    async fn forwards_parsed_notifications_without_raw_logging() {
        let (rpc, mut server) = initialized_connection().await;
        let mut notifications = rpc.subscribe();
        server
            .send(json!({"method": "turn/started", "params": {"threadId": "t"}}).to_string())
            .await
            .unwrap();
        let notification = notifications.recv().await.unwrap();
        assert_eq!(notification.method, "turn/started");
        assert_eq!(notification.params["threadId"], "t");
    }

    #[tokio::test]
    async fn rejects_outgoing_message_over_limit() {
        let (rpc, _server) = initialized_connection().await;
        let error = rpc
            .request("large", json!({"data": "x".repeat(5000)}))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);
    }

    #[tokio::test]
    async fn connection_close_fails_pending_callers_and_marks_rpc_closed() {
        let (rpc, mut server) = initialized_connection().await;
        let pending = tokio::spawn({
            let rpc = Arc::clone(&rpc);
            async move { rpc.request("pending", json!({})).await }
        });
        let request: Value = serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], "pending");
        drop(server);
        let error = pending.await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);
        assert!(rpc.is_closed());
    }

    #[tokio::test]
    async fn upstream_error_messages_are_not_reflected_to_clients() {
        let (rpc, mut server) = initialized_connection().await;
        let request = tokio::spawn({
            let rpc = Arc::clone(&rpc);
            async move { rpc.request("fails", json!({})).await }
        });
        let incoming: Value = serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
        server
            .send(
                json!({
                    "id": incoming["id"],
                    "error": {"code": "invalid_request", "message": "secret prompt and /private/path"}
                })
                .to_string(),
            )
            .await
            .unwrap();
        let error = request.await.unwrap().unwrap_err();
        assert!(!error.message.contains("secret"));
        assert!(!error.message.contains("/private"));
        assert_eq!(error.details["rpc_code"], "invalid_request");
    }
}
