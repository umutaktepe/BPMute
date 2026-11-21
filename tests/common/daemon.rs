#![allow(
    dead_code,
    reason = "shared test harness items are used selectively by separate integration test binaries"
)]

use tokio::sync::{mpsc, watch};

use bpmute::daemon::{WearableEvent, WearableSource};

/// Channel-backed [`WearableSource`] test double for integration tests.
pub struct TestWearableSource {
    rx: tokio::sync::Mutex<Option<mpsc::Receiver<WearableEvent>>>,
}

impl TestWearableSource {
    pub fn new() -> (Self, mpsc::Sender<WearableEvent>) {
        let (tx, rx) = mpsc::channel(4096);
        (
            Self {
                rx: tokio::sync::Mutex::new(Some(rx)),
            },
            tx,
        )
    }
}

impl WearableSource for TestWearableSource {
    fn subscribe(&self, _shutdown_rx: watch::Receiver<bool>) -> mpsc::Receiver<WearableEvent> {
        self.rx
            .try_lock()
            .expect("single subscriber")
            .take()
            .expect("only subscribed once")
    }
}
