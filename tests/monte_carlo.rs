mod common;

use std::fs;
use std::path::PathBuf;

use bpmute::config::{DetectionConfig, Sensitivity};
use bpmute::paths::{EnvSource, ProcessEnv};
use bpmute::sleep_engine::ppg::quantile;
use bpmute::sleep_engine::{
    CalibrationMaturity, CalibrationPriors, DetectorState, EngineConfig, EngineEvent, EngineOutput,
    Epoch, GraceOutcome, GraceResult, SleepEngine, WearableFloor,
};
use common::synthetic::{
    Phenotype, StageKind, generate_paired_trace, inject_flatline, inject_gt5_optical_lock_ramp,
    inject_hampel_spikes, inject_rr_corruption, inject_silent_gap,
};
use rand::Rng;
use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::SeedableRng;
use serde::Serialize;

const ROOT_SEED: u64 = 0x5EED_B907_E024;
const NUM_SLEEP_SESSIONS: usize = 100;
const NUM_WAKE_SESSIONS: usize = 100;

const MIN_WAKE_SPECIFICITY: f64 = 0.98;
const MIN_SLEEP_RECALL: f64 = 0.95;
const MAX_MEDIAN_LATENCY_BPM_RR_MIN: f64 = 8.0;
const MAX_MEDIAN_LATENCY_BPM_ONLY_MIN: f64 = 10.0;
const MAX_P95_LATENCY_MIN: f64 = 18.0;

#[derive(Debug, Clone, Copy)]
enum SleepArchetype {
    StandardN1N2,
    SlowDescent,
    EveningActiveSpikeThenSleep,
    PostWorkoutCooldownThenSleep,
}

#[derive(Debug, Clone, Copy)]
enum WakeArchetype {
    MotionlessMovie,
    PostWorkoutRecovery,
    BriefDrowsyDips,
    ActiveDeskSpikes,
}

#[derive(Debug, Serialize)]
struct ModeMetricsReport {
    mode: String,
    total_sessions: usize,
    sleep_sessions: usize,
    wake_sessions: usize,
    wake_negatives_total: usize,
    wake_false_positives: usize,
    pre_onset_false_positives: usize,
    total_false_positives: usize,
    wake_specificity: f64,
    false_positive_rate: f64,
    sleep_true_positives: usize,
    sleep_missed: usize,
    sleep_recall: f64,
    median_latency_min: f64,
    p95_latency_min: f64,
    max_latency_min: f64,
    gates_passed: bool,
}

#[derive(Debug, Serialize)]
struct MonteCarloReport {
    seed: u64,
    total_sessions: usize,
    sleep_sessions: usize,
    wake_sessions: usize,
    rhr_range_bpm: [f64; 2],
    drop_range_bpm: [f64; 2],
    bpm_rr: ModeMetricsReport,
    bpm_only: ModeMetricsReport,
    all_gates_passed: bool,
    failures: Vec<SessionFailureRecord>,
}

#[derive(Debug, Serialize)]
struct SessionFailureRecord {
    session_index: usize,
    kind: String,
    mode: String,
    archetype: String,
    maturity: String,
    rhr_bpm: f64,
    drop_bpm: f64,
    outcome: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trace_window: Vec<String>,
}

struct ModeStatsAccum {
    mode: &'static str,
    wake_negatives_total: usize,
    wake_false_positives: usize,
    pre_onset_false_positives: usize,
    sleep_total: usize,
    sleep_true_positives: usize,
    latencies_min: Vec<f64>,
}

impl ModeStatsAccum {
    fn new(mode: &'static str) -> Self {
        Self {
            mode,
            wake_negatives_total: 0,
            wake_false_positives: 0,
            pre_onset_false_positives: 0,
            sleep_total: 0,
            sleep_true_positives: 0,
            latencies_min: Vec::new(),
        }
    }

    fn finalize(&self, max_median_min: f64) -> ModeMetricsReport {
        let total_sessions = self.sleep_total + self.wake_negatives_total;
        let total_fp = self.wake_false_positives + self.pre_onset_false_positives;
        let wake_specificity = if self.wake_negatives_total > 0 {
            1.0 - (self.wake_false_positives as f64 / self.wake_negatives_total as f64)
        } else {
            1.0
        };
        let false_positive_rate = if total_sessions > 0 {
            total_fp as f64 / total_sessions as f64
        } else {
            0.0
        };
        let sleep_recall = if self.sleep_total > 0 {
            self.sleep_true_positives as f64 / self.sleep_total as f64
        } else {
            1.0
        };

        let median_latency_min = quantile(&self.latencies_min, 0.50).unwrap_or(f64::NAN);
        let p95_latency_min = quantile(&self.latencies_min, 0.95).unwrap_or(f64::NAN);
        let max_latency_min = self.latencies_min.iter().copied().fold(0.0_f64, f64::max);

        let gates_passed = wake_specificity >= MIN_WAKE_SPECIFICITY
            && false_positive_rate <= (1.0 - MIN_WAKE_SPECIFICITY)
            && sleep_recall >= MIN_SLEEP_RECALL
            && !self.latencies_min.is_empty()
            && median_latency_min <= max_median_min
            && p95_latency_min <= MAX_P95_LATENCY_MIN;

        ModeMetricsReport {
            mode: self.mode.to_string(),
            total_sessions,
            sleep_sessions: self.sleep_total,
            wake_sessions: self.wake_negatives_total,
            wake_negatives_total: self.wake_negatives_total,
            wake_false_positives: self.wake_false_positives,
            pre_onset_false_positives: self.pre_onset_false_positives,
            total_false_positives: total_fp,
            wake_specificity,
            false_positive_rate,
            sleep_true_positives: self.sleep_true_positives,
            sleep_missed: self.sleep_total.saturating_sub(self.sleep_true_positives),
            sleep_recall,
            median_latency_min,
            p95_latency_min,
            max_latency_min,
            gates_passed,
        }
    }
}

fn random_phenotype(rng: &mut ChaCha8Rng) -> Phenotype {
    let quiet_wake_hr = rng.gen_range(45.0_f64..=85.0_f64);
    let max_drop = (quiet_wake_hr - 36.0).clamp(5.0, 15.0);
    let sleep_drop_bpm = rng.gen_range(5.0_f64..=max_drop);
    let wake_rmssd_base = 65.0 - 0.65 * (quiet_wake_hr - 45.0);
    let wake_rmssd_ms = (wake_rmssd_base + rng.gen_range(-6.0_f64..=6.0_f64)).clamp(16.0, 72.0);
    let sleep_rmssd_ms = wake_rmssd_ms * rng.gen_range(1.22_f64..=1.38_f64);
    Phenotype {
        quiet_wake_hr,
        sleep_drop_bpm,
        wake_rmssd_ms,
        sleep_rmssd_ms,
    }
}

fn random_priors(rng: &mut ChaCha8Rng, pheno: Phenotype) -> (CalibrationPriors, &'static str) {
    let roll = rng.gen_range(0_u8..100);
    if roll < 45 {
        (CalibrationPriors::cold_start(), "ColdStart")
    } else if roll < 75 {
        (
            CalibrationPriors {
                maturity: CalibrationMaturity::Learning,
                mu_wake: Some(pheno.quiet_wake_hr + rng.gen_range(-1.5_f64..=1.5_f64)),
                wake_median: None,
                sigma_wake: Some(2.2),
                mu_sleep: Some(
                    (pheno.quiet_wake_hr - pheno.sleep_drop_bpm
                        + rng.gen_range(-1.0_f64..=1.0_f64))
                    .max(35.0),
                ),
                delta_hr_onset: Some(-pheno.sleep_drop_bpm * 0.85),
                mu_ln_rmssd_sleep: Some(pheno.sleep_rmssd_ms.ln()),
                tau_p_sleep_offset: 0.0,
                dwell_adjust_sec: 0,
                wake_mad_s: Some(2.4),
            },
            "Learning",
        )
    } else if roll < 92 {
        (
            CalibrationPriors {
                maturity: CalibrationMaturity::Calibrated,
                mu_wake: Some(pheno.quiet_wake_hr + rng.gen_range(-1.0_f64..=1.0_f64)),
                wake_median: None,
                sigma_wake: Some(2.3),
                mu_sleep: Some((pheno.quiet_wake_hr - pheno.sleep_drop_bpm).max(35.0)),
                delta_hr_onset: Some(-pheno.sleep_drop_bpm * 0.90),
                mu_ln_rmssd_sleep: Some(pheno.sleep_rmssd_ms.ln()),
                tau_p_sleep_offset: 0.0,
                dwell_adjust_sec: 0,
                wake_mad_s: Some(2.4),
            },
            "Calibrated",
        )
    } else {
        // Anomalous warmup elevation (> 2.5 S from learned personal baseline)
        (
            CalibrationPriors {
                maturity: CalibrationMaturity::Calibrated,
                mu_wake: Some((pheno.quiet_wake_hr - 9.0).max(40.0)),
                wake_median: None,
                sigma_wake: Some(2.2),
                mu_sleep: Some((pheno.quiet_wake_hr - 9.0 - pheno.sleep_drop_bpm).max(35.0)),
                delta_hr_onset: Some(-pheno.sleep_drop_bpm),
                mu_ln_rmssd_sleep: Some(pheno.sleep_rmssd_ms.ln()),
                tau_p_sleep_offset: 0.0,
                dwell_adjust_sec: 0,
                wake_mad_s: Some(2.0),
            },
            "AnomalousShift",
        )
    }
}

fn apply_stochastic_noise(
    rng: &mut ChaCha8Rng,
    mut events: Vec<EngineEvent>,
    total_sec: i64,
    onset_sec: Option<i64>,
    is_bpm_rr: bool,
) -> Vec<EngineEvent> {
    // 1) Optical lock ramp (~6-11 s)
    if rng.gen_bool(0.40) {
        let ramp_ms = rng.gen_range(5_000_i64..=11_000_i64);
        events = inject_gt5_optical_lock_ramp(&events, ramp_ms);
    }

    // 2) Hampel motion spikes (+35..+75 bpm at 2-4 points well spaced apart)
    if rng.gen_bool(0.55) && total_sec > 400 {
        let count = rng.gen_range(2_usize..=4);
        let mut spike_secs = Vec::with_capacity(count);
        for _ in 0..count {
            spike_secs.push(rng.gen_range(60_i64..(total_sec - 60)));
        }
        let delta = rng.gen_range(35.0_f64..=75.0_f64);
        events = inject_hampel_spikes(&events, &spike_secs, delta);
    }

    // 3) Short Tier-1 silent gap (25..75 s) outside warmup start and final minute
    if rng.gen_bool(0.30) && total_sec > 900 {
        let gap_start_sec = rng.gen_range(520_i64..(total_sec - 240));
        let gap_dur_sec = rng.gen_range(25_i64..=75_i64);
        let with_state = rng.gen_bool(0.5);
        events = inject_silent_gap(
            &events,
            gap_start_sec * 1000,
            (gap_start_sec + gap_dur_sec) * 1000,
            with_state,
        );
    }

    // 4) Mid-session >5% RR corruption on BPM+RR nights to stress HRV marginalization
    if is_bpm_rr && rng.gen_bool(0.35) && total_sec > 1000 {
        let start_ms = match onset_sec {
            Some(on) => (on + rng.gen_range(60..=240)) * 1000,
            None => rng.gen_range(400..=(total_sec - 400)) * 1000,
        };
        let end_ms = (start_ms + rng.gen_range(240_000..=480_000)).min((total_sec - 30) * 1000);
        if end_ms > start_ms {
            let every_n = rng.gen_range(4_usize..=8);
            events = inject_rr_corruption(&events, start_ms, end_ms, every_n);
        }
    }

    // 5) Brief 30-50 s integer-quantized BPM plateau during N2 sleep (not a firmware flatline)
    if let Some(on) = onset_sec
        && rng.gen_bool(0.30)
        && total_sec > on + 700
    {
        let plat_start = (on + rng.gen_range(360..=540)) * 1000;
        let plat_end = plat_start + rng.gen_range(30_000..=50_000);
        events = inject_flatline(&events, plat_start, plat_end, false);
    }

    events
}

struct EngineTrace {
    confirmed_ts: Vec<i64>,
    epochs: Vec<Epoch>,
}

fn run_engine(events: &[EngineEvent], priors: CalibrationPriors) -> EngineTrace {
    let mut engine = SleepEngine::new(EngineConfig {
        detection: DetectionConfig {
            sensitivity: Sensitivity::Balanced,
            advanced: None,
        },
        priors,
        wearable_floor: WearableFloor::default(),
    });
    let mut confirmed_ts = Vec::new();
    let mut epochs = Vec::new();
    for ev in events {
        let outputs = engine.handle_event(ev.clone());
        let mut expire_at = None;
        for out in outputs {
            match out {
                EngineOutput::EpochEmitted(epoch) => epochs.push(epoch),
                EngineOutput::StateChanged {
                    ts_ms,
                    to: DetectorState::Confirmed,
                    ..
                } => {
                    confirmed_ts.push(ts_ms);
                    expire_at = Some(ts_ms);
                }
                _ => {}
            }
        }
        if let Some(ts_ms) = expire_at {
            let _ = engine.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
                ts_ms,
                outcome: GraceResult::Expired,
            }));
        }
    }
    EngineTrace {
        confirmed_ts,
        epochs,
    }
}

fn format_epoch_window(epochs: &[Epoch], target_ts_ms: i64) -> Vec<String> {
    if epochs.is_empty() {
        return Vec::new();
    }
    let center = epochs
        .iter()
        .enumerate()
        .min_by_key(|(_, ep)| (ep.ts_ms - target_ts_ms).abs())
        .map_or(0, |(idx, _)| idx);
    let start = center.saturating_sub(5);
    let end = (center + 5).min(epochs.len() - 1);
    epochs[start..=end]
        .iter()
        .map(|ep| {
            format!(
                "t=+{}s st={:?} hr={:.1} mu={:.1} sig={:.2} z={:.2} slp={:.2} r_var={:.2} p_slp={:.3} frz={}",
                ep.ts_ms / 1000,
                ep.state,
                ep.features.hr_bpm_60s,
                ep.baseline_mu_hr,
                ep.baseline_sigma_hr,
                ep.features.z_hr,
                ep.features.hr_slope_4m,
                ep.features.r_var_120s,
                ep.p_sleep,
                ep.baseline_frozen,
            )
        })
        .collect()
}

fn matches_filter(
    filter: Option<&str>,
    session_index: usize,
    arch_name: &str,
    mode_name: &str,
) -> bool {
    let Some(needle) = filter else {
        return true;
    };
    let haystack = format!("#{session_index} {arch_name} {mode_name}").to_ascii_lowercase();
    haystack.contains(needle)
}

fn build_sleep_stages(rng: &mut ChaCha8Rng, arch: SleepArchetype) -> (Vec<StageKind>, i64) {
    let stages = match arch {
        SleepArchetype::StandardN1N2 => vec![
            StageKind::QuietWake {
                duration_sec: rng.gen_range(900..=1500),
            },
            StageKind::N1Onset {
                duration_sec: rng.gen_range(300..=480),
            },
            StageKind::N2Sleep {
                duration_sec: rng.gen_range(1500..=1800),
            },
        ],
        SleepArchetype::SlowDescent => vec![
            StageKind::QuietWake {
                duration_sec: rng.gen_range(900..=1200),
            },
            StageKind::N1Onset {
                duration_sec: rng.gen_range(600..=780),
            },
            StageKind::N2Sleep {
                duration_sec: rng.gen_range(1500..=1800),
            },
        ],
        SleepArchetype::EveningActiveSpikeThenSleep => vec![
            StageKind::QuietWake { duration_sec: 600 },
            StageKind::ActiveWakeSpike {
                duration_sec: rng.gen_range(120..=240),
                elevation_bpm: rng.gen_range(14.0_f64..=24.0_f64),
            },
            StageKind::QuietWake {
                duration_sec: rng.gen_range(780..=1020),
            },
            StageKind::N1Onset {
                duration_sec: rng.gen_range(360..=480),
            },
            StageKind::N2Sleep {
                duration_sec: rng.gen_range(1500..=1800),
            },
        ],
        SleepArchetype::PostWorkoutCooldownThenSleep => vec![
            StageKind::PostWorkoutCooldown {
                duration_sec: 900,
                initial_excess_bpm: rng.gen_range(16.0_f64..=24.0_f64),
            },
            StageKind::QuietWake {
                duration_sec: rng.gen_range(900..=1200),
            },
            StageKind::N1Onset {
                duration_sec: rng.gen_range(360..=480),
            },
            StageKind::N2Sleep {
                duration_sec: rng.gen_range(1500..=1800),
            },
        ],
    };
    let total_sec = stages.iter().map(stage_duration_sec).sum();
    (stages, total_sec)
}

fn build_wake_stages(rng: &mut ChaCha8Rng, arch: WakeArchetype) -> (Vec<StageKind>, i64) {
    let stages = match arch {
        WakeArchetype::MotionlessMovie => vec![StageKind::QuietWake {
            duration_sec: rng.gen_range(4500..=7200),
        }],
        WakeArchetype::PostWorkoutRecovery => vec![
            StageKind::PostWorkoutCooldown {
                duration_sec: rng.gen_range(780..=1080),
                initial_excess_bpm: rng.gen_range(15.0_f64..=25.0_f64),
            },
            StageKind::QuietWake {
                duration_sec: rng.gen_range(2700..=4200),
            },
        ],
        WakeArchetype::BriefDrowsyDips => vec![
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::DrowsyDip {
                duration_sec: rng.gen_range(90..=150),
                dip_bpm: rng.gen_range(3.5_f64..=5.0_f64),
            },
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::DrowsyDip {
                duration_sec: rng.gen_range(90..=160),
                dip_bpm: rng.gen_range(3.5_f64..=5.0_f64),
            },
            StageKind::QuietWake { duration_sec: 1800 },
        ],
        WakeArchetype::ActiveDeskSpikes => vec![
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::ActiveWakeSpike {
                duration_sec: rng.gen_range(120..=240),
                elevation_bpm: rng.gen_range(16.0_f64..=24.0_f64),
            },
            StageKind::QuietWake { duration_sec: 1800 },
            StageKind::ActiveWakeSpike {
                duration_sec: rng.gen_range(120..=180),
                elevation_bpm: rng.gen_range(18.0_f64..=26.0_f64),
            },
            StageKind::QuietWake { duration_sec: 1800 },
        ],
    };
    let total_sec = stages.iter().map(stage_duration_sec).sum();
    (stages, total_sec)
}

fn stage_duration_sec(stage: &StageKind) -> i64 {
    match *stage {
        StageKind::QuietWake { duration_sec }
        | StageKind::ActiveWakeSpike { duration_sec, .. }
        | StageKind::PostWorkoutCooldown { duration_sec, .. }
        | StageKind::DrowsyDip { duration_sec, .. }
        | StageKind::N1Onset { duration_sec }
        | StageKind::N2Sleep { duration_sec }
        | StageKind::Arousal { duration_sec }
        | StageKind::PostSleepWake { duration_sec, .. } => duration_sec,
    }
}

fn render_markdown_report(report: &MonteCarloReport) -> String {
    let mut md = String::new();
    md.push_str("# BPMute Tier 2 Seeded Monte Carlo Report\n\n");
    md.push_str(&format!(
        "- **Seed:** `0x{:X}`\n- **Total Monitoring Sessions:** `{}` (`{}` sleep-onset + `{}` hard wake-negative, paired `BPM+RR` & `BPM-only`)\n- **Phenotype Sweep:** RHR `{:.1}–{:.1} bpm`, sleep drop `{:.1}–{:.1} bpm`, noisy PPG (`GT5 ramp`, `Hampel spikes`, `short gaps`, `>5% RR corruption`, `N2 plateaus`)\n- **Overall Status:** **{}**\n\n",
        report.seed,
        report.total_sessions,
        report.sleep_sessions,
        report.wake_sessions,
        report.rhr_range_bpm[0],
        report.rhr_range_bpm[1],
        report.drop_range_bpm[0],
        report.drop_range_bpm[1],
        if report.all_gates_passed {
            "PASS"
        } else {
            "FAIL"
        },
    ));
    md.push_str("| Mode | Wake Specificity (Gate >= 98.0%) | FP Rate (Gate <= 2.0%) | Sleep Recall (Gate >= 95.0%) | Median Latency (Gate <= 8 / 10 min) | P95 Latency (Gate <= 18.0 min) | Max Latency | Gate |\n");
    md.push_str("|---|---:|---:|---:|---:|---:|---:|:---:|\n");
    for (m, med_limit) in [
        (&report.bpm_rr, MAX_MEDIAN_LATENCY_BPM_RR_MIN),
        (&report.bpm_only, MAX_MEDIAN_LATENCY_BPM_ONLY_MIN),
    ] {
        md.push_str(&format!(
            "| `{}` | {:.2}% ({} wake FP / {}) | {:.2}% ({} / {}) | {:.2}% ({}/{}) | {:.2} min (<= {:.1}) | {:.2} min (<= {:.1}) | {:.2} min | {} |\n",
            m.mode,
            m.wake_specificity * 100.0,
            m.wake_false_positives,
            m.wake_negatives_total,
            m.false_positive_rate * 100.0,
            m.total_false_positives,
            m.total_sessions,
            m.sleep_recall * 100.0,
            m.sleep_true_positives,
            m.sleep_sessions,
            m.median_latency_min,
            med_limit,
            m.p95_latency_min,
            MAX_P95_LATENCY_MIN,
            m.max_latency_min,
            if m.gates_passed { "PASS" } else { "FAIL" },
        ));
    }

    if !report.failures.is_empty() {
        md.push_str("\n## Notable Failures / Misses\n\n");
        for (idx, f) in report.failures.iter().take(20).enumerate() {
            md.push_str(&format!(
                "- Session #{} (`{}`, `{}`, `{}`, `{}`): RHR {:.1} bpm, drop {:.1} bpm — {}\n",
                f.session_index,
                f.kind,
                f.mode,
                f.archetype,
                f.maturity,
                f.rhr_bpm,
                f.drop_bpm,
                f.outcome
            ));
            if idx < 5 {
                for line in &f.trace_window {
                    md.push_str(&format!("  - `{line}`\n"));
                }
            }
        }
    }
    md
}

#[test]
#[ignore = "Tier 2 seeded Monte Carlo sweep (~200 Monitoring Sessions); run with cargo test --release --test monte_carlo -- --ignored --nocapture"]
fn tier2_seeded_monte_carlo_sweep() {
    let filter = ProcessEnv
        .get("BPMUTE_MONTE_CARLO_FILTER")
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let trace_failures = ProcessEnv
        .get("BPMUTE_MONTE_CARLO_TRACE_FAILURES")
        .is_some_and(|v| v == "1");
    let mut rng = ChaCha8Rng::seed_from_u64(ROOT_SEED);
    let mut bpm_rr_stats = ModeStatsAccum::new("BPM+RR");
    let mut bpm_only_stats = ModeStatsAccum::new("BPM-only");
    let mut failures = Vec::new();
    let mut min_rhr = f64::INFINITY;
    let mut max_rhr = f64::NEG_INFINITY;
    let mut min_drop = f64::INFINITY;
    let mut max_drop = f64::NEG_INFINITY;

    let sleep_arches = [
        (SleepArchetype::StandardN1N2, "StandardN1N2"),
        (SleepArchetype::SlowDescent, "SlowDescent"),
        (
            SleepArchetype::EveningActiveSpikeThenSleep,
            "EveningActiveSpikeThenSleep",
        ),
        (
            SleepArchetype::PostWorkoutCooldownThenSleep,
            "PostWorkoutCooldownThenSleep",
        ),
    ];
    let wake_arches = [
        (WakeArchetype::MotionlessMovie, "MotionlessMovie"),
        (WakeArchetype::PostWorkoutRecovery, "PostWorkoutRecovery"),
        (WakeArchetype::BriefDrowsyDips, "BriefDrowsyDips"),
        (WakeArchetype::ActiveDeskSpikes, "ActiveDeskSpikes"),
    ];

    // 100 randomized sleep-onset Monitoring Sessions
    for idx in 0..NUM_SLEEP_SESSIONS {
        let pheno = random_phenotype(&mut rng);
        min_rhr = min_rhr.min(pheno.quiet_wake_hr);
        max_rhr = max_rhr.max(pheno.quiet_wake_hr);
        min_drop = min_drop.min(pheno.sleep_drop_bpm);
        max_drop = max_drop.max(pheno.sleep_drop_bpm);

        let (priors, maturity_label) = random_priors(&mut rng, pheno);
        let (arch, arch_name) = sleep_arches[idx % sleep_arches.len()];
        let (stages, total_sec) = build_sleep_stages(&mut rng, arch);
        let trace_seed = rng.r#gen::<u64>();
        let trace = generate_paired_trace(trace_seed, pheno, &stages);
        let onset_ms = trace.onset_sec.expect("sleep trace has N1Onset") * 1000;

        let rr_events =
            apply_stochastic_noise(&mut rng, trace.bpm_rr, total_sec, trace.onset_sec, true);
        let bpm_events = apply_stochastic_noise(
            &mut rng,
            trace.bpm_only_gt5,
            total_sec,
            trace.onset_sec,
            false,
        );

        for (mode_name, events, stats) in [
            ("BPM+RR", &rr_events, &mut bpm_rr_stats),
            ("BPM-only", &bpm_events, &mut bpm_only_stats),
        ] {
            // RNG draws above already executed so filtered runs preserve exact session seeds.
            if !matches_filter(filter.as_deref(), idx, arch_name, mode_name) {
                continue;
            }
            stats.sleep_total += 1;
            let trace = run_engine(events, priors.clone());
            let confirmed = &trace.confirmed_ts;
            let pre_onset = confirmed.iter().find(|&&t| t < onset_ms).copied();
            if let Some(fp_ts) = pre_onset {
                stats.pre_onset_false_positives += 1;
                let window = format_epoch_window(&trace.epochs, fp_ts);
                if trace_failures {
                    eprintln!(
                        "[TRACE] Session #{idx} sleep {mode_name} {arch_name} pre-onset FP:\n  {}",
                        window.join("\n  ")
                    );
                }
                failures.push(SessionFailureRecord {
                    session_index: idx,
                    kind: "sleep".to_string(),
                    mode: mode_name.to_string(),
                    archetype: arch_name.to_string(),
                    maturity: maturity_label.to_string(),
                    rhr_bpm: pheno.quiet_wake_hr,
                    drop_bpm: pheno.sleep_drop_bpm,
                    outcome: format!("pre-onset FP confirm at {:?}", confirmed),
                    trace_window: window,
                });
            } else if let Some(&first_conf) = confirmed.iter().find(|&&t| t >= onset_ms) {
                stats.sleep_true_positives += 1;
                stats
                    .latencies_min
                    .push((first_conf - onset_ms) as f64 / 60_000.0);
            } else {
                let window = format_epoch_window(&trace.epochs, onset_ms + 600_000);
                if trace_failures {
                    eprintln!(
                        "[TRACE] Session #{idx} sleep {mode_name} {arch_name} missed onset:\n  {}",
                        window.join("\n  ")
                    );
                }
                failures.push(SessionFailureRecord {
                    session_index: idx,
                    kind: "sleep".to_string(),
                    mode: mode_name.to_string(),
                    archetype: arch_name.to_string(),
                    maturity: maturity_label.to_string(),
                    rhr_bpm: pheno.quiet_wake_hr,
                    drop_bpm: pheno.sleep_drop_bpm,
                    outcome: "missed sleep onset".to_string(),
                    trace_window: window,
                });
            }
        }
    }

    // 100 randomized hard wake-negative Monitoring Sessions
    for idx in 0..NUM_WAKE_SESSIONS {
        let pheno = random_phenotype(&mut rng);
        min_rhr = min_rhr.min(pheno.quiet_wake_hr);
        max_rhr = max_rhr.max(pheno.quiet_wake_hr);
        min_drop = min_drop.min(pheno.sleep_drop_bpm);
        max_drop = max_drop.max(pheno.sleep_drop_bpm);

        let (priors, maturity_label) = random_priors(&mut rng, pheno);
        let (arch, arch_name) = wake_arches[idx % wake_arches.len()];
        let (stages, total_sec) = build_wake_stages(&mut rng, arch);
        let trace_seed = rng.r#gen::<u64>();
        let trace = generate_paired_trace(trace_seed, pheno, &stages);

        let rr_events = apply_stochastic_noise(&mut rng, trace.bpm_rr, total_sec, None, true);
        let bpm_events =
            apply_stochastic_noise(&mut rng, trace.bpm_only_gt5, total_sec, None, false);

        for (mode_name, events, stats) in [
            ("BPM+RR", &rr_events, &mut bpm_rr_stats),
            ("BPM-only", &bpm_events, &mut bpm_only_stats),
        ] {
            let session_index = NUM_SLEEP_SESSIONS + idx;
            if !matches_filter(filter.as_deref(), session_index, arch_name, mode_name) {
                continue;
            }
            stats.wake_negatives_total += 1;
            let trace = run_engine(events, priors.clone());
            let confirmed = &trace.confirmed_ts;
            if let Some(&first_fp) = confirmed.first() {
                stats.wake_false_positives += 1;
                let window = format_epoch_window(&trace.epochs, first_fp);
                if trace_failures {
                    eprintln!(
                        "[TRACE] Session #{session_index} wake_negative {mode_name} {arch_name} FP:\n  {}",
                        window.join("\n  ")
                    );
                }
                failures.push(SessionFailureRecord {
                    session_index,
                    kind: "wake_negative".to_string(),
                    mode: mode_name.to_string(),
                    archetype: arch_name.to_string(),
                    maturity: maturity_label.to_string(),
                    rhr_bpm: pheno.quiet_wake_hr,
                    drop_bpm: pheno.sleep_drop_bpm,
                    outcome: format!("false positive confirm at {:?}", confirmed),
                    trace_window: window,
                });
            }
        }
    }

    let bpm_rr_report = bpm_rr_stats.finalize(MAX_MEDIAN_LATENCY_BPM_RR_MIN);
    let bpm_only_report = bpm_only_stats.finalize(MAX_MEDIAN_LATENCY_BPM_ONLY_MIN);
    let all_gates_passed = bpm_rr_report.gates_passed && bpm_only_report.gates_passed;

    let report = MonteCarloReport {
        seed: ROOT_SEED,
        total_sessions: bpm_rr_report
            .total_sessions
            .max(bpm_only_report.total_sessions),
        sleep_sessions: bpm_rr_report
            .sleep_sessions
            .max(bpm_only_report.sleep_sessions),
        wake_sessions: bpm_rr_report
            .wake_sessions
            .max(bpm_only_report.wake_sessions),
        rhr_range_bpm: [min_rhr, max_rhr],
        drop_range_bpm: [min_drop, max_drop],
        bpm_rr: bpm_rr_report,
        bpm_only: bpm_only_report,
        all_gates_passed,
        failures,
    };

    let markdown = render_markdown_report(&report);
    println!("{markdown}");

    if let Some(ref active_filter) = filter {
        assert!(
            report.total_sessions > 0,
            "BPMUTE_MONTE_CARLO_FILTER={active_filter:?} matched 0 Monitoring Sessions"
        );
        println!(
            "Filtered debug run (`{active_filter}`): skipping full 200-session report writes and aggregate gates."
        );
        return;
    }

    let out_dir = ProcessEnv
        .get("BPMUTE_MONTE_CARLO_OUT_DIR")
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/tier2-report"));
    fs::create_dir_all(&out_dir).expect("create tier2 report directory");
    fs::write(out_dir.join("monte-carlo-report.md"), markdown.as_bytes())
        .expect("write monte-carlo-report.md");
    let json_bytes = serde_json::to_vec_pretty(&report).expect("serialize monte-carlo-report.json");
    fs::write(out_dir.join("monte-carlo-report.json"), json_bytes)
        .expect("write monte-carlo-report.json");

    assert!(
        report.bpm_rr.gates_passed,
        "Tier 2 BPM+RR gates failed: {:#?}",
        report.bpm_rr
    );
    assert!(
        report.bpm_only.gates_passed,
        "Tier 2 BPM-only gates failed: {:#?}",
        report.bpm_only
    );
}
