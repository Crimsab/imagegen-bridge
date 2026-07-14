//! Configuration-driven application assembly and embeddable builder APIs.

use std::sync::Arc;

#[cfg(feature = "codex-app-server")]
use std::{path::Path, time::Duration};

#[cfg(any(feature = "codex-app-server", feature = "codex-responses"))]
use imagegen_bridge_artifacts::ImageLimits;
use imagegen_bridge_config::BridgeConfig;
use imagegen_bridge_core::{BridgeError, ErrorCode, ImageProvider};
use imagegen_bridge_runtime::{ImagegenRuntime, ProviderRegistry, RuntimeConfig};

/// Fully assembled provider-neutral application runtime.
#[derive(Debug, Clone)]
pub struct BridgeApplication {
    runtime: Arc<ImagegenRuntime>,
}

impl BridgeApplication {
    /// Creates a custom application builder for embedded or third-party providers.
    #[must_use]
    pub fn builder(
        default_provider: impl Into<String>,
        runtime_config: RuntimeConfig,
    ) -> BridgeApplicationBuilder {
        BridgeApplicationBuilder {
            default_provider: default_provider.into(),
            runtime_config,
            providers: Vec::new(),
        }
    }

    /// Builds all enabled first-party providers and mutable runtime state.
    ///
    /// This method may create the artifact directory, session database, and an
    /// owned `codex app-server` child process. Use `BridgeConfig::check` for a
    /// non-mutating validation pass.
    #[cfg_attr(
        not(any(feature = "codex-app-server", feature = "codex-responses")),
        allow(clippy::unused_async)
    )]
    pub async fn from_config(config: BridgeConfig) -> Result<Self, BridgeError> {
        config.validate()?;
        if config.providers.openai.enabled {
            return Err(configuration_error(
                "the OpenAI provider is configured but is not available in this build",
            ));
        }

        #[cfg(any(feature = "codex-app-server", feature = "codex-responses"))]
        let inputs = config.input_components()?;
        let artifact_store = config.artifact_store()?;
        #[cfg(any(feature = "codex-app-server", feature = "codex-responses"))]
        let image_limits = configured_image_limits(&config);
        let providers: Vec<Arc<dyn ImageProvider>> = Vec::new();
        #[cfg(any(feature = "codex-app-server", feature = "codex-responses"))]
        let mut providers = providers;

        if config.providers.codex_app_server.enabled {
            #[cfg(feature = "codex-app-server")]
            {
                use imagegen_bridge_codex_app_server::{
                    AppServerImageProvider, AppServerProviderConfig, AppServerReferenceInputs,
                    CodexProcess, CodexProcessConfig, RpcConfig, SessionBindingStore,
                };
                use imagegen_bridge_runtime::{
                    OutputFanoutConfig, OutputFanoutProvider, SqliteSessionBindingStore,
                };

                let settings = &config.providers.codex_app_server;
                create_parent_directory(&settings.session_database).await?;
                let sessions: Arc<dyn SessionBindingStore> = Arc::new(
                    SqliteSessionBindingStore::open(&settings.session_database, "codex-app-server")
                        .await?,
                );
                let process = Arc::new(
                    CodexProcess::spawn(CodexProcessConfig {
                        executable: settings.executable.clone(),
                        args: settings.args.clone(),
                        cwd: settings.cwd.clone(),
                        rpc: RpcConfig {
                            max_message_bytes: settings.rpc_max_message_bytes,
                            max_notification_bytes: settings.rpc_max_notification_bytes,
                            request_timeout: Duration::from_millis(settings.rpc_timeout_ms),
                            notification_capacity: settings.notification_capacity,
                        },
                        shutdown_timeout: Duration::from_millis(settings.shutdown_timeout_ms),
                        restart_backoff: Duration::from_millis(settings.restart_backoff_ms),
                    })
                    .await?,
                );
                let cwd = settings
                    .cwd
                    .clone()
                    .map_or_else(std::env::current_dir, Ok)
                    .map_err(|_| configuration_error("could not determine provider directory"))?;
                let reference_inputs = AppServerReferenceInputs::new(
                    Arc::clone(&inputs.loader),
                    inputs.remote.clone(),
                    Arc::clone(&artifact_store),
                    64 * 1024 * 1024,
                )?;
                let provider: Arc<dyn ImageProvider> =
                    Arc::new(AppServerImageProvider::with_process_and_inputs(
                        process,
                        sessions,
                        AppServerProviderConfig {
                            codex_model: settings.codex_model.clone(),
                            cwd,
                            image_limits,
                        },
                        reference_inputs,
                    ));
                providers.push(Arc::new(OutputFanoutProvider::new(
                    provider,
                    OutputFanoutConfig {
                        max_outputs: settings.max_outputs,
                        max_parallel_outputs: settings.max_parallel_outputs,
                    },
                )?));
            }
            #[cfg(not(feature = "codex-app-server"))]
            return Err(feature_error("codex-app-server"));
        }

        if config.providers.codex_responses.enabled {
            #[cfg(feature = "codex-responses")]
            {
                use imagegen_bridge_codex_responses::{
                    CodexAuthFile, CodexResponsesConfig, CodexResponsesProvider,
                };
                use url::Url;

                let settings = &config.providers.codex_responses;
                let mut provider_config =
                    CodexResponsesConfig::production(Arc::clone(&inputs.loader))?;
                provider_config.endpoint = Url::parse(&settings.endpoint)
                    .map_err(|_| configuration_error("Codex Responses endpoint is invalid"))?;
                provider_config
                    .responses_model
                    .clone_from(&settings.responses_model);
                provider_config
                    .image_model
                    .clone_from(&settings.image_model);
                provider_config.max_parallel_outputs = settings.max_parallel_outputs;
                provider_config.remote_fetcher.clone_from(&inputs.remote);
                provider_config.image_limits = image_limits;
                provider_config.max_base64_chars = config.artifacts.max_base64_chars;
                providers.push(Arc::new(CodexResponsesProvider::new(
                    Arc::new(CodexAuthFile::discover()?),
                    provider_config,
                )?));
            }
            #[cfg(not(feature = "codex-responses"))]
            return Err(feature_error("codex-responses"));
        }

        let mut runtime_config = config.runtime_config()?;
        runtime_config.materialization.artifact_store = Some(artifact_store);
        Self::builder(config.default_provider, runtime_config)
            .providers(providers)
            .build()
    }

    /// Returns the shared runtime used by all transports.
    #[must_use]
    pub const fn runtime(&self) -> &Arc<ImagegenRuntime> {
        &self.runtime
    }

    /// Drains active calls and shuts down owned providers.
    pub async fn shutdown(&self) -> Result<(), BridgeError> {
        self.runtime.shutdown().await
    }
}

/// Builder for applications embedding arbitrary provider implementations.
pub struct BridgeApplicationBuilder {
    default_provider: String,
    runtime_config: RuntimeConfig,
    providers: Vec<Arc<dyn ImageProvider>>,
}

impl std::fmt::Debug for BridgeApplicationBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BridgeApplicationBuilder")
            .field("default_provider", &self.default_provider)
            .field("runtime_config", &self.runtime_config)
            .field("provider_count", &self.providers.len())
            .finish()
    }
}

impl BridgeApplicationBuilder {
    /// Registers one provider implementation.
    #[must_use]
    pub fn provider(mut self, provider: Arc<dyn ImageProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    /// Registers several provider implementations.
    #[must_use]
    pub fn providers(
        mut self,
        providers: impl IntoIterator<Item = Arc<dyn ImageProvider>>,
    ) -> Self {
        self.providers.extend(providers);
        self
    }

    /// Validates provider registration and constructs fixed-capacity runtime state.
    pub fn build(self) -> Result<BridgeApplication, BridgeError> {
        let registry = ProviderRegistry::new(self.providers, self.default_provider)?;
        let runtime = ImagegenRuntime::new(registry, self.runtime_config)?;
        Ok(BridgeApplication {
            runtime: Arc::new(runtime),
        })
    }
}

#[cfg(feature = "codex-app-server")]
async fn create_parent_directory(path: &Path) -> Result<(), BridgeError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|_| configuration_error("could not create session database directory"))?;
    }
    Ok(())
}

#[cfg(any(feature = "codex-app-server", feature = "codex-responses"))]
const fn configured_image_limits(config: &BridgeConfig) -> ImageLimits {
    let limits = config.artifacts.image;
    ImageLimits {
        max_encoded_bytes: limits.max_encoded_bytes,
        max_edge: limits.max_edge,
        max_pixels: limits.max_pixels,
        max_decode_alloc: limits.max_decode_alloc,
    }
}

#[cfg(any(not(feature = "codex-app-server"), not(feature = "codex-responses")))]
fn feature_error(feature: &str) -> BridgeError {
    configuration_error("an enabled provider is unavailable in this build")
        .with_detail("required_feature", feature)
}

fn configuration_error(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Configuration, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use async_trait::async_trait;
    use imagegen_bridge_core::{
        ImageRequest, ImageResponse, ProviderCapabilities, ProviderContext, ProviderDescriptor,
    };

    use super::*;

    struct FakeProvider;

    #[async_trait]
    impl ImageProvider for FakeProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                name: "fake".to_owned(),
                display_name: "Fake".to_owned(),
                version: "test".to_owned(),
                experimental: false,
                models: vec!["test-image".to_owned()],
            }
        }

        async fn capabilities(
            &self,
            _model: Option<&str>,
        ) -> Result<ProviderCapabilities, BridgeError> {
            Err(BridgeError::new(ErrorCode::Internal, "unused"))
        }

        async fn execute(
            &self,
            _request: ImageRequest,
            _context: ProviderContext,
        ) -> Result<ImageResponse, BridgeError> {
            Err(BridgeError::new(ErrorCode::Internal, "unused"))
        }

        async fn check_ready(&self) -> Result<(), BridgeError> {
            Ok(())
        }
    }

    #[test]
    fn embedded_builder_registers_custom_providers() {
        let application = BridgeApplication::builder("fake", RuntimeConfig::default())
            .provider(Arc::new(FakeProvider))
            .build()
            .unwrap();
        assert_eq!(application.runtime().registry().default_name(), "fake");
        assert_eq!(application.runtime().registry().descriptors().len(), 1);
    }

    #[tokio::test]
    async fn configured_unavailable_provider_fails_explicitly() {
        let mut config = BridgeConfig::default();
        config.providers.codex_app_server.enabled = false;
        config.providers.openai.enabled = true;
        config.default_provider = "openai".to_owned();
        let error = BridgeApplication::from_config(config).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Configuration);
    }

    #[tokio::test]
    #[cfg(all(feature = "codex-app-server", feature = "codex-responses"))]
    #[ignore = "spawns the installed Codex app-server and checks local OAuth readiness"]
    async fn live_config_bootstrap_reports_both_codex_providers_ready() {
        if std::env::var("IMAGEGEN_BRIDGE_LIVE_BOOTSTRAP").as_deref() != Ok("1") {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut config = BridgeConfig::default();
        config.inputs.local_roots = vec![directory.path().to_owned()];
        config.artifacts.root = directory.path().join("artifacts");
        config.providers.codex_app_server.session_database = directory.path().join("state.sqlite3");
        config.providers.codex_app_server.cwd = Some(directory.path().to_owned());
        config.providers.codex_responses.enabled = true;

        let application = BridgeApplication::from_config(config).await.unwrap();
        let readiness = application.runtime().registry().readiness().await;
        assert_eq!(readiness.len(), 2);
        assert!(readiness.iter().all(|item| matches!(
            item.status,
            imagegen_bridge_runtime::ProviderReadinessStatus::Ready
        )));
        let capabilities = application
            .runtime()
            .registry()
            .capabilities(Some("codex-app-server"), None)
            .await
            .unwrap();
        assert_eq!(capabilities.count.max, 4);
        assert_eq!(
            capabilities.batching.mode,
            imagegen_bridge_core::BatchMode::FanOut
        );
        application.shutdown().await.unwrap();
    }
}
