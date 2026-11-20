//! Staged stream state tracking, silence timers, and lifecycle actions (ADR-0003).

use std::collections::{HashMap, HashSet};

use crate::sleep_engine::{ContactStatus, HrPacket, StreamStatus};
use crate::storage::WearableDescriptor;

/// Reason for a Bluetooth LE disconnection reported by BlueZ or the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Wearable initiated a clean disconnect (e.g. user stopped workout or broadcast).
    Remote,
    /// Connection timed out (e.g. RF drop or out of range). Debounced by 15-20 s.
    Timeout,
    /// Authentication or pairing key mismatch (e.g. stale bond keys). Actionable alert.
    Authentication,
    /// Other reason string or numeric BlueZ error.
    Other(String),
}

/// Lifecycle action or diagnostic emitted by [`StreamTracker`].
#[derive(Debug, Clone, PartialEq)]
pub enum LifecycleAction {
    /// State machine transitioned to a new stream state.
    StatusTransition {
        mac: String,
        status: StreamStatus,
        ts_ms: i64,
    },
    /// Directs BlueZ driver to call `GattCharacteristic1.StartNotify()`.
    StartNotify { mac: String },
    /// Directs BlueZ driver to verify `GattCharacteristic1.Notifying` after 30 s silence.
    CheckNotifying { mac: String },
    /// Replace-in-place desktop notification when wearable is connected and streaming starts.
    NotifyConnected {
        mac: String,
        name: String,
        battery: Option<u8>,
    },
    /// Desktop notification when wearable cleanly disconnects or RF timeout debounces.
    NotifyDisconnected { mac: String, name: String },
    /// Actionable alert for authentication failure or GATT permission denial.
    AlertActionRequired { mac: String, message: String },
}

/// Incoming event driving the [`StreamTracker`].
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Connected {
        mac: String,
        ts_ms: i64,
    },
    NotifyStarted {
        mac: String,
        ts_ms: i64,
    },
    Packet {
        mac: String,
        packet: HrPacket,
    },
    BatteryUpdated {
        mac: String,
        percentage: u8,
    },
    Disconnected {
        mac: String,
        reason: DisconnectReason,
        ts_ms: i64,
    },
    Tick {
        ts_ms: i64,
    },
}

struct PendingRfDisconnect {
    mac: String,
    name: String,
    disconnected_at_ms: i64,
}

/// Tracks staged stream states (`Disconnected -> Connecting -> Acquiring -> Streaming <-> SignalLost`),
/// manages silence/optical-lock timers, and arbitrates single active stream selection across multiple Wearables.
pub struct StreamTracker {
    enrolled: HashMap<String, WearableDescriptor>,
    connected: HashSet<String>,
    active_mac: Option<String>,
    standby_macs: Vec<String>,
    active_status: Option<StreamStatus>,
    acquiring_started_ts_ms: Option<i64>,
    last_valid_packet_ts_ms: Option<i64>,
    last_any_packet_ts_ms: Option<i64>,
    notifying_checked: bool,
    pending_rf_disconnect: Option<PendingRfDisconnect>,
    batteries: HashMap<String, u8>,
}

impl Default for StreamTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamTracker {
    /// Creates an empty stream tracker.
    pub fn new() -> Self {
        Self {
            enrolled: HashMap::new(),
            connected: HashSet::new(),
            active_mac: None,
            standby_macs: Vec::new(),
            active_status: None,
            acquiring_started_ts_ms: None,
            last_valid_packet_ts_ms: None,
            last_any_packet_ts_ms: None,
            notifying_checked: false,
            pending_rf_disconnect: None,
            batteries: HashMap::new(),
        }
    }

    /// Registers an enrolled Wearable allowlist entry.
    pub fn add_enrolled(&mut self, desc: WearableDescriptor) {
        let norm_mac = desc.mac.to_ascii_uppercase();
        self.enrolled.insert(norm_mac, desc);
    }

    /// Removes a Wearable from the allowlist.
    pub fn remove_enrolled(&mut self, mac: &str) {
        let norm = mac.to_ascii_uppercase();
        self.enrolled.remove(&norm);
        self.connected.remove(&norm);
        self.standby_macs.retain(|m| m != &norm);
        if matches!(self.pending_rf_disconnect, Some(ref pending) if pending.mac == norm) {
            self.pending_rf_disconnect = None;
        }
        if self.active_mac.as_deref() == Some(&norm) {
            self.active_mac = None;
            self.active_status = None;
            self.acquiring_started_ts_ms = None;
            self.last_valid_packet_ts_ms = None;
            self.last_any_packet_ts_ms = None;
            self.notifying_checked = false;
        }
    }

    /// Returns whether a Wearable MAC is currently marked connected.
    pub fn is_connected(&self, mac: &str) -> bool {
        self.connected.contains(&mac.to_ascii_uppercase())
    }

    /// Returns the currently active Wearable MAC address.
    pub fn active_mac(&self) -> Option<&str> {
        self.active_mac.as_deref()
    }

    /// Returns the active Wearable's current [`StreamStatus`].
    pub fn active_stream_status(&self) -> Option<StreamStatus> {
        self.active_status
    }

    /// Processes an incoming [`StreamEvent`] and returns all lifecycle actions to execute.
    pub fn handle_event(&mut self, event: StreamEvent) -> Vec<LifecycleAction> {
        let mut actions = Vec::new();

        match event {
            StreamEvent::Connected { mac, ts_ms } => {
                let norm = mac.to_ascii_uppercase();
                if !self.enrolled.contains_key(&norm) {
                    return actions;
                }

                // If this device had a pending RF disconnect, cancel the debounced notification
                if let Some(ref pending) = self.pending_rf_disconnect
                    && pending.mac == norm
                {
                    self.pending_rf_disconnect = None;
                }

                self.connected.insert(norm.clone());

                if self.active_mac.is_none() {
                    self.transition_status(&norm, StreamStatus::Connecting, ts_ms, &mut actions);
                    actions.push(LifecycleAction::StartNotify { mac: norm });
                } else if self.active_mac.as_ref() != Some(&norm)
                    && !self.standby_macs.contains(&norm)
                {
                    self.standby_macs.push(norm.clone());
                    actions.push(LifecycleAction::StatusTransition {
                        mac: norm,
                        status: StreamStatus::PassiveStandby,
                        ts_ms,
                    });
                }
            }

            StreamEvent::NotifyStarted { mac, ts_ms } => {
                let norm = mac.to_ascii_uppercase();
                if self.active_mac.as_ref() == Some(&norm) {
                    self.transition_status(&norm, StreamStatus::Acquiring, ts_ms, &mut actions);

                    let name = self.wearable_name(&norm);
                    let battery = self.batteries.get(&norm).copied();
                    actions.push(LifecycleAction::NotifyConnected {
                        mac: norm,
                        name,
                        battery,
                    });
                }
            }

            StreamEvent::Packet { mac, packet } => {
                let norm = mac.to_ascii_uppercase();
                if self.active_mac.as_ref() != Some(&norm) {
                    return actions;
                }

                self.last_any_packet_ts_ms = Some(packet.ts_ms);
                self.notifying_checked = false;

                let is_valid =
                    packet.bpm > 0.0 && packet.contact_status != ContactStatus::NoContact;

                if is_valid {
                    self.last_valid_packet_ts_ms = Some(packet.ts_ms);
                    if self.active_status != Some(StreamStatus::Streaming) {
                        self.transition_status(
                            &norm,
                            StreamStatus::Streaming,
                            packet.ts_ms,
                            &mut actions,
                        );
                    }
                } else {
                    // Invalid reading (0 BPM or lost skin contact) -> SignalLost
                    if self.active_status == Some(StreamStatus::Streaming)
                        || self.active_status == Some(StreamStatus::Acquiring)
                    {
                        self.transition_status(
                            &norm,
                            StreamStatus::SignalLost,
                            packet.ts_ms,
                            &mut actions,
                        );
                    }
                }
            }

            StreamEvent::BatteryUpdated { mac, percentage } => {
                let norm = mac.to_ascii_uppercase();
                self.batteries.insert(norm, percentage);
            }

            StreamEvent::Disconnected { mac, reason, ts_ms } => {
                let norm = mac.to_ascii_uppercase();
                self.connected.remove(&norm);
                self.standby_macs.retain(|m| m != &norm);

                if self.active_mac.as_ref() == Some(&norm) {
                    self.transition_status(&norm, StreamStatus::Disconnected, ts_ms, &mut actions);

                    let name = self.wearable_name(&norm);

                    match reason {
                        DisconnectReason::Remote => {
                            actions.push(LifecycleAction::NotifyDisconnected {
                                mac: norm.clone(),
                                name,
                            });
                        }
                        DisconnectReason::Timeout => {
                            self.pending_rf_disconnect = Some(PendingRfDisconnect {
                                mac: norm.clone(),
                                name,
                                disconnected_at_ms: ts_ms,
                            });
                        }
                        DisconnectReason::Authentication => {
                            actions.push(LifecycleAction::AlertActionRequired {
                                mac: norm.clone(),
                                message: format!(
                                    "Authentication failure for {name}; re-pair/re-enroll Wearable"
                                ),
                            });
                        }
                        DisconnectReason::Other(ref err) => {
                            if err.contains("NotPermitted") {
                                actions.push(LifecycleAction::AlertActionRequired {
                                    mac: norm.clone(),
                                    message: format!(
                                        "{name} rejected GATT subscription (NotPermitted); enable Heart Rate Push or check Wearable support"
                                    ),
                                });
                            }
                        }
                    }

                    // Auto-promote standby wearable
                    if let Some(next_mac) = self.standby_macs.first().cloned() {
                        self.standby_macs.remove(0);
                        self.transition_status(
                            &next_mac,
                            StreamStatus::Connecting,
                            ts_ms,
                            &mut actions,
                        );
                        actions.push(LifecycleAction::StartNotify { mac: next_mac });
                    }
                }
            }

            StreamEvent::Tick { ts_ms } => {
                // 1. Check pending RF disconnect debounce (15 s)
                if let Some(ref pending) = self.pending_rf_disconnect
                    && ts_ms >= pending.disconnected_at_ms + 15_000
                {
                    actions.push(LifecycleAction::NotifyDisconnected {
                        mac: pending.mac.clone(),
                        name: pending.name.clone(),
                    });
                    self.pending_rf_disconnect = None;
                }

                // 2. Check active stream timers
                let Some(ref active) = self.active_mac.clone() else {
                    return actions;
                };

                match self.active_status {
                    Some(StreamStatus::Acquiring) => {
                        // 20 s timeout for Acquiring -> SignalLost
                        if let Some(started) = self.acquiring_started_ts_ms
                            && ts_ms >= started + 20_000
                        {
                            self.transition_status(
                                active,
                                StreamStatus::SignalLost,
                                ts_ms,
                                &mut actions,
                            );
                        }
                    }
                    Some(StreamStatus::Streaming) => {
                        // 10 s packet silence -> SignalLost
                        let last_ts = self
                            .last_valid_packet_ts_ms
                            .or(self.acquiring_started_ts_ms)
                            .unwrap_or(ts_ms);
                        if ts_ms >= last_ts + 10_000 {
                            self.transition_status(
                                active,
                                StreamStatus::SignalLost,
                                ts_ms,
                                &mut actions,
                            );
                        }
                    }
                    Some(StreamStatus::SignalLost) => {
                        // 30 s silence -> non-destructive Notifying check
                        let last_ts = self
                            .last_any_packet_ts_ms
                            .or(self.acquiring_started_ts_ms)
                            .unwrap_or(ts_ms);
                        if !self.notifying_checked && ts_ms >= last_ts + 30_000 {
                            self.notifying_checked = true;
                            actions.push(LifecycleAction::CheckNotifying {
                                mac: active.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        actions
    }

    fn transition_status(
        &mut self,
        mac: &str,
        status: StreamStatus,
        ts_ms: i64,
        actions: &mut Vec<LifecycleAction>,
    ) {
        let norm = mac.to_ascii_uppercase();
        self.active_status = Some(status);
        match status {
            StreamStatus::Disconnected => {
                self.active_mac = None;
                self.acquiring_started_ts_ms = None;
                self.last_valid_packet_ts_ms = None;
                self.last_any_packet_ts_ms = None;
                self.notifying_checked = false;
            }
            StreamStatus::Connecting => {
                self.active_mac = Some(norm.clone());
                self.acquiring_started_ts_ms = None;
                self.last_valid_packet_ts_ms = None;
                self.last_any_packet_ts_ms = None;
                self.notifying_checked = false;
            }
            StreamStatus::Acquiring => {
                self.acquiring_started_ts_ms = Some(ts_ms);
                self.last_valid_packet_ts_ms = None;
                self.last_any_packet_ts_ms = None;
                self.notifying_checked = false;
            }
            StreamStatus::Streaming => {
                // Keep packet timers
            }
            StreamStatus::SignalLost => {
                // Keep packet timers for 30s check
            }
            StreamStatus::PassiveStandby => {}
        }
        actions.push(LifecycleAction::StatusTransition {
            mac: norm,
            status,
            ts_ms,
        });
    }

    fn wearable_name(&self, mac: &str) -> String {
        self.enrolled
            .get(mac)
            .and_then(|w| w.alias.as_deref().or(w.advertised_name.as_deref()))
            .unwrap_or("Wearable")
            .to_string()
    }
}
