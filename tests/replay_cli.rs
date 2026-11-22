mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command as ProcessCommand;

use bpmute::cli::{Cli, execute_cli};
use bpmute::config::{DetectionConfig, Sensitivity};
use bpmute::exit_code::ExitCode;
use bpmute::paths::MapEnv;
use bpmute::sleep_engine::{
    CalibrationPriors, ContactStatus, DetectorState, EngineConfig, EngineEvent, EngineOutput,
    Epoch, GraceOutcome, GraceResult, SleepEngine, WearableFloor,
};
use clap::Parser;
use common::synthetic::{Phenotype, StageKind, generate_paired_trace};
use serde_json::Value;
use tempfile::TempDir;

async fn run_inproc_cli(args: &[&str], env: &MapEnv) -> (ExitCode, String, String) {
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

fn temp_env(temp: &TempDir) -> MapEnv {
    MapEnv::new()
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
        )
}

fn contact_status_label(cs: ContactStatus) -> &'static str {
    match cs {
        ContactStatus::Unsupported => "unsupported",
        ContactStatus::NoContact => "no_contact",
        ContactStatus::Contact => "contact",
    }
}

fn stream_status_label(status: bpmute::sleep_engine::StreamStatus) -> &'static str {
    match status {
        bpmute::sleep_engine::StreamStatus::Streaming => "stream_streaming",
        bpmute::sleep_engine::StreamStatus::SignalLost => "stream_signal_lost",
        bpmute::sleep_engine::StreamStatus::Disconnected => "stream_disconnected",
        bpmute::sleep_engine::StreamStatus::Acquiring => "stream_acquiring",
        bpmute::sleep_engine::StreamStatus::Connecting => "stream_connecting",
        bpmute::sleep_engine::StreamStatus::PassiveStandby => "stream_passive_standby",
    }
}

fn events_to_csv(events: &[EngineEvent]) -> String {
    let mut out = String::from("ts_ms,bpm,rr_ms,contact_status,event\n");
    for ev in events {
        match ev {
            EngineEvent::HrPacket(pkt) => {
                let rr = pkt
                    .rr_ms
                    .iter()
                    .map(|v| format!("{v}"))
                    .collect::<Vec<_>>()
                    .join(";");
                let cs = contact_status_label(pkt.contact_status);
                out.push_str(&format!("{},{},{},{},\n", pkt.ts_ms, pkt.bpm, rr, cs));
            }
            EngineEvent::StreamState(st) => {
                out.push_str(&format!(
                    "{},,,,{}\n",
                    st.ts_ms,
                    stream_status_label(st.status)
                ));
            }
            EngineEvent::DesktopActivity(_) | EngineEvent::GraceOutcome(_) => {}
        }
    }
    out
}

fn events_to_jsonl(events: &[EngineEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        let line = match ev {
            EngineEvent::HrPacket(pkt) => serde_json::json!({
                "ts_ms": pkt.ts_ms,
                "bpm": pkt.bpm,
                "rr_ms": pkt.rr_ms,
                "contact_status": contact_status_label(pkt.contact_status),
            })
            .to_string(),
            EngineEvent::StreamState(st) => serde_json::json!({
                "ts_ms": st.ts_ms,
                "event": stream_status_label(st.status),
            })
            .to_string(),
            EngineEvent::DesktopActivity(_) | EngineEvent::GraceOutcome(_) => continue,
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[tokio::test]
async fn replay_csv_and_jsonl_matches_direct_engine_parity_and_stays_read_only() {
    let temp = TempDir::new().unwrap();
    let env = temp_env(&temp);

    let trace = generate_paired_trace(
        901,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep { duration_sec: 900 },
        ],
    );

    let csv_path = temp.path().join("sleep_rr.csv");
    fs::write(&csv_path, events_to_csv(&trace.bpm_rr)).unwrap();

    let jsonl_path = temp.path().join("sleep_gt5.jsonl");
    fs::write(&jsonl_path, events_to_jsonl(&trace.bpm_only_gt5)).unwrap();

    // 1) CSV replay via CLI with --json
    let (code, stdout, stderr) = run_inproc_cli(
        &[
            "bpmute",
            "replay",
            csv_path.to_str().unwrap(),
            "--preset",
            "balanced",
            "--cold-start",
            "--json",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {stderr}");

    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["v"], 1);
    assert_eq!(report["fidelity"], "raw_samples");
    assert_eq!(report["read_only"], true);
    assert_eq!(report["cold_start"], true);
    assert_eq!(report["preset"], "balanced");
    assert!(!report["sessions"].as_array().unwrap().is_empty());

    // Parity check against direct SleepEngine execution
    let replayed_epochs: Vec<Epoch> = serde_json::from_value(report["epochs"].clone()).unwrap();
    let mut direct = SleepEngine::new(EngineConfig {
        detection: DetectionConfig {
            sensitivity: Sensitivity::Balanced,
            advanced: None,
        },
        priors: CalibrationPriors::cold_start(),
        wearable_floor: WearableFloor::default(),
    });
    let mut direct_epochs = Vec::new();
    let mut pending_confirmed: Option<i64> = None;
    for ev in &trace.bpm_rr {
        if let Some(conf_ts) = pending_confirmed
            && ev.ts_ms() - conf_ts >= 30_000
        {
            pending_confirmed = None;
            for o in direct.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
                ts_ms: conf_ts + 30_000,
                outcome: GraceResult::Expired,
            })) {
                if let EngineOutput::EpochEmitted(ep) = o {
                    direct_epochs.push(ep);
                }
            }
        }
        for o in direct.handle_event(ev.clone()) {
            match o {
                EngineOutput::EpochEmitted(ep) => direct_epochs.push(ep),
                EngineOutput::StateChanged {
                    ts_ms,
                    to: DetectorState::Confirmed,
                    ..
                } => pending_confirmed = Some(ts_ms),
                _ => {}
            }
        }
    }
    assert_eq!(replayed_epochs.len(), direct_epochs.len());
    for (rep, dir) in replayed_epochs.iter().zip(&direct_epochs) {
        assert_eq!(rep.ts_ms, dir.ts_ms);
        assert_eq!(rep.state, dir.state);
        assert!(
            (rep.p_sleep - dir.p_sleep).abs() < 1e-12,
            "mismatch at ts={}: rep={} vs dir={}",
            rep.ts_ms,
            rep.p_sleep,
            dir.p_sleep
        );
    }

    // 2) JSONL replay (BPM-only) human-readable output + preset ordering
    let (code_j, stdout_j, stderr_j) =
        run_inproc_cli(&["bpmute", "replay", jsonl_path.to_str().unwrap()], &env).await;
    assert_eq!(code_j, ExitCode::Success, "stderr: {stderr_j}");
    assert!(stdout_j.contains("raw_samples"));
    assert!(stdout_j.contains("read-only"));

    // What-if preset comparison: responsive confirms earlier than conservative
    let (_, resp_out, _) = run_inproc_cli(
        &[
            "bpmute",
            "replay",
            csv_path.to_str().unwrap(),
            "--preset",
            "responsive",
            "--json",
        ],
        &env,
    )
    .await;
    let (_, cons_out, _) = run_inproc_cli(
        &[
            "bpmute",
            "replay",
            csv_path.to_str().unwrap(),
            "--preset",
            "conservative",
            "--json",
        ],
        &env,
    )
    .await;
    let resp_json: Value = serde_json::from_str(&resp_out).unwrap();
    let cons_json: Value = serde_json::from_str(&cons_out).unwrap();
    let resp_conf = resp_json["sessions"][0]["confirmed_ts_ms"]
        .as_i64()
        .unwrap();
    let cons_conf = cons_json["sessions"][0]["confirmed_ts_ms"]
        .as_i64()
        .unwrap();
    assert!(resp_conf < cons_conf);

    // 3) Zero XDG filesystem mutation during read-only replay
    assert!(!temp.path().join("cfg").try_exists().unwrap());
    assert!(!temp.path().join("data").try_exists().unwrap());
    assert!(!temp.path().join("run").try_exists().unwrap());

    // 4) Quoted JSON array rr_ms + contact_status=0b10 + config preset override behavior
    let cfg_dir = temp.path().join("cfg/bpmute");
    fs::create_dir_all(&cfg_dir).unwrap();
    let cfg_path = cfg_dir.join("config.toml");
    let orig_cfg = "schema_version = 1\n[detection]\nsensitivity = \"conservative\"\n[detection.advanced]\ndwell_bpm_rr_sec = 600\n";
    fs::write(&cfg_path, orig_cfg).unwrap();

    let quoted_csv = temp.path().join("quoted_bracket.csv");
    fs::write(
        &quoted_csv,
        "ts_ms,bpm,rr_ms,contact_status,event\n0,68,\"[880, 895]\",0b11,\n1000,0,,0b10,\n2000,67,\"[895]\",contact,\n",
    )
    .unwrap();
    let (code_q, out_q, err_q) = run_inproc_cli(
        &["bpmute", "replay", quoted_csv.to_str().unwrap(), "--json"],
        &env,
    )
    .await;
    assert_eq!(code_q, ExitCode::Success, "stderr: {err_q}");
    let q_json: Value = serde_json::from_str(&out_q).unwrap();
    assert_eq!(q_json["preset"], "conservative");
    assert_eq!(q_json["samples_processed"], 3);

    // Explicit --preset responsive overrides both sensitivity and detection.advanced without modifying config.toml
    let (code_ov, out_ov, _) = run_inproc_cli(
        &[
            "bpmute",
            "replay",
            csv_path.to_str().unwrap(),
            "--preset",
            "responsive",
            "--json",
        ],
        &env,
    )
    .await;
    assert_eq!(code_ov, ExitCode::Success);
    let ov_json: Value = serde_json::from_str(&out_ov).unwrap();
    assert_eq!(ov_json["preset"], "responsive");
    let ov_conf = ov_json["sessions"][0]["confirmed_ts_ms"].as_i64().unwrap();
    assert!(ov_conf < cons_conf);
    assert_eq!(fs::read_to_string(&cfg_path).unwrap(), orig_cfg);
}

#[test]
fn real_binary_replay_error_exit_codes() {
    let temp = TempDir::new().unwrap();
    let bin = env!("CARGO_BIN_EXE_bpmute");
    let run_bin = |args: &[&str]| {
        ProcessCommand::new(bin)
            .env_clear()
            .env("HOME", temp.path().join("home"))
            .env("XDG_CONFIG_HOME", temp.path().join("cfg"))
            .env("XDG_DATA_HOME", temp.path().join("data"))
            .env("XDG_RUNTIME_DIR", temp.path().join("run"))
            .args(args)
            .output()
            .unwrap()
    };

    // Malformed CSV -> exit 2
    let bad_csv = temp.path().join("bad.csv");
    fs::write(&bad_csv, "ts_ms,bpm\n1000,not_a_number\n").unwrap();
    assert_eq!(
        run_bin(&["replay", bad_csv.to_str().unwrap()])
            .status
            .code(),
        Some(2)
    );

    // Non-monotonic timestamps -> exit 2
    let non_mono = temp.path().join("nonmono.jsonl");
    fs::write(
        &non_mono,
        "{\"ts_ms\":2000,\"bpm\":65}\n{\"ts_ms\":1000,\"bpm\":64}\n",
    )
    .unwrap();
    assert_eq!(
        run_bin(&["replay", non_mono.to_str().unwrap()])
            .status
            .code(),
        Some(2)
    );

    // Missing file -> exit 1
    let missing = temp.path().join("missing.csv");
    assert_eq!(
        run_bin(&["replay", missing.to_str().unwrap()])
            .status
            .code(),
        Some(1)
    );

    // Unreadable file (0o000) -> exit 5 (unless running as root where DAC_OVERRIDE applies)
    let unreadable = temp.path().join("secret.csv");
    fs::write(&unreadable, "ts_ms,bpm\n1000,65\n").unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    let blocked_by_perm = fs::read_to_string(&unreadable).is_err();
    let out = run_bin(&["replay", unreadable.to_str().unwrap()]);
    let _ = fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600));
    if blocked_by_perm {
        assert_eq!(out.status.code(), Some(5));
    }
}

#[tokio::test]
async fn replay_reports_sleep_diagnostics_for_open_and_closed_sessions() {
    let temp = TempDir::new().unwrap();
    let env = temp_env(&temp);

    let trace = generate_paired_trace(
        901,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep { duration_sec: 900 },
        ],
    );

    let open_csv = temp.path().join("open_sleep.csv");
    fs::write(&open_csv, events_to_csv(&trace.bpm_rr)).unwrap();

    // Open-at-EOF sleep trace populates active session snapshot diagnostics.
    let (code_open, out_open, err_open) = run_inproc_cli(
        &[
            "bpmute",
            "replay",
            open_csv.to_str().unwrap(),
            "--cold-start",
            "--json",
        ],
        &env,
    )
    .await;
    assert_eq!(code_open, ExitCode::Success, "stderr: {err_open}");
    let open_json: Value = serde_json::from_str(&out_open).unwrap();
    let open_s = &open_json["sessions"][0];
    assert!(open_s["end_reason"].is_null());
    assert!(open_s["sleeping_ts_ms"].as_i64().is_some());
    assert!(open_s["sleep_mu_hr"].as_f64().is_some());
    assert!(open_s["warmup_sigma_hr"].as_f64().is_some());
    assert!(open_s["onset_hr_bpm"].as_f64().is_some());
    assert!(open_s["observed_hr_var_ratio"].as_f64().is_some());
    assert!(
        open_s["post_trigger_sleep_sec"].as_u64().unwrap() > 0,
        "expected post_trigger_sleep_sec > 0 for open sleep session: {open_s}"
    );

    // Human-readable output includes sleep_mu and post_trigger_sleep fields.
    let (code_txt, out_txt, err_txt) =
        run_inproc_cli(&["bpmute", "replay", open_csv.to_str().unwrap()], &env).await;
    assert_eq!(code_txt, ExitCode::Success, "stderr: {err_txt}");
    assert!(out_txt.contains("sleep_mu=Some("));
    assert!(out_txt.contains("post_trigger_sleep="));

    // Closed session via sustained post-sleep wake also retains diagnostics.
    let closed_trace = generate_paired_trace(
        901,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep { duration_sec: 900 },
            StageKind::PostSleepWake {
                duration_sec: 900,
                elevation_bpm: 18.0,
            },
        ],
    );
    let closed_csv = temp.path().join("closed_sleep.csv");
    fs::write(&closed_csv, events_to_csv(&closed_trace.bpm_rr)).unwrap();

    let (code_closed, out_closed, err_closed) = run_inproc_cli(
        &[
            "bpmute",
            "replay",
            closed_csv.to_str().unwrap(),
            "--cold-start",
            "--json",
        ],
        &env,
    )
    .await;
    assert_eq!(code_closed, ExitCode::Success, "stderr: {err_closed}");
    let closed_json: Value = serde_json::from_str(&out_closed).unwrap();
    let closed_s = &closed_json["sessions"][0];
    assert_eq!(closed_s["end_reason"], "sustained_wakefulness");
    assert!(closed_s["sleep_mu_hr"].as_f64().is_some());
    assert!(closed_s["warmup_sigma_hr"].as_f64().is_some());
    assert!(closed_s["onset_hr_bpm"].as_f64().is_some());
    assert!(closed_s["post_trigger_sleep_sec"].as_u64().unwrap() > 0);
}
