//! Shared black-box HTTP fixture server used by every non-Rust SDK test suite.

use std::{collections::HashMap, convert::Infallible};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use serde_json::{Value, json};

const REQUEST_ID: &str = "request_fixture_01";
const GENERATE_REQUEST: &str = include_str!("../../../fixtures/sdk/generate-request.json");
const EDIT_REQUEST: &str = include_str!("../../../fixtures/sdk/edit-request.json");
const IMAGE_RESPONSE: &str = include_str!("../../../fixtures/sdk/image-response.json");
const ERROR_RESPONSE: &str = include_str!("../../../fixtures/sdk/error-response.json");
const PROVIDERS_RESPONSE: &str = include_str!("../../../fixtures/sdk/providers-response.json");
const CAPABILITIES_RESPONSE: &str =
    include_str!("../../../fixtures/sdk/capabilities-response.json");
const SESSION_RESPONSE: &str = include_str!("../../../fixtures/sdk/session-response.json");
const IMAGE_STREAM: &str = include_str!("../../../fixtures/sdk/image-stream.sse");
const ONE_PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
const ARTIFACT_ID: &str = "019f0000-0000-7000-8000-000000000002";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    println!("{}", json!({"base_url": format!("http://{address}")}));
    let app = Router::new()
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .route("/v1/images", post(images))
        .route("/v1/images/stream", post(image_stream))
        .route("/v1/jobs", post(create_job).get(list_jobs))
        .route(
            "/v1/jobs/{id}",
            get(get_job).delete(cancel_job).patch(update_job),
        )
        .route("/v1/jobs/{id}/partial", get(job_partial))
        .route("/v1/presets", get(list_presets).post(create_preset))
        .route(
            "/v1/presets/{name}",
            get(get_preset).put(replace_preset).delete(delete_preset),
        )
        .route("/v1/providers", get(providers))
        .route("/v1/diagnostics", get(diagnostics))
        .route("/v1/providers/{provider}/capabilities", get(capabilities))
        .route("/v1/sessions/{key}", get(session).delete(delete_session))
        .route("/v1/artifacts/{id}", get(artifact))
        .route("/v1/artifacts/{id}/thumbnail", get(artifact))
        .merge(imagegen_bridge_server::dashboard_router());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn liveness() -> Json<Value> {
    Json(json!({"status": "live"}))
}

async fn readiness() -> Json<Value> {
    Json(json!({"status": "ready", "providers": []}))
}

fn preset_value(name: &str, description: Option<&Value>, template: Option<&Value>) -> Value {
    json!({
        "name": name,
        "description": description,
        "template": template.cloned().unwrap_or_else(|| json!({})),
        "created": 1_784_000_000_u64,
        "updated": 1_784_000_001_u64
    })
}

async fn list_presets(headers: HeaderMap) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    Json(json!({"items":[preset_value("portrait-high", Some(&json!("Editorial portrait")), Some(&json!({"operation":"generate"})))]})).into_response()
}

async fn create_preset(headers: HeaderMap, Json(payload): Json<Value>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "preset name is required");
    };
    (
        StatusCode::CREATED,
        Json(preset_value(
            name,
            payload.get("description"),
            payload.get("template"),
        )),
    )
        .into_response()
}

async fn get_preset(headers: HeaderMap, Path(name): Path<String>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    Json(preset_value(
        &name,
        Some(&json!("Editorial portrait")),
        Some(&json!({"operation":"generate"})),
    ))
    .into_response()
}

async fn replace_preset(
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    Json(preset_value(
        &name,
        payload.get("description"),
        payload.get("template"),
    ))
    .into_response()
}

async fn delete_preset(headers: HeaderMap, Path(_name): Path<String>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn diagnostics(headers: HeaderMap) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    Json(json!({
        "bridge_version": "0.1.3-test",
        "configuration": {
            "version": 1,
            "default_provider": "codex-responses",
            "listener_scope": "loopback",
            "authentication_required": true,
            "metrics_enabled": false,
            "jobs_enabled": true,
            "max_connections": 32,
            "max_body_bytes": 83_886_080,
            "read_timeout_ms": 0,
            "write_timeout_ms": 30000,
            "provenance": [
                {"field":"default_provider","source":"file","key":"default_provider"},
                {"field":"server.jobs.enabled","source":"default","key":"server.jobs.enabled"}
            ]
        },
        "artifact_storage_enabled": true,
        "runtime": {"global_queued": 0, "providers_queued": {"codex-responses": 0}},
        "jobs": {
            "total": 1,
            "queued": 0,
            "running": 0,
            "succeeded": 1,
            "failed": 0,
            "cancelled": 0,
            "interrupted": 0,
            "hidden": 0,
            "database_bytes": 40960,
            "logical_bytes": 8192,
            "active_workers": 0,
            "max_pending": 1000,
            "max_running": 4,
            "retention_secs": 604_800,
            "max_retained": 10000,
            "max_retained_bytes": 268_435_456,
            "max_database_bytes": 1_073_741_824
        },
        "providers": [
            {"provider":"codex-app-server","status":"ready"},
            {"provider":"codex-responses","status":"ready"}
        ],
        "events": {
            "capacity": 256,
            "dropped": 0,
            "items": [
                {"sequence":2,"timestamp_ms":1_783_960_001_000_u64,"method":"GET","route":"/v1/jobs","status":200,"duration_ms":3},
                {"sequence":1,"timestamp_ms":1_783_960_000_000_u64,"method":"POST","route":"/v1/images","status":200,"duration_ms":36}
            ]
        }
    }))
    .into_response()
}

async fn images(headers: HeaderMap, Json(request): Json<Value>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if request.get("prompt").and_then(Value::as_str) == Some("trigger-error") {
        return fixture_response(
            StatusCode::TOO_MANY_REQUESTS,
            ERROR_RESPONSE,
            "application/json",
        );
    }
    let fixture = if request.get("operation").and_then(Value::as_str) == Some("edit") {
        EDIT_REQUEST
    } else {
        GENERATE_REQUEST
    };
    let Ok(mut expected) = serde_json::from_str::<Value>(fixture) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "shared request fixture is invalid",
        );
    };
    let selected_provider = request
        .pointer("/routing/provider")
        .and_then(Value::as_str)
        .unwrap_or("codex-app-server");
    if !matches!(selected_provider, "codex-app-server" | "codex-responses") {
        return error(StatusCode::NOT_FOUND, "provider was not found");
    }
    expected["routing"]["provider"] = Value::String(selected_provider.to_owned());
    if request != expected
        || headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            != request.get("idempotency_key").and_then(Value::as_str)
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "request did not match the shared SDK fixture",
        );
    }
    let Ok(mut response) = serde_json::from_str::<Value>(IMAGE_RESPONSE) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "shared image fixture is invalid",
        );
    };
    response["provider"] = Value::String(selected_provider.to_owned());
    fixture_response(StatusCode::OK, &response.to_string(), "application/json")
}

async fn image_stream(headers: HeaderMap, Json(request): Json<Value>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if request.get("prompt").and_then(Value::as_str) == Some("trigger-error") {
        let Ok(error) = serde_json::from_str::<Value>(ERROR_RESPONSE) else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "shared error fixture is invalid",
            );
        };
        let body = format!("event: error\ndata: {error}\n\n");
        return fixture_response(StatusCode::OK, &body, "text/event-stream");
    }
    let midpoint = IMAGE_STREAM.len() / 2;
    let chunks = vec![
        Ok::<_, Infallible>(Bytes::copy_from_slice(&IMAGE_STREAM.as_bytes()[..midpoint])),
        Ok(Bytes::copy_from_slice(&IMAGE_STREAM.as_bytes()[midpoint..])),
    ];
    response_with_headers(
        StatusCode::OK,
        Body::from_stream(tokio_stream::iter(chunks)),
        "text/event-stream",
    )
}

async fn create_job(headers: HeaderMap, Json(request): Json<Value>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    let Ok(expected) = serde_json::from_str::<Value>(GENERATE_REQUEST) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "shared request fixture is invalid",
        );
    };
    let dashboard_request =
        serde_json::from_value::<imagegen_bridge_core::ImageRequest>(request.clone()).is_ok();
    if request != expected && !dashboard_request {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "job request did not match fixture",
        );
    }
    job_response(StatusCode::ACCEPTED, "queued", false)
}

async fn list_jobs(headers: HeaderMap, Query(_query): Query<Value>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    let Some(job) = job_value("succeeded", false) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "job fixture is invalid");
    };
    fixture_response(
        StatusCode::OK,
        &json!({"items": [job], "next_cursor": "sdk-next"}).to_string(),
        "application/json",
    )
}

async fn get_job(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if id != "019f0000-0000-7000-8000-000000000001" {
        return error(StatusCode::NOT_FOUND, "job was not found");
    }
    job_response(StatusCode::OK, "succeeded", true)
}

async fn cancel_job(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if id != "019f0000-0000-7000-8000-000000000001" {
        return error(StatusCode::NOT_FOUND, "job was not found");
    }
    job_response(StatusCode::OK, "cancelled", false)
}

async fn job_partial(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if id != "019f0000-0000-7000-8000-000000000001" {
        return error(StatusCode::NOT_FOUND, "partial preview is not available");
    }
    let Ok(bytes) = STANDARD.decode(ONE_PIXEL_PNG) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "partial fixture is invalid",
        );
    };
    response_with_headers(StatusCode::OK, Body::from(bytes), "image/png")
}

async fn update_job(
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(update): Json<Value>,
) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if id != "019f0000-0000-7000-8000-000000000001" {
        return error(StatusCode::NOT_FOUND, "job was not found");
    }
    let valid = update.as_object().is_some_and(|fields| {
        !fields.is_empty()
            && fields
                .keys()
                .all(|key| matches!(key.as_str(), "favorite" | "deleted"))
            && fields.values().all(Value::is_boolean)
    });
    if !valid {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "job update did not match fixture",
        );
    }
    let Some(mut job) = job_value("succeeded", true) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "job fixture is invalid");
    };
    if let Some(favorite) = update.get("favorite") {
        job["favorite"] = favorite.clone();
    }
    if update.get("deleted").and_then(Value::as_bool) == Some(true) {
        job["deleted"] = Value::Number(1_783_960_002_u64.into());
    }
    fixture_response(StatusCode::OK, &job.to_string(), "application/json")
}

fn job_response(status: StatusCode, job_status: &str, result: bool) -> Response {
    let Some(job) = job_value(job_status, result) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "job fixture is invalid");
    };
    fixture_response(status, &job.to_string(), "application/json")
}

fn job_value(status: &str, include_detail: bool) -> Option<Value> {
    let mut request = serde_json::from_str::<Value>(GENERATE_REQUEST).ok()?;
    request["output"]["response_format"] = Value::String("artifact".to_owned());
    let mut value = json!({
        "id": "019f0000-0000-7000-8000-000000000001",
        "status": status,
        "created": 1_783_960_000,
        "updated": 1_783_960_001,
        "favorite": false
    });
    if include_detail || matches!(status, "queued" | "cancelled") {
        value["request"] = request;
        value["cancel_requested"] = Value::Bool(status == "cancelled");
    }
    if include_detail {
        let mut result = serde_json::from_str::<Value>(IMAGE_RESPONSE).ok()?;
        result["id"] = value["id"].clone();
        result["data"][0] = json!({
            "type":"artifact",
            "id":ARTIFACT_ID,
            "name":"fixture.png",
            "index":0,
            "format":"png",
            "width":1,
            "height":1,
            "bytes":70,
            "sha256":"0000000000000000000000000000000000000000000000000000000000000000"
        });
        result["warnings"] = json!(["fixture_warning"]);
        value["result"] = result;
    }
    Some(value)
}

async fn providers(headers: HeaderMap, Query(_query): Query<Value>) -> Response {
    authenticate(&headers)
        .unwrap_or_else(|| fixture_response(StatusCode::OK, PROVIDERS_RESPONSE, "application/json"))
}

async fn capabilities(
    headers: HeaderMap,
    Path(provider): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if !matches!(provider.as_str(), "codex-app-server" | "codex-responses") {
        return error(StatusCode::NOT_FOUND, "provider was not found");
    }
    let Ok(mut capabilities) = serde_json::from_str::<Value>(CAPABILITIES_RESPONSE) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "capability fixture is invalid",
        );
    };
    capabilities["provider"] = Value::String(provider.clone());
    capabilities["experimental"] = Value::Bool(false);
    if let Some(model) = query.get("model").filter(|model| !model.is_empty()) {
        capabilities["model"] = Value::String(model.clone());
    }
    fixture_response(
        StatusCode::OK,
        &capabilities.to_string(),
        "application/json",
    )
}

async fn session(headers: HeaderMap, Path(key): Path<String>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if key != "sdk-fixture" {
        return error(StatusCode::NOT_FOUND, "session was not found");
    }
    fixture_response(StatusCode::OK, SESSION_RESPONSE, "application/json")
}

async fn delete_session(headers: HeaderMap, Path(key): Path<String>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if key != "sdk-fixture" {
        return error(StatusCode::NOT_FOUND, "session was not found");
    }
    response_with_headers(StatusCode::NO_CONTENT, Body::empty(), "application/json")
}

async fn artifact(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if id != ARTIFACT_ID {
        return error(StatusCode::NOT_FOUND, "artifact was not found");
    }
    let Ok(bytes) = STANDARD.decode(ONE_PIXEL_PNG) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "artifact fixture is invalid",
        );
    };
    response_with_headers(StatusCode::OK, Body::from(bytes), "image/png")
}

fn authenticate(headers: &HeaderMap) -> Option<Response> {
    (headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some("Bearer sdk-test-token"))
    .then(|| error(StatusCode::UNAUTHORIZED, "bridge authentication required"))
}

fn fixture_response(status: StatusCode, body: &str, content_type: &'static str) -> Response {
    response_with_headers(status, Body::from(body.to_owned()), content_type)
}

fn response_with_headers(status: StatusCode, body: Body, content_type: &'static str) -> Response {
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert("x-request-id", HeaderValue::from_static(REQUEST_ID));
    response
}

fn error(status: StatusCode, message: &str) -> Response {
    fixture_response(
        status,
        &json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": null,
                "code": "invalid_request",
                "imagegen_bridge": {"code": "invalid_request", "retryable": false}
            },
            "request_id": REQUEST_ID
        })
        .to_string(),
        "application/json",
    )
}

#[cfg(test)]
mod tests {
    use imagegen_bridge_core::{
        ImageJob, ImageJobSummary, ImageRequest, ImageResponse, ProviderCapabilities,
        SessionMetadata,
    };

    use super::*;

    #[test]
    fn shared_fixtures_match_the_rust_wire_contract() {
        assert!(serde_json::from_str::<ImageRequest>(GENERATE_REQUEST).is_ok());
        let edit = serde_json::from_str::<ImageRequest>(EDIT_REQUEST);
        assert!(edit.is_ok(), "{edit:?}");
        let response = serde_json::from_str::<ImageResponse>(IMAGE_RESPONSE);
        assert!(response.is_ok(), "{response:?}");
        let capabilities = serde_json::from_str::<ProviderCapabilities>(CAPABILITIES_RESPONSE);
        assert!(capabilities.is_ok(), "{capabilities:?}");
        assert!(serde_json::from_str::<SessionMetadata>(SESSION_RESPONSE).is_ok());
        assert!(serde_json::from_str::<Value>(ERROR_RESPONSE).is_ok());
        assert!(
            job_value("queued", false)
                .is_some_and(|value| serde_json::from_value::<ImageJob>(value).is_ok())
        );
        assert!(
            job_value("succeeded", false)
                .is_some_and(|value| serde_json::from_value::<ImageJobSummary>(value).is_ok())
        );
    }
}
