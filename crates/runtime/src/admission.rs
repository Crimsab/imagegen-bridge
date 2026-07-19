//! Bounded concurrency gates with explicit queue capacity.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use imagegen_bridge_core::{BridgeError, ErrorCode};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub(crate) struct AdmissionGate {
    semaphore: Arc<Semaphore>,
    queued: Arc<AtomicUsize>,
    max_queued: usize,
    label: Arc<str>,
}

impl AdmissionGate {
    pub(crate) fn new(
        max_concurrent: usize,
        max_queued: usize,
        label: impl Into<Arc<str>>,
    ) -> Result<Self, BridgeError> {
        if max_concurrent == 0 {
            return Err(BridgeError::new(
                ErrorCode::Configuration,
                "concurrency limits must be greater than zero",
            ));
        }
        let permits = if max_concurrent == usize::MAX {
            Semaphore::MAX_PERMITS
        } else {
            max_concurrent
        };
        Ok(Self {
            semaphore: Arc::new(Semaphore::new(permits)),
            queued: Arc::new(AtomicUsize::new(0)),
            max_queued,
            label: label.into(),
        })
    }

    pub(crate) async fn acquire(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, BridgeError> {
        if let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() {
            return Ok(permit);
        }
        let reservation = self.reserve_queue()?;
        if deadline <= Instant::now() {
            return Err(timeout_error(&self.label));
        }
        let permit = tokio::select! {
            permit = Arc::clone(&self.semaphore).acquire_owned() => {
                permit.map_err(|_| BridgeError::new(
                    ErrorCode::Cancelled,
                    "runtime is shutting down",
                ))?
            }
            () = cancellation.cancelled() => {
                return Err(BridgeError::new(
                    ErrorCode::Cancelled,
                    "request was cancelled while waiting for capacity",
                ));
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err(timeout_error(&self.label));
            }
        };
        drop(reservation);
        Ok(permit)
    }

    pub(crate) fn close(&self) {
        self.semaphore.close();
    }

    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    fn reserve_queue(&self) -> Result<QueueReservation, BridgeError> {
        let mut current = self.queued.load(Ordering::Acquire);
        loop {
            if self.max_queued != usize::MAX && current >= self.max_queued {
                return Err(BridgeError::new(
                    ErrorCode::Overloaded,
                    "runtime queue capacity is exhausted",
                )
                .retryable(true)
                .with_detail("limit", self.label.as_ref()));
            }
            match self.queued.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(QueueReservation {
                        queued: Arc::clone(&self.queued),
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }
}

struct QueueReservation {
    queued: Arc<AtomicUsize>,
}

impl Drop for QueueReservation {
    fn drop(&mut self) {
        self.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

fn timeout_error(label: &str) -> BridgeError {
    BridgeError::new(
        ErrorCode::Timeout,
        "request deadline elapsed while waiting for capacity",
    )
    .retryable(true)
    .with_detail("limit", label)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn rejects_waiters_beyond_the_bounded_queue() {
        let gate = Arc::new(AdmissionGate::new(1, 1, "test").unwrap());
        let cancellation = CancellationToken::new();
        let first = gate
            .acquire(Instant::now() + Duration::from_secs(5), &cancellation)
            .await
            .unwrap();
        let waiting_gate = Arc::clone(&gate);
        let waiting_cancel = cancellation.clone();
        let waiter = tokio::spawn(async move {
            waiting_gate
                .acquire(Instant::now() + Duration::from_secs(5), &waiting_cancel)
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(gate.queued(), 1);
        let error = gate
            .acquire(Instant::now() + Duration::from_secs(5), &cancellation)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Overloaded);
        drop(first);
        assert!(waiter.await.unwrap().is_ok());
        assert_eq!(gate.queued(), 0);
    }

    #[tokio::test]
    async fn cancellation_releases_a_queue_slot() {
        let gate = Arc::new(AdmissionGate::new(1, 1, "test").unwrap());
        let first_cancel = CancellationToken::new();
        let _first = gate
            .acquire(Instant::now() + Duration::from_secs(5), &first_cancel)
            .await
            .unwrap();
        let waiting_gate = Arc::clone(&gate);
        let waiting_cancel = CancellationToken::new();
        let task_cancel = waiting_cancel.clone();
        let waiter = tokio::spawn(async move {
            waiting_gate
                .acquire(Instant::now() + Duration::from_secs(5), &task_cancel)
                .await
        });
        tokio::task::yield_now().await;
        waiting_cancel.cancel();
        let error = waiter.await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(gate.queued(), 0);
    }

    #[tokio::test]
    async fn unlimited_gate_admits_independent_calls_without_queueing() {
        let gate = AdmissionGate::new(usize::MAX, usize::MAX, "unlimited").unwrap();
        let cancellation = CancellationToken::new();
        let mut permits = Vec::new();
        for _ in 0..8 {
            permits.push(
                gate.acquire(Instant::now() + Duration::from_secs(1), &cancellation)
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(gate.queued(), 0);
        assert_eq!(permits.len(), 8);
    }
}
