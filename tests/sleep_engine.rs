mod common;

use bpmute::config::{DetectionAdvancedConfig, DetectionConfig, Sensitivity};
use bpmute::sleep_engine::{
    CalibrationMaturity, CalibrationPriors, ContactStatus, DesktopActivity, DesktopActivityKind,
    DetectorState, EngineConfig, EngineEvent, EngineOutput, Epoch, GapTier, GraceAbortReason,
    GraceOutcome, GraceResult, HrPacket, SessionEndReason, SleepEngine, StreamState, StreamStatus,
    WearableFloor,
};

fn default_engine() -> SleepEngine {
    SleepEngine::new(EngineConfig {
        detection: DetectionConfig::default(),
        priors: CalibrationPriors::cold_start(),
        wearable_floor: WearableFloor::default(),
    })
}

#[test]
fn slice1_stage1_bounds_contact_gate_and_coverage_freeze() {
    let mut engine = default_engine();
    assert_eq!(engine.state(), DetectorState::Idle);
    assert_eq!(engine.current_session_index(), None);

    // Start streaming explicitly.
    let out = engine.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    assert_eq!(engine.state(), DetectorState::Warmup);
    assert_eq!(engine.current_session_index(), Some(1));
    assert!(out.iter().any(|o| matches!(
        o,
        EngineOutput::SessionStarted {
            session_index: 1,
            ts_ms: 0
        }
    )));

    // First 15 s epoch: 10 valid packets at 68 bpm + 5 rejected packets (BPM==0, out-of-bounds, NoContact)
    // Valid coverage = 10 / 15 = 0.667 < 0.80 => accumulators_frozen = true.
    let mut epoch1 = None;
    for sec in 1..=15_i64 {
        let ts_ms = sec * 1000;
        let pkt = match sec {
            3 => HrPacket {
                ts_ms,
                bpm: 0.0,
                rr_ms: vec![],
                contact_status: ContactStatus::Contact,
            },
            6 => HrPacket {
                ts_ms,
                bpm: 25.0,
                rr_ms: vec![],
                contact_status: ContactStatus::Contact,
            },
            9 => HrPacket {
                ts_ms,
                bpm: 215.0,
                rr_ms: vec![],
                contact_status: ContactStatus::Contact,
            },
            12 => HrPacket {
                ts_ms,
                bpm: 68.0,
                rr_ms: vec![],
                contact_status: ContactStatus::NoContact,
            },
            14 => HrPacket {
                ts_ms,
                bpm: f64::NAN,
                rr_ms: vec![],
                contact_status: ContactStatus::Contact,
            },
            _ => HrPacket {
                ts_ms,
                bpm: 68.0,
                rr_ms: vec![880.0],
                contact_status: ContactStatus::Contact,
            },
        };
        for o in engine.handle_event(EngineEvent::HrPacket(pkt)) {
            if let EngineOutput::EpochEmitted(ep) = o {
                epoch1 = Some(ep);
            }
        }
    }

    let ep1 = epoch1.expect("first 15 s epoch should be emitted");
    assert!(
        ep1.coverage < 0.80,
        "expected coverage < 0.80, got {}",
        ep1.coverage
    );
    assert!(
        ep1.accumulators_frozen,
        "expected accumulators frozen on low coverage"
    );
    assert!(
        (ep1.features.hr_bpm_60s - 68.0).abs() < 1e-6,
        "rejected BPM spikes/zeros must not enter hr_bpm_60s: got {}",
        ep1.features.hr_bpm_60s
    );

    // Second 15 s epoch: 15 valid packets at 68 bpm with valid RR (plus one out-of-bounds RR ignored/corrected)
    // Coverage = 1.0 >= 0.80 => accumulators_frozen = false.
    let mut epoch2 = None;
    for sec in 16..=30_i64 {
        let ts_ms = sec * 1000;
        let rr_ms = if sec == 20 {
            vec![250.0, 882.0] // 250 ms < 300 ms must be rejected by Stage 1 RR bounds
        } else {
            vec![880.0 + ((sec % 3) as f64) * 5.0]
        };
        for o in engine.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms,
            bpm: 68.0,
            rr_ms,
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                epoch2 = Some(ep);
            }
        }
    }

    let ep2 = epoch2.expect("second 15 s epoch should be emitted");
    assert!(
        ep2.coverage >= 0.80,
        "expected coverage >= 0.80, got {}",
        ep2.coverage
    );
    assert!(!ep2.accumulators_frozen);
    assert!(
        ep2.rr_artifact_frac > 0.0,
        "out-of-bounds RR should register as artifact"
    );
}

#[test]
fn slice2_hampel_filter_lt_rr_sqi_and_mode_aware_flatlines() {
    // 1) Causal 25 s Hampel filter rejects transient motion spike (145 bpm) while preserving
    // legitimate integer-quantized descent.
    let mut engine = default_engine();
    let mut last_ep = None;
    for sec in 1..=30_i64 {
        let bpm = if sec == 25 { 145.0 } else { 66.0 };
        for o in engine.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                last_ep = Some(ep);
            }
        }
    }
    let ep_spike = last_ep.expect("epoch emitted");
    assert!(
        (ep_spike.features.hr_bpm_60s - 66.0).abs() < 0.05,
        "Hampel filter should replace 145 bpm spike with ~66 bpm, got {}",
        ep_spike.features.hr_bpm_60s
    );

    // 2) Lipponen-Tarvainen RR cleaning and > 5% 60 s SQI gate.
    let mut engine_rr = default_engine();
    let mut epochs_rr = Vec::new();
    // 120 s clean RR with gentle RSA oscillation (period 6 s -> ~900 +/- 25 ms), then 60 s with
    // >5% ectopic/extra RR artifacts (6 artifacts in 60 beats = 10%).
    for sec in 1..=180_i64 {
        let phase = (sec as f64) * std::f64::consts::TAU / 6.0;
        let clean_rr = 900.0 + 25.0 * phase.sin();
        let rr = if sec > 120 && sec % 10 == 0 {
            420.0 // extra/ectopic spike < 0.65 * 900
        } else {
            clean_rr
        };
        for o in engine_rr.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm: 66.0,
            rr_ms: vec![rr],
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                epochs_rr.push(ep);
            }
        }
    }
    let clean_ep = epochs_rr
        .iter()
        .find(|e| e.ts_ms >= 120_000)
        .expect("epoch at ~120 s");
    assert!(clean_ep.hrv_valid, "clean RR should enable HRV features");
    assert!(clean_ep.features.ln_rmssd_60s.is_some());
    assert!(clean_ep.features.rmssd_sdnn_120s.is_some());

    let noisy_ep = epochs_rr.last().expect("final noisy epoch");
    assert!(
        noisy_ep.rr_artifact_frac > 0.05,
        "expected > 5% corrected RR beats, got {}",
        noisy_ep.rr_artifact_frac
    );
    assert!(!noisy_ep.hrv_valid, "HRV must gate off when corrected > 5%");
    assert!(noisy_ep.features.ln_rmssd_60s.is_none());
    assert!(noisy_ep.features.rmssd_sdnn_120s.is_none());

    // 3) Mode-aware flatline detection & Wearable R_var floor:
    // BPM-only 60 s plateau must NOT flag flatline, and R_var >= wearable_floor; 130 s flags stale.
    let mut engine_bpm_only = SleepEngine::new(EngineConfig {
        detection: DetectionConfig::default(),
        priors: CalibrationPriors::cold_start(),
        wearable_floor: WearableFloor { hr_var_floor: 0.30 },
    });
    let mut epochs_bpm_only = Vec::new();
    for sec in 1..=150_i64 {
        for o in engine_bpm_only.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm: 58.0,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                epochs_bpm_only.push(ep);
            }
        }
    }
    let ep_60 = epochs_bpm_only
        .iter()
        .find(|e| e.ts_ms >= 60_000)
        .expect("60 s epoch");
    assert!(
        !ep_60.flatline_stale && !ep_60.accumulators_frozen,
        "60 s integer BPM-only plateau must not trigger flatline"
    );
    assert!(
        ep_60.features.r_var_120s >= 0.30,
        "R_var must respect per-Wearable floor 0.30, got {}",
        ep_60.features.r_var_120s
    );
    let ep_135 = epochs_bpm_only
        .iter()
        .find(|e| e.ts_ms >= 135_000)
        .expect("135 s epoch");
    assert!(
        ep_135.flatline_stale,
        "130+ s BPM-only flatline must flag stale"
    );
    assert!(ep_135.accumulators_frozen);

    // BPM+RR: 35 s identical BPM with varying RR is NOT flatline; 35 s frozen RR IS flatline.
    let mut engine_rr_varying = default_engine();
    let mut last_varying = None;
    for sec in 1..=45_i64 {
        let rr = if sec % 2 == 0 { 1020.0 } else { 1045.0 };
        for o in engine_rr_varying.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm: 58.0,
            rr_ms: vec![rr],
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                last_varying = Some(ep);
            }
        }
    }
    assert!(!last_varying.unwrap().flatline_stale);

    let mut engine_rr_frozen = default_engine();
    let mut last_frozen = None;
    for sec in 1..=45_i64 {
        for o in engine_rr_frozen.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm: 58.0,
            rr_ms: vec![1034.0],
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                last_frozen = Some(ep);
            }
        }
    }
    let frozen_ep = last_frozen.unwrap();
    assert!(frozen_ep.flatline_stale);
    assert!(frozen_ep.accumulators_frozen);
}

#[test]
fn slice3_warmup_durations_hmm_baseline_freeze_and_hrv_marginalization() {
    // 1) Warmup duration: ColdStart requires 32 clean epochs (8 min), Calibrated requires 20 (5 min).
    let mut cold = default_engine();
    cold.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let mut cold_epochs = Vec::new();
    for sec in 1..=500_i64 {
        let bpm = 68.0 + ((sec % 5) as f64 - 2.0) * 0.8;
        for o in cold.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                cold_epochs.push(ep);
            }
        }
    }
    assert_eq!(cold_epochs[30].state, DetectorState::Warmup);
    assert_eq!(cold_epochs[31].state, DetectorState::Monitoring);

    let mut cal = SleepEngine::new(EngineConfig {
        detection: DetectionConfig::default(),
        priors: CalibrationPriors {
            maturity: CalibrationMaturity::Calibrated,
            mu_wake: Some(68.0),
            wake_median: None,
            sigma_wake: Some(2.2),
            mu_sleep: Some(60.0),
            delta_hr_onset: Some(-5.5),
            mu_ln_rmssd_sleep: Some(3.7),
            tau_p_sleep_offset: 0.0,
            dwell_adjust_sec: 0,
            wake_mad_s: Some(2.5),
        },
        wearable_floor: WearableFloor::default(),
    });
    cal.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let mut cal_epochs = Vec::new();
    for sec in 1..=320_i64 {
        let bpm = 68.0 + ((sec % 5) as f64 - 2.0) * 0.8;
        for o in cal.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                cal_epochs.push(ep);
            }
        }
    }
    assert_eq!(cal_epochs[18].state, DetectorState::Warmup);
    assert_eq!(cal_epochs[19].state, DetectorState::Monitoring);

    // 2) ActiveWake spike (> +15%) excluded from Quiet-Wake Baseline, then slow N1->N2 descent
    // freezes baseline immediately when P(SleepNREM) >= 0.30, and mid-transition RR dropout
    // produces |ΔP| <= 0.15 single-epoch jump.
    let mut engine = default_engine();
    engine.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let mut epochs: Vec<Epoch> = Vec::new();

    for sec in 1..=1800_i64 {
        let (bpm, rr) = if sec <= 600 {
            // Calm wake around 68 bpm with waking RSA + natural sigh variance (~2.1 bpm SD)
            let wave = 2.8 * (sec as f64 * 0.11).sin() + 1.2 * (sec as f64 * 0.31).cos();
            let base = 68.0 + wave;
            let rr_val = 60_000.0 / base + 12.0 * (sec as f64 * 1.5).sin();
            (base, vec![rr_val])
        } else if sec <= 720 {
            // ActiveWake spike > +15% (84 bpm)
            let base = 84.0 + 3.0 * (sec as f64 * 0.25).sin();
            (base, vec![60_000.0 / base])
        } else if sec <= 900 {
            // Back to calm wake 68 bpm
            let base = 68.0 + 2.6 * (sec as f64 * 0.13).sin();
            let rr_val = 60_000.0 / base + 12.0 * (sec as f64 * 1.5).sin();
            (base, vec![rr_val])
        } else {
            // Slow N1 -> N2 descent (-0.5 bpm/min down to 60 bpm, low variance)
            let mins = (sec - 900) as f64 / 60.0;
            let base = (68.0 - 0.55 * mins).max(60.0) + 0.4 * (sec as f64 * 0.4).sin();
            let rr_val = if sec >= 1320 && sec % 4 == 0 {
                // Mid-transition >5% RR artifact burst forcing HRV marginalization
                390.0
            } else {
                60_000.0 / base + 24.0 * (sec as f64 * 1.5).sin()
            };
            (base, vec![rr_val])
        };

        for o in engine.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm,
            rr_ms: rr,
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                epochs.push(ep);
            }
        }
    }

    let post_spike_ep = epochs.iter().find(|e| e.ts_ms == 840_000).unwrap();
    assert!(
        (post_spike_ep.baseline_mu_hr - 68.0).abs() < 1.5,
        "ActiveWake 84 bpm spike must be excluded from Quiet-Wake Baseline, got {}",
        post_spike_ep.baseline_mu_hr
    );
    assert!(post_spike_ep.p_sleep < 0.30);

    let first_freeze_idx = epochs
        .iter()
        .position(|e| e.ts_ms > 900_000 && e.p_sleep >= 0.30)
        .expect("slow sleep descent must reach P(SleepNREM) >= 0.30");
    let frozen_mu = epochs[first_freeze_idx].baseline_mu_hr;
    assert!(epochs[first_freeze_idx].baseline_frozen);
    let later_ep = epochs.last().unwrap();
    assert!(
        (later_ep.baseline_mu_hr - frozen_mu).abs() < 0.25,
        "frozen Quiet-Wake Baseline must not chase N1 descent: froze at {frozen_mu}, later {}",
        later_ep.baseline_mu_hr
    );
    assert!(
        later_ep.p_sleep >= 0.85,
        "expected sleep onset P >= 0.85, got {}",
        later_ep.p_sleep
    );

    // Check marginalization discontinuity when RR drops out around sec 1320 (hrv_valid true -> false)
    for w in epochs.windows(2) {
        if w[0].hrv_valid && !w[1].hrv_valid {
            let jump = (w[1].p_sleep - w[0].p_sleep).abs();
            assert!(
                jump <= 0.15,
                "HRV marginalization single-epoch jump |ΔP| = {jump:.3} exceeded 0.15 ({} -> {})",
                w[0].p_sleep,
                w[1].p_sleep
            );
        }
    }
}

fn push_calm_wake(
    engine: &mut SleepEngine,
    start_sec: i64,
    end_sec: i64,
    base_bpm: f64,
    with_rr: bool,
) -> Vec<EngineOutput> {
    let mut out = Vec::new();
    for sec in start_sec..=end_sec {
        let wave = 2.6 * (sec as f64 * 0.11).sin() + 1.1 * (sec as f64 * 0.31).cos();
        let bpm = base_bpm + wave;
        let rr_ms = if with_rr {
            vec![60_000.0 / bpm + 14.0 * (sec as f64 * 1.5).sin()]
        } else {
            vec![]
        };
        out.extend(engine.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm,
            rr_ms,
            contact_status: ContactStatus::Contact,
        })));
    }
    out
}

fn push_sleep(
    engine: &mut SleepEngine,
    start_sec: i64,
    end_sec: i64,
    wake_bpm: f64,
    sleep_bpm: f64,
    with_rr: bool,
) -> Vec<EngineOutput> {
    let mut out = Vec::new();
    for sec in start_sec..=end_sec {
        let mins = (sec - start_sec) as f64 / 60.0;
        let target = (wake_bpm - 0.8 * mins).max(sleep_bpm);
        let bpm = target + 0.35 * (sec as f64 * 0.4).sin();
        let rr_ms = if with_rr {
            vec![60_000.0 / bpm + 26.0 * (sec as f64 * 1.5).sin()]
        } else {
            vec![]
        };
        out.extend(engine.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm,
            rr_ms,
            contact_status: ContactStatus::Contact,
        })));
    }
    out
}

fn push_sleep_until_state(
    engine: &mut SleepEngine,
    start_sec: i64,
    max_end_sec: i64,
    wake_bpm: f64,
    sleep_bpm: f64,
    with_rr: bool,
    target_state: DetectorState,
) -> (Vec<EngineOutput>, i64) {
    let mut out = Vec::new();
    for sec in start_sec..=max_end_sec {
        let mins = (sec - start_sec) as f64 / 60.0;
        let target = (wake_bpm - 0.8 * mins).max(sleep_bpm);
        let bpm = target + 0.35 * (sec as f64 * 0.4).sin();
        let rr_ms = if with_rr {
            vec![60_000.0 / bpm + 26.0 * (sec as f64 * 1.5).sin()]
        } else {
            vec![]
        };
        out.extend(engine.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm,
            rr_ms,
            contact_status: ContactStatus::Contact,
        })));
        if engine.state() == target_state {
            return (out, sec);
        }
    }
    (out, max_end_sec)
}

fn confirmed_ts(outputs: &[EngineOutput]) -> Option<i64> {
    outputs.iter().find_map(|o| match o {
        EngineOutput::StateChanged {
            ts_ms,
            to: DetectorState::Confirmed,
            ..
        } => Some(*ts_ms),
        _ => None,
    })
}

#[test]
fn slice4_presets_dwell_grace_outcomes_cooldown_and_anomalous_detection() {
    // 1) Preset latency ordering: Responsive < Balanced < Conservative, plus advanced override
    let run_preset = |sensitivity: Sensitivity, advanced: Option<DetectionAdvancedConfig>| -> i64 {
        let mut eng = SleepEngine::new(EngineConfig {
            detection: DetectionConfig {
                sensitivity,
                advanced,
            },
            priors: CalibrationPriors::cold_start(),
            wearable_floor: WearableFloor::default(),
        });
        eng.handle_event(EngineEvent::StreamState(StreamState {
            ts_ms: 0,
            status: StreamStatus::Streaming,
        }));
        let mut all = push_calm_wake(&mut eng, 1, 600, 68.0, true);
        let (slp, _) = push_sleep_until_state(
            &mut eng,
            601,
            1800,
            68.0,
            60.0,
            true,
            DetectorState::Confirmed,
        );
        all.extend(slp);
        confirmed_ts(&all).expect("preset must reach Confirmed")
    };

    let t_resp = run_preset(Sensitivity::Responsive, None);
    let t_bal = run_preset(Sensitivity::Balanced, None);
    let t_cons = run_preset(Sensitivity::Conservative, None);
    assert!(
        t_resp < t_bal && t_bal < t_cons,
        "expected Responsive ({t_resp}) < Balanced ({t_bal}) < Conservative ({t_cons})"
    );
    let t_adv = run_preset(
        Sensitivity::Balanced,
        Some(DetectionAdvancedConfig {
            p_sleep_threshold: Some(0.75),
            dwell_bpm_rr_sec: Some(60),
            dwell_bpm_only_sec: Some(120),
        }),
    );
    assert!(
        t_adv < t_bal,
        "advanced 60 s dwell ({t_adv}) should confirm faster than Balanced ({t_bal})"
    );

    // 2) Cancel cooldown (15 min), FP cap (max 1 per session), Snooze (0 FP), and Arousal abort (0 FP)
    let mut eng = default_engine();
    eng.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let mut out = push_calm_wake(&mut eng, 1, 600, 68.0, true);
    let (slp1, t_conf_sec) = push_sleep_until_state(
        &mut eng,
        601,
        1400,
        68.0,
        60.0,
        true,
        DetectorState::Confirmed,
    );
    out.extend(slp1);
    let t_conf = confirmed_ts(&out).expect("first Confirmed");
    assert_eq!(eng.state(), DetectorState::Confirmed);

    // Cancel #1 -> Monitoring + 15 min cooldown + counted FP
    let cancel_ts = t_conf + 5_000;
    let cancel_out = eng.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: cancel_ts,
        outcome: GraceResult::Cancelled,
    }));
    assert_eq!(eng.state(), DetectorState::Monitoring);
    assert!(
        cancel_out
            .iter()
            .any(|o| matches!(o, EngineOutput::FalsePositiveRecorded { counted: true, .. }))
    );

    // Keep low HR during cooldown (15 min = 900 s): must NOT enter Candidate before cooldown ends!
    let cd_start_sec = t_conf_sec + 6;
    let cd_end_sec = (cancel_ts / 1000) + 890;
    let during_cd = push_sleep(&mut eng, cd_start_sec, cd_end_sec, 60.0, 60.0, true);
    assert!(
        !during_cd.iter().any(|o| matches!(
            o,
            EngineOutput::StateChanged {
                to: DetectorState::Candidate | DetectorState::Confirmed,
                ..
            }
        )),
        "must suppress Candidate during 15 min post-cancel cooldown"
    );

    // After cooldown expires and user falls back asleep, Confirmed can trigger again.
    // Cancelling a second time in the SAME session must emit counted: false (max 1 FP/session).
    let mut wake_then_sleep =
        push_calm_wake(&mut eng, cd_end_sec + 1, cd_end_sec + 400, 68.0, true);
    let (slp2, _) = push_sleep_until_state(
        &mut eng,
        cd_end_sec + 401,
        cd_end_sec + 1600,
        68.0,
        59.0,
        true,
        DetectorState::Confirmed,
    );
    wake_then_sleep.extend(slp2);
    let t_conf2 = confirmed_ts(&wake_then_sleep).expect("second Confirmed after cooldown");
    let cancel2_out = eng.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: t_conf2 + 5_000,
        outcome: GraceResult::Cancelled,
    }));
    assert!(cancel2_out.iter().any(|o| matches!(
        o,
        EngineOutput::FalsePositiveRecorded { counted: false, .. }
    )));

    // Snooze emits zero FP
    let mut eng_snooze = default_engine();
    eng_snooze.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let _ = push_calm_wake(&mut eng_snooze, 1, 600, 68.0, true);
    let (out_s, _) = push_sleep_until_state(
        &mut eng_snooze,
        601,
        1400,
        68.0,
        60.0,
        true,
        DetectorState::Confirmed,
    );
    let tc = confirmed_ts(&out_s).expect("Confirmed before snooze");
    let snooze_out = eng_snooze.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: tc + 5_000,
        outcome: GraceResult::Snoozed { duration_sec: 1800 },
    }));
    assert_eq!(eng_snooze.state(), DetectorState::Monitoring);
    assert!(
        !snooze_out
            .iter()
            .any(|o| matches!(o, EngineOutput::FalsePositiveRecorded { .. }))
    );

    // Confirmed physiological arousal (P(SleepNREM) < 0.50 for 2 consecutive epochs) emits GraceAbortRequested
    let mut eng_arousal = default_engine();
    eng_arousal.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let _ = push_calm_wake(&mut eng_arousal, 1, 600, 68.0, true);
    let (_, t_arousal_conf_sec) = push_sleep_until_state(
        &mut eng_arousal,
        601,
        1400,
        68.0,
        60.0,
        true,
        DetectorState::Confirmed,
    );
    assert_eq!(eng_arousal.state(), DetectorState::Confirmed);
    let arousal_out = push_calm_wake(
        &mut eng_arousal,
        t_arousal_conf_sec + 1,
        t_arousal_conf_sec + 300,
        82.0,
        true,
    );
    assert!(arousal_out.iter().any(|o| matches!(
        o,
        EngineOutput::GraceAbortRequested {
            reason: GraceAbortReason::PhysiologicalArousal,
            ..
        }
    )));
    assert!(
        !arousal_out
            .iter()
            .any(|o| matches!(o, EngineOutput::FalsePositiveRecorded { .. }))
    );

    // 3) Anomalous Session (> 2.5 S warmup deviation) runs relative-only and still detects sleep
    let mut eng_anom = SleepEngine::new(EngineConfig {
        detection: DetectionConfig::default(),
        priors: CalibrationPriors {
            maturity: CalibrationMaturity::Calibrated,
            mu_wake: Some(62.0),
            wake_median: None,
            sigma_wake: Some(2.2),
            mu_sleep: Some(55.0),
            delta_hr_onset: Some(-5.5),
            mu_ln_rmssd_sleep: Some(3.8),
            tau_p_sleep_offset: 0.0,
            dwell_adjust_sec: 0,
            wake_mad_s: Some(2.0),
        },
        wearable_floor: WearableFloor::default(),
    });
    eng_anom.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    // Anomalous Session (elevated waking HR 76 bpm > 62 + 2.5 * 2 = 67), drops to 68 bpm asleep!
    let mut out_anom = push_calm_wake(&mut eng_anom, 1, 600, 76.0, true);
    assert!(eng_anom.is_session_anomalous());
    let (slp_anom, _) = push_sleep_until_state(
        &mut eng_anom,
        601,
        1600,
        76.0,
        67.0,
        true,
        DetectorState::Confirmed,
    );
    out_anom.extend(slp_anom);
    assert!(
        confirmed_ts(&out_anom).is_some(),
        "Anomalous Session must still detect relative sleep onset"
    );
}

#[test]
fn slice4_tiered_gaps_and_post_sleep_1_to_1_session_splitting() {
    // Tier 1 (<= 2 min): pauses Candidate dwell in place without resetting
    let mut eng1 = default_engine();
    eng1.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let _ = push_calm_wake(&mut eng1, 1, 600, 68.0, true);
    let (_, cand_sec) = push_sleep_until_state(
        &mut eng1,
        601,
        1400,
        68.0,
        60.0,
        true,
        DetectorState::Candidate,
    );
    assert_eq!(eng1.state(), DetectorState::Candidate);

    // Inject a 90 s SignalLost gap -> ShortPause keeps Candidate in place
    let _ = eng1.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: (cand_sec + 1) * 1000,
        status: StreamStatus::SignalLost,
    }));
    let resume1_sec = cand_sec + 91;
    let gap1_out = eng1.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: resume1_sec * 1000,
        status: StreamStatus::Streaming,
    }));
    assert!(gap1_out.iter().any(|o| matches!(
        o,
        EngineOutput::GapResolved {
            tier: GapTier::ShortPause,
            ..
        }
    )));
    assert_eq!(eng1.current_session_index(), Some(1));
    assert_eq!(eng1.state(), DetectorState::Candidate);

    // Tier 2 (2-15 min): resets Candidate to Monitoring and enforces 2 min stabilization
    let _ = eng1.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: (resume1_sec + 1) * 1000,
        status: StreamStatus::SignalLost,
    }));
    let resume2_sec = resume1_sec + 361; // 6 min
    let gap2_out = eng1.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: resume2_sec * 1000,
        status: StreamStatus::Streaming,
    }));
    assert!(gap2_out.iter().any(|o| matches!(
        o,
        EngineOutput::GapResolved {
            tier: GapTier::MediumStabilize,
            ..
        }
    )));
    assert_eq!(eng1.current_session_index(), Some(1));
    assert_eq!(eng1.state(), DetectorState::Monitoring);

    // During 2 min (8 clean epochs = 120 s) stabilization after Tier 2 gap, must not re-enter Candidate
    let stab_out = push_sleep(
        &mut eng1,
        resume2_sec + 1,
        resume2_sec + 110,
        60.0,
        60.0,
        true,
    );
    assert!(!stab_out.iter().any(|o| matches!(
        o,
        EngineOutput::StateChanged {
            to: DetectorState::Candidate,
            ..
        }
    )));

    // Tier 3 (> 15 min): closes Monitoring Session and opens session 2 on resume
    let _ = eng1.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: (resume2_sec + 111) * 1000,
        status: StreamStatus::SignalLost,
    }));
    let resume3_sec = resume2_sec + 111 + 960; // 16 min
    let gap3_out = eng1.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: resume3_sec * 1000,
        status: StreamStatus::Streaming,
    }));
    assert!(gap3_out.iter().any(|o| matches!(
        o,
        EngineOutput::GapResolved {
            tier: GapTier::LongSessionClose,
            ..
        }
    )));
    assert!(gap3_out.iter().any(|o| matches!(
        o,
        EngineOutput::SessionEnded(s) if s.session_index == 1 && s.end_reason == SessionEndReason::GapExceeded
    )));
    assert_eq!(eng1.current_session_index(), Some(2));
    assert_eq!(eng1.state(), DetectorState::Warmup);

    // Sustained gap timeout (>= 15 min) without stream resume closes session to Idle
    let mut eng_idle = default_engine();
    assert_eq!(eng_idle.state(), DetectorState::Idle);
    assert_eq!(eng_idle.gap_deadline_ms(), None);
    assert!(eng_idle.check_gap_timeout(100_000).is_empty());
    assert_eq!(eng_idle.state(), DetectorState::Idle);

    // Session opened via Streaming and then disconnected without receiving packets
    let mut eng_silent = default_engine();
    eng_silent.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 10_000,
        status: StreamStatus::Streaming,
    }));
    assert_eq!(eng_silent.state(), DetectorState::Warmup);
    assert_eq!(eng_silent.gap_deadline_ms(), None);
    eng_silent.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 20_000,
        status: StreamStatus::Disconnected,
    }));
    assert_eq!(eng_silent.gap_deadline_ms(), Some(20_000 + 900_000 + 1));
    assert!(eng_silent.check_gap_timeout(20_000 + 900_000).is_empty());
    assert_eq!(eng_silent.state(), DetectorState::Warmup);
    let silent_timeout = eng_silent.check_gap_timeout(20_000 + 900_001);
    assert!(silent_timeout.iter().any(|o| matches!(
        o,
        EngineOutput::SessionEnded(s)
            if s.end_reason == SessionEndReason::GapExceeded && s.end_ts_ms == 20_000
    )));
    assert_eq!(eng_silent.state(), DetectorState::Idle);

    let mut eng_timeout = default_engine();
    eng_timeout.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let _ = push_calm_wake(&mut eng_timeout, 1, 600, 68.0, true);
    assert_eq!(eng_timeout.state(), DetectorState::Monitoring);
    assert_eq!(eng_timeout.current_session_index(), Some(1));

    // Disconnect at t = 600s (600_000 ms)
    let _ = eng_timeout.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 600_000,
        status: StreamStatus::Disconnected,
    }));
    assert_eq!(eng_timeout.gap_started_ms(), Some(600_000));
    assert_eq!(eng_timeout.gap_deadline_ms(), Some(1_500_001));

    // Check timeout after 10 minutes (1_200_000 ms = 600s after gap) -> gap NOT exceeded yet
    let no_timeout_out = eng_timeout.check_gap_timeout(1_200_000);
    assert!(no_timeout_out.is_empty());
    assert_eq!(eng_timeout.state(), DetectorState::Monitoring);
    assert_eq!(eng_timeout.current_session_index(), Some(1));

    // Check timeout at 15m 0s (1_500_000 ms) -> not exceeded yet (<= 900_000 ms)
    let at_limit_out = eng_timeout.check_gap_timeout(1_500_000);
    assert!(at_limit_out.is_empty());
    assert_eq!(eng_timeout.state(), DetectorState::Monitoring);

    // Check timeout after 15 minutes and 1 ms (1_500_001 ms > 900s) -> closes session to Idle
    let timeout_out = eng_timeout.check_gap_timeout(1_500_001);
    assert!(timeout_out.iter().any(|o| matches!(
        o,
        EngineOutput::GapResolved {
            tier: GapTier::LongSessionClose,
            ..
        }
    )));
    assert!(timeout_out.iter().any(|o| matches!(
        o,
        EngineOutput::SessionEnded(s)
            if s.session_index == 1
                && s.end_reason == SessionEndReason::GapExceeded
                && s.end_ts_ms == 600_000
    )));
    assert_eq!(eng_timeout.state(), DetectorState::Idle);
    assert_eq!(eng_timeout.current_session_index(), Some(1));
    assert_eq!(eng_timeout.gap_started_ms(), None);
    assert_eq!(eng_timeout.gap_deadline_ms(), None);

    // Post-sleep 1:1 split on sustained wakefulness (>= 10 min with P(SleepNREM) < 0.30)
    let mut eng_split = default_engine();
    eng_split.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let _ = push_calm_wake(&mut eng_split, 1, 600, 68.0, true);
    let (out_s1, tc_sec) = push_sleep_until_state(
        &mut eng_split,
        601,
        1400,
        68.0,
        60.0,
        true,
        DetectorState::Confirmed,
    );
    let tc = confirmed_ts(&out_s1).expect("Confirmed");
    eng_split.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: tc + 30_000,
        outcome: GraceResult::Expired,
    }));
    assert_eq!(eng_split.state(), DetectorState::Sleeping);
    assert_eq!(eng_split.current_session_index(), Some(1));

    // User wakes up and stays awake >= 10 min at 74 bpm
    let split_out = push_calm_wake(&mut eng_split, tc_sec + 35, tc_sec + 950, 74.0, true);
    assert!(split_out.iter().any(|o| matches!(
        o,
        EngineOutput::SessionEnded(s) if s.session_index == 1 && s.end_reason == SessionEndReason::SustainedWakefulness
    )));
    assert_eq!(eng_split.current_session_index(), Some(2));

    // Path B: DesktopActivity + HR rebound splits session immediately
    let mut eng_desk = default_engine();
    eng_desk.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let _ = push_calm_wake(&mut eng_desk, 1, 600, 68.0, true);
    let (out_d, td_sec) = push_sleep_until_state(
        &mut eng_desk,
        601,
        1400,
        68.0,
        60.0,
        true,
        DetectorState::Confirmed,
    );
    let tcd = confirmed_ts(&out_d).expect("Confirmed");
    eng_desk.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: tcd + 30_000,
        outcome: GraceResult::Expired,
    }));
    assert_eq!(eng_desk.state(), DetectorState::Sleeping);
    let mut rebound_out = push_calm_wake(&mut eng_desk, td_sec + 35, td_sec + 190, 72.0, true);
    rebound_out.extend(
        eng_desk.handle_event(EngineEvent::DesktopActivity(DesktopActivity {
            ts_ms: (td_sec + 191) * 1000,
            kind: DesktopActivityKind::UserInput,
        })),
    );
    assert!(rebound_out.iter().any(|o| matches!(
        o,
        EngineOutput::SessionEnded(s) if s.session_index == 1 && s.end_reason == SessionEndReason::DesktopWakeRebound
    )));
    assert_eq!(eng_desk.current_session_index(), Some(2));

    // Post-filter feed_epoch_features matches Epoch stream seamlessly
    let mut eng_direct = default_engine();
    eng_direct.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let mut direct_eps = Vec::new();
    for o in push_calm_wake(&mut eng_direct, 1, 520, 68.0, true) {
        if let EngineOutput::EpochEmitted(ep) = o {
            direct_eps.push(ep);
        }
    }
    let mut eng_fe = default_engine();
    let mut replayed_eps = Vec::new();
    for ep in &direct_eps {
        for o in eng_fe.feed_epoch_features(
            ep.ts_ms,
            ep.features.clone(),
            ep.hrv_valid,
            ep.coverage,
            ep.flatline_stale,
        ) {
            if let EngineOutput::EpochEmitted(r) = o {
                replayed_eps.push(r);
            }
        }
    }
    assert_eq!(eng_fe.state(), eng_direct.state());
    assert_eq!(replayed_eps.len(), direct_eps.len());
}

fn run_trace_collecting_confirmed(
    events: &[EngineEvent],
    priors: CalibrationPriors,
) -> (Vec<i64>, Vec<Epoch>, Vec<EngineOutput>) {
    let mut engine = SleepEngine::new(EngineConfig {
        detection: DetectionConfig::default(),
        priors,
        wearable_floor: WearableFloor::default(),
    });
    let mut confirmed = Vec::new();
    let mut epochs = Vec::new();
    let mut all_out = Vec::new();

    for ev in events {
        let outputs = engine.handle_event(ev.clone());
        let mut trigger_expire_at = None;
        for o in &outputs {
            match o {
                EngineOutput::EpochEmitted(ep) => epochs.push(ep.clone()),
                EngineOutput::StateChanged {
                    ts_ms,
                    to: DetectorState::Confirmed,
                    ..
                } => {
                    confirmed.push(*ts_ms);
                    trigger_expire_at = Some(*ts_ms);
                }
                _ => {}
            }
        }
        all_out.extend(outputs);
        if let Some(ts) = trigger_expire_at {
            // Simulate immediate grace expiry for multi-onset 1:1 split tests
            all_out.extend(engine.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
                ts_ms: ts,
                outcome: GraceResult::Expired,
            })));
        }
    }
    (confirmed, epochs, all_out)
}

#[test]
fn tier1_sleep_positives_paired_parity_and_slow_gradual_descent() {
    use common::synthetic::{Phenotype, StageKind, generate_paired_trace, inject_rr_corruption};

    let scenarios = [
        (
            101_u64,
            Phenotype::typical(),
            CalibrationPriors::cold_start(),
            "cold_typical",
        ),
        (
            202_u64,
            Phenotype::athletic_low_rhr(),
            CalibrationPriors {
                maturity: CalibrationMaturity::Learning,
                mu_wake: Some(50.0),
                wake_median: None,
                sigma_wake: Some(2.1),
                mu_sleep: Some(44.0),
                delta_hr_onset: Some(-6.0),
                mu_ln_rmssd_sleep: Some(4.2),
                tau_p_sleep_offset: 0.0,
                dwell_adjust_sec: 0,
                wake_mad_s: Some(2.2),
            },
            "learning_athletic",
        ),
        (
            303_u64,
            Phenotype::high_rhr(),
            CalibrationPriors {
                maturity: CalibrationMaturity::Calibrated,
                mu_wake: Some(80.0),
                wake_median: None,
                sigma_wake: Some(2.3),
                mu_sleep: Some(71.0),
                delta_hr_onset: Some(-8.5),
                mu_ln_rmssd_sleep: Some(3.3),
                tau_p_sleep_offset: 0.0,
                dwell_adjust_sec: 0,
                wake_mad_s: Some(2.4),
            },
            "calibrated_high_rhr",
        ),
    ];

    for (seed, pheno, priors, label) in scenarios {
        let trace = generate_paired_trace(
            seed,
            pheno,
            &[
                StageKind::QuietWake { duration_sec: 1200 }, // 20 min calm wake
                StageKind::N1Onset { duration_sec: 480 },    // 8 min N1 descent
                StageKind::N2Sleep { duration_sec: 1500 },   // 25 min consolidated N2
            ],
        );
        let onset_ms = trace.onset_sec.unwrap() * 1000;

        let (conf_rr, eps_rr, _) = run_trace_collecting_confirmed(&trace.bpm_rr, priors.clone());
        let (conf_bpm, eps_bpm, _) = run_trace_collecting_confirmed(&trace.bpm_only_gt5, priors);

        assert_eq!(
            conf_rr.len(),
            1,
            "{label} BPM+RR expected 1 onset, got {:?}",
            conf_rr
        );
        assert_eq!(
            conf_bpm.len(),
            1,
            "{label} BPM-only expected 1 onset, got {:?}",
            conf_bpm
        );

        let lat_rr_min = (conf_rr[0] - onset_ms) as f64 / 60_000.0;
        let lat_bpm_min = (conf_bpm[0] - onset_ms) as f64 / 60_000.0;
        assert!(
            (3.0..=15.0).contains(&lat_rr_min),
            "{label} BPM+RR latency {lat_rr_min:.2} min outside [+3, +15] min (P_end={:.3})",
            eps_rr.last().unwrap().p_sleep
        );
        assert!(
            (5.0..=18.0).contains(&lat_bpm_min),
            "{label} BPM-only latency {lat_bpm_min:.2} min outside [+5, +18] min (P_end={:.3})",
            eps_bpm.last().unwrap().p_sleep
        );

        let paired_diff_min = lat_bpm_min - lat_rr_min;
        assert!(
            (0.0..=4.0).contains(&paired_diff_min),
            "{label} paired latency gap {paired_diff_min:.2} min outside +2 +/- 2 min (rr={lat_rr_min:.2}, bpm={lat_bpm_min:.2}, cand_rr_p={:.3}, cand_bpm_p={:.3})",
            eps_rr
                .iter()
                .find(|e| e.ts_ms >= conf_rr[0] - 180_000)
                .map(|e| e.p_sleep)
                .unwrap_or(0.0),
            eps_bpm
                .iter()
                .find(|e| e.ts_ms >= conf_bpm[0] - 300_000)
                .map(|e| e.p_sleep)
                .unwrap_or(0.0),
        );
    }

    // Slow gradual descent (18 min ramp -> N2) verifying baseline freezes at P(SleepNREM) >= 0.30
    let slow_trace = generate_paired_trace(
        404,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 1080 }, // very slow 18 min descent
            StageKind::N2Sleep { duration_sec: 1200 },
        ],
    );
    let slow_onset_ms = slow_trace.onset_sec.unwrap() * 1000;
    let (conf_slow, eps_slow, _) =
        run_trace_collecting_confirmed(&slow_trace.bpm_rr, CalibrationPriors::cold_start());
    assert_eq!(conf_slow.len(), 1, "slow gradual descent must still detect");
    let freeze_ep = eps_slow
        .iter()
        .find(|e| e.ts_ms > slow_onset_ms && e.p_sleep >= 0.30)
        .expect("must freeze baseline at P >= 0.30");
    assert!(freeze_ep.baseline_frozen);

    // Mid-session > 5% RR corruption -> |ΔP| <= 0.15 marginalization parity and still detects
    let corrupted_rr = inject_rr_corruption(
        &slow_trace.bpm_rr,
        slow_onset_ms + 120_000,
        slow_onset_ms + 900_000,
        5, // 20% RR artifacts
    );
    let (conf_corr, eps_corr, _) =
        run_trace_collecting_confirmed(&corrupted_rr, CalibrationPriors::cold_start());
    assert_eq!(
        conf_corr.len(),
        1,
        "corrupted RR must fall back seamlessly to BPM-only"
    );
    for w in eps_corr.windows(2) {
        if w[0].hrv_valid && !w[1].hrv_valid {
            let dp = (w[1].p_sleep - w[0].p_sleep).abs();
            assert!(
                dp <= 0.15,
                "mid-session HRV dropout |ΔP| = {dp:.3} exceeded 0.15"
            );
        }
    }

    // Mid-night wake + second onset (1:1 Monitoring Session split)
    let split_trace = generate_paired_trace(
        505,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 360 },
            StageKind::N2Sleep { duration_sec: 900 },
            StageKind::PostSleepWake {
                duration_sec: 1200,
                elevation_bpm: 6.0,
            }, // 20 min wake -> splits session 1 and enters session 2
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 360 },
            StageKind::N2Sleep { duration_sec: 900 },
        ],
    );
    let (conf_split, _, out_split) =
        run_trace_collecting_confirmed(&split_trace.bpm_rr, CalibrationPriors::cold_start());
    assert_eq!(
        conf_split.len(),
        2,
        "mid-night wake + second onset must produce 2 confirmed onsets across 2 sessions"
    );
    assert!(out_split.iter().any(|o| matches!(
        o,
        EngineOutput::SessionEnded(s)
            if s.session_index == 1 && s.end_reason == SessionEndReason::SustainedWakefulness
    )));
}

#[test]
fn tier1_hard_wake_negatives_zero_false_positives() {
    use common::synthetic::{Phenotype, StageKind, generate_paired_trace};

    let wake_scenarios = [
        (
            "motionless_movie_110min",
            vec![
                StageKind::QuietWake { duration_sec: 6600 }, // 110 min motionless movie
            ],
        ),
        (
            "post_workout_22bpm_recovery_then_wake",
            vec![
                StageKind::PostWorkoutCooldown {
                    duration_sec: 900,
                    initial_excess_bpm: 22.0,
                },
                StageKind::QuietWake { duration_sec: 3600 }, // 60 min normal wake afterward
            ],
        ),
        (
            "brief_drowsy_dips_arousing_before_dwell",
            vec![
                StageKind::QuietWake { duration_sec: 900 },
                StageKind::DrowsyDip {
                    duration_sec: 150,
                    dip_bpm: 4.5,
                },
                StageKind::QuietWake { duration_sec: 900 },
                StageKind::DrowsyDip {
                    duration_sec: 180,
                    dip_bpm: 5.0,
                },
                StageKind::QuietWake { duration_sec: 1800 },
            ],
        ),
        (
            "active_desk_spikes_excluded_from_baseline",
            vec![
                StageKind::QuietWake { duration_sec: 900 },
                StageKind::ActiveWakeSpike {
                    duration_sec: 180,
                    elevation_bpm: 18.0, // > +15%
                },
                StageKind::QuietWake { duration_sec: 1800 },
                StageKind::ActiveWakeSpike {
                    duration_sec: 120,
                    elevation_bpm: 22.0,
                },
                StageKind::QuietWake { duration_sec: 1800 },
            ],
        ),
    ];

    for seed in [11_u64, 29_u64, 47_u64] {
        for (name, stages) in &wake_scenarios {
            let trace = generate_paired_trace(seed, Phenotype::typical(), stages);
            for (mode, events) in [("BPM+RR", &trace.bpm_rr), ("BPM-only", &trace.bpm_only_gt5)] {
                let (conf, eps, _) =
                    run_trace_collecting_confirmed(events, CalibrationPriors::cold_start());
                assert!(
                    conf.is_empty(),
                    "wake-negative FP in {name} ({mode}, seed {seed}): confirmed at {:?} (max P={:.3}, warmup_end={:?}, at_conf={:?})",
                    conf,
                    eps.iter().map(|e| e.p_sleep).fold(0.0_f64, f64::max),
                    eps.iter()
                        .find(|e| e.state == DetectorState::Monitoring)
                        .map(|e| (
                            e.ts_ms,
                            e.baseline_mu_hr,
                            e.baseline_sigma_hr,
                            e.features.hr_bpm_60s,
                            e.p_sleep
                        )),
                    eps.iter()
                        .find(|e| e.ts_ms >= conf[0] - 300_000 && e.p_sleep >= 0.30)
                        .map(|e| (
                            e.ts_ms,
                            e.baseline_mu_hr,
                            e.baseline_sigma_hr,
                            e.features.hr_bpm_60s,
                            e.features.z_hr,
                            e.features.r_var_120s,
                            e.features.hr_slope_4m,
                            e.p_sleep
                        ))
                );
            }
        }
    }
}

#[test]
fn tier1_gt5_optical_lock_ramp_flatlines_hampel_and_silent_gaps() {
    use common::synthetic::{
        Phenotype, StageKind, generate_paired_trace, inject_flatline, inject_gt5_optical_lock_ramp,
        inject_hampel_spikes, inject_silent_gap,
    };

    let base = generate_paired_trace(
        777,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep { duration_sec: 1500 },
        ],
    );

    // 1) GT5 ~9.3 s optical lock ramp + Hampel motion spikes
    let gt5_ramp = inject_gt5_optical_lock_ramp(&base.bpm_only_gt5, 9_300);
    let gt5_spiked = inject_hampel_spikes(&gt5_ramp, &[120, 450, 1000, 1150], 60.0);
    let (conf_gt5, _, _) =
        run_trace_collecting_confirmed(&gt5_spiked, CalibrationPriors::cold_start());
    assert_eq!(
        conf_gt5.len(),
        1,
        "GT5 ramp + Hampel spikes must detect sleep cleanly"
    );

    // 2) 60 s N2 plateau (NO false flatline) vs 130 s frozen BPM-only flatline vs 35 s frozen BPM+RR flatline
    let plateau_60s = inject_flatline(&base.bpm_only_gt5, 1_600_000, 1_660_000, false);
    let (_, eps_plat, _) =
        run_trace_collecting_confirmed(&plateau_60s, CalibrationPriors::cold_start());
    assert!(
        eps_plat
            .iter()
            .filter(|e| e.ts_ms >= 1_600_000 && e.ts_ms <= 1_660_000)
            .all(|e| !e.flatline_stale),
        "60 s BPM-only plateau must not flag flatline_stale"
    );

    let frozen_130s = inject_flatline(&base.bpm_only_gt5, 1_600_000, 1_735_000, false);
    let (_, eps_f130, _) =
        run_trace_collecting_confirmed(&frozen_130s, CalibrationPriors::cold_start());
    assert!(
        eps_f130
            .iter()
            .any(|e| e.ts_ms >= 1_720_000 && e.flatline_stale),
        "130 s BPM-only flatline must flag flatline_stale"
    );

    let frozen_rr_35s = inject_flatline(&base.bpm_rr, 1_600_000, 1_640_000, true);
    let (_, eps_frr, _) =
        run_trace_collecting_confirmed(&frozen_rr_35s, CalibrationPriors::cold_start());
    assert!(
        eps_frr
            .iter()
            .any(|e| e.ts_ms >= 1_630_000 && e.flatline_stale),
        "35 s BPM+RR flatline must flag flatline_stale"
    );

    // 3) Silent 3-tier gaps without explicit StreamState (emulating GT5 silent off-wrist halt)
    let g1 = inject_silent_gap(&base.bpm_only_gt5, 700_000, 760_000, false); // 60 s <= 2 min
    let (_, _, out_g1) = run_trace_collecting_confirmed(&g1, CalibrationPriors::cold_start());
    assert!(out_g1.iter().any(|o| matches!(
        o,
        EngineOutput::GapResolved {
            tier: GapTier::ShortPause,
            ..
        }
    )));

    let g2 = inject_silent_gap(&base.bpm_only_gt5, 700_000, 1_000_000, false); // 5 min in 2-15 min
    let (_, _, out_g2) = run_trace_collecting_confirmed(&g2, CalibrationPriors::cold_start());
    assert!(out_g2.iter().any(|o| matches!(
        o,
        EngineOutput::GapResolved {
            tier: GapTier::MediumStabilize,
            ..
        }
    )));

    let g3 = inject_silent_gap(&base.bpm_only_gt5, 700_000, 1_700_000, false); // 16.6 min > 15 min
    let (_, _, out_g3) = run_trace_collecting_confirmed(&g3, CalibrationPriors::cold_start());
    assert!(out_g3.iter().any(|o| matches!(
        o,
        EngineOutput::GapResolved {
            tier: GapTier::LongSessionClose,
            ..
        }
    )));
    assert!(out_g3.iter().any(|o| matches!(
        o,
        EngineOutput::SessionEnded(s) if s.end_reason == SessionEndReason::GapExceeded
    )));
}

#[test]
fn review_regression_desktop_candidate_cancel_grace_outcomes_single_gap_and_rr_drop_flatline() {
    // 1) DesktopActivity cancels Candidate and unfreezes baseline in Monitoring;
    //    GraceResult::AbortedNoPenalty returns to Monitoring with 0 FP and no cooldown;
    //    GraceResult::PostTriggerRapidWake returns Sleeping -> Monitoring with 15 min cooldown + FP.
    let mut eng = default_engine();
    eng.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let _ = push_calm_wake(&mut eng, 1, 600, 68.0, true);
    let (_, cand_sec) = push_sleep_until_state(
        &mut eng,
        601,
        1400,
        68.0,
        60.0,
        true,
        DetectorState::Candidate,
    );
    assert_eq!(eng.state(), DetectorState::Candidate);

    let desk_out = eng.handle_event(EngineEvent::DesktopActivity(DesktopActivity {
        ts_ms: (cand_sec + 1) * 1000,
        kind: DesktopActivityKind::UserInput,
    }));
    assert_eq!(eng.state(), DetectorState::Monitoring);
    assert!(desk_out.iter().any(|o| matches!(
        o,
        EngineOutput::StateChanged {
            from: DetectorState::Candidate,
            to: DetectorState::Monitoring,
            ..
        }
    )));

    // Push back to Confirmed, then test AbortedNoPenalty -> 0 FP, no cooldown
    let (_, conf_sec) = push_sleep_until_state(
        &mut eng,
        cand_sec + 2,
        cand_sec + 800,
        60.0,
        59.0,
        true,
        DetectorState::Confirmed,
    );
    let abort_out = eng.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: (conf_sec + 5) * 1000,
        outcome: GraceResult::AbortedNoPenalty,
    }));
    assert_eq!(eng.state(), DetectorState::Monitoring);
    assert!(
        !abort_out
            .iter()
            .any(|o| matches!(o, EngineOutput::FalsePositiveRecorded { .. }))
    );

    // Confirm again -> Expired -> Sleeping -> PostTriggerRapidWake -> Monitoring + FP + cooldown
    let (_, conf2_sec) = push_sleep_until_state(
        &mut eng,
        conf_sec + 6,
        conf_sec + 800,
        59.0,
        58.0,
        true,
        DetectorState::Confirmed,
    );
    eng.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: (conf2_sec + 30) * 1000,
        outcome: GraceResult::Expired,
    }));
    assert_eq!(eng.state(), DetectorState::Sleeping);
    let rapid_out = eng.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: (conf2_sec + 60) * 1000,
        outcome: GraceResult::PostTriggerRapidWake,
    }));
    assert_eq!(eng.state(), DetectorState::Monitoring);
    assert!(
        rapid_out
            .iter()
            .any(|o| matches!(o, EngineOutput::FalsePositiveRecorded { counted: true, .. }))
    );

    // 2) Single GapResolved emission: StreamState(SignalLost) + StreamState(Streaming) + next HrPacket
    //    must emit GapResolved exactly ONCE (no double-counting on next HrPacket).
    let mut eng_gap = default_engine();
    eng_gap.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let _ = push_calm_wake(&mut eng_gap, 1, 60, 68.0, false);
    eng_gap.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 61_000,
        status: StreamStatus::SignalLost,
    }));
    let g_stream = eng_gap.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 140_000,
        status: StreamStatus::Streaming,
    }));
    let g_pkt = eng_gap.handle_event(EngineEvent::HrPacket(HrPacket {
        ts_ms: 141_000,
        bpm: 68.0,
        rr_ms: vec![],
        contact_status: ContactStatus::Contact,
    }));
    let resolved_count = g_stream
        .iter()
        .chain(g_pkt.iter())
        .filter(|o| matches!(o, EngineOutput::GapResolved { .. }))
        .count();
    assert_eq!(resolved_count, 1, "expected exactly 1 GapResolved event");

    // 3) BPM+RR -> BPM-only transition: dropping rr_ms must reset 30 s BPM+RR flatline state
    //    and enforce the 120 s BPM-only threshold (no false flatline at 35 s).
    let mut eng_switch = default_engine();
    eng_switch.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    for sec in 1..=25_i64 {
        eng_switch.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm: 60.0,
            rr_ms: vec![1000.0],
            contact_status: ContactStatus::Contact,
        }));
    }
    let mut switch_eps = Vec::new();
    for sec in 26..=75_i64 {
        for o in eng_switch.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm: 60.0,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        })) {
            if let EngineOutput::EpochEmitted(ep) = o {
                switch_eps.push(ep);
            }
        }
    }
    assert!(
        switch_eps.iter().all(|ep| !ep.flatline_stale),
        "steady BPM for 50 s after dropping RR must not trigger 30 s BPM+RR flatline"
    );
}

#[test]
fn tier1_calibrated_and_cold_start_post_workout_recovery_zero_false_positives() {
    use common::synthetic::{Phenotype, StageKind, generate_paired_trace};

    let calibrated_priors = CalibrationPriors {
        maturity: CalibrationMaturity::Calibrated,
        mu_wake: Some(68.0),
        wake_median: None,
        sigma_wake: Some(2.2),
        mu_sleep: Some(60.0),
        delta_hr_onset: Some(-8.0),
        mu_ln_rmssd_sleep: Some(3.75),
        tau_p_sleep_offset: 0.0,
        dwell_adjust_sec: 0,
        wake_mad_s: Some(2.3),
    };

    for (label, priors) in [
        ("cold_start", CalibrationPriors::cold_start()),
        ("calibrated", calibrated_priors),
    ] {
        for (seed, dur) in [(17_u64, 900_i64), (83_u64, 1080_i64)] {
            let trace = generate_paired_trace(
                seed,
                Phenotype::typical(),
                &[
                    StageKind::PostWorkoutCooldown {
                        duration_sec: dur,
                        initial_excess_bpm: 22.0,
                    },
                    StageKind::QuietWake { duration_sec: 3600 },
                ],
            );
            for (mode, events) in [("BPM+RR", &trace.bpm_rr), ("BPM-only", &trace.bpm_only_gt5)] {
                let (conf, _, _) = run_trace_collecting_confirmed(events, priors.clone());
                assert!(
                    conf.is_empty(),
                    "post-workout recovery FP in {label} ({mode}, seed {seed}, dur {dur}s): {:?}",
                    conf
                );
            }
        }
    }
}

fn push_rsa_sleep(
    engine: &mut SleepEngine,
    start_sec: i64,
    end_sec: i64,
    target_bpm: f64,
    rsa_amp_ms: f64,
) -> Vec<EngineOutput> {
    let mut out = Vec::new();
    for sec in start_sec..=end_sec {
        let bpm = target_bpm + 0.3 * (sec as f64 * 0.4).sin();
        let phase = (sec as f64) * std::f64::consts::TAU / 6.0;
        let rr_ms = vec![60_000.0 / bpm + rsa_amp_ms * phase.sin()];
        out.extend(engine.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm,
            rr_ms,
            contact_status: ContactStatus::Contact,
        })));
    }
    out
}

fn enter_sleeping_engine() -> (SleepEngine, i64) {
    let mut eng = default_engine();
    eng.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let _ = push_calm_wake(&mut eng, 1, 600, 68.0, true);
    let (_, t_conf_sec) = push_sleep_until_state(
        &mut eng,
        601,
        1800,
        68.0,
        56.0,
        true,
        DetectorState::Confirmed,
    );
    assert_eq!(eng.state(), DetectorState::Confirmed);
    let g_out = eng.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: (t_conf_sec + 1) * 1000,
        outcome: GraceResult::Expired,
    }));
    assert!(g_out.iter().any(|o| matches!(
        o,
        EngineOutput::StateChanged {
            to: DetectorState::Sleeping,
            ..
        }
    )));
    (eng, t_conf_sec + 2)
}

fn extract_session_ended(outputs: &[EngineOutput]) -> &bpmute::sleep_engine::SessionSummary {
    outputs
        .iter()
        .find_map(|o| match o {
            EngineOutput::SessionEnded(s) => Some(s),
            _ => None,
        })
        .expect("Monitoring Session must emit SessionEnded")
}

fn push_wake_arousal(
    engine: &mut SleepEngine,
    start_sec: i64,
    max_end_sec: i64,
    base_bpm: f64,
) -> (Vec<EngineOutput>, i64) {
    let mut out = Vec::new();
    for sec in start_sec..=max_end_sec {
        let bpm = base_bpm + 8.0 * (sec as f64 * 0.35).sin();
        let phase = (sec as f64) * std::f64::consts::TAU / 6.0;
        let rr_ms = vec![60_000.0 / bpm + 4.0 * phase.sin()];
        let step_out = engine.handle_event(EngineEvent::HrPacket(HrPacket {
            ts_ms: sec * 1000,
            bpm,
            rr_ms,
            contact_status: ContactStatus::Contact,
        }));
        let ended = step_out
            .iter()
            .any(|o| matches!(o, EngineOutput::SessionEnded(_)));
        out.extend(step_out);
        if ended {
            return (out, sec);
        }
    }
    (out, max_end_sec)
}

#[test]
fn wake_tail_excluded_on_sustained_wakefulness_session_close() {
    use common::synthetic::{Phenotype, StageKind, generate_paired_trace};

    // Real physiological BPM+RR trace: 15 min QuietWake -> 7 min N1 -> 12 min N2 (~60 bpm) -> 12 min PostSleepWake (~86 bpm).
    let pheno = Phenotype::typical();
    let trace = generate_paired_trace(
        42,
        pheno,
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep { duration_sec: 720 },
            StageKind::PostSleepWake {
                duration_sec: 900,
                elevation_bpm: 18.0,
            },
        ],
    );

    let mut eng = default_engine();
    let mut summary = None;
    let mut grace_delivered = false;
    for ev in &trace.bpm_rr {
        let out = eng.handle_event(ev.clone());
        for o in &out {
            if let EngineOutput::StateChanged {
                ts_ms,
                to: DetectorState::Confirmed,
                ..
            } = o
                && !grace_delivered
            {
                grace_delivered = true;
                let _ = eng.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
                    ts_ms: ts_ms + 1_000,
                    outcome: GraceResult::Expired,
                }));
            }
            if let EngineOutput::SessionEnded(s) = o {
                summary = Some(s.clone());
            }
        }
        if summary.is_some() {
            break;
        }
    }

    let summary = summary.expect("PostSleepWake must close the Monitoring Session");
    assert_eq!(summary.end_reason, SessionEndReason::SustainedWakefulness);
    let sleep_mu = summary.sleep_mu_hr.expect("sleep_mu_hr must be present");
    let target_sleep_hr = pheno.quiet_wake_hr - pheno.sleep_drop_bpm;
    assert!(
        (target_sleep_hr - 2.0..=target_sleep_hr + 2.5).contains(&sleep_mu),
        "trailing PostSleepWake tail must be excluded from sleep_mu_hr (expected ~{target_sleep_hr:.1}, got {sleep_mu:.2})",
    );
    let var_ratio = summary
        .observed_hr_var_ratio
        .expect("observed_hr_var_ratio must be present");
    assert!(
        var_ratio < 0.65,
        "trailing wake tail must not inflate observed_hr_var_ratio (got {var_ratio:.3})"
    );
    let ln_rmssd = summary
        .sleep_ln_rmssd
        .expect("sleep_ln_rmssd must be present");
    assert!(
        ln_rmssd > 3.2,
        "trailing wake tail must not depress sleep_ln_rmssd (got {ln_rmssd:.3})"
    );
}

#[test]
fn wake_tail_rejoined_on_sleep_resume_and_excluded_on_gap_close() {
    let (mut eng, t0) = enter_sleeping_engine();

    // 3 min sleep (56 bpm) -> 4 min light/excursion (67 bpm, p_sleep < 0.30) -> 3 min sleep (56 bpm)
    // -> 5 min wake tail (88 bpm) -> >15 min disconnection (GapExceeded).
    let t1 = t0 + 180;
    let _ = push_rsa_sleep(&mut eng, t0, t1, 56.0, 25.0);
    let t2 = t1 + 240;
    let exc_out = push_rsa_sleep(&mut eng, t1 + 1, t2, 67.0, 20.0);
    assert!(
        exc_out.iter().any(|o| matches!(
            o,
            EngineOutput::EpochEmitted(ep) if ep.p_sleep < 0.30
        )),
        "mid-sleep 67 bpm excursion must drop below 0.30"
    );
    let t3 = t2 + 180;
    let _ = push_rsa_sleep(&mut eng, t2 + 1, t3, 56.0, 25.0);
    let t4 = t3 + 300;
    let _ = push_rsa_sleep(&mut eng, t3 + 1, t4, 88.0, 4.0);

    eng.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: (t4 + 1) * 1000,
        status: StreamStatus::Disconnected,
    }));
    let gap_out = eng.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: (t4 + 960) * 1000,
        status: StreamStatus::Streaming,
    }));
    let gap_summary = extract_session_ended(&gap_out);
    assert_eq!(gap_summary.end_reason, SessionEndReason::GapExceeded);
    let gap_mu = gap_summary.sleep_mu_hr.unwrap();
    assert!(
        (56.5..=67.5).contains(&gap_mu),
        "rejoined mid-sleep excursion must lift median above 56.5 while final 88 bpm wake tail is dropped (got {gap_mu:.2})"
    );
}

#[test]
fn wake_tail_excluded_on_rebound_and_promoted_when_committed_empty() {
    let (mut eng, s_start) = enter_sleeping_engine();
    let s_end = s_start + 300;
    let _ = push_rsa_sleep(&mut eng, s_start, s_end, 56.0, 25.0);
    eng.handle_event(EngineEvent::DesktopActivity(DesktopActivity {
        ts_ms: (s_end + 1) * 1000,
        kind: DesktopActivityKind::UserInput,
    }));
    let (reb_out, _) = push_wake_arousal(&mut eng, s_end + 2, s_end + 400, 88.0);
    let reb_summary = extract_session_ended(&reb_out);
    assert_eq!(reb_summary.end_reason, SessionEndReason::DesktopWakeRebound);
    let reb_mu = reb_summary.sleep_mu_hr.unwrap();
    assert!(
        (54.5..=57.5).contains(&reb_mu),
        "DesktopWakeRebound must exclude trailing wake tail from sleep_mu_hr (got {reb_mu:.2})"
    );
    assert!(
        reb_summary.observed_hr_var_ratio.unwrap() < 0.65,
        "DesktopWakeRebound must exclude trailing wake tail from observed_hr_var_ratio"
    );

    // Fallback: if Sleeping closes with only p_sleep < 0.30 wake_tail epochs (empty committed buffer),
    // finalize_medians promotes wake_tail rather than returning None.
    use bpmute::sleep_engine::EpochFeatures;
    let mut eng_fb = default_engine();
    eng_fb.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let mut ts = 15_000_i64;
    let wake_feat = EpochFeatures {
        hr_bpm_60s: 68.0,
        z_hr: 0.0,
        hr_slope_4m: 0.0,
        r_var_120s: 0.95,
        ln_rmssd_60s: None,
        delta_ln_rmssd_60s: None,
        rmssd_sdnn_120s: None,
    };
    for _ in 0..32 {
        eng_fb.feed_epoch_features(ts, wake_feat.clone(), false, 1.0, false);
        ts += 15_000;
    }
    let sleep_feat = EpochFeatures {
        hr_bpm_60s: 54.0,
        z_hr: -2.6,
        hr_slope_4m: -0.3,
        r_var_120s: 0.35,
        ln_rmssd_60s: None,
        delta_ln_rmssd_60s: None,
        rmssd_sdnn_120s: None,
    };
    while eng_fb.state() != DetectorState::Confirmed {
        eng_fb.feed_epoch_features(ts, sleep_feat.clone(), false, 1.0, false);
        ts += 15_000;
    }
    eng_fb.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
        ts_ms: ts,
        outcome: GraceResult::Expired,
    }));
    assert_eq!(eng_fb.state(), DetectorState::Sleeping);

    let high_wake_feat = EpochFeatures {
        hr_bpm_60s: 88.0,
        z_hr: 5.0,
        hr_slope_4m: 0.8,
        r_var_120s: 1.8,
        ln_rmssd_60s: None,
        delta_ln_rmssd_60s: None,
        rmssd_sdnn_120s: None,
    };
    let mut fb_summary = None;
    for _ in 0..45 {
        ts += 15_000;
        for o in eng_fb.feed_epoch_features(ts, high_wake_feat.clone(), false, 1.0, false) {
            if let EngineOutput::SessionEnded(s) = o {
                fb_summary = Some(s);
            }
        }
        if fb_summary.is_some() {
            break;
        }
    }
    let fb_summary = fb_summary.expect("SustainedWakefulness must close fallback session");
    assert!(
        fb_summary.sleep_mu_hr.is_some_and(|hr| hr > 80.0),
        "empty committed buffer must promote wake_tail fallback on close"
    );
}

#[test]
fn bpm_only_low_rvar_above_baseline_does_not_spike_to_sleep() {
    use bpmute::sleep_engine::EpochFeatures;

    let mut eng = default_engine();
    eng.handle_event(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));

    let mut ts = 15_000_i64;
    let wake_feat = EpochFeatures {
        hr_bpm_60s: 64.0,
        z_hr: 0.0,
        hr_slope_4m: 0.0,
        r_var_120s: 0.90,
        ln_rmssd_60s: None,
        delta_ln_rmssd_60s: None,
        rmssd_sdnn_120s: None,
    };
    for _ in 0..32 {
        eng.feed_epoch_features(ts, wake_feat.clone(), false, 1.0, false);
        ts += 15_000;
    }
    assert_eq!(eng.state(), DetectorState::Monitoring);

    // Exact Watch GT5 failure point: elevated HR above Quiet-Wake Baseline (z_hr = +1.35)
    // paired with low firmware-smoothed variability (r_var_120s = 0.38, BPM-only).
    let low_rvar_above_baseline = EpochFeatures {
        hr_bpm_60s: 66.7,
        z_hr: 1.35,
        hr_slope_4m: 0.04,
        r_var_120s: 0.38,
        ln_rmssd_60s: None,
        delta_ln_rmssd_60s: None,
        rmssd_sdnn_120s: None,
    };
    for _ in 0..10 {
        let out = eng.feed_epoch_features(ts, low_rvar_above_baseline.clone(), false, 1.0, false);
        ts += 15_000;
        let ep = out
            .iter()
            .find_map(|o| match o {
                EngineOutput::EpochEmitted(e) => Some(e),
                _ => None,
            })
            .expect("epoch emitted");
        assert!(
            ep.p_sleep < 0.50,
            "low r_var with z_hr=+1.35 must not spike p_sleep >= 0.50 (got {:.3})",
            ep.p_sleep
        );
        assert_eq!(eng.state(), DetectorState::Monitoring);
    }
}

#[test]
fn rem_cycle_quiet_wake_plateau_does_not_split_sleeping_session() {
    use common::synthetic::{Phenotype, StageKind, generate_paired_trace};

    let pheno = Phenotype::typical(); // quiet_wake_hr = 66.0, sleep_drop_bpm = 7.5
    let trace = generate_paired_trace(
        777,
        pheno,
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep { duration_sec: 900 },
            // 25-min REM / light-sleep plateau slightly above quiet_wake_hr (z_hr < 2.0)
            StageKind::PostSleepWake {
                duration_sec: 1500,
                elevation_bpm: 2.0,
            },
            StageKind::N2Sleep { duration_sec: 900 },
            // 15-min real morning active wake-up
            StageKind::PostSleepWake {
                duration_sec: 900,
                elevation_bpm: 16.0,
            },
        ],
    );

    let mut eng = default_engine();
    let mut confirmed_count = 0_usize;
    let mut rem_quiet_wake_epochs = 0_usize;
    let mut closed_summaries = Vec::new();
    let mut grace_delivered = false;

    let rem_start_ms = (900 + 420 + 900) * 1000_i64;
    let rem_end_ms = rem_start_ms + 1500 * 1000_i64;

    for ev in &trace.bpm_only_gt5 {
        for o in eng.handle_event(ev.clone()) {
            match o {
                EngineOutput::StateChanged {
                    ts_ms,
                    to: DetectorState::Confirmed,
                    ..
                } => {
                    confirmed_count += 1;
                    if !grace_delivered {
                        grace_delivered = true;
                        let _ = eng.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
                            ts_ms: ts_ms + 1_000,
                            outcome: GraceResult::Expired,
                        }));
                    }
                }
                EngineOutput::EpochEmitted(ep)
                    if (rem_start_ms..rem_end_ms).contains(&ep.ts_ms)
                        && ep.p_sleep < 0.30
                        && ep.p_active_wake < 0.50 =>
                {
                    rem_quiet_wake_epochs += 1;
                }
                EngineOutput::SessionEnded(s) => {
                    assert!(
                        s.end_ts_ms >= rem_end_ms,
                        "REM QuietWake plateau must not split Sleeping session early (ended at {} ms)",
                        s.end_ts_ms
                    );
                    closed_summaries.push(s);
                }
                _ => {}
            }
        }
    }

    assert!(
        rem_quiet_wake_epochs >= 40,
        "expected >= 10 min of QuietWake epochs during REM plateau, got {rem_quiet_wake_epochs}"
    );
    assert_eq!(
        confirmed_count, 1,
        "overnight REM plateau must produce exactly 1 Confirmed onset"
    );
    assert_eq!(closed_summaries.len(), 1);
    let summary = &closed_summaries[0];
    assert_eq!(summary.end_reason, SessionEndReason::SustainedWakefulness);
    let sleep_mu = summary.sleep_mu_hr.expect("sleep_mu_hr must be recorded");
    let target_sleep_hr = pheno.quiet_wake_hr - pheno.sleep_drop_bpm;
    assert!(
        sleep_mu <= target_sleep_hr + 4.0,
        "morning wake tail must be excluded from sleep_mu_hr (got {sleep_mu:.2})"
    );
}
