//! Bounded operator event history containing only fixed-cardinality HTTP facts.

use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::http::{Method, StatusCode};
use serde::Serialize;

const EVENT_CAPACITY: usize = 256;

/// One redacted HTTP event safe for authenticated operator diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OperatorEvent {
    pub(crate) sequence: u64,
    pub(crate) timestamp_ms: u64,
    pub(crate) method: &'static str,
    pub(crate) route: &'static str,
    pub(crate) status: u16,
    pub(crate) duration_ms: u64,
}

/// Complete bounded event snapshot, newest event first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OperatorEventHistory {
    pub(crate) capacity: usize,
    pub(crate) dropped: u64,
    pub(crate) items: Vec<OperatorEvent>,
}

#[derive(Debug, Default)]
struct EventState {
    next_sequence: u64,
    dropped: u64,
    items: VecDeque<OperatorEvent>,
}

/// Thread-safe bounded redacted event recorder.
#[derive(Debug, Default)]
pub(crate) struct OperatorEvents {
    state: Mutex<EventState>,
}

impl OperatorEvents {
    pub(crate) fn record(
        &self,
        method: &Method,
        route: &'static str,
        status: StatusCode,
        duration: Duration,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_sequence = state.next_sequence.saturating_add(1);
        if state.items.len() == EVENT_CAPACITY {
            state.items.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        let sequence = state.next_sequence;
        state.items.push_back(OperatorEvent {
            sequence,
            timestamp_ms: timestamp_ms(),
            method: safe_method(method),
            route,
            status: status.as_u16(),
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        });
    }

    pub(crate) fn snapshot(&self) -> OperatorEventHistory {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        OperatorEventHistory {
            capacity: EVENT_CAPACITY,
            dropped: state.dropped,
            items: state.items.iter().rev().cloned().collect(),
        }
    }
}

pub(crate) fn redacted_route(path: &str) -> Option<&'static str> {
    let route = match path {
        "/v1/images" => "/v1/images",
        "/v1/images/stream" => "/v1/images/stream",
        "/v1/images/generations" => "/v1/images/generations",
        "/v1/images/edits" => "/v1/images/edits",
        "/v1/providers" => "/v1/providers",
        "/v1/diagnostics" => "/v1/diagnostics",
        "/v1/jobs" => "/v1/jobs",
        "/metrics" => "/metrics",
        "/v1/openapi.json" => return None,
        path if one_component_after(path, "/v1/jobs/") => "/v1/jobs/{id}",
        path if two_components_after(path, "/v1/jobs/", "partial") => "/v1/jobs/{id}/partial",
        path if one_component_after(path, "/v1/sessions/") => "/v1/sessions/{key}",
        path if one_component_after(path, "/v1/artifacts/") => "/v1/artifacts/{id}",
        path if artifact_thumbnail(path) => "/v1/artifacts/{id}/thumbnail",
        path if provider_capabilities(path) => "/v1/providers/{provider}/capabilities",
        path if path.starts_with("/v1/") => "/v1/unmatched",
        _ => return None,
    };
    Some(route)
}

fn two_components_after(path: &str, prefix: &str, tail: &str) -> bool {
    path.strip_prefix(prefix).is_some_and(|remainder| {
        remainder
            .split_once('/')
            .is_some_and(|(component, last)| !component.is_empty() && last == tail)
    })
}

fn one_component_after(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|suffix| !suffix.is_empty() && !suffix.contains('/'))
}

fn artifact_thumbnail(path: &str) -> bool {
    path.strip_prefix("/v1/artifacts/")
        .and_then(|suffix| suffix.strip_suffix("/thumbnail"))
        .is_some_and(|identifier| !identifier.is_empty() && !identifier.contains('/'))
}

fn provider_capabilities(path: &str) -> bool {
    path.strip_prefix("/v1/providers/")
        .and_then(|suffix| suffix.strip_suffix("/capabilities"))
        .is_some_and(|provider| !provider.is_empty() && !provider.contains('/'))
}

fn safe_method(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_templates_never_retain_dynamic_values() {
        assert_eq!(
            redacted_route("/v1/sessions/private-session-key"),
            Some("/v1/sessions/{key}")
        );
        assert_eq!(
            redacted_route("/v1/artifacts/private-id/thumbnail"),
            Some("/v1/artifacts/{id}/thumbnail")
        );
        assert_eq!(
            redacted_route("/v1/providers/private-provider/capabilities"),
            Some("/v1/providers/{provider}/capabilities")
        );
        assert_eq!(
            redacted_route("/v1/jobs/private-job-id/partial"),
            Some("/v1/jobs/{id}/partial")
        );
        assert_eq!(
            redacted_route("/v1/prompt-like-secret"),
            Some("/v1/unmatched")
        );
        assert_eq!(redacted_route("/dashboard/app.js"), None);
        assert_eq!(redacted_route("/v1/openapi.json"), None);
    }

    #[test]
    fn history_is_bounded_newest_first_and_counts_overwrites() {
        let events = OperatorEvents::default();
        for _ in 0..EVENT_CAPACITY + 2 {
            events.record(
                &Method::POST,
                "/v1/images",
                StatusCode::OK,
                Duration::from_millis(7),
            );
        }
        let snapshot = events.snapshot();
        assert_eq!(snapshot.capacity, EVENT_CAPACITY);
        assert_eq!(snapshot.items.len(), EVENT_CAPACITY);
        assert_eq!(snapshot.dropped, 2);
        assert_eq!(snapshot.items[0].sequence, 258);
        assert_eq!(snapshot.items.last().map(|event| event.sequence), Some(3));
        assert_eq!(snapshot.items[0].duration_ms, 7);
    }
}
