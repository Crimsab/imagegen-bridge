//! Transparent-background routing and local chroma-key planning.

use std::fmt::Write as _;

use imagegen_bridge_artifacts::{ChromaKey, ChromaKeyOptions};
use imagegen_bridge_core::{
    Background, BridgeError, CompatibilityMode, ErrorCode, ImageRequest, Normalization,
    OutputFormat, ProviderCapabilities, TransparencyMode,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct TransparencyPlan {
    pub(crate) chroma: ChromaKeyOptions,
}

pub(crate) struct PreparedTransparency {
    pub(crate) provider_request: ImageRequest,
    pub(crate) plan: Option<TransparencyPlan>,
    pub(crate) normalizations: Vec<Normalization>,
}

pub(crate) fn prepare_transparency(
    request: &ImageRequest,
    capabilities: &ProviderCapabilities,
) -> Result<PreparedTransparency, BridgeError> {
    let mut provider_request = request.clone();
    let mut normalizations = Vec::new();
    if request.parameters.background != Background::Transparent {
        return Ok(PreparedTransparency {
            provider_request,
            plan: None,
            normalizations,
        });
    }

    let native = capabilities.transparent_background == imagegen_bridge_core::SupportLevel::Native;
    if request.output.transparency.mode == TransparencyMode::Auto
        && capabilities.transparent_background == imagegen_bridge_core::SupportLevel::Unsupported
    {
        return Err(BridgeError::new(
            ErrorCode::UnsupportedCapability,
            "selected provider/model does not support transparent output",
        )
        .with_provider(&capabilities.provider)
        .with_detail("field", "parameters.background"));
    }
    let mode = match request.output.transparency.mode {
        TransparencyMode::Auto if native => TransparencyMode::Native,
        TransparencyMode::Auto => TransparencyMode::ChromaKey,
        explicit => explicit,
    };
    normalizations.push(Normalization {
        field: "output.transparency.mode".to_owned(),
        requested: Some(serde_json::json!(request.output.transparency.mode)),
        effective: Some(serde_json::json!(mode)),
        reason: if mode == TransparencyMode::Native {
            "provider_supports_native_alpha"
        } else {
            "provider_alpha_emulated_with_local_chroma_key"
        }
        .to_owned(),
    });

    if mode == TransparencyMode::Native {
        if !native {
            return Err(BridgeError::new(
                ErrorCode::UnsupportedCapability,
                "selected provider/model does not support native transparent output",
            )
            .with_provider(&capabilities.provider)
            .with_detail("field", "output.transparency.mode")
            .with_detail("requested", "native"));
        }
        return Ok(PreparedTransparency {
            provider_request,
            plan: None,
            normalizations,
        });
    }

    if request.parameters.output_format == OutputFormat::Jpeg {
        if request.policies.compatibility == CompatibilityMode::Strict {
            return Err(BridgeError::new(
                ErrorCode::UnsupportedCapability,
                "transparent output cannot be encoded as JPEG",
            )
            .with_provider(&capabilities.provider)
            .with_detail("field", "parameters.output_format"));
        }
        provider_request.parameters.output_format = OutputFormat::Png;
        provider_request.parameters.output_compression = None;
        normalizations.push(Normalization {
            field: "parameters.output_format".to_owned(),
            requested: Some(serde_json::json!(OutputFormat::Jpeg)),
            effective: Some(serde_json::json!(OutputFormat::Png)),
            reason: "transparent_output_requires_alpha_capable_format".to_owned(),
        });
    }

    let key = request
        .output
        .transparency
        .key_color
        .as_deref()
        .map(ChromaKey::parse)
        .transpose()?
        .unwrap_or_else(|| select_key(&request.prompt));
    let effective_background = if capabilities.backgrounds.contains(&Background::Auto) {
        Background::Auto
    } else if capabilities.backgrounds.contains(&Background::Opaque) {
        Background::Opaque
    } else {
        return Err(BridgeError::new(
            ErrorCode::UnsupportedCapability,
            "selected provider/model cannot generate a chroma-key background",
        )
        .with_provider(&capabilities.provider)
        .with_detail("field", "parameters.background"));
    };
    provider_request.parameters.background = effective_background;
    let _ = write!(
        provider_request.prompt,
        "\n\nTRANSPARENT BACKGROUND WORKFLOW: Render the complete subject isolated on a perfectly flat, uniform solid {key} background. The background must contain only that exact color: no gradient, texture, pattern, horizon, environment, floor, cast shadow, glow, reflection, or extra objects. Keep every part of the subject away from the image edges and preserve clean antialiased contours. Do not use {key} anywhere in the subject.",
        key = key.hex()
    );
    normalizations.push(Normalization {
        field: "parameters.background".to_owned(),
        requested: Some(serde_json::json!(Background::Transparent)),
        effective: Some(serde_json::json!(Background::Transparent)),
        reason: "transparent_result_via_chroma_key_postprocessing".to_owned(),
    });
    normalizations.push(Normalization {
        field: "output.transparency.key_color".to_owned(),
        requested: request
            .output
            .transparency
            .key_color
            .as_ref()
            .map(|value| serde_json::json!(value)),
        effective: Some(serde_json::json!(key.hex())),
        reason: if request.output.transparency.key_color.is_some() {
            "explicit_chroma_key"
        } else {
            "prompt_aware_chroma_key_selection"
        }
        .to_owned(),
    });
    Ok(PreparedTransparency {
        provider_request,
        plan: Some(TransparencyPlan {
            chroma: ChromaKeyOptions {
                key,
                transparent_threshold: request.output.transparency.transparent_threshold,
                opaque_threshold: request.output.transparency.opaque_threshold,
                despill: request.output.transparency.despill,
            },
        }),
        normalizations,
    })
}

fn select_key(prompt: &str) -> ChromaKey {
    let prompt = prompt.to_ascii_lowercase();
    let candidates = [
        (
            ChromaKey(0, 255, 0),
            ["green", "verde", "lime", "emerald", "smeraldo", "forest"].as_slice(),
        ),
        (
            ChromaKey(255, 0, 255),
            ["magenta", "pink", "rosa", "purple", "viola", "fuchsia"].as_slice(),
        ),
        (
            ChromaKey(0, 0, 255),
            ["blue", "blu", "azzurro", "cyan", "cobalt", "navy"].as_slice(),
        ),
    ];
    candidates
        .into_iter()
        .min_by_key(|(_, words)| {
            words
                .iter()
                .map(|word| prompt.match_indices(word).count())
                .sum::<usize>()
        })
        .map_or(ChromaKey(0, 255, 0), |(key, _)| key)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeSet;

    use imagegen_bridge_core::{
        BatchCapabilities, BatchMode, ImageAction, ImageSize, InputCapabilities, InputFidelity,
        Moderation, Quality, SizeCapabilities, SupportLevel, U8Range,
    };

    use super::*;

    fn capabilities(backgrounds: &BTreeSet<Background>) -> ProviderCapabilities {
        let unsupported_inputs = InputCapabilities {
            support: SupportLevel::Unsupported,
            max_count: 0,
            max_bytes_each: 0,
            max_bytes_total: 0,
        };
        ProviderCapabilities {
            provider: "test".to_owned(),
            implementation_version: "test".to_owned(),
            model: Some("test-image".to_owned()),
            experimental: false,
            generation: true,
            edits: false,
            count: U8Range { min: 1, max: 1 },
            batching: BatchCapabilities {
                mode: BatchMode::Native,
                native_count: U8Range { min: 1, max: 1 },
                max_parallel_outputs: 1,
            },
            sizes: SizeCapabilities {
                auto: true,
                allowed: BTreeSet::from([ImageSize::exact(2, 2).unwrap()]),
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
            backgrounds: backgrounds.clone(),
            transparent_background: if backgrounds.contains(&Background::Transparent) {
                SupportLevel::Native
            } else {
                SupportLevel::Emulated
            },
            moderation: BTreeSet::from([Moderation::Auto]),
            negative_prompt: SupportLevel::Unsupported,
            revised_prompt: SupportLevel::Unsupported,
            user_attribution: SupportLevel::Unsupported,
            input_fidelities: BTreeSet::from([InputFidelity::Low]),
            actions: BTreeSet::from([ImageAction::Auto]),
            reference_images: unsupported_inputs.clone(),
            edit_images: unsupported_inputs.clone(),
            masks: unsupported_inputs,
            partial_images: U8Range { min: 0, max: 0 },
            persistent_sessions: false,
            explicit_threads: false,
        }
    }

    #[test]
    fn auto_uses_native_alpha_when_available() {
        let mut request = ImageRequest::generate("red fox");
        request.parameters.background = Background::Transparent;
        let prepared = prepare_transparency(
            &request,
            &capabilities(&BTreeSet::from([Background::Transparent])),
        )
        .unwrap();
        assert!(prepared.plan.is_none());
        assert_eq!(
            prepared.provider_request.parameters.background,
            Background::Transparent
        );
    }

    #[test]
    fn auto_uses_prompt_aware_chroma_key_when_native_alpha_is_missing() {
        let mut request = ImageRequest::generate("an emerald green dragon");
        request.parameters.background = Background::Transparent;
        let prepared =
            prepare_transparency(&request, &capabilities(&BTreeSet::from([Background::Auto])))
                .unwrap();
        assert_eq!(prepared.plan.unwrap().chroma.key, ChromaKey(255, 0, 255));
        assert!(prepared.provider_request.prompt.contains("#ff00ff"));
        assert_eq!(
            prepared.provider_request.parameters.background,
            Background::Auto
        );
    }
}
