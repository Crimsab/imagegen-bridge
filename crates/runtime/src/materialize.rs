//! Independent provider-output verification and delivery projection.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use imagegen_bridge_artifacts::{
    ArtifactStore, ImageLimits, ImageMetadata, RemoteImageFetcher, inspect_image,
};
use imagegen_bridge_core::{
    BridgeError, ErrorCode, GeneratedImage, ImagePayload, ImageResponse, OutputFormat,
    OutputOptions, ResponseFormat,
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
        expected_count: u8,
        expected_format: OutputFormat,
    ) -> Result<ImageResponse, BridgeError> {
        if response.data.len() != usize::from(expected_count) {
            return Err(protocol_error(
                "provider returned a different number of images than negotiated",
            )
            .with_detail("expected", expected_count)
            .with_detail("actual", response.data.len()));
        }

        let mut verified = Vec::with_capacity(response.data.len());
        for image in &response.data {
            verified.push(self.verify(image, expected_format).await?);
        }

        let mut projected = Vec::with_capacity(verified.len());
        for image in verified {
            projected.push(self.project(image, output)?);
        }
        response.data = projected;
        Ok(response)
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
        Ok(VerifiedImage { bytes, metadata })
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
            payload,
            format: image.metadata.format,
            width: image.metadata.width,
            height: image.metadata.height,
            bytes: image.metadata.bytes,
            sha256: image.metadata.sha256,
        })
    }
}

struct VerifiedImage {
    bytes: Vec<u8>,
    metadata: ImageMetadata,
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
