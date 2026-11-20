//! Narrow async boundary trait for incoming Wearable BLE heart-rate streams (ADR-0007).

use tokio::sync::{mpsc, watch};

use crate::sleep_engine::{HrPacket, StreamState};

pub use crate::storage::WearableDescriptor;

/// Timestamped event emitted by an active [`WearableSource`].
#[derive(Debug, Clone, PartialEq)]
pub enum WearableEvent {
    StreamState {
        wearable: WearableDescriptor,
        state: StreamState,
    },
    HrPacket {
        wearable: WearableDescriptor,
        packet: HrPacket,
    },
    Metadata {
        wearable: WearableDescriptor,
        battery: Option<u8>,
        body_location: Option<String>,
    },
}

/// Async source of timestamped Wearable heart-rate and stream-lifecycle events.
pub trait WearableSource: Send + Sync {
    /// Subscribes to the Wearable stream until `shutdown_rx` signals `true` or the source stops.
    fn subscribe(&self, shutdown_rx: watch::Receiver<bool>) -> mpsc::Receiver<WearableEvent>;
}

/// Default no-op [`WearableSource`] used when BLE streaming is inactive (until `#18`).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullWearableSource;

impl WearableSource for NullWearableSource {
    fn subscribe(&self, mut shutdown_rx: watch::Receiver<bool>) -> mpsc::Receiver<WearableEvent> {
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            while !*shutdown_rx.borrow() {
                if shutdown_rx.changed().await.is_err() {
                    break;
                }
            }
            drop(tx);
        });
        rx
    }
}
