use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::actions::traits::BoxFuture;
use crate::bluetooth::agent::{AgentAuthRequest, ENROLLMENT_AGENT_PATH, EnrollmentAgent};
use crate::bluetooth::dbus::AgentManager1Proxy;
use crate::bluetooth::operations::{
    build_bluez_signal_stream, is_relevant_wearable_props_changed, is_wearable_interfaces_added,
    list_heart_rate_devices_for_adapters, next_stream_msg, resolve_target_adapters,
    start_le_discovery, stop_le_discovery,
};
use crate::ipc::protocol::{
    AlertSeverity, DaemonAlert, DiscoveredWearable, EnrollAuthResponseParams,
    EnrollmentUpdateEvent, ErrorPayload, PairingPrompt, alert_ids, error_codes,
};
use crate::logging::mask_mac;

use super::{DaemonCore, EnrollmentLock, InnerState, ipc_err, upsert_alert};

/// Narrow async boundary for BLE bonding and interactive `Agent1` pairing during enrollment.
pub trait EnrollmentPairingHook: Send + Sync {
    /// Sets device trust and optionally executes BLE SMP pairing bridged through `DaemonCore`.
    fn bond_and_pair<'a>(
        &'a self,
        core: &'a Arc<DaemonCore>,
        mac: &'a str,
        pair: bool,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// No-op [`EnrollmentPairingHook`] used in hermetic tests that do not inject a pairing harness.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEnrollmentDriver;

impl EnrollmentPairingHook for NoopEnrollmentDriver {
    fn bond_and_pair<'a>(
        &'a self,
        _core: &'a Arc<DaemonCore>,
        _mac: &'a str,
        _pair: bool,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

/// Production BlueZ [`EnrollmentPairingHook`] registering a temporary enrollment-scoped `Agent1`.
#[derive(Debug, Default, Clone, Copy)]
pub struct BluezEnrollmentDriver;

impl EnrollmentPairingHook for BluezEnrollmentDriver {
    fn bond_and_pair<'a>(
        &'a self,
        core: &'a Arc<DaemonCore>,
        mac: &'a str,
        pair: bool,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let Some(conn) = try_system_conn().await else {
                return Ok(());
            };

            if !pair {
                let _ =
                    crate::bluetooth::operations::bond_and_trust_device(&conn, mac, false).await;
                return Ok(());
            }

            let (auth_tx, mut auth_rx) = mpsc::channel::<AgentAuthRequest>(8);
            let agent = EnrollmentAgent::new(auth_tx);
            let agent_path = zbus::zvariant::ObjectPath::try_from(ENROLLMENT_AGENT_PATH)
                .map_err(|e| format!("invalid agent path: {e}"))?;

            let registered_obj = conn
                .object_server()
                .at(agent_path.clone(), agent)
                .await
                .is_ok();
            let agent_mgr = AgentManager1Proxy::new(&conn).await.ok();
            let mut registered_mgr = false;
            if let Some(ref mgr) = agent_mgr
                && mgr
                    .register_agent(agent_path.as_ref(), "KeyboardDisplay")
                    .await
                    .is_ok()
            {
                registered_mgr = true;
            }

            let core_bridge = Arc::clone(core);
            let bridge_task = tokio::spawn(async move {
                while let Some(req) = auth_rx.recv().await {
                    core_bridge.bridge_agent_auth_request(req).await;
                }
            });

            let res = crate::bluetooth::operations::bond_and_trust_device(&conn, mac, true)
                .await
                .map_err(|e| format!("Bluetooth pairing failed: {e}"));

            bridge_task.abort();
            let _ = bridge_task.await;
            if registered_mgr && let Some(ref mgr) = agent_mgr {
                let _ = mgr.unregister_agent(agent_path.as_ref()).await;
            }
            if registered_obj {
                let _ = conn
                    .object_server()
                    .remove::<EnrollmentAgent, _>(agent_path)
                    .await;
            }

            res
        })
    }
}

pub(super) async fn try_system_conn() -> Option<zbus::Connection> {
    zbus::connection::Builder::system().ok()?.build().await.ok()
}

impl DaemonCore {
    /// Stops any active background BlueZ enrollment discovery task and awaits cleanup.
    pub(super) async fn stop_active_discovery(&self) {
        {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            if let Some(cancel_tx) = guard.enrollment_cancel_tx.take() {
                let _ = cancel_tx.send(true);
            }
        }
        let handle = {
            let mut guard = self.active_discovery.lock().await;
            guard.take()
        };
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Acquires the enrollment lock and runs live BlueZ LE discovery with pushed `EnrollmentUpdate` events.
    pub(super) async fn start_enrollment_scan(
        self: &Arc<Self>,
        client_id: u64,
        timeout_sec: u32,
    ) -> Result<Vec<DiscoveredWearable>, ErrorPayload> {
        let now_ms = self.clock.now_utc().timestamp_millis();
        let timeout_ms = (timeout_sec as i64) * 1000;

        let (seq, pinned_adapter) = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            if let Some(ref lock) = guard.enrollment {
                let expired = now_ms >= lock.started_at_ms + lock.timeout_ms;
                if !expired && lock.client_id != client_id {
                    return Err(ipc_err(
                        error_codes::ENROLLMENT_IN_PROGRESS,
                        "another enrollment session is currently in progress",
                    ));
                }
            }
            guard.enrollment_seq = guard.enrollment_seq.wrapping_add(1);
            let seq = guard.enrollment_seq;
            guard.enrollment = Some(EnrollmentLock {
                client_id,
                started_at_ms: now_ms,
                timeout_ms,
            });
            let pinned_adapter = guard.loaded_config.config.bluetooth.adapter.clone();
            (seq, pinned_adapter)
        };

        self.stop_active_discovery().await;

        let Some(conn) = try_system_conn().await else {
            let guard = self.inner.lock().expect("daemon state mutex poisoned");
            return Ok(guard.discovered_wearables.clone());
        };

        let started_at_instant = self.clock.now_instant();
        let target_adapters = match resolve_target_adapters(&conn, pinned_adapter.as_deref()).await
        {
            Ok(resolved) if !resolved.targets.is_empty() => resolved.targets,
            Ok(resolved) => {
                self.release_enrollment_if_seq(seq);
                let msg = if resolved.any_present {
                    "no powered Bluetooth adapter found in BlueZ (turn Bluetooth on)"
                } else {
                    "no Bluetooth adapter found in BlueZ"
                };
                return Err(ipc_err(error_codes::BACKEND_REFUSED, msg));
            }
            Err(err) => {
                self.release_enrollment_if_seq(seq);
                return Err(ipc_err(
                    error_codes::BACKEND_REFUSED,
                    format!("Bluetooth discovery unavailable: {err}"),
                ));
            }
        };

        // Subscribe to ObjectManager and Properties signals before StartDiscovery so early advertisements are never missed.
        let iface_added_stream = build_bluez_signal_stream(
            &conn,
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
        )
        .await;
        let props_stream = build_bluez_signal_stream(
            &conn,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .await;

        let mut started_adapters = Vec::new();
        let mut last_err = None;
        for adapter in &target_adapters {
            match start_le_discovery(&conn, adapter).await {
                Ok(()) => started_adapters.push(adapter.clone()),
                Err(err) => {
                    tracing::warn!(adapter = %adapter, error = %err, "BlueZ Adapter1.StartDiscovery failed");
                    last_err = Some(err);
                }
            }
        }

        if started_adapters.is_empty() {
            self.release_enrollment_if_seq(seq);
            let msg = last_err
                .map(|e| format!("failed to start Bluetooth LE discovery: {e}"))
                .unwrap_or_else(|| "failed to start Bluetooth LE discovery".to_string());
            return Err(ipc_err(error_codes::BACKEND_REFUSED, msg));
        }

        let initial_wearables = match self
            .query_discovered_wearables(&conn, &started_adapters)
            .await
        {
            Ok(wearables) => wearables,
            Err(err) => {
                tracing::warn!(error = %err, "failed to list initial BlueZ 0x180D wearables");
                Vec::new()
            }
        };

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let still_active = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            if guard.enrollment_seq != seq {
                false
            } else {
                guard.discovered_wearables = initial_wearables.clone();
                guard.enrollment_cancel_tx = Some(cancel_tx);
                true
            }
        };
        if !still_active {
            for adapter in &started_adapters {
                if let Err(err) = stop_le_discovery(&conn, adapter).await {
                    tracing::warn!(adapter = %adapter, error = %err, "failed to stop BlueZ LE discovery after cancellation");
                }
            }
            return Err(ipc_err(
                error_codes::PRECONDITION_FAILED,
                "enrollment session was cancelled",
            ));
        }

        let args = DiscoveryWatcherArgs {
            core: Arc::clone(self),
            conn,
            started_adapters,
            iface_added_stream,
            props_stream,
            cancel_rx,
            shutdown_rx: self.shutdown_rx.clone(),
            seq,
            timeout_sec,
            started_at_instant,
        };
        let task = tokio::spawn(async move {
            run_discovery_watcher(args).await;
        });
        {
            let mut active = self.active_discovery.lock().await;
            *active = Some(task);
        }

        Ok(initial_wearables)
    }

    fn release_enrollment_if_seq(&self, seq: u64) {
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        if guard.enrollment_seq == seq {
            guard.enrollment = None;
            guard.enrollment_cancel_tx = None;
        }
    }

    /// Cancels active enrollment state and signals any running BlueZ discovery task to stop.
    pub(super) fn cancel_enrollment_locked(guard: &mut InnerState) {
        guard.enrollment_seq = guard.enrollment_seq.wrapping_add(1);
        if let Some(cancel_tx) = guard.enrollment_cancel_tx.take() {
            let _ = cancel_tx.send(true);
        }
        guard.enrollment = None;
        guard.discovered_wearables.clear();
        guard.active_auth_reply = None;
    }

    pub(super) async fn query_discovered_wearables(
        &self,
        conn: &zbus::Connection,
        adapter_paths: &[String],
    ) -> zbus::Result<Vec<DiscoveredWearable>> {
        let mut discovered = list_heart_rate_devices_for_adapters(conn, adapter_paths).await?;
        let enrolled_macs: HashSet<String> = self
            .persistence
            .list_wearables()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "failed to query enrolled wearables for discovery flags");
            })
            .unwrap_or_default()
            .into_iter()
            .map(|w| w.mac.to_ascii_uppercase())
            .collect();
        for wearable in &mut discovered {
            wearable.already_enrolled =
                enrolled_macs.contains(&wearable.address.to_ascii_uppercase());
        }
        Ok(discovered)
    }

    /// Automatically cancels any active enrollment owned by `client_id` when the socket disconnects.
    pub fn on_client_disconnected(&self, client_id: u64) {
        let released = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            if matches!(guard.enrollment, Some(ref lock) if lock.client_id == client_id) {
                tracing::info!(
                    client_id,
                    "enrollment auto-cancelled due to client socket disconnect"
                );
                Self::cancel_enrollment_locked(&mut guard);
                true
            } else {
                false
            }
        };
        if released {
            let event = EnrollmentUpdateEvent {
                wearables: Vec::new(),
                pairing_prompt: None,
                remaining_sec: Some(0),
            };
            if let Ok(val) = serde_json::to_value(event) {
                self.broker.publish_control("EnrollmentUpdate", val);
            }
        }
    }

    /// Triggers an interactive Agent1 pairing prompt pushed over IPC for BlueZ/test bridging.
    pub fn trigger_pairing_prompt(
        &self,
        prompt: PairingPrompt,
    ) -> tokio::sync::oneshot::Receiver<EnrollAuthResponseParams> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let wearables = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            guard.active_auth_reply = Some(tx);
            guard.discovered_wearables.clone()
        };
        let event = EnrollmentUpdateEvent {
            wearables,
            pairing_prompt: Some(prompt),
            remaining_sec: Some(30),
        };
        if let Ok(v) = serde_json::to_value(event) {
            self.broker.publish_control("EnrollmentUpdate", v);
        }
        rx
    }

    /// Bridges an incoming `AgentAuthRequest` to the enrolling client over IPC and forwards its response.
    pub async fn bridge_agent_auth_request(&self, req: AgentAuthRequest) {
        const PAIRING_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

        match req {
            AgentAuthRequest::Confirmation {
                device,
                passkey,
                reply,
            } => {
                let wearable = self.resolve_wearable_label(&device);
                let rx =
                    self.trigger_pairing_prompt(PairingPrompt::Confirmation { passkey, wearable });
                let confirmed = match tokio::time::timeout(PAIRING_REPLY_TIMEOUT, rx).await {
                    Ok(Ok(r)) => r.confirmed.unwrap_or(false),
                    _ => {
                        self.clear_active_auth_reply();
                        false
                    }
                };
                let _ = reply.send(confirmed);
            }
            AgentAuthRequest::Passkey { device, reply } => {
                let wearable = self.resolve_wearable_label(&device);
                let rx = self.trigger_pairing_prompt(PairingPrompt::Passkey { wearable });
                let passkey = match tokio::time::timeout(PAIRING_REPLY_TIMEOUT, rx).await {
                    Ok(Ok(r)) if r.confirmed != Some(false) => r.passkey,
                    _ => {
                        self.clear_active_auth_reply();
                        None
                    }
                };
                let _ = reply.send(passkey);
            }
            AgentAuthRequest::PinCode { device, reply } => {
                let wearable = self.resolve_wearable_label(&device);
                let rx = self.trigger_pairing_prompt(PairingPrompt::PinCode { wearable });
                let pincode = match tokio::time::timeout(PAIRING_REPLY_TIMEOUT, rx).await {
                    Ok(Ok(r)) if r.confirmed != Some(false) => r.pincode.filter(|s| !s.is_empty()),
                    _ => {
                        self.clear_active_auth_reply();
                        None
                    }
                };
                let _ = reply.send(pincode);
            }
            AgentAuthRequest::Authorization { reply, .. } => {
                let _ = reply.send(true);
            }
        }
    }

    fn clear_active_auth_reply(&self) {
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        guard.active_auth_reply = None;
    }

    /// Upserts or clears the dismissible `logind` inhibitor refusal alert and broadcasts `AlertChanged`.
    pub fn set_logind_inhibited_alert(&self, reason: Option<&str>) {
        let alerts_opt = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            let changed = match reason {
                Some(r) => upsert_alert(
                    &mut guard.alerts,
                    DaemonAlert {
                        id: alert_ids::LOGIND_INHIBITED.to_string(),
                        kind: alert_ids::LOGIND_INHIBITED.to_string(),
                        severity: AlertSeverity::Warn,
                        message: format!(
                            "Sleep action blocked by logind inhibitor ({r}); media remains paused."
                        ),
                        dismissible: true,
                    },
                ),
                None => {
                    let before = guard.alerts.len();
                    guard.alerts.retain(|a| a.id != alert_ids::LOGIND_INHIBITED);
                    guard.alerts.len() != before
                }
            };
            changed.then(|| guard.alerts.clone())
        };
        if let Some(alerts) = alerts_opt {
            self.broker
                .publish_control("AlertChanged", json!({ "alerts": alerts }));
        }
    }

    fn resolve_wearable_label(&self, path_or_mac: &str) -> String {
        let mac = if let Some(pos) = path_or_mac.rfind("/dev_") {
            path_or_mac[pos + 5..].replace('_', ":")
        } else {
            path_or_mac.to_string()
        };
        let guard = self.inner.lock().expect("daemon state mutex poisoned");
        if let Some(found) = guard
            .discovered_wearables
            .iter()
            .find(|w| w.address.eq_ignore_ascii_case(&mac))
            && let Some(ref name) = found.name
        {
            return name.clone();
        }
        mask_mac(&mac)
    }
}

struct DiscoveryWatcherArgs {
    core: Arc<DaemonCore>,
    conn: zbus::Connection,
    started_adapters: Vec<String>,
    iface_added_stream: Option<zbus::MessageStream>,
    props_stream: Option<zbus::MessageStream>,
    cancel_rx: watch::Receiver<bool>,
    shutdown_rx: watch::Receiver<bool>,
    seq: u64,
    timeout_sec: u32,
    started_at_instant: tokio::time::Instant,
}

async fn run_discovery_watcher(args: DiscoveryWatcherArgs) {
    let DiscoveryWatcherArgs {
        core,
        conn,
        started_adapters,
        mut iface_added_stream,
        mut props_stream,
        mut cancel_rx,
        mut shutdown_rx,
        seq,
        timeout_sec,
        started_at_instant,
    } = args;
    let mut tick_interval = tokio::time::interval(Duration::from_secs(1));
    // First interval tick resolves immediately; skip it since initial snapshot was just sent.
    tick_interval.tick().await;

    let mut timed_out = false;
    loop {
        if *cancel_rx.borrow() || *shutdown_rx.borrow() {
            break;
        }
        let elapsed_sec = core
            .clock
            .now_instant()
            .saturating_duration_since(started_at_instant)
            .as_secs() as u32;
        let rem_sec = timeout_sec.saturating_sub(elapsed_sec);

        tokio::select! {
            _ = tick_interval.tick() => {
                let elapsed = core
                    .clock
                    .now_instant()
                    .saturating_duration_since(started_at_instant)
                    .as_secs() as u32;
                let next_rem = timeout_sec.saturating_sub(elapsed);
                refresh_and_publish_enrollment(&core, &conn, &started_adapters, seq, next_rem).await;
                if next_rem == 0 {
                    timed_out = true;
                    break;
                }
            }
            msg_opt = next_stream_msg(&mut iface_added_stream) => {
                match msg_opt {
                    Some(Ok(msg)) if is_wearable_interfaces_added(&msg) => {
                        refresh_and_publish_enrollment(&core, &conn, &started_adapters, seq, rem_sec).await;
                    }
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "BlueZ InterfacesAdded stream error during enrollment");
                    }
                    Some(Ok(_)) => {}
                    None => {
                        tracing::debug!("BlueZ InterfacesAdded stream closed during enrollment");
                        iface_added_stream = None;
                    }
                }
            }
            msg_opt = next_stream_msg(&mut props_stream) => {
                match msg_opt {
                    Some(Ok(msg)) if is_relevant_wearable_props_changed(&msg) => {
                        refresh_and_publish_enrollment(&core, &conn, &started_adapters, seq, rem_sec).await;
                    }
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "BlueZ PropertiesChanged stream error during enrollment");
                    }
                    Some(Ok(_)) => {}
                    None => {
                        tracing::debug!("BlueZ PropertiesChanged stream closed during enrollment");
                        props_stream = None;
                    }
                }
            }
            res = cancel_rx.changed() => {
                if res.is_err() || *cancel_rx.borrow() {
                    break;
                }
            }
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    for adapter in &started_adapters {
        if let Err(err) = stop_le_discovery(&conn, adapter).await {
            tracing::warn!(adapter = %adapter, error = %err, "failed to stop BlueZ LE discovery");
        }
    }

    if timed_out {
        let wearables = {
            let mut guard = core.inner.lock().expect("daemon state mutex poisoned");
            if guard.enrollment_seq == seq {
                guard.enrollment_cancel_tx = None;
                Some(guard.discovered_wearables.clone())
            } else {
                None
            }
        };
        if let Some(wearables) = wearables {
            let event = EnrollmentUpdateEvent {
                wearables,
                pairing_prompt: None,
                remaining_sec: Some(0),
            };
            if let Ok(val) = serde_json::to_value(event) {
                core.broker.publish_control("EnrollmentUpdate", val);
            }
        }
    }
}

async fn refresh_and_publish_enrollment(
    core: &Arc<DaemonCore>,
    conn: &zbus::Connection,
    started_adapters: &[String],
    seq: u64,
    remaining_sec: u32,
) {
    let wearables = match core
        .query_discovered_wearables(conn, started_adapters)
        .await
    {
        Ok(wearables) => wearables,
        Err(err) => {
            tracing::warn!(error = %err, "failed to refresh discovered BlueZ 0x180D wearables");
            return;
        }
    };
    let changed = {
        let mut guard = core.inner.lock().expect("daemon state mutex poisoned");
        if guard.enrollment_seq != seq || guard.discovered_wearables == wearables {
            false
        } else {
            guard.discovered_wearables = wearables.clone();
            true
        }
    };
    if changed {
        let event = EnrollmentUpdateEvent {
            wearables,
            pairing_prompt: None,
            remaining_sec: Some(remaining_sec),
        };
        if let Ok(val) = serde_json::to_value(event) {
            core.broker.publish_control("EnrollmentUpdate", val);
        }
    }
}
