//! Detail-free cached readiness for the public health route.

use std::{
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use imagegen_bridge_runtime::{ProviderReadinessStatus, ProviderRegistry};

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const STALE_AFTER: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicReadiness {
    Ready,
    NotReady,
}

#[derive(Debug, Clone, Copy)]
struct Snapshot {
    ready: bool,
    updated_at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct ReadinessCache {
    snapshot: RwLock<Option<Snapshot>>,
    started: AtomicBool,
}

impl ReadinessCache {
    pub(crate) fn start(self: &Arc<Self>, registry: ProviderRegistry) {
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.started.store(false, Ordering::Release);
            return;
        };
        let cache = Arc::downgrade(self);
        handle.spawn(refresh_loop(cache, registry));
    }

    pub(crate) fn status(&self) -> PublicReadiness {
        let Ok(snapshot) = self.snapshot.read() else {
            return PublicReadiness::NotReady;
        };
        match *snapshot {
            Some(snapshot) if snapshot.ready && snapshot.updated_at.elapsed() <= STALE_AFTER => {
                PublicReadiness::Ready
            }
            _ => PublicReadiness::NotReady,
        }
    }

    fn update(&self, ready: bool) {
        if let Ok(mut snapshot) = self.snapshot.write() {
            *snapshot = Some(Snapshot {
                ready,
                updated_at: Instant::now(),
            });
        }
    }
}

async fn refresh_loop(cache: Weak<ReadinessCache>, registry: ProviderRegistry) {
    loop {
        let Some(cache) = cache.upgrade() else {
            return;
        };
        let ready = tokio::time::timeout(PROBE_TIMEOUT, registry.readiness())
            .await
            .is_ok_and(|providers| {
                providers
                    .iter()
                    .all(|check| matches!(check.status, ProviderReadinessStatus::Ready))
            });
        cache.update(ready);
        drop(cache);
        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}
