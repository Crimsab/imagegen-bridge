//! Redaction-safe immutable configuration facts for operator diagnostics.

use std::net::SocketAddr;

use imagegen_bridge_config::{ConfigSource, ResolvedConfig, ServerSettings};
use serde::Serialize;

/// One effective configuration field origin without its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ConfigurationOrigin {
    pub(crate) field: String,
    pub(crate) source: &'static str,
    pub(crate) key: String,
}

/// Safe configuration summary retained by the HTTP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ConfigurationDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) default_provider: Option<String>,
    pub(crate) listener_scope: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) listener_port: Option<u16>,
    pub(crate) authentication_required: bool,
    pub(crate) metrics_enabled: bool,
    pub(crate) jobs_enabled: bool,
    pub(crate) max_connections: usize,
    pub(crate) max_body_bytes: u64,
    pub(crate) read_timeout_ms: u64,
    pub(crate) provenance: Vec<ConfigurationOrigin>,
}

impl ConfigurationDiagnostics {
    pub(crate) fn from_settings(settings: &ServerSettings) -> Self {
        let address = settings.bind.parse::<SocketAddr>().ok();
        Self {
            version: None,
            default_provider: None,
            listener_scope: address.map_or("unknown", |address| {
                if address.ip().is_loopback() {
                    "loopback"
                } else {
                    "remote"
                }
            }),
            listener_port: address.map(|address| address.port()),
            authentication_required: settings.bearer_token_env.is_some(),
            metrics_enabled: settings.metrics.enabled,
            jobs_enabled: settings.jobs.enabled,
            max_connections: settings.max_connections,
            max_body_bytes: settings.max_body_bytes,
            read_timeout_ms: settings.read_timeout_ms,
            provenance: Vec::new(),
        }
    }

    pub(crate) fn from_resolved(resolved: &ResolvedConfig) -> Self {
        let mut snapshot = Self::from_settings(&resolved.config.server);
        snapshot.version = Some(resolved.config.version);
        snapshot.default_provider = Some(resolved.config.default_provider.clone());
        snapshot.provenance = resolved
            .provenance()
            .iter()
            .map(|(field, origin)| ConfigurationOrigin {
                field: field.clone(),
                source: source_name(origin.source),
                key: origin.key.clone(),
            })
            .collect();
        snapshot
    }

    pub(crate) fn embedded(authentication_required: bool, metrics_enabled: bool) -> Self {
        Self {
            version: None,
            default_provider: None,
            listener_scope: "embedded",
            listener_port: None,
            authentication_required,
            metrics_enabled,
            jobs_enabled: false,
            max_connections: 0,
            max_body_bytes: 0,
            read_timeout_ms: 0,
            provenance: Vec::new(),
        }
    }
}

const fn source_name(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::Default => "default",
        ConfigSource::File => "file",
        ConfigSource::Environment => "environment",
        ConfigSource::Override => "override",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use imagegen_bridge_config::{ConfigLoader, ConfigOverride};

    use super::*;

    #[test]
    fn resolved_diagnostics_keep_origins_but_not_values_or_paths() {
        let resolved = ConfigLoader::default()
            .resolve(
                None,
                &[
                    ConfigOverride::set("artifacts.root", r#""/private/secret/output""#),
                    ConfigOverride::set("server.bearer_token_env", r#""SECRET_TOKEN_ENV""#),
                ],
            )
            .expect("resolved config");
        let snapshot = ConfigurationDiagnostics::from_resolved(&resolved);
        let json = serde_json::to_string(&snapshot).expect("diagnostics JSON");
        assert!(json.contains("artifacts.root"));
        assert!(json.contains("server.bearer_token_env"));
        assert!(!json.contains("/private/secret/output"));
        assert!(!json.contains("SECRET_TOKEN_ENV"));
    }
}
