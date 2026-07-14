//! Shared black-box HTTP fixture server used by every non-Rust SDK test suite.

use std::convert::Infallible;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
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
        .route("/v1/jobs/{id}", get(get_job).delete(cancel_job))
        .route("/v1/providers", get(providers))
        .route("/v1/providers/{provider}/capabilities", get(capabilities))
        .route("/v1/sessions/{key}", get(session).delete(delete_session));
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
    if request != expected {
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
            "id":"019f0000-0000-7000-8000-000000000002",
            "name":"fixture.png",
            "index":0,
            "format":"png",
            "width":1,
            "height":1,
            "bytes":70,
            "sha256":"0000000000000000000000000000000000000000000000000000000000000000"
        });
        value["result"] = result;
    }
    Some(value)
}

async fn providers(headers: HeaderMap, Query(_query): Query<Value>) -> Response {
    authenticate(&headers)
        .unwrap_or_else(|| fixture_response(StatusCode::OK, PROVIDERS_RESPONSE, "application/json"))
}

async fn capabilities(headers: HeaderMap, Path(provider): Path<String>) -> Response {
    if let Some(response) = authenticate(&headers) {
        return response;
    }
    if provider != "codex-app-server" {
        return error(StatusCode::NOT_FOUND, "provider was not found");
    }
    fixture_response(StatusCode::OK, CAPABILITIES_RESPONSE, "application/json")
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
        ImageRequest, ImageResponse, ProviderCapabilities, SessionMetadata,
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
    }
}
