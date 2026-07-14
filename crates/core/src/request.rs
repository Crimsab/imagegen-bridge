//! Normalized image request types.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::{
    ArtifactCollisionPolicy, ArtifactMetadataPolicy, AspectRatio, Background, CompatibilityMode,
    ImageAction, ImageSize, InputFidelity, Moderation, MultiImageFailurePolicy, NegativePromptMode,
    OutputFormat, Quality, Resolution, ResponseFormat, RevisedPromptPolicy, SessionMode,
};

const COMMON_REQUEST_FIELDS: &[&str] = &[
    "version",
    "prompt",
    "negative_prompt",
    "parameters",
    "routing",
    "session",
    "output",
    "policies",
    "idempotency_key",
    "timeout_ms",
    "user",
];

/// A complete provider-neutral image request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
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

impl<'de> Deserialize<'de> for ImageRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut object = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        for field in object.keys() {
            if field != "operation"
                && field != "reference_images"
                && field != "images"
                && field != "mask"
                && !COMMON_REQUEST_FIELDS.contains(&field.as_str())
            {
                return Err(de::Error::unknown_field(
                    field,
                    &[
                        "version",
                        "prompt",
                        "negative_prompt",
                        "operation",
                        "reference_images",
                        "images",
                        "mask",
                        "parameters",
                        "routing",
                        "session",
                        "output",
                        "policies",
                        "idempotency_key",
                        "timeout_ms",
                        "user",
                    ],
                ));
            }
        }

        let operation_tag = object
            .remove("operation")
            .ok_or_else(|| de::Error::missing_field("operation"))?;
        let operation_name = operation_tag
            .as_str()
            .ok_or_else(|| de::Error::custom("operation must be a string"))?;
        let operation_fields: &[&str] = match operation_name {
            "generate" => &["reference_images"],
            "edit" => &["images", "mask", "reference_images"],
            _ => {
                return Err(de::Error::unknown_variant(
                    operation_name,
                    &["generate", "edit"],
                ));
            }
        };
        for forbidden in ["images", "mask", "reference_images"] {
            if object.contains_key(forbidden) && !operation_fields.contains(&forbidden) {
                return Err(de::Error::custom(format!(
                    "field '{forbidden}' is invalid for operation '{operation_name}'"
                )));
            }
        }
        let mut operation = serde_json::Map::new();
        operation.insert("operation".to_owned(), operation_tag);
        for field in operation_fields {
            if let Some(value) = object.remove(*field) {
                operation.insert((*field).to_owned(), value);
            }
        }
        let operation = serde_json::from_value(serde_json::Value::Object(operation))
            .map_err(de::Error::custom)?;
        let fields: RequestFields =
            serde_json::from_value(serde_json::Value::Object(object)).map_err(de::Error::custom)?;
        Ok(Self {
            version: fields.version,
            prompt: fields.prompt,
            negative_prompt: fields.negative_prompt,
            operation,
            parameters: fields.parameters,
            routing: fields.routing,
            session: fields.session,
            output: fields.output,
            policies: fields.policies,
            idempotency_key: fields.idempotency_key,
            timeout_ms: fields.timeout_ms,
            user: fields.user,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestFields {
    #[serde(default = "default_contract_version")]
    version: String,
    prompt: String,
    negative_prompt: Option<String>,
    #[serde(default)]
    parameters: GenerationParameters,
    #[serde(default)]
    routing: RoutingOptions,
    #[serde(default)]
    session: SessionOptions,
    #[serde(default)]
    output: OutputOptions,
    #[serde(default)]
    policies: RequestPolicies,
    idempotency_key: Option<String>,
    timeout_ms: Option<u64>,
    user: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
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

impl<'de> Deserialize<'de> for ImageInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut fields = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let media_type = fields
            .remove("media_type")
            .map(serde_json::from_value::<Option<String>>)
            .transpose()
            .map_err(de::Error::custom)?
            .flatten();
        let filename = fields
            .remove("filename")
            .map(serde_json::from_value::<Option<String>>)
            .transpose()
            .map_err(de::Error::custom)?
            .flatten();
        let source =
            serde_json::from_value(serde_json::Value::Object(fields)).map_err(de::Error::custom)?;
        Ok(Self {
            source,
            media_type,
            filename,
        })
    }
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
    /// Behavior when one output in a multi-image request fails.
    pub failure_policy: MultiImageFailurePolicy,
    /// Optional input-image fidelity for edit/reference operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_fidelity: Option<InputFidelity>,
    /// Generate/edit selection for transports with an image tool action.
    pub action: ImageAction,
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
            failure_policy: MultiImageFailurePolicy::default(),
            input_fidelity: None,
            action: ImageAction::default(),
        }
    }
}

#[cfg(test)]
mod serde_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn image_request_round_trips_with_flattened_operation() {
        let request = ImageRequest::generate("test");
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["operation"], "generate");
        let decoded: ImageRequest = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn image_request_rejects_unknown_and_operation_specific_fields() {
        let unknown = serde_json::json!({
            "prompt": "test",
            "operation": "generate",
            "surprise": true
        });
        assert!(serde_json::from_value::<ImageRequest>(unknown).is_err());
        let inconsistent = serde_json::json!({
            "prompt": "test",
            "operation": "generate",
            "images": []
        });
        assert!(serde_json::from_value::<ImageRequest>(inconsistent).is_err());
    }

    #[test]
    fn every_image_input_source_round_trips_and_rejects_unknown_fields() {
        let inputs = [
            ImageInput {
                source: ImageSource::File {
                    path: PathBuf::from("fixture.png"),
                },
                media_type: Some("image/png".to_owned()),
                filename: Some("fixture.png".to_owned()),
            },
            ImageInput {
                source: ImageSource::Url {
                    url: "https://example.test/fixture.png".to_owned(),
                },
                media_type: None,
                filename: None,
            },
            ImageInput {
                source: ImageSource::DataUrl {
                    data_url: "data:image/png;base64,aW1hZ2U=".to_owned(),
                },
                media_type: None,
                filename: None,
            },
            ImageInput {
                source: ImageSource::Base64 {
                    data: "aW1hZ2U=".to_owned(),
                },
                media_type: Some("image/png".to_owned()),
                filename: None,
            },
        ];
        for input in inputs {
            let encoded = serde_json::to_value(&input).unwrap();
            assert_eq!(
                serde_json::from_value::<ImageInput>(encoded).unwrap(),
                input
            );
        }

        let unknown = serde_json::json!({
            "type": "url",
            "url": "https://example.test/fixture.png",
            "unexpected": true
        });
        assert!(serde_json::from_value::<ImageInput>(unknown).is_err());
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
    /// Optional portable relative directory below the configured artifact root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// Optional exact single-image filename, with or without a matching extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Atomic behavior if an explicit filename already exists.
    pub collision: ArtifactCollisionPolicy,
    /// Optional portable metadata persistence beside each artifact.
    pub metadata: ArtifactMetadataPolicy,
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
