//! Honest HTTP status projection for stable bridge errors.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use imagegen_bridge_core::{BridgeError, ErrorCode};
use serde::Serialize;
use serde_json::Value;

use crate::RequestId;

/// Canonical JSON error envelope returned by every HTTP route.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorEnvelope {
    /// OpenAI-compatible error object with namespaced bridge detail.
    pub error: CompatibleError,
    /// Safe bridge-generated correlation ID.
    pub request_id: String,
}

/// OpenAI-compatible public error fields.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibleError {
    /// Human-readable safe explanation.
    pub message: String,
    /// Broad OpenAI-compatible error family.
    #[serde(rename = "type")]
    pub error_type: &'static str,
    /// Request field associated with the failure when one is known.
    pub param: Option<String>,
    /// Stable programmatic error discriminator.
    pub code: &'static str,
    /// Complete provider-neutral extension for native clients.
    pub imagegen_bridge: BridgeErrorExtension,
}

/// Stable bridge taxonomy retained beside the compatibility fields.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeErrorExtension {
    /// Original stable bridge error code.
    pub code: ErrorCode,
    /// Whether an unchanged request may succeed if retried later.
    pub retryable: bool,
    /// Safe provider name when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Safe upstream correlation ID when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
    /// Redaction-safe structured bridge diagnostics.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub details: std::collections::BTreeMap<String, Value>,
}

/// One response-ready error with explicit HTTP semantics.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    envelope: Box<ErrorEnvelope>,
}

impl ApiError {
    pub(crate) fn from_bridge(error: BridgeError, request_id: RequestId) -> Self {
        let param = error
            .details
            .get("field")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let error_type = compatible_type(error.code);
        let code = compatible_code(error.code);
        let compatible = CompatibleError {
            message: error.message,
            error_type,
            param,
            code,
            imagegen_bridge: BridgeErrorExtension {
                code: error.code,
                retryable: error.retryable,
                provider: error.provider,
                upstream_request_id: error.upstream_request_id,
                details: error.details,
            },
        };
        Self {
            status: status_for(error.code),
            envelope: Box::new(ErrorEnvelope {
                error: compatible,
                request_id: request_id.0,
            }),
        }
    }

    pub(crate) fn bad_request(message: &str, request_id: RequestId) -> Self {
        Self::from_bridge(
            BridgeError::new(ErrorCode::InvalidRequest, message),
            request_id,
        )
    }

    pub(crate) fn authentication(request_id: RequestId) -> Self {
        Self::from_bridge(
            BridgeError::new(ErrorCode::Authentication, "bridge authentication required"),
            request_id,
        )
    }

    pub(crate) fn browser_origin_forbidden(request_id: RequestId) -> Self {
        Self::from_bridge(
            BridgeError::new(
                ErrorCode::PermissionDenied,
                "cross-origin browser requests are not allowed",
            ),
            request_id,
        )
    }

    pub(crate) const fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    pub(crate) fn envelope(&self) -> ErrorEnvelope {
        (*self.envelope).clone()
    }
}

const fn compatible_type(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidRequest
        | ErrorCode::UnsupportedCapability
        | ErrorCode::Input
        | ErrorCode::Session
        | ErrorCode::IdempotencyConflict => "invalid_request_error",
        ErrorCode::Authentication => "authentication_error",
        ErrorCode::PermissionDenied => "permission_denied_error",
        ErrorCode::SafetyRejected => "image_generation_user_error",
        ErrorCode::RateLimited => "rate_limit_error",
        ErrorCode::Overloaded => "server_overloaded_error",
        ErrorCode::Timeout | ErrorCode::Cancelled => "request_timeout_error",
        ErrorCode::Configuration
        | ErrorCode::Upstream
        | ErrorCode::Protocol
        | ErrorCode::Artifact
        | ErrorCode::Internal => "api_error",
    }
}

const fn compatible_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidRequest => "invalid_request",
        ErrorCode::UnsupportedCapability => "unsupported_capability",
        ErrorCode::Configuration => "configuration",
        ErrorCode::Authentication => "authentication_required",
        ErrorCode::PermissionDenied => "permission_denied",
        ErrorCode::SafetyRejected => "moderation_blocked",
        ErrorCode::RateLimited => "rate_limited",
        ErrorCode::Overloaded => "overloaded",
        ErrorCode::Timeout => "timeout",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::Upstream => "upstream_error",
        ErrorCode::Protocol => "protocol_error",
        ErrorCode::Input => "invalid_image_input",
        ErrorCode::Artifact => "artifact_error",
        ErrorCode::Session => "session_not_found",
        ErrorCode::IdempotencyConflict => "idempotency_conflict",
        ErrorCode::Internal => "internal_error",
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let retry_after_seconds = self
            .envelope
            .error
            .imagegen_bridge
            .details
            .get("retry_after_ms")
            .and_then(Value::as_u64)
            .map_or(1, |milliseconds| milliseconds.saturating_add(999) / 1_000)
            .max(1);
        let mut response = (self.status, Json(*self.envelope)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"imagegen-bridge\""),
            );
        }
        if matches!(
            self.status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        ) && let Ok(value) = HeaderValue::from_str(&retry_after_seconds.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

const fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::InvalidRequest | ErrorCode::SafetyRejected => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::UnsupportedCapability | ErrorCode::Input => StatusCode::BAD_REQUEST,
        ErrorCode::Configuration | ErrorCode::Internal | ErrorCode::Artifact => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        ErrorCode::Authentication => StatusCode::UNAUTHORIZED,
        ErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
        ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
        ErrorCode::Cancelled => StatusCode::REQUEST_TIMEOUT,
        ErrorCode::Upstream | ErrorCode::Protocol => StatusCode::BAD_GATEWAY,
        ErrorCode::Session => StatusCode::NOT_FOUND,
        ErrorCode::IdempotencyConflict => StatusCode::CONFLICT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_compatibility_fields_without_losing_bridge_taxonomy() {
        let error = BridgeError::new(ErrorCode::SafetyRejected, "request was blocked")
            .with_detail("field", "prompt")
            .with_provider("codex-app-server");
        let projected = ApiError::from_bridge(error, RequestId("request-1".to_owned(), None));
        let envelope = projected.envelope();
        assert_eq!(envelope.error.error_type, "image_generation_user_error");
        assert_eq!(envelope.error.code, "moderation_blocked");
        assert_eq!(envelope.error.param.as_deref(), Some("prompt"));
        assert_eq!(
            envelope.error.imagegen_bridge.code,
            ErrorCode::SafetyRejected
        );
        assert_eq!(envelope.request_id, "request-1");
    }

    #[test]
    fn circuit_cooldown_sets_a_ceiling_retry_after() {
        let response = ApiError::from_bridge(
            BridgeError::new(ErrorCode::Overloaded, "circuit open")
                .retryable(true)
                .with_detail("retry_after_ms", 180_001_u64),
            RequestId("request-2".to_owned(), None),
        )
        .into_response();
        assert_eq!(response.headers()[header::RETRY_AFTER], "181");
    }
}
