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
use imagegen_bridge_config::{ResolvedConfig, ServerSettings};
use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImageJob, ImageJobPage, ImageJobStatus, ImageJobUpdate, ImageRequest,
    ProviderDescriptor, ProviderEvent,
};
use imagegen_bridge_runtime::{
    ExecutionContext, ImageJobListFilter, ImageJobVisibility, ImagegenRuntime,
    ProviderReadinessStatus,
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower::limit::ConcurrencyLimitLayer;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::{
    ApiError, JobManager,
    auth::{AuthPolicy, AuthScope, authorize},
    compat::{edit_compatible, generate_compatible},
    dashboard_router,
    diagnostics::ConfigurationDiagnostics,
    events::{OperatorEventHistory, OperatorEvents, redacted_route},
    metrics::ServerMetrics,
    openapi::openapi_document,
    streaming::stream_image,
};

const REQUEST_ID_HEADER: &str = "x-request-id";
const MAX_CURSOR_BYTES: usize = 256;
const MAX_PAGE_SIZE: usize = 100;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 512;
const MAX_JOB_SEARCH_BYTES: usize = 512;

/// Immutable state shared by every HTTP request.
#[derive(Clone)]
pub struct ServerState {
    /// Shared provider-neutral execution runtime.
    pub runtime: Arc<ImagegenRuntime>,
    /// Optional durable asynchronous job manager.
    pub jobs: Option<Arc<JobManager>>,
    pub(crate) auth: Option<AuthPolicy>,
    pub(crate) metrics: Option<Arc<ServerMetrics>>,
    pub(crate) diagnostics: Arc<ConfigurationDiagnostics>,
    pub(crate) events: Arc<OperatorEvents>,
}

impl std::fmt::Debug for ServerState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerState")
            .field("runtime", &self.runtime)
            .field("jobs", &self.jobs.is_some())
            .field("auth", &self.auth.is_some())
            .field("metrics", &self.metrics.is_some())
            .field("diagnostics", &self.diagnostics)
            .field("events", &self.events.snapshot().items.len())
            .finish()
    }
}

impl ServerState {
    /// Builds state and resolves an optional bridge token from its env reference.
    pub async fn from_settings(
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
        let jobs = if settings.jobs.enabled {
            Some(JobManager::open(Arc::clone(&runtime), settings.jobs.clone()).await?)
        } else {
            None
        };
        Ok(Self {
            runtime,
            jobs,
            auth,
            metrics,
            diagnostics: Arc::new(ConfigurationDiagnostics::from_settings(settings)),
            events: Arc::new(OperatorEvents::default()),
        })
    }

    /// Builds server state with redaction-safe effective configuration provenance.
    pub async fn from_resolved(
        runtime: Arc<ImagegenRuntime>,
        resolved: &ResolvedConfig,
    ) -> Result<Self, BridgeError> {
        let mut state = Self::from_settings(runtime, &resolved.config.server).await?;
        state.diagnostics = Arc::new(ConfigurationDiagnostics::from_resolved(resolved));
        Ok(state)
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
        let authentication_required = token.is_some();
        let auth = token.and_then(|token| {
            use secrecy::ExposeSecret as _;
            AuthPolicy::new(token.expose_secret().to_owned())
        });
        let metrics = metrics_enabled.then(|| Arc::new(ServerMetrics::default()));
        Self {
            runtime,
            jobs: None,
            auth,
            metrics,
            diagnostics: Arc::new(ConfigurationDiagnostics::embedded(
                authentication_required,
                metrics_enabled,
            )),
            events: Arc::new(OperatorEvents::default()),
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
        .route("/v1/diagnostics", get(operator_diagnostics))
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
    if state.jobs.is_some() {
        protected = protected
            .route("/v1/jobs", post(create_job).get(list_jobs))
            .route("/v1/jobs/{id}/partial", get(get_job_partial))
            .route(
                "/v1/jobs/{id}",
                get(get_job).delete(cancel_job).patch(update_job),
            );
    }
    if state.runtime.has_artifact_store() {
        protected = protected
            .route("/v1/artifacts/{id}", get(get_artifact))
            .route("/v1/artifacts/{id}/thumbnail", get(get_artifact_thumbnail));
    }
    let protected = protected.route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    let mut public = Router::new()
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .route(
            "/v1/openapi.json",
            get(|| async { Json(openapi_document()) }),
        );
    if state.jobs.is_some() {
        public = public.merge(dashboard_router());
    }
    public
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
        .layer(middleware::from_fn_with_state(state.clone(), request_id))
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

async fn request_id(
    State(state): State<ServerState>,
    mut request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let request_id = RequestId::new();
    let route = redacted_route(request.uri().path());
    let method = request.method().clone();
    let started = Instant::now();
    request.extensions_mut().insert(request_id.clone());
    let mut response = next.run(request).await;
    if let Some(route) = route {
        state
            .events
            .record(&method, route, response.status(), started.elapsed());
    }
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

#[derive(Serialize)]
struct RuntimeDiagnostics {
    global_queued: usize,
    providers_queued: std::collections::BTreeMap<String, usize>,
}

#[derive(Serialize)]
struct OperatorDiagnostics {
    bridge_version: &'static str,
    configuration: ConfigurationDiagnostics,
    artifact_storage_enabled: bool,
    runtime: RuntimeDiagnostics,
    #[serde(skip_serializing_if = "Option::is_none")]
    jobs: Option<crate::JobManagerDiagnostics>,
    providers: Vec<imagegen_bridge_runtime::ProviderReadiness>,
    events: OperatorEventHistory,
}

async fn operator_diagnostics(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<OperatorDiagnostics>, ApiError> {
    let queue = state.runtime.queue_snapshot();
    let jobs = match state.jobs.as_ref() {
        Some(jobs) => Some(
            jobs.diagnostics()
                .await
                .map_err(|error| ApiError::from_bridge(error, request_id))?,
        ),
        None => None,
    };
    Ok(Json(OperatorDiagnostics {
        bridge_version: env!("CARGO_PKG_VERSION"),
        configuration: state.diagnostics.as_ref().clone(),
        artifact_storage_enabled: state.runtime.has_artifact_store(),
        runtime: RuntimeDiagnostics {
            global_queued: queue.global_queued,
            providers_queued: queue.providers_queued,
        },
        jobs,
        providers: state.runtime.registry().readiness().await,
        events: state.events.snapshot(),
    }))
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

async fn create_job(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    payload: Result<Json<ImageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ImageJob>), ApiError> {
    let Json(request) = payload.map_err(|_| {
        ApiError::bad_request(
            "request body must be valid ImageRequest JSON",
            request_id.clone(),
        )
    })?;
    let manager = state.jobs.as_ref().ok_or_else(|| {
        ApiError::from_bridge(
            BridgeError::new(ErrorCode::Configuration, "durable jobs are disabled"),
            request_id.clone(),
        )
    })?;
    manager
        .submit(request)
        .await
        .map(|job| (StatusCode::ACCEPTED, Json(job)))
        .map_err(|error| ApiError::from_bridge(error, request_id))
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct JobQuery {
    limit: usize,
    cursor: Option<String>,
    status: Option<ImageJobStatus>,
    visibility: Option<JobVisibilityQuery>,
    favorite: Option<bool>,
    search: Option<String>,
    /// Deprecated compatibility alias for `visibility=all`.
    include_deleted: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobVisibilityQuery {
    Active,
    Hidden,
    All,
}

impl From<JobVisibilityQuery> for ImageJobVisibility {
    fn from(value: JobVisibilityQuery) -> Self {
        match value {
            JobVisibilityQuery::Active => Self::Active,
            JobVisibilityQuery::Hidden => Self::Hidden,
            JobVisibilityQuery::All => Self::All,
        }
    }
}

impl Default for JobQuery {
    fn default() -> Self {
        Self {
            limit: default_page_size(),
            cursor: None,
            status: None,
            visibility: None,
            favorite: None,
            search: None,
            include_deleted: false,
        }
    }
}

async fn list_jobs(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    query: Result<Query<JobQuery>, QueryRejection>,
) -> Result<Json<ImageJobPage>, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request("job query parameters are invalid", request_id.clone())
    })?;
    if query.limit == 0 || query.limit > MAX_PAGE_SIZE {
        return Err(ApiError::bad_request(
            "job page limit must be between 1 and 100",
            request_id,
        ));
    }
    if query.include_deleted && query.visibility.is_some() {
        return Err(ApiError::bad_request(
            "include_deleted cannot be combined with visibility",
            request_id,
        ));
    }
    let search = query
        .search
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if search.as_ref().is_some_and(|value| {
        value.len() > MAX_JOB_SEARCH_BYTES || value.chars().any(char::is_control)
    }) {
        return Err(ApiError::bad_request(
            "job search must contain at most 512 bytes and no control characters",
            request_id,
        ));
    }
    let before = query
        .cursor
        .as_deref()
        .map(decode_job_cursor)
        .transpose()
        .map_err(|message| ApiError::bad_request(message, request_id.clone()))?;
    let manager = state.jobs.as_ref().ok_or_else(|| {
        ApiError::from_bridge(
            BridgeError::new(ErrorCode::Configuration, "durable jobs are disabled"),
            request_id.clone(),
        )
    })?;
    let mut items = manager
        .list(ImageJobListFilter {
            before,
            limit: query.limit.saturating_add(1),
            visibility: query.visibility.map_or_else(
                || {
                    if query.include_deleted {
                        ImageJobVisibility::All
                    } else {
                        ImageJobVisibility::Active
                    }
                },
                Into::into,
            ),
            status: query.status,
            favorite: query.favorite,
            search,
        })
        .await
        .map_err(|error| ApiError::from_bridge(error, request_id.clone()))?;
    let has_more = items.len() > query.limit;
    items.truncate(query.limit);
    let next_cursor = has_more
        .then(|| {
            items
                .last()
                .map(|item| encode_job_cursor(item.created, &item.id))
        })
        .flatten();
    Ok(Json(ImageJobPage { items, next_cursor }))
}

async fn get_job(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Path(id): Path<String>,
) -> Result<Json<ImageJob>, ApiError> {
    let manager = state.jobs.as_ref().ok_or_else(|| {
        ApiError::from_bridge(
            BridgeError::new(ErrorCode::Configuration, "durable jobs are disabled"),
            request_id.clone(),
        )
    })?;
    manager
        .get(&id)
        .await
        .map(Json)
        .map_err(|error| job_api_error(error, request_id))
}

async fn get_job_partial(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let manager = state.jobs.as_ref().ok_or_else(|| {
        ApiError::from_bridge(
            BridgeError::new(ErrorCode::Configuration, "durable jobs are disabled"),
            request_id.clone(),
        )
    })?;
    let preview = manager.partial_preview(&id).await.ok_or_else(|| {
        ApiError::bad_request("partial preview is not available", request_id)
            .with_status(StatusCode::NOT_FOUND)
    })?;
    let mut response = Response::new(axum::body::Body::from(preview.bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(preview.content_type),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        "x-image-output-index",
        HeaderValue::from_str(&preview.output_index.to_string())
            .unwrap_or(HeaderValue::from_static("0")),
    );
    response.headers_mut().insert(
        "x-image-partial-index",
        HeaderValue::from_str(&preview.partial_index.to_string())
            .unwrap_or(HeaderValue::from_static("0")),
    );
    Ok(response)
}

async fn cancel_job(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Path(id): Path<String>,
) -> Result<Json<ImageJob>, ApiError> {
    let manager = state.jobs.as_ref().ok_or_else(|| {
        ApiError::from_bridge(
            BridgeError::new(ErrorCode::Configuration, "durable jobs are disabled"),
            request_id.clone(),
        )
    })?;
    manager
        .cancel(&id)
        .await
        .map(Json)
        .map_err(|error| job_api_error(error, request_id))
}

async fn update_job(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Path(id): Path<String>,
    payload: Result<Json<ImageJobUpdate>, JsonRejection>,
) -> Result<Json<ImageJob>, ApiError> {
    let Json(update) = payload.map_err(|_| {
        ApiError::bad_request(
            "request body must be a valid ImageJobUpdate JSON object",
            request_id.clone(),
        )
    })?;
    let manager = state.jobs.as_ref().ok_or_else(|| {
        ApiError::from_bridge(
            BridgeError::new(ErrorCode::Configuration, "durable jobs are disabled"),
            request_id.clone(),
        )
    })?;
    manager
        .update_history(&id, update)
        .await
        .map(Json)
        .map_err(|error| job_api_error(error, request_id))
}

async fn get_artifact(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let runtime = Arc::clone(&state.runtime);
    let artifact = tokio::task::spawn_blocking(move || runtime.read_artifact(&id))
        .await
        .map_err(|_| {
            ApiError::from_bridge(
                BridgeError::new(ErrorCode::Internal, "artifact delivery task failed"),
                request_id.clone(),
            )
        })?
        .map_err(|error| artifact_api_error(error, request_id.clone()))?;
    let content_type = match artifact.metadata.format {
        imagegen_bridge_core::OutputFormat::Png => "image/png",
        imagegen_bridge_core::OutputFormat::Jpeg => "image/jpeg",
        imagegen_bridge_core::OutputFormat::Webp => "image/webp",
    };
    artifact_response(
        artifact.bytes,
        content_type,
        &artifact.metadata.sha256,
        artifact.name.rsplit('/').next(),
        request_id,
    )
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThumbnailQuery {
    edge: u32,
}

impl Default for ThumbnailQuery {
    fn default() -> Self {
        Self { edge: 384 }
    }
}

async fn get_artifact_thumbnail(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Path(id): Path<String>,
    query: Result<Query<ThumbnailQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request("thumbnail query parameters are invalid", request_id.clone())
    })?;
    if !(32..=2_048).contains(&query.edge) {
        return Err(ApiError::bad_request(
            "thumbnail edge must be between 32 and 2048 pixels",
            request_id,
        ));
    }
    let runtime = Arc::clone(&state.runtime);
    let artifact_id = id.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        runtime.read_artifact_thumbnail(&artifact_id, query.edge)
    })
    .await
    .map_err(|_| {
        ApiError::from_bridge(
            BridgeError::new(ErrorCode::Internal, "thumbnail delivery task failed"),
            request_id.clone(),
        )
    })?
    .map_err(|error| artifact_api_error(error, request_id.clone()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let filename = format!("thumbnail-{id}.png");
    artifact_response(bytes, "image/png", &digest, Some(&filename), request_id)
}

fn artifact_response(
    bytes: Vec<u8>,
    content_type: &'static str,
    digest: &str,
    filename: Option<&str>,
    request_id: RequestId,
) -> Result<Response, ApiError> {
    let etag = HeaderValue::from_str(&format!("\"{digest}\"")).map_err(|_| {
        ApiError::from_bridge(
            BridgeError::new(ErrorCode::Internal, "artifact checksum header is invalid"),
            request_id.clone(),
        )
    })?;
    let disposition = HeaderValue::from_str(&format!(
        "inline; filename=\"{}\"",
        filename.unwrap_or("image")
    ))
    .map_err(|_| {
        ApiError::from_bridge(
            BridgeError::new(ErrorCode::Internal, "artifact filename header is invalid"),
            request_id,
        )
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=31536000, immutable"),
            ),
            (header::ETAG, etag),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        bytes,
    )
        .into_response())
}

fn job_api_error(error: BridgeError, request_id: RequestId) -> ApiError {
    let not_found = error.code == ErrorCode::InvalidRequest
        && error
            .details
            .get("resource")
            .and_then(serde_json::Value::as_str)
            == Some("job");
    let error = ApiError::from_bridge(error, request_id);
    if not_found {
        error.with_status(StatusCode::NOT_FOUND)
    } else {
        error
    }
}

fn artifact_api_error(error: BridgeError, request_id: RequestId) -> ApiError {
    ApiError::from_bridge(error, request_id).with_status(StatusCode::NOT_FOUND)
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

fn encode_job_cursor(created: u64, id: &str) -> String {
    encode_cursor(&format!("{created}:{id}"))
}

fn decode_job_cursor(value: &str) -> Result<(u64, String), &'static str> {
    let decoded = decode_cursor(value).map_err(|_| "job cursor is malformed")?;
    let (created, id) = decoded.split_once(':').ok_or("job cursor is malformed")?;
    let created = created
        .parse::<u64>()
        .map_err(|_| "job cursor is malformed")?;
    if Uuid::parse_str(id).is_err() {
        return Err("job cursor is malformed");
    }
    Ok((created, id.to_owned()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use async_trait::async_trait;
    use axum::{body::Body, http::Request};
    use base64::engine::general_purpose::STANDARD;
    use http_body_util::BodyExt as _;
    use imagegen_bridge_core::{
        Background, GeneratedImage, ImageAction, ImagePayload, ImageProvider, ImageResponse,
        InputCapabilities, InputFidelity, Moderation, OutputFormat, ProviderCapabilities,
        ProviderContext, Quality, ResponseFormat, SizeCapabilities, SupportLevel, Timings, U8Range,
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
                models: vec!["test-image".to_owned()],
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
                        b64_json: if request.prompt == "invalid partial preview" {
                            "not-base64".to_owned()
                        } else {
                            ONE_PIXEL_PNG.to_owned()
                        },
                    })
                    .await
                    .unwrap();
                if request.prompt == "partial preview test" {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
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
                    metadata_name: None,
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

    async fn test_job_router(directory: &std::path::Path) -> (Router, Arc<JobManager>) {
        let registry =
            ProviderRegistry::new([Arc::new(ReadyProvider) as Arc<dyn ImageProvider>], "ready")
                .unwrap();
        let mut config = imagegen_bridge_config::BridgeConfig::default();
        config.artifacts.root = directory.join("artifacts");
        config.server.jobs.database = directory.join("jobs.sqlite3");
        let mut runtime_config = config.runtime_config().unwrap();
        runtime_config.materialization.artifact_store = Some(config.artifact_store().unwrap());
        let runtime = Arc::new(ImagegenRuntime::new(registry, runtime_config).unwrap());
        let state = ServerState::from_settings(runtime, &config.server)
            .await
            .unwrap();
        let jobs = state.jobs.clone().unwrap();
        (router(state, &config.server), jobs)
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
        let body = authorized.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["items"][0]["models"][0], "test-image");
    }

    #[tokio::test]
    async fn protected_api_rejects_cross_origin_browser_requests() {
        let app = test_router(Some("bridge-secret"));
        let rejected = app
            .clone()
            .oneshot(
                Request::get("/v1/providers")
                    .header(header::HOST, "bridge.local:8787")
                    .header(header::ORIGIN, "https://attacker.example")
                    .header("sec-fetch-site", "cross-site")
                    .header(header::AUTHORIZATION, "Bearer bridge-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        let body = rejected.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], "permission_denied");
        assert_eq!(
            value["error"]["message"],
            "cross-origin browser requests are not allowed"
        );

        let accepted = app
            .oneshot(
                Request::get("/v1/providers")
                    .header(header::HOST, "bridge.local:8787")
                    .header(header::ORIGIN, "http://bridge.local:8787")
                    .header("sec-fetch-site", "same-origin")
                    .header(header::AUTHORIZATION, "Bearer bridge-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
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
    async fn durable_job_routes_execute_list_and_return_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let (app, jobs) = test_job_router(directory.path()).await;
        let created = app
            .clone()
            .oneshot(
                Request::post("/v1/jobs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&ImageRequest::generate("durable test")).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = created.status();
        let body = created.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let created: ImageJob = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            created.request.output.response_format,
            ResponseFormat::Artifact
        );

        let id = created.summary.id;
        let completed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let response = app
                    .clone()
                    .oneshot(
                        Request::get(format!("/v1/jobs/{id}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let job: ImageJob = serde_json::from_slice(&body).unwrap();
                if job.summary.status.terminal() {
                    break job;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(completed.summary.status, ImageJobStatus::Succeeded);
        let result = completed.result.unwrap();
        let artifact_id = result
            .data
            .iter()
            .find_map(|image| match &image.payload {
                ImagePayload::Artifact { id, .. } => Some(id.clone()),
                _ => None,
            })
            .unwrap();

        let artifact = app
            .clone()
            .oneshot(
                Request::get(format!("/v1/artifacts/{artifact_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(artifact.status(), StatusCode::OK);
        assert_eq!(artifact.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(
            artifact.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );

        let thumbnail = app
            .clone()
            .oneshot(
                Request::get(format!("/v1/artifacts/{artifact_id}/thumbnail?edge=128"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(thumbnail.status(), StatusCode::OK);
        assert_eq!(thumbnail.headers()[header::CONTENT_TYPE], "image/png");

        let listed = app
            .clone()
            .oneshot(
                Request::get("/v1/jobs?limit=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let body = listed.into_body().collect().await.unwrap().to_bytes();
        let page: ImageJobPage = serde_json::from_slice(&body).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, id);

        let updated = app
            .clone()
            .oneshot(
                Request::patch(format!("/v1/jobs/{id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"favorite":true,"deleted":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(updated.status(), StatusCode::OK);
        let body = updated.into_body().collect().await.unwrap().to_bytes();
        let updated: ImageJob = serde_json::from_slice(&body).unwrap();
        assert!(updated.summary.favorite);
        assert!(updated.summary.deleted.is_some());

        let hidden = app
            .clone()
            .oneshot(Request::get("/v1/jobs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = hidden.into_body().collect().await.unwrap().to_bytes();
        let hidden: ImageJobPage = serde_json::from_slice(&body).unwrap();
        assert!(hidden.items.is_empty());

        let filtered = app
            .clone()
            .oneshot(
                Request::get("/v1/jobs?visibility=hidden&favorite=true&search=DURABLE%20TEST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(filtered.status(), StatusCode::OK);
        let body = filtered.into_body().collect().await.unwrap().to_bytes();
        let filtered: ImageJobPage = serde_json::from_slice(&body).unwrap();
        assert_eq!(filtered.items.len(), 1);
        assert_eq!(filtered.items[0].id, id);

        let conflicting = app
            .clone()
            .oneshot(
                Request::get("/v1/jobs?visibility=hidden&include_deleted=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(conflicting.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let restored = app
            .clone()
            .oneshot(
                Request::patch(format!("/v1/jobs/{id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"deleted":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(restored.status(), StatusCode::OK);

        let missing = app
            .oneshot(
                Request::get("/v1/jobs/019f0000-0000-7000-8000-000000000000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn durable_jobs_expose_only_verified_transient_partial_previews() {
        let directory = tempfile::tempdir().unwrap();
        let (app, jobs) = test_job_router(directory.path()).await;
        let mut request = ImageRequest::generate("partial preview test");
        request.parameters.partial_images = 1;
        let created = app
            .clone()
            .oneshot(
                Request::post("/v1/jobs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::ACCEPTED);
        let body = created.into_body().collect().await.unwrap().to_bytes();
        let created: ImageJob = serde_json::from_slice(&body).unwrap();
        let id = created.summary.id;

        let preview = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let response = app
                    .clone()
                    .oneshot(
                        Request::get(format!("/v1/jobs/{id}/partial"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                if response.status() == StatusCode::OK {
                    break response;
                }
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(preview.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(
            preview.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        assert_eq!(preview.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(preview.headers()["x-image-output-index"], "0");
        assert_eq!(preview.headers()["x-image-partial-index"], "0");
        let bytes = preview.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(STANDARD.encode(bytes), ONE_PIXEL_PNG);

        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        let expired = app
            .oneshot(
                Request::get(format!("/v1/jobs/{id}/partial"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(expired.status(), StatusCode::NOT_FOUND);
        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn invalid_partial_preview_does_not_discard_a_verified_final_result() {
        let directory = tempfile::tempdir().unwrap();
        let (app, jobs) = test_job_router(directory.path()).await;
        let mut request = ImageRequest::generate("invalid partial preview");
        request.parameters.partial_images = 1;
        let created = app
            .clone()
            .oneshot(
                Request::post("/v1/jobs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = created.into_body().collect().await.unwrap().to_bytes();
        let created: ImageJob = serde_json::from_slice(&body).unwrap();
        let id = created.summary.id;
        let completed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let response = app
                    .clone()
                    .oneshot(
                        Request::get(format!("/v1/jobs/{id}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let job: ImageJob = serde_json::from_slice(&body).unwrap();
                if job.summary.status.terminal() {
                    break job;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(completed.summary.status, ImageJobStatus::Succeeded);
        assert!(completed.result.is_some());
        assert_eq!(completed.summary.progress.unwrap().partial_images, 1);
        let preview = app
            .oneshot(
                Request::get(format!("/v1/jobs/{id}/partial"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(preview.status(), StatusCode::NOT_FOUND);
        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn embedded_dashboard_is_available_only_with_durable_jobs() {
        let directory = tempfile::tempdir().unwrap();
        let (app, jobs) = test_job_router(directory.path()).await;
        for (path, content_type) in [
            ("/dashboard", "text/html; charset=utf-8"),
            ("/dashboard/", "text/html; charset=utf-8"),
            ("/dashboard/app.css", "text/css; charset=utf-8"),
            ("/dashboard/app.js", "text/javascript; charset=utf-8"),
            ("/dashboard/api.js", "text/javascript; charset=utf-8"),
            ("/dashboard/form.js", "text/javascript; charset=utf-8"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        }
        let disabled = test_router(None)
            .oneshot(Request::get("/dashboard").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(disabled.status(), StatusCode::NOT_FOUND);
        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn diagnostics_are_authenticated_aggregate_and_redaction_safe() {
        let directory = tempfile::tempdir().unwrap();
        let (app, jobs) = test_job_router(directory.path()).await;
        let _ = app
            .clone()
            .oneshot(
                Request::get("/v1/sessions/private-session-key?provider=private-provider")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let response = app
            .oneshot(Request::get("/v1/diagnostics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["bridge_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["configuration"]["listener_scope"], "loopback");
        assert_eq!(value["configuration"]["jobs_enabled"], true);
        assert_eq!(value["artifact_storage_enabled"], true);
        assert_eq!(value["jobs"]["total"], 0);
        assert_eq!(value["jobs"]["active_workers"], 0);
        assert!(value["jobs"]["database_bytes"].as_u64().unwrap() > 0);
        assert_eq!(value["providers"][0]["provider"], "ready");
        assert_eq!(value["events"]["capacity"], 256);
        assert_eq!(value["events"]["dropped"], 0);
        assert_eq!(value["events"]["items"][0]["route"], "/v1/sessions/{key}");
        assert_eq!(value["events"]["items"][0]["method"], "GET");
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains(directory.path().to_str().unwrap()));
        assert!(!body.contains("prompt"));
        assert!(!body.contains("token"));
        assert!(!body.contains("private-session-key"));
        assert!(!body.contains("private-provider"));
        jobs.shutdown().await;

        let protected = test_router(Some("bridge-secret"))
            .oneshot(Request::get("/v1/diagnostics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
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
        assert!(body.contains(&format!("\"b64_json\":\"{ONE_PIXEL_PNG}\"")));
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
