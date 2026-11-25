#![allow(
    dead_code,
    reason = "shared test harness items are used selectively by separate integration test binaries"
)]

use std::path::PathBuf;

use bpmute::cli::{Cli, execute_cli};
use bpmute::config::{PowerAction, Sensitivity};
use bpmute::exit_code::ExitCode;
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::{ContactStatus, DetectorState, Epoch, EpochFeatures, HrPacket};
use bpmute::storage::{
    CloseSessionRequest, LedgerStore, OpenSessionRequest, WearableDescriptor, db_path_in_data_dir,
    lock_path_in_data_dir,
};
use clap::Parser;
use tempfile::TempDir;

use super::sample_session_summary;

/// Isolated temporary directory, `MapEnv`, `ResolvedPaths`, and SQLite paths for storage/CLI tests.
pub struct TempStoreEnv {
    pub temp: TempDir,
    pub paths: ResolvedPaths,
    pub env: MapEnv,
    pub db_path: PathBuf,
    pub lock_path: PathBuf,
}

impl TempStoreEnv {
    pub fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let (paths, env) = test_paths_and_env(&temp);
        let db_path = db_path_in_data_dir(&paths.data_dir);
        let lock_path = lock_path_in_data_dir(&paths.data_dir);
        Self {
            temp,
            paths,
            env,
            db_path,
            lock_path,
        }
    }
}

pub fn test_paths_and_env(temp: &TempDir) -> (ResolvedPaths, MapEnv) {
    let env = MapEnv::new()
        .with(
            "HOME",
            temp.path().join("home").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_CONFIG_HOME",
            temp.path().join("cfg").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_DATA_HOME",
            temp.path().join("data").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_RUNTIME_DIR",
            temp.path().join("run").to_string_lossy().into_owned(),
        );
    let paths = ResolvedPaths::resolve(None, None, None, &env).unwrap();
    (paths, env)
}

pub async fn run_inproc_cli(args: &[&str], env: &MapEnv) -> (ExitCode, String, String) {
    let cli = Cli::try_parse_from(args).expect("CLI args should parse");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = execute_cli(cli, env, &mut stdout, &mut stderr).await;
    (
        code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

pub fn sample_epoch(ts_ms: i64, hr_bpm: f64, p_sleep: f64, state: DetectorState) -> Epoch {
    Epoch {
        session_index: 1,
        ts_ms,
        features: EpochFeatures {
            hr_bpm_60s: hr_bpm,
            z_hr: -1.5,
            hr_slope_4m: -0.35,
            r_var_120s: 0.45,
            ln_rmssd_60s: Some(3.6),
            delta_ln_rmssd_60s: Some(0.15),
            rmssd_sdnn_120s: Some(0.95),
        },
        hrv_valid: true,
        rr_artifact_frac: 0.01,
        coverage: 1.0,
        flatline_stale: false,
        accumulators_frozen: false,
        baseline_mu_hr: 66.0,
        baseline_sigma_hr: 2.3,
        baseline_frozen: p_sleep >= 0.30,
        p_active_wake: 0.05,
        p_quiet_wake: 1.0 - p_sleep - 0.05,
        p_sleep,
        state,
    }
}

pub fn seed_wearable_and_closed_session(
    store: &mut LedgerStore,
    mac: &str,
    alias: &str,
    start_ts_ms: i64,
    warmup_mu_hr: f64,
    raw_count: u32,
) -> (String, String) {
    let wid = store
        .upsert_wearable(
            &WearableDescriptor {
                mac: mac.to_string(),
                address_type: "public".to_string(),
                alias: Some(alias.to_string()),
                advertised_name: None,
                supports_rr: true,
                supports_contact: true,
            },
            start_ts_ms.saturating_sub(1_000),
        )
        .unwrap();
    let sid = store
        .open_session(&OpenSessionRequest {
            wearable_id: wid.clone(),
            start_ts_ms,
            detection_sensitivity: Sensitivity::Balanced,
        })
        .unwrap();
    if raw_count > 0 {
        let raw: Vec<(u32, HrPacket)> = (0..raw_count)
            .map(|i| {
                (
                    i,
                    HrPacket {
                        ts_ms: start_ts_ms + i64::from(i) * 1000,
                        bpm: 58.0 + ((i % 28) as f64) * 0.5,
                        rr_ms: vec![f64::from(880 + (i % 150) as u16) * 1000.0 / 1024.0],
                        contact_status: ContactStatus::Contact,
                    },
                )
            })
            .collect();
        let raw_refs: Vec<(u32, &HrPacket)> = raw.iter().map(|(seq, p)| (*seq, p)).collect();
        store.append_epoch_and_raw(&sid, None, &raw_refs).unwrap();
    }
    let mut summary = sample_session_summary(start_ts_ms, start_ts_ms + 200_000, 56.0);
    summary.warmup_mu_hr = Some(warmup_mu_hr);
    store
        .close_session_and_recompute(
            &CloseSessionRequest {
                session_id: sid.clone(),
                summary,
                trigger_power_action: PowerAction::None,
                rapid_resume_within_5m: false,
            },
            start_ts_ms + 200_000,
        )
        .unwrap();
    (wid, sid)
}
