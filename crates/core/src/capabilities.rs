//! Provider capability descriptions used for explicit negotiation.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Background, BridgeError, ErrorCode, ImageAction, ImageSize, InputFidelity, Moderation,
    OutputFormat, Quality,
};

/// Degree to which a provider supports a semantic feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SupportLevel {
    /// The provider handles the feature natively.
    Native,
    /// The bridge can emulate the feature with a reported transformation.
    Emulated,
    /// The feature is unavailable.
    Unsupported,
}

/// Inclusive range for a small integer parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct U8Range {
    /// Inclusive minimum.
    pub min: u8,
    /// Inclusive maximum.
    pub max: u8,
}

/// How a provider fulfills a request for more than one output image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BatchMode {
    /// One upstream provider operation can return the requested output count.
    Native,
    /// The bridge fans the request out into multiple bounded upstream operations.
    FanOut,
}

/// Effective and native multi-output behavior for a provider/model pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchCapabilities {
    /// Whether multi-output execution is native or bridge-managed fan-out.
    pub mode: BatchMode,
    /// Output-count range accepted by one upstream provider operation.
    pub native_count: U8Range,
    /// Maximum simultaneous upstream operations across active batches.
    pub max_parallel_outputs: u8,
}

impl U8Range {
    /// Returns true when the range contains the value.
    #[must_use]
    pub const fn contains(self, value: u8) -> bool {
        value >= self.min && value <= self.max
    }
}

/// Explicit-size constraints for a provider or model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SizeCapabilities {
    /// Whether `auto` is accepted.
    pub auto: bool,
    /// Fixed accepted sizes. Empty when generic constraints apply.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub allowed: BTreeSet<ImageSize>,
    /// Whether arbitrary explicit dimensions are accepted.
    pub arbitrary: bool,
    /// Minimum edge for arbitrary dimensions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_edge: Option<u32>,
    /// Maximum edge for arbitrary dimensions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_edge: Option<u32>,
    /// Required edge multiple for arbitrary dimensions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_multiple: Option<u32>,
    /// Minimum total pixels for arbitrary dimensions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_pixels: Option<u64>,
    /// Maximum total pixels for arbitrary dimensions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_pixels: Option<u64>,
    /// Maximum long-edge to short-edge ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_aspect_ratio: Option<f64>,
}

/// Input-image constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputCapabilities {
    /// Support level for the input class.
    pub support: SupportLevel,
    /// Maximum number of inputs.
    pub max_count: u16,
    /// Maximum decoded bytes per input.
    pub max_bytes_each: u64,
    /// Maximum decoded bytes across all inputs.
    pub max_bytes_total: u64,
}

/// Complete capability declaration for one provider/model pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderCapabilities {
    /// Stable provider name.
    pub provider: String,
    /// Provider implementation version.
    pub implementation_version: String,
    /// Model for which these capabilities apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether this adapter targets an unstable/private upstream interface.
    pub experimental: bool,
    /// Whether generation is supported.
    pub generation: bool,
    /// Whether edits are supported.
    pub edits: bool,
    /// Supported output count range.
    pub count: U8Range,
    /// How the advertised output count is fulfilled upstream.
    pub batching: BatchCapabilities,
    /// Supported size behavior.
    pub sizes: SizeCapabilities,
    /// Aspect-ratio hint support.
    pub aspect_ratio: SupportLevel,
    /// Coarse resolution hint support.
    pub resolution: SupportLevel,
    /// Supported qualities.
    pub qualities: BTreeSet<Quality>,
    /// Supported output encodings.
    pub output_formats: BTreeSet<OutputFormat>,
    /// Supported backgrounds.
    pub backgrounds: BTreeSet<Background>,
    /// Supported moderation values.
    pub moderation: BTreeSet<Moderation>,
    /// Negative prompt support.
    pub negative_prompt: SupportLevel,
    /// Revised-prompt availability.
    pub revised_prompt: SupportLevel,
    /// Opaque end-user attribution support.
    pub user_attribution: SupportLevel,
    /// Supported explicit input-fidelity values.
    pub input_fidelities: BTreeSet<InputFidelity>,
    /// Supported image-tool actions.
    pub actions: BTreeSet<ImageAction>,
    /// Reference-image constraints.
    pub reference_images: InputCapabilities,
    /// Edit-image constraints.
    pub edit_images: InputCapabilities,
    /// Mask constraints.
    pub masks: InputCapabilities,
    /// Supported partial image count.
    pub partial_images: U8Range,
    /// Whether provider-backed persistent sessions are supported.
    pub persistent_sessions: bool,
    /// Whether explicit upstream thread IDs are supported.
    pub explicit_threads: bool,
}

impl ProviderCapabilities {
    /// Rejects semantically inconsistent dynamic provider declarations.
    pub fn validate(&self) -> Result<(), BridgeError> {
        if self.count.min == 0 || self.count.min > self.count.max {
            return Err(invalid_capability(
                self,
                "provider output-count capability range is invalid",
            ));
        }
        if self.batching.native_count.min == 0
            || self.batching.native_count.min > self.batching.native_count.max
            || self.batching.native_count.min < self.count.min
            || self.batching.native_count.max > self.count.max
            || self.batching.max_parallel_outputs == 0
            || self.batching.max_parallel_outputs > self.count.max
            || (self.batching.mode == BatchMode::Native && self.batching.native_count != self.count)
            || (self.batching.mode == BatchMode::FanOut
                && self.batching.native_count.max >= self.count.max)
        {
            return Err(invalid_capability(
                self,
                "provider batching capability is inconsistent with its output-count range",
            ));
        }
        if self.partial_images.min > self.partial_images.max {
            return Err(invalid_capability(
                self,
                "provider partial-image capability range is invalid",
            ));
        }
        Ok(())
    }
}

fn invalid_capability(capabilities: &ProviderCapabilities, message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message).with_provider(&capabilities.provider)
}

/// Provider identity shown in discovery endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderDescriptor {
    /// Stable registry name.
    pub name: String,
    /// Human-readable provider title.
    pub display_name: String,
    /// Provider implementation version.
    pub version: String,
    /// Whether the adapter is experimental.
    pub experimental: bool,
    /// Image models that can be queried through the capability endpoint.
    #[serde(default)]
    pub models: Vec<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::ProviderDescriptor;

    #[test]
    fn provider_model_inventory_is_additive_for_older_payloads() {
        let descriptor: ProviderDescriptor = serde_json::from_str(
            r#"{"name":"test","display_name":"Test","version":"1","experimental":false}"#,
        )
        .unwrap();
        assert!(descriptor.models.is_empty());

        let encoded = serde_json::to_value(ProviderDescriptor {
            models: vec!["gpt-image-2".to_owned()],
            ..descriptor
        })
        .unwrap();
        assert_eq!(encoded["models"][0], "gpt-image-2");
    }
}
