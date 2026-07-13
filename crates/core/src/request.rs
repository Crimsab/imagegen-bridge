//! Normalized image request types.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AspectRatio, Background, CompatibilityMode, ImageSize, Moderation, NegativePromptMode,
    OutputFormat, Quality, Resolution, ResponseFormat, RevisedPromptPolicy, SessionMode,
};

/// A complete provider-neutral image request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageRequest {
    /// Contract version. Currently `1`.
    #[serde(default = "default_contract_version")]
    pub version: String,
    /// Positive prompt passed to image generation.
    pub prompt: String,
    /// Optional negative prompt interpreted according to bridge policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negative_prompt: Option<String>,
    /// Generation or edit inputs.
    #[serde(flatten)]
    pub operation: ImageOperation,
    /// Image-generation parameters.
    #[serde(default)]
    pub parameters: GenerationParameters,
    /// Provider and model routing controls.
    #[serde(default)]
    pub routing: RoutingOptions,
    /// Session behavior for providers that support conversations.
    #[serde(default)]
    pub session: SessionOptions,
    /// Output delivery and artifact controls.
    #[serde(default)]
    pub output: OutputOptions,
    /// Fallback and compatibility policies.
    #[serde(default)]
    pub policies: RequestPolicies,
    /// Optional client idempotency key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Optional request deadline in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Optional opaque end-user identifier forwarded only by configured providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl ImageRequest {
    /// Creates a generation request using safe defaults.
    #[must_use]
    pub fn generate(prompt: impl Into<String>) -> Self {
        Self {
            version: default_contract_version(),
            prompt: prompt.into(),
            negative_prompt: None,
            operation: ImageOperation::Generate {
                reference_images: Vec::new(),
            },
            parameters: GenerationParameters::default(),
            routing: RoutingOptions::default(),
            session: SessionOptions::default(),
            output: OutputOptions::default(),
            policies: RequestPolicies::default(),
            idempotency_key: None,
            timeout_ms: None,
            user: None,
        }
    }
}

fn default_contract_version() -> String {
    crate::CONTRACT_VERSION.to_owned()
}

/// Operation-specific image inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageOperation {
    /// Generate an image, optionally using reference images.
    Generate {
        /// Images used as visual references.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reference_images: Vec<ImageInput>,
    },
    /// Edit one or more source images with an optional mask and references.
    Edit {
        /// Source images to edit.
        images: Vec<ImageInput>,
        /// Optional edit mask.
        #[serde(skip_serializing_if = "Option::is_none")]
        mask: Option<Box<ImageInput>>,
        /// Additional visual references.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reference_images: Vec<ImageInput>,
    },
}

impl ImageOperation {
    /// Returns all reference inputs without edit sources or masks.
    #[must_use]
    pub fn reference_images(&self) -> &[ImageInput] {
        match self {
            Self::Generate { reference_images }
            | Self::Edit {
                reference_images, ..
            } => reference_images,
        }
    }
}

/// Supported image input locations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageSource {
    /// Path resolved under configured allowed roots.
    File {
        /// Local filesystem path.
        path: PathBuf,
    },
    /// Remote HTTP(S) URL, only when remote loading is enabled.
    Url {
        /// Remote URL.
        url: String,
    },
    /// RFC 2397 data URL.
    DataUrl {
        /// Complete data URL.
        data_url: String,
    },
    /// Base64-encoded image body.
    Base64 {
        /// Encoded body without a data URL prefix.
        data: String,
    },
}

/// One image input plus optional metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageInput {
    /// Source from which bytes will be loaded.
    #[serde(flatten)]
    pub source: ImageSource,
    /// Optional expected media type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Optional safe logical filename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

/// Image-generation parameters shared across providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GenerationParameters {
    /// Number of requested output images.
    pub n: u8,
    /// Automatic or explicit output size.
    pub size: ImageSize,
    /// Optional aspect-ratio hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<AspectRatio>,
    /// Optional coarse resolution hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    /// Requested quality.
    pub quality: Quality,
    /// Requested encoded image format.
    pub output_format: OutputFormat,
    /// Compression from 0 to 100 for JPEG or WebP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_compression: Option<u8>,
    /// Requested background behavior.
    pub background: Background,
    /// Requested moderation behavior.
    pub moderation: Moderation,
    /// Requested number of partial progress images.
    pub partial_images: u8,
}

impl Default for GenerationParameters {
    fn default() -> Self {
        Self {
            n: 1,
            size: ImageSize::default(),
            aspect_ratio: None,
            resolution: None,
            quality: Quality::default(),
            output_format: OutputFormat::default(),
            output_compression: None,
            background: Background::default(),
            moderation: Moderation::default(),
            partial_images: 0,
        }
    }
}

/// Provider selection controls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingOptions {
    /// Explicit provider name, or the configured default when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Explicit provider model, or the provider default when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Conversation/session controls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SessionOptions {
    /// Isolated, persistent-key, or explicit-thread mode.
    pub mode: SessionMode,
    /// Caller-selected durable binding key for persistent mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Existing provider thread ID for explicit-thread mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Output delivery controls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OutputOptions {
    /// Response payload representation.
    pub response_format: ResponseFormat,
    /// Optional logical artifact filename prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename_prefix: Option<String>,
}

/// Explicit fallback and visibility controls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RequestPolicies {
    /// Provider capability compatibility behavior.
    pub compatibility: CompatibilityMode,
    /// Negative-prompt handling behavior.
    pub negative_prompt: NegativePromptMode,
    /// Revised-prompt visibility and requirement behavior.
    pub revised_prompt: RevisedPromptPolicy,
}
