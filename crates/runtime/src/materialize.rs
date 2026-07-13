//! Independent provider-output verification and delivery projection.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use imagegen_bridge_artifacts::{
    ArtifactStore, ImageLimits, ImageMetadata, RemoteImageFetcher, inspect_image,
};
use imagegen_bridge_core::{
    BridgeError, CompatibilityMode, ErrorCode, GeneratedImage, GenerationParameters, ImagePayload,
    ImageResponse, ImageSize, MultiImageFailurePolicy, Normalization, OutputFormat, OutputOptions,
    ResponseFormat,
};
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
                let stored = store.publish(
                    &image.bytes,
                    output.filename_prefix.as_deref(),
                    Some(image.metadata.format),
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
        })
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

fn protocol_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use imagegen_bridge_core::{GenerationParameters, ImageFailure, Timings};

    use super::*;

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
}
