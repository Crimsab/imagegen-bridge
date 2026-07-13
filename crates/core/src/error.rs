//! Stable error types shared by providers and transports.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable machine-readable error classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A request failed intrinsic validation.
    InvalidRequest,
    /// The selected provider cannot honor the request.
    UnsupportedCapability,
    /// Configuration is absent or invalid.
    Configuration,
    /// Provider authentication is absent, expired, or rejected.
    Authentication,
    /// The caller is authenticated but not entitled to the operation.
    PermissionDenied,
    /// A safety system rejected the request.
    SafetyRejected,
    /// A provider or bridge rate limit was reached.
    RateLimited,
    /// Admission control rejected the request because capacity is exhausted.
    Overloaded,
    /// The operation exceeded its deadline.
    Timeout,
    /// The caller or runtime cancelled the operation.
    Cancelled,
    /// An upstream provider returned an invalid or unsuccessful response.
    Upstream,
    /// The upstream protocol no longer matches the supported schema.
    Protocol,
    /// An input could not be loaded or validated.
    Input,
    /// A generated artifact could not be verified or published.
    Artifact,
    /// A requested session does not exist or cannot be accessed.
    Session,
    /// An idempotency key conflicts with a different request.
    IdempotencyConflict,
    /// An unexpected internal failure occurred.
    Internal,
}

/// Public, redaction-safe error returned by the bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Error)]
#[error("{code:?}: {message}")]
#[serde(deny_unknown_fields)]
pub struct BridgeError {
    /// Stable machine-readable code.
    pub code: ErrorCode,
    /// Human-readable message safe to show to a client.
    pub message: String,
    /// Whether retrying later may succeed without changing the request.
    pub retryable: bool,
    /// Optional provider name, never a credential or account identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Safe provider correlation ID when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
    /// Structured, redaction-safe diagnostic values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, serde_json::Value>,
}

impl BridgeError {
    /// Creates a non-retryable error without provider details.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            provider: None,
            upstream_request_id: None,
            details: BTreeMap::new(),
        }
    }

    /// Marks the error as potentially retryable.
    #[must_use]
    pub const fn retryable(mut self, value: bool) -> Self {
        self.retryable = value;
        self
    }

    /// Attaches a safe provider name.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Attaches a redaction-safe structured detail.
    #[must_use]
    pub fn with_detail(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        if let Ok(value) = serde_json::to_value(value) {
            self.details.insert(key.into(), value);
        }
        self
    }
}
