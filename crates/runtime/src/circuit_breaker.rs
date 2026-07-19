//! Per-provider circuit breakers with bounded, deterministic state.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use imagegen_bridge_core::{BridgeError, ErrorCode};

/// Circuit-breaker policy applied independently to every provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitBreakerConfig {
    /// Whether calls are guarded by the breaker.
    pub enabled: bool,
    /// Consecutive qualifying failures that open a closed circuit.
    pub failure_threshold: u32,
    /// Recovery delay before a bounded half-open probe is permitted.
    pub open_duration: Duration,
    /// Simultaneous probes permitted while half-open.
    pub half_open_max_calls: u32,
    /// Consecutive successful probes required to close the circuit.
    pub success_threshold: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: 5,
            open_duration: Duration::from_secs(3 * 60),
            half_open_max_calls: 1,
            success_threshold: 1,
        }
    }
}

/// Stable circuit state exposed to diagnostics and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation.
    Closed,
    /// Calls fail fast during the recovery delay.
    Open,
    /// A bounded number of recovery probes are in flight.
    HalfOpen,
}

impl CircuitState {
    /// Stable lowercase metric/JSON label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

/// Redaction-safe breaker state for one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitBreakerSnapshot {
    /// Current state.
    pub state: CircuitState,
    /// Consecutive failures observed in the current closed epoch.
    pub consecutive_failures: u32,
    /// Remaining recovery delay when open.
    pub retry_after_ms: u64,
    /// Calls rejected without touching the provider.
    pub rejected_calls: u64,
    /// State transitions since startup.
    pub transitions: u64,
}

#[derive(Debug)]
pub(crate) struct CircuitBreaker {
    config: CircuitBreakerConfig,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    phase: Phase,
    epoch: u64,
    rejected_calls: u64,
    transitions: u64,
}

#[derive(Debug)]
enum Phase {
    Closed { consecutive_failures: u32 },
    Open { until: Instant },
    HalfOpen { in_flight: u32, successes: u32 },
}

#[derive(Debug, Clone, Copy)]
enum PermitKind {
    Closed(u64),
    HalfOpen(u64),
    Disabled,
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    Healthy,
    Failed,
    Neutral,
}

/// One admitted provider attempt. It must be completed or explicitly abandoned.
#[derive(Debug)]
pub(crate) struct CircuitPermit<'a> {
    breaker: &'a CircuitBreaker,
    kind: PermitKind,
    finished: bool,
}

impl CircuitBreaker {
    pub(crate) fn new(config: CircuitBreakerConfig) -> Result<Self, BridgeError> {
        if config.enabled
            && (config.failure_threshold == 0
                || config.open_duration.is_zero()
                || config.half_open_max_calls == 0
                || config.success_threshold == 0)
        {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "enabled circuit-breaker limits must be greater than zero",
            ));
        }
        Ok(Self {
            config,
            inner: Mutex::new(Inner {
                phase: Phase::Closed {
                    consecutive_failures: 0,
                },
                epoch: 0,
                rejected_calls: 0,
                transitions: 0,
            }),
        })
    }

    pub(crate) fn acquire(&self, provider: &str) -> Result<CircuitPermit<'_>, BridgeError> {
        if !self.config.enabled {
            return Ok(CircuitPermit {
                breaker: self,
                kind: PermitKind::Disabled,
                finished: false,
            });
        }
        let now = Instant::now();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Phase::Open { until } = inner.phase
            && now >= until
        {
            inner.phase = Phase::HalfOpen {
                in_flight: 0,
                successes: 0,
            };
            inner.epoch = inner.epoch.saturating_add(1);
            inner.transitions = inner.transitions.saturating_add(1);
        }
        let epoch = inner.epoch;
        let kind = match &mut inner.phase {
            Phase::Closed { .. } => PermitKind::Closed(epoch),
            Phase::HalfOpen { in_flight, .. } if *in_flight < self.config.half_open_max_calls => {
                *in_flight = in_flight.saturating_add(1);
                PermitKind::HalfOpen(epoch)
            }
            Phase::Open { until } => {
                let retry_after_ms = duration_ms(until.saturating_duration_since(now));
                inner.rejected_calls = inner.rejected_calls.saturating_add(1);
                return Err(open_error(provider, retry_after_ms));
            }
            Phase::HalfOpen { .. } => {
                inner.rejected_calls = inner.rejected_calls.saturating_add(1);
                return Err(open_error(provider, duration_ms(self.config.open_duration))
                    .with_detail("circuit_state", "half_open"));
            }
        };
        drop(inner);
        Ok(CircuitPermit {
            breaker: self,
            kind,
            finished: false,
        })
    }

    pub(crate) fn snapshot(&self) -> CircuitBreakerSnapshot {
        let now = Instant::now();
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, consecutive_failures, retry_after_ms) = match inner.phase {
            Phase::Closed {
                consecutive_failures,
            } => (CircuitState::Closed, consecutive_failures, 0),
            Phase::Open { until } => (
                CircuitState::Open,
                self.config.failure_threshold,
                duration_ms(until.saturating_duration_since(now)),
            ),
            Phase::HalfOpen { .. } => (CircuitState::HalfOpen, self.config.failure_threshold, 0),
        };
        CircuitBreakerSnapshot {
            state,
            consecutive_failures,
            retry_after_ms,
            rejected_calls: inner.rejected_calls,
            transitions: inner.transitions,
        }
    }

    fn complete(&self, kind: PermitKind, result: Result<(), &BridgeError>) {
        if matches!(kind, PermitKind::Disabled) {
            return;
        }
        let outcome = match result {
            Ok(()) => Outcome::Healthy,
            Err(error) if counts_as_failure(error) => Outcome::Failed,
            Err(_) => Outcome::Neutral,
        };
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let permit_epoch = match kind {
            PermitKind::Closed(epoch) | PermitKind::HalfOpen(epoch) => epoch,
            PermitKind::Disabled => return,
        };
        if permit_epoch != inner.epoch {
            return;
        }
        match (&mut inner.phase, kind, outcome) {
            (
                Phase::Closed {
                    consecutive_failures,
                },
                PermitKind::Closed(_),
                Outcome::Healthy,
            ) => {
                *consecutive_failures = 0;
            }
            (
                Phase::Closed {
                    consecutive_failures,
                },
                PermitKind::Closed(_),
                Outcome::Failed,
            ) => {
                *consecutive_failures = consecutive_failures.saturating_add(1);
                if *consecutive_failures >= self.config.failure_threshold {
                    inner.phase = Phase::Open {
                        until: Instant::now() + self.config.open_duration,
                    };
                    inner.epoch = inner.epoch.saturating_add(1);
                    inner.transitions = inner.transitions.saturating_add(1);
                }
            }
            (
                Phase::HalfOpen {
                    in_flight,
                    successes,
                },
                PermitKind::HalfOpen(_),
                Outcome::Healthy,
            ) => {
                *in_flight = in_flight.saturating_sub(1);
                *successes = successes.saturating_add(1);
                if *successes >= self.config.success_threshold && *in_flight == 0 {
                    inner.phase = Phase::Closed {
                        consecutive_failures: 0,
                    };
                    inner.epoch = inner.epoch.saturating_add(1);
                    inner.transitions = inner.transitions.saturating_add(1);
                }
            }
            (
                Phase::HalfOpen { .. },
                PermitKind::HalfOpen(_),
                Outcome::Failed | Outcome::Neutral,
            ) => {
                inner.phase = Phase::Open {
                    until: Instant::now() + self.config.open_duration,
                };
                inner.epoch = inner.epoch.saturating_add(1);
                inner.transitions = inner.transitions.saturating_add(1);
            }
            _ => {}
        }
    }

    fn abandon(&self, kind: PermitKind) {
        let PermitKind::HalfOpen(epoch) = kind else {
            return;
        };
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if epoch == inner.epoch
            && let Phase::HalfOpen { in_flight, .. } = &mut inner.phase
        {
            *in_flight = in_flight.saturating_sub(1);
        }
    }
}

impl CircuitPermit<'_> {
    pub(crate) fn finish<T>(mut self, result: &Result<T, BridgeError>) {
        self.breaker
            .complete(self.kind, result.as_ref().map(|_| ()));
        self.finished = true;
    }
}

impl Drop for CircuitPermit<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.breaker.abandon(self.kind);
        }
    }
}

fn counts_as_failure(error: &BridgeError) -> bool {
    matches!(
        error.code,
        ErrorCode::RateLimited
            | ErrorCode::Overloaded
            | ErrorCode::Timeout
            | ErrorCode::Upstream
            | ErrorCode::Protocol
    )
}

fn open_error(provider: &str, retry_after_ms: u64) -> BridgeError {
    BridgeError::new(ErrorCode::Overloaded, "provider circuit breaker is open")
        .retryable(true)
        .with_provider(provider)
        .with_detail("circuit_state", "open")
        .with_detail("retry_after_ms", retry_after_ms)
}

fn duration_ms(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 2,
            open_duration: Duration::from_millis(20),
            ..CircuitBreakerConfig::default()
        }
    }

    #[test]
    fn opens_after_threshold_and_recovers_through_half_open() {
        let breaker = CircuitBreaker::new(config()).unwrap();
        for _ in 0..2 {
            let permit = breaker.acquire("fake").unwrap();
            let result: Result<(), BridgeError> =
                Err(BridgeError::new(ErrorCode::Upstream, "injected"));
            permit.finish(&result);
        }
        assert_eq!(breaker.snapshot().state, CircuitState::Open);
        let rejected = breaker.acquire("fake").unwrap_err();
        assert_eq!(rejected.details["circuit_state"], "open");
        std::thread::sleep(Duration::from_millis(25));
        let permit = breaker.acquire("fake").unwrap();
        assert_eq!(breaker.snapshot().state, CircuitState::HalfOpen);
        permit.finish(&Ok::<_, BridgeError>(()));
        assert_eq!(breaker.snapshot().state, CircuitState::Closed);
    }

    #[test]
    fn slow_success_and_request_errors_do_not_open_the_circuit() {
        let breaker = CircuitBreaker::new(config()).unwrap();
        for code in [
            ErrorCode::SafetyRejected,
            ErrorCode::InvalidRequest,
            ErrorCode::Cancelled,
        ] {
            let permit = breaker.acquire("fake").unwrap();
            permit.finish(&Err::<(), _>(BridgeError::new(
                code,
                "non-provider failure",
            )));
        }
        let permit = breaker.acquire("fake").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        permit.finish(&Ok::<_, BridgeError>(()));
        assert_eq!(breaker.snapshot().state, CircuitState::Closed);
    }

    #[test]
    fn stale_closed_permit_cannot_contaminate_a_recovered_epoch() {
        let policy = CircuitBreakerConfig {
            failure_threshold: 1,
            open_duration: Duration::from_millis(5),
            ..CircuitBreakerConfig::default()
        };
        let breaker = CircuitBreaker::new(policy).unwrap();
        let stale = breaker.acquire("fake").unwrap();
        let opener = breaker.acquire("fake").unwrap();
        opener.finish(&Err::<(), _>(BridgeError::new(ErrorCode::Upstream, "open")));
        std::thread::sleep(Duration::from_millis(8));
        breaker
            .acquire("fake")
            .unwrap()
            .finish(&Ok::<_, BridgeError>(()));
        stale.finish(&Err::<(), _>(BridgeError::new(
            ErrorCode::Upstream,
            "stale",
        )));
        assert_eq!(breaker.snapshot().state, CircuitState::Closed);
        assert_eq!(breaker.snapshot().consecutive_failures, 0);
    }

    #[test]
    fn concurrent_half_open_probes_settle_before_closing() {
        let policy = CircuitBreakerConfig {
            failure_threshold: 1,
            open_duration: Duration::from_millis(5),
            half_open_max_calls: 2,
            success_threshold: 1,
            ..CircuitBreakerConfig::default()
        };
        let breaker = CircuitBreaker::new(policy).unwrap();
        breaker
            .acquire("fake")
            .unwrap()
            .finish(&Err::<(), _>(BridgeError::new(ErrorCode::Upstream, "open")));
        std::thread::sleep(Duration::from_millis(8));
        let healthy = breaker.acquire("fake").unwrap();
        let failed = breaker.acquire("fake").unwrap();
        healthy.finish(&Ok::<_, BridgeError>(()));
        assert_eq!(breaker.snapshot().state, CircuitState::HalfOpen);
        failed.finish(&Err::<(), _>(BridgeError::new(
            ErrorCode::Upstream,
            "probe failed",
        )));
        assert_eq!(breaker.snapshot().state, CircuitState::Open);
    }
}
