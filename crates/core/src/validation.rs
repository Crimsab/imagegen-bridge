//! Intrinsic request validation independent of provider capabilities.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    BridgeError, ErrorCode, ImageOperation, ImageRequest, ImageSource, NegativePromptMode,
    OutputFormat, ResponseFormat, SessionMode,
};

/// Configurable limits applied before provider negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestLimits {
    /// Maximum UTF-8 prompt bytes.
    pub max_prompt_bytes: usize,
    /// Maximum UTF-8 negative-prompt bytes.
    pub max_negative_prompt_bytes: usize,
    /// Generic maximum output count before provider-specific validation.
    pub max_outputs: u8,
    /// Generic maximum input image count.
    pub max_inputs: usize,
    /// Maximum inline encoded characters before decoding.
    pub max_inline_encoded_bytes: usize,
    /// Maximum explicit edge before provider-specific validation.
    pub max_edge: u32,
    /// Maximum request timeout.
    pub max_timeout_ms: u64,
    /// Maximum user-controlled identifier bytes.
    pub max_identifier_bytes: usize,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_prompt_bytes: 128 * 1024,
            max_negative_prompt_bytes: 64 * 1024,
            max_outputs: 16,
            max_inputs: 16,
            max_inline_encoded_bytes: 64 * 1024 * 1024,
            max_edge: 16_384,
            max_timeout_ms: 30 * 60 * 1_000,
            max_identifier_bytes: 256,
        }
    }
}

/// One intrinsic validation issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidationIssue {
    /// JSON-style field path.
    pub field: String,
    /// Stable issue code.
    pub code: String,
    /// Human-readable redaction-safe explanation.
    pub message: String,
}

/// Validates a request before any provider or I/O work occurs.
pub fn validate_request(request: &ImageRequest, limits: RequestLimits) -> Result<(), BridgeError> {
    let issues = validation_issues(request, limits);
    if issues.is_empty() {
        return Ok(());
    }
    Err(
        BridgeError::new(ErrorCode::InvalidRequest, "request validation failed")
            .with_detail("issues", issues),
    )
}

/// Returns all intrinsic issues in deterministic field order.
#[must_use]
pub fn validation_issues(request: &ImageRequest, limits: RequestLimits) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let mut issue = |field: &str, code: &str, message: &str| {
        issues.push(ValidationIssue {
            field: field.to_owned(),
            code: code.to_owned(),
            message: message.to_owned(),
        });
    };

    if request.version != crate::CONTRACT_VERSION {
        issue(
            "version",
            "unsupported_version",
            "only contract version 1 is supported",
        );
    }
    if request.prompt.trim().is_empty() {
        issue("prompt", "required", "prompt must not be empty");
    } else if request.prompt.len() > limits.max_prompt_bytes {
        issue(
            "prompt",
            "too_large",
            "prompt exceeds the configured byte limit",
        );
    }
    if let Some(negative_prompt) = &request.negative_prompt {
        if negative_prompt.trim().is_empty() {
            issue(
                "negative_prompt",
                "empty",
                "negative prompt must be omitted instead of empty",
            );
        } else if negative_prompt.len() > limits.max_negative_prompt_bytes {
            issue(
                "negative_prompt",
                "too_large",
                "negative prompt exceeds the configured byte limit",
            );
        }
        if request.policies.negative_prompt == NegativePromptMode::Reject {
            issue(
                "policies.negative_prompt",
                "rejected_by_policy",
                "negative prompt is present while its policy is reject",
            );
        }
    }

    if request.parameters.n == 0 || request.parameters.n > limits.max_outputs {
        issue(
            "parameters.n",
            "out_of_range",
            "output count is outside the configured generic range",
        );
    }
    if let Some((width, height)) = request.parameters.size.dimensions()
        && (width > limits.max_edge || height > limits.max_edge)
    {
        issue(
            "parameters.size",
            "out_of_range",
            "an image edge exceeds the configured generic limit",
        );
    }
    if !request.parameters.size.is_auto()
        && (request.parameters.aspect_ratio.is_some() || request.parameters.resolution.is_some())
    {
        issue(
            "parameters.size",
            "ambiguous",
            "explicit size cannot be combined with aspect_ratio or resolution hints",
        );
    }
    if request
        .parameters
        .output_compression
        .is_some_and(|value| value > 100)
    {
        issue(
            "parameters.output_compression",
            "out_of_range",
            "output compression must be between 0 and 100",
        );
    }
    if request.parameters.output_compression.is_some()
        && !matches!(
            request.parameters.output_format,
            OutputFormat::Jpeg | OutputFormat::Webp
        )
    {
        issue(
            "parameters.output_compression",
            "incompatible",
            "output compression is valid only for jpeg or webp",
        );
    }
    if request.parameters.partial_images > 3 {
        issue(
            "parameters.partial_images",
            "out_of_range",
            "partial image count must be between 0 and 3",
        );
    }

    validate_identifier(
        request.routing.provider.as_deref(),
        "routing.provider",
        limits,
        &mut issue,
    );
    validate_identifier(
        request.routing.model.as_deref(),
        "routing.model",
        limits,
        &mut issue,
    );
    validate_identifier(
        request.idempotency_key.as_deref(),
        "idempotency_key",
        limits,
        &mut issue,
    );
    validate_identifier(request.user.as_deref(), "user", limits, &mut issue);

    match request.session.mode {
        SessionMode::Isolated => {
            if request.session.key.is_some() || request.session.thread_id.is_some() {
                issue(
                    "session",
                    "incompatible",
                    "isolated mode cannot include a key or thread_id",
                );
            }
        }
        SessionMode::Persistent => {
            if request.session.key.as_deref().is_none_or(str::is_empty) {
                issue(
                    "session.key",
                    "required",
                    "persistent mode requires a non-empty session key",
                );
            }
            if request.session.thread_id.is_some() {
                issue(
                    "session.thread_id",
                    "incompatible",
                    "persistent mode uses a key, not an explicit thread_id",
                );
            }
        }
        SessionMode::Thread => {
            if request
                .session
                .thread_id
                .as_deref()
                .is_none_or(str::is_empty)
            {
                issue(
                    "session.thread_id",
                    "required",
                    "thread mode requires a non-empty thread_id",
                );
            }
            if request.session.key.is_some() {
                issue(
                    "session.key",
                    "incompatible",
                    "thread mode cannot include a persistent key",
                );
            }
        }
    }
    validate_identifier(
        request.session.key.as_deref(),
        "session.key",
        limits,
        &mut issue,
    );
    validate_identifier(
        request.session.thread_id.as_deref(),
        "session.thread_id",
        limits,
        &mut issue,
    );

    if let Some(timeout_ms) = request.timeout_ms
        && (timeout_ms == 0 || timeout_ms > limits.max_timeout_ms)
    {
        issue(
            "timeout_ms",
            "out_of_range",
            "timeout is outside the configured range",
        );
    }

    if request.output.response_format != ResponseFormat::Artifact
        && request.output.filename_prefix.is_some()
    {
        issue(
            "output.filename_prefix",
            "incompatible",
            "filename_prefix requires artifact response format",
        );
    }

    let (edit_images, mask, references) = match &request.operation {
        ImageOperation::Generate { reference_images } => (0, false, reference_images.as_slice()),
        ImageOperation::Edit {
            images,
            mask,
            reference_images,
        } => {
            if images.is_empty() {
                issue(
                    "images",
                    "required",
                    "edit operation requires at least one source image",
                );
            }
            for (index, input) in images.iter().enumerate() {
                validate_input(input, &format!("images[{index}]"), limits, &mut issue);
            }
            if let Some(mask) = mask {
                validate_input(mask, "mask", limits, &mut issue);
            }
            (images.len(), mask.is_some(), reference_images.as_slice())
        }
    };
    for (index, input) in references.iter().enumerate() {
        validate_input(
            input,
            &format!("reference_images[{index}]"),
            limits,
            &mut issue,
        );
    }
    let input_count = edit_images + usize::from(mask) + references.len();
    if input_count > limits.max_inputs {
        issue(
            "operation",
            "too_many_inputs",
            "total input image count exceeds the configured limit",
        );
    }

    issues.sort_by(|left, right| {
        left.field
            .cmp(&right.field)
            .then(left.code.cmp(&right.code))
    });
    issues
}

fn validate_identifier(
    value: Option<&str>,
    field: &str,
    limits: RequestLimits,
    issue: &mut impl FnMut(&str, &str, &str),
) {
    let Some(value) = value else { return };
    if value.trim().is_empty() {
        issue(
            field,
            "empty",
            "identifier must be omitted instead of empty",
        );
    } else if value.len() > limits.max_identifier_bytes {
        issue(
            field,
            "too_large",
            "identifier exceeds the configured byte limit",
        );
    } else if value.chars().any(char::is_control) {
        issue(
            field,
            "invalid",
            "identifier must not contain control characters",
        );
    }
}

fn validate_input(
    input: &crate::ImageInput,
    field: &str,
    limits: RequestLimits,
    issue: &mut impl FnMut(&str, &str, &str),
) {
    match &input.source {
        ImageSource::File { path } if path.as_os_str().is_empty() => {
            issue(field, "empty", "file path must not be empty");
        }
        ImageSource::Url { url } if url.trim().is_empty() => {
            issue(field, "empty", "URL must not be empty");
        }
        ImageSource::DataUrl { data_url } if data_url.len() > limits.max_inline_encoded_bytes => {
            issue(
                field,
                "too_large",
                "data URL exceeds the encoded byte limit",
            );
        }
        ImageSource::DataUrl { data_url } if !data_url.starts_with("data:image/") => {
            issue(
                field,
                "invalid",
                "data URL must contain an image media type",
            );
        }
        ImageSource::Base64 { data } if data.len() > limits.max_inline_encoded_bytes => {
            issue(
                field,
                "too_large",
                "base64 input exceeds the encoded byte limit",
            );
        }
        ImageSource::Base64 { data } if data.is_empty() => {
            issue(field, "empty", "base64 input must not be empty");
        }
        _ => {}
    }
    if input.filename.as_deref().is_some_and(|name| {
        name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.chars().any(char::is_control)
    }) {
        issue(
            field,
            "invalid_filename",
            "logical filename must be a single safe path component",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ImageInput, SessionOptions};

    #[test]
    fn default_generation_request_is_valid() {
        assert!(
            validate_request(
                &ImageRequest::generate("a lighthouse"),
                RequestLimits::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn validation_accumulates_and_sorts_issues() {
        let mut request = ImageRequest::generate("  ");
        request.parameters.n = 0;
        request.parameters.output_compression = Some(80);
        request.timeout_ms = Some(0);

        let issues = validation_issues(&request, RequestLimits::default());
        let fields: Vec<_> = issues.iter().map(|item| item.field.as_str()).collect();
        assert_eq!(
            fields,
            [
                "parameters.n",
                "parameters.output_compression",
                "prompt",
                "timeout_ms"
            ]
        );
    }

    #[test]
    fn persistent_session_requires_only_a_key() {
        let mut request = ImageRequest::generate("test");
        request.session = SessionOptions {
            mode: SessionMode::Persistent,
            key: None,
            thread_id: Some("thread-1".to_owned()),
        };
        let issues = validation_issues(&request, RequestLimits::default());
        assert!(issues.iter().any(|item| item.field == "session.key"));
        assert!(issues.iter().any(|item| item.field == "session.thread_id"));
    }

    #[test]
    fn edit_requires_an_image() {
        let mut request = ImageRequest::generate("edit it");
        request.operation = ImageOperation::Edit {
            images: Vec::new(),
            mask: None,
            reference_images: Vec::new(),
        };
        assert!(
            validation_issues(&request, RequestLimits::default())
                .iter()
                .any(|item| item.field == "images")
        );
    }

    #[test]
    fn unsafe_logical_filename_is_rejected() {
        let mut request = ImageRequest::generate("test");
        request.operation = ImageOperation::Generate {
            reference_images: vec![ImageInput {
                source: ImageSource::Base64 {
                    data: "AA==".to_owned(),
                },
                media_type: Some("image/png".to_owned()),
                filename: Some("../escape.png".to_owned()),
            }],
        };
        assert!(
            validation_issues(&request, RequestLimits::default())
                .iter()
                .any(|item| item.code == "invalid_filename")
        );
    }
}
