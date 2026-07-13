//! Versioned route graph and native request handlers.

use std::{sync::Arc, time::Instant};

use axum::{
    Json, Router,
    extract::{
        Extension, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use imagegen_bridge_config::ServerSettings;
use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImageRequest, ProviderDescriptor, ProviderEvent,
};
use imagegen_bridge_runtime::{ExecutionContext, ImagegenRuntime, ProviderReadinessStatus};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower::limit::ConcurrencyLimitLayer;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::{
    ApiError,
    auth::{AuthPolicy, AuthScope, authorize},
    compat::{edit_compatible, generate_compatible},
    metrics::ServerMetrics,
    openapi::openapi_document,
    streaming::stream_image,
};

const REQUEST_ID_HEADER: &str = "x-request-id";
const MAX_CURSOR_BYTES: usize = 256;
const MAX_PAGE_SIZE: usize = 100;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 512;

/// Immutable state shared by every HTTP request.
#[derive(Clone)]
pub struct ServerState {
    /// Shared provider-neutral execution runtime.
    pub runtime: Arc<ImagegenRuntime>,
    pub(crate) auth: Option<AuthPolicy>,
    pub(crate) metrics: Option<Arc<ServerMetrics>>,
}

impl std::fmt::Debug for ServerState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerState")
            .field("runtime", &self.runtime)
            .field("auth", &self.auth.is_some())
            .field("metrics", &self.metrics.is_some())
            .finish()
    }
}

impl ServerState {
    /// Builds state and resolves an optional bridge token from its env reference.
    pub fn from_settings(
        runtime: Arc<ImagegenRuntime>,
        settings: &ServerSettings,
    ) -> Result<Self, BridgeError> {
        let auth = settings
            .bearer_token_env
            .as_deref()
            .map(|name| {
                std::env::var(name).map_err(|_| {
                    BridgeError::new(
                        ErrorCode::Configuration,
                        "configured bridge bearer token is unavailable",
                    )
                })
            })
            .transpose()?
            .and_then(AuthPolicy::new);
        if settings.bearer_token_env.is_some() && auth.is_none() {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "configured bridge bearer token is empty",
            ));
        }
        let metrics = settings
            .metrics
            .enabled
            .then(|| Arc::new(ServerMetrics::default()));
        Ok(Self {
            runtime,
            auth,
            metrics,
        })
    }

    /// Builds state with an explicit secret for embedded/tests use.
    #[must_use]
    pub fn with_bearer(runtime: Arc<ImagegenRuntime>, token: Option<SecretString>) -> Self {
        Self::with_bearer_and_metrics(runtime, token, false)
    }

    /// Builds state with explicit auth and metrics controls for embedding/tests.
    #[must_use]
    pub fn with_bearer_and_metrics(
        runtime: Arc<ImagegenRuntime>,
        token: Option<SecretString>,
        metrics_enabled: bool,
    ) -> Self {
        let auth = token.and_then(|token| {
            use secrecy::ExposeSecret as _;
            AuthPolicy::new(token.expose_secret().to_owned())
        });
        let metrics = metrics_enabled.then(|| Arc::new(ServerMetrics::default()));
        Self {
            runtime,
            auth,
            metrics,
        }
    }
}

/// Safe bridge-generated request correlation ID.
#[derive(Debug, Clone)]
pub struct RequestId(pub(crate) String);

impl RequestId {
    pub(crate) fn new() -> Self {
        Self(Uuid::now_v7().to_string())
    }
}

/// Builds the complete route graph with configured body and concurrency bounds.
pub fn router(state: ServerState, settings: &ServerSettings) -> Router {
    let mut protected = Router::new()
        .route("/v1/images", post(execute_image))
        .route("/v1/images/stream", post(stream_image))
        .route("/v1/images/generations", post(generate_compatible))
        .route("/v1/images/edits", post(edit_compatible))
        .route("/v1/providers", get(list_providers))
        .route(
            "/v1/providers/{provider}/capabilities",
            get(provider_capabilities),
        )
        .route(
            "/v1/sessions/{key}",
            get(get_session).delete(delete_session),
        );
    if state.metrics.is_some() {
        protected = protected.route("/metrics", get(prometheus_metrics));
    }
    let protected = protected.route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Router::new()
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .route(
            "/v1/openapi.json",
            get(|| async { Json(openapi_document()) }),
        )
        .merge(protected)
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(ConcurrencyLimitLayer::new(settings.max_connections))
        .layer(axum::extract::DefaultBodyLimit::max(
            usize::try_from(settings.max_body_bytes).unwrap_or(usize::MAX),
        ))
        .layer(middleware::from_fn_with_state(
            settings.max_header_bytes,
            enforce_header_limit,
        ))
        .layer(middleware::from_fn(request_id))
        .with_state(state)
}

async fn not_found(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::bad_request("route was not found", request_id).with_status(StatusCode::NOT_FOUND)
}

async fn method_not_allowed(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::bad_request("HTTP method is not allowed for this route", request_id)
        .with_status(StatusCode::METHOD_NOT_ALLOWED)
}

async fn enforce_header_limit(
    State(maximum): State<usize>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::new);
    let bytes = request
        .headers()
        .iter()
        .try_fold(0_usize, |total, (name, value)| {
            total
                .checked_add(name.as_str().len())?
                .checked_add(value.as_bytes().len())
        });
    if bytes.is_none_or(|bytes| bytes > maximum) {
        return ApiError::bad_request("request headers exceed the byte limit", request_id)
            .with_status(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE)
            .into_response();
    }
    next.run(request).await
}

async fn request_id(mut request: axum::extract::Request, next: middleware::Next) -> Response {
    let request_id = RequestId::new();
    request.extensions_mut().insert(request_id.clone());
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&request_id.0) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

#[derive(Serialize)]
struct LiveResponse {
    status: &'static str,
}

async fn liveness() -> Json<LiveResponse> {
    Json(LiveResponse { status: "live" })
}

#[derive(Serialize)]
struct ReadinessResponse {
    status: &'static str,
    providers: Vec<imagegen_bridge_runtime::ProviderReadiness>,
}

async fn readiness(State(state): State<ServerState>) -> Response {
    let providers = state.runtime.registry().readiness().await;
    let ready = providers
        .iter()
        .all(|check| matches!(check.status, ProviderReadinessStatus::Ready));
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(ReadinessResponse {
            status: if ready { "ready" } else { "not_ready" },
            providers,
        }),
    )
        .into_response()
}

async fn prometheus_metrics(State(state): State<ServerState>) -> Response {
    let provider_restarts = state
        .runtime
        .registry()
        .descriptors()
        .into_iter()
        .filter_map(|descriptor| {
            state
                .runtime
                .registry()
                .resolve(Some(&descriptor.name))
                .ok()?
                .restart_count()
                .map(|count| (descriptor.name, count))
        })
        .collect();
    let body = state.metrics.as_ref().map_or_else(String::new, |metrics| {
        metrics.render(&state.runtime.queue_snapshot(), &provider_restarts)
    });
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderQuery {
    #[serde(default = "default_page_size")]
    limit: usize,
    cursor: Option<String>,
}

const fn default_page_size() -> usize {
    20
}

#[derive(Serialize)]
struct ProviderPage {
    items: Vec<ProviderDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

async fn list_providers(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    query: Result<Query<ProviderQuery>, QueryRejection>,
) -> Result<Json<ProviderPage>, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request("provider query parameters are invalid", request_id.clone())
    })?;
    if query.limit == 0 || query.limit > MAX_PAGE_SIZE {
        return Err(ApiError::bad_request(
            "provider page limit must be between 1 and 100",
            request_id,
        ));
    }
    let after = query
        .cursor
        .as_deref()
        .map(decode_cursor)
        .transpose()
        .map_err(|message| ApiError::bad_request(message, request_id.clone()))?;
    let mut items = state.runtime.registry().descriptors();
    if let Some(after) = after {
        items.retain(|item| item.name > after);
    }
    let has_more = items.len() > query.limit;
    items.truncate(query.limit);
    let next_cursor = has_more
        .then(|| items.last().map(|item| encode_cursor(&item.name)))
        .flatten();
    Ok(Json(ProviderPage { items, next_cursor }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityQuery {
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionQuery {
    provider: Option<String>,
}

async fn get_session(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Path(key): Path<String>,
    query: Result<Query<SessionQuery>, QueryRejection>,
) -> Result<Json<imagegen_bridge_core::SessionMetadata>, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request("session query parameters are invalid", request_id.clone())
    })?;
    state
        .runtime
        .registry()
        .resolve(query.provider.as_deref())
        .map_err(|error| ApiError::from_bridge(error, request_id.clone()))?
        .get_session(&key)
        .await
        .map(Json)
        .map_err(|error| ApiError::from_bridge(error, request_id))
}

async fn delete_session(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Path(key): Path<String>,
    query: Result<Query<SessionQuery>, QueryRejection>,
) -> Result<StatusCode, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request("session query parameters are invalid", request_id.clone())
    })?;
    state
        .runtime
        .registry()
        .resolve(query.provider.as_deref())
        .map_err(|error| ApiError::from_bridge(error, request_id.clone()))?
        .delete_session(&key)
        .await
        .map_err(|error| ApiError::from_bridge(error, request_id))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn provider_capabilities(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Path(provider): Path<String>,
    query: Result<Query<CapabilityQuery>, QueryRejection>,
) -> Result<Json<imagegen_bridge_core::ProviderCapabilities>, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request(
            "capability query parameters are invalid",
            request_id.clone(),
        )
    })?;
    state
        .runtime
        .registry()
        .capabilities(Some(&provider), query.model.as_deref())
        .await
        .map(Json)
        .map_err(|error| ApiError::from_bridge(error, request_id))
}

async fn execute_image(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(scope): Extension<AuthScope>,
    headers: HeaderMap,
    payload: Result<Json<ImageRequest>, JsonRejection>,
) -> Result<Json<imagegen_bridge_core::ImageResponse>, ApiError> {
    let Json(request) = payload.map_err(|_| {
        ApiError::bad_request(
            "request body must be valid ImageRequest JSON",
            request_id.clone(),
        )
    })?;
    run_request(&state, request_id, scope, &headers, request)
        .await
        .map(Json)
}

pub(crate) async fn run_request(
    state: &ServerState,
    request_id: RequestId,
    scope: AuthScope,
    headers: &HeaderMap,
    request: ImageRequest,
) -> Result<imagegen_bridge_core::ImageResponse, ApiError> {
    run_request_with_cancellation(
        state,
        request_id,
        scope,
        headers,
        request,
        CancellationToken::new(),
    )
    .await
}

pub(crate) async fn run_request_with_cancellation(
    state: &ServerState,
    request_id: RequestId,
    scope: AuthScope,
    headers: &HeaderMap,
    request: ImageRequest,
    cancellation: CancellationToken,
) -> Result<imagegen_bridge_core::ImageResponse, ApiError> {
    run_request_internal(
        state,
        request_id,
        scope,
        headers,
        request,
        cancellation,
        None,
    )
    .await
}

pub(crate) async fn run_request_with_events(
    state: &ServerState,
    request_id: RequestId,
    scope: AuthScope,
    headers: &HeaderMap,
    request: ImageRequest,
    cancellation: CancellationToken,
    events: mpsc::Sender<ProviderEvent>,
) -> Result<imagegen_bridge_core::ImageResponse, ApiError> {
    run_request_internal(
        state,
        request_id,
        scope,
        headers,
        request,
        cancellation,
        Some(events),
    )
    .await
}

async fn run_request_internal(
    state: &ServerState,
    request_id: RequestId,
    scope: AuthScope,
    headers: &HeaderMap,
    mut request: ImageRequest,
    cancellation: CancellationToken,
    events: Option<mpsc::Sender<ProviderEvent>>,
) -> Result<imagegen_bridge_core::ImageResponse, ApiError> {
    if let Some(value) = headers.get("idempotency-key") {
        let key = value.to_str().map_err(|_| {
            ApiError::bad_request("Idempotency-Key must be visible ASCII", request_id.clone())
        })?;
        if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
            return Err(ApiError::bad_request(
                "Idempotency-Key length is invalid",
                request_id,
            ));
        }
        if request
            .idempotency_key
            .as_deref()
            .is_some_and(|body| body != key)
        {
            return Err(ApiError::bad_request(
                "Idempotency-Key conflicts with the request body",
                request_id,
            ));
        }
        request.idempotency_key = Some(key.to_owned());
    }
    let guard = CancelOnDrop(cancellation.clone());
    let provider = state
        .runtime
        .registry()
        .resolve(request.routing.provider.as_deref())
        .map_or_else(
            |_| "unresolved".to_owned(),
            |provider| provider.descriptor().name,
        );
    let started = Instant::now();
    let span = tracing::info_span!(
        "imagegen_bridge.image_operation",
        request_id = %request_id.0,
        provider = %provider
    );
    let context = ExecutionContext {
        request_id: Some(request_id.0.clone()),
        idempotency_scope: scope.0,
        cancellation,
    };
    let result = async {
        if let Some(events) = events {
            state
                .runtime
                .execute_with_events(request, context, events)
                .await
        } else {
            state.runtime.execute_with(request, context).await
        }
    }
    .instrument(span)
    .await;
    guard.disarm();
    if let Some(metrics) = &state.metrics {
        metrics.record(&provider, &result, started.elapsed());
    }
    match &result {
        Ok(_) => {
            tracing::info!(request_id = %request_id.0, provider = %provider, "image operation completed");
        }
        Err(error) => {
            tracing::warn!(request_id = %request_id.0, provider = %provider, error_code = ?error.code, retryable = error.retryable, "image operation failed");
        }
    }
    result.map_err(|error| ApiError::from_bridge(error, request_id))
}

struct CancelOnDrop(CancellationToken);

impl CancelOnDrop {
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn encode_cursor(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value)
}

fn decode_cursor(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || value.len() > MAX_CURSOR_BYTES {
        return Err("provider cursor length is invalid");
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| "provider cursor is malformed")?;
    let decoded = String::from_utf8(decoded).map_err(|_| "provider cursor is malformed")?;
    if decoded.is_empty() || decoded.len() > 64 {
        return Err("provider cursor is malformed");
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use async_trait::async_trait;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt as _;
    use imagegen_bridge_core::{
        Background, GeneratedImage, ImageAction, ImagePayload, ImageProvider, ImageResponse,
        InputCapabilities, InputFidelity, Moderation, OutputFormat, ProviderCapabilities,
        ProviderContext, Quality, SizeCapabilities, SupportLevel, Timings, U8Range,
    };
    use imagegen_bridge_runtime::{ProviderRegistry, RuntimeConfig};
    use tower::ServiceExt as _;

    use super::*;

    struct ReadyProvider;

    const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[async_trait]
    impl ImageProvider for ReadyProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                name: "ready".to_owned(),
                display_name: "Ready".to_owned(),
                version: "test".to_owned(),
                experimental: false,
            }
        }

        async fn capabilities(
            &self,
            _model: Option<&str>,
        ) -> Result<ProviderCapabilities, BridgeError> {
            let inputs = InputCapabilities {
                support: SupportLevel::Native,
                max_count: 5,
                max_bytes_each: 32 * 1024 * 1024,
                max_bytes_total: 64 * 1024 * 1024,
            };
            Ok(ProviderCapabilities {
                provider: "ready".to_owned(),
                implementation_version: "test".to_owned(),
                model: Some("test-image".to_owned()),
                experimental: false,
                generation: true,
                edits: true,
                count: U8Range { min: 1, max: 4 },
                sizes: SizeCapabilities {
                    auto: true,
                    allowed: std::collections::BTreeSet::default(),
                    arbitrary: true,
                    min_edge: None,
                    max_edge: Some(4096),
                    edge_multiple: None,
                    min_pixels: None,
                    max_pixels: None,
                    max_aspect_ratio: None,
                },
                aspect_ratio: SupportLevel::Native,
                resolution: SupportLevel::Native,
                qualities: [Quality::Auto, Quality::Low, Quality::Medium, Quality::High]
                    .into_iter()
                    .collect(),
                output_formats: [OutputFormat::Png].into_iter().collect(),
                backgrounds: [Background::Auto, Background::Opaque].into_iter().collect(),
                moderation: [Moderation::Auto, Moderation::Low].into_iter().collect(),
                negative_prompt: SupportLevel::Native,
                revised_prompt: SupportLevel::Native,
                user_attribution: SupportLevel::Native,
                input_fidelities: [InputFidelity::Low, InputFidelity::High]
                    .into_iter()
                    .collect(),
                actions: [ImageAction::Auto, ImageAction::Generate, ImageAction::Edit]
                    .into_iter()
                    .collect(),
                reference_images: inputs.clone(),
                edit_images: inputs,
                masks: InputCapabilities {
                    support: SupportLevel::Native,
                    max_count: 1,
                    max_bytes_each: 32 * 1024 * 1024,
                    max_bytes_total: 32 * 1024 * 1024,
                },
                partial_images: U8Range { min: 0, max: 3 },
                persistent_sessions: true,
                explicit_threads: true,
            })
        }

        async fn execute(
            &self,
            request: ImageRequest,
            context: ProviderContext,
        ) -> Result<ImageResponse, BridgeError> {
            if request.parameters.partial_images > 0
                && let Some(events) = &context.events
            {
                events
                    .send(ProviderEvent::PartialImage {
                        index: 0,
                        partial_index: 0,
                        b64_json: "partial-fixture".to_owned(),
                    })
                    .await
                    .unwrap();
            }
            Ok(ImageResponse {
                id: context.request_id,
                created: 123,
                provider: "ready".to_owned(),
                model: "test-image".to_owned(),
                requested: request.parameters.clone(),
                effective: request.parameters,
                normalizations: Vec::new(),
                data: vec![GeneratedImage {
                    index: 0,
                    payload: ImagePayload::B64Json {
                        b64_json: ONE_PIXEL_PNG.to_owned(),
                    },
                    format: OutputFormat::Png,
                    width: 1,
                    height: 1,
                    bytes: 68,
                    sha256: "431ced6916a2a21a156e38701afe55bbd7f88969fbbfc56d7fe099d47f265460"
                        .to_owned(),
                    generation_ms: None,
                }],
                failures: Vec::new(),
                revised_prompt: Some("revised".to_owned()),
                usage: None,
                session: None,
                timings: Timings::default(),
                warnings: Vec::new(),
            })
        }

        async fn check_ready(&self) -> Result<(), BridgeError> {
            Ok(())
        }
    }

    fn test_router(token: Option<&str>) -> Router {
        test_router_with_settings(token, &ServerSettings::default())
    }

    fn test_router_with_settings(token: Option<&str>, settings: &ServerSettings) -> Router {
        let registry =
            ProviderRegistry::new([Arc::new(ReadyProvider) as Arc<dyn ImageProvider>], "ready")
                .unwrap();
        let runtime = Arc::new(ImagegenRuntime::new(registry, RuntimeConfig::default()).unwrap());
        let state = ServerState::with_bearer_and_metrics(
            runtime,
            token.map(|value| SecretString::from(value.to_owned())),
            settings.metrics.enabled,
        );
        router(state, settings)
    }

    #[tokio::test]
    async fn health_is_public_but_v1_requires_valid_bearer() {
        let app = test_router(Some("bridge-secret"));
        let health = app
            .clone()
            .oneshot(Request::get("/health/live").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        assert!(health.headers().contains_key(REQUEST_ID_HEADER));

        let missing = app
            .clone()
            .oneshot(Request::get("/v1/providers").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        assert!(
            missing
                .headers()
                .contains_key(axum::http::header::WWW_AUTHENTICATE)
        );
        let authorized = app
            .oneshot(
                Request::get("/v1/providers")
                    .header(axum::http::header::AUTHORIZATION, "Bearer bridge-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn native_execution_returns_verified_response() {
        let app = test_router(None);
        let response = app
            .oneshot(
                Request::post("/v1/images")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&ImageRequest::generate("test")).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["provider"], "ready");
        assert_eq!(value["data"][0]["width"], 1);
        assert_eq!(
            value["data"][0]["sha256"],
            "431ced6916a2a21a156e38701afe55bbd7f88969fbbfc56d7fe099d47f265460"
        );
    }

    #[tokio::test]
    async fn compatibility_and_streaming_routes_share_the_runtime() {
        let app = test_router(None);
        let compatible = app
            .clone()
            .oneshot(
                Request::post("/v1/images/generations")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"prompt":"test","quality":"auto"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = compatible.status();
        let bytes = compatible.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["data"][0]["revised_prompt"], "revised");
        assert_eq!(value["imagegen_bridge"]["provider"], "ready");

        let mut stream_request = ImageRequest::generate("test");
        stream_request.parameters.partial_images = 1;
        let streaming = app
            .oneshot(
                Request::post("/v1/images/stream")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&stream_request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(streaming.status(), StatusCode::OK);
        let body = streaming.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("event: started"));
        assert!(body.contains("event: partial_image"));
        assert!(body.contains("\"b64_json\":\"partial-fixture\""));
        assert!(body.contains("event: completed"));
        assert!(body.contains("\"provider\":\"ready\""));
    }

    #[tokio::test]
    async fn rejects_invalid_pagination_without_panicking() {
        let response = test_router(None)
            .oneshot(
                Request::get("/v1/providers?limit=0&cursor=%25")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn header_limit_and_unknown_routes_use_canonical_errors() {
        let settings = ServerSettings {
            max_header_bytes: 24,
            ..ServerSettings::default()
        };
        let oversized = test_router_with_settings(None, &settings)
            .oneshot(
                Request::get("/v1/providers")
                    .header("x-oversized", "a".repeat(32))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            oversized.status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
        assert!(oversized.headers().contains_key(REQUEST_ID_HEADER));
        let body = oversized.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], "invalid_request");
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(value["error"]["param"].is_null());
        assert_eq!(value["error"]["imagegen_bridge"]["code"], "invalid_request");
        assert_eq!(value["error"]["imagegen_bridge"]["retryable"], false);
        assert!(value["request_id"].is_string());

        let missing = test_router(None)
            .oneshot(Request::get("/missing").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let body = missing.into_body().collect().await.unwrap().to_bytes();
        assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
    }

    #[tokio::test]
    async fn metrics_are_opt_in_authenticated_and_content_safe() {
        let disabled = test_router(None)
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(disabled.status(), StatusCode::NOT_FOUND);

        let settings = ServerSettings {
            metrics: imagegen_bridge_config::MetricsSettings { enabled: true },
            ..ServerSettings::default()
        };
        let app = test_router_with_settings(Some("metrics-secret"), &settings);
        let unauthorized = app
            .clone()
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let generation = app
            .clone()
            .oneshot(
                Request::post("/v1/images")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer metrics-secret")
                    .body(Body::from(
                        serde_json::to_vec(&ImageRequest::generate("never expose this prompt"))
                            .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(generation.status(), StatusCode::OK);

        let rejected = app
            .clone()
            .oneshot(
                Request::post("/v1/images")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer metrics-secret")
                    .body(Body::from(
                        serde_json::to_vec(&ImageRequest::generate("")).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let metrics = app
            .oneshot(
                Request::get("/metrics")
                    .header(header::AUTHORIZATION, "Bearer metrics-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(metrics.status(), StatusCode::OK);
        assert_eq!(
            metrics.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        let body = metrics.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("imagegen_bridge_requests_total"));
        assert!(body.contains("provider=\"ready\""));
        assert!(body.contains("result=\"success\""));
        assert!(body.contains("result=\"error\",code=\"invalid_request\""));
        assert!(body.contains("imagegen_bridge_generated_bytes_total"));
        assert!(body.contains("} 68"));
        assert!(body.contains("imagegen_bridge_queue_depth"));
        assert!(!body.contains("never expose this prompt"));
        assert!(!body.contains("metrics-secret"));
    }
}
