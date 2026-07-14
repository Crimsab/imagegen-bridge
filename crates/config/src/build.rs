//! Typed conversion into runtime components, separating checks from mutations.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use imagegen_bridge_artifacts::{
    ArtifactStore, ImageLimits, InputLoader, RemoteImageFetcher, RemoteInputPolicy, RetentionPolicy,
};
use imagegen_bridge_core::{BridgeError, RequestLimits};
use imagegen_bridge_runtime::{
    ConcurrencyLimit, IdempotencyConfig, MaterializationConfig, RuntimeConfig,
};
use url::Url;

use crate::{BridgeConfig, ImageLimitSettings, RemoteImageSettings};

/// Validated local and optional remote input loaders.
pub struct InputComponents {
    /// Capability-based local/inline loader.
    pub loader: Arc<InputLoader>,
    /// SSRF-resistant remote loader when explicitly enabled.
    pub remote: Option<RemoteImageFetcher>,
}

impl std::fmt::Debug for InputComponents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputComponents")
            .field("loader", &"[CAPABILITY ROOTS]")
            .field("remote", &self.remote.is_some())
            .finish()
    }
}

impl BridgeConfig {
    /// Builds a non-mutating runtime configuration after full validation.
    ///
    /// Artifact storage remains absent until [`Self::artifact_store`] is called
    /// explicitly by a serving/generation command.
    pub fn runtime_config(&self) -> Result<RuntimeConfig, BridgeError> {
        self.validate()?;
        let image_limits = image_limits(self.artifacts.image);
        let public_artifact_base_url = self
            .artifacts
            .public_base_url
            .as_deref()
            .map(Url::parse)
            .transpose()
            .map_err(|_| configuration_error())?;
        let remote_output_fetcher = remote_fetcher(&self.artifacts.remote_output, image_limits);
        Ok(RuntimeConfig {
            request_limits: RequestLimits {
                max_prompt_bytes: self.runtime.request.max_prompt_bytes,
                max_negative_prompt_bytes: self.runtime.request.max_negative_prompt_bytes,
                max_outputs: self.runtime.request.max_outputs,
                max_inputs: self.runtime.request.max_inputs,
                max_inline_encoded_bytes: self.runtime.request.max_inline_encoded_bytes,
                max_edge: self.runtime.request.max_edge,
                max_timeout_ms: self.runtime.request.max_timeout_ms,
                max_identifier_bytes: self.runtime.request.max_identifier_bytes,
            },
            default_timeout: Duration::from_millis(self.runtime.default_timeout_ms),
            cancellation_grace: Duration::from_millis(self.runtime.cancellation_grace_ms),
            shutdown_grace: Duration::from_millis(self.runtime.shutdown_grace_ms),
            global_limit: concurrency(self.runtime.global),
            default_provider_limit: concurrency(self.runtime.provider_default),
            provider_limits: self
                .runtime
                .providers
                .iter()
                .map(|(provider, limit)| (provider.clone(), concurrency(*limit)))
                .collect(),
            idempotency: IdempotencyConfig {
                max_entries: self.runtime.idempotency.max_entries,
                max_completed_bytes: self.runtime.idempotency.max_completed_bytes,
                completed_ttl: Duration::from_secs(self.runtime.idempotency.completed_ttl_secs),
                in_flight_ttl: Duration::from_secs(self.runtime.idempotency.in_flight_ttl_secs),
                unknown_ttl: Duration::from_secs(self.runtime.idempotency.unknown_ttl_secs),
            },
            materialization: MaterializationConfig {
                image_limits,
                max_base64_chars: self.artifacts.max_base64_chars,
                max_response_bytes: self.artifacts.max_response_bytes,
                artifact_store: None,
                remote_output_fetcher,
                public_artifact_base_url,
            },
        })
    }

    /// Explicitly creates/opens the configured bridge-owned artifact root.
    /// This is intentionally separate from [`Self::validate`] and
    /// [`Self::runtime_config`] so `config check` remains non-mutating.
    pub fn artifact_store(&self) -> Result<Arc<ArtifactStore>, BridgeError> {
        self.validate()?;
        ArtifactStore::new(
            self.artifacts.root.clone(),
            image_limits(self.artifacts.image),
        )
        .map(Arc::new)
    }

    /// Opens configured local roots and constructs optional remote input policy.
    pub fn input_components(&self) -> Result<InputComponents, BridgeError> {
        self.validate()?;
        let limits = image_limits(self.artifacts.image);
        let loader = InputLoader::new(self.inputs.local_roots.clone(), limits)?;
        Ok(InputComponents {
            loader: Arc::new(loader),
            remote: remote_fetcher(&self.inputs.remote, limits),
        })
    }

    /// Returns the ownership-safe cleanup policy without touching storage.
    pub fn retention_policy(&self) -> Result<RetentionPolicy, BridgeError> {
        self.validate()?;
        Ok(RetentionPolicy {
            max_age: Duration::from_secs(self.artifacts.retention.max_age_secs),
            max_artifacts: self.artifacts.retention.max_artifacts,
            max_scan_entries: self.artifacts.retention.max_scan_entries,
        })
    }
}

const fn concurrency(settings: crate::ConcurrencySettings) -> ConcurrencyLimit {
    ConcurrencyLimit {
        max_concurrent: settings.max_concurrent,
        max_queued: settings.max_queued,
    }
}

const fn image_limits(settings: ImageLimitSettings) -> ImageLimits {
    ImageLimits {
        max_encoded_bytes: settings.max_encoded_bytes,
        max_edge: settings.max_edge,
        max_pixels: settings.max_pixels,
        max_decode_alloc: settings.max_decode_alloc,
    }
}

fn remote_fetcher(
    settings: &RemoteImageSettings,
    limits: ImageLimits,
) -> Option<RemoteImageFetcher> {
    settings.enabled.then(|| {
        RemoteImageFetcher::new(
            RemoteInputPolicy {
                enabled: true,
                allowed_hosts: settings.allowed_hosts.iter().cloned().collect(),
                allowed_ports: settings
                    .allowed_ports
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>(),
                allow_private_networks: settings.allow_private_networks,
                max_redirects: settings.max_redirects,
                timeout: Duration::from_millis(settings.timeout_ms),
                max_url_bytes: settings.max_url_bytes,
            },
            limits,
        )
    })
}

fn configuration_error() -> BridgeError {
    BridgeError::new(
        imagegen_bridge_core::ErrorCode::Configuration,
        "could not build validated configuration",
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn runtime_build_and_check_do_not_create_artifact_storage() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("not-created-yet");
        let mut config = BridgeConfig::default();
        config.artifacts.root = root.clone();
        let runtime = config.runtime_config().unwrap();
        assert!(runtime.materialization.artifact_store.is_none());
        assert!(!root.exists());
        let store = config.artifact_store().unwrap();
        assert_eq!(store.root(), root.canonicalize().unwrap());
    }

    #[test]
    fn maps_all_runtime_limits_without_loss() {
        let mut config = BridgeConfig::default();
        config.runtime.global = crate::ConcurrencySettings {
            max_concurrent: 7,
            max_queued: 11,
        };
        config.runtime.request.max_prompt_bytes = 1234;
        config.runtime.idempotency.max_entries = 99;
        let runtime = config.runtime_config().unwrap();
        assert_eq!(runtime.global_limit.max_concurrent, 7);
        assert_eq!(runtime.global_limit.max_queued, 11);
        assert_eq!(runtime.request_limits.max_prompt_bytes, 1234);
        assert_eq!(runtime.idempotency.max_entries, 99);
    }
}
