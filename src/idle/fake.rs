use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use super::traits::{BoxFuture, IdleBackendKind, IdleProbe, IdleProbeError};

/// In-memory fake implementation of [`IdleProbe`] for unit and virtual-time tests.
pub struct FakeIdleProbe {
    backend_kind: Mutex<IdleBackendKind>,
    user_active_trailing: AtomicBool,
    activity_tx: broadcast::Sender<()>,
}

impl Default for FakeIdleProbe {
    fn default() -> Self {
        Self::new(IdleBackendKind::WaylandExtIdleV2)
    }
}

impl FakeIdleProbe {
    /// Creates a new fake idle probe with the specified backend kind.
    pub fn new(backend_kind: IdleBackendKind) -> Self {
        let (activity_tx, _) = broadcast::channel(32);
        Self {
            backend_kind: Mutex::new(backend_kind),
            user_active_trailing: AtomicBool::new(false),
            activity_tx,
        }
    }

    /// Sets the backend kind reported by this fake probe.
    pub fn set_backend_kind(&self, kind: IdleBackendKind) {
        *self.backend_kind.lock().unwrap() = kind;
    }

    /// Configures whether trailing activity was detected in the pre-gate window.
    pub fn set_user_active_trailing(&self, active: bool) {
        self.user_active_trailing.store(active, Ordering::SeqCst);
    }

    /// Triggers a simulated desktop user activity event (pointer motion, click, or keypress).
    pub fn trigger_activity(&self) {
        let _ = self.activity_tx.send(());
    }
}

impl IdleProbe for FakeIdleProbe {
    fn backend_kind(&self) -> IdleBackendKind {
        *self.backend_kind.lock().unwrap()
    }

    fn is_user_active_trailing<'a>(
        &'a self,
        _window: Duration,
    ) -> BoxFuture<'a, Result<bool, IdleProbeError>> {
        Box::pin(async move { Ok(self.user_active_trailing.load(Ordering::SeqCst)) })
    }

    fn watch_grace_period_activity<'a>(
        &'a self,
        settle_delay: Duration,
    ) -> BoxFuture<'a, Result<mpsc::Receiver<()>, IdleProbeError>> {
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(16);
            let mut sub_rx = self.activity_tx.subscribe();
            let start = tokio::time::Instant::now();

            tokio::spawn(async move {
                while let Ok(()) = sub_rx.recv().await {
                    if tokio::time::Instant::now().duration_since(start) >= settle_delay {
                        if tx.send(()).await.is_err() {
                            break;
                        }
                    } else {
                        tracing::debug!(
                            "fake idle probe: user activity during settle delay suppressed"
                        );
                    }
                }
            });

            Ok(rx)
        })
    }

    fn watch_post_trigger_activity<'a>(&'a self, timeout: Duration) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut sub_rx = self.activity_tx.subscribe();
            tokio::select! {
                biased;
                Ok(()) = sub_rx.recv() => true,
                _ = tokio::time::sleep(timeout) => false,
            }
        })
    }
}
