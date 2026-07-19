//! Stable, OAuth-sterile front door for single-active blue/green handoffs.

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    error_handling::HandleErrorLayer,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt as _;
use imagegen_bridge::core::{BridgeError, ErrorCode};
use reqwest::{Client, Url};
use serde_json::json;
use tokio::time::Instant;
use tower::{BoxError, ServiceBuilder, limit::ConcurrencyLimitLayer, load_shed::LoadShedLayer};
use tower_http::limit::RequestBodyLimitLayer;

use crate::{args::GatewayArgs, commands::shutdown_signal, output::Output};

#[derive(Clone)]
struct GatewayState {
    client: Client,
    blue: Url,
    green: Url,
    state_file: Arc<std::path::PathBuf>,
    hold_timeout: Duration,
    probe_interval: Duration,
    forward_timeout: Duration,
    response_idle_timeout: Duration,
}

pub(crate) async fn run(args: &GatewayArgs, output: &Output) -> Result<(), BridgeError> {
    if args.hold_timeout_ms == 0
        || args.probe_interval_ms == 0
        || args.max_connections == 0
        || args.max_body_bytes == 0
        || args.forward_timeout_ms == 0
        || args.response_idle_timeout_ms == 0
    {
        return Err(invalid("gateway limits must be greater than zero"));
    }
    let address: SocketAddr = args
        .bind
        .parse()
        .map_err(|_| invalid("gateway bind must be a numeric socket address"))?;
    let blue = backend_url(&args.blue)?;
    let green = backend_url(&args.green)?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .build()
        .map_err(|_| internal("could not initialize gateway HTTP client"))?;
    let state = GatewayState {
        client,
        blue,
        green,
        state_file: Arc::new(args.state_file.clone()),
        hold_timeout: Duration::from_millis(args.hold_timeout_ms),
        probe_interval: Duration::from_millis(args.probe_interval_ms),
        forward_timeout: Duration::from_millis(args.forward_timeout_ms),
        response_idle_timeout: Duration::from_millis(args.response_idle_timeout_ms),
    };
    let app = Router::new()
        .route("/health/live", get(gateway_liveness))
        .route("/health/ready", get(gateway_readiness))
        .fallback(proxy)
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_: BoxError| async {
                    unavailable("gateway request capacity is exhausted")
                }))
                .layer(LoadShedLayer::new())
                .layer(ConcurrencyLimitLayer::new(args.max_connections)),
        )
        .layer(RequestBodyLimitLayer::new(args.max_body_bytes))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|_| internal("could not bind gateway listener"))?;
    let local = listener
        .local_addr()
        .map_err(|_| internal("could not inspect gateway listener"))?;
    output.status(&format!("deployment gateway listening on http://{local}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown_signal().await;
        })
        .await
        .map_err(|_| internal("deployment gateway failed"))
}

async fn gateway_liveness() -> Json<serde_json::Value> {
    Json(json!({"status":"live","component":"deployment_gateway"}))
}

async fn gateway_readiness(State(state): State<GatewayState>) -> Response {
    match selected_ready_backend(&state).await {
        Some((slot, _)) => Json(json!({"status":"ready","active_slot":slot})).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, HeaderValue::from_static("1"))],
            Json(json!({"status":"holding"})),
        )
            .into_response(),
    }
}

async fn proxy(State(state): State<GatewayState>, request: Request) -> Response {
    let deadline = Instant::now() + state.hold_timeout;
    let backend = loop {
        if let Some((_, backend)) = selected_ready_backend(&state).await {
            break backend;
        }
        if Instant::now() >= deadline {
            return unavailable("deployment handoff exceeded the bounded hold timeout");
        }
        tokio::time::sleep(state.probe_interval).await;
    };
    forward(&state, backend, request).await
}

async fn selected_ready_backend(state: &GatewayState) -> Option<(&'static str, Url)> {
    let slot = tokio::fs::read_to_string(state.state_file.as_ref())
        .await
        .ok()?;
    let (name, backend) = match slot.trim() {
        "blue" => ("blue", state.blue.clone()),
        "green" => ("green", state.green.clone()),
        _ => return None,
    };
    let health = backend.join("health/ready").ok()?;
    let response = tokio::time::timeout(Duration::from_secs(2), state.client.get(health).send())
        .await
        .ok()?
        .ok()?;
    response.status().is_success().then_some((name, backend))
}

async fn forward(state: &GatewayState, backend: Url, request: Request) -> Response {
    let deadline = Instant::now() + state.forward_timeout;
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str);
    let Ok(target) = backend.join(path.trim_start_matches('/')) else {
        return unavailable("gateway could not resolve the selected backend route");
    };
    let mut upstream = state.client.request(parts.method, target);
    for (name, value) in &parts.headers {
        if !hop_by_hop(name) && name != header::HOST {
            upstream = upstream.header(name, value);
        }
    }
    let response = tokio::time::timeout_at(
        deadline,
        upstream
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send(),
    )
    .await;
    let Ok(Ok(response)) = response else {
        return unavailable("active backend did not return headers before the request deadline");
    };
    let status = response.status();
    let headers = filtered_headers(response.headers());
    let mut output = Response::builder().status(status);
    if let Some(target) = output.headers_mut() {
        *target = headers;
    }
    let idle_timeout = state.response_idle_timeout;
    let mut stream = response.bytes_stream();
    let bounded = async_stream::stream! {
        loop {
            let idle_deadline = (Instant::now() + idle_timeout).min(deadline);
            match tokio::time::timeout_at(idle_deadline, stream.next()).await {
                Ok(Some(Ok(bytes))) => yield Ok::<_, io::Error>(bytes),
                Ok(Some(Err(_))) => {
                    yield Err(io::Error::other("backend response stream failed"));
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    yield Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "backend response stream deadline exceeded",
                    ));
                    break;
                }
            }
        }
    };
    output
        .body(Body::from_stream(bounded))
        .unwrap_or_else(|_| unavailable("gateway could not construct the upstream response"))
}

fn filtered_headers(source: &HeaderMap) -> HeaderMap {
    source
        .iter()
        .filter(|(name, _)| !hop_by_hop(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn unavailable(message: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, HeaderValue::from_static("1"))],
        Json(json!({
            "error": {
                "code": "deployment_handoff",
                "message": message,
                "retryable": true
            }
        })),
    )
        .into_response()
}

fn backend_url(value: &str) -> Result<Url, BridgeError> {
    let mut url = Url::parse(value).map_err(|_| invalid("gateway backend URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid("gateway backend must use HTTP or HTTPS"));
    }
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn invalid(message: &'static str) -> BridgeError {
    BridgeError::new(ErrorCode::InvalidRequest, message)
}

fn internal(message: &'static str) -> BridgeError {
    BridgeError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    #[test]
    fn rejects_unsupported_backend_schemes_and_filters_hop_headers() {
        assert!(backend_url("file:///tmp/socket").is_err());
        assert!(hop_by_hop(&header::CONNECTION));
        assert!(!hop_by_hop(&header::AUTHORIZATION));
    }

    #[tokio::test]
    async fn holds_during_cutover_then_streams_to_the_selected_ready_slot() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let backend = Router::new()
            .route("/health/ready", get(|| async { StatusCode::OK }))
            .route("/echo", post(|body: Body| async { body }));
        tokio::spawn(async move { axum::serve(listener, backend).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let state_file = directory.path().join("active-slot");
        tokio::fs::write(&state_file, "hold\n").await.unwrap();
        let state = GatewayState {
            client: Client::builder().no_proxy().build().unwrap(),
            blue: Url::parse("http://127.0.0.1:9/").unwrap(),
            green: Url::parse(&format!("http://{address}/")).unwrap(),
            state_file: Arc::new(state_file.clone()),
            hold_timeout: Duration::from_secs(2),
            probe_interval: Duration::from_millis(10),
            forward_timeout: Duration::from_secs(2),
            response_idle_timeout: Duration::from_secs(1),
        };
        let request = Request::post("/echo")
            .body(Body::from("held-body"))
            .unwrap();
        let proxy_task = tokio::spawn(proxy(State(state), request));
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(!proxy_task.is_finished());
        tokio::fs::write(&state_file, "green\n").await.unwrap();
        let response = proxy_task.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, "held-body");
    }

    #[tokio::test]
    async fn gateway_enforces_streamed_body_and_backend_header_deadlines() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let backend = Router::new()
            .route("/health/ready", get(|| async { StatusCode::OK }))
            .route(
                "/slow",
                post(|| async {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    "late"
                }),
            );
        tokio::spawn(async move { axum::serve(listener, backend).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let state_file = directory.path().join("active-slot");
        tokio::fs::write(&state_file, "blue\n").await.unwrap();
        let state = GatewayState {
            client: Client::builder().no_proxy().build().unwrap(),
            blue: Url::parse(&format!("http://{address}/")).unwrap(),
            green: Url::parse("http://127.0.0.1:9/").unwrap(),
            state_file: Arc::new(state_file),
            hold_timeout: Duration::from_secs(1),
            probe_interval: Duration::from_millis(5),
            forward_timeout: Duration::from_millis(20),
            response_idle_timeout: Duration::from_millis(20),
        };
        let app = Router::new()
            .fallback(proxy)
            .layer(RequestBodyLimitLayer::new(4))
            .with_state(state);
        let oversized = app
            .clone()
            .oneshot(
                Request::post("/slow")
                    .header(header::CONTENT_LENGTH, "5")
                    .body(Body::from("12345"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let timed_out = app
            .oneshot(Request::post("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(timed_out.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn gateway_load_sheds_beyond_its_bounded_hold_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let state_file = directory.path().join("active-slot");
        tokio::fs::write(&state_file, "hold\n").await.unwrap();
        let state = GatewayState {
            client: Client::builder().no_proxy().build().unwrap(),
            blue: Url::parse("http://127.0.0.1:9/").unwrap(),
            green: Url::parse("http://127.0.0.1:10/").unwrap(),
            state_file: Arc::new(state_file),
            hold_timeout: Duration::from_millis(100),
            probe_interval: Duration::from_millis(5),
            forward_timeout: Duration::from_secs(1),
            response_idle_timeout: Duration::from_secs(1),
        };
        let app = Router::new()
            .fallback(proxy)
            .layer(
                ServiceBuilder::new()
                    .layer(HandleErrorLayer::new(|_: BoxError| async {
                        unavailable("gateway request capacity is exhausted")
                    }))
                    .layer(LoadShedLayer::new())
                    .layer(ConcurrencyLimitLayer::new(1)),
            )
            .with_state(state);
        let held = tokio::spawn(
            app.clone()
                .oneshot(Request::get("/held").body(Body::empty()).unwrap()),
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        let shed = app
            .oneshot(Request::get("/shed").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(shed.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            held.await.unwrap().unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
