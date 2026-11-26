//! Offline and online session replay CLI command.

use std::io::Write;

use crate::config::LoadedConfig;
use crate::exit_code::ExitCode;
use crate::replay::{ReplayFidelity, ReplayOptions, replay_file, replay_stored_session};
use crate::sleep_engine::{CalibrationPriors, WearableFloor};
use crate::storage::db_path_in_data_dir;

use super::super::ReplayCliArgs;

pub(crate) fn run_replay<WOut: Write, WErr: Write>(
    args: ReplayCliArgs<'_>,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let loaded = match LoadedConfig::load(&args.paths.config_file, args.env, args.cli_overrides) {
        Ok(loaded) => loaded,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return err.exit_code();
        }
    };

    let grace_sec = loaded.config.actions.effective_grace_period_sec();
    let report = if let Some(session_id) = args.session {
        let db_path = db_path_in_data_dir(&args.paths.data_dir);
        replay_stored_session(
            &db_path,
            session_id,
            loaded.config.detection,
            args.preset,
            args.cold_start,
            grace_sec,
        )
    } else if let Some(file) = args.file {
        let options = ReplayOptions {
            preset: args.preset,
            cold_start: args.cold_start,
            grace_period_sec: grace_sec,
            priors: CalibrationPriors::cold_start(),
            wearable_floor: WearableFloor::default(),
        };
        replay_file(file, loaded.config.detection, options)
    } else {
        let _ = writeln!(stderr, "error: either <FILE> or --session <ID> is required");
        return ExitCode::UsageOrConfigError;
    };

    let report = match report {
        Ok(report) => report,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return err.exit_code();
        }
    };

    if args.as_json {
        if let Err(err) = serde_json::to_writer(&mut *stdout, &report) {
            let _ = writeln!(stderr, "error: failed to serialize replay report: {err}");
            return ExitCode::RuntimeError;
        }
        let _ = writeln!(stdout);
        return ExitCode::Success;
    }

    let fidelity_desc = match report.fidelity {
        ReplayFidelity::RawSamples => "raw_samples (full 4-stage filter + HMM pipeline)",
        ReplayFidelity::SessionEpochs => {
            "session_epochs (fallback: raw_samples unavailable; post-filter HMM + state machine)"
        }
    };
    let _ = writeln!(
        stdout,
        "Replay fidelity: {} (read-only) | format: {} | preset: {} | cold_start: {}",
        fidelity_desc,
        report.format.as_str(),
        report.preset,
        report.cold_start
    );
    let _ = writeln!(
        stdout,
        "Processed {} HR samples ({} events) -> {} epochs across {} session(s)",
        report.samples_processed,
        report.events_processed,
        report.epochs_emitted,
        report.sessions.len()
    );
    for s in &report.sessions {
        let end_tag = s
            .end_reason
            .map(|r| format!("{r:?}"))
            .unwrap_or_else(|| "open".to_string());
        let _ = writeln!(
            stdout,
            "  Session #{}: start={} ms, end={} ms ({}), epochs={}, confirmed={:?}, sleeping={:?}, sleep_mu={:?}, post_trigger_sleep={}s, anomalous={}, fps={}",
            s.session_index,
            s.start_ts_ms,
            s.end_ts_ms,
            end_tag,
            s.epochs_count,
            s.confirmed_ts_ms,
            s.sleeping_ts_ms,
            s.sleep_mu_hr,
            s.post_trigger_sleep_sec,
            s.anomalous,
            s.fp_count
        );
    }

    ExitCode::Success
}
