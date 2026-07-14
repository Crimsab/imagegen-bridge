//! Independent provider-output verification and delivery projection.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use imagegen_bridge_artifacts::{
    ArtifactPublication, ArtifactStore, ImageLimits, ImageMetadata, RemoteImageFetcher,
    inspect_image,
};
use imagegen_bridge_core::{
    ArtifactMetadataPolicy, BridgeError, CompatibilityMode, ErrorCode, GeneratedImage,
    GenerationParameters, ImageOperation, ImagePayload, ImageRequest, ImageResponse, ImageSize,
    MultiImageFailurePolicy, Normalization, OutputFormat, OutputOptions, RequestPolicies,
    ResponseFormat,
};
use serde::Serialize;
use url::Url;

/// Output verification, remote retrieval, and artifact delivery configuration.
#[derive(Clone)]
pub struct MaterializationConfig {
    /// Independent decoder limits for every provider output.
    pub image_limits: ImageLimits,
    /// Maximum encoded base64 characters accepted from a provider.
    pub max_base64_chars: usize,
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
        if config.max_base64_chars == 0 {
            return Err(configuration_error(
                "maximum output base64 size must be greater than zero",
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

    pub(crate) async fn materialize(
        &self,
        mut response: ImageResponse,
        output: &OutputOptions,
        expected: &GenerationParameters,
        compatibility: CompatibilityMode,
    ) -> Result<ImageResponse, BridgeError> {
        Self::validate_output_set(&response, expected.n, expected.failure_policy)?;

        let mut verified = Vec::with_capacity(response.data.len());
        for image in &response.data {
            verified.push(self.verify(image, expected.output_format).await?);
        }

        Self::reconcile_dimensions(&mut response, &verified, &expected.size, compatibility)?;

        let mut projected = Vec::with_capacity(verified.len());
        for image in verified {
            projected.push(self.project(image, output)?);
        }
        response.data = projected;
        Ok(response)
    }

    pub(crate) fn attach_metadata(
        &self,
        original: &ImageRequest,
        effective: &ImageRequest,
        response: &mut ImageResponse,
    ) -> Result<(), BridgeError> {
        if effective.output.metadata == ArtifactMetadataPolicy::None {
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
                        "metadata sidecars require bridge artifact output",
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
        compatibility: CompatibilityMode,
    ) -> Result<(), BridgeError> {
        let Some((expected_width, expected_height)) = expected_size.dimensions() else {
            return Ok(());
        };
        let Some(first) = verified.first() else {
            return Ok(());
        };
        let actual = (first.metadata.width, first.metadata.height);
        if verified
            .iter()
            .any(|image| (image.metadata.width, image.metadata.height) != actual)
        {
            return Err(protocol_error(
                "provider returned inconsistent dimensions across generated images",
            ));
        }
        if actual == (expected_width, expected_height) {
            return Ok(());
        }
        if compatibility == CompatibilityMode::Strict {
            return Err(protocol_error(
                "provider output dimensions do not match the negotiated size",
            )
            .with_detail("expected", expected_size.to_string())
            .with_detail("actual", format!("{}x{}", actual.0, actual.1)));
        }
        let actual_size = ImageSize::exact(actual.0, actual.1)?;
        response.effective.size = actual_size.clone();
        response.normalizations.push(Normalization {
            field: "parameters.size".to_owned(),
            requested: Some(serde_json::Value::String(expected_size.to_string())),
            effective: Some(serde_json::Value::String(actual_size.to_string())),
            reason: "provider_output_dimensions_differed".to_owned(),
        });
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

    use imagegen_bridge_core::{GenerationParameters, ImageFailure, Timings};

    use super::*;

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

    fn response(data: Vec<GeneratedImage>, failures: Vec<ImageFailure>) -> ImageResponse {
        ImageResponse {
            id: "test".to_owned(),
            created: 0,
            provider: "test".to_owned(),
            model: "test".to_owned(),
            requested: GenerationParameters::default(),
            effective: GenerationParameters::default(),
            normalizations: Vec::new(),
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
}
