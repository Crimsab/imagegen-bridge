//! Honest HTTP status projection for stable bridge errors.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use imagegen_bridge_core::{BridgeError, ErrorCode};
use serde::Serialize;

use crate::RequestId;

/// Canonical JSON error envelope returned by every HTTP route.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorEnvelope {
    /// Stable provider-neutral bridge error.
    pub error: BridgeError,
    /// Safe bridge-generated correlation ID.
    pub request_id: String,
}

/// One response-ready error with explicit HTTP semantics.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    envelope: Box<ErrorEnvelope>,
}

impl ApiError {
    pub(crate) fn from_bridge(error: BridgeError, request_id: RequestId) -> Self {
        Self {
            status: status_for(error.code),
            envelope: Box::new(ErrorEnvelope {
                error,
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

    pub(crate) const fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    pub(crate) fn envelope(&self) -> ErrorEnvelope {
        (*self.envelope).clone()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
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
        ) {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
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
