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
        settings.max_connections,
        Duration::from_millis(settings.read_timeout_ms),
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

struct LimitedListener {
    listener: TcpListener,
    permits: Arc<Semaphore>,
    read_timeout: Duration,
}

impl LimitedListener {
    fn new(listener: TcpListener, maximum: usize, read_timeout: Duration) -> Self {
        Self {
            listener,
            permits: Arc::new(Semaphore::new(maximum)),
            read_timeout,
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
                    return (LimitedIo::new(stream, permit, self.read_timeout), address);
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
    read_timeout: Duration,
    read_deadline: Pin<Box<Sleep>>,
}

impl LimitedIo {
    fn new(stream: TcpStream, permit: OwnedSemaphorePermit, read_timeout: Duration) -> Self {
        Self {
            stream,
            _permit: permit,
            read_timeout,
            read_deadline: Box::pin(sleep(read_timeout)),
        }
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
            if result.is_ok() && buffer.filled().len() > before {
                let timeout = self.read_timeout;
                self.read_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + timeout);
            }
            return Poll::Ready(result);
        }
        if self.read_deadline.as_mut().poll(context).is_ready() {
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
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use axum::{Router, routing::get};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        sync::oneshot,
    };

    use super::*;

    #[tokio::test]
    async fn serves_real_tcp_and_shuts_down_gracefully() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = listener.local_addr().unwrap();
        let settings = ServerSettings {
            max_connections: 1,
            read_timeout_ms: 1_000,
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
}
