//! Provider capability descriptions used for explicit negotiation.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Background, ImageSize, Moderation, OutputFormat, Quality};

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
    /// Supported size behavior.
    pub sizes: SizeCapabilities,
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
}
