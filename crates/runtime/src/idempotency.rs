//! Scoped, bounded in-memory idempotency coordination.

use std::{
    collections::HashMap,
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use imagegen_bridge_core::{BridgeError, ErrorCode, ImageRequest, ImageResponse};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

const MAX_SCOPE_BYTES: usize = 256;

/// Bounds for replay records and abandoned in-flight operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdempotencyConfig {
    /// Maximum keys retained at once.
    pub max_entries: usize,
    /// Maximum serialized response bytes retained for replay.
    pub max_completed_bytes: usize,
    /// Completed response replay lifetime.
    pub completed_ttl: Duration,
    /// Safety lifetime for a leader that disappears without completing.
    pub in_flight_ttl: Duration,
    /// Retention for a provider operation whose outcome is unknown.
    pub unknown_ttl: Duration,
}

impl Default for IdempotencyConfig {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_completed_bytes: 256 * 1024 * 1024,
            completed_ttl: Duration::from_secs(24 * 60 * 60),
            in_flight_ttl: Duration::from_secs(31 * 60),
            unknown_ttl: Duration::from_secs(24 * 60 * 60),
        }
    }
}

pub(crate) struct IdempotencyCoordinator {
    inner: Mutex<IdempotencyState>,
    config: IdempotencyConfig,
}

impl IdempotencyCoordinator {
    pub(crate) fn new(config: IdempotencyConfig) -> Result<Self, BridgeError> {
        if config.max_entries == 0
            || config.max_completed_bytes == 0
            || config.completed_ttl.is_zero()
            || config.in_flight_ttl.is_zero()
            || config.unknown_ttl.is_zero()
        {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "idempotency limits and retention periods must be greater than zero",
            ));
        }
        Ok(Self {
            inner: Mutex::new(IdempotencyState::default()),
            config,
        })
    }

    pub(crate) async fn begin(
        &self,
        scope: &str,
        key: &str,
        request: &ImageRequest,
    ) -> Result<IdempotencyAction, BridgeError> {
        validate_scope(scope)?;
        let fingerprint = request_fingerprint(request)?;
        let record_key = RecordKey {
            scope: scope.to_owned(),
            key: key.to_owned(),
        };
        let now = Instant::now();
        let mut state = self.inner.lock().await;
        state.cleanup(now, self.config.in_flight_ttl);
        state.access_clock = state.access_clock.saturating_add(1);
        let access = state.access_clock;
        if let Some(entry) = state.entries.get_mut(&record_key) {
            if entry.fingerprint != fingerprint {
                return Err(BridgeError::new(
                    ErrorCode::IdempotencyConflict,
                    "idempotency key was already used for a different request",
                ));
            }
            entry.last_access = access;
            let receiver = entry.sender.subscribe();
            let outcome = receiver.borrow().clone();
            return match outcome {
                EntryOutcome::Completed(response) => {
                    Ok(IdempotencyAction::Cached(Box::new((*response).clone())))
                }
                EntryOutcome::Failed(error) | EntryOutcome::Unknown(error) => Err(error),
                EntryOutcome::Pending => Ok(IdempotencyAction::Wait(receiver)),
            };
        }
        state.make_capacity(self.config.max_entries)?;
        let (sender, _receiver) = watch::channel(EntryOutcome::Pending);
        state.entries.insert(
            record_key.clone(),
            Entry {
                fingerprint,
                created_at: now,
                expires_at: None,
                retained_bytes: 0,
                uncertain: false,
                last_access: access,
                sender,
            },
        );
        Ok(IdempotencyAction::Leader(IdempotencyToken {
            record_key,
            fingerprint,
        }))
    }

    pub(crate) async fn complete(&self, token: IdempotencyToken, response: ImageResponse) {
        let retained_bytes = response_weight(&response).unwrap_or(usize::MAX);
        let mut state = self.inner.lock().await;
        state.complete(&token, response, retained_bytes, &self.config);
    }

    pub(crate) async fn fail(&self, token: IdempotencyToken, error: BridgeError) {
        let mut state = self.inner.lock().await;
        if error
            .details
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            == Some("unknown")
        {
            if let Some(entry) = state.entries.get_mut(&token.record_key)
                && entry.fingerprint == token.fingerprint
            {
                entry.expires_at = Some(Instant::now() + self.config.unknown_ttl);
                entry.uncertain = true;
                entry.sender.send_replace(EntryOutcome::Unknown(
                    error.retryable(false).with_detail("outcome", "unknown"),
                ));
            }
        } else if state
            .entries
            .get(&token.record_key)
            .is_some_and(|entry| entry.fingerprint == token.fingerprint)
            && let Some(entry) = state.remove(&token.record_key)
        {
            entry.sender.send_replace(EntryOutcome::Failed(error));
        }
    }
}

pub(crate) enum IdempotencyAction {
    Leader(IdempotencyToken),
    Cached(Box<ImageResponse>),
    Wait(watch::Receiver<EntryOutcome>),
}

impl IdempotencyAction {
    pub(crate) async fn wait(
        mut receiver: watch::Receiver<EntryOutcome>,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<ImageResponse, BridgeError> {
        loop {
            let outcome = receiver.borrow().clone();
            match outcome {
                EntryOutcome::Completed(response) => return Ok((*response).clone()),
                EntryOutcome::Failed(error) | EntryOutcome::Unknown(error) => return Err(error),
                EntryOutcome::Pending => {}
            }
            if deadline <= Instant::now() {
                return Err(BridgeError::new(
                    ErrorCode::Timeout,
                    "request deadline elapsed while waiting for idempotent result",
                ));
            }
            tokio::select! {
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return Err(BridgeError::new(
                            ErrorCode::Internal,
                            "idempotency leader ended without a result",
                        ));
                    }
                }
                () = cancellation.cancelled() => {
                    return Err(BridgeError::new(
                        ErrorCode::Cancelled,
                        "request was cancelled while waiting for idempotent result",
                    ));
                }
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    return Err(BridgeError::new(
                        ErrorCode::Timeout,
                        "request deadline elapsed while waiting for idempotent result",
                    ));
                }
            }
        }
    }
}

#[derive(Clone)]
pub(crate) enum EntryOutcome {
    Pending,
    Completed(Arc<ImageResponse>),
    Failed(BridgeError),
    Unknown(BridgeError),
}

pub(crate) struct IdempotencyToken {
    record_key: RecordKey,
    fingerprint: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RecordKey {
    scope: String,
    key: String,
}

struct Entry {
    fingerprint: [u8; 32],
    created_at: Instant,
    expires_at: Option<Instant>,
    retained_bytes: usize,
    uncertain: bool,
    last_access: u64,
    sender: watch::Sender<EntryOutcome>,
}

#[derive(Default)]
struct IdempotencyState {
    entries: HashMap<RecordKey, Entry>,
    completed_bytes: usize,
    access_clock: u64,
}

impl IdempotencyState {
    fn cleanup(&mut self, now: Instant, in_flight_ttl: Duration) {
        let stale: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(key, entry)| {
                let expired = entry.expires_at.is_some_and(|expires| expires <= now)
                    || (entry.expires_at.is_none()
                        && now.saturating_duration_since(entry.created_at) >= in_flight_ttl);
                expired.then(|| key.clone())
            })
            .collect();
        for key in stale {
            if let Some(entry) = self.remove(&key)
                && entry.expires_at.is_none()
            {
                entry.sender.send_replace(EntryOutcome::Failed(
                    BridgeError::new(ErrorCode::Timeout, "idempotent operation expired")
                        .retryable(true),
                ));
            }
        }
    }

    fn make_capacity(&mut self, maximum: usize) -> Result<(), BridgeError> {
        if self.entries.len() < maximum {
            return Ok(());
        }
        let oldest_completed = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.expires_at.is_some() && !entry.uncertain)
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest_completed {
            self.remove(&key);
            return Ok(());
        }
        Err(
            BridgeError::new(ErrorCode::Overloaded, "idempotency capacity is exhausted")
                .retryable(true),
        )
    }

    fn complete(
        &mut self,
        token: &IdempotencyToken,
        response: ImageResponse,
        retained_bytes: usize,
        config: &IdempotencyConfig,
    ) {
        if self
            .entries
            .get(&token.record_key)
            .is_none_or(|entry| entry.fingerprint != token.fingerprint)
        {
            return;
        }
        if retained_bytes > config.max_completed_bytes
            || self
                .reserve_completed_bytes(
                    retained_bytes,
                    config.max_completed_bytes,
                    &token.record_key,
                )
                .is_err()
        {
            if let Some(entry) = self.entries.get_mut(&token.record_key) {
                entry.expires_at = Some(Instant::now() + config.completed_ttl);
                entry.uncertain = true;
                entry.retained_bytes = 0;
                entry.sender.send_replace(EntryOutcome::Unknown(
                    BridgeError::new(
                        ErrorCode::Overloaded,
                        "completed response is too large for idempotent replay",
                    )
                    .retryable(false)
                    .with_detail("outcome", "completed_unreplayable"),
                ));
            }
            return;
        }
        self.access_clock = self.access_clock.saturating_add(1);
        if let Some(entry) = self.entries.get_mut(&token.record_key) {
            entry.expires_at = Some(Instant::now() + config.completed_ttl);
            entry.uncertain = false;
            entry.retained_bytes = retained_bytes;
            entry.last_access = self.access_clock;
            self.completed_bytes = self.completed_bytes.saturating_add(retained_bytes);
            entry
                .sender
                .send_replace(EntryOutcome::Completed(Arc::new(response)));
        }
    }

    fn reserve_completed_bytes(
        &mut self,
        incoming: usize,
        maximum: usize,
        exclude: &RecordKey,
    ) -> Result<(), BridgeError> {
        while self.completed_bytes.saturating_add(incoming) > maximum {
            let victim = self
                .entries
                .iter()
                .filter(|(key, entry)| {
                    *key != exclude && entry.retained_bytes > 0 && !entry.uncertain
                })
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone())
                .ok_or_else(|| {
                    BridgeError::new(
                        ErrorCode::Overloaded,
                        "idempotency completed-response byte capacity is exhausted",
                    )
                })?;
            self.remove(&victim);
        }
        Ok(())
    }

    fn remove(&mut self, key: &RecordKey) -> Option<Entry> {
        let entry = self.entries.remove(key)?;
        self.completed_bytes = self.completed_bytes.saturating_sub(entry.retained_bytes);
        Some(entry)
    }
}

fn response_weight(response: &ImageResponse) -> Result<usize, BridgeError> {
    struct Counter(usize);

    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::FileTooLarge, "response weight overflowed")
            })?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(256);
    serde_json::to_writer(&mut counter, response).map_err(|_| {
        BridgeError::new(
            ErrorCode::Internal,
            "could not measure idempotency response weight",
        )
    })?;
    Ok(counter.0)
}

fn request_fingerprint(request: &ImageRequest) -> Result<[u8; 32], BridgeError> {
    let mut request = request.clone();
    request.idempotency_key = None;
    request.timeout_ms = None;
    let encoded = serde_json::to_vec(&request).map_err(|_| {
        BridgeError::new(
            ErrorCode::Internal,
            "could not fingerprint the normalized request",
        )
    })?;
    Ok(Sha256::digest(encoded).into())
}

fn validate_scope(scope: &str) -> Result<(), BridgeError> {
    if scope.is_empty() || scope.len() > MAX_SCOPE_BYTES || scope.chars().any(char::is_control) {
        Err(BridgeError::new(
            ErrorCode::InvalidRequest,
            "invalid idempotency scope",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::manual_let_else, clippy::panic, clippy::unwrap_used)]

    use imagegen_bridge_core::{GenerationParameters, Timings};

    use super::*;

    fn response(id: &str) -> ImageResponse {
        ImageResponse {
            id: id.to_owned(),
            created: 0,
            provider: "fake".to_owned(),
            model: "fake".to_owned(),
            requested: GenerationParameters::default(),
            effective: GenerationParameters::default(),
            normalizations: Vec::new(),
            data: Vec::new(),
            failures: Vec::new(),
            revised_prompt: None,
            usage: None,
            session: None,
            timings: Timings::default(),
            warnings: Vec::new(),
        }
    }

    #[tokio::test]
    async fn replays_completed_response_and_rejects_conflicts() {
        let coordinator = IdempotencyCoordinator::new(IdempotencyConfig::default()).unwrap();
        let request = ImageRequest::generate("same");
        let IdempotencyAction::Leader(token) =
            coordinator.begin("tenant", "key", &request).await.unwrap()
        else {
            panic!("first caller must lead");
        };
        coordinator.complete(token, response("original")).await;
        let IdempotencyAction::Cached(cached) =
            coordinator.begin("tenant", "key", &request).await.unwrap()
        else {
            panic!("completed call must be cached");
        };
        assert_eq!(cached.id, "original");
        let error = match coordinator
            .begin("tenant", "key", &ImageRequest::generate("different"))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("a conflicting request must fail"),
        };
        assert_eq!(error.code, ErrorCode::IdempotencyConflict);
    }

    #[tokio::test]
    async fn followers_receive_the_leader_result() {
        let coordinator =
            Arc::new(IdempotencyCoordinator::new(IdempotencyConfig::default()).unwrap());
        let request = ImageRequest::generate("same");
        let IdempotencyAction::Leader(token) =
            coordinator.begin("tenant", "key", &request).await.unwrap()
        else {
            panic!("first caller must lead");
        };
        let IdempotencyAction::Wait(receiver) =
            coordinator.begin("tenant", "key", &request).await.unwrap()
        else {
            panic!("second caller must wait");
        };
        let follower = tokio::spawn(async move {
            IdempotencyAction::wait(
                receiver,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
        });
        coordinator.complete(token, response("leader")).await;
        assert_eq!(follower.await.unwrap().unwrap().id, "leader");
    }

    #[tokio::test]
    async fn unknown_outcome_is_retained_and_cannot_elect_a_second_leader() {
        let coordinator = IdempotencyCoordinator::new(IdempotencyConfig::default()).unwrap();
        let request = ImageRequest::generate("same");
        let IdempotencyAction::Leader(token) = coordinator
            .begin("tenant", "unknown", &request)
            .await
            .unwrap()
        else {
            panic!("first caller must lead");
        };
        coordinator
            .fail(
                token,
                BridgeError::new(ErrorCode::Timeout, "provider result is uncertain")
                    .retryable(true)
                    .with_detail("outcome", "unknown"),
            )
            .await;

        let repeated = match coordinator.begin("tenant", "unknown", &request).await {
            Err(error) => error,
            Ok(_) => panic!("unknown outcome must remain a tombstone"),
        };
        assert_eq!(repeated.code, ErrorCode::Timeout);
        assert!(!repeated.retryable);
        assert_eq!(repeated.details["outcome"], "unknown");

        let conflict = match coordinator
            .begin("tenant", "unknown", &ImageRequest::generate("different"))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("different request must still conflict"),
        };
        assert_eq!(conflict.code, ErrorCode::IdempotencyConflict);
    }

    #[tokio::test]
    async fn completed_response_bytes_are_bounded_and_oversize_keeps_a_tombstone() {
        let mut sample = response("sample");
        sample.revised_prompt = Some("x".repeat(700));
        let weight = response_weight(&sample).unwrap();
        let coordinator = IdempotencyCoordinator::new(IdempotencyConfig {
            max_entries: 10,
            max_completed_bytes: weight + 64,
            ..IdempotencyConfig::default()
        })
        .unwrap();
        let request = ImageRequest::generate("same");

        let IdempotencyAction::Leader(first) = coordinator
            .begin("tenant", "first", &request)
            .await
            .unwrap()
        else {
            panic!("first caller must lead");
        };
        coordinator.complete(first, sample.clone()).await;
        let IdempotencyAction::Leader(second) = coordinator
            .begin("tenant", "second", &request)
            .await
            .unwrap()
        else {
            panic!("second caller must lead");
        };
        coordinator.complete(second, sample).await;
        let state = coordinator.inner.lock().await;
        assert!(state.completed_bytes <= coordinator.config.max_completed_bytes);
        assert_eq!(
            state
                .entries
                .values()
                .filter(|entry| entry.retained_bytes > 0)
                .count(),
            1
        );
        drop(state);

        let mut oversized = response("oversized");
        oversized.revised_prompt = Some("y".repeat(weight * 2));
        let IdempotencyAction::Leader(oversize_token) = coordinator
            .begin("tenant", "oversized", &request)
            .await
            .unwrap()
        else {
            panic!("oversized caller must lead once");
        };
        coordinator.complete(oversize_token, oversized).await;
        let error = match coordinator.begin("tenant", "oversized", &request).await {
            Err(error) => error,
            Ok(_) => panic!("unreplayable completion must retain a tombstone"),
        };
        assert_eq!(error.details["outcome"], "completed_unreplayable");
        assert!(!error.retryable);
    }
}
