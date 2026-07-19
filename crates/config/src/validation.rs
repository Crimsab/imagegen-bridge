//! Complete non-mutating configuration checks.

use std::{collections::BTreeSet, net::SocketAddr};

use imagegen_bridge_core::{BridgeError, ErrorCode};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{BridgeConfig, CONFIG_VERSION, ConcurrencySettings, RemoteImageSettings};

/// One deterministic redaction-safe configuration issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigIssue {
    /// Dotted field path.
    pub field: String,
    /// Stable issue code.
    pub code: String,
    /// Safe explanation without the rejected value.
    pub message: String,
}

impl BridgeConfig {
    /// Checks the full effective configuration without creating files or directories.
    #[must_use]
    pub fn check(&self) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        let mut issue = |field: &str, code: &str, message: &str| {
            issues.push(ConfigIssue {
                field: field.to_owned(),
                code: code.to_owned(),
                message: message.to_owned(),
            });
        };

        if self.version != CONFIG_VERSION {
            issue(
                "version",
                "unsupported",
                "configuration version is unsupported",
            );
        }
        if !valid_provider_name(&self.default_provider) {
            issue(
                "default_provider",
                "invalid",
                "default provider name is not a lowercase ASCII identifier",
            );
        }
        validate_concurrency("runtime.global", self.runtime.global, &mut issue);
        validate_concurrency(
            "runtime.provider_default",
            self.runtime.provider_default,
            &mut issue,
        );
        for (provider, limit) in &self.runtime.providers {
            if !valid_provider_name(provider) {
                issue(
                    "runtime.providers",
                    "invalid_key",
                    "provider limit key is invalid",
                );
            }
            validate_concurrency("runtime.providers.*", *limit, &mut issue);
        }
        validate_circuit_breaker(
            "runtime.circuit_breaker",
            self.runtime.circuit_breaker,
            &mut issue,
        );
        for (provider, breaker) in &self.runtime.circuit_breakers {
            if !valid_provider_name(provider) {
                issue(
                    "runtime.circuit_breakers",
                    "invalid",
                    "provider circuit-breaker key is invalid",
                );
            }
            validate_circuit_breaker("runtime.circuit_breakers.*", *breaker, &mut issue);
        }
        if self.runtime.default_timeout_ms == 0
            || self.runtime.default_timeout_ms > self.runtime.request.max_timeout_ms
        {
            issue(
                "runtime.default_timeout_ms",
                "out_of_range",
                "default timeout must be positive and within the request maximum",
            );
        }
        if self.runtime.cancellation_grace_ms == 0 || self.runtime.cancellation_grace_ms > 30_000 {
            issue(
                "runtime.cancellation_grace_ms",
                "out_of_range",
                "cancellation grace must be between 1 and 30000 milliseconds",
            );
        }
        if self.runtime.shutdown_grace_ms == 0 || self.runtime.shutdown_grace_ms > 5 * 60 * 1_000 {
            issue(
                "runtime.shutdown_grace_ms",
                "out_of_range",
                "shutdown grace must be between 1 millisecond and five minutes",
            );
        }
        let request = self.runtime.request;
        for (field, invalid) in [
            ("max_prompt_bytes", request.max_prompt_bytes == 0),
            (
                "max_negative_prompt_bytes",
                request.max_negative_prompt_bytes == 0,
            ),
            ("max_outputs", request.max_outputs == 0),
            ("max_inputs", request.max_inputs == 0),
            (
                "max_inline_encoded_bytes",
                request.max_inline_encoded_bytes == 0,
            ),
            ("max_edge", request.max_edge == 0),
            ("max_timeout_ms", request.max_timeout_ms == 0),
            ("max_identifier_bytes", request.max_identifier_bytes == 0),
        ] {
            if invalid {
                issue(
                    &format!("runtime.request.{field}"),
                    "out_of_range",
                    "request limit must be greater than zero",
                );
            }
        }
        if self.runtime.idempotency.max_entries == 0
            || self.runtime.idempotency.max_completed_bytes == 0
            || self.runtime.idempotency.completed_ttl_secs == 0
            || self.runtime.idempotency.in_flight_ttl_secs == 0
            || self.runtime.idempotency.unknown_ttl_secs == 0
        {
            issue(
                "runtime.idempotency",
                "out_of_range",
                "idempotency capacity and lifetimes must be greater than zero",
            );
        }

        if self.inputs.local_roots.is_empty() {
            issue(
                "inputs.local_roots",
                "required",
                "at least one local input root is required",
            );
        }
        validate_remote("inputs.remote", &self.inputs.remote, &mut issue);
        validate_remote(
            "artifacts.remote_output",
            &self.artifacts.remote_output,
            &mut issue,
        );
        if self.artifacts.max_base64_chars == 0 || self.artifacts.max_response_bytes == 0 {
            issue(
                "artifacts",
                "out_of_range",
                "base64 and aggregate response byte limits must be greater than zero",
            );
        }
        let image = self.artifacts.image;
        if image.max_encoded_bytes == 0
            || image.max_edge == 0
            || image.max_pixels == 0
            || image.max_decode_alloc == 0
        {
            issue(
                "artifacts.image",
                "out_of_range",
                "all image limits must be greater than zero",
            );
        }
        if image.max_pixels > u64::from(image.max_edge) * u64::from(image.max_edge) {
            issue(
                "artifacts.image.max_pixels",
                "inconsistent",
                "maximum pixels cannot exceed the square of maximum edge",
            );
        }
        if self.artifacts.retention.max_scan_entries == 0
            || self.artifacts.retention.max_age_secs == 0
            || self.artifacts.retention.max_artifacts == Some(0)
        {
            issue(
                "artifacts.retention",
                "out_of_range",
                "retention age, scan bound, and optional artifact count must be positive",
            );
        }
        if let Some(base) = &self.artifacts.public_base_url
            && !valid_public_base_url(base)
        {
            issue(
                "artifacts.public_base_url",
                "invalid_url",
                "public artifact base must be credential-free HTTP(S) ending in a slash",
            );
        }

        let app = &self.providers.codex_app_server;
        if app.enabled {
            if app.executable.as_os_str().is_empty() {
                issue(
                    "providers.codex_app_server.executable",
                    "required",
                    "Codex executable must not be empty",
                );
            }
            if app.max_outputs == 0
                || app.max_outputs > self.runtime.request.max_outputs
                || app
                    .max_parallel_outputs
                    .limited()
                    .is_some_and(|value| value == 0 || value > usize::from(app.max_outputs))
            {
                issue(
                    "providers.codex_app_server.max_outputs",
                    "out_of_range",
                    "app-server output capacity must fit the request limit; parallelism accepts `auto` or a positive value no greater than max_outputs",
                );
            }
            if app.rpc_max_message_bytes == 0
                || app.rpc_max_message_bytes > 64 * 1024 * 1024
                || app.rpc_max_notification_bytes == 0
                || app.rpc_max_notification_bytes > app.rpc_max_message_bytes
                || app.rpc_max_notification_bytes > 48 * 1024 * 1024
                || app.rpc_timeout_ms == 0
                || app.notification_capacity == 0
                || app.notification_capacity > 64
                || app
                    .notification_capacity
                    .checked_next_power_of_two()
                    .and_then(|slots| app.rpc_max_notification_bytes.checked_mul(slots))
                    .is_none_or(|bytes| bytes > 256 * 1024 * 1024)
                || app.shutdown_timeout_ms == 0
                || app.restart_backoff_ms > 30_000
            {
                issue(
                    "providers.codex_app_server",
                    "out_of_range",
                    "app-server RPC/process limits are invalid",
                );
            }
        }
        let responses = &self.providers.codex_responses;
        if responses.enabled {
            if !valid_upstream_url(&responses.endpoint) {
                issue(
                    "providers.codex_responses.endpoint",
                    "invalid_url",
                    "Codex Responses endpoint must use HTTPS or loopback HTTP",
                );
            }
            if responses.responses_model.trim().is_empty()
                || responses.image_model.trim().is_empty()
            {
                issue(
                    "providers.codex_responses",
                    "required",
                    "Codex Responses model names must not be empty",
                );
            }
            if responses.max_outputs == 0
                || responses.max_outputs > self.runtime.request.max_outputs
                || responses
                    .max_parallel_outputs
                    .limited()
                    .is_some_and(|value| value == 0 || value > usize::from(responses.max_outputs))
            {
                issue(
                    "providers.codex_responses.max_parallel_outputs",
                    "out_of_range",
                    "Codex Responses output capacity must fit the request limit; parallelism accepts `auto` or a positive value no greater than max_outputs",
                );
            }
            if !(1..=2).contains(&responses.max_transient_attempts) {
                issue(
                    "providers.codex_responses.max_transient_attempts",
                    "out_of_range",
                    "transient attempt limit must be between 1 and 2",
                );
            }
            if responses.transient_retry_backoff_ms > 30_000 {
                issue(
                    "providers.codex_responses.transient_retry_backoff_ms",
                    "out_of_range",
                    "transient retry backoff must not exceed 30000 milliseconds",
                );
            }
        }
        let openai = &self.providers.openai;
        if openai.enabled {
            if !valid_upstream_url(&openai.base_url) {
                issue(
                    "providers.openai.base_url",
                    "invalid_url",
                    "OpenAI base URL must use HTTPS or loopback HTTP",
                );
            }
            validate_env_name(
                "providers.openai.api_key_env",
                &openai.api_key_env,
                &mut issue,
            );
            if let Some(name) = &openai.organization_env {
                validate_env_name("providers.openai.organization_env", name, &mut issue);
            }
            if let Some(name) = &openai.project_env {
                validate_env_name("providers.openai.project_env", name, &mut issue);
            }
        }
        if let Some(name) = &self.server.bearer_token_env {
            validate_env_name("server.bearer_token_env", name, &mut issue);
        }
        match self.server.bind.parse::<SocketAddr>() {
            Ok(address) => {
                if !address.ip().is_loopback() && self.server.bearer_token_env.is_none() {
                    issue(
                        "server.bearer_token_env",
                        "required_for_remote_bind",
                        "a non-loopback server bind requires bridge bearer authentication",
                    );
                }
            }
            Err(_) => issue(
                "server.bind",
                "invalid",
                "server bind must be a numeric socket address",
            ),
        }
        if self.server.max_body_bytes == 0
            || self.server.max_header_bytes == 0
            || self.server.max_connections.limited() == Some(0)
            || self.server.write_timeout_ms == 0
        {
            issue(
                "server",
                "out_of_range",
                "server request and connection limits, except the optional read-stall timeout, must be greater than zero",
            );
        }
        if self
            .server
            .activation_lock
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            issue(
                "server.activation_lock",
                "invalid",
                "activation lock path must not be empty",
            );
        }
        if self.server.jobs.enabled
            && (self.server.jobs.database.as_os_str().is_empty()
                || self.server.jobs.max_pending == 0
                || self.server.jobs.max_running == 0
                || self.server.jobs.retention_secs == 0
                || self.server.jobs.max_retained == 0
                || self.server.jobs.max_retained_bytes == 0
                || self.server.jobs.max_database_bytes == 0
                || self.server.jobs.max_retained_bytes > self.server.jobs.max_database_bytes)
        {
            issue(
                "server.jobs",
                "out_of_range",
                "enabled job storage paths and limits must be non-empty and greater than zero",
            );
        }

        let enabled: BTreeSet<_> = [
            app.enabled.then_some("codex-app-server"),
            responses.enabled.then_some("codex-responses"),
            openai.enabled.then_some("openai"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if enabled.is_empty() {
            issue(
                "providers",
                "required",
                "at least one provider must be enabled",
            );
        }
        if !enabled.contains(self.default_provider.as_str()) {
            issue(
                "default_provider",
                "disabled",
                "default provider is not enabled",
            );
        }

        issues.sort_by(|left, right| {
            left.field
                .cmp(&right.field)
                .then_with(|| left.code.cmp(&right.code))
        });
        issues
    }

    /// Converts any check failures to one stable configuration error.
    pub fn validate(&self) -> Result<(), BridgeError> {
        let issues = self.check();
        if issues.is_empty() {
            Ok(())
        } else {
            Err(
                BridgeError::new(ErrorCode::Configuration, "configuration validation failed")
                    .with_detail("issues", issues),
            )
        }
    }
}

fn validate_concurrency(
    field: &str,
    settings: ConcurrencySettings,
    issue: &mut impl FnMut(&str, &str, &str),
) {
    if settings.max_concurrent.limited() == Some(0) {
        issue(
            field,
            "out_of_range",
            "maximum concurrency must be greater than zero",
        );
    }
}

fn validate_circuit_breaker(
    field: &'static str,
    settings: crate::CircuitBreakerSettings,
    issue: &mut impl FnMut(&'static str, &'static str, &'static str),
) {
    if settings.enabled
        && (settings.failure_threshold == 0
            || settings.failure_threshold > 10_000
            || settings.open_duration_ms == 0
            || settings.open_duration_ms > 24 * 60 * 60 * 1_000
            || settings.half_open_max_calls == 0
            || settings.half_open_max_calls > 100
            || settings.success_threshold == 0
            || settings.success_threshold > 100)
    {
        issue(
            field,
            "out_of_range",
            "enabled circuit-breaker thresholds, probe limits, and cooldown must be within bounds",
        );
    }
}

fn validate_remote(
    field: &str,
    settings: &RemoteImageSettings,
    issue: &mut impl FnMut(&str, &str, &str),
) {
    if settings.max_redirects > 10
        || settings.timeout_ms == 0
        || settings.max_url_bytes == 0
        || (settings.enabled && settings.allowed_ports.is_empty())
        || settings.allowed_ports.contains(&0)
    {
        issue(
            field,
            "out_of_range",
            "remote image redirect, timeout, URL, or port limits are invalid",
        );
    }
    for host in &settings.allowed_hosts {
        if host.is_empty()
            || host != &host.to_ascii_lowercase()
            || host.chars().any(char::is_whitespace)
        {
            issue(
                field,
                "invalid_host",
                "allowed remote hosts must be lowercase names without whitespace",
            );
            break;
        }
    }
}

fn validate_env_name(field: &str, value: &str, issue: &mut impl FnMut(&str, &str, &str)) {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_uppercase()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
    if !valid {
        issue(
            field,
            "invalid",
            "credential environment reference is invalid",
        );
    }
}

fn valid_provider_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_public_base_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path().ends_with('/')
    })
}

fn valid_upstream_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
        (url.scheme() == "https" || (url.scheme() == "http" && loopback))
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn defaults_pass_the_complete_non_mutating_check() {
        assert!(BridgeConfig::default().check().is_empty());
    }

    #[test]
    fn reports_disabled_default_and_invalid_limits_together() {
        let mut config = BridgeConfig::default();
        config.providers.codex_responses.enabled = false;
        config.runtime.global.max_concurrent = crate::Capacity::Limited(0);
        config.server.bind = "not-a-socket".to_owned();
        let issues = config.check();
        assert!(issues.iter().any(|issue| issue.field == "default_provider"));
        assert!(issues.iter().any(|issue| issue.field == "runtime.global"));
        assert!(issues.iter().any(|issue| issue.field == "server.bind"));
        assert!(issues.windows(2).all(|pair| pair[0].field <= pair[1].field));
    }

    #[test]
    fn accepts_loopback_http_but_rejects_remote_plaintext_upstreams() {
        let mut config = BridgeConfig::default();
        config.providers.codex_responses.enabled = true;
        config.providers.codex_responses.endpoint = "http://127.0.0.1:8080/responses".to_owned();
        assert!(config.check().is_empty());
        config.providers.codex_responses.endpoint = "http://example.test/responses".to_owned();
        config.providers.codex_responses.max_parallel_outputs =
            crate::OutputParallelism::Limited(0);
        config.providers.codex_responses.max_transient_attempts = 0;
        config.providers.codex_responses.transient_retry_backoff_ms = 30_001;
        assert!(
            config
                .check()
                .iter()
                .any(|issue| issue.field == "providers.codex_responses.endpoint")
        );
        assert!(
            config
                .check()
                .iter()
                .any(|issue| { issue.field == "providers.codex_responses.max_parallel_outputs" })
        );
        assert!(
            config
                .check()
                .iter()
                .any(|issue| { issue.field == "providers.codex_responses.max_transient_attempts" })
        );
        assert!(config.check().iter().any(|issue| {
            issue.field == "providers.codex_responses.transient_retry_backoff_ms"
        }));
    }

    #[test]
    fn validates_app_server_fanout_against_request_limits() {
        let mut config = BridgeConfig::default();
        config.providers.codex_app_server.max_outputs = 5;
        config.runtime.request.max_outputs = 4;
        config.providers.codex_app_server.max_parallel_outputs =
            crate::OutputParallelism::Limited(5);
        assert!(
            config
                .check()
                .iter()
                .any(|issue| { issue.field == "providers.codex_app_server.max_outputs" })
        );

        config.runtime.request.max_outputs = u8::MAX;
        config.providers.codex_app_server.max_outputs = 4;
        config.providers.codex_app_server.max_parallel_outputs =
            crate::OutputParallelism::Limited(2);
        assert!(config.check().is_empty());
    }

    #[test]
    fn remote_server_binds_require_a_bearer_reference() {
        for bind in ["0.0.0.0:8787", "[::]:8787", "192.0.2.1:8787"] {
            let mut config = BridgeConfig::default();
            config.server.bind = bind.to_owned();
            assert!(config.check().iter().any(|issue| {
                issue.field == "server.bearer_token_env" && issue.code == "required_for_remote_bind"
            }));
            config.server.bearer_token_env = Some("IMAGEGEN_BRIDGE_BEARER_TOKEN".to_owned());
            assert!(config.check().is_empty(), "remote bind {bind} with auth");
        }

        for bind in ["127.0.0.1:8787", "[::1]:8787"] {
            let mut config = BridgeConfig::default();
            config.server.bind = bind.to_owned();
            assert!(config.check().is_empty(), "loopback bind {bind}");
        }
    }

    #[test]
    fn checked_in_example_matches_the_typed_contract() {
        let config: BridgeConfig =
            toml::from_str(include_str!("../../../config.example.toml")).unwrap();
        assert!(config.check().is_empty());
    }
}
