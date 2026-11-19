mod common;

use bpmute::calibration::{
    BoundaryReason, CALIBRATION_ALGORITHM_VERSION, CalibrationBoundaryRecord, CalibrationFolder,
    CalibrationState, DismissedShift, FeedbackVerdict, LedgerRecomputeInput, SessionFeedbackRecord,
    SessionLedgerRecord, SessionReplayInput, ShiftDirection, WearableSeedMeta,
    recompute_calibration,
};
use bpmute::config::{DetectionConfig, PowerAction, Sensitivity};
use bpmute::sleep_engine::{
    CalibrationMaturity, CalibrationPriors, EngineConfig, EngineOutput, Epoch, SessionEndReason,
    SleepEngine, WearableFloor,
};
use common::synthetic::{Phenotype, StageKind, generate_paired_trace};

fn make_clean_session(
    idx: usize,
    wearable_id: &str,
    warmup_mu_hr: f64,
    sleep_mu_hr: f64,
    sleep_hr_var_ratio: f64,
) -> SessionReplayInput {
    let day_ms = 86_400_000_i64;
    let start_ts_ms = (idx as i64 + 1) * day_ms;
    let delta = sleep_mu_hr - warmup_mu_hr;
    SessionReplayInput {
        session: SessionLedgerRecord {
            id: format!("01900000-0000-7000-8000-{idx:012}"),
            wearable_id: wearable_id.to_string(),
            start_ts_ms,
            end_ts_ms: start_ts_ms + 3_600_000,
            end_reason: SessionEndReason::SustainedWakefulness,
            detection_sensitivity: Sensitivity::Balanced,
            had_rr: true,
            warmup_mu_hr: Some(warmup_mu_hr),
            warmup_sigma_hr: Some(2.4),
            warmup_ln_rmssd: Some(3.45),
            onset_candidate_ts_ms: Some(start_ts_ms + 900_000),
            onset_confirmed_ts_ms: Some(start_ts_ms + 1_080_000),
            sleeping_ts_ms: Some(start_ts_ms + 1_110_000),
            sleep_mu_hr: Some(sleep_mu_hr),
            onset_hr_bpm: Some(sleep_mu_hr + 0.5),
            delta_hr_onset: Some(delta),
            sleep_ln_rmssd: Some(3.75),
            sleep_hr_var_ratio: Some(sleep_hr_var_ratio),
            typical_sqi: Some(0.96),
            post_trigger_sleep_sec: 1_800,
            trigger_power_action: PowerAction::None,
            rapid_resume_within_5m: false,
            grace_cancel_count: 0,
            rapid_wake_fp_count: 0,
            snooze_count: 0,
            auto_anomalous: false,
            epochs_count: 240,
            raw_samples_count: 3_600,
        },
        feedback: None,
        epochs: Vec::new(),
        wearable_supports_rr: true,
        wearable_supports_contact: true,
    }
}

fn run_synthetic_epochs(phenotype: Phenotype) -> (Vec<Epoch>, i64) {
    let trace = generate_paired_trace(
        777,
        phenotype,
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep {
                duration_sec: 1_200,
            },
        ],
    );
    let mut engine = SleepEngine::new(EngineConfig {
        detection: DetectionConfig::default(),
        priors: CalibrationPriors::cold_start(),
        wearable_floor: WearableFloor::default(),
    });
    let mut epochs = Vec::new();
    for ev in &trace.bpm_rr {
        for out in engine.handle_event(ev.clone()) {
            if let EngineOutput::EpochEmitted(ep) = out {
                epochs.push(ep);
            }
        }
    }
    let onset_ts = trace.onset_sec.unwrap_or(900) * 1000;
    (epochs, onset_ts)
}

fn assert_state_bitwise_eq(a: &CalibrationState, b: &CalibrationState) {
    assert_eq!(a.algorithm_version, b.algorithm_version);
    assert_eq!(a.maturity, b.maturity);
    assert_eq!(a.valid_sessions, b.valid_sessions);
    assert_eq!(a.quarantined_sessions, b.quarantined_sessions);
    assert_eq!(a.mu_wake.map(f64::to_bits), b.mu_wake.map(f64::to_bits));
    assert_eq!(
        a.sigma_wake.map(f64::to_bits),
        b.sigma_wake.map(f64::to_bits)
    );
    assert_eq!(
        a.wake_median.map(f64::to_bits),
        b.wake_median.map(f64::to_bits)
    );
    assert_eq!(
        a.wake_mad_s.map(f64::to_bits),
        b.wake_mad_s.map(f64::to_bits)
    );
    assert_eq!(a.mu_sleep.map(f64::to_bits), b.mu_sleep.map(f64::to_bits));
    assert_eq!(
        a.delta_hr_onset.map(f64::to_bits),
        b.delta_hr_onset.map(f64::to_bits)
    );
    assert_eq!(
        a.mu_ln_rmssd_sleep.map(f64::to_bits),
        b.mu_ln_rmssd_sleep.map(f64::to_bits)
    );
    assert_eq!(
        a.tau_p_sleep_offset.to_bits(),
        b.tau_p_sleep_offset.to_bits()
    );
    assert_eq!(a.dwell_adjust_sec, b.dwell_adjust_sec);
    assert_eq!(
        a.day1_seed_sleep_hr.map(f64::to_bits),
        b.day1_seed_sleep_hr.map(f64::to_bits)
    );
    assert_eq!(a.active_boundary_id, b.active_boundary_id);
    assert_eq!(a.active_boundary_ts_ms, b.active_boundary_ts_ms);
    assert_eq!(a.consecutive_anomaly_count, b.consecutive_anomaly_count);
    assert_eq!(
        a.consecutive_anomaly_direction,
        b.consecutive_anomaly_direction
    );
    assert_eq!(a.pending_shift_alert, b.pending_shift_alert);
    assert_eq!(a.wearables.len(), b.wearables.len());
    for (k, wa) in &a.wearables {
        let wb = b.wearables.get(k).expect("wearable keys must match");
        assert_eq!(wa.wearable_id, wb.wearable_id);
        assert_eq!(wa.hr_var_floor.to_bits(), wb.hr_var_floor.to_bits());
        assert_eq!(wa.supports_rr, wb.supports_rr);
        assert_eq!(wa.supports_contact, wb.supports_contact);
        assert_eq!(
            wa.typical_sqi.map(f64::to_bits),
            wb.typical_sqi.map(f64::to_bits)
        );
        assert_eq!(wa.sessions_count, wb.sessions_count);
    }
}

#[test]
fn maturity_progression_day1_seed_and_10_session_convergence() {
    let w_id = "wearable-ppg";
    let wearables = vec![WearableSeedMeta {
        wearable_id: w_id.to_string(),
        supports_rr: true,
        supports_contact: true,
    }];

    // Day-1 seeded ColdStart: seed 56.0 -> onset prior = 58.5 (+2.5 bpm nadir offset).
    let seeded_empty = recompute_calibration(&LedgerRecomputeInput {
        day1_seed_sleep_hr: Some(56.0),
        wearables: wearables.clone(),
        ..Default::default()
    });
    assert_eq!(
        seeded_empty.algorithm_version,
        CALIBRATION_ALGORITHM_VERSION
    );
    assert_eq!(seeded_empty.maturity, CalibrationMaturity::ColdStart);
    assert_eq!(seeded_empty.valid_sessions, 0);
    assert_eq!(seeded_empty.mu_sleep, Some(58.5));

    // Check tier transitions 0..=2 ColdStart -> 3..=6 Learning -> 7+ Calibrated,
    // and MAD quarantine activation at 5 clean sessions.
    let mut sessions = Vec::new();
    for i in 0..10 {
        // Alternate slight realistic nightly jitter around true mu_sleep = 58.5 bpm
        let jitter = if i % 2 == 0 { 0.4 } else { -0.4 };
        sessions.push(make_clean_session(
            i,
            w_id,
            66.0 + jitter * 0.5,
            58.5 + jitter,
            0.28,
        ));
    }

    let after_2 = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: sessions[..2].to_vec(),
        ..Default::default()
    });
    assert_eq!(after_2.maturity, CalibrationMaturity::ColdStart);
    assert_eq!(after_2.valid_sessions, 2);
    assert!(after_2.wake_mad_s.is_none());

    let after_4 = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: sessions[..4].to_vec(),
        ..Default::default()
    });
    assert_eq!(after_4.maturity, CalibrationMaturity::Learning);
    assert_eq!(after_4.valid_sessions, 4);
    assert!(
        after_4.wake_mad_s.is_none(),
        "MAD quarantine must not activate before 5 clean sessions"
    );

    let after_5 = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: sessions[..5].to_vec(),
        ..Default::default()
    });
    assert_eq!(after_5.maturity, CalibrationMaturity::Learning);
    assert!(
        after_5.wake_mad_s.is_some(),
        "MAD quarantine activates at 5 clean sessions"
    );

    let after_7 = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: sessions[..7].to_vec(),
        ..Default::default()
    });
    assert_eq!(after_7.maturity, CalibrationMaturity::Calibrated);
    assert_eq!(after_7.valid_sessions, 7);

    // Convergence <= 1.0 bpm after 10 clean sessions across unseeded & seeded low/typical/high RHR phenotypes.
    for (wake, true_sleep, seed) in [
        (48.0, 42.0, None),
        (66.0, 58.5, None),
        (82.0, 73.0, None),
        (66.0, 58.5, Some(56.0)),
    ] {
        let pheno_sessions: Vec<SessionReplayInput> = (0..10)
            .map(|i| {
                let jitter = ((i % 3) as f64 - 1.0) * 0.5;
                make_clean_session(i, w_id, wake + jitter * 0.4, true_sleep + jitter, 0.25)
            })
            .collect();
        let st = recompute_calibration(&LedgerRecomputeInput {
            day1_seed_sleep_hr: seed,
            wearables: wearables.clone(),
            sessions: pheno_sessions,
            ..Default::default()
        });
        assert_eq!(st.maturity, CalibrationMaturity::Calibrated);
        let mu_sleep = st.mu_sleep.expect("mu_sleep learned");
        assert!(
            (mu_sleep - true_sleep).abs() <= 1.0,
            "expected <= 1.0 bpm convergence to {true_sleep}, got {mu_sleep}"
        );
    }
}

#[test]
fn feedback_weight_matrix_suspend_cancel_snooze_and_manual_overrides() {
    let w_id = "wearable-1";
    let wearables = vec![WearableSeedMeta {
        wearable_id: w_id.to_string(),
        supports_rr: true,
        supports_contact: true,
    }];

    // Baseline 2 clean sessions -> mu_sleep = 58.0, mu_wake = 66.0.
    let s0 = make_clean_session(0, w_id, 66.0, 58.0, 0.25);
    let s1 = make_clean_session(1, w_id, 66.0, 58.0, 0.25);
    let base_2 = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: vec![s0.clone(), s1.clone()],
        ..Default::default()
    });
    assert_eq!(base_2.mu_sleep, Some(58.0));

    // 1) Uncancelled Suspend/PowerOff (w = 0.35): updates waking baseline and onset drop ONLY, NOT mu_sleep!
    let mut s_suspend = make_clean_session(2, w_id, 68.0, 50.0, 0.25);
    s_suspend.session.end_reason = SessionEndReason::HostSuspend;
    s_suspend.session.trigger_power_action = PowerAction::Suspend;
    s_suspend.session.post_trigger_sleep_sec = 0;
    s_suspend.session.sleep_mu_hr = None;
    s_suspend.session.onset_hr_bpm = Some(60.0);
    s_suspend.session.delta_hr_onset = Some(-8.0);
    let after_suspend = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: vec![s0.clone(), s1.clone(), s_suspend],
        ..Default::default()
    });
    assert_eq!(after_suspend.valid_sessions, 3);
    assert_eq!(
        after_suspend.mu_sleep.unwrap().to_bits(),
        base_2.mu_sleep.unwrap().to_bits(),
        "uncancelled Suspend/PowerOff must NOT update mu_sleep"
    );
    assert!(
        after_suspend.mu_wake.unwrap() > base_2.mu_wake.unwrap(),
        "Suspend/PowerOff updates mu_wake"
    );
    assert_ne!(
        after_suspend.delta_hr_onset.map(f64::to_bits),
        base_2.delta_hr_onset.map(f64::to_bits),
        "Suspend/PowerOff updates delta_hr_onset"
    );

    // 2) Grace cancel & < 5 min rapid resume: bounded FP tightening capped at 1 per session,
    //    excluded from mu_sleep; Snooze carries zero penalty!
    let mut s_snooze = make_clean_session(2, w_id, 66.0, 58.0, 0.25);
    s_snooze.session.snooze_count = 3;
    let after_snooze = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: vec![s0.clone(), s1.clone(), s_snooze],
        ..Default::default()
    });
    assert_eq!(after_snooze.tau_p_sleep_offset, 0.0);
    assert_eq!(after_snooze.dwell_adjust_sec, 0);

    let mut s_single_cancel = make_clean_session(2, w_id, 66.0, 48.0, 0.25);
    s_single_cancel.session.grace_cancel_count = 1;
    s_single_cancel.session.post_trigger_sleep_sec = 0;
    let mut s_multi_cancel = s_single_cancel.clone();
    s_multi_cancel.session.grace_cancel_count = 4;
    s_multi_cancel.session.rapid_wake_fp_count = 2;
    s_multi_cancel.session.rapid_resume_within_5m = true;
    let after_one_cancel = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: vec![s0.clone(), s1.clone(), s_single_cancel],
        ..Default::default()
    });
    let after_multi_cancel = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: vec![s0.clone(), s1.clone(), s_multi_cancel],
        ..Default::default()
    });
    assert!(after_one_cancel.tau_p_sleep_offset > 0.0);
    assert!(after_one_cancel.dwell_adjust_sec > 0);
    assert_state_bitwise_eq(&after_one_cancel, &after_multi_cancel);
    assert_eq!(
        after_one_cancel.mu_sleep.unwrap().to_bits(),
        base_2.mu_sleep.unwrap().to_bits(),
        "FP session must be excluded from sleeping-HR estimation"
    );

    // 3) Manual avg_sleep_hr (w = 2.0 + 2.5 bpm nadir offset) outweighs single implicit session (w = 0.5)
    let mut s_manual_hr = make_clean_session(2, w_id, 66.0, 58.0, 0.25);
    s_manual_hr.feedback = Some(SessionFeedbackRecord {
        id: "fb-1".to_string(),
        session_id: s_manual_hr.session.id.clone(),
        verdict: None,
        manual_onset_ts_ms: None,
        manual_avg_sleep_hr: Some(50.0), // +2.5 nadir offset => 52.5 onset sleep HR
        notes: None,
        updated_at_ms: 100,
    });
    let after_manual_hr = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: vec![s0.clone(), s1.clone(), s_manual_hr],
        ..Default::default()
    });
    // Base had w = 1.0 at 58.0; manual adds w = 2.0 at 52.5 => (58.0 + 2*52.5)/3 = 54.333...
    assert!(
        (after_manual_hr.mu_sleep.unwrap() - 54.333_333).abs() < 0.2,
        "expected manual avg sleep HR weight ~2.0 with +2.5 nadir offset, got {:?}",
        after_manual_hr.mu_sleep
    );

    // 4) Manual Wearable-app onset timestamp replayed against stored 15 s Epochs + missed_sleep verdict
    let (epochs, true_onset_ms) = run_synthetic_epochs(Phenotype::athletic_low_rhr());
    let mut s_missed = make_clean_session(2, w_id, 50.0, 44.0, 0.20);
    s_missed.session.start_ts_ms = epochs.first().unwrap().ts_ms;
    s_missed.session.end_ts_ms = epochs.last().unwrap().ts_ms;
    s_missed.session.warmup_mu_hr = Some(50.0);
    s_missed.session.onset_confirmed_ts_ms = None;
    s_missed.session.sleeping_ts_ms = None;
    s_missed.session.sleep_mu_hr = None;
    s_missed.session.post_trigger_sleep_sec = 0;
    s_missed.epochs = epochs;
    s_missed.feedback = Some(SessionFeedbackRecord {
        id: "fb-2".to_string(),
        session_id: s_missed.session.id.clone(),
        verdict: Some(FeedbackVerdict::MissedSleep),
        manual_onset_ts_ms: Some(true_onset_ms),
        manual_avg_sleep_hr: None,
        notes: None,
        updated_at_ms: 200,
    });
    let after_onset_replay = recompute_calibration(&LedgerRecomputeInput {
        wearables,
        sessions: vec![s0, s1, s_missed],
        ..Default::default()
    });
    assert_eq!(after_onset_replay.valid_sessions, 3);
    assert!(
        after_onset_replay.tau_p_sleep_offset < 0.0,
        "missed_sleep verdict should gently loosen tau_p_sleep_offset"
    );
    assert!(
        after_onset_replay.mu_sleep.unwrap() < base_2.mu_sleep.unwrap(),
        "replaying Wearable-app onset against athletic 15s Epochs should pull mu_sleep down"
    );
}

#[test]
fn mad_quarantine_zero_drift_persistent_shift_boundary_and_dismissal() {
    let w_id = "wearable-1";
    let wearables = vec![WearableSeedMeta {
        wearable_id: w_id.to_string(),
        supports_rr: true,
        supports_contact: true,
    }];

    // 5 clean sessions at mu_wake = 65.0 (+/- 0.8), mu_sleep = 58.0
    let mut clean: Vec<SessionReplayInput> = (0..5)
        .map(|i| {
            let jitter = match i % 3 {
                0 => -0.8,
                1 => 0.0,
                _ => 0.8,
            };
            make_clean_session(i, w_id, 65.0 + jitter, 58.0 + jitter, 0.25)
        })
        .collect();
    let baseline_5 = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: clean.clone(),
        ..Default::default()
    });
    assert_eq!(baseline_5.valid_sessions, 5);
    assert!(baseline_5.wake_mad_s.is_some());

    // Add 1..=4 fever/alcohol Anomalous Sessions (+12 bpm => > 2.5 S) -> EXACT 0.0 drift!
    for k in 1..=4 {
        let mut with_anomalies = clean.clone();
        for j in 0..k {
            with_anomalies.push(make_clean_session(5 + j, w_id, 78.0, 71.0, 0.25));
        }
        let st = recompute_calibration(&LedgerRecomputeInput {
            wearables: wearables.clone(),
            sessions: with_anomalies,
            ..Default::default()
        });
        assert_eq!(st.valid_sessions, 5);
        assert_eq!(st.quarantined_sessions, k as u32);
        assert_eq!(
            st.mu_wake.unwrap().to_bits(),
            baseline_5.mu_wake.unwrap().to_bits(),
            "exact 0.0 drift across {k} quarantined sessions"
        );
        assert_eq!(
            st.mu_sleep.unwrap().to_bits(),
            baseline_5.mu_sleep.unwrap().to_bits()
        );
        assert!(
            st.pending_shift_alert.is_none(),
            "shift prompt must not trigger before 5th consecutive same-direction anomaly"
        );
    }

    // Opposite-direction anomaly in streak resets same-direction counter
    let mut mixed_streak = clean.clone();
    for j in 0..4 {
        mixed_streak.push(make_clean_session(5 + j, w_id, 78.0, 71.0, 0.25));
    }
    mixed_streak.push(make_clean_session(9, w_id, 52.0, 45.0, 0.25)); // opposite (Lower)
    let st_mixed = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: mixed_streak,
        ..Default::default()
    });
    assert!(st_mixed.pending_shift_alert.is_none());
    assert_eq!(st_mixed.consecutive_anomaly_count, 1);
    assert_eq!(
        st_mixed.consecutive_anomaly_direction,
        Some(ShiftDirection::Lower)
    );

    // 5th consecutive Higher anomaly triggers persistent-shift prompt anchored at session #5
    for j in 0..5 {
        clean.push(make_clean_session(5 + j, w_id, 78.0, 71.0, 0.14));
    }
    let st_5th = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: clean.clone(),
        ..Default::default()
    });
    let alert = st_5th
        .pending_shift_alert
        .as_ref()
        .expect("5th consecutive same-direction shift must trigger prompt");
    assert_eq!(alert.direction, ShiftDirection::Higher);
    assert_eq!(alert.session_count, 5);
    assert_eq!(alert.first_session_id, clean[5].session.id);
    assert_eq!(
        alert.first_session_start_ts_ms,
        clean[5].session.start_ts_ms
    );

    // Keep ignoring (dismiss) suppresses the alert without drifting old baseline even on 6th session
    let mut with_6th = clean.clone();
    with_6th.push(make_clean_session(10, w_id, 78.0, 71.0, 0.14));
    let dismissed = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: with_6th,
        dismissed_shift: Some(DismissedShift {
            anchor_ts_ms: alert.first_session_start_ts_ms,
            direction: ShiftDirection::Higher,
        }),
        ..Default::default()
    });
    assert!(dismissed.pending_shift_alert.is_none());
    assert_eq!(
        dismissed.mu_wake.unwrap().to_bits(),
        baseline_5.mu_wake.unwrap().to_bits()
    );

    // Accept new baseline inserts CalibrationBoundary right before the 5 shifted sessions,
    // clears their quarantine, folds them cleanly, and preserves per-Wearable hr_var_floor!
    let boundary = CalibrationBoundaryRecord {
        id: "boundary-1".to_string(),
        effective_ts_ms: alert.first_session_start_ts_ms,
        reason: BoundaryReason::PersistentShift,
        seed_sleep_hr: None,
        reset_floors: false,
        created_at_ms: 999_999,
        reverted_at_ms: None,
    };
    let accepted = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        boundaries: vec![boundary.clone()],
        sessions: clean.clone(),
        ..Default::default()
    });
    assert!(accepted.pending_shift_alert.is_none());
    assert_eq!(accepted.valid_sessions, 5);
    assert_eq!(accepted.quarantined_sessions, 0);
    assert!((accepted.mu_wake.unwrap() - 78.0).abs() <= 0.1);
    assert!((accepted.mu_sleep.unwrap() - 71.0).abs() <= 0.1);
    assert_eq!(
        accepted.wearables[w_id].sessions_count, 10,
        "Wearable floor history must be preserved across non-floor-reset CalibrationBoundary"
    );

    // Reverting the CalibrationBoundary restores the pre-boundary state bit-for-bit
    let mut reverted_boundary = boundary;
    reverted_boundary.reverted_at_ms = Some(1_000_000);
    let reverted = recompute_calibration(&LedgerRecomputeInput {
        wearables,
        boundaries: vec![reverted_boundary],
        sessions: clean,
        ..Default::default()
    });
    assert_state_bitwise_eq(&reverted, &st_5th);
}

#[test]
fn hybrid_wearable_scoping_and_bitwise_round_trip_determinism() {
    let wrist_ppg = "wearable-gt5";
    let chest_strap = "wearable-h10";
    let wearables = vec![
        WearableSeedMeta {
            wearable_id: wrist_ppg.to_string(),
            supports_rr: false,
            supports_contact: true,
        },
        WearableSeedMeta {
            wearable_id: chest_strap.to_string(),
            supports_rr: true,
            supports_contact: true,
        },
    ];

    // 4 noisy wrist PPG sessions + 4 quiet chest strap sessions on the same user
    let mut sessions = Vec::new();
    for i in 0..4 {
        let mut s = make_clean_session(i, wrist_ppg, 64.0, 57.0, 0.55);
        s.session.had_rr = false;
        s.wearable_supports_rr = false;
        s.session.typical_sqi = Some(0.86);
        sessions.push(s);
    }
    for i in 4..8 {
        let mut s = make_clean_session(i, chest_strap, 64.0, 57.0, 0.10);
        s.session.had_rr = true;
        s.wearable_supports_rr = true;
        s.session.typical_sqi = Some(0.99);
        sessions.push(s);
    }

    let st = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: sessions.clone(),
        ..Default::default()
    });

    // Personal physiological state is shared across Wearables (4 + 4 = 8 -> Calibrated!)
    assert_eq!(st.maturity, CalibrationMaturity::Calibrated);
    assert_eq!(st.valid_sessions, 8);
    assert!((st.mu_sleep.unwrap() - 57.0).abs() <= 0.2);

    // Per-Wearable floors and SQI remain strictly isolated
    let w_ppg = &st.wearables[wrist_ppg];
    let w_h10 = &st.wearables[chest_strap];
    assert!(
        w_ppg.hr_var_floor > 0.35,
        "noisy optical PPG should raise floor, got {}",
        w_ppg.hr_var_floor
    );
    assert!(
        w_h10.hr_var_floor < 0.20,
        "clean ECG chest strap should stay low and isolated, got {}",
        w_h10.hr_var_floor
    );
    assert!(!w_ppg.supports_rr);
    assert!(w_h10.supports_rr);
    assert!((w_ppg.typical_sqi.unwrap() - 0.86).abs() < 0.02);
    assert!((w_h10.typical_sqi.unwrap() - 0.99).abs() < 0.02);

    // Bit-for-bit incremental close == scratch recompute (even with reversed input order for scratch)
    let mut folder = CalibrationFolder::new(None, &[], None, &wearables);
    for s in &sessions {
        folder.fold_session(s);
    }
    let incremental = folder.finish();

    let mut reversed_sessions = sessions.clone();
    reversed_sessions.reverse();
    let scratch_unsorted = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: reversed_sessions,
        ..Default::default()
    });
    assert_state_bitwise_eq(&incremental, &scratch_unsorted);

    // Bit-for-bit edit + revert invariance
    let mut edited_sessions = sessions.clone();
    edited_sessions[3].feedback = Some(SessionFeedbackRecord {
        id: "fb-temp".to_string(),
        session_id: edited_sessions[3].session.id.clone(),
        verdict: Some(FeedbackVerdict::AnomalousExclude),
        manual_onset_ts_ms: None,
        manual_avg_sleep_hr: None,
        notes: None,
        updated_at_ms: 500,
    });
    let edited = recompute_calibration(&LedgerRecomputeInput {
        wearables: wearables.clone(),
        sessions: edited_sessions.clone(),
        ..Default::default()
    });
    assert_ne!(edited.valid_sessions, st.valid_sessions);

    edited_sessions[3].feedback = None;
    let reverted = recompute_calibration(&LedgerRecomputeInput {
        wearables,
        sessions: edited_sessions,
        ..Default::default()
    });
    assert_state_bitwise_eq(&st, &reverted);
}
