//! Listener lifecycle and graceful shutdown.

use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::{Router, serve::Listener};
use imagegen_bridge_config::ServerSettings;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Sleep, sleep},
};

/// Binds a numeric socket address using Tokio.
pub async fn bind(address: SocketAddr) -> io::Result<TcpListener> {
    TcpListener::bind(address).await
}

/// Serves until the supplied shutdown signal resolves, then drains connections.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    settings: ServerSettings,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let listener = LimitedListener::new(
        listener,
        settings.max_connections.runtime_value(),
        (settings.read_timeout_ms > 0).then(|| Duration::from_millis(settings.read_timeout_ms)),
        Duration::from_millis(settings.write_timeout_ms),
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

struct LimitedListener {
    listener: TcpListener,
    permits: Arc<Semaphore>,
    read_timeout: Option<Duration>,
    write_timeout: Duration,
}

impl LimitedListener {
    fn new(
        listener: TcpListener,
        maximum: usize,
        read_timeout: Option<Duration>,
        write_timeout: Duration,
    ) -> Self {
        Self {
            listener,
            permits: Arc::new(Semaphore::new(if maximum == usize::MAX {
                Semaphore::MAX_PERMITS
            } else {
                maximum
            })),
            read_timeout,
            write_timeout,
        }
    }
}

impl Listener for LimitedListener {
    type Io = LimitedIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let permit = match Arc::clone(&self.permits).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => std::future::pending().await,
        };
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => {
                    return (
                        LimitedIo::new(stream, permit, self.read_timeout, self.write_timeout),
                        address,
                    );
                }
                Err(_) => sleep(Duration::from_secs(1)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

struct LimitedIo {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
    read_timeout: Option<Duration>,
    read_deadline: Option<Pin<Box<Sleep>>>,
    write_timeout: Duration,
    write_deadline: Pin<Box<Sleep>>,
    write_stalled: bool,
}

impl LimitedIo {
    fn new(
        stream: TcpStream,
        permit: OwnedSemaphorePermit,
        read_timeout: Option<Duration>,
        write_timeout: Duration,
    ) -> Self {
        Self {
            stream,
            _permit: permit,
            read_timeout,
            read_deadline: read_timeout.map(|timeout| Box::pin(sleep(timeout))),
            write_timeout,
            write_deadline: Box::pin(sleep(write_timeout)),
            write_stalled: false,
        }
    }

    fn poll_write_stall(&mut self, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.write_stalled {
            self.write_stalled = true;
            self.write_deadline
                .as_mut()
                .reset(tokio::time::Instant::now() + self.write_timeout);
        }
        if self.write_deadline.as_mut().poll(context).is_ready() {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP connection write timed out",
            )))
        } else {
            Poll::Pending
        }
    }

    fn record_write_progress(&mut self) {
        self.write_stalled = false;
    }
}

impl AsyncRead for LimitedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buffer.filled().len();
        if let Poll::Ready(result) = Pin::new(&mut self.stream).poll_read(context, buffer) {
            if result.is_ok()
                && buffer.filled().len() > before
                && let (Some(timeout), Some(deadline)) =
                    (self.read_timeout, self.read_deadline.as_mut())
            {
                deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + timeout);
            }
            return Poll::Ready(result);
        }
        if self
            .read_deadline
            .as_mut()
            .is_some_and(|deadline| deadline.as_mut().poll(context).is_ready())
        {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP connection read timed out",
            )))
        } else {
            Poll::Pending
        }
    }
}

impl AsyncWrite for LimitedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write(context, buffer) {
            Poll::Ready(Ok(written)) => {
                if written > 0 {
                    self.record_write_progress();
                }
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => self
                .poll_write_stall(context)
                .map(|result| result.map(|()| 0)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_flush(context) {
            Poll::Ready(Ok(())) => {
                self.record_write_progress();
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => self.poll_write_stall(context),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_shutdown(context) {
            Poll::Ready(Ok(())) => {
                self.record_write_progress();
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => self.poll_write_stall(context),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use axum::{Router, body::Body, http::Response, routing::get};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpSocket,
        sync::oneshot,
        time::timeout,
    };

    use super::*;

    #[tokio::test]
    async fn serves_real_tcp_and_shuts_down_gracefully() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = listener.local_addr().unwrap();
        let settings = ServerSettings {
            max_connections: imagegen_bridge_config::Capacity::Limited(1),
            read_timeout_ms: 1_000,
            write_timeout_ms: 1_000,
            ..ServerSettings::default()
        };
        let (shutdown, signal) = oneshot::channel();
        let server = tokio::spawn(serve(
            listener,
            Router::new().route("/", get(|| async { "ok" })),
            settings,
            async move {
                let _ = signal.await;
            },
        ));
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(response.ends_with(b"ok"));
        let _ = shutdown.send(());
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn default_read_policy_keeps_a_long_running_handler_connected() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = listener.local_addr().unwrap();
        let settings = ServerSettings {
            max_connections: imagegen_bridge_config::Capacity::Limited(1),
            write_timeout_ms: 1_000,
            ..ServerSettings::default()
        };
        assert_eq!(settings.read_timeout_ms, 0);
        let (shutdown, signal) = oneshot::channel();
        let server = tokio::spawn(serve(
            listener,
            Router::new().route(
                "/generate",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    "generated"
                }),
            ),
            settings,
            async move {
                let _ = signal.await;
            },
        ));
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET /generate HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(response.ends_with(b"generated"));
        let _ = shutdown.send(());
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stalled_writer_releases_the_connection_permit() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = listener.local_addr().unwrap();
        let settings = ServerSettings {
            max_connections: imagegen_bridge_config::Capacity::Limited(1),
            read_timeout_ms: 5_000,
            write_timeout_ms: 150,
            ..ServerSettings::default()
        };
        let router = Router::new()
            .route(
                "/large",
                get(|| async { Response::new(Body::from(vec![b'A'; 8 * 1024 * 1024])) }),
            )
            .route("/probe", get(|| async { "probe-ok" }));
        let (shutdown, signal) = oneshot::channel();
        let server = tokio::spawn(serve(listener, router, settings, async move {
            let _ = signal.await;
        }));

        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4 * 1024).unwrap();
        let mut slow_client = socket.connect(address).await.unwrap();
        slow_client
            .write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
            .await
            .unwrap();

        let mut probe = TcpStream::connect(address).await.unwrap();
        probe
            .write_all(b"GET /probe HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(3), probe.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(response.ends_with(b"probe-ok"));

        let _ = shutdown.send(());
        timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
