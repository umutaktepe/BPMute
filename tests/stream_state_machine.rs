use bpmute::bluetooth::stream::{DisconnectReason, LifecycleAction, StreamEvent, StreamTracker};
use bpmute::sleep_engine::{ContactStatus, HrPacket, StreamStatus};
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
