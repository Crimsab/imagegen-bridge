//! OpenAI-familiar generation and multipart edit compatibility routes.

use std::str::FromStr;

use axum::{
    Json,
    extract::{
        Extension, Multipart, State, multipart::MultipartRejection, rejection::JsonRejection,
    },
    http::HeaderMap,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use imagegen_bridge_core::{
    AspectRatio, Background, CompatibilityMode, GeneratedImage, ImageAction, ImageInput,
    ImageOperation, ImagePayload, ImageRequest, ImageResponse, ImageSize, InputFidelity,
    Moderation, NegativePromptMode, OutputFormat, Quality, Resolution, ResponseFormat,
    RevisedPromptPolicy, SessionOptions, Usage,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{ApiError, RequestId, ServerState, auth::AuthScope, routes::run_request};

const MAX_TEXT_FIELD_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct CompatibleGenerationRequest {
    prompt: String,
    model: Option<String>,
    n: u8,
    size: ImageSize,
    quality: Quality,
    output_format: OutputFormat,
    output_compression: Option<u8>,
    background: Background,
    moderation: Moderation,
    response_format: CompatibleResponseFormat,
    user: Option<String>,
    imagegen_bridge: CompatibleExtensions,
}

impl Default for CompatibleGenerationRequest {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            model: None,
            n: 1,
            size: ImageSize::default(),
            quality: Quality::default(),
            output_format: OutputFormat::default(),
            output_compression: None,
            background: Background::default(),
            moderation: Moderation::default(),
            response_format: CompatibleResponseFormat::default(),
            user: None,
            imagegen_bridge: CompatibleExtensions::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CompatibleResponseFormat {
    #[default]
    B64Json,
    Url,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CompatibleExtensions {
    provider: Option<String>,
    negative_prompt: Option<String>,
    compatibility: CompatibilityMode,
    negative_prompt_mode: NegativePromptMode,
    revised_prompt: RevisedPromptPolicy,
    aspect_ratio: Option<AspectRatio>,
    resolution: Option<Resolution>,
    partial_images: u8,
    input_fidelity: Option<InputFidelity>,
    action: ImageAction,
    session: SessionOptions,
    reference_images: Vec<ImageInput>,
    filename_prefix: Option<String>,
}

impl CompatibleGenerationRequest {
    fn into_native(self) -> ImageRequest {
        let extensions = self.imagegen_bridge;
        let mut request = ImageRequest::generate(self.prompt);
        request.negative_prompt = extensions.negative_prompt;
        request.operation = ImageOperation::Generate {
            reference_images: extensions.reference_images,
        };
        request.parameters.n = self.n;
        request.parameters.size = self.size;
        request.parameters.aspect_ratio = extensions.aspect_ratio;
        request.parameters.resolution = extensions.resolution;
        request.parameters.quality = self.quality;
        request.parameters.output_format = self.output_format;
        request.parameters.output_compression = self.output_compression;
        request.parameters.background = self.background;
        request.parameters.moderation = self.moderation;
        request.parameters.partial_images = extensions.partial_images;
        request.parameters.input_fidelity = extensions.input_fidelity;
        request.parameters.action = extensions.action;
        request.routing.provider = extensions.provider;
        request.routing.model = self.model;
        request.session = extensions.session;
        request.output.response_format = match self.response_format {
            CompatibleResponseFormat::B64Json => ResponseFormat::B64Json,
            CompatibleResponseFormat::Url => ResponseFormat::Url,
        };
        request.output.filename_prefix = extensions.filename_prefix;
        request.policies.compatibility = extensions.compatibility;
        request.policies.negative_prompt = extensions.negative_prompt_mode;
        request.policies.revised_prompt = extensions.revised_prompt;
        request.user = self.user;
        request
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CompatibleImageResponse {
    created: u64,
    data: Vec<CompatibleImageData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
    imagegen_bridge: CompatibleResponseExtensions,
}

#[derive(Debug, Serialize)]
struct CompatibleImageData {
    #[serde(skip_serializing_if = "Option::is_none")]
    b64_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revised_prompt: Option<String>,
}

#[derive(Debug, Serialize)]
struct CompatibleResponseExtensions {
    id: String,
    provider: String,
    model: String,
    effective: imagegen_bridge_core::GenerationParameters,
    normalizations: Vec<imagegen_bridge_core::Normalization>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<imagegen_bridge_core::SessionMetadata>,
    timings: imagegen_bridge_core::Timings,
    warnings: Vec<String>,
}

impl TryFrom<ImageResponse> for CompatibleImageResponse {
    type Error = imagegen_bridge_core::BridgeError;

    fn try_from(response: ImageResponse) -> Result<Self, Self::Error> {
        let data = response
            .data
            .into_iter()
            .map(|image| compatible_data(image, response.revised_prompt.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            created: response.created,
            data,
            usage: response.usage,
            imagegen_bridge: CompatibleResponseExtensions {
                id: response.id,
                provider: response.provider,
                model: response.model,
                effective: response.effective,
                normalizations: response.normalizations,
                session: response.session,
                timings: response.timings,
                warnings: response.warnings,
            },
        })
    }
}

fn compatible_data(
    image: GeneratedImage,
    revised_prompt: Option<String>,
) -> Result<CompatibleImageData, imagegen_bridge_core::BridgeError> {
    let (b64_json, url) = match image.payload {
        ImagePayload::B64Json { b64_json } => (Some(b64_json), None),
        ImagePayload::Url { url } => (None, Some(url)),
        ImagePayload::Artifact { .. } | ImagePayload::Metadata => {
            return Err(imagegen_bridge_core::BridgeError::new(
                imagegen_bridge_core::ErrorCode::Internal,
                "compatibility response materialization is inconsistent",
            ));
        }
    };
    Ok(CompatibleImageData {
        b64_json,
        url,
        revised_prompt,
    })
}

pub(crate) async fn generate_compatible(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(scope): Extension<AuthScope>,
    headers: HeaderMap,
    payload: Result<Json<CompatibleGenerationRequest>, JsonRejection>,
) -> Result<Json<CompatibleImageResponse>, ApiError> {
    let Json(payload) = payload.map_err(|_| {
        ApiError::bad_request(
            "request body must be valid image generation JSON",
            request_id.clone(),
        )
    })?;
    let response = run_request(
        &state,
        request_id.clone(),
        scope,
        &headers,
        payload.into_native(),
    )
    .await?;
    CompatibleImageResponse::try_from(response)
        .map(Json)
        .map_err(|error| ApiError::from_bridge(error, request_id))
}

#[derive(Default)]
struct EditFields {
    prompt: Option<String>,
    model: Option<String>,
    n: Option<String>,
    size: Option<String>,
    quality: Option<String>,
    output_format: Option<String>,
    output_compression: Option<String>,
    background: Option<String>,
    moderation: Option<String>,
    input_fidelity: Option<String>,
    response_format: Option<String>,
    user: Option<String>,
    provider: Option<String>,
    negative_prompt: Option<String>,
    compatibility: Option<String>,
    revised_prompt: Option<String>,
    session_key: Option<String>,
    images: Vec<ImageInput>,
    references: Vec<ImageInput>,
    mask: Option<ImageInput>,
}

pub(crate) async fn edit_compatible(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(scope): Extension<AuthScope>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Json<CompatibleImageResponse>, ApiError> {
    let mut multipart = multipart.map_err(|_| {
        ApiError::bad_request(
            "request must be valid multipart/form-data",
            request_id.clone(),
        )
    })?;
    let mut fields = EditFields::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::bad_request("multipart body is malformed", request_id.clone()))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if matches!(
            name.as_str(),
            "image" | "image[]" | "reference_image" | "mask"
        ) {
            let filename = field.file_name().map(str::to_owned);
            let media_type = field.content_type().map(str::to_owned);
            let bytes = field.bytes().await.map_err(|_| {
                ApiError::bad_request("multipart image field is malformed", request_id.clone())
            })?;
            let input = ImageInput {
                source: imagegen_bridge_core::ImageSource::Base64 {
                    data: STANDARD.encode(bytes),
                },
                media_type,
                filename,
            };
            match name.as_str() {
                "image" | "image[]" => fields.images.push(input),
                "reference_image" => fields.references.push(input),
                "mask" if fields.mask.is_none() => fields.mask = Some(input),
                "mask" => {
                    return Err(ApiError::bad_request(
                        "multipart mask field must not be repeated",
                        request_id,
                    ));
                }
                _ => {
                    return Err(ApiError::bad_request(
                        "multipart image field is unsupported",
                        request_id,
                    ));
                }
            }
        } else {
            let value = field.text().await.map_err(|_| {
                ApiError::bad_request("multipart text field is malformed", request_id.clone())
            })?;
            if value.len() > MAX_TEXT_FIELD_BYTES {
                return Err(ApiError::bad_request(
                    "multipart text field exceeds the byte limit",
                    request_id,
                ));
            }
            set_text_field(&mut fields, &name, value, request_id.clone())?;
        }
    }
    if fields.images.is_empty() {
        return Err(ApiError::bad_request(
            "multipart edit requires at least one image",
            request_id,
        ));
    }
    let prompt = fields.prompt.take().ok_or_else(|| {
        ApiError::bad_request("multipart edit requires a prompt", request_id.clone())
    })?;
    let mut request = ImageRequest::generate(prompt);
    request.operation = ImageOperation::Edit {
        images: std::mem::take(&mut fields.images),
        mask: fields.mask.take().map(Box::new),
        reference_images: std::mem::take(&mut fields.references),
    };
    apply_edit_fields(&mut request, fields, request_id.clone())?;
    let response = run_request(&state, request_id.clone(), scope, &headers, request).await?;
    CompatibleImageResponse::try_from(response)
        .map(Json)
        .map_err(|error| ApiError::from_bridge(error, request_id))
}

fn set_text_field(
    fields: &mut EditFields,
    name: &str,
    value: String,
    request_id: RequestId,
) -> Result<(), ApiError> {
    let target = match name {
        "prompt" => &mut fields.prompt,
        "model" => &mut fields.model,
        "n" => &mut fields.n,
        "size" => &mut fields.size,
        "quality" => &mut fields.quality,
        "output_format" => &mut fields.output_format,
        "output_compression" => &mut fields.output_compression,
        "background" => &mut fields.background,
        "moderation" => &mut fields.moderation,
        "input_fidelity" => &mut fields.input_fidelity,
        "response_format" => &mut fields.response_format,
        "user" => &mut fields.user,
        "provider" => &mut fields.provider,
        "negative_prompt" => &mut fields.negative_prompt,
        "compatibility" => &mut fields.compatibility,
        "revised_prompt" => &mut fields.revised_prompt,
        "session_key" => &mut fields.session_key,
        _ => {
            return Err(ApiError::bad_request(
                "multipart field is unsupported",
                request_id,
            ));
        }
    };
    if target.replace(value).is_some() {
        return Err(ApiError::bad_request(
            "multipart text field must not be repeated",
            request_id,
        ));
    }
    Ok(())
}

fn apply_edit_fields(
    request: &mut ImageRequest,
    fields: EditFields,
    request_id: RequestId,
) -> Result<(), ApiError> {
    request.routing.model = fields.model;
    request.routing.provider = fields.provider;
    request.negative_prompt = fields.negative_prompt;
    request.user = fields.user;
    if let Some(value) = fields.n {
        request.parameters.n = parse_number(&value, "n", request_id.clone())?;
    }
    if let Some(value) = fields.size {
        request.parameters.size = ImageSize::from_str(&value)
            .map_err(|_| ApiError::bad_request("multipart size is invalid", request_id.clone()))?;
    }
    if let Some(value) = fields.quality {
        request.parameters.quality = parse_enum(&value, "quality", request_id.clone())?;
    }
    if let Some(value) = fields.output_format {
        request.parameters.output_format = parse_enum(&value, "output_format", request_id.clone())?;
    }
    if let Some(value) = fields.output_compression {
        request.parameters.output_compression = Some(parse_number(
            &value,
            "output_compression",
            request_id.clone(),
        )?);
    }
    if let Some(value) = fields.background {
        request.parameters.background = parse_enum(&value, "background", request_id.clone())?;
    }
    if let Some(value) = fields.moderation {
        request.parameters.moderation = parse_enum(&value, "moderation", request_id.clone())?;
    }
    if let Some(value) = fields.input_fidelity {
        request.parameters.input_fidelity = Some(parse_enum::<InputFidelity>(
            &value,
            "input_fidelity",
            request_id.clone(),
        )?);
    }
    if let Some(value) = fields.response_format {
        request.output.response_format = match value.as_str() {
            "b64_json" => ResponseFormat::B64Json,
            "url" => ResponseFormat::Url,
            _ => {
                return Err(ApiError::bad_request(
                    "multipart response_format is invalid",
                    request_id,
                ));
            }
        };
    }
    if let Some(value) = fields.compatibility {
        request.policies.compatibility = parse_enum(&value, "compatibility", request_id.clone())?;
    }
    if let Some(value) = fields.revised_prompt {
        request.policies.revised_prompt = parse_enum(&value, "revised_prompt", request_id.clone())?;
    }
    if let Some(key) = fields.session_key {
        request.session.mode = imagegen_bridge_core::SessionMode::Persistent;
        request.session.key = Some(key);
    }
    Ok(())
}

fn parse_enum<T: DeserializeOwned>(
    value: &str,
    field: &str,
    request_id: RequestId,
) -> Result<T, ApiError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|_| ApiError::bad_request(&format!("multipart {field} is invalid"), request_id))
}

fn parse_number<T: FromStr>(
    value: &str,
    field: &str,
    request_id: RequestId,
) -> Result<T, ApiError> {
    value
        .parse()
        .map_err(|_| ApiError::bad_request(&format!("multipart {field} is invalid"), request_id))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use imagegen_bridge_core::{GenerationParameters, Normalization, SessionMetadata, Timings};
    use serde_json::json;

    use super::*;

    #[test]
    fn generation_compatibility_maps_every_advanced_field() {
        let payload: CompatibleGenerationRequest = serde_json::from_value(json!({
            "prompt": "draw",
            "model": "gpt-image-2",
            "n": 2,
            "size": "1536x1024",
            "quality": "high",
            "output_format": "webp",
            "output_compression": 81,
            "background": "opaque",
            "moderation": "low",
            "response_format": "url",
            "user": "caller",
            "imagegen_bridge": {
                "provider": "codex-responses",
                "negative_prompt": "no text",
                "compatibility": "normalize",
                "negative_prompt_mode": "merge",
                "revised_prompt": "require",
                "aspect_ratio": "3:2",
                "resolution": "2k",
                "partial_images": 2,
                "input_fidelity": "high",
                "action": "edit",
                "session": {"mode":"persistent","key":"gallery"},
                "filename_prefix": "poster"
            }
        }))
        .unwrap();
        let request = payload.into_native();
        assert_eq!(request.parameters.n, 2);
        assert_eq!(request.parameters.size.to_string(), "1536x1024");
        assert_eq!(request.parameters.output_compression, Some(81));
        assert_eq!(request.parameters.input_fidelity, Some(InputFidelity::High));
        assert_eq!(request.parameters.action, ImageAction::Edit);
        assert_eq!(request.routing.provider.as_deref(), Some("codex-responses"));
        assert_eq!(request.negative_prompt.as_deref(), Some("no text"));
        assert_eq!(request.output.response_format, ResponseFormat::Url);
        assert_eq!(request.session.key.as_deref(), Some("gallery"));
        assert_eq!(
            request.policies.revised_prompt,
            RevisedPromptPolicy::Require
        );
    }

    #[test]
    fn compatible_response_preserves_standard_data_and_bridge_metadata() {
        let response = ImageResponse {
            id: "request-1".to_owned(),
            created: 123,
            provider: "codex-app-server".to_owned(),
            model: "gpt-image-2".to_owned(),
            requested: GenerationParameters::default(),
            effective: GenerationParameters::default(),
            normalizations: vec![Normalization {
                field: "quality".to_owned(),
                requested: Some(json!("high")),
                effective: Some(json!("auto")),
                reason: "provider_default".to_owned(),
            }],
            data: vec![GeneratedImage {
                index: 0,
                payload: ImagePayload::B64Json {
                    b64_json: "image".to_owned(),
                },
                format: OutputFormat::Png,
                width: 1,
                height: 1,
                bytes: 1,
                sha256: "00".repeat(32),
                generation_ms: None,
                metadata_name: None,
            }],
            failures: Vec::new(),
            revised_prompt: Some("revised".to_owned()),
            usage: Some(Usage::default()),
            session: Some(SessionMetadata {
                key: Some("gallery".to_owned()),
                thread_id: Some("thread-1".to_owned()),
                reused: true,
            }),
            timings: Timings::default(),
            warnings: vec!["test".to_owned()],
        };
        let compatible = CompatibleImageResponse::try_from(response).unwrap();
        let value = serde_json::to_value(compatible).unwrap();
        assert_eq!(value["created"], 123);
        assert_eq!(value["data"][0]["b64_json"], "image");
        assert_eq!(value["data"][0]["revised_prompt"], "revised");
        assert_eq!(value["imagegen_bridge"]["provider"], "codex-app-server");
        assert_eq!(value["imagegen_bridge"]["session"]["reused"], true);
    }

    #[test]
    fn multipart_edit_maps_input_fidelity() {
        let mut request = ImageRequest::generate("edit");
        apply_edit_fields(
            &mut request,
            EditFields {
                input_fidelity: Some("high".to_owned()),
                ..EditFields::default()
            },
            RequestId("test-request".to_owned()),
        )
        .unwrap();
        assert_eq!(request.parameters.input_fidelity, Some(InputFidelity::High));
    }
}
