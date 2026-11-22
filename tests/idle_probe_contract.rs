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

#[tokio::test(start_paused = true)]
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

#[tokio::test(start_paused = true)]
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

mod fake_wayland {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::{mpsc, oneshot};

    #[derive(Debug, Clone, Copy)]
    pub struct NotificationCreated {
        pub opcode: u16,
        pub timeout_ms: u32,
    }

    enum ServerCmd {
        SendIdled(oneshot::Sender<()>),
        SendResumed(oneshot::Sender<()>),
        DropClient(oneshot::Sender<()>),
        Shutdown,
    }

    pub struct FakeWaylandCompositor {
        cmd_tx: mpsc::UnboundedSender<ServerCmd>,
        pub created_rx: mpsc::UnboundedReceiver<NotificationCreated>,
        pub notifications: Arc<Mutex<Vec<NotificationCreated>>>,
        thread: Option<JoinHandle<()>>,
    }

    impl FakeWaylandCompositor {
        pub fn start(socket_path: &Path, ext_idle_version: u32) -> Self {
            let std_listener = std::os::unix::net::UnixListener::bind(socket_path)
                .expect("bind fake wayland socket");
            std_listener
                .set_nonblocking(true)
                .expect("set_nonblocking on fake wayland listener");
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ServerCmd>();
            let (created_tx, created_rx) = mpsc::unbounded_channel::<NotificationCreated>();
            let notifications = Arc::new(Mutex::new(Vec::new()));
            let notifications_bg = Arc::clone(&notifications);

            let thread = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build fake wayland runtime");
                rt.block_on(async move {
                    let listener =
                        UnixListener::from_std(std_listener).expect("wrap fake wayland listener");
                    let mut active_stream: Option<UnixStream> = None;
                    let mut active_notif_id: Option<u32> = None;
                    let mut registry_id: Option<u32> = None;
                    let mut notifier_id: Option<u32> = None;
                    let mut read_buf = Vec::<u8>::new();

                    loop {
                        if active_stream.is_none() {
                            tokio::select! {
                                res = listener.accept() => {
                                    match res {
                                        Ok((stream, _)) => {
                                            active_stream = Some(stream);
                                            active_notif_id = None;
                                            registry_id = None;
                                            notifier_id = None;
                                            read_buf.clear();
                                        }
                                        Err(_) => break,
                                    }
                                }
                                cmd = cmd_rx.recv() => {
                                    if matches!(cmd, None | Some(ServerCmd::Shutdown)) {
                                        break;
                                    }
                                }
                            }
                            continue;
                        }

                        let stream = active_stream.as_mut().expect("active stream");
                        let mut tmp = [0u8; 4096];
                        tokio::select! {
                            cmd = cmd_rx.recv() => {
                                match cmd {
                                    None | Some(ServerCmd::Shutdown) => break,
                                    Some(ServerCmd::DropClient(ack)) => {
                                        active_stream = None;
                                        active_notif_id = None;
                                        let _ = ack.send(());
                                    }
                                    Some(ServerCmd::SendIdled(ack)) => {
                                        if let Some(nid) = active_notif_id {
                                            let pkt = encode_msg(nid, 0, &[]);
                                            if stream.write_all(&pkt).await.is_err() {
                                                active_stream = None;
                                            }
                                        }
                                        let _ = ack.send(());
                                    }
                                    Some(ServerCmd::SendResumed(ack)) => {
                                        if let Some(nid) = active_notif_id {
                                            let pkt = encode_msg(nid, 1, &[]);
                                            if stream.write_all(&pkt).await.is_err() {
                                                active_stream = None;
                                            }
                                        }
                                        let _ = ack.send(());
                                    }
                                }
                            }
                            read_res = stream.read(&mut tmp) => {
                                match read_res {
                                    Ok(0) | Err(_) => {
                                        active_stream = None;
                                        active_notif_id = None;
                                    }
                                    Ok(n) => {
                                        read_buf.extend_from_slice(&tmp[..n]);
                                        while read_buf.len() >= 8 {
                                            let obj_id = u32::from_ne_bytes(read_buf[0..4].try_into().unwrap());
                                            let size_op = u32::from_ne_bytes(read_buf[4..8].try_into().unwrap());
                                            let msg_size = (size_op >> 16) as usize;
                                            let opcode = (size_op & 0xffff) as u16;
                                            if msg_size < 8 || read_buf.len() < msg_size {
                                                break;
                                            }
                                            let payload = read_buf[8..msg_size].to_vec();
                                            read_buf.drain(..msg_size);

                                            if obj_id == 1 {
                                                if opcode == 0 && payload.len() >= 4 {
                                                    // wl_display::sync(callback)
                                                    let cb_id = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
                                                    let mut out = encode_msg(cb_id, 0, &0u32.to_ne_bytes());
                                                    out.extend_from_slice(&encode_msg(1, 1, &cb_id.to_ne_bytes()));
                                                    let _ = stream.write_all(&out).await;
                                                } else if opcode == 1 && payload.len() >= 4 {
                                                    // wl_display::get_registry(registry)
                                                    let reg_id = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
                                                    registry_id = Some(reg_id);
                                                    let mut out = encode_global(reg_id, 1, "wl_seat", 1);
                                                    out.extend_from_slice(&encode_global(
                                                        reg_id,
                                                        2,
                                                        "ext_idle_notifier_v1",
                                                        ext_idle_version,
                                                    ));
                                                    let _ = stream.write_all(&out).await;
                                                }
                                            } else if Some(obj_id) == registry_id && opcode == 0 && payload.len() >= 12 {
                                                // wl_registry::bind(name, interface_str, version, id)
                                                let name = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
                                                let new_id = u32::from_ne_bytes(payload[payload.len() - 4..].try_into().unwrap());
                                                if name == 2 {
                                                    notifier_id = Some(new_id);
                                                }
                                            } else if Some(obj_id) == notifier_id && (opcode == 1 || opcode == 2) && payload.len() >= 8 {
                                                // ext_idle_notifier_v1: 0=destroy, 1=get_idle_notification, 2=get_input_idle_notification
                                                let notif_id = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
                                                let timeout_ms = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
                                                active_notif_id = Some(notif_id);
                                                let ev = NotificationCreated { opcode, timeout_ms };
                                                notifications_bg.lock().unwrap().push(ev);
                                                let _ = created_tx.send(ev);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
            });

            Self {
                cmd_tx,
                created_rx,
                notifications,
                thread: Some(thread),
            }
        }

        async fn send_cmd(&self, make_cmd: impl FnOnce(oneshot::Sender<()>) -> ServerCmd) {
            let (tx, rx) = oneshot::channel();
            let _ = self.cmd_tx.send(make_cmd(tx));
            let _ = rx.await;
        }

        pub async fn send_idled(&self) {
            self.send_cmd(ServerCmd::SendIdled).await;
        }

        pub async fn send_resumed(&self) {
            self.send_cmd(ServerCmd::SendResumed).await;
        }

        pub async fn drop_client(&self) {
            self.send_cmd(ServerCmd::DropClient).await;
        }
    }

    impl Drop for FakeWaylandCompositor {
        fn drop(&mut self) {
            let _ = self.cmd_tx.send(ServerCmd::Shutdown);
            if let Some(handle) = self.thread.take() {
                let _ = handle.join();
            }
        }
    }

    fn encode_msg(obj_id: u32, opcode: u16, payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let size_op = (size << 16) | u32::from(opcode);
        let mut buf = Vec::with_capacity(size as usize);
        buf.extend_from_slice(&obj_id.to_ne_bytes());
        buf.extend_from_slice(&size_op.to_ne_bytes());
        buf.extend_from_slice(payload);
        buf
    }

    fn encode_global(reg_id: u32, name: u32, iface: &str, version: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&name.to_ne_bytes());
        let len_with_nul = (iface.len() + 1) as u32;
        payload.extend_from_slice(&len_with_nul.to_ne_bytes());
        payload.extend_from_slice(iface.as_bytes());
        payload.push(0);
        while !payload.len().is_multiple_of(4) {
            payload.push(0);
        }
        payload.extend_from_slice(&version.to_ne_bytes());
        encode_msg(reg_id, 0, &payload)
    }
}

fn setup_fake_wayland_probe(
    socket_name: &str,
    ext_idle_version: u32,
) -> (
    tempfile::TempDir,
    fake_wayland::FakeWaylandCompositor,
    bpmute::idle::SystemIdleProbe,
) {
    use bpmute::idle::SystemIdleProbe;
    use bpmute::paths::MapEnv;
    use std::sync::Arc;
    use tempfile::TempDir;

    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join(socket_name);
    let compositor = fake_wayland::FakeWaylandCompositor::start(&socket_path, ext_idle_version);
    let env = Arc::new(
        MapEnv::new()
            .with("WAYLAND_DISPLAY", socket_name)
            .with("XDG_RUNTIME_DIR", temp.path().to_string_lossy().as_ref())
            .with("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null"),
    );
    (temp, compositor, SystemIdleProbe::new(env))
}

async fn flush_wayland_io() {
    tokio::time::sleep(Duration::from_millis(1)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn test_wayland_ext_idle_v2_pre_gate_and_grace_activity() {
    use bpmute::idle::traits::{IdleBackendKind, IdleProbe};

    let (_temp, mut compositor, probe) = setup_fake_wayland_probe("wayland-test", 2);

    assert_eq!(
        probe.resolve_backend().await,
        IdleBackendKind::WaylandExtIdleV2
    );

    // Persistent monitor should have created one notification object and kept it open.
    let created = compositor
        .created_rx
        .recv()
        .await
        .expect("compositor channel closed");
    assert_eq!(created.opcode, 2); // get_input_idle_notification (v2)
    assert_eq!(created.timeout_ms, 1000);

    // Before `idled` fires, user is active.
    assert!(
        probe
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap()
    );

    // Compositor signals `idled`; after 118s (total idle 119s < 120s), still active in trailing window.
    compositor.send_idled().await;
    flush_wayland_io().await;
    tokio::time::advance(Duration::from_secs(118)).await;
    assert!(
        probe
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap(),
        "total idle 119s must still report active within 120s trailing window"
    );

    // Advance 2s more (total idle ~121s >= 120s) -> pre-gate must pass (inactive).
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        !probe
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap(),
        "user idle for >120s must not be reported active by pre-gate"
    );

    // Grace period activity watcher with 1s settle delay:
    // Resumed within 200ms (before 1s settle delay) must be ignored by grace_rx,
    // while immediately flipping trailing activity back to true.
    let mut grace_rx = probe
        .watch_grace_period_activity(Duration::from_secs(1))
        .await
        .unwrap();
    tokio::time::advance(Duration::from_millis(200)).await;
    compositor.send_resumed().await;
    flush_wayland_io().await;
    assert!(grace_rx.try_recv().is_err());
    assert!(
        probe
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap()
    );

    // Re-idle, advance past the 1s settle delay, then resume -> grace_rx delivers activity.
    compositor.send_idled().await;
    flush_wayland_io().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    compositor.send_resumed().await;
    flush_wayland_io().await;
    assert_eq!(grace_rx.recv().await, Some(()));
    assert!(
        probe
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap()
    );

    let recorded = compositor.notifications.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].opcode, 2);
    assert_eq!(recorded[0].timeout_ms, 1000);
}

#[tokio::test(start_paused = true)]
async fn test_wayland_ext_idle_v1_pre_gate_uses_v1_request_and_reports_idle() {
    use bpmute::idle::traits::{IdleBackendKind, IdleProbe};

    let (_temp, mut compositor, probe) = setup_fake_wayland_probe("wayland-v1", 1);

    assert_eq!(
        probe.resolve_backend().await,
        IdleBackendKind::WaylandExtIdleV1
    );

    let created = compositor
        .created_rx
        .recv()
        .await
        .expect("compositor channel closed");
    assert_eq!(created.opcode, 1); // get_idle_notification (v1)
    assert_eq!(created.timeout_ms, 1000);

    compositor.send_idled().await;
    flush_wayland_io().await;
    tokio::time::advance(Duration::from_secs(120)).await;

    assert!(
        !probe
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn test_wayland_monitor_reconnects_after_compositor_disconnect() {
    use bpmute::idle::traits::{IdleBackendKind, IdleProbe};

    let (_temp, mut compositor, probe) = setup_fake_wayland_probe("wayland-reconnect", 2);

    assert_eq!(
        probe.resolve_backend().await,
        IdleBackendKind::WaylandExtIdleV2
    );
    let first = compositor.created_rx.recv().await.unwrap();
    assert_eq!(first.opcode, 2);

    compositor.send_idled().await;
    flush_wayland_io().await;

    // Simulate compositor restart/drop; let background task observe EOF.
    compositor.drop_client().await;
    flush_wayland_io().await;

    // Query triggers reconnect on the persistent monitor, returning `true` before `idled`.
    assert!(
        probe
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap()
    );
    let second = compositor.created_rx.recv().await.unwrap();
    assert_eq!(second.opcode, 2);

    // Once the reconnected monitor receives `idled` and 120s pass, pre-gate reports idle again.
    compositor.send_idled().await;
    flush_wayland_io().await;
    tokio::time::advance(Duration::from_secs(120)).await;
    assert!(
        !probe
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap()
    );
}
