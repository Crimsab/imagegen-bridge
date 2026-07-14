//! Intrinsic request validation independent of provider capabilities.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactCollisionPolicy, BridgeError, ErrorCode, ImageAction, ImageOperation, ImageRequest,
    ImageSource, NegativePromptMode, OutputFormat, ResponseFormat, SessionMode,
};

const MAX_EMBEDDED_REQUEST_TEXT_BYTES: usize = 12 * 1024;
const MAX_VALIDATION_ISSUES: usize = 64;
const MAX_DETAILED_INPUT_VALIDATIONS: usize = 64;

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
    let mut issues = Vec::with_capacity(MAX_VALIDATION_ISSUES);
    let mut issues_truncated = false;
    let mut issue = |field: &str, code: &str, message: &str| {
        if issues.len() < MAX_VALIDATION_ISSUES - 1 {
            issues.push(ValidationIssue {
                field: field.to_owned(),
                code: code.to_owned(),
                message: message.to_owned(),
            });
        } else {
            issues_truncated = true;
        }
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
    if request.routing.fallbacks.len() > 4 {
        issue(
            "routing.fallbacks",
            "too_many",
            "at most four fallback routes may be configured",
        );
    }
    let mut fallback_names = std::collections::BTreeSet::new();
    for (index, route) in request.routing.fallbacks.iter().enumerate() {
        validate_identifier(
            Some(&route.provider),
            &format!("routing.fallbacks[{index}].provider"),
            limits,
            &mut issue,
        );
        validate_identifier(
            route.model.as_deref(),
            &format!("routing.fallbacks[{index}].model"),
            limits,
            &mut issue,
        );
        if !fallback_names.insert((&route.provider, route.model.as_deref())) {
            issue(
                &format!("routing.fallbacks[{index}]"),
                "duplicate",
                "fallback routes must be unique",
            );
        }
        if request.routing.provider.as_deref() == Some(route.provider.as_str())
            && request.routing.model.as_deref() == route.model.as_deref()
        {
            issue(
                &format!("routing.fallbacks[{index}]"),
                "duplicate",
                "a fallback route must differ from the explicit primary route",
            );
        }
    }
    if !request.routing.fallbacks.is_empty() && request.session.mode != SessionMode::Isolated {
        issue(
            "routing.fallbacks",
            "incompatible",
            "provider fallback is supported only for isolated sessions",
        );
    }
    if request.policies.batch_execution == crate::BatchExecution::Parallel
        && request.session.mode != SessionMode::Isolated
    {
        issue(
            "policies.batch_execution",
            "incompatible",
            "parallel batch execution requires an isolated session",
        );
    }
    validate_identifier(
        request.idempotency_key.as_deref(),
        "idempotency_key",
        limits,
        &mut issue,
    );
    validate_identifier(request.user.as_deref(), "user", limits, &mut issue);

    if request.output.transparency.transparent_threshold
        >= request.output.transparency.opaque_threshold
    {
        issue(
            "output.transparency.transparent_threshold",
            "out_of_range",
            "transparent threshold must be lower than opaque threshold",
        );
    }
    if let Some(key) = request.output.transparency.key_color.as_deref()
        && !valid_hex_color(key)
    {
        issue(
            "output.transparency.key_color",
            "invalid_format",
            "chroma key must be a hex RGB color like #00ff00",
        );
    }

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

    let artifact_delivery = matches!(
        request.output.response_format,
        ResponseFormat::Artifact | ResponseFormat::Url
    );
    if !artifact_delivery && request.output.filename_prefix.is_some() {
        issue(
            "output.filename_prefix",
            "incompatible",
            "filename_prefix requires artifact response format",
        );
    }
    if !artifact_delivery
        && (request.output.directory.is_some() || request.output.filename.is_some())
    {
        issue(
            "output",
            "incompatible",
            "artifact path controls require artifact or url response format",
        );
    }
    if request.output.filename.is_some() && request.output.filename_prefix.is_some() {
        issue(
            "output.filename",
            "incompatible",
            "filename cannot be combined with filename_prefix",
        );
    }
    if request.output.filename.is_some() && request.parameters.n != 1 {
        issue(
            "output.filename",
            "incompatible",
            "an exact filename requires a single output",
        );
    }
    if request.output.filename.is_none()
        && request.output.collision != ArtifactCollisionPolicy::Error
    {
        issue(
            "output.collision",
            "incompatible",
            "collision policy applies only to an explicit filename",
        );
    }
    if request.output.metadata.writes_sidecar()
        && request.output.response_format != ResponseFormat::Artifact
    {
        issue(
            "output.metadata",
            "incompatible",
            "metadata sidecars require artifact response format",
        );
    }
    if request.output.metadata.embeds()
        && request.output.response_format == ResponseFormat::Metadata
    {
        issue(
            "output.metadata",
            "incompatible",
            "embedded metadata requires an image-bearing response format",
        );
    }
    if request.output.metadata.embeds()
        && request
            .prompt
            .len()
            .saturating_add(request.negative_prompt.as_deref().map_or(0, str::len))
            > MAX_EMBEDDED_REQUEST_TEXT_BYTES
    {
        issue(
            "output.metadata",
            "too_large",
            "embedded metadata requires combined prompt text no larger than 12 KiB",
        );
    }
    if let Some(directory) = request.output.directory.as_deref()
        && !valid_portable_directory(directory)
    {
        issue(
            "output.directory",
            "invalid",
            "output directory must be a safe portable relative path",
        );
    }
    if let Some(filename) = request.output.filename.as_deref()
        && !valid_output_filename(filename, request.parameters.output_format)
    {
        issue(
            "output.filename",
            "invalid",
            "output filename must be a safe component with a matching image extension",
        );
    }

    let (edit_images, mask, references) = match &request.operation {
        ImageOperation::Generate { reference_images } => (0, None, reference_images.as_slice()),
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
            (images.len(), mask.as_ref(), reference_images.as_slice())
        }
    };
    let input_count = edit_images
        .saturating_add(usize::from(mask.is_some()))
        .saturating_add(references.len());
    if input_count > limits.max_inputs {
        issue(
            "operation",
            "too_many_inputs",
            "total input image count exceeds the configured limit",
        );
    }
    let mut detailed_inputs = 0_usize;
    if let ImageOperation::Edit { images, .. } = &request.operation {
        for (index, input) in images
            .iter()
            .take(MAX_DETAILED_INPUT_VALIDATIONS)
            .enumerate()
        {
            validate_input(input, &format!("images[{index}]"), limits, &mut issue);
            detailed_inputs += 1;
        }
    }
    if let Some(mask) = mask
        && detailed_inputs < MAX_DETAILED_INPUT_VALIDATIONS
    {
        validate_input(mask, "mask", limits, &mut issue);
        detailed_inputs += 1;
    }
    for (index, input) in references
        .iter()
        .take(MAX_DETAILED_INPUT_VALIDATIONS.saturating_sub(detailed_inputs))
        .enumerate()
    {
        validate_input(
            input,
            &format!("reference_images[{index}]"),
            limits,
            &mut issue,
        );
    }
    if request.parameters.input_fidelity.is_some() && edit_images + references.len() == 0 {
        issue(
            "parameters.input_fidelity",
            "incompatible",
            "input_fidelity requires at least one source or reference image",
        );
    }
    match (&request.operation, request.parameters.action) {
        (ImageOperation::Generate { reference_images }, ImageAction::Edit)
            if reference_images.is_empty() =>
        {
            issue(
                "parameters.action",
                "incompatible",
                "edit action requires an image in context",
            );
        }
        (ImageOperation::Edit { .. }, ImageAction::Generate) => issue(
            "parameters.action",
            "incompatible",
            "generate action conflicts with the edit operation",
        ),
        _ => {}
    }

    if issues_truncated {
        issues.push(ValidationIssue {
            field: "$".to_owned(),
            code: "too_many_issues".to_owned(),
            message: "additional validation issues were omitted".to_owned(),
        });
    }
    issues.sort_by(|left, right| {
        left.field
            .cmp(&right.field)
            .then(left.code.cmp(&right.code))
    });
    issues
}

fn valid_hex_color(value: &str) -> bool {
    let value = value.strip_prefix('#').unwrap_or(value);
    value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_portable_directory(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && component.len() <= 128
                && !component.starts_with('.')
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn valid_output_filename(value: &str, format: OutputFormat) -> bool {
    if value.is_empty()
        || value.len() > 160
        || value.starts_with('.')
        || value.contains(['/', '\\'])
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return false;
    }
    let Some((_, extension)) = value.rsplit_once('.') else {
        return true;
    };
    match format {
        OutputFormat::Png => extension.eq_ignore_ascii_case("png"),
        OutputFormat::Jpeg => {
            extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg")
        }
        OutputFormat::Webp => extension.eq_ignore_ascii_case("webp"),
    }
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
    use crate::{ArtifactMetadataPolicy, ImageInput, InputFidelity, SessionOptions};

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
    fn validation_bounds_issue_and_input_amplification() {
        let invalid = ImageInput {
            source: ImageSource::Base64 {
                data: String::new(),
            },
            media_type: None,
            filename: Some("../invalid.png".to_owned()),
        };
        let mut request = ImageRequest::generate("test");
        request.operation = ImageOperation::Edit {
            images: vec![invalid; 10_000],
            mask: None,
            reference_images: Vec::new(),
        };

        let issues = validation_issues(&request, RequestLimits::default());
        assert_eq!(issues.len(), MAX_VALIDATION_ISSUES);
        assert!(issues.iter().any(|item| item.code == "too_many_inputs"));
        assert!(issues.iter().any(|item| item.code == "too_many_issues"));
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

    #[test]
    fn fidelity_and_edit_action_require_image_context() {
        let mut request = ImageRequest::generate("edit it");
        request.parameters.input_fidelity = Some(InputFidelity::High);
        request.parameters.action = ImageAction::Edit;
        let issues = validation_issues(&request, RequestLimits::default());
        assert!(
            issues
                .iter()
                .any(|item| item.field == "parameters.input_fidelity")
        );
        assert!(issues.iter().any(|item| item.field == "parameters.action"));
    }

    #[test]
    fn artifact_paths_are_portable_and_match_the_image_format() {
        let mut request = ImageRequest::generate("test");
        request.output.response_format = ResponseFormat::Artifact;
        request.output.directory = Some("../outside".to_owned());
        request.output.filename = Some("portrait.jpg".to_owned());
        let issues = validation_issues(&request, RequestLimits::default());
        assert!(issues.iter().any(|item| item.field == "output.directory"));
        assert!(issues.iter().any(|item| item.field == "output.filename"));
    }

    #[test]
    fn exact_filename_requires_one_image_and_controls_collision() {
        let mut request = ImageRequest::generate("test");
        request.output.response_format = ResponseFormat::Artifact;
        request.output.filename = Some("portrait.png".to_owned());
        request.parameters.n = 2;
        assert!(
            validation_issues(&request, RequestLimits::default())
                .iter()
                .any(|item| item.field == "output.filename" && item.code == "incompatible")
        );

        request.output.filename = None;
        request.parameters.n = 1;
        request.output.collision = ArtifactCollisionPolicy::Suffix;
        assert!(
            validation_issues(&request, RequestLimits::default())
                .iter()
                .any(|item| item.field == "output.collision")
        );
    }

    #[test]
    fn metadata_sidecars_require_artifact_delivery() {
        let mut request = ImageRequest::generate("test");
        request.output.metadata = ArtifactMetadataPolicy::Sidecar;
        assert!(
            validation_issues(&request, RequestLimits::default())
                .iter()
                .any(|item| item.field == "output.metadata")
        );
        request.output.response_format = ResponseFormat::Artifact;
        assert!(validate_request(&request, RequestLimits::default()).is_ok());

        request.output.metadata = ArtifactMetadataPolicy::SidecarAndEmbedded;
        assert!(validate_request(&request, RequestLimits::default()).is_ok());
    }

    #[test]
    fn embedded_metadata_requires_image_bytes_but_not_artifact_delivery() {
        let mut request = ImageRequest::generate("test");
        request.output.metadata = ArtifactMetadataPolicy::Embedded;
        assert!(validate_request(&request, RequestLimits::default()).is_ok());

        request.output.response_format = ResponseFormat::Metadata;
        assert!(
            validation_issues(&request, RequestLimits::default())
                .iter()
                .any(|item| item.field == "output.metadata")
        );

        request.output.response_format = ResponseFormat::B64Json;
        request.prompt = "x".repeat(MAX_EMBEDDED_REQUEST_TEXT_BYTES + 1);
        assert!(
            validation_issues(&request, RequestLimits::default())
                .iter()
                .any(|item| item.field == "output.metadata" && item.code == "too_large")
        );
    }

    #[test]
    fn validates_fallback_isolation_and_transparency_controls() {
        let mut request = ImageRequest::generate("test");
        request.routing.fallbacks.push(crate::ProviderRoute {
            provider: "fallback".to_owned(),
            model: Some("gpt-image-2".to_owned()),
        });
        request.session.mode = SessionMode::Persistent;
        request.session.key = Some("character".to_owned());
        request.output.transparency.key_color = Some("not-a-color".to_owned());
        request.output.transparency.transparent_threshold = 100;
        request.output.transparency.opaque_threshold = 50;
        let issues = validation_issues(&request, RequestLimits::default());
        assert!(
            issues
                .iter()
                .any(|item| item.field == "routing.fallbacks" && item.code == "incompatible")
        );
        assert!(
            issues
                .iter()
                .any(|item| item.field == "output.transparency.key_color")
        );
        assert!(
            issues
                .iter()
                .any(|item| { item.field == "output.transparency.transparent_threshold" })
        );
    }
}
