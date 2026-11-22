#![allow(
    dead_code,
    reason = "shared test harness items are used selectively by separate integration test binaries"
)]

use tokio::sync::{mpsc, watch};

use bpmute::daemon::{WearableEvent, WearableSource};
use bpmute::sleep_engine::EngineEvent;
use bpmute::storage::WearableDescriptor;

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

/// Streams a synthetic `EngineEvent` slice shifted by `base_ts` into `wearable_tx`.
pub async fn send_trace_slice(
    wearable_tx: &mpsc::Sender<WearableEvent>,
    desc: &WearableDescriptor,
    events: &[EngineEvent],
    base_ts: i64,
) {
    for ev in events {
        match ev {
            EngineEvent::StreamState(st) => {
                let mut shifted = *st;
                shifted.ts_ms += base_ts;
                wearable_tx
                    .send(WearableEvent::StreamState {
                        wearable: desc.clone(),
                        state: shifted,
                    })
                    .await
                    .unwrap();
            }
            EngineEvent::HrPacket(pkt) => {
                let mut shifted = pkt.clone();
                shifted.ts_ms += base_ts;
                wearable_tx
                    .send(WearableEvent::HrPacket {
                        wearable: desc.clone(),
                        packet: shifted,
                    })
                    .await
                    .unwrap();
            }
            EngineEvent::StreamDisconnected(dc) => {
                wearable_tx
                    .send(WearableEvent::StreamDisconnected {
                        wearable: desc.clone(),
                        ts_ms: dc.ts_ms + base_ts,
                        reason: dc.reason,
                    })
                    .await
                    .unwrap();
            }
            EngineEvent::DesktopActivity(_) | EngineEvent::GraceOutcome(_) => {}
        }
    }
}
