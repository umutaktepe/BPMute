use std::io::Write;
use std::path::Path;

use serde_json::json;

use crate::cli::duration::parse_pause_target;
use crate::exit_code::ExitCode;
use crate::ipc::protocol::{
    SnoozeGraceParams, SnoozeGraceResponse, TestTriggerParams, TestTriggerResponse,
};
use crate::ipc::{IpcClient, IpcClientError};

/// Cancels an active Grace Period countdown via `monitor.cancel_grace`.
pub async fn run_cancel<WOut: Write, WErr: Write>(
    socket_path: &Path,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let mut client = match IpcClient::connect(socket_path).await {
        Ok(c) => c,
        Err(err) => return handle_ipc_error(err, stderr),
    };

    match client.request("monitor.cancel_grace", json!({})).await {
        Ok(_) => {
            let _ = writeln!(
                stdout,
                "Cancelled sleep grace period; returning to monitoring with false-positive penalty."
            );
            ExitCode::Success
        }
        Err(err) => handle_ipc_error(err, stderr),
    }
}

/// Snoozes sleep triggers during an active Grace Period without calibration penalty.
pub async fn run_snooze<WOut: Write, WErr: Write>(
    socket_path: &Path,
    duration_str: Option<&str>,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let duration_sec = if let Some(dur_str) = duration_str {
        match parse_pause_target(Some(dur_str)) {
            Ok(crate::cli::duration::PauseTarget::DurationSecs(s)) => {
                if !(300..=14400).contains(&s) {
                    let _ = writeln!(
                        stderr,
                        "error: snooze duration must be between 5m and 4h (300 to 14400 seconds)"
                    );
                    return ExitCode::UsageOrConfigError;
                }
                Some(s as u32)
            }
            _ => {
                let _ = writeln!(
                    stderr,
                    "error: invalid snooze duration `{dur_str}`: expected duration like 30m, 1h"
                );
                return ExitCode::UsageOrConfigError;
            }
        }
    } else {
        None
    };

    let mut client = match IpcClient::connect(socket_path).await {
        Ok(c) => c,
        Err(err) => return handle_ipc_error(err, stderr),
    };

    let params = SnoozeGraceParams {
        duration_sec,
        duration_min: None,
    };

    let param_val = match serde_json::to_value(&params) {
        Ok(v) => v,
        Err(err) => {
            let _ = writeln!(stderr, "error: failed to serialize snooze params: {err}");
            return ExitCode::RuntimeError;
        }
    };

    match client.request("monitor.snooze_grace", param_val).await {
        Ok(resp) => {
            let parsed = serde_json::from_value::<SnoozeGraceResponse>(resp).ok();
            let dur_sec = parsed.map_or(1800, |r| r.duration_sec);
            let mins = dur_sec / 60;
            let _ = writeln!(
                stdout,
                "Snoozed sleep triggers for {mins}m; zero false-positive penalty recorded."
            );
            ExitCode::Success
        }
        Err(err) => handle_ipc_error(err, stderr),
    }
}

/// Initiates a zero-side-effect test trigger via `monitor.test_trigger`.
pub async fn run_test_trigger<WOut: Write, WErr: Write>(
    socket_path: &Path,
    grace_sec: Option<u32>,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    if let Some(sec) = grace_sec
        && !(10..=300).contains(&sec)
    {
        let _ = writeln!(stderr, "error: grace_sec must be within 10..=300 seconds");
        return ExitCode::UsageOrConfigError;
    }

    let mut client = match IpcClient::connect(socket_path).await {
        Ok(c) => c,
        Err(err) => return handle_ipc_error(err, stderr),
    };

    let params = TestTriggerParams {
        grace_sec,
        grace_period_sec: None,
    };

    let param_val = match serde_json::to_value(&params) {
        Ok(v) => v,
        Err(err) => {
            let _ = writeln!(
                stderr,
                "error: failed to serialize test_trigger params: {err}"
            );
            return ExitCode::RuntimeError;
        }
    };

    match client.request("monitor.test_trigger", param_val).await {
        Ok(resp) => {
            let parsed = serde_json::from_value::<TestTriggerResponse>(resp).ok();
            let eff_sec = parsed.map_or(30, |r| r.grace_period_sec);
            let _ = writeln!(
                stdout,
                "Initiated dry-run test trigger ({eff_sec}s grace period)."
            );
            ExitCode::Success
        }
        Err(err) => handle_ipc_error(err, stderr),
    }
}

fn handle_ipc_error<WErr: Write>(err: IpcClientError, stderr: &mut WErr) -> ExitCode {
    match err {
        IpcClientError::DaemonOffline { path, .. } => {
            let _ = writeln!(
                stderr,
                "error: daemon is offline (socket {path} unreachable)"
            );
            ExitCode::DaemonOffline
        }
        IpcClientError::PermissionDenied { path, .. } => {
            let _ = writeln!(stderr, "error: permission denied accessing socket {path}");
            ExitCode::PermissionOrBackendRefusal
        }
        IpcClientError::Server {
            ref code,
            ref message,
        } => {
            let _ = writeln!(stderr, "error: {message}");
            ExitCode::from_ipc_error_code(code)
        }
        other => {
            let _ = writeln!(stderr, "error: {other}");
            ExitCode::RuntimeError
        }
    }
}
