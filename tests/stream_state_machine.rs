use bpmute::bluetooth::operations::parse_bluez_disconnect_reason;
use bpmute::bluetooth::stream::{DisconnectReason, LifecycleAction, StreamEvent, StreamTracker};
use bpmute::sleep_engine::{ContactStatus, HrPacket, StreamDisconnectReason, StreamStatus};
use bpmute::storage::WearableDescriptor;

fn make_wearable(mac: &str, alias: &str) -> WearableDescriptor {
    WearableDescriptor {
        mac: mac.to_string(),
        address_type: "public".to_string(),
        alias: Some(alias.to_string()),
        advertised_name: Some(alias.to_string()),
        supports_rr: true,
        supports_contact: true,
    }
}

#[test]
fn staged_transitions_acquiring_streaming_and_signal_lost_after_silence() {
    let w = make_wearable("AA:BB:CC:DD:EE:01", "Polar H10");
    let mut tracker = StreamTracker::new();
    tracker.add_enrolled(w.clone());

    // 1. Connection begins -> Connecting
    let _events = tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 1_000,
    });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Connecting)
    );

    // 2. Services resolved & notify started -> Acquiring
    let _events = tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 2_000,
    });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Acquiring)
    );

    // 3. Valid HR packet arrives -> Streaming
    let pkt = HrPacket {
        ts_ms: 2_500,
        bpm: 72.0,
        rr_ms: vec![833.0],
        contact_status: ContactStatus::Contact,
    };
    let _events = tracker.handle_event(StreamEvent::Packet {
        mac: w.mac.clone(),
        packet: pkt,
    });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Streaming)
    );

    // 4. 9 seconds of silence -> still Streaming
    let _events = tracker.handle_event(StreamEvent::Tick { ts_ms: 11_500 });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Streaming)
    );

    // 5. 10 seconds of silence (12_500 ms) -> transitions to SignalLost
    let _events = tracker.handle_event(StreamEvent::Tick { ts_ms: 12_500 });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::SignalLost)
    );

    // 6. After 30 seconds of silence (32_500 ms) -> triggers non-destructive Notifying check
    let events = tracker.handle_event(StreamEvent::Tick { ts_ms: 32_500 });
    assert!(
        events
            .iter()
            .any(|a| matches!(a, LifecycleAction::CheckNotifying { .. })),
        "expected CheckNotifying action after 30s silence"
    );

    // 7. Valid packet resumes -> recovers back to Streaming
    let resume_pkt = HrPacket {
        ts_ms: 33_000,
        bpm: 70.0,
        rr_ms: vec![850.0],
        contact_status: ContactStatus::Contact,
    };
    let _events = tracker.handle_event(StreamEvent::Packet {
        mac: w.mac.clone(),
        packet: resume_pkt,
    });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Streaming)
    );
}

#[test]
fn acquiring_times_out_to_signal_lost_after_20_seconds() {
    let w = make_wearable("AA:BB:CC:DD:EE:02", "Huawei Watch GT5");
    let mut tracker = StreamTracker::new();
    tracker.add_enrolled(w.clone());

    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 1_000,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 1_500,
    });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Acquiring)
    );

    // Optical lock takes time (e.g. 19 seconds) -> still Acquiring
    tracker.handle_event(StreamEvent::Tick { ts_ms: 20_500 });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Acquiring)
    );

    // After 20 seconds without valid BPM (21_501 ms) -> SignalLost
    tracker.handle_event(StreamEvent::Tick { ts_ms: 21_501 });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::SignalLost)
    );
}

#[test]
fn zero_bpm_or_contact_lost_immediately_transitions_to_signal_lost() {
    let w = make_wearable("AA:BB:CC:DD:EE:03", "Strap");
    let mut tracker = StreamTracker::new();
    tracker.add_enrolled(w.clone());

    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 1_000,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 1_500,
    });
    tracker.handle_event(StreamEvent::Packet {
        mac: w.mac.clone(),
        packet: HrPacket {
            ts_ms: 2_000,
            bpm: 65.0,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        },
    });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Streaming)
    );

    // Zero BPM
    tracker.handle_event(StreamEvent::Packet {
        mac: w.mac.clone(),
        packet: HrPacket {
            ts_ms: 3_000,
            bpm: 0.0,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        },
    });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::SignalLost)
    );

    // Resume
    tracker.handle_event(StreamEvent::Packet {
        mac: w.mac.clone(),
        packet: HrPacket {
            ts_ms: 4_000,
            bpm: 66.0,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        },
    });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Streaming)
    );

    // NoContact
    tracker.handle_event(StreamEvent::Packet {
        mac: w.mac.clone(),
        packet: HrPacket {
            ts_ms: 5_000,
            bpm: 66.0,
            rr_ms: vec![],
            contact_status: ContactStatus::NoContact,
        },
    });
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::SignalLost)
    );
}

#[test]
fn single_active_stream_and_secondary_standby_auto_promotion() {
    let w1 = make_wearable("11:11:11:11:11:11", "Watch 1");
    let w2 = make_wearable("22:22:22:22:22:22", "Strap 2");
    let mut tracker = StreamTracker::new();
    tracker.add_enrolled(w1.clone());
    tracker.add_enrolled(w2.clone());

    // w1 connects first -> becomes active
    tracker.handle_event(StreamEvent::Connected {
        mac: w1.mac.clone(),
        ts_ms: 1_000,
    });
    assert_eq!(tracker.active_mac(), Some(w1.mac.as_str()));

    // w2 connects while w1 is active -> kept in passive standby (no notify action for w2)
    let actions = tracker.handle_event(StreamEvent::Connected {
        mac: w2.mac.clone(),
        ts_ms: 1_500,
    });
    assert_eq!(tracker.active_mac(), Some(w1.mac.as_str()));
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::StartNotify { mac } if mac == &w2.mac))
    );

    // w1 disconnects -> w2 is auto-promoted!
    let actions = tracker.handle_event(StreamEvent::Disconnected {
        mac: w1.mac.clone(),
        reason: DisconnectReason::Remote,
        ts_ms: 2_000,
    });
    assert_eq!(tracker.active_mac(), Some(w2.mac.as_str()));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::StartNotify { mac } if mac == &w2.mac))
    );
}

#[test]
fn disconnect_notifications_and_rf_timeout_debounce() {
    let w = make_wearable("AA:BB:CC:DD:EE:99", "Watch");
    let mut tracker = StreamTracker::new();
    tracker.add_enrolled(w.clone());

    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 1_000,
    });
    let actions = tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 2_000,
    });
    // Immediate connection lifecycle notification
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyConnected { .. }))
    );

    // Remote disconnect -> immediate notification
    let actions = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Remote,
        ts_ms: 3_000,
    });
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { .. }))
    );

    // Reconnect and test RF timeout debounce (15-20 s)
    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 4_000,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 5_000,
    });

    let actions = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Timeout,
        ts_ms: 6_000,
    });
    // RF timeout is debounced: should NOT immediately emit disconnect notification
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { .. }))
    );

    // 14 seconds later (ts_ms = 20_000): debounce not expired yet (< 15s)
    let actions = tracker.handle_event(StreamEvent::Tick { ts_ms: 20_000 });
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { .. }))
    );

    // 16 seconds after disconnect (ts_ms = 22_000 >= 6_000 + 15_000): debounce expired -> fires notification!
    let actions = tracker.handle_event(StreamEvent::Tick { ts_ms: 22_000 });
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { .. }))
    );
}

#[test]
fn battery_is_cleared_on_disconnect_and_ignored_while_disconnected() {
    let w = make_wearable("AA:BB:CC:DD:EE:42", "Polar H10");
    let mut tracker = StreamTracker::new();
    tracker.add_enrolled(w.clone());

    // Battery update while disconnected must not stick across a later connection
    tracker.handle_event(StreamEvent::BatteryUpdated {
        mac: w.mac.clone(),
        percentage: 64,
    });
    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 1_000,
    });
    let actions = tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 1_500,
    });
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyConnected { battery: None, .. }))
    );

    // Battery update while connected is reflected on NotifyConnected if sent before NotifyStarted,
    // and then cleared on disconnect so a subsequent reconnect does not emit stale battery.
    tracker.handle_event(StreamEvent::BatteryUpdated {
        mac: w.mac.clone(),
        percentage: 88,
    });
    tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Remote,
        ts_ms: 2_000,
    });

    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 3_000,
    });
    let actions = tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 3_500,
    });
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyConnected { battery: None, .. }))
    );
}

#[test]
fn bluez_reason_mapping_and_remote_before_timeout_deduplication() {
    assert_eq!(
        parse_bluez_disconnect_reason("org.bluez.Reason.Remote"),
        DisconnectReason::Remote
    );
    assert_eq!(
        parse_bluez_disconnect_reason("org.bluez.Reason.Timeout"),
        DisconnectReason::Timeout
    );
    assert_eq!(
        parse_bluez_disconnect_reason("org.bluez.Reason.Authentication"),
        DisconnectReason::Authentication
    );
    assert_eq!(
        parse_bluez_disconnect_reason("org.bluez.Reason.Unknown"),
        DisconnectReason::Other("org.bluez.Reason.Unknown".to_string())
    );
    assert_eq!(
        parse_bluez_disconnect_reason("org.bluez.Reason.Local"),
        DisconnectReason::Other("org.bluez.Reason.Local".to_string())
    );

    let w = make_wearable("AA:BB:CC:DD:EE:50", "Watch");
    let mut tracker = StreamTracker::new();
    tracker.add_enrolled(w.clone());

    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 1_000,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 1_500,
    });

    // 1. Device1.Disconnected("org.bluez.Reason.Remote") arrives before PropertiesChanged(Connected = false)
    let first_actions = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Remote,
        ts_ms: 10_000,
    });
    assert!(first_actions.iter().any(|a| matches!(
        a,
        LifecycleAction::StreamDisconnected {
            mac,
            reason: StreamDisconnectReason::Remote,
            ts_ms: 10_000,
        } if mac == &w.mac
    )));
    assert!(
        first_actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { mac, .. } if mac == &w.mac))
    );
    assert_eq!(
        tracker.active_stream_status(),
        Some(StreamStatus::Disconnected)
    );

    // 2. Trailing PropertiesChanged(Connected = false) mapped to Timeout is deduplicated
    let trailing_actions = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Timeout,
        ts_ms: 10_020,
    });
    assert!(
        trailing_actions.is_empty(),
        "trailing Connected=false after Remote must be deduplicated: {trailing_actions:?}"
    );

    // 3. No 15s RF debounce was armed, so Tick after 15s emits nothing
    let tick_actions = tracker.handle_event(StreamEvent::Tick { ts_ms: 26_000 });
    assert!(
        tick_actions.is_empty(),
        "no debounced NotifyDisconnected should fire after Remote disconnect: {tick_actions:?}"
    );
}

#[test]
fn timeout_before_remote_reconciles_within_upgrade_window_and_cancels_debounce() {
    let w = make_wearable("AA:BB:CC:DD:EE:51", "Watch");
    let mut tracker = StreamTracker::new();
    tracker.add_enrolled(w.clone());

    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 1_000,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 1_500,
    });

    // 1. PropertiesChanged(Connected = false) arrives first -> Timeout + 15s debounce
    let timeout_actions = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Timeout,
        ts_ms: 10_000,
    });
    assert!(timeout_actions.iter().any(|a| matches!(
        a,
        LifecycleAction::StreamDisconnected {
            mac,
            reason: StreamDisconnectReason::Timeout,
            ts_ms: 10_000,
        } if mac == &w.mac
    )));
    assert!(
        !timeout_actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { .. }))
    );

    // 2. Follow-up Device1.Disconnected("org.bluez.Reason.Remote") arrives 50ms later
    let upgrade_actions = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Remote,
        ts_ms: 10_050,
    });
    assert!(
        upgrade_actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { mac, .. } if mac == &w.mac)),
        "upgrade to Remote must fire NotifyDisconnected immediately: {upgrade_actions:?}"
    );
    assert!(
        upgrade_actions.iter().any(|a| matches!(
            a,
            LifecycleAction::StreamDisconnected {
                mac,
                reason: StreamDisconnectReason::Remote,
                ts_ms: 10_000,
            } if mac == &w.mac
        )),
        "upgrade to Remote must emit Remote StreamDisconnected anchored at disconnect_ts: {upgrade_actions:?}"
    );

    // 3. Pending 15s RF debounce was cancelled -> Tick at +16s emits nothing
    let tick_actions = tracker.handle_event(StreamEvent::Tick { ts_ms: 26_000 });
    assert!(
        tick_actions.is_empty(),
        "pending RF debounce must stay cancelled after upgrade: {tick_actions:?}"
    );

    // 4. Outside the brief upgrade window (6s later), late Remote does not override Timeout
    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 29_000,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 29_500,
    });
    tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Timeout,
        ts_ms: 30_000,
    });
    let late_actions = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Remote,
        ts_ms: 36_000,
    });
    assert!(
        late_actions.is_empty(),
        "Remote outside upgrade window must be ignored: {late_actions:?}"
    );
    let debounced_actions = tracker.handle_event(StreamEvent::Tick { ts_ms: 45_000 });
    assert!(
        debounced_actions
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { mac, .. } if mac == &w.mac))
    );

    // 5. Reconnecting within upgrade window clears stale reconciliation state
    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 50_000,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 50_010,
    });
    tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Timeout,
        ts_ms: 51_000,
    });
    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 51_020,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 51_030,
    });
    let fresh_dc = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Remote,
        ts_ms: 51_100,
    });
    assert!(fresh_dc.iter().any(|a| matches!(
        a,
        LifecycleAction::StreamDisconnected {
            mac,
            reason: StreamDisconnectReason::Remote,
            ts_ms: 51_100,
        } if mac == &w.mac
    )));
}

#[test]
fn authentication_alert_and_fallback_other_reason_reconcile_across_signal_orders() {
    let w = make_wearable("AA:BB:CC:DD:EE:52", "Strap");
    let mut tracker = StreamTracker::new();
    tracker.add_enrolled(w.clone());

    // 1. Authentication arrives BEFORE Connected = false
    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 1_000,
    });
    let auth_first = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Authentication,
        ts_ms: 2_000,
    });
    assert_eq!(
        auth_first
            .iter()
            .filter(
                |a| matches!(a, LifecycleAction::AlertActionRequired { mac, .. } if mac == &w.mac)
            )
            .count(),
        1
    );
    assert!(auth_first.iter().any(|a| matches!(
        a,
        LifecycleAction::StreamDisconnected {
            mac,
            reason: StreamDisconnectReason::Timeout,
            ts_ms: 2_000,
        } if mac == &w.mac
    )));
    let trailing_timeout = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Timeout,
        ts_ms: 2_020,
    });
    assert!(trailing_timeout.is_empty());
    assert!(
        tracker
            .handle_event(StreamEvent::Tick { ts_ms: 18_000 })
            .is_empty()
    );

    // 2. Connected = false (Timeout) arrives BEFORE Authentication
    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 20_000,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 20_500,
    });
    let timeout_first = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Timeout,
        ts_ms: 21_000,
    });
    assert!(timeout_first.iter().any(|a| matches!(
        a,
        LifecycleAction::StreamDisconnected {
            reason: StreamDisconnectReason::Timeout,
            ..
        }
    )));
    let auth_followup = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Authentication,
        ts_ms: 21_040,
    });
    assert_eq!(
        auth_followup
            .iter()
            .filter(
                |a| matches!(a, LifecycleAction::AlertActionRequired { mac, .. } if mac == &w.mac)
            )
            .count(),
        1,
        "Authentication shortly after Connected=false must emit AlertActionRequired: {auth_followup:?}"
    );
    assert!(
        tracker
            .handle_event(StreamEvent::Tick { ts_ms: 37_000 })
            .is_empty(),
        "Authentication follow-up must cancel pending RF NotifyDisconnected debounce"
    );

    // 3. Authentication while disconnected (e.g. failed background reconnect) still alerts once
    let auth_disconnected = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Authentication,
        ts_ms: 45_000,
    });
    assert_eq!(
        auth_disconnected
            .iter()
            .filter(
                |a| matches!(a, LifecycleAction::AlertActionRequired { mac, .. } if mac == &w.mac)
            )
            .count(),
        1
    );
    let auth_dup = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Authentication,
        ts_ms: 45_050,
    });
    assert!(auth_dup.is_empty());

    // 4. Fallback reason (e.g. org.bluez.Reason.Unknown) before Connected=false falls back to Timeout + 15s debounce
    tracker.handle_event(StreamEvent::Connected {
        mac: w.mac.clone(),
        ts_ms: 60_000,
    });
    tracker.handle_event(StreamEvent::NotifyStarted {
        mac: w.mac.clone(),
        ts_ms: 60_500,
    });
    let other_first = tracker.handle_event(StreamEvent::Disconnected {
        mac: w.mac.clone(),
        reason: DisconnectReason::Other("org.bluez.Reason.Unknown".to_string()),
        ts_ms: 61_000,
    });
    assert!(other_first.iter().any(|a| matches!(
        a,
        LifecycleAction::StreamDisconnected {
            reason: StreamDisconnectReason::Timeout,
            ts_ms: 61_000,
            ..
        }
    )));
    assert!(
        !other_first
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { .. }))
    );
    assert!(
        tracker
            .handle_event(StreamEvent::Disconnected {
                mac: w.mac.clone(),
                reason: DisconnectReason::Timeout,
                ts_ms: 61_030,
            })
            .is_empty()
    );
    let after_debounce = tracker.handle_event(StreamEvent::Tick { ts_ms: 76_000 });
    assert!(
        after_debounce
            .iter()
            .any(|a| matches!(a, LifecycleAction::NotifyDisconnected { mac, .. } if mac == &w.mac)),
        "fallback Other reason must preserve 15s RF debounce: {after_debounce:?}"
    );
}
