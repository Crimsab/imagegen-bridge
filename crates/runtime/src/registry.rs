//! Immutable provider registration and discovery.

use std::{collections::BTreeMap, sync::Arc};

use imagegen_bridge_core::{
    BridgeError, ErrorCode, ImageProvider, ProviderCapabilities, ProviderDescriptor,
};
use serde::{Deserialize, Serialize};

const MAX_PROVIDER_NAME_BYTES: usize = 64;

/// Immutable set of providers used by one runtime instance.
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: Arc<BTreeMap<String, Arc<dyn ImageProvider>>>,
    default: Arc<str>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("default", &self.default)
            .finish()
    }
}

impl ProviderRegistry {
    /// Creates a validated registry and deterministic default provider.
    pub fn new(
        providers: impl IntoIterator<Item = Arc<dyn ImageProvider>>,
        default: impl Into<String>,
    ) -> Result<Self, BridgeError> {
        let mut registered = BTreeMap::new();
        for provider in providers {
            let descriptor = provider.descriptor();
            validate_provider_name(&descriptor.name)?;
            if registered
                .insert(descriptor.name.clone(), provider)
                .is_some()
            {
                return Err(registry_error(format!(
                    "provider '{}' is registered more than once",
                    descriptor.name
                )));
            }
        }
        if registered.is_empty() {
            return Err(registry_error("at least one provider must be registered"));
        }
        let default = default.into();
        validate_provider_name(&default)?;
        if !registered.contains_key(&default) {
            return Err(registry_error(format!(
                "default provider '{default}' is not registered"
            )));
        }
        Ok(Self {
            providers: Arc::new(registered),
            default: Arc::from(default),
        })
    }

    /// Returns the configured default provider name.
    #[must_use]
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// Resolves an explicit provider or the deterministic default.
    pub fn resolve(&self, requested: Option<&str>) -> Result<Arc<dyn ImageProvider>, BridgeError> {
        let name = requested.unwrap_or(&self.default);
        validate_provider_name(name)?;
        self.providers.get(name).cloned().ok_or_else(|| {
            BridgeError::new(
                ErrorCode::Configuration,
                "requested provider is not registered",
            )
            .with_detail("provider", name)
        })
    }

    /// Returns provider descriptors in stable name order.
    #[must_use]
    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.descriptor())
            .collect()
    }

    /// Returns dynamic capabilities for one selected provider/model.
    pub async fn capabilities(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderCapabilities, BridgeError> {
        self.resolve(provider)?.capabilities(model).await
    }

    /// Performs non-generating readiness checks in stable name order.
    pub async fn readiness(&self) -> Vec<ProviderReadiness> {
        let mut checks = Vec::with_capacity(self.providers.len());
        for (name, provider) in self.providers.iter() {
            let status = match provider.check_ready().await {
                Ok(()) => ProviderReadinessStatus::Ready,
                Err(error) => ProviderReadinessStatus::NotReady { error },
            };
            checks.push(ProviderReadiness {
                provider: name.clone(),
                status,
            });
        }
        checks
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (&str, Arc<dyn ImageProvider>)> + '_ {
        self.providers
            .iter()
            .map(|(name, provider)| (name.as_str(), Arc::clone(provider)))
    }
}

/// Redaction-safe readiness result for one provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderReadiness {
    /// Stable provider name.
    pub provider: String,
    /// Readiness result without credentials or prompt content.
    #[serde(flatten)]
    pub status: ProviderReadinessStatus,
}

/// Ready or safely classified non-ready state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderReadinessStatus {
    /// Provider authentication/configuration is ready.
    Ready,
    /// Provider cannot currently accept work.
    NotReady {
        /// Stable redaction-safe cause.
        error: BridgeError,
    },
}

fn validate_provider_name(name: &str) -> Result<(), BridgeError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_PROVIDER_NAME_BYTES
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(registry_error(
            "provider names must be lowercase ASCII identifiers",
        ))
    }
}

fn registry_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Configuration, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use async_trait::async_trait;
    use imagegen_bridge_core::{ImageRequest, ImageResponse, ProviderContext, ProviderEventStream};

    use super::*;

    struct NamedProvider(&'static str);

    #[async_trait]
    impl ImageProvider for NamedProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                name: self.0.to_owned(),
                display_name: self.0.to_owned(),
                version: "test".to_owned(),
                experimental: false,
                models: vec!["test-image".to_owned()],
            }
        }

        async fn capabilities(
            &self,
            _model: Option<&str>,
        ) -> Result<ProviderCapabilities, BridgeError> {
            unreachable!()
        }

        async fn execute(
            &self,
            _request: ImageRequest,
            _context: ProviderContext,
        ) -> Result<ImageResponse, BridgeError> {
            unreachable!()
        }

        async fn execute_stream(
            &self,
            _request: ImageRequest,
            _context: ProviderContext,
        ) -> Result<ProviderEventStream, BridgeError> {
            unreachable!()
        }

        async fn check_ready(&self) -> Result<(), BridgeError> {
            Ok(())
        }
    }

    fn provider(name: &'static str) -> Arc<dyn ImageProvider> {
        Arc::new(NamedProvider(name))
    }

    #[test]
    fn resolves_default_and_explicit_providers_deterministically() {
        let registry =
            ProviderRegistry::new([provider("second"), provider("first")], "first").unwrap();
        assert_eq!(registry.resolve(None).unwrap().descriptor().name, "first");
        assert_eq!(
            registry.resolve(Some("second")).unwrap().descriptor().name,
            "second"
        );
        assert_eq!(
            registry
                .descriptors()
                .into_iter()
                .map(|value| value.name)
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[test]
    fn rejects_collisions_invalid_names_and_missing_defaults() {
        assert!(ProviderRegistry::new([provider("same"), provider("same")], "same").is_err());
        assert!(ProviderRegistry::new([provider("Bad_Name")], "Bad_Name").is_err());
        assert!(ProviderRegistry::new([provider("only")], "missing").is_err());
    }
}
