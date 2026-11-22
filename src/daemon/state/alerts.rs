use crate::calibration::CalibrationState;
use crate::config::PowerAction;
use crate::idle::IdleBackendKind;
use crate::ipc::protocol::{AlertSeverity, DaemonAlert, alert_ids};

pub(super) fn clear_reload_alert(alerts: &mut Vec<DaemonAlert>) -> bool {
    let before = alerts.len();
    alerts.retain(|a| a.id != alert_ids::CONFIG_RELOAD_FAILED);
    alerts.len() != before
}

pub(super) fn upsert_reload_alert(alerts: &mut Vec<DaemonAlert>, err_msg: &str) -> bool {
    let message = format!("Configuration reload failed (keeping last valid config): {err_msg}");
    upsert_alert(
        alerts,
        DaemonAlert {
            id: alert_ids::CONFIG_RELOAD_FAILED.to_string(),
            kind: alert_ids::CONFIG_RELOAD_FAILED.to_string(),
            severity: AlertSeverity::Error,
            message,
            dismissible: true,
        },
    )
}

pub(super) fn sync_shift_alert_locked(
    alerts: &mut Vec<DaemonAlert>,
    state: &CalibrationState,
) -> bool {
    if let Some(ref shift) = state.pending_shift_alert {
        let message = format!(
            "Persistent {} baseline shift detected across {} consecutive sessions; accept or dismiss via calibration controls",
            shift.direction, shift.session_count
        );
        upsert_alert(
            alerts,
            DaemonAlert {
                id: alert_ids::PERSISTENT_SHIFT.to_string(),
                kind: alert_ids::PERSISTENT_SHIFT.to_string(),
                severity: AlertSeverity::Warn,
                message,
                dismissible: true,
            },
        )
    } else {
        let before = alerts.len();
        alerts.retain(|a| a.id != alert_ids::PERSISTENT_SHIFT);
        alerts.len() != before
    }
}

pub(super) fn sync_idle_alerts_locked(
    alerts: &mut Vec<DaemonAlert>,
    backend: IdleBackendKind,
    power_action: PowerAction,
) -> bool {
    let mut changed = false;

    let needs_degraded = backend.is_degraded()
        && matches!(power_action, PowerAction::Suspend | PowerAction::Poweroff);
    if needs_degraded {
        changed |= upsert_alert(
            alerts,
            DaemonAlert {
                id: alert_ids::DEGRADED_IDLE.to_string(),
                kind: alert_ids::DEGRADED_IDLE.to_string(),
                severity: AlertSeverity::Warn,
                message: "No desktop idle backend available. Destructive power actions run in degraded mode with a 90s Grace Period safety floor.".to_string(),
                dismissible: false,
            },
        );
    } else {
        let before = alerts.len();
        alerts.retain(|a| a.id != alert_ids::DEGRADED_IDLE);
        changed |= alerts.len() != before;
    }

    if backend.is_inhibitor_sensitive() {
        changed |= upsert_alert(
            alerts,
            DaemonAlert {
                id: alert_ids::INHIBITOR_SENSITIVE.to_string(),
                kind: alert_ids::INHIBITOR_SENSITIVE.to_string(),
                severity: AlertSeverity::Info,
                message: "Idle detection using Wayland ext-idle-notify-v1 v1 (inhibitor-sensitive: media playback idle inhibitors may suppress idle detection).".to_string(),
                dismissible: false,
            },
        );
    } else {
        let before = alerts.len();
        alerts.retain(|a| a.id != alert_ids::INHIBITOR_SENSITIVE);
        changed |= alerts.len() != before;
    }

    changed
}

pub(in crate::daemon) fn upsert_alert(alerts: &mut Vec<DaemonAlert>, next: DaemonAlert) -> bool {
    if let Some(existing) = alerts.iter_mut().find(|a| a.id == next.id) {
        if *existing == next {
            return false;
        }
        *existing = next;
        return true;
    }
    alerts.push(next);
    true
}
