#![allow(
    dead_code,
    reason = "shared test harness items are used selectively by separate integration test binaries"
)]

use tokio::sync::{mpsc, watch};

use bpmute::daemon::{WearableEvent, WearableSource};
use bpmute::sleep_engine::{ContactStatus, EngineEvent, HrPacket};
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

/// Parameters for streaming synthetic heart-rate packets with biological wave variation.
#[derive(Debug, Clone)]
pub struct SyntheticStreamParams {
    pub start_ts_ms: i64,
    pub count_sec: usize,
    pub base_bpm: f64,
    pub bpm_amplitude: f64,
    pub contact: ContactStatus,
}

impl Default for SyntheticStreamParams {
    fn default() -> Self {
        Self {
            start_ts_ms: 0,
            count_sec: 60,
            base_bpm: 72.0,
            bpm_amplitude: 2.0,
            contact: ContactStatus::Contact,
        }
    }
}

/// Streams synthetic heart-rate packets at 1 Hz with biological wave variation.
///
/// Prevents PPG coverage gate drops (<80%) and flatline filters (>=30s identical BPM/RR)
/// from freezing SleepEngine accumulators in integration tests.
pub async fn send_synthetic_wave_stream(
    wearable_tx: &mpsc::Sender<WearableEvent>,
    desc: &WearableDescriptor,
    params: SyntheticStreamParams,
) {
    for sec in 0..params.count_sec {
        let ts_ms = params.start_ts_ms + (sec as i64) * 1000;
        let wave = params.bpm_amplitude * (sec as f64 * 0.11).sin()
            + (params.bpm_amplitude * 0.4) * (sec as f64 * 0.31).cos();
        let bpm = params.base_bpm + wave;
        let rr_ms = vec![60_000.0 / bpm + 14.0 * (sec as f64 * 1.5).sin()];
        wearable_tx
            .send(WearableEvent::HrPacket {
                wearable: desc.clone(),
                packet: HrPacket {
                    ts_ms,
                    bpm,
                    rr_ms,
                    contact_status: params.contact,
                },
            })
            .await
            .unwrap();
    }
}

/// Streams a synthetic 1 Hz sleep-onset heart-rate descent with RR intervals into `wearable_tx`.
pub async fn send_sleep_descent_stream(
    wearable_tx: &mpsc::Sender<WearableEvent>,
    desc: &WearableDescriptor,
    start_ts_ms: i64,
    count_sec: usize,
    wake_bpm: f64,
    sleep_bpm: f64,
) {
    for sec in 0..count_sec {
        let ts_ms = start_ts_ms + (sec as i64) * 1000;
        let mins = sec as f64 / 60.0;
        let target = (wake_bpm - 0.8 * mins).max(sleep_bpm);
        let bpm = target + 0.35 * (sec as f64 * 0.4).sin();
        let rr_ms = vec![60_000.0 / bpm + 26.0 * (sec as f64 * 1.5).sin()];
        wearable_tx
            .send(WearableEvent::HrPacket {
                wearable: desc.clone(),
                packet: HrPacket {
                    ts_ms,
                    bpm,
                    rr_ms,
                    contact_status: ContactStatus::Contact,
                },
            })
            .await
            .unwrap();
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
