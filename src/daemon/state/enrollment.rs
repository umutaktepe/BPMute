use std::sync::Arc;

use serde_json::json;
use tokio::sync::mpsc;

use crate::actions::traits::BoxFuture;
use crate::bluetooth::agent::{AgentAuthRequest, ENROLLMENT_AGENT_PATH, EnrollmentAgent};
use crate::bluetooth::dbus::AgentManager1Proxy;
use crate::ipc::protocol::{
    AlertSeverity, DaemonAlert, EnrollAuthResponseParams, EnrollmentUpdateEvent, PairingPrompt,
    alert_ids,
};
use crate::logging::mask_mac;

use super::{DaemonCore, upsert_alert};

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
            let _ = crate::bluetooth::operations::stop_le_discovery(&conn, "/org/bluez/hci0").await;

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
    /// Automatically cancels any active enrollment owned by `client_id` when the socket disconnects.
    pub fn on_client_disconnected(&self, client_id: u64) {
        let released = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            if matches!(guard.enrollment, Some(ref lock) if lock.client_id == client_id) {
                tracing::info!(
                    client_id,
                    "enrollment auto-cancelled due to client socket disconnect"
                );
                guard.enrollment = None;
                guard.discovered_wearables.clear();
                guard.active_auth_reply = None;
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
        match req {
            AgentAuthRequest::Confirmation {
                device,
                passkey,
                reply,
            } => {
                let wearable = self.resolve_wearable_label(&device);
                let rx =
                    self.trigger_pairing_prompt(PairingPrompt::Confirmation { passkey, wearable });
                let confirmed = rx.await.ok().and_then(|r| r.confirmed).unwrap_or(false);
                let _ = reply.send(confirmed);
            }
            AgentAuthRequest::Passkey { device, reply } => {
                let wearable = self.resolve_wearable_label(&device);
                let rx = self.trigger_pairing_prompt(PairingPrompt::Passkey { wearable });
                let passkey = match rx.await {
                    Ok(r) if r.confirmed != Some(false) => r.passkey,
                    _ => None,
                };
                let _ = reply.send(passkey);
            }
            AgentAuthRequest::PinCode { device, reply } => {
                let wearable = self.resolve_wearable_label(&device);
                let rx = self.trigger_pairing_prompt(PairingPrompt::PinCode { wearable });
                let pincode = match rx.await {
                    Ok(r) if r.confirmed != Some(false) => r.pincode.filter(|s| !s.is_empty()),
                    _ => None,
                };
                let _ = reply.send(pincode);
            }
            AgentAuthRequest::Authorization { reply, .. } => {
                let _ = reply.send(true);
            }
        }
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
