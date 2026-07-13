//! Experimental provider for advanced image parameters over Codex OAuth.

use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt as _;
use imagegen_bridge_artifacts::{
    ImageLimits, InputLoader, LoadedImage, RemoteImageFetcher, inspect_image,
};
use imagegen_bridge_core::{
    Background, BridgeError, ErrorCode, GeneratedImage, ImageOperation, ImagePayload,
    ImageProvider, ImageRequest, ImageResponse, ImageSize, ImageSource, InputCapabilities,
    Moderation, OutputFormat, ProviderCapabilities, ProviderContext, ProviderDescriptor, Quality,
    RequestLimits, RevisedPromptPolicy, SizeCapabilities, SupportLevel, Timings, U8Range, Usage,
    negotiate_request, validate_request,
};
use reqwest::{Client, StatusCode, Url, header};
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};

use crate::{
    CodexCredentialSource, SseDecoder, SseLimits,
    events::{CallResult, EventState, merge_usage, process_event},
};

const DEFAULT_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_RESPONSES_MODEL: &str = "gpt-5.5";
const DEFAULT_IMAGE_MODEL: &str = "gpt-image-2";
const SUPPORTED_IMAGE_MODELS: [&str; 4] = [
    "gpt-image-2",
    "gpt-image-1.5",
    "gpt-image-1",
    "gpt-image-1-mini",
];
const INSTRUCTIONS: &str = "You are an image generation assistant inside the Codex backend. Invoke the image_generation tool exactly once and do not answer with text only.";

/// Experimental provider configuration.
#[derive(Clone)]
pub struct CodexResponsesConfig {
    /// Private upstream Responses endpoint.
    pub endpoint: Url,
    /// Chat model that orchestrates the image tool.
    pub responses_model: String,
    /// Image model requested inside the tool.
    pub image_model: String,
    /// Secure loader for local and inline references.
    pub input_loader: Arc<InputLoader>,
    /// Optional SSRF-resistant URL fetcher.
    pub remote_fetcher: Option<RemoteImageFetcher>,
    /// Limits for output verification.
    pub image_limits: ImageLimits,
    /// Streaming parser limits.
    pub sse_limits: SseLimits,
    /// Maximum base64 characters accepted per output.
    pub max_base64_chars: usize,
    /// Maximum aggregate decoded reference bytes.
    pub max_reference_bytes: u64,
}

impl std::fmt::Debug for CodexResponsesConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexResponsesConfig")
            .field("endpoint", &self.endpoint)
            .field("responses_model", &self.responses_model)
            .field("image_model", &self.image_model)
            .field("input_loader", &self.input_loader)
            .field("remote_fetcher", &self.remote_fetcher)
            .field("image_limits", &self.image_limits)
            .field("sse_limits", &self.sse_limits)
            .field("max_base64_chars", &self.max_base64_chars)
            .field("max_reference_bytes", &self.max_reference_bytes)
            .finish()
    }
}

impl CodexResponsesConfig {
    /// Builds production defaults around explicit credential/input sources.
    pub fn production(input_loader: Arc<InputLoader>) -> Result<Self, BridgeError> {
        Ok(Self {
            endpoint: Url::parse(DEFAULT_ENDPOINT).map_err(|_| {
                BridgeError::new(ErrorCode::Internal, "default Codex endpoint is invalid")
            })?,
            responses_model: DEFAULT_RESPONSES_MODEL.to_owned(),
            image_model: DEFAULT_IMAGE_MODEL.to_owned(),
            input_loader,
            remote_fetcher: None,
            image_limits: ImageLimits::default(),
            sse_limits: SseLimits::default(),
            max_base64_chars: 128 * 1024 * 1024,
            max_reference_bytes: 64 * 1024 * 1024,
        })
    }
}

/// Advanced Codex OAuth image provider.
pub struct CodexResponsesProvider {
    credentials: Arc<dyn CodexCredentialSource>,
    client: Client,
    config: CodexResponsesConfig,
}

impl std::fmt::Debug for CodexResponsesProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexResponsesProvider")
            .field("credentials", &"[REDACTED SOURCE]")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl CodexResponsesProvider {
    /// Creates a provider with redirects and ambient proxies disabled.
    pub fn new(
        credentials: Arc<dyn CodexCredentialSource>,
        config: CodexResponsesConfig,
    ) -> Result<Self, BridgeError> {
        if config.endpoint.scheme() != "https"
            && !matches!(
                config.endpoint.host_str(),
                Some("127.0.0.1" | "localhost" | "::1")
            )
        {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "Codex Responses endpoint must use HTTPS",
            ));
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| {
                BridgeError::new(
                    ErrorCode::Configuration,
                    "could not initialize Codex Responses HTTP client",
                )
            })?;
        Ok(Self {
            credentials,
            client,
            config,
        })
    }

    async fn execute_inner(
        &self,
        request: ImageRequest,
        context: ProviderContext,
    ) -> Result<ImageResponse, BridgeError> {
        validate_request(&request, RequestLimits::default())?;
        let image_model = self.selected_image_model(request.routing.model.as_deref())?;
        let negotiated = negotiate_request(&request, &capabilities(image_model)?)?;
        let request = negotiated.effective_request;
        let user_content = self.input_content(&request).await?;
        let started = Instant::now();
        let mut images = Vec::with_capacity(usize::from(request.parameters.n));
        let mut revised_prompt = None;
        let mut usage = Usage::default();
        for _ in 0..request.parameters.n {
            let result = self
                .call_once(&request, image_model, &user_content, &context)
                .await?;
            revised_prompt = result.revised_prompt.or(revised_prompt);
            merge_usage(&mut usage, &result.usage);
            images.push(self.normalized_image(&result.b64_json)?);
        }
        if request.policies.revised_prompt == RevisedPromptPolicy::Require
            && revised_prompt.is_none()
        {
            return Err(BridgeError::new(
                ErrorCode::Upstream,
                "Codex response did not include the required revised prompt",
            )
            .with_provider("codex-responses"));
        }
        if request.policies.revised_prompt == RevisedPromptPolicy::Omit {
            revised_prompt = None;
        }
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(ImageResponse {
            id: context.request_id,
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
            provider: "codex-responses".to_owned(),
            model: image_model.to_owned(),
            requested: negotiated.requested,
            effective: request.parameters,
            normalizations: negotiated.normalizations,
            data: images,
            revised_prompt,
            usage: Some(usage),
            session: None,
            timings: Timings {
                provider_ms: elapsed,
                total_ms: elapsed,
                ..Timings::default()
            },
            warnings: vec!["experimental_private_upstream".to_owned()],
        })
    }

    async fn input_content(&self, request: &ImageRequest) -> Result<Vec<Value>, BridgeError> {
        let mut content = vec![json!({"type": "input_text", "text": request.prompt})];
        let inputs = request_images(request);
        let mut total = 0_u64;
        for input in inputs {
            let image = match &input.source {
                ImageSource::Url { url } => {
                    let fetcher = self.config.remote_fetcher.as_ref().ok_or_else(|| {
                        BridgeError::new(ErrorCode::Input, "remote image inputs are disabled")
                    })?;
                    fetcher.fetch(url).await?
                }
                _ => self.config.input_loader.load(input)?,
            };
            total = total.checked_add(image.metadata.bytes).ok_or_else(|| {
                BridgeError::new(ErrorCode::Input, "reference image bytes overflowed")
            })?;
            if total > self.config.max_reference_bytes {
                return Err(BridgeError::new(
                    ErrorCode::Input,
                    "aggregate reference images exceed the configured byte limit",
                ));
            }
            content.push(json!({
                "type": "input_image",
                "image_url": image_data_url(&image),
                "detail": "auto"
            }));
        }
        Ok(content)
    }

    async fn call_once(
        &self,
        request: &ImageRequest,
        image_model: &str,
        user_content: &[Value],
        context: &ProviderContext,
    ) -> Result<CallResult, BridgeError> {
        let credentials = self.credentials.load().await?;
        let body = self.request_body(request, image_model, user_content);
        let remaining = context.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BridgeError::new(
                ErrorCode::Timeout,
                "request deadline elapsed",
            ));
        }
        let mut builder = self
            .client
            .post(self.config.endpoint.clone())
            .timeout(remaining)
            .header(header::ACCEPT, "text/event-stream")
            .header(header::CONTENT_TYPE, "application/json")
            .header("originator", "imagegen-bridge")
            .bearer_auth(credentials.access_token.expose_secret())
            .json(&body);
        if let Some(account_id) = credentials.account_id {
            builder = builder.header("ChatGPT-Account-Id", account_id.expose_secret());
        }
        let response = tokio::select! {
            response = builder.send() => response.map_err(|error| http_transport_error(&error))?,
            () = context.cancellation.cancelled() => {
                return Err(BridgeError::new(ErrorCode::Cancelled, "Codex Responses request was cancelled"));
            }
        };
        if !response.status().is_success() {
            return Err(status_error(response.status()));
        }
        // The private Codex endpoint currently omits Content-Type even though the
        // successful body is an SSE stream. The bounded decoder below validates
        // the actual framing and event payloads instead of trusting MIME metadata.
        self.consume_stream(response, context).await
    }

    async fn consume_stream(
        &self,
        response: reqwest::Response,
        context: &ProviderContext,
    ) -> Result<CallResult, BridgeError> {
        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::new(self.config.sse_limits);
        let mut state = EventState::default();
        loop {
            let next = tokio::select! {
                next = stream.next() => next,
                () = context.cancellation.cancelled() => {
                    return Err(BridgeError::new(ErrorCode::Cancelled, "Codex Responses stream was cancelled"));
                }
                () = tokio::time::sleep(context.deadline.saturating_duration_since(Instant::now())) => {
                    return Err(BridgeError::new(ErrorCode::Timeout, "Codex Responses stream timed out").retryable(true));
                }
            };
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|error| http_transport_error(&error))?;
            for data in decoder.push(&chunk)? {
                process_event(&data, &mut state, self.config.max_base64_chars)?;
            }
        }
        for data in decoder.finish()? {
            process_event(&data, &mut state, self.config.max_base64_chars)?;
        }
        state.finish()
    }

    fn request_body(
        &self,
        request: &ImageRequest,
        image_model: &str,
        user_content: &[Value],
    ) -> Value {
        let parameters = &request.parameters;
        let mut tool = json!({
            "type": "image_generation",
            "model": image_model,
            "size": parameters.size.to_string(),
            "quality": parameters.quality.to_string(),
            "output_format": parameters.output_format.to_string(),
            "background": parameters.background.to_string(),
            "moderation": parameters.moderation.to_string()
        });
        if let Some(compression) = parameters.output_compression {
            tool["output_compression"] = json!(compression);
        }
        if parameters.partial_images > 0 {
            tool["partial_images"] = json!(parameters.partial_images);
        }
        json!({
            "model": self.config.responses_model,
            "instructions": INSTRUCTIONS,
            "input": [{"type": "message", "role": "user", "content": user_content}],
            "tools": [tool],
            "tool_choice": {
                "type": "allowed_tools",
                "mode": "required",
                "tools": [{"type": "image_generation"}]
            },
            "stream": true,
            "store": false
        })
    }

    fn normalized_image(&self, encoded: &str) -> Result<GeneratedImage, BridgeError> {
        if encoded.len() > self.config.max_base64_chars {
            return Err(protocol_error(
                "Codex image base64 exceeds the configured limit",
            ));
        }
        let bytes = STANDARD
            .decode(encoded.trim())
            .map_err(|_| protocol_error("Codex returned malformed base64 image data"))?;
        let metadata =
            inspect_image(&bytes, self.config.image_limits).map_err(|error| BridgeError {
                code: ErrorCode::Protocol,
                provider: Some("codex-responses".to_owned()),
                ..error
            })?;
        Ok(GeneratedImage {
            payload: ImagePayload::B64Json {
                b64_json: encoded.to_owned(),
            },
            format: metadata.format,
            width: metadata.width,
            height: metadata.height,
            bytes: metadata.bytes,
            sha256: metadata.sha256,
        })
    }

    fn selected_image_model<'a>(
        &'a self,
        requested: Option<&'a str>,
    ) -> Result<&'a str, BridgeError> {
        let model = requested.unwrap_or(&self.config.image_model);
        if SUPPORTED_IMAGE_MODELS.contains(&model) {
            Ok(model)
        } else {
            Err(BridgeError::new(
                ErrorCode::UnsupportedCapability,
                "Codex Responses does not support the requested image model",
            )
            .with_provider("codex-responses")
            .with_detail("field", "routing.model")
            .with_detail("requested_model", model)
            .with_detail("supported_models", SUPPORTED_IMAGE_MODELS))
        }
    }

    fn capability_document(
        &self,
        requested: Option<&str>,
    ) -> Result<ProviderCapabilities, BridgeError> {
        capabilities(self.selected_image_model(requested)?)
    }
}

#[async_trait]
impl ImageProvider for CodexResponsesProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            name: "codex-responses".to_owned(),
            display_name: "Codex OAuth Responses (experimental)".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            experimental: true,
        }
    }

    async fn capabilities(&self, model: Option<&str>) -> Result<ProviderCapabilities, BridgeError> {
        self.capability_document(model)
    }

    async fn execute(
        &self,
        request: ImageRequest,
        context: ProviderContext,
    ) -> Result<ImageResponse, BridgeError> {
        self.execute_inner(request, context).await
    }

    async fn check_ready(&self) -> Result<(), BridgeError> {
        self.credentials.load().await.map(|_| ())
    }
}

fn capabilities(model: &str) -> Result<ProviderCapabilities, BridgeError> {
    if !SUPPORTED_IMAGE_MODELS.contains(&model) {
        return Err(BridgeError::new(
            ErrorCode::UnsupportedCapability,
            "Codex Responses does not support the requested image model",
        )
        .with_provider("codex-responses")
        .with_detail("field", "routing.model")
        .with_detail("requested_model", model));
    }
    let references = InputCapabilities {
        support: SupportLevel::Native,
        max_count: 5,
        max_bytes_each: 32 * 1024 * 1024,
        max_bytes_total: 64 * 1024 * 1024,
    };
    let size_pairs: &[(u32, u32)] = if matches!(model, "gpt-image-1" | "gpt-image-1-mini") {
        &[(1024, 1024), (1536, 1024), (1024, 1536)]
    } else {
        &[
            (1024, 1024),
            (1536, 1024),
            (1024, 1536),
            (2048, 2048),
            (2048, 1152),
            (3840, 2160),
            (2160, 3840),
        ]
    };
    let allowed = size_pairs
        .iter()
        .map(|&(width, height)| ImageSize::exact(width, height))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut backgrounds = BTreeSet::from([Background::Auto, Background::Opaque]);
    if model != "gpt-image-2" {
        backgrounds.insert(Background::Transparent);
    }
    Ok(ProviderCapabilities {
        provider: "codex-responses".to_owned(),
        implementation_version: env!("CARGO_PKG_VERSION").to_owned(),
        model: Some(model.to_owned()),
        experimental: true,
        generation: true,
        edits: true,
        count: U8Range { min: 1, max: 4 },
        sizes: SizeCapabilities {
            auto: true,
            allowed,
            arbitrary: false,
            min_edge: None,
            max_edge: None,
            edge_multiple: None,
            min_pixels: None,
            max_pixels: None,
            max_aspect_ratio: None,
        },
        aspect_ratio: SupportLevel::Emulated,
        resolution: SupportLevel::Emulated,
        qualities: BTreeSet::from([Quality::Auto, Quality::Low, Quality::Medium, Quality::High]),
        output_formats: BTreeSet::from([OutputFormat::Png, OutputFormat::Jpeg, OutputFormat::Webp]),
        backgrounds,
        moderation: BTreeSet::from([Moderation::Auto, Moderation::Low]),
        negative_prompt: SupportLevel::Emulated,
        revised_prompt: SupportLevel::Native,
        reference_images: references.clone(),
        edit_images: references,
        masks: InputCapabilities {
            support: SupportLevel::Unsupported,
            max_count: 0,
            max_bytes_each: 0,
            max_bytes_total: 0,
        },
        partial_images: U8Range { min: 0, max: 3 },
        persistent_sessions: false,
        explicit_threads: false,
    })
}

fn request_images(request: &ImageRequest) -> Vec<&imagegen_bridge_core::ImageInput> {
    match &request.operation {
        ImageOperation::Generate { reference_images } => reference_images.iter().collect(),
        ImageOperation::Edit {
            images,
            reference_images,
            ..
        } => images.iter().chain(reference_images).collect(),
    }
}

fn image_data_url(image: &LoadedImage) -> String {
    let media_type = match image.metadata.format {
        OutputFormat::Png => "image/png",
        OutputFormat::Jpeg => "image/jpeg",
        OutputFormat::Webp => "image/webp",
    };
    format!("data:{media_type};base64,{}", STANDARD.encode(&image.bytes))
}

fn status_error(status: StatusCode) -> BridgeError {
    let (code, retryable) = match status {
        StatusCode::UNAUTHORIZED => (ErrorCode::Authentication, false),
        StatusCode::FORBIDDEN => (ErrorCode::PermissionDenied, false),
        StatusCode::REQUEST_TIMEOUT => (ErrorCode::Timeout, false),
        StatusCode::PAYLOAD_TOO_LARGE => (ErrorCode::Input, false),
        StatusCode::TOO_MANY_REQUESTS => (ErrorCode::RateLimited, true),
        status if status.is_server_error() => (ErrorCode::Upstream, true),
        _ => (ErrorCode::Upstream, false),
    };
    BridgeError::new(
        code,
        format!("Codex Responses returned HTTP {}", status.as_u16()),
    )
    .retryable(retryable)
    .with_provider("codex-responses")
    .with_detail("http_status", status.as_u16())
}

fn http_transport_error(error: &reqwest::Error) -> BridgeError {
    let code = if error.is_timeout() {
        ErrorCode::Timeout
    } else {
        ErrorCode::Upstream
    };
    BridgeError::new(code, "Codex Responses transport failed")
        // A transport failure after sending a paid generation request has an
        // ambiguous outcome. Callers may explicitly retry with idempotency,
        // but the provider must not advertise a blind retry as safe.
        .retryable(false)
        .with_provider("codex-responses")
        .with_detail("outcome", "unknown")
}

fn protocol_error(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message).with_provider("codex-responses")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use super::*;
    use imagegen_bridge_core::{
        CompatibilityMode, GenerationParameters, ImageInput, RequestPolicies,
    };
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    struct FakeCredentials;

    #[async_trait]
    impl CodexCredentialSource for FakeCredentials {
        async fn load(&self) -> Result<crate::CodexOAuthCredentials, BridgeError> {
            Ok(crate::CodexOAuthCredentials {
                access_token: secrecy::SecretString::from("token".to_owned()),
                account_id: Some(secrecy::SecretString::from("account".to_owned())),
            })
        }
    }

    fn provider() -> CodexResponsesProvider {
        let loader =
            Arc::new(InputLoader::new(Vec::<PathBuf>::new(), ImageLimits::default()).unwrap());
        let mut config = CodexResponsesConfig::production(loader).unwrap();
        config.endpoint = Url::parse("http://127.0.0.1:1/responses").unwrap();
        CodexResponsesProvider::new(Arc::new(FakeCredentials), config).unwrap()
    }

    async fn mock_responses_server(
        calls: usize,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            for _ in 0..calls {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                let header_end = loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0, "request ended before the headers completed");
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(position) =
                        request.windows(4).position(|value| value == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                assert!(headers.starts_with("POST /responses HTTP/1.1\r\n"));
                assert!(headers.contains("authorization: Bearer token\r\n"));
                assert!(headers.contains("chatgpt-account-id: account\r\n"));
                assert!(headers.contains("originator: imagegen-bridge\r\n"));
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                while request.len() - header_end < content_length {
                    let read = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0, "request ended before the body completed");
                    request.extend_from_slice(&buffer[..read]);
                }
                let body: Value =
                    serde_json::from_slice(&request[header_end..header_end + content_length])
                        .unwrap();
                assert_eq!(body["stream"], true);
                assert_eq!(body["store"], false);
                assert_eq!(body["tools"][0]["type"], "image_generation");

                let events = format!(
                    "data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"image_generation_call\",\"result\":\"{ONE_PIXEL_PNG}\",\"revised_prompt\":\"revised test prompt\"}}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"usage\":{{\"input_tokens\":2,\"output_tokens\":3,\"total_tokens\":5}}}}}}\n\ndata: [DONE]\n\n"
                );
                let response_headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    events.len()
                );
                stream.write_all(response_headers.as_bytes()).await.unwrap();
                for chunk in events.as_bytes().chunks(17) {
                    stream.write_all(chunk).await.unwrap();
                    tokio::task::yield_now().await;
                }
            }
        });
        (address, handle)
    }

    #[test]
    fn advanced_parameters_are_present_in_tool_request() {
        let provider = provider();
        let mut request = ImageRequest::generate("test");
        request.parameters = GenerationParameters {
            size: "1536x1024".parse().unwrap(),
            quality: Quality::High,
            output_format: OutputFormat::Webp,
            output_compression: Some(80),
            background: Background::Opaque,
            moderation: Moderation::Low,
            partial_images: 2,
            ..GenerationParameters::default()
        };
        let body = provider.request_body(
            &request,
            "gpt-image-2",
            &[json!({"type":"input_text","text":"test"})],
        );
        let expected: Value =
            serde_json::from_str(include_str!("../tests/fixtures/advanced-request.json")).unwrap();
        assert_eq!(body, expected);
        let tool = &body["tools"][0];
        assert_eq!(tool["model"], "gpt-image-2");
        assert_eq!(tool["size"], "1536x1024");
        assert_eq!(tool["quality"], "high");
        assert_eq!(tool["output_format"], "webp");
        assert_eq!(tool["output_compression"], 80);
        assert_eq!(tool["partial_images"], 2);
        assert_eq!(body["store"], false);
    }

    #[test]
    fn model_selection_changes_payload_and_model_specific_capabilities() {
        let provider = provider();
        let request = ImageRequest::generate("test");
        let body = provider.request_body(
            &request,
            "gpt-image-1.5",
            &[json!({"type":"input_text","text":"test"})],
        );
        assert_eq!(body["tools"][0]["model"], "gpt-image-1.5");

        let image_two = provider.capability_document(Some("gpt-image-2")).unwrap();
        assert!(!image_two.backgrounds.contains(&Background::Transparent));
        assert_eq!(image_two.sizes.allowed.len(), 7);

        let image_one = provider.capability_document(Some("gpt-image-1")).unwrap();
        assert!(image_one.backgrounds.contains(&Background::Transparent));
        assert_eq!(image_one.sizes.allowed.len(), 3);
    }

    #[test]
    fn unsupported_model_is_rejected_during_capability_discovery() {
        let provider = provider();
        let error = provider
            .capability_document(Some("not-an-image-model"))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedCapability);
        assert_eq!(
            error.details.get("field"),
            Some(&serde_json::Value::String("routing.model".to_owned()))
        );
    }

    #[test]
    fn golden_sse_fixture_handles_heartbeats_unknown_fields_and_event_names() {
        let fixture = include_bytes!("../tests/fixtures/success.sse");
        let mut decoder = SseDecoder::new(SseLimits::default());
        let mut state = EventState::default();
        for chunk in fixture.chunks(11) {
            for event in decoder.push(chunk).unwrap() {
                process_event(&event, &mut state, 100).unwrap();
            }
        }
        for event in decoder.finish().unwrap() {
            process_event(&event, &mut state, 100).unwrap();
        }
        let result = state.finish().unwrap();
        assert_eq!(result.b64_json, "final");
        assert_eq!(result.revised_prompt.as_deref(), Some("revised"));
        assert_eq!(result.usage.total_tokens, Some(5));
    }

    #[test]
    fn app_server_only_session_mode_is_rejected_by_capabilities() {
        let provider = provider();
        let mut request = ImageRequest::generate("test");
        request.session.mode = imagegen_bridge_core::SessionMode::Persistent;
        request.session.key = Some("key".to_owned());
        request.policies = RequestPolicies {
            compatibility: CompatibilityMode::BestEffort,
            ..RequestPolicies::default()
        };
        assert!(negotiate_request(&request, &provider.capability_document(None).unwrap()).is_err());
    }

    #[tokio::test]
    async fn sends_oauth_headers_and_consumes_fragmented_sse_without_mime_header() {
        let (address, server) = mock_responses_server(1).await;
        let loader =
            Arc::new(InputLoader::new(Vec::<PathBuf>::new(), ImageLimits::default()).unwrap());
        let mut config = CodexResponsesConfig::production(loader).unwrap();
        config.endpoint = Url::parse(&format!("http://{address}/responses")).unwrap();
        let provider = CodexResponsesProvider::new(Arc::new(FakeCredentials), config).unwrap();
        let response = provider
            .execute(
                ImageRequest::generate("test prompt"),
                ProviderContext {
                    request_id: "mock-request".to_owned(),
                    deadline: Instant::now() + std::time::Duration::from_secs(10),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(response.data.len(), 1);
        assert_eq!((response.data[0].width, response.data[0].height), (1, 1));
        assert_eq!(
            response.revised_prompt.as_deref(),
            Some("revised test prompt")
        );
        assert_eq!(response.usage.unwrap().total_tokens, Some(5));
    }

    #[tokio::test]
    async fn generates_requested_count_and_aggregates_usage() {
        let (address, server) = mock_responses_server(2).await;
        let loader =
            Arc::new(InputLoader::new(Vec::<PathBuf>::new(), ImageLimits::default()).unwrap());
        let mut config = CodexResponsesConfig::production(loader).unwrap();
        config.endpoint = Url::parse(&format!("http://{address}/responses")).unwrap();
        let provider = CodexResponsesProvider::new(Arc::new(FakeCredentials), config).unwrap();
        let mut request = ImageRequest::generate("two test images");
        request.parameters.n = 2;
        let response = provider
            .execute(
                request,
                ProviderContext {
                    request_id: "mock-request-two".to_owned(),
                    deadline: Instant::now() + std::time::Duration::from_secs(10),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(response.data.len(), 2);
        assert_eq!(response.usage.unwrap().total_tokens, Some(10));
    }

    #[tokio::test]
    #[ignore = "uses the private Codex endpoint and performs a real OAuth image generation"]
    async fn live_codex_responses_generates_a_verified_image() {
        if std::env::var("IMAGEGEN_BRIDGE_LIVE_CODEX_RESPONSES").as_deref() != Ok("1") {
            return;
        }
        let loader =
            Arc::new(InputLoader::new(Vec::<PathBuf>::new(), ImageLimits::default()).unwrap());
        let provider = CodexResponsesProvider::new(
            Arc::new(crate::CodexAuthFile::discover().unwrap()),
            CodexResponsesConfig::production(loader).unwrap(),
        )
        .unwrap();
        provider.check_ready().await.unwrap();
        let mut request = ImageRequest::generate(
            "A single vermilion triangle centered on a plain cream background",
        );
        request.operation = ImageOperation::Generate {
            reference_images: vec![ImageInput {
                source: ImageSource::Base64 {
                    data: ONE_PIXEL_PNG.to_owned(),
                },
                media_type: Some("image/png".to_owned()),
                filename: Some("reference.png".to_owned()),
            }],
        };
        request.parameters.size = "1024x1024".parse().unwrap();
        request.parameters.quality = Quality::Medium;
        request.parameters.output_format = OutputFormat::Png;
        request.parameters.background = Background::Opaque;
        request.parameters.partial_images = 1;
        let response = provider
            .execute(
                request,
                ProviderContext {
                    request_id: "live-codex-responses".to_owned(),
                    deadline: Instant::now() + std::time::Duration::from_secs(240),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(response.data.len(), 1);
        assert!(response.data[0].bytes > 0);
        assert_eq!(response.provider, "codex-responses");
    }
}
