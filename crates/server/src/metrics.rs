//! Fixed-cardinality Prometheus metrics without request content or caller identifiers.

use std::{collections::BTreeMap, fmt::Write as _, sync::Mutex, time::Duration};

use imagegen_bridge_core::{BridgeError, ErrorCode, ImageResponse};
use imagegen_bridge_runtime::RuntimeQueueSnapshot;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MetricKey {
    provider: String,
    result: &'static str,
    code: &'static str,
}

#[derive(Debug, Clone, Copy, Default)]
struct Observation {
    requests: u64,
    operation_millis: u128,
    provider_millis: u128,
    provider_samples: u64,
    queue_millis: u128,
    queue_samples: u64,
    generated_bytes: u128,
    normalizations: u64,
}

/// In-memory bounded metric aggregation keyed only by registered provider and stable code.
#[derive(Debug, Default)]
pub(crate) struct ServerMetrics {
    observations: Mutex<BTreeMap<MetricKey, Observation>>,
}

impl ServerMetrics {
    pub(crate) fn record(
        &self,
        provider: &str,
        result: &Result<ImageResponse, BridgeError>,
        elapsed: Duration,
    ) {
        let (status, code, success) = match result {
            Ok(response) => ("success", "none", Some(response)),
            Err(error) => ("error", error_code_name(error.code), None),
        };
        let key = MetricKey {
            provider: provider.to_owned(),
            result: status,
            code,
        };
        let mut observations = self
            .observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let observation = observations.entry(key).or_default();
        observation.requests = observation.requests.saturating_add(1);
        observation.operation_millis = observation
            .operation_millis
            .saturating_add(elapsed.as_millis());
        if let Some(response) = success {
            observation.provider_millis = observation
                .provider_millis
                .saturating_add(u128::from(response.timings.provider_ms));
            observation.provider_samples = observation.provider_samples.saturating_add(1);
            observation.queue_millis = observation
                .queue_millis
                .saturating_add(u128::from(response.timings.queue_ms));
            observation.queue_samples = observation.queue_samples.saturating_add(1);
            observation.generated_bytes = observation.generated_bytes.saturating_add(
                response
                    .data
                    .iter()
                    .map(|image| u128::from(image.bytes))
                    .sum::<u128>(),
            );
            observation.normalizations = observation
                .normalizations
                .saturating_add(u64::try_from(response.normalizations.len()).unwrap_or(u64::MAX));
        }
    }

    pub(crate) fn render(
        &self,
        queues: &RuntimeQueueSnapshot,
        provider_restarts: &BTreeMap<String, u64>,
    ) -> String {
        let observations = self
            .observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut output = String::from(
            "# HELP imagegen_bridge_requests_total Completed image operations by registered provider and stable result.\n\
             # TYPE imagegen_bridge_requests_total counter\n\
             # HELP imagegen_bridge_operation_duration_seconds End-to-end image operation duration.\n\
             # TYPE imagegen_bridge_operation_duration_seconds summary\n\
             # HELP imagegen_bridge_provider_duration_seconds Provider-reported successful execution duration.\n\
             # TYPE imagegen_bridge_provider_duration_seconds summary\n\
             # HELP imagegen_bridge_queue_duration_seconds Successful request admission queue duration.\n\
             # TYPE imagegen_bridge_queue_duration_seconds summary\n\
             # HELP imagegen_bridge_generated_bytes_total Verified generated image bytes.\n\
             # TYPE imagegen_bridge_generated_bytes_total counter\n\
             # HELP imagegen_bridge_normalizations_total Explicit parameter normalizations.\n\
             # TYPE imagegen_bridge_normalizations_total counter\n",
        );
        for (key, value) in observations.iter() {
            let labels = format!(
                "provider=\"{}\",result=\"{}\",code=\"{}\"",
                key.provider, key.result, key.code
            );
            let _ = writeln!(
                output,
                "imagegen_bridge_requests_total{{{labels}}} {}",
                value.requests
            );
            let _ = writeln!(
                output,
                "imagegen_bridge_operation_duration_seconds_sum{{{labels}}} {}",
                seconds(value.operation_millis)
            );
            let _ = writeln!(
                output,
                "imagegen_bridge_operation_duration_seconds_count{{{labels}}} {}",
                value.requests
            );
            if value.provider_samples > 0 {
                let _ = writeln!(
                    output,
                    "imagegen_bridge_provider_duration_seconds_sum{{{labels}}} {}",
                    seconds(value.provider_millis)
                );
                let _ = writeln!(
                    output,
                    "imagegen_bridge_provider_duration_seconds_count{{{labels}}} {}",
                    value.provider_samples
                );
                let _ = writeln!(
                    output,
                    "imagegen_bridge_queue_duration_seconds_sum{{{labels}}} {}",
                    seconds(value.queue_millis)
                );
                let _ = writeln!(
                    output,
                    "imagegen_bridge_queue_duration_seconds_count{{{labels}}} {}",
                    value.queue_samples
                );
                let _ = writeln!(
                    output,
                    "imagegen_bridge_generated_bytes_total{{{labels}}} {}",
                    value.generated_bytes
                );
                let _ = writeln!(
                    output,
                    "imagegen_bridge_normalizations_total{{{labels}}} {}",
                    value.normalizations
                );
            }
        }
        output.push_str(
            "# HELP imagegen_bridge_queue_depth Current bounded admission queue depth.\n\
             # TYPE imagegen_bridge_queue_depth gauge\n",
        );
        let _ = writeln!(
            output,
            "imagegen_bridge_queue_depth{{scope=\"global\",provider=\"none\"}} {}",
            queues.global_queued
        );
        for (provider, queued) in &queues.providers_queued {
            let _ = writeln!(
                output,
                "imagegen_bridge_queue_depth{{scope=\"provider\",provider=\"{provider}\"}} {queued}"
            );
        }
        output.push_str(
            "# HELP imagegen_bridge_provider_restarts_total Supervised provider child process restarts.\n\
             # TYPE imagegen_bridge_provider_restarts_total counter\n",
        );
        for (provider, restarts) in provider_restarts {
            let _ = writeln!(
                output,
                "imagegen_bridge_provider_restarts_total{{provider=\"{provider}\"}} {restarts}"
            );
        }
        output
    }
}

fn seconds(milliseconds: u128) -> String {
    format!("{}.{:03}", milliseconds / 1_000, milliseconds % 1_000)
}

const fn error_code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidRequest => "invalid_request",
        ErrorCode::UnsupportedCapability => "unsupported_capability",
        ErrorCode::Configuration => "configuration",
        ErrorCode::Authentication => "authentication",
        ErrorCode::PermissionDenied => "permission_denied",
        ErrorCode::SafetyRejected => "safety_rejected",
        ErrorCode::RateLimited => "rate_limited",
        ErrorCode::Overloaded => "overloaded",
        ErrorCode::Timeout => "timeout",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::Upstream => "upstream",
        ErrorCode::Protocol => "protocol",
        ErrorCode::Input => "input",
        ErrorCode::Artifact => "artifact",
        ErrorCode::Session => "session",
        ErrorCode::IdempotencyConflict => "idempotency_conflict",
        ErrorCode::Internal => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_only_bounded_safe_labels() {
        let metrics = ServerMetrics::default();
        metrics.record(
            "codex-app-server",
            &Err(BridgeError::new(ErrorCode::RateLimited, "secret prompt")),
            Duration::from_millis(1_234),
        );
        let rendered = metrics.render(
            &RuntimeQueueSnapshot {
                global_queued: 2,
                providers_queued: [("codex-app-server".to_owned(), 1)].into_iter().collect(),
            },
            &[("codex-app-server".to_owned(), 2)].into_iter().collect(),
        );
        assert!(rendered.contains("provider=\"codex-app-server\""));
        assert!(rendered.contains("code=\"rate_limited\""));
        assert!(rendered.contains("1.234"));
        assert!(rendered.contains("scope=\"global\""));
        assert!(rendered.contains("provider_restarts_total{provider=\"codex-app-server\"} 2"));
        assert!(!rendered.contains("secret prompt"));
    }
}
