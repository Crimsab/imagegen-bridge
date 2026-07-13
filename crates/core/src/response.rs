//! Normalized provider results and progress events.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de};

use crate::{BridgeError, GenerationParameters, OutputFormat};

/// One explicit normalization or fallback applied to a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Normalization {
    /// JSON-style field path.
    pub field: String,
    /// Safe serialized requested value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested: Option<serde_json::Value>,
    /// Safe serialized effective value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective: Option<serde_json::Value>,
    /// Stable normalization reason.
    pub reason: String,
}

/// Generated output payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImagePayload {
    /// Base64 JSON response data.
    B64Json {
        /// Base64-encoded bytes.
        b64_json: String,
    },
    /// Provider-hosted or bridge-hosted URL.
    Url {
        /// Output URL.
        url: String,
    },
    /// Bridge-owned artifact identifier.
    Artifact {
        /// Opaque artifact ID.
        id: String,
        /// Optional client-safe relative name.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// No body was requested.
    Metadata,
}

/// Metadata and payload for one generated image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GeneratedImage {
    /// Zero-based requested output index.
    pub index: u8,
    /// Output payload or artifact reference.
    #[serde(flatten)]
    pub payload: ImagePayload,
    /// Verified output format.
    pub format: OutputFormat,
    /// Verified width in pixels.
    pub width: u32,
    /// Verified height in pixels.
    pub height: u32,
    /// Verified encoded byte length.
    pub bytes: u64,
    /// Lowercase hexadecimal SHA-256 digest.
    pub sha256: String,
    /// Provider time for this output when measured independently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for GeneratedImage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut fields = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let format = take_required(&mut fields, "format")?;
        let index = take_required(&mut fields, "index")?;
        let width = take_required(&mut fields, "width")?;
        let height = take_required(&mut fields, "height")?;
        let bytes = take_required(&mut fields, "bytes")?;
        let sha256 = take_required(&mut fields, "sha256")?;
        let generation_ms = fields
            .remove("generation_ms")
            .map(serde_json::from_value)
            .transpose()
            .map_err(de::Error::custom)?;
        let payload =
            serde_json::from_value(serde_json::Value::Object(fields)).map_err(de::Error::custom)?;
        Ok(Self {
            index,
            payload,
            format,
            width,
            height,
            bytes,
            sha256,
            generation_ms,
        })
    }
}

/// Failure for one output in a best-effort multi-image request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageFailure {
    /// Zero-based requested output index.
    pub index: u8,
    /// Safe structured provider error.
    pub error: BridgeError,
    /// Provider time spent before the failure.
    pub generation_ms: u64,
}

fn take_required<T, E>(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    name: &'static str,
) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: de::Error,
{
    let value = fields
        .remove(name)
        .ok_or_else(|| E::custom(format!("missing field `{name}`")))?;
    serde_json::from_value(value).map_err(E::custom)
}

/// Optional provider usage accounting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Usage {
    /// Input tokens reported by the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens reported by the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Total tokens reported by the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Provider-specific safe numeric counters.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider: BTreeMap<String, u64>,
}

/// Session information returned after a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionMetadata {
    /// Caller-visible session key when persistent mode was used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Upstream thread ID when policy allows returning it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Whether an existing thread was reused.
    pub reused: bool,
}

/// Stage timings in milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Timings {
    /// Time waiting for admission.
    pub queue_ms: u64,
    /// Time spent loading and validating inputs.
    pub input_ms: u64,
    /// Time spent in the provider.
    pub provider_ms: u64,
    /// Time spent validating/publishing output.
    pub artifact_ms: u64,
    /// Total request time.
    pub total_ms: u64,
}

/// Complete normalized response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageResponse {
    /// Stable bridge request ID.
    pub id: String,
    /// Unix timestamp at completion.
    pub created: u64,
    /// Provider that executed the request.
    pub provider: String,
    /// Effective provider model.
    pub model: String,
    /// Requested generation parameters.
    pub requested: GenerationParameters,
    /// Effective generation parameters.
    pub effective: GenerationParameters,
    /// Explicit normalizations and fallbacks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub normalizations: Vec<Normalization>,
    /// Generated images.
    pub data: Vec<GeneratedImage>,
    /// Per-output failures returned only by best-effort multi-image requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<ImageFailure>,
    /// Revised prompt when available and permitted by policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
    /// Provider usage accounting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Session/thread metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionMetadata>,
    /// Request stage timings.
    pub timings: Timings,
    /// Stable warning codes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Incremental provider progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderEvent {
    /// Provider accepted the operation.
    Started,
    /// Optional textual progress that contains no prompt or credential data.
    Progress {
        /// Provider-safe stage label.
        stage: String,
    },
    /// A bounded partial image payload.
    PartialImage {
        /// Zero-based output index.
        index: u8,
        /// Zero-based partial image index.
        partial_index: u8,
        /// Base64-encoded partial bytes.
        b64_json: String,
    },
    /// Provider operation completed.
    Completed {
        /// Normalized provider response before artifact publication.
        response: Box<ImageResponse>,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn generated_image_round_trips_and_rejects_unknown_fields() {
        let image = GeneratedImage {
            index: 0,
            payload: ImagePayload::B64Json {
                b64_json: "aW1hZ2U=".to_owned(),
            },
            format: OutputFormat::Png,
            width: 1,
            height: 1,
            bytes: 5,
            sha256: "0".repeat(64),
            generation_ms: Some(12),
        };
        let encoded = serde_json::to_value(&image).unwrap();
        assert_eq!(
            serde_json::from_value::<GeneratedImage>(encoded.clone()).unwrap(),
            image
        );

        let mut unknown = encoded.as_object().unwrap().clone();
        unknown.insert("unexpected".to_owned(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<GeneratedImage>(serde_json::Value::Object(unknown)).is_err()
        );
    }
}
