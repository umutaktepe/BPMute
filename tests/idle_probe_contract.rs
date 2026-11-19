use std::time::Duration;

use bpmute::idle::jitter::{InputEvent, PointerJitterFilter};

#[test]
fn test_pointer_jitter_filtering_threshold_and_instant_keys() {
    let mut filter = PointerJitterFilter::new(25.0, Duration::from_secs(2));

    let t0 = 1_000_000; // ms

    // Small motion: 10 px at t0 -> below 25 px threshold -> no activity
    assert!(!filter.record_event(InputEvent::RawMotion { dx: 6.0, dy: 8.0 }, t0)); // sqrt(36+64) = 10 px

    // Another small motion: 10 px at t0 + 1000 ms -> cumulative 20 px -> still below 25 px
    assert!(!filter.record_event(InputEvent::RawMotion { dx: 0.0, dy: 10.0 }, t0 + 1000));

    // Motion at t0 + 2500 ms: previous t0 motion (10 px) expired (> 2 s old).
    // Remaining in window: 10 px (from t0+1000) + 10 px = 20 px -> still below 25 px
    assert!(!filter.record_event(InputEvent::RawMotion { dx: 10.0, dy: 0.0 }, t0 + 2500));

    // Deliberate motion at t0 + 2600 ms: 15 px added -> in window [t0+600, t0+2600] we have:
    // 10 px (at t0+1000) + 10 px (at t0+2500) + 15 px (at t0+2600) = 35 px >= 25 px -> triggers activity!
    assert!(filter.record_event(InputEvent::RawMotion { dx: 15.0, dy: 0.0 }, t0 + 2600));

    // Keypress triggers activity immediately regardless of pointer motion
    assert!(filter.record_event(InputEvent::RawKeyPress, t0 + 3000));

    // Button click triggers activity immediately regardless of pointer motion
    assert!(filter.record_event(InputEvent::RawButtonPress, t0 + 3500));
}

#[test]
fn test_probe_chain_priority_order_and_inhibitor_sensitivity() {
    use bpmute::idle::traits::{
        DisplayEnvironment, IdleBackendKind, SystemCapabilities, select_idle_backend,
    };

    // 1. Wayland ext-idle-notify-v1 v2 takes top priority when WAYLAND_DISPLAY is set
    let env_wayland = DisplayEnvironment {
        wayland_display: Some("wayland-0".to_string()),
        display: Some(":0".to_string()),
    };
    let caps_all = SystemCapabilities {
        wayland_ext_idle_version: Some(2),
        gnome_mutter_available: true,
        x11_available: true,
    };
    let backend1 = select_idle_backend(&env_wayland, &caps_all);
    assert_eq!(backend1, IdleBackendKind::WaylandExtIdleV2);
    assert!(!backend1.is_inhibitor_sensitive());
    assert!(!backend1.is_degraded());

    // 2. GNOME Mutter takes priority 2 when Wayland v2 is unavailable (e.g. GNOME Wayland)
    let caps_mutter_wayland = SystemCapabilities {
        wayland_ext_idle_version: None,
        gnome_mutter_available: true,
        x11_available: true,
    };
    let backend2 = select_idle_backend(&env_wayland, &caps_mutter_wayland);
    assert_eq!(backend2, IdleBackendKind::GnomeMutter);
    assert!(!backend2.is_inhibitor_sensitive());

    // 3. Wayland ext-idle-notify-v1 v1 takes priority 3 (flagged inhibitor-sensitive)
    let caps_v1_only = SystemCapabilities {
        wayland_ext_idle_version: Some(1),
        gnome_mutter_available: false,
        x11_available: true,
    };
    let backend3 = select_idle_backend(&env_wayland, &caps_v1_only);
    assert_eq!(backend3, IdleBackendKind::WaylandExtIdleV1);
    assert!(backend3.is_inhibitor_sensitive());

    // 4. Native X11 takes priority 4 when DISPLAY is set and WAYLAND_DISPLAY is unset
    let env_x11 = DisplayEnvironment {
        wayland_display: None,
        display: Some(":0".to_string()),
    };
    let caps_x11 = SystemCapabilities {
        wayland_ext_idle_version: None,
        gnome_mutter_available: false,
        x11_available: true,
    };
    let backend4 = select_idle_backend(&env_x11, &caps_x11);
    assert_eq!(backend4, IdleBackendKind::X11);
    assert!(!backend4.is_inhibitor_sensitive());

    // 5. Degraded fallback when no backend is available
    let caps_none = SystemCapabilities {
        wayland_ext_idle_version: None,
        gnome_mutter_available: false,
        x11_available: false,
    };
    let backend5 = select_idle_backend(&env_x11, &caps_none);
    assert_eq!(backend5, IdleBackendKind::Degraded);
    assert!(backend5.is_degraded());
}

#[test]
fn test_degraded_fallback_grace_floor_and_strict_idle_refusal() {
    use bpmute::config::PowerAction;
    use bpmute::idle::traits::{
        IdleBackendKind, compute_effective_grace_period, is_power_action_refused,
    };

    // Degraded backend with non-destructive action (pause_media only, power_action = None):
    // configured 30s is allowed normally, no 90s floor enforced.
    assert_eq!(
        compute_effective_grace_period(30, IdleBackendKind::Degraded, PowerAction::None),
        30
    );
    assert!(!is_power_action_refused(
        IdleBackendKind::Degraded,
        PowerAction::None,
        true
    ));

    // Degraded backend with destructive power actions (Suspend / Poweroff):
    // Enforces >= 90s safety floor!
    assert_eq!(
        compute_effective_grace_period(30, IdleBackendKind::Degraded, PowerAction::Suspend),
        90
    );
    assert_eq!(
        compute_effective_grace_period(60, IdleBackendKind::Degraded, PowerAction::Poweroff),
        90
    );
    // If configured >= 90s (e.g. 120s), it preserves the higher configured value
    assert_eq!(
        compute_effective_grace_period(120, IdleBackendKind::Degraded, PowerAction::Suspend),
        120
    );

    // Non-degraded backend does not enforce the 90s floor for Suspend
    assert_eq!(
        compute_effective_grace_period(30, IdleBackendKind::WaylandExtIdleV2, PowerAction::Suspend),
        30
    );

    // strict_idle_backend = true: refuses destructive power actions on Degraded backend
    assert!(is_power_action_refused(
        IdleBackendKind::Degraded,
        PowerAction::Suspend,
        true
    ));
    assert!(is_power_action_refused(
        IdleBackendKind::Degraded,
        PowerAction::Poweroff,
        true
    ));

    // strict_idle_backend = false: does not refuse on Degraded backend
    assert!(!is_power_action_refused(
        IdleBackendKind::Degraded,
        PowerAction::Suspend,
        false
    ));

    // strict_idle_backend = true on healthy backend: does not refuse
    assert!(!is_power_action_refused(
        IdleBackendKind::WaylandExtIdleV2,
        PowerAction::Suspend,
        true
    ));
}

#[test]
fn test_systemd_environment_lazy_display_discovery() {
    use bpmute::idle::traits::parse_systemd_environment;

    let env_list = vec![
        "HOME=/home/user".to_string(),
        "WAYLAND_DISPLAY=wayland-1".to_string(),
        "DISPLAY=:1".to_string(),
        "XDG_RUNTIME_DIR=/run/user/1000".to_string(),
    ];

    let discovered = parse_systemd_environment(&env_list);
    assert_eq!(discovered.wayland_display.as_deref(), Some("wayland-1"));
    assert_eq!(discovered.display.as_deref(), Some(":1"));

    let empty_list: Vec<String> = vec![];
    let empty_discovered = parse_systemd_environment(&empty_list);
    assert_eq!(empty_discovered.wayland_display, None);
    assert_eq!(empty_discovered.display, None);
}

#[tokio::test]
async fn test_fake_idle_probe_pre_gate_and_settle_delay() {
    use bpmute::idle::fake::FakeIdleProbe;
    use bpmute::idle::traits::{IdleBackendKind, IdleProbe};

    let fake = FakeIdleProbe::new(IdleBackendKind::WaylandExtIdleV2);

    // Default: no trailing activity -> pre-gate passes (user not active)
    assert!(
        !fake
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap()
    );

    // Inject trailing activity -> pre-gate reports active
    fake.set_user_active_trailing(true);
    assert!(
        fake.is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap()
    );
    fake.set_user_active_trailing(false);

    // Grace period activity with 1s settle delay
    let mut rx = fake
        .watch_grace_period_activity(Duration::from_millis(1000))
        .await
        .unwrap();

    // Trigger activity immediately (at 0ms, before 1000ms settle delay) -> dropped/ignored!
    fake.trigger_activity();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(rx.try_recv().is_err());

    // Advance virtual time past the 1s settle delay
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Trigger activity after settle delay -> delivered!
    fake.trigger_activity();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(rx.try_recv().is_ok());
}

#[tokio::test]
async fn test_system_idle_probe_degraded_fallback_headless() {
    use bpmute::idle::SystemIdleProbe;
    use bpmute::idle::traits::{IdleBackendKind, IdleProbe};
    use bpmute::paths::MapEnv;
    use std::sync::Arc;

    // Headless / non-graphical environment: no WAYLAND_DISPLAY, no DISPLAY, isolated D-Bus
    let env = Arc::new(MapEnv::new().with("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null"));
    let probe = SystemIdleProbe::new(env);

    // Initial backend kind before probe defaults to Degraded
    assert_eq!(probe.backend_kind(), IdleBackendKind::Degraded);

    // Trailing activity check returns Ok(false) on degraded
    let is_active = probe
        .is_user_active_trailing(Duration::from_secs(120))
        .await;
    assert!(is_active.is_ok());
    assert!(!is_active.unwrap());

    // Resolved backend is Degraded
    assert_eq!(probe.backend_kind(), IdleBackendKind::Degraded);

    // Grace period activity stream can be established without error
    let rx = probe
        .watch_grace_period_activity(Duration::from_secs(1))
        .await;
    assert!(rx.is_ok());

    // Post-trigger activity stream can be established without error and returns false on degraded
    let post_active = probe
        .watch_post_trigger_activity(Duration::from_millis(10))
        .await;
    assert!(!post_active);
}
