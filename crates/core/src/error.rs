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
    /// Ordered, redaction-safe recovery actions suitable for humans and automation.
    #[serde(default, skip_serializing_if = "suggestions_are_empty")]
    pub suggestions: Box<[String]>,
}

fn suggestions_are_empty(value: &[String]) -> bool {
    value.is_empty()
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
            suggestions: default_suggestions(code)
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    /// Creates a safety rejection with stable, non-bypass recovery guidance.
    #[must_use]
    pub fn safety_rejected(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::SafetyRejected, message)
            .with_detail("safety_category", "content_policy")
            .with_detail("recovery", "revise_prompt_or_inputs")
            .with_detail("retry_same_request", false)
            .with_detail("safety_controls_relaxed", false)
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

    /// Adds one concrete, redaction-safe recovery action.
    #[must_use]
    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        let suggestion = suggestion.into();
        if !suggestion.is_empty() && !self.suggestions.contains(&suggestion) {
            let mut suggestions = self.suggestions.into_vec();
            suggestions.push(suggestion);
            self.suggestions = suggestions.into_boxed_slice();
        }
        self
    }
}

const fn default_suggestions(code: ErrorCode) -> &'static [&'static str] {
    match code {
        ErrorCode::InvalidRequest => &[
            "Inspect the reported field/details and compare the request with `imagegen-bridge providers capabilities`.",
        ],
        ErrorCode::UnsupportedCapability => &[
            "Run `imagegen-bridge providers capabilities --json` and remove or change unsupported parameters.",
        ],
        ErrorCode::Configuration => &[
            "Run `imagegen-bridge config check` to list every invalid field and its source.",
            "Run `imagegen-bridge doctor` after correcting the configuration.",
        ],
        ErrorCode::Authentication => &[
            "Run `imagegen-bridge auth-doctor` and refresh the provider login if it is expired or missing.",
        ],
        ErrorCode::PermissionDenied => &[
            "Verify the bridge bearer scope and the provider account entitlement for this operation.",
        ],
        ErrorCode::SafetyRejected => {
            &["Revise the prompt or input images; retrying the unchanged request will not help."]
        }
        ErrorCode::RateLimited => &[
            "Honor Retry-After when present; if rate limits persist, set an explicit lower provider concurrency instead of `unlimited`/`auto`.",
        ],
        ErrorCode::Overloaded => &[
            "Inspect `imagegen-bridge diagnostics` for the exhausted gate or open circuit.",
            "For a configured admission gate, increase its capacity or set `max_concurrent = \"unlimited\"`; keep a finite value only when you want backpressure.",
        ],
        ErrorCode::Timeout => &[
            "Compare provider and queue timing, then increase `runtime.default_timeout_ms` and `runtime.request.max_timeout_ms` if the provider is still making progress.",
        ],
        ErrorCode::Cancelled => {
            &["Retry only if the caller intentionally cancelled before provider dispatch."]
        }
        ErrorCode::Upstream => &[
            "Use the request ID with provider/bridge logs, honor `retryable`, and run `imagegen-bridge doctor` if failures persist.",
        ],
        ErrorCode::Protocol => &[
            "Run `imagegen-bridge update check`; if already current, capture the request ID and provider diagnostics for a compatibility report.",
        ],
        ErrorCode::Input => &[
            "Verify that every input is readable, within configured byte/pixel limits, and under an allowed local or remote source.",
        ],
        ErrorCode::Artifact => &[
            "Check artifact-root permissions, free space, and configured image/response size ceilings.",
        ],
        ErrorCode::Session => &[
            "List existing sessions or retry with an isolated session when conversational continuity is not required.",
        ],
        ErrorCode::IdempotencyConflict => {
            &["Reuse an idempotency key only with the identical request, or generate a new key."]
        }
        ErrorCode::Internal => &[
            "Run `imagegen-bridge doctor` and `imagegen-bridge update check`, then correlate the failure with its request ID.",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_rejections_include_actionable_non_bypass_guidance() {
        let error = BridgeError::safety_rejected("request rejected");
        assert_eq!(error.code, ErrorCode::SafetyRejected);
        assert!(!error.retryable);
        assert_eq!(error.details["recovery"], "revise_prompt_or_inputs");
        assert_eq!(error.details["retry_same_request"], false);
        assert_eq!(error.details["safety_controls_relaxed"], false);
        assert!(!error.suggestions.is_empty());
    }
}
