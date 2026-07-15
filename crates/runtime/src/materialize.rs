//! Independent provider-output verification and delivery projection.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use imagegen_bridge_artifacts::{
    ArtifactPublication, ArtifactStore, ImageLimits, ImageMetadata, MAX_EMBEDDED_METADATA_BYTES,
    RemoteImageFetcher, StoredArtifactContent, detect_border_chroma_key, embed_image_metadata,
    inspect_image, inspect_transparent_alpha, remove_chroma_key, thumbnail_png,
};
use imagegen_bridge_core::{
    BridgeError, ErrorCode, GeneratedImage, GenerationParameters, ImageOperation, ImagePayload,
    ImageRequest, ImageResponse, ImageSize, MultiImageFailurePolicy, Normalization, OutputFormat,
    OutputOptions, RequestPolicies, ResponseFormat,
};
use serde::Serialize;
use url::Url;

use crate::transparency::TransparencyPlan;

/// Output verification, remote retrieval, and artifact delivery configuration.
#[derive(Clone)]
pub struct MaterializationConfig {
    /// Independent decoder limits for every provider output.
    pub image_limits: ImageLimits,
    /// Maximum encoded base64 characters accepted from a provider.
    pub max_base64_chars: usize,
    /// Maximum aggregate provider, decoded, and projected response payload bytes.
    pub max_response_bytes: usize,
    /// Optional bridge-owned artifact store.
    pub artifact_store: Option<Arc<ArtifactStore>>,
    /// Optional SSRF-resistant fetcher for provider-hosted image URLs.
    pub remote_output_fetcher: Option<RemoteImageFetcher>,
    /// Public base URL used only for bridge-owned URL responses.
    pub public_artifact_base_url: Option<Url>,
}

impl std::fmt::Debug for MaterializationConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MaterializationConfig")
            .field("image_limits", &self.image_limits)
            .field("max_base64_chars", &self.max_base64_chars)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("artifact_store", &self.artifact_store.is_some())
            .field(
                "remote_output_fetcher",
                &self.remote_output_fetcher.is_some(),
            )
            .field("public_artifact_base_url", &self.public_artifact_base_url)
            .finish()
    }
}

impl Default for MaterializationConfig {
    fn default() -> Self {
        Self {
            image_limits: ImageLimits::default(),
            max_base64_chars: 128 * 1024 * 1024,
            max_response_bytes: 256 * 1024 * 1024,
            artifact_store: None,
            remote_output_fetcher: None,
            public_artifact_base_url: None,
        }
    }
}

pub(crate) struct OutputMaterializer {
    config: MaterializationConfig,
}

impl OutputMaterializer {
    pub(crate) fn new(config: MaterializationConfig) -> Result<Self, BridgeError> {
        if config.max_base64_chars == 0 || config.max_response_bytes == 0 {
            return Err(configuration_error(
                "output payload limits must be greater than zero",
            ));
        }
        if let Some(base) = &config.public_artifact_base_url {
            let valid = matches!(base.scheme(), "http" | "https")
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none()
                && base.path().ends_with('/');
            if !valid {
                return Err(configuration_error(
                    "public artifact base URL must be credential-free HTTP(S) ending in a slash",
                ));
            }
            if config.artifact_store.is_none() {
                return Err(configuration_error(
                    "public artifact URL delivery requires an artifact store",
                ));
            }
        }
        Ok(Self { config })
    }

    pub(crate) const fn has_artifact_store(&self) -> bool {
        self.config.artifact_store.is_some()
    }

    pub(crate) fn read_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<StoredArtifactContent, BridgeError> {
        self.config
            .artifact_store
            .as_ref()
            .ok_or_else(|| configuration_error("artifact output is not configured"))?
            .read(artifact_id)
    }

    pub(crate) fn read_thumbnail(
        &self,
        artifact_id: &str,
        maximum_edge: u32,
    ) -> Result<Vec<u8>, BridgeError> {
        let artifact = self.read_artifact(artifact_id)?;
        thumbnail_png(&artifact.bytes, maximum_edge, self.config.image_limits).map_err(|error| {
            BridgeError {
                code: ErrorCode::Artifact,
                ..error
            }
        })
    }

    pub(crate) async fn materialize(
        &self,
        mut response: ImageResponse,
        original: &ImageRequest,
        effective: &ImageRequest,
        transparency: Option<TransparencyPlan>,
    ) -> Result<ImageResponse, BridgeError> {
        Self::validate_output_set(
            &response,
            effective.parameters.n,
            effective.parameters.failure_policy,
        )?;

        let mut aggregate_bytes = response_payload_bytes(&response)?;
        self.check_response_budget(aggregate_bytes)?;
        let provider_images = std::mem::take(&mut response.data);
        let mut verified = Vec::with_capacity(provider_images.len());
        for image in provider_images {
            let mut image = self
                .verify(&image, effective.parameters.output_format)
                .await?;
            if let Some(plan) = transparency {
                let detected_key = detect_border_chroma_key(&image.bytes, self.config.image_limits)
                    .map_err(as_artifact_error)?;
                let mut chroma = plan.chroma;
                chroma.key = detected_key;
                let processed = remove_chroma_key(
                    &image.bytes,
                    effective.parameters.output_format,
                    chroma,
                    self.config.image_limits,
                )
                .map_err(as_artifact_error)?;
                response.normalizations.push(Normalization {
                    field: format!("data[{}].transparency.key_color", image.index),
                    requested: Some(serde_json::json!(plan.chroma.key.hex())),
                    effective: Some(serde_json::json!(detected_key.hex())),
                    reason: "sampled_provider_output_border".to_owned(),
                });
                response.normalizations.push(alpha_normalization(
                    image.index,
                    processed.alpha,
                    "validated_chroma_key_alpha",
                ));
                image.bytes = processed.bytes;
                image.metadata = processed.metadata;
            } else if effective.parameters.background
                == imagegen_bridge_core::Background::Transparent
            {
                let alpha = inspect_transparent_alpha(&image.bytes, self.config.image_limits)
                    .map_err(as_artifact_error)?;
                response.normalizations.push(alpha_normalization(
                    image.index,
                    alpha,
                    "validated_native_alpha",
                ));
            }
            aggregate_bytes = aggregate_bytes
                .checked_add(image.bytes.len())
                .ok_or_else(|| protocol_error("aggregate response size overflowed"))?;
            self.check_response_budget(aggregate_bytes)?;
            verified.push(image);
        }

        Self::reconcile_dimensions(&mut response, &verified, &effective.parameters.size)?;

        let mut projected = Vec::with_capacity(verified.len());
        for mut image in verified {
            if effective.output.metadata.embeds() {
                let encoded = embedded_metadata(original, effective, &response, &image)?;
                let (bytes, metadata) = embed_image_metadata(
                    &image.bytes,
                    image.metadata.format,
                    &encoded,
                    self.config.image_limits,
                )?;
                image.bytes = bytes;
                image.metadata = metadata;
            }
            let projected_image = self.project(image, &effective.output)?;
            aggregate_bytes = aggregate_bytes
                .checked_add(payload_bytes(&projected_image.payload))
                .ok_or_else(|| protocol_error("aggregate response size overflowed"))?;
            self.check_response_budget(aggregate_bytes)?;
            projected.push(projected_image);
        }
        response.data = projected;
        Ok(response)
    }

    fn check_response_budget(&self, bytes: usize) -> Result<(), BridgeError> {
        if bytes > self.config.max_response_bytes {
            return Err(protocol_error(
                "aggregate response payload exceeds the configured byte limit",
            )
            .with_detail("maximum_bytes", self.config.max_response_bytes));
        }
        Ok(())
    }

    pub(crate) fn attach_metadata(
        &self,
        original: &ImageRequest,
        effective: &ImageRequest,
        response: &mut ImageResponse,
    ) -> Result<(), BridgeError> {
        if !effective.output.metadata.writes_sidecar() {
            return Ok(());
        }
        let store = self
            .config
            .artifact_store
            .as_ref()
            .ok_or_else(|| configuration_error("artifact output is not configured"))?;
        let snapshot = response.clone();
        let operation = operation_summary(&original.operation);
        for image in &mut response.data {
            let (id, name) = match &image.payload {
                ImagePayload::Artifact {
                    id,
                    name: Some(name),
                } => (id.as_str(), name.as_str()),
                _ => {
                    return Err(protocol_error(
                        "metadata sidecar policy requires bridge artifact output",
                    ));
                }
            };
            let encoded = serde_json::to_vec_pretty(&ArtifactMetadataSidecar {
                version: 1,
                request_id: &snapshot.id,
                created: snapshot.created,
                operation: &operation,
                original_prompt: &original.prompt,
                effective_prompt: &effective.prompt,
                negative_prompt: original.negative_prompt.as_deref(),
                policies: &effective.policies,
                provider: &snapshot.provider,
                model: &snapshot.model,
                requested: &snapshot.requested,
                effective: &snapshot.effective,
                normalizations: &snapshot.normalizations,
                attempts: &snapshot.attempts,
                revised_prompt: snapshot.revised_prompt.as_deref(),
                usage: snapshot.usage.as_ref(),
                session: snapshot.session.as_ref(),
                timings: &snapshot.timings,
                warnings: &snapshot.warnings,
                image: ArtifactMetadataImage {
                    index: image.index,
                    artifact_name: name,
                    format: image.format,
                    width: image.width,
                    height: image.height,
                    bytes: image.bytes,
                    sha256: &image.sha256,
                    generation_ms: image.generation_ms,
                },
            })
            .map_err(|_| artifact_error("could not encode artifact metadata"))?;
            image.metadata_name = Some(store.attach_metadata(id, name, &encoded)?.name);
        }
        Ok(())
    }

    fn validate_output_set(
        response: &ImageResponse,
        expected_count: u8,
        expected_failure_policy: MultiImageFailurePolicy,
    ) -> Result<(), BridgeError> {
        let allows_failures = expected_failure_policy == MultiImageFailurePolicy::BestEffort;
        if !allows_failures && !response.failures.is_empty() {
            return Err(protocol_error(
                "provider returned partial failures for a fail-fast request",
            ));
        }
        let actual = response.data.len().saturating_add(response.failures.len());
        if actual != usize::from(expected_count) {
            return Err(protocol_error(
                "provider returned a different number of output results than negotiated",
            )
            .with_detail("expected", expected_count)
            .with_detail("actual", actual));
        }
        let mut indices = response
            .data
            .iter()
            .map(|image| image.index)
            .chain(response.failures.iter().map(|failure| failure.index))
            .collect::<Vec<_>>();
        indices.sort_unstable();
        if indices != (0..expected_count).collect::<Vec<_>>() {
            return Err(protocol_error(
                "provider returned duplicate or missing output indices",
            ));
        }
        if !response
            .data
            .windows(2)
            .all(|pair| pair[0].index < pair[1].index)
            || !response
                .failures
                .windows(2)
                .all(|pair| pair[0].index < pair[1].index)
        {
            return Err(protocol_error(
                "provider returned output results in unstable index order",
            ));
        }
        Ok(())
    }

    fn reconcile_dimensions(
        response: &mut ImageResponse,
        verified: &[VerifiedImage],
        expected_size: &ImageSize,
    ) -> Result<(), BridgeError> {
        let Some((expected_width, expected_height)) = expected_size.dimensions() else {
            return Ok(());
        };
        let Some(first) = verified.first() else {
            return Ok(());
        };
        let requested = (expected_width, expected_height);
        if verified
            .iter()
            .all(|image| (image.metadata.width, image.metadata.height) == requested)
        {
            return Ok(());
        }

        let first_actual = (first.metadata.width, first.metadata.height);
        let uniform = verified
            .iter()
            .all(|image| (image.metadata.width, image.metadata.height) == first_actual);
        if uniform {
            let actual_size = ImageSize::exact(first_actual.0, first_actual.1)?;
            response.effective.size = actual_size.clone();
            response.normalizations.push(Normalization {
                field: "parameters.size".to_owned(),
                requested: Some(serde_json::Value::String(expected_size.to_string())),
                effective: Some(serde_json::Value::String(actual_size.to_string())),
                reason: "provider_output_dimensions_differed".to_owned(),
            });
        } else {
            for image in verified {
                let actual = (image.metadata.width, image.metadata.height);
                if actual == requested {
                    continue;
                }
                response.normalizations.push(Normalization {
                    field: format!("data[{}].size", image.index),
                    requested: Some(serde_json::Value::String(expected_size.to_string())),
                    effective: Some(serde_json::Value::String(format!(
                        "{}x{}",
                        actual.0, actual.1
                    ))),
                    reason: "provider_output_dimensions_differed".to_owned(),
                });
            }
        }
        if !response
            .warnings
            .iter()
            .any(|warning| warning == "provider_output_dimensions_differed")
        {
            response
                .warnings
                .push("provider_output_dimensions_differed".to_owned());
        }
        Ok(())
    }

    async fn verify(
        &self,
        image: &GeneratedImage,
        expected_format: OutputFormat,
    ) -> Result<VerifiedImage, BridgeError> {
        let (bytes, metadata) = match &image.payload {
            ImagePayload::B64Json { b64_json } => {
                if b64_json.len() > self.config.max_base64_chars {
                    return Err(protocol_error(
                        "provider image base64 exceeds the configured limit",
                    ));
                }
                let bytes = STANDARD
                    .decode(b64_json.trim())
                    .map_err(|_| protocol_error("provider returned malformed base64 image data"))?;
                let metadata =
                    inspect_image(&bytes, self.config.image_limits).map_err(as_artifact_error)?;
                (bytes, metadata)
            }
            ImagePayload::Url { url } => {
                let fetcher = self.config.remote_output_fetcher.as_ref().ok_or_else(|| {
                    configuration_error("provider URL output retrieval is not configured")
                })?;
                let loaded = fetcher.fetch(url).await.map_err(as_artifact_error)?;
                (loaded.bytes, loaded.metadata)
            }
            ImagePayload::Artifact { .. } | ImagePayload::Metadata => {
                return Err(protocol_error(
                    "provider returned an unverifiable internal payload type",
                ));
            }
        };
        if metadata.format != expected_format {
            return Err(protocol_error(
                "provider output format does not match the negotiated format",
            ));
        }
        if image.format != metadata.format
            || image.width != metadata.width
            || image.height != metadata.height
            || image.bytes != metadata.bytes
            || image.sha256 != metadata.sha256
        {
            return Err(protocol_error(
                "provider output metadata does not match the decoded image",
            ));
        }
        Ok(VerifiedImage {
            index: image.index,
            bytes,
            metadata,
            generation_ms: image.generation_ms,
        })
    }

    fn project(
        &self,
        image: VerifiedImage,
        output: &OutputOptions,
    ) -> Result<GeneratedImage, BridgeError> {
        let payload = match output.response_format {
            ResponseFormat::B64Json => ImagePayload::B64Json {
                b64_json: STANDARD.encode(&image.bytes),
            },
            ResponseFormat::Metadata => ImagePayload::Metadata,
            ResponseFormat::Artifact | ResponseFormat::Url => {
                let public_base = if output.response_format == ResponseFormat::Url {
                    Some(
                        self.config
                            .public_artifact_base_url
                            .as_ref()
                            .ok_or_else(|| {
                                configuration_error("public artifact base URL is not configured")
                            })?,
                    )
                } else {
                    None
                };
                let store = self
                    .config
                    .artifact_store
                    .as_ref()
                    .ok_or_else(|| configuration_error("artifact output is not configured"))?;
                let stored = store.publish_with_options(
                    &image.bytes,
                    output.filename_prefix.as_deref(),
                    Some(image.metadata.format),
                    ArtifactPublication {
                        directory: output.directory.as_deref(),
                        filename: output.filename.as_deref(),
                        collision: output.collision,
                    },
                )?;
                if output.response_format == ResponseFormat::Artifact {
                    ImagePayload::Artifact {
                        id: stored.id,
                        name: Some(stored.name),
                    }
                } else {
                    let base = public_base.ok_or_else(|| {
                        configuration_error("public artifact base URL is not configured")
                    })?;
                    let url = base.join(&stored.name).map_err(|_| {
                        configuration_error("could not construct public artifact URL")
                    })?;
                    ImagePayload::Url {
                        url: url.to_string(),
                    }
                }
            }
        };
        Ok(GeneratedImage {
            index: image.index,
            payload,
            format: image.metadata.format,
            width: image.metadata.width,
            height: image.metadata.height,
            bytes: image.metadata.bytes,
            sha256: image.metadata.sha256,
            generation_ms: image.generation_ms,
            metadata_name: None,
        })
    }
}

fn alpha_normalization(
    index: u8,
    alpha: imagegen_bridge_artifacts::AlphaSummary,
    reason: &str,
) -> Normalization {
    Normalization {
        field: format!("data[{index}].transparency.alpha"),
        requested: None,
        effective: Some(serde_json::json!({
            "total_pixels": alpha.total_pixels,
            "transparent_pixels": alpha.transparent_pixels,
            "partial_pixels": alpha.partial_pixels,
            "opaque_pixels": alpha.opaque_pixels,
            "transparent_corners": alpha.transparent_corners,
        })),
        reason: reason.to_owned(),
    }
}

#[derive(Serialize)]
struct ArtifactMetadataSidecar<'a> {
    version: u8,
    request_id: &'a str,
    created: u64,
    operation: &'a ArtifactOperationSummary,
    original_prompt: &'a str,
    effective_prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    negative_prompt: Option<&'a str>,
    policies: &'a RequestPolicies,
    provider: &'a str,
    model: &'a str,
    requested: &'a GenerationParameters,
    effective: &'a GenerationParameters,
    normalizations: &'a [Normalization],
    #[serde(skip_serializing_if = "slice_is_empty")]
    attempts: &'a [imagegen_bridge_core::ProviderAttempt],
    #[serde(skip_serializing_if = "Option::is_none")]
    revised_prompt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<&'a imagegen_bridge_core::Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a imagegen_bridge_core::SessionMetadata>,
    timings: &'a imagegen_bridge_core::Timings,
    warnings: &'a [String],
    image: ArtifactMetadataImage<'a>,
}

#[derive(Serialize)]
struct EmbeddedArtifactMetadata<'a> {
    version: u8,
    kind: &'static str,
    request_id: &'a str,
    created: u64,
    operation: &'a ArtifactOperationSummary,
    original_prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_prompt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    negative_prompt: Option<&'a str>,
    policies: &'a RequestPolicies,
    provider: &'a str,
    model: &'a str,
    requested: &'a GenerationParameters,
    effective: &'a GenerationParameters,
    #[serde(skip_serializing_if = "slice_is_empty")]
    normalizations: &'a [Normalization],
    #[serde(skip_serializing_if = "Option::is_none")]
    revised_prompt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<&'a imagegen_bridge_core::Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a imagegen_bridge_core::SessionMetadata>,
    timings: EmbeddedMetadataTimings,
    #[serde(skip_serializing_if = "slice_is_empty")]
    warnings: &'a [String],
    image: EmbeddedMetadataImage,
}

#[derive(Serialize)]
struct EmbeddedMetadataTimings {
    #[serde(rename = "queue_ms")]
    queue: u64,
    #[serde(rename = "provider_ms")]
    provider: u64,
    #[serde(rename = "generation_ms", skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
}

#[derive(Serialize)]
struct EmbeddedMetadataImage {
    index: u8,
    format: OutputFormat,
    width: u32,
    height: u32,
}

fn embedded_metadata(
    original: &ImageRequest,
    effective: &ImageRequest,
    response: &ImageResponse,
    image: &VerifiedImage,
) -> Result<Vec<u8>, BridgeError> {
    let operation = operation_summary(&original.operation);
    let mut value = serde_json::to_value(EmbeddedArtifactMetadata {
        version: 1,
        kind: "imagegen_bridge_generation",
        request_id: &response.id,
        created: response.created,
        operation: &operation,
        original_prompt: &original.prompt,
        effective_prompt: (original.prompt != effective.prompt)
            .then_some(effective.prompt.as_str()),
        negative_prompt: original.negative_prompt.as_deref(),
        policies: &effective.policies,
        provider: &response.provider,
        model: &response.model,
        requested: &response.requested,
        effective: &response.effective,
        normalizations: &response.normalizations,
        revised_prompt: response.revised_prompt.as_deref(),
        usage: response.usage.as_ref(),
        session: response.session.as_ref(),
        timings: EmbeddedMetadataTimings {
            queue: response.timings.queue_ms,
            provider: response.timings.provider_ms,
            generation: image.generation_ms,
        },
        warnings: &response.warnings,
        image: EmbeddedMetadataImage {
            index: image.index,
            format: image.metadata.format,
            width: image.metadata.width,
            height: image.metadata.height,
        },
    })
    .map_err(|_| artifact_error("could not encode embedded artifact metadata"))?;
    let mut encoded = serde_json::to_vec(&value)
        .map_err(|_| artifact_error("could not encode embedded artifact metadata"))?;
    if encoded.len() <= MAX_EMBEDDED_METADATA_BYTES {
        return Ok(encoded);
    }

    let object = value
        .as_object_mut()
        .ok_or_else(|| artifact_error("embedded artifact metadata is not an object"))?;
    let mut omitted = Vec::new();
    for field in [
        "usage",
        "session",
        "normalizations",
        "warnings",
        "revised_prompt",
    ] {
        if object.remove(field).is_none() {
            continue;
        }
        omitted.push(field);
        object.insert(
            "omitted_fields".to_owned(),
            serde_json::to_value(&omitted)
                .map_err(|_| artifact_error("could not encode omitted metadata fields"))?,
        );
        encoded = serde_json::to_vec(&object)
            .map_err(|_| artifact_error("could not encode embedded artifact metadata"))?;
        if encoded.len() <= MAX_EMBEDDED_METADATA_BYTES {
            return Ok(encoded);
        }
    }
    Err(artifact_error(
        "embedded artifact metadata exceeds the portable 40 KiB limit",
    ))
}

fn slice_is_empty<T>(value: &&[T]) -> bool {
    value.is_empty()
}

#[derive(Serialize)]
struct ArtifactMetadataImage<'a> {
    index: u8,
    artifact_name: &'a str,
    format: OutputFormat,
    width: u32,
    height: u32,
    bytes: u64,
    sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_ms: Option<u64>,
}

#[derive(Serialize)]
struct ArtifactOperationSummary {
    kind: &'static str,
    input_images: usize,
    reference_images: usize,
    mask: bool,
}

fn operation_summary(operation: &ImageOperation) -> ArtifactOperationSummary {
    match operation {
        ImageOperation::Generate { reference_images } => ArtifactOperationSummary {
            kind: "generate",
            input_images: 0,
            reference_images: reference_images.len(),
            mask: false,
        },
        ImageOperation::Edit {
            images,
            mask,
            reference_images,
        } => ArtifactOperationSummary {
            kind: "edit",
            input_images: images.len(),
            reference_images: reference_images.len(),
            mask: mask.is_some(),
        },
    }
}

struct VerifiedImage {
    index: u8,
    bytes: Vec<u8>,
    metadata: ImageMetadata,
    generation_ms: Option<u64>,
}

fn response_payload_bytes(response: &ImageResponse) -> Result<usize, BridgeError> {
    let mut total = response
        .id
        .len()
        .checked_add(response.provider.len())
        .and_then(|value| value.checked_add(response.model.len()))
        .and_then(|value| value.checked_add(response.revised_prompt.as_deref().map_or(0, str::len)))
        .ok_or_else(|| protocol_error("aggregate response size overflowed"))?;
    for warning in &response.warnings {
        total = total
            .checked_add(warning.len())
            .ok_or_else(|| protocol_error("aggregate response size overflowed"))?;
    }
    for image in &response.data {
        total = total
            .checked_add(payload_bytes(&image.payload))
            .ok_or_else(|| protocol_error("aggregate response size overflowed"))?;
    }
    Ok(total)
}

fn payload_bytes(payload: &ImagePayload) -> usize {
    match payload {
        ImagePayload::B64Json { b64_json } => b64_json.len(),
        ImagePayload::Url { url } => url.len(),
        ImagePayload::Artifact { id, name } => {
            id.len().saturating_add(name.as_deref().map_or(0, str::len))
        }
        ImagePayload::Metadata => 0,
    }
}

fn as_artifact_error(error: BridgeError) -> BridgeError {
    BridgeError {
        code: ErrorCode::Artifact,
        message: "provider output image failed independent verification".to_owned(),
        provider: error.provider,
        details: error.details,
        ..error
    }
}

fn configuration_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Configuration, message)
}

fn artifact_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Artifact, message)
}

fn protocol_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;

    use imagegen_bridge_artifacts::extract_embedded_metadata;
    use imagegen_bridge_core::{
        ArtifactMetadataPolicy, GenerationParameters, ImageFailure, Timings,
    };

    use super::*;
    use crate::transparency::TransparencyPlan;

    const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    fn image(index: u8) -> GeneratedImage {
        GeneratedImage {
            index,
            payload: ImagePayload::Metadata,
            format: OutputFormat::Png,
            width: 1,
            height: 1,
            bytes: 1,
            sha256: "0".repeat(64),
            generation_ms: Some(1),
            metadata_name: None,
        }
    }

    fn keyed_image() -> GeneratedImage {
        let mut rgba = image::RgbaImage::from_pixel(20, 20, image::Rgba([0, 255, 0, 255]));
        for y in 5..15 {
            for x in 5..15 {
                rgba.put_pixel(x, y, image::Rgba([220, 30, 20, 255]));
            }
        }
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        let metadata = inspect_image(&bytes, ImageLimits::default()).unwrap();
        GeneratedImage {
            index: 0,
            payload: ImagePayload::B64Json {
                b64_json: STANDARD.encode(bytes),
            },
            format: metadata.format,
            width: metadata.width,
            height: metadata.height,
            bytes: metadata.bytes,
            sha256: metadata.sha256,
            generation_ms: None,
            metadata_name: None,
        }
    }

    #[tokio::test]
    async fn chroma_postprocessing_is_applied_before_projection() {
        let materializer = OutputMaterializer::new(MaterializationConfig::default()).unwrap();
        let mut request = ImageRequest::generate("isolated red square");
        request.parameters.background = imagegen_bridge_core::Background::Transparent;
        let projected = materializer
            .materialize(
                response(vec![keyed_image()], Vec::new()),
                &request,
                &request,
                Some(TransparencyPlan {
                    chroma: imagegen_bridge_artifacts::ChromaKeyOptions::default(),
                }),
            )
            .await
            .unwrap();
        assert!(matches!(
            projected.data[0].payload,
            ImagePayload::B64Json { .. }
        ));
        let ImagePayload::B64Json { b64_json } = &projected.data[0].payload else {
            return;
        };
        let decoded = image::load_from_memory(&STANDARD.decode(b64_json).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(decoded.get_pixel(10, 10).0, [220, 30, 20, 255]);
        assert!(projected.normalizations.iter().any(|item| {
            item.field == "data[0].transparency.alpha"
                && item.reason == "validated_chroma_key_alpha"
        }));
    }

    fn response(data: Vec<GeneratedImage>, failures: Vec<ImageFailure>) -> ImageResponse {
        ImageResponse {
            id: "test".to_owned(),
            created: 0,
            provider: "test".to_owned(),
            model: "test".to_owned(),
            requested: GenerationParameters::default(),
            effective: GenerationParameters::default(),
            normalizations: Vec::new(),
            attempts: Vec::new(),
            data,
            failures,
            revised_prompt: None,
            usage: None,
            session: None,
            timings: Timings::default(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn accepts_complete_indexed_best_effort_results() {
        let response = response(
            vec![image(0), image(2)],
            vec![ImageFailure {
                index: 1,
                error: BridgeError::new(ErrorCode::Upstream, "failed"),
                generation_ms: 1,
            }],
        );
        OutputMaterializer::validate_output_set(&response, 3, MultiImageFailurePolicy::BestEffort)
            .unwrap();
    }

    #[test]
    fn rejects_partial_failures_in_fail_fast_and_duplicate_indices() {
        let response = response(
            vec![image(0), image(1)],
            vec![ImageFailure {
                index: 1,
                error: BridgeError::new(ErrorCode::Upstream, "failed"),
                generation_ms: 1,
            }],
        );
        assert!(
            OutputMaterializer::validate_output_set(
                &response,
                3,
                MultiImageFailurePolicy::FailFast,
            )
            .is_err()
        );
        assert!(
            OutputMaterializer::validate_output_set(
                &response,
                3,
                MultiImageFailurePolicy::BestEffort,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_outputs_that_exceed_the_aggregate_response_budget() {
        let bytes = STANDARD.decode(ONE_PIXEL_PNG).unwrap();
        let metadata = inspect_image(&bytes, ImageLimits::default()).unwrap();
        let generated = |index| GeneratedImage {
            index,
            payload: ImagePayload::B64Json {
                b64_json: ONE_PIXEL_PNG.to_owned(),
            },
            format: metadata.format,
            width: metadata.width,
            height: metadata.height,
            bytes: metadata.bytes,
            sha256: metadata.sha256.clone(),
            generation_ms: None,
            metadata_name: None,
        };
        let materializer = OutputMaterializer::new(MaterializationConfig {
            max_response_bytes: 120,
            ..MaterializationConfig::default()
        })
        .unwrap();
        let mut request = ImageRequest::generate("two images");
        request.parameters.n = 2;
        let error = materializer
            .materialize(
                response(vec![generated(0), generated(1)], Vec::new()),
                &request,
                &request,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);
        assert!(error.message.contains("aggregate response payload"));
    }

    #[test]
    fn artifact_projection_honors_per_request_placement() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(ArtifactStore::new(root.path(), ImageLimits::default()).unwrap());
        let materializer = OutputMaterializer::new(MaterializationConfig {
            artifact_store: Some(Arc::clone(&store)),
            ..MaterializationConfig::default()
        })
        .unwrap();
        let bytes = STANDARD.decode(ONE_PIXEL_PNG).unwrap();
        let metadata = inspect_image(&bytes, ImageLimits::default()).unwrap();
        let output = OutputOptions {
            response_format: ResponseFormat::Artifact,
            directory: Some("portraits".to_owned()),
            filename: Some("woman.png".to_owned()),
            metadata: ArtifactMetadataPolicy::Sidecar,
            ..OutputOptions::default()
        };
        let projected = materializer
            .project(
                VerifiedImage {
                    index: 0,
                    bytes,
                    metadata,
                    generation_ms: Some(1),
                },
                &output,
            )
            .unwrap();
        assert!(matches!(
            projected.payload,
            ImagePayload::Artifact { name: Some(ref name), .. } if name == "portraits/woman.png"
        ));
        assert!(root.path().join("portraits/woman.png").is_file());

        let mut original = ImageRequest::generate("an original prompt");
        original.negative_prompt = Some("blur".to_owned());
        original.output = output;
        let mut response = response(vec![projected], Vec::new());
        response.provider = "codex-app-server".to_owned();
        response.model = "gpt-image-2".to_owned();
        materializer
            .attach_metadata(&original, &original, &mut response)
            .unwrap();
        let metadata_name = response.data[0].metadata_name.as_deref().unwrap();
        let sidecar: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join(metadata_name)).unwrap()).unwrap();
        assert_eq!(sidecar["original_prompt"], "an original prompt");
        assert_eq!(sidecar["negative_prompt"], "blur");
        assert_eq!(sidecar["provider"], "codex-app-server");
        assert_eq!(sidecar["model"], "gpt-image-2");
        assert_eq!(sidecar["image"]["artifact_name"], "portraits/woman.png");
        assert_eq!(sidecar["image"]["width"], 1);
    }

    #[tokio::test]
    async fn embedded_policy_updates_payload_checksum_and_carries_generation_contract() {
        let materializer = OutputMaterializer::new(MaterializationConfig::default()).unwrap();
        let bytes = STANDARD.decode(ONE_PIXEL_PNG).unwrap();
        let metadata = inspect_image(&bytes, ImageLimits::default()).unwrap();
        let generated = GeneratedImage {
            index: 0,
            payload: ImagePayload::B64Json {
                b64_json: STANDARD.encode(&bytes),
            },
            format: metadata.format,
            width: metadata.width,
            height: metadata.height,
            bytes: metadata.bytes,
            sha256: metadata.sha256.clone(),
            generation_ms: Some(41),
            metadata_name: None,
        };
        let mut provider_response = response(vec![generated], Vec::new());
        provider_response.provider = "codex-app-server".to_owned();
        provider_response.model = "gpt-image-2".to_owned();
        provider_response.timings.queue_ms = 3;
        provider_response.timings.provider_ms = 40;
        let mut request = ImageRequest::generate("a red paper fox");
        request.output.metadata = ArtifactMetadataPolicy::Embedded;

        let projected = materializer
            .materialize(provider_response, &request, &request, None)
            .await
            .unwrap();
        let b64_json = match &projected.data[0].payload {
            ImagePayload::B64Json { b64_json } => Some(b64_json),
            _ => None,
        }
        .unwrap();
        let embedded = STANDARD.decode(b64_json).unwrap();
        let record: serde_json::Value = serde_json::from_slice(
            &extract_embedded_metadata(&embedded, ImageLimits::default())
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(record["kind"], "imagegen_bridge_generation");
        assert_eq!(record["original_prompt"], "a red paper fox");
        assert_eq!(record["provider"], "codex-app-server");
        assert_eq!(record["model"], "gpt-image-2");
        assert_eq!(record["timings"]["provider_ms"], 40);
        assert_eq!(record["timings"]["generation_ms"], 41);
        assert_ne!(projected.data[0].sha256, metadata.sha256);
        assert_eq!(
            projected.data[0].sha256,
            inspect_image(&embedded, ImageLimits::default())
                .unwrap()
                .sha256
        );
    }

    #[tokio::test]
    async fn combined_policy_publishes_embedded_xmp_and_matching_sidecar() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(ArtifactStore::new(root.path(), ImageLimits::default()).unwrap());
        let materializer = OutputMaterializer::new(MaterializationConfig {
            artifact_store: Some(Arc::clone(&store)),
            ..MaterializationConfig::default()
        })
        .unwrap();
        let bytes = STANDARD.decode(ONE_PIXEL_PNG).unwrap();
        let metadata = inspect_image(&bytes, ImageLimits::default()).unwrap();
        let generated = GeneratedImage {
            index: 0,
            payload: ImagePayload::B64Json {
                b64_json: STANDARD.encode(&bytes),
            },
            format: metadata.format,
            width: metadata.width,
            height: metadata.height,
            bytes: metadata.bytes,
            sha256: metadata.sha256,
            generation_ms: Some(7),
            metadata_name: None,
        };
        let mut request = ImageRequest::generate("a glass lighthouse");
        request.output = OutputOptions {
            response_format: ResponseFormat::Artifact,
            filename: Some("lighthouse.png".to_owned()),
            metadata: ArtifactMetadataPolicy::SidecarAndEmbedded,
            ..OutputOptions::default()
        };
        let mut provider_response = response(vec![generated], Vec::new());
        provider_response.provider = "codex-app-server".to_owned();
        provider_response.model = "gpt-image-2".to_owned();
        let mut projected = materializer
            .materialize(provider_response, &request, &request, None)
            .await
            .unwrap();
        materializer
            .attach_metadata(&request, &request, &mut projected)
            .unwrap();

        let (id, name) = match &projected.data[0].payload {
            ImagePayload::Artifact { id, name } => Some((id, name)),
            _ => None,
        }
        .unwrap();
        assert_eq!(name.as_deref(), Some("lighthouse.png"));
        let stored = store.read(id).unwrap();
        let embedded: serde_json::Value = serde_json::from_slice(
            &extract_embedded_metadata(&stored.bytes, ImageLimits::default())
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(embedded["original_prompt"], "a glass lighthouse");
        assert_eq!(stored.metadata.sha256, projected.data[0].sha256);
        assert!(
            root.path()
                .join(projected.data[0].metadata_name.as_deref().unwrap())
                .is_file()
        );
    }

    #[test]
    fn embedded_record_reports_optional_fields_omitted_for_container_portability() {
        let bytes = STANDARD.decode(ONE_PIXEL_PNG).unwrap();
        let metadata = inspect_image(&bytes, ImageLimits::default()).unwrap();
        let image = VerifiedImage {
            index: 0,
            bytes,
            metadata,
            generation_ms: Some(9),
        };
        let request = ImageRequest::generate("a small prompt");
        let mut provider_response = response(Vec::new(), Vec::new());
        provider_response.revised_prompt = Some("r".repeat(50 * 1024));

        let encoded = embedded_metadata(&request, &request, &provider_response, &image).unwrap();
        assert!(encoded.len() <= MAX_EMBEDDED_METADATA_BYTES);
        let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert!(value.get("revised_prompt").is_none());
        assert!(
            value["omitted_fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "revised_prompt")
        );
    }
}
