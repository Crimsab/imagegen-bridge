//! Reusable, input-free image request presets.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    BridgeError, ErrorCode, GenerationParameters, ImageInput, ImageOperation, ImageRequest,
    ImageSource, OutputOptions, RequestLimits, RequestPolicies, RoutingOptions, SessionOptions,
    validate_request,
};

/// Operation selected by a preset without retaining source image bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PresetOperation {
    /// Generate a new image.
    #[default]
    Generate,
    /// Edit caller-supplied images.
    Edit,
}

/// Complete reusable request configuration, excluding image inputs and idempotency keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ImagePresetTemplate {
    /// Optional reusable prompt. A caller may replace it when applying the preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Optional reusable negative prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negative_prompt: Option<String>,
    /// Generate or edit operation; image inputs are always supplied at execution time.
    pub operation: PresetOperation,
    /// Image generation parameters.
    pub parameters: GenerationParameters,
    /// Provider and fallback routing.
    pub routing: RoutingOptions,
    /// Conversation behavior.
    pub session: SessionOptions,
    /// Artifact and response configuration.
    pub output: OutputOptions,
    /// Compatibility and prompt policies.
    pub policies: RequestPolicies,
    /// Optional request deadline in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Optional opaque end-user identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl Default for ImagePresetTemplate {
    fn default() -> Self {
        Self {
            prompt: None,
            negative_prompt: None,
            operation: PresetOperation::Generate,
            parameters: GenerationParameters::default(),
            routing: RoutingOptions::default(),
            session: SessionOptions::default(),
            output: OutputOptions::default(),
            policies: RequestPolicies::default(),
            timeout_ms: None,
            user: None,
        }
    }
}

impl ImagePresetTemplate {
    /// Captures reusable configuration from a request while dropping image inputs and idempotency.
    #[must_use]
    pub fn from_request(request: &ImageRequest) -> Self {
        Self {
            prompt: (!request.prompt.is_empty()).then(|| request.prompt.clone()),
            negative_prompt: request.negative_prompt.clone(),
            operation: match request.operation {
                ImageOperation::Generate { .. } => PresetOperation::Generate,
                ImageOperation::Edit { .. } => PresetOperation::Edit,
            },
            parameters: request.parameters.clone(),
            routing: request.routing.clone(),
            session: request.session.clone(),
            output: request.output.clone(),
            policies: request.policies.clone(),
            timeout_ms: request.timeout_ms,
            user: request.user.clone(),
        }
    }

    /// Builds an input-free request, using `prompt` before the stored prompt when supplied.
    pub fn request(&self, prompt: Option<String>) -> Result<ImageRequest, BridgeError> {
        let prompt = prompt
            .or_else(|| self.prompt.clone())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                BridgeError::new(
                    ErrorCode::InvalidRequest,
                    "preset application requires a prompt or a stored preset prompt",
                )
                .with_detail("field", "prompt")
            })?;
        let operation = match self.operation {
            PresetOperation::Generate => ImageOperation::Generate {
                reference_images: Vec::new(),
            },
            PresetOperation::Edit => ImageOperation::Edit {
                images: Vec::new(),
                mask: None,
                reference_images: Vec::new(),
            },
        };
        Ok(ImageRequest {
            version: crate::CONTRACT_VERSION.to_owned(),
            prompt,
            negative_prompt: self.negative_prompt.clone(),
            operation,
            parameters: self.parameters.clone(),
            routing: self.routing.clone(),
            session: self.session.clone(),
            output: self.output.clone(),
            policies: self.policies.clone(),
            idempotency_key: None,
            timeout_ms: self.timeout_ms,
            user: self.user.clone(),
        })
    }
}

/// Create payload for a named preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImagePresetCreate {
    /// Stable URL- and CLI-safe preset name.
    pub name: String,
    /// Optional human explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Complete reusable configuration.
    pub template: ImagePresetTemplate,
}

/// Full replacement payload for a preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImagePresetWrite {
    /// Optional human explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Complete reusable configuration.
    pub template: ImagePresetTemplate,
}

/// Persisted named preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImagePreset {
    /// Stable preset name.
    pub name: String,
    /// Optional human explanation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Complete reusable configuration.
    pub template: ImagePresetTemplate,
    /// Unix creation timestamp in seconds.
    pub created: u64,
    /// Unix last-update timestamp in seconds.
    pub updated: u64,
}

/// Cursor-paginated preset collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImagePresetPage {
    /// Presets in stable name order.
    pub items: Vec<ImagePreset>,
    /// Opaque cursor for the next page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Validates a preset resource name.
pub fn validate_preset_name(name: &str) -> Result<(), BridgeError> {
    let valid = (1..=64).contains(&name.len())
        && name.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => true,
            b'.' | b'_' | b'-' => index > 0,
            _ => false,
        });
    if valid {
        Ok(())
    } else {
        Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "preset name must be 1-64 characters using letters, numbers, dot, underscore, or hyphen",
        )
        .with_detail("field", "name"))
    }
}

/// Validates bounded preset metadata independent of provider capabilities.
pub fn validate_preset_write(write: &ImagePresetWrite) -> Result<(), BridgeError> {
    if write.description.as_ref().is_some_and(|description| {
        description.len() > 512 || description.chars().any(char::is_control)
    }) {
        return Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "preset description must contain at most 512 bytes and no control characters",
        )
        .with_detail("field", "description"));
    }
    if write
        .template
        .prompt
        .as_ref()
        .is_some_and(|prompt| prompt.trim().is_empty())
    {
        return Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "stored preset prompt cannot be empty",
        )
        .with_detail("field", "template.prompt"));
    }
    let mut request = write
        .template
        .request(Some("preset validation".to_owned()))?;
    let placeholder = || ImageInput {
        source: ImageSource::Base64 {
            data: "AA==".to_owned(),
        },
        media_type: Some("image/png".to_owned()),
        filename: Some("preset-input.png".to_owned()),
    };
    match &mut request.operation {
        ImageOperation::Generate { reference_images }
            if request.parameters.input_fidelity.is_some()
                || request.parameters.action == crate::ImageAction::Edit =>
        {
            reference_images.push(placeholder());
        }
        ImageOperation::Edit { images, .. } => images.push(placeholder()),
        ImageOperation::Generate { .. } => {}
    }
    validate_request(&request, RequestLimits::default())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_round_trip_drops_inputs_and_idempotency() -> Result<(), BridgeError> {
        let mut request = ImageRequest::generate("portrait");
        request.idempotency_key = Some("one-shot".to_owned());
        let template = ImagePresetTemplate::from_request(&request);
        let restored = template.request(None)?;
        assert_eq!(restored.prompt, "portrait");
        assert_eq!(restored.idempotency_key, None);
        assert!(restored.operation.reference_images().is_empty());
        Ok(())
    }

    #[test]
    fn preset_names_are_portable() {
        assert!(validate_preset_name("portrait.high-v2").is_ok());
        assert!(validate_preset_name("../escape").is_err());
        assert!(validate_preset_name("spaces are not portable").is_err());
    }

    #[test]
    fn preset_validation_rejects_intrinsically_invalid_configuration() {
        let mut template = ImagePresetTemplate::default();
        template.parameters.n = 0;
        let write = ImagePresetWrite {
            description: None,
            template,
        };
        assert!(validate_preset_write(&write).is_err());
    }
}
