//! Capability negotiation with explicit, machine-readable normalization.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Background, BridgeError, CompatibilityMode, ErrorCode, GenerationParameters, ImageOperation,
    ImageRequest, ImageSize, Moderation, NegativePromptMode, Normalization, OutputFormat,
    ProviderCapabilities, Quality, RevisedPromptPolicy, SessionMode, SupportLevel,
};

/// Request after provider capability negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NegotiatedRequest {
    /// Original generation parameters.
    pub requested: GenerationParameters,
    /// Request containing effective parameters and prompt policy transforms.
    pub effective_request: ImageRequest,
    /// Every explicit transformation applied during negotiation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub normalizations: Vec<Normalization>,
}

/// Negotiates one intrinsically valid request against provider capabilities.
///
/// # Errors
///
/// Returns [`ErrorCode::UnsupportedCapability`] when an operation or parameter
/// cannot be honored under the request's compatibility policy.
pub fn negotiate_request(
    request: &ImageRequest,
    capabilities: &ProviderCapabilities,
) -> Result<NegotiatedRequest, BridgeError> {
    capabilities.validate()?;
    let requested = request.parameters.clone();
    let mut effective_request = request.clone();
    let mut normalizations = Vec::new();
    let mode = request.policies.compatibility;

    check_model(request, capabilities)?;
    check_operation(request, capabilities)?;
    check_input_counts(request, capabilities)?;
    check_sessions(request, capabilities)?;
    check_user_attribution(request, capabilities)?;
    check_input_fidelity(request, capabilities)?;
    check_action(request, capabilities)?;
    negotiate_count(&mut effective_request, capabilities, &mut normalizations)?;
    negotiate_size(&mut effective_request, capabilities, &mut normalizations)?;
    negotiate_hints(&mut effective_request, capabilities, &mut normalizations)?;
    negotiate_quality(&mut effective_request, capabilities, &mut normalizations)?;
    negotiate_format(&mut effective_request, capabilities, &mut normalizations)?;
    negotiate_background(&mut effective_request, capabilities, &mut normalizations)?;
    negotiate_moderation(&mut effective_request, capabilities, &mut normalizations)?;
    negotiate_partial_images(&mut effective_request, capabilities, &mut normalizations)?;
    negotiate_negative_prompt(&mut effective_request, capabilities, &mut normalizations)?;

    if request.policies.revised_prompt == RevisedPromptPolicy::Require
        && capabilities.revised_prompt == SupportLevel::Unsupported
    {
        return Err(unsupported(
            capabilities,
            "policies.revised_prompt",
            "provider cannot return a revised prompt",
        ));
    }

    debug_assert_eq!(mode, effective_request.policies.compatibility);
    Ok(NegotiatedRequest {
        requested,
        effective_request,
        normalizations,
    })
}

fn check_user_attribution(
    request: &ImageRequest,
    capabilities: &ProviderCapabilities,
) -> Result<(), BridgeError> {
    if request.user.is_none() || capabilities.user_attribution != SupportLevel::Unsupported {
        return Ok(());
    }
    Err(unsupported(
        capabilities,
        "user",
        "provider cannot consume the requested end-user attribution",
    ))
}

fn check_input_fidelity(
    request: &ImageRequest,
    capabilities: &ProviderCapabilities,
) -> Result<(), BridgeError> {
    let Some(requested) = request.parameters.input_fidelity else {
        return Ok(());
    };
    if capabilities.input_fidelities.contains(&requested) {
        return Ok(());
    }
    Err(unsupported(
        capabilities,
        "parameters.input_fidelity",
        "provider cannot honor the requested input fidelity",
    )
    .with_detail("requested", requested)
    .with_detail("supported", &capabilities.input_fidelities))
}

fn check_action(
    request: &ImageRequest,
    capabilities: &ProviderCapabilities,
) -> Result<(), BridgeError> {
    let requested = request.parameters.action;
    if capabilities.actions.contains(&requested) {
        return Ok(());
    }
    Err(unsupported(
        capabilities,
        "parameters.action",
        "provider cannot honor the requested image action",
    )
    .with_detail("requested", requested)
    .with_detail("supported", &capabilities.actions))
}

fn check_model(
    request: &ImageRequest,
    capabilities: &ProviderCapabilities,
) -> Result<(), BridgeError> {
    let (Some(requested), Some(effective)) = (
        request.routing.model.as_deref(),
        capabilities.model.as_deref(),
    ) else {
        return Ok(());
    };
    if requested == effective {
        return Ok(());
    }
    Err(unsupported(
        capabilities,
        "routing.model",
        "provider cannot honor the requested image model",
    )
    .with_detail("requested_model", requested)
    .with_detail("effective_model", effective))
}

fn check_operation(
    request: &ImageRequest,
    capabilities: &ProviderCapabilities,
) -> Result<(), BridgeError> {
    let (supported, field, message) = match request.operation {
        ImageOperation::Generate { .. } => (
            capabilities.generation,
            "operation",
            "provider does not support image generation",
        ),
        ImageOperation::Edit { .. } => (
            capabilities.edits,
            "operation",
            "provider does not support image edits",
        ),
    };
    if supported {
        Ok(())
    } else {
        Err(unsupported(capabilities, field, message))
    }
}

fn check_input_counts(
    request: &ImageRequest,
    capabilities: &ProviderCapabilities,
) -> Result<(), BridgeError> {
    let reference_count = request.operation.reference_images().len();
    if reference_count > usize::from(capabilities.reference_images.max_count) {
        return Err(unsupported(
            capabilities,
            "reference_images",
            "reference image count exceeds provider capability",
        ));
    }
    if reference_count > 0 && capabilities.reference_images.support == SupportLevel::Unsupported {
        return Err(unsupported(
            capabilities,
            "reference_images",
            "provider does not support reference images",
        ));
    }
    if let ImageOperation::Edit { images, mask, .. } = &request.operation {
        if images.len() > usize::from(capabilities.edit_images.max_count)
            || capabilities.edit_images.support == SupportLevel::Unsupported
        {
            return Err(unsupported(
                capabilities,
                "images",
                "edit images exceed provider capability",
            ));
        }
        if mask.is_some()
            && (capabilities.masks.support == SupportLevel::Unsupported
                || capabilities.masks.max_count == 0)
        {
            return Err(unsupported(
                capabilities,
                "mask",
                "provider does not support edit masks",
            ));
        }
    }
    Ok(())
}

fn check_sessions(
    request: &ImageRequest,
    capabilities: &ProviderCapabilities,
) -> Result<(), BridgeError> {
    match request.session.mode {
        SessionMode::Isolated => Ok(()),
        SessionMode::Persistent if capabilities.persistent_sessions => Ok(()),
        SessionMode::Thread if capabilities.explicit_threads => Ok(()),
        SessionMode::Persistent => Err(unsupported(
            capabilities,
            "session.mode",
            "provider does not support persistent sessions",
        )),
        SessionMode::Thread => Err(unsupported(
            capabilities,
            "session.mode",
            "provider does not support explicit threads",
        )),
    }
}

fn negotiate_count(
    request: &mut ImageRequest,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
) -> Result<(), BridgeError> {
    let requested = request.parameters.n;
    if capabilities.count.contains(requested) {
        return Ok(());
    }
    if request.policies.compatibility == CompatibilityMode::Strict {
        return Err(unsupported(
            capabilities,
            "parameters.n",
            "output count is outside provider capability",
        ));
    }
    let effective = requested.clamp(capabilities.count.min, capabilities.count.max);
    record(
        normalizations,
        "parameters.n",
        requested,
        effective,
        "clamped_to_provider_range",
    );
    request.parameters.n = effective;
    Ok(())
}

fn negotiate_size(
    request: &mut ImageRequest,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
) -> Result<(), BridgeError> {
    let requested = request.parameters.size.clone();
    if size_supported(&requested, capabilities) {
        return Ok(());
    }
    if request.policies.compatibility == CompatibilityMode::Strict {
        return Err(unsupported(
            capabilities,
            "parameters.size",
            "requested size is outside provider capability",
        ));
    }

    let effective = if !requested.is_auto() && capabilities.sizes.auto {
        ImageSize::default()
    } else if let Some(size) = capabilities.sizes.allowed.iter().next() {
        size.clone()
    } else {
        return Err(unsupported(
            capabilities,
            "parameters.size",
            "provider has no usable size fallback",
        ));
    };
    record(
        normalizations,
        "parameters.size",
        requested.to_string(),
        effective.to_string(),
        "normalized_to_provider_size",
    );
    request.parameters.size = effective;
    Ok(())
}

fn size_supported(size: &ImageSize, capabilities: &ProviderCapabilities) -> bool {
    if size.is_auto() {
        return capabilities.sizes.auto;
    }
    if capabilities.sizes.allowed.contains(size) {
        return true;
    }
    let Some((width, height)) = size.dimensions() else {
        return false;
    };
    if !capabilities.sizes.arbitrary {
        return false;
    }
    let short = width.min(height);
    let long = width.max(height);
    let pixels = u64::from(width) * u64::from(height);
    capabilities.sizes.min_edge.is_none_or(|min| short >= min)
        && capabilities.sizes.max_edge.is_none_or(|max| long <= max)
        && capabilities
            .sizes
            .edge_multiple
            .is_none_or(|multiple| width % multiple == 0 && height % multiple == 0)
        && capabilities
            .sizes
            .min_pixels
            .is_none_or(|min| pixels >= min)
        && capabilities
            .sizes
            .max_pixels
            .is_none_or(|max| pixels <= max)
        && capabilities
            .sizes
            .max_aspect_ratio
            .is_none_or(|max| f64::from(long) / f64::from(short) <= max)
}

fn negotiate_hints(
    request: &mut ImageRequest,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
) -> Result<(), BridgeError> {
    if request.parameters.aspect_ratio.is_some()
        && capabilities.aspect_ratio == SupportLevel::Unsupported
    {
        if request.policies.compatibility == CompatibilityMode::Strict {
            return Err(unsupported(
                capabilities,
                "parameters.aspect_ratio",
                "provider cannot honor an aspect-ratio hint",
            ));
        }
        let requested = request
            .parameters
            .aspect_ratio
            .take()
            .map(|value| value.to_string());
        record_optional(
            normalizations,
            "parameters.aspect_ratio",
            requested,
            None::<String>,
            "provider_does_not_support_hint",
        );
    }
    if request.parameters.resolution.is_some()
        && capabilities.resolution == SupportLevel::Unsupported
    {
        if request.policies.compatibility == CompatibilityMode::Strict {
            return Err(unsupported(
                capabilities,
                "parameters.resolution",
                "provider cannot honor a resolution hint",
            ));
        }
        let requested = request
            .parameters
            .resolution
            .take()
            .map(|value| value.to_string());
        record_optional(
            normalizations,
            "parameters.resolution",
            requested,
            None::<String>,
            "provider_does_not_support_hint",
        );
    }
    Ok(())
}

fn negotiate_quality(
    request: &mut ImageRequest,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
) -> Result<(), BridgeError> {
    let requested = request.parameters.quality;
    if capabilities.qualities.contains(&requested) {
        return Ok(());
    }
    let effective = capabilities
        .qualities
        .get(&Quality::Auto)
        .copied()
        .or_else(|| capabilities.qualities.iter().next().copied())
        .ok_or_else(|| {
            unsupported(
                capabilities,
                "parameters.quality",
                "provider declares no supported quality",
            )
        })?;
    normalize_or_reject(
        request.policies.compatibility,
        capabilities,
        normalizations,
        "parameters.quality",
        requested,
        effective,
    )?;
    request.parameters.quality = effective;
    Ok(())
}

fn negotiate_format(
    request: &mut ImageRequest,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
) -> Result<(), BridgeError> {
    let requested = request.parameters.output_format;
    if !capabilities.output_formats.contains(&requested) {
        let effective = capabilities
            .output_formats
            .get(&OutputFormat::Png)
            .copied()
            .or_else(|| capabilities.output_formats.iter().next().copied())
            .ok_or_else(|| {
                unsupported(
                    capabilities,
                    "parameters.output_format",
                    "provider declares no output format",
                )
            })?;
        normalize_or_reject(
            request.policies.compatibility,
            capabilities,
            normalizations,
            "parameters.output_format",
            requested,
            effective,
        )?;
        request.parameters.output_format = effective;
    }
    if request.parameters.output_compression.is_some()
        && !matches!(
            request.parameters.output_format,
            OutputFormat::Jpeg | OutputFormat::Webp
        )
    {
        if request.policies.compatibility == CompatibilityMode::Strict {
            return Err(unsupported(
                capabilities,
                "parameters.output_compression",
                "effective output format does not support compression",
            ));
        }
        let requested_compression = request.parameters.output_compression.take();
        record_optional(
            normalizations,
            "parameters.output_compression",
            requested_compression,
            None::<u8>,
            "effective_format_has_no_compression",
        );
    }
    Ok(())
}

fn negotiate_background(
    request: &mut ImageRequest,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
) -> Result<(), BridgeError> {
    let requested = request.parameters.background;
    if capabilities.backgrounds.contains(&requested) {
        return Ok(());
    }
    let effective = capabilities
        .backgrounds
        .get(&Background::Auto)
        .copied()
        .or_else(|| capabilities.backgrounds.iter().next().copied())
        .ok_or_else(|| {
            unsupported(
                capabilities,
                "parameters.background",
                "provider declares no background behavior",
            )
        })?;
    normalize_or_reject(
        request.policies.compatibility,
        capabilities,
        normalizations,
        "parameters.background",
        requested,
        effective,
    )?;
    request.parameters.background = effective;
    Ok(())
}

fn negotiate_moderation(
    request: &mut ImageRequest,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
) -> Result<(), BridgeError> {
    let requested = request.parameters.moderation;
    if capabilities.moderation.contains(&requested) {
        return Ok(());
    }
    let effective = capabilities
        .moderation
        .get(&Moderation::Auto)
        .copied()
        .or_else(|| capabilities.moderation.iter().next().copied())
        .ok_or_else(|| {
            unsupported(
                capabilities,
                "parameters.moderation",
                "provider declares no moderation behavior",
            )
        })?;
    normalize_or_reject(
        request.policies.compatibility,
        capabilities,
        normalizations,
        "parameters.moderation",
        requested,
        effective,
    )?;
    request.parameters.moderation = effective;
    Ok(())
}

fn negotiate_partial_images(
    request: &mut ImageRequest,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
) -> Result<(), BridgeError> {
    let requested = request.parameters.partial_images;
    if capabilities.partial_images.contains(requested) {
        return Ok(());
    }
    if request.policies.compatibility == CompatibilityMode::Strict {
        return Err(unsupported(
            capabilities,
            "parameters.partial_images",
            "partial image count is outside provider capability",
        ));
    }
    let effective = requested.clamp(
        capabilities.partial_images.min,
        capabilities.partial_images.max,
    );
    record(
        normalizations,
        "parameters.partial_images",
        requested,
        effective,
        "clamped_to_provider_range",
    );
    request.parameters.partial_images = effective;
    Ok(())
}

fn negotiate_negative_prompt(
    request: &mut ImageRequest,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
) -> Result<(), BridgeError> {
    let Some(negative_prompt) = request.negative_prompt.clone() else {
        return Ok(());
    };
    let mode = request.policies.negative_prompt;
    let merge = match mode {
        NegativePromptMode::Reject => {
            return Err(unsupported(
                capabilities,
                "negative_prompt",
                "negative prompt is rejected by request policy",
            ));
        }
        NegativePromptMode::Native => {
            if capabilities.negative_prompt != SupportLevel::Native {
                return Err(unsupported(
                    capabilities,
                    "negative_prompt",
                    "provider does not support a native negative prompt",
                ));
            }
            false
        }
        NegativePromptMode::Merge => true,
        NegativePromptMode::Auto => match capabilities.negative_prompt {
            SupportLevel::Native => false,
            SupportLevel::Emulated => true,
            SupportLevel::Unsupported => {
                return Err(unsupported(
                    capabilities,
                    "negative_prompt",
                    "provider cannot honor a negative prompt under auto policy",
                ));
            }
        },
    };
    if merge {
        request
            .prompt
            .push_str("\n\nNegative constraints (avoid these elements): ");
        request.prompt.push_str(&negative_prompt);
        request.negative_prompt = None;
        normalizations.push(Normalization {
            field: "negative_prompt".to_owned(),
            requested: Some(serde_json::Value::Bool(true)),
            effective: Some(serde_json::Value::String("merged_into_prompt".to_owned())),
            reason: "negative_prompt_prompt_merge".to_owned(),
        });
    }
    Ok(())
}

fn normalize_or_reject<T>(
    mode: CompatibilityMode,
    capabilities: &ProviderCapabilities,
    normalizations: &mut Vec<Normalization>,
    field: &str,
    requested: T,
    effective: T,
) -> Result<(), BridgeError>
where
    T: Serialize + Copy,
{
    if mode == CompatibilityMode::Strict {
        return Err(unsupported(
            capabilities,
            field,
            "requested value is outside provider capability",
        ));
    }
    record(
        normalizations,
        field,
        requested,
        effective,
        "normalized_to_provider_value",
    );
    Ok(())
}

fn record<T: Serialize, U: Serialize>(
    normalizations: &mut Vec<Normalization>,
    field: &str,
    requested: T,
    effective: U,
    reason: &str,
) {
    record_optional(
        normalizations,
        field,
        Some(requested),
        Some(effective),
        reason,
    );
}

fn record_optional<T: Serialize, U: Serialize>(
    normalizations: &mut Vec<Normalization>,
    field: &str,
    requested: Option<T>,
    effective: Option<U>,
    reason: &str,
) {
    normalizations.push(Normalization {
        field: field.to_owned(),
        requested: requested.and_then(|value| serde_json::to_value(value).ok()),
        effective: effective.and_then(|value| serde_json::to_value(value).ok()),
        reason: reason.to_owned(),
    });
}

fn unsupported(capabilities: &ProviderCapabilities, field: &str, message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::UnsupportedCapability, message)
        .with_provider(&capabilities.provider)
        .with_detail("field", field)
        .with_detail("model", &capabilities.model)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeSet;

    use super::*;
    use crate::{
        BatchCapabilities, BatchMode, InputCapabilities, RequestPolicies, SizeCapabilities, U8Range,
    };

    fn capabilities() -> ProviderCapabilities {
        ProviderCapabilities {
            provider: "test".to_owned(),
            implementation_version: "1".to_owned(),
            model: Some("test-image".to_owned()),
            experimental: false,
            generation: true,
            edits: false,
            count: U8Range { min: 1, max: 2 },
            batching: BatchCapabilities {
                mode: BatchMode::Native,
                native_count: U8Range { min: 1, max: 2 },
                max_parallel_outputs: 1,
            },
            sizes: SizeCapabilities {
                auto: true,
                allowed: BTreeSet::new(),
                arbitrary: false,
                min_edge: None,
                max_edge: None,
                edge_multiple: None,
                min_pixels: None,
                max_pixels: None,
                max_aspect_ratio: None,
            },
            aspect_ratio: SupportLevel::Unsupported,
            resolution: SupportLevel::Unsupported,
            qualities: BTreeSet::from([Quality::Auto]),
            output_formats: BTreeSet::from([OutputFormat::Png]),
            backgrounds: BTreeSet::from([Background::Auto]),
            transparent_background: SupportLevel::Unsupported,
            moderation: BTreeSet::from([Moderation::Auto]),
            negative_prompt: SupportLevel::Emulated,
            revised_prompt: SupportLevel::Unsupported,
            user_attribution: SupportLevel::Unsupported,
            input_fidelities: BTreeSet::new(),
            actions: BTreeSet::from([crate::ImageAction::Auto]),
            reference_images: no_inputs(),
            edit_images: no_inputs(),
            masks: no_inputs(),
            partial_images: U8Range { min: 0, max: 0 },
            persistent_sessions: true,
            explicit_threads: true,
        }
    }

    #[test]
    fn capability_validation_rejects_inconsistent_batching() {
        let mut value = capabilities();
        value.batching.mode = BatchMode::FanOut;
        value.batching.native_count = value.count;
        let error = value.validate().unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);

        let mut value = capabilities();
        value.batching.max_parallel_outputs = 0;
        assert!(value.validate().is_err());
    }

    fn no_inputs() -> InputCapabilities {
        InputCapabilities {
            support: SupportLevel::Unsupported,
            max_count: 0,
            max_bytes_each: 0,
            max_bytes_total: 0,
        }
    }

    #[test]
    fn strict_mode_rejects_unsupported_quality() {
        let mut request = ImageRequest::generate("test");
        request.parameters.quality = Quality::High;
        let error = negotiate_request(&request, &capabilities()).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedCapability);
        assert_eq!(
            error.details.get("field"),
            Some(&serde_json::Value::String("parameters.quality".to_owned()))
        );
    }

    #[test]
    fn explicit_model_mismatch_is_never_silently_normalized() {
        let mut request = ImageRequest::generate("test");
        request.routing.model = Some("another-image-model".to_owned());
        request.policies.compatibility = CompatibilityMode::BestEffort;
        let error = negotiate_request(&request, &capabilities()).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedCapability);
        assert_eq!(
            error.details.get("field"),
            Some(&serde_json::Value::String("routing.model".to_owned()))
        );
    }

    #[test]
    fn unsupported_user_attribution_is_never_silently_dropped() {
        let mut request = ImageRequest::generate("test");
        request.user = Some("opaque-caller".to_owned());
        request.policies.compatibility = CompatibilityMode::BestEffort;
        let error = negotiate_request(&request, &capabilities()).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedCapability);
        assert_eq!(
            error.details.get("field"),
            Some(&serde_json::Value::String("user".to_owned()))
        );
    }

    #[test]
    fn explicit_fidelity_and_action_are_capability_checked() {
        let mut request = ImageRequest::generate("test");
        request.parameters.input_fidelity = Some(crate::InputFidelity::High);
        let error = negotiate_request(&request, &capabilities()).unwrap_err();
        assert_eq!(error.details["field"], "parameters.input_fidelity");

        request.parameters.input_fidelity = None;
        request.parameters.action = crate::ImageAction::Generate;
        let error = negotiate_request(&request, &capabilities()).unwrap_err();
        assert_eq!(error.details["field"], "parameters.action");
    }

    #[test]
    fn normalize_mode_reports_every_changed_parameter() {
        let mut request = ImageRequest::generate("test");
        request.policies = RequestPolicies {
            compatibility: CompatibilityMode::Normalize,
            ..RequestPolicies::default()
        };
        request.parameters.n = 3;
        request.parameters.quality = Quality::High;
        request.parameters.size = "1536x1024".parse().unwrap();

        let negotiated = negotiate_request(&request, &capabilities()).unwrap();
        assert_eq!(negotiated.effective_request.parameters.n, 2);
        assert_eq!(
            negotiated.effective_request.parameters.quality,
            Quality::Auto
        );
        assert!(negotiated.effective_request.parameters.size.is_auto());
        assert_eq!(negotiated.normalizations.len(), 3);
    }

    #[test]
    fn emulated_negative_prompt_is_merged_without_echoing_its_text_in_metadata() {
        let mut request = ImageRequest::generate("a portrait");
        request.negative_prompt = Some("watermark".to_owned());
        let negotiated = negotiate_request(&request, &capabilities()).unwrap();
        assert!(negotiated.effective_request.prompt.contains("watermark"));
        assert!(negotiated.effective_request.negative_prompt.is_none());
        let metadata = serde_json::to_string(&negotiated.normalizations).unwrap();
        assert!(!metadata.contains("watermark"));
    }

    #[test]
    fn arbitrary_size_constraints_are_enforced() {
        let mut capabilities = capabilities();
        capabilities.sizes = SizeCapabilities {
            auto: true,
            allowed: BTreeSet::new(),
            arbitrary: true,
            min_edge: None,
            max_edge: Some(3840),
            edge_multiple: Some(16),
            min_pixels: Some(655_360),
            max_pixels: Some(8_294_400),
            max_aspect_ratio: Some(3.0),
        };
        let mut request = ImageRequest::generate("test");
        request.parameters.size = "1024x1024".parse().unwrap();
        assert!(negotiate_request(&request, &capabilities).is_ok());

        request.parameters.size = "4000x1024".parse().unwrap();
        assert!(negotiate_request(&request, &capabilities).is_err());
    }

    #[test]
    fn malformed_provider_ranges_are_rejected_without_panicking() {
        for mode in [
            CompatibilityMode::Strict,
            CompatibilityMode::Normalize,
            CompatibilityMode::BestEffort,
        ] {
            let mut request = ImageRequest::generate("test");
            request.policies.compatibility = mode;

            let mut invalid_count = capabilities();
            invalid_count.count = U8Range { min: 2, max: 1 };
            let error = negotiate_request(&request, &invalid_count).unwrap_err();
            assert_eq!(error.code, ErrorCode::Protocol);

            let mut invalid_partials = capabilities();
            invalid_partials.partial_images = U8Range { min: 2, max: 1 };
            let error = negotiate_request(&request, &invalid_partials).unwrap_err();
            assert_eq!(error.code, ErrorCode::Protocol);
        }
    }
}
