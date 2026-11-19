#![allow(
    dead_code,
    reason = "shared test harness items are used selectively by separate integration test binaries"
)]

use bpmute::sleep_engine::{ContactStatus, EngineEvent, HrPacket, StreamState, StreamStatus};
use rand::Rng;
use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::SeedableRng;

/// Physiological phenotype controlling synthetic Monitoring Sessions.
#[derive(Debug, Clone, Copy)]
pub struct Phenotype {
    pub quiet_wake_hr: f64,
    pub sleep_drop_bpm: f64,
    pub wake_rmssd_ms: f64,
    pub sleep_rmssd_ms: f64,
}

impl Phenotype {
    pub fn typical() -> Self {
        Self {
            quiet_wake_hr: 66.0,
            sleep_drop_bpm: 7.5,
            wake_rmssd_ms: 34.0,
            sleep_rmssd_ms: 44.0,
        }
    }

    pub fn athletic_low_rhr() -> Self {
        Self {
            quiet_wake_hr: 50.0,
            sleep_drop_bpm: 6.0,
            wake_rmssd_ms: 58.0,
            sleep_rmssd_ms: 72.0,
        }
    }

    pub fn high_rhr() -> Self {
        Self {
            quiet_wake_hr: 80.0,
            sleep_drop_bpm: 9.0,
            wake_rmssd_ms: 22.0,
            sleep_rmssd_ms: 29.0,
        }
    }
}

/// Physiological scenario stages synthesized from a continuous 1 Hz latent trajectory.
#[derive(Debug, Clone, Copy)]
pub enum StageKind {
    QuietWake {
        duration_sec: i64,
    },
    ActiveWakeSpike {
        duration_sec: i64,
        elevation_bpm: f64,
    },
    PostWorkoutCooldown {
        duration_sec: i64,
        initial_excess_bpm: f64,
    },
    DrowsyDip {
        duration_sec: i64,
        dip_bpm: f64,
    },
    N1Onset {
        duration_sec: i64,
    },
    N2Sleep {
        duration_sec: i64,
    },
    Arousal {
        duration_sec: i64,
    },
    PostSleepWake {
        duration_sec: i64,
        elevation_bpm: f64,
    },
}

/// Ground-truth realization at 1 Hz from which both `BPM+RR` and `BPM-only` streams are derived.
#[derive(Debug, Clone)]
pub struct LatentSample {
    pub sec: i64,
    pub hr_bpm: f64,
    pub rmssd_ms: f64,
    pub slow_sd_ms: f64,
    pub stage_tag: &'static str,
}

/// Paired synthetic outputs generated from identical underlying physiology.
#[derive(Debug, Clone)]
pub struct PairedTrace {
    pub bpm_rr: Vec<EngineEvent>,
    pub bpm_only_gt5: Vec<EngineEvent>,
    pub onset_sec: Option<i64>,
}

fn sample_normal(rng: &mut ChaCha8Rng) -> f64 {
    let u1: f64 = rng.gen_range(1e-6_f64..1.0_f64);
    let u2: f64 = rng.gen_range(0.0_f64..std::f64::consts::TAU);
    (-2.0 * u1.ln()).sqrt() * u2.cos()
}

/// Synthesizes a continuous 1 Hz latent physiological realization and derives both `BPM+RR`
/// and Huawei Watch GT5-profile `BPM-only` event streams.
pub fn generate_paired_trace(seed: u64, phenotype: Phenotype, stages: &[StageKind]) -> PairedTrace {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut latent = Vec::new();
    let mut sec = 1_i64;
    let mut onset_sec = None;

    let mut ar_hr = 0.0_f64;
    let mut cur_hr = phenotype.quiet_wake_hr;

    for &stage in stages {
        let dur = match stage {
            StageKind::QuietWake { duration_sec }
            | StageKind::ActiveWakeSpike { duration_sec, .. }
            | StageKind::PostWorkoutCooldown { duration_sec, .. }
            | StageKind::DrowsyDip { duration_sec, .. }
            | StageKind::N1Onset { duration_sec }
            | StageKind::N2Sleep { duration_sec }
            | StageKind::Arousal { duration_sec }
            | StageKind::PostSleepWake { duration_sec, .. } => duration_sec,
        };

        if matches!(stage, StageKind::N1Onset { .. }) && onset_sec.is_none() {
            onset_sec = Some(sec);
        }

        let start_hr_for_stage = cur_hr;
        for step in 0..dur {
            let frac = (step as f64) / (dur.max(1) as f64);
            let (target_hr, ar_phi, ar_sigma, rmssd, slow_sd, tag) = match stage {
                StageKind::QuietWake { .. } => {
                    // Subtle movie-watching relaxation drifts (+/- 1.2 bpm) while keeping waking
                    // sympathetic variability (~2.1 bpm local MAD and realistic RMSSD/SDNN ~0.60)
                    let drift = 1.2 * (sec as f64 * 0.015).sin();
                    (
                        phenotype.quiet_wake_hr + drift,
                        0.88,
                        1.05,
                        phenotype.wake_rmssd_ms,
                        1.85 * phenotype.wake_rmssd_ms,
                        "quiet_wake",
                    )
                }
                StageKind::ActiveWakeSpike { elevation_bpm, .. } => (
                    phenotype.quiet_wake_hr + elevation_bpm,
                    0.82,
                    1.65,
                    phenotype.wake_rmssd_ms * 0.78,
                    2.20 * phenotype.wake_rmssd_ms,
                    "active_wake",
                ),
                StageKind::PostWorkoutCooldown {
                    initial_excess_bpm, ..
                } => {
                    // Exponential recovery curve back down to quiet_wake_hr
                    let decay = (-3.2 * frac).exp();
                    (
                        phenotype.quiet_wake_hr + initial_excess_bpm * decay,
                        0.85,
                        1.15,
                        phenotype.wake_rmssd_ms * (0.75 + 0.25 * (1.0 - decay)),
                        2.00 * phenotype.wake_rmssd_ms,
                        "post_workout",
                    )
                }
                StageKind::DrowsyDip { dip_bpm, .. } => {
                    let bell = (std::f64::consts::PI * frac).sin();
                    (
                        phenotype.quiet_wake_hr - dip_bpm * bell,
                        0.85,
                        0.90,
                        phenotype.wake_rmssd_ms * (1.0 + 0.1 * bell),
                        1.50 * phenotype.wake_rmssd_ms,
                        "drowsy_dip",
                    )
                }
                StageKind::N1Onset { .. } => {
                    let end_target = phenotype.quiet_wake_hr - phenotype.sleep_drop_bpm * 0.75;
                    let target = start_hr_for_stage + (end_target - start_hr_for_stage) * frac;
                    let rmssd = phenotype.wake_rmssd_ms
                        + (phenotype.sleep_rmssd_ms - phenotype.wake_rmssd_ms) * frac;
                    let slow_start = 1.75 * phenotype.wake_rmssd_ms;
                    let slow_end = 0.55 * phenotype.sleep_rmssd_ms;
                    let slow_sd = slow_start + (slow_end - slow_start) * frac;
                    (target, 0.80, 0.42, rmssd, slow_sd, "n1_onset")
                }
                StageKind::N2Sleep { .. } => (
                    phenotype.quiet_wake_hr - phenotype.sleep_drop_bpm,
                    0.75,
                    0.30,
                    phenotype.sleep_rmssd_ms,
                    0.45 * phenotype.sleep_rmssd_ms,
                    "n2_sleep",
                ),
                StageKind::Arousal { .. } => (
                    phenotype.quiet_wake_hr + 4.0,
                    0.82,
                    1.55,
                    phenotype.wake_rmssd_ms * 0.80,
                    1.90 * phenotype.wake_rmssd_ms,
                    "arousal",
                ),
                StageKind::PostSleepWake { elevation_bpm, .. } => (
                    phenotype.quiet_wake_hr + elevation_bpm,
                    0.86,
                    1.20,
                    phenotype.wake_rmssd_ms * 0.92,
                    1.90 * phenotype.wake_rmssd_ms,
                    "post_sleep_wake",
                ),
            };

            ar_hr = ar_phi * ar_hr + ar_sigma * sample_normal(&mut rng);
            // Sympathetic 0.05 Hz oscillation in wake, suppressed in sleep
            let lf_amp = if matches!(stage, StageKind::N2Sleep { .. }) {
                0.15
            } else if matches!(stage, StageKind::N1Onset { .. }) {
                1.2 * (1.0 - 0.7 * frac)
            } else {
                1.25
            };
            let lf_osc = lf_amp * (sec as f64 * 0.28).sin();
            cur_hr = (target_hr + ar_hr + lf_osc).clamp(36.0, 185.0);

            latent.push(LatentSample {
                sec,
                hr_bpm: cur_hr,
                rmssd_ms: rmssd,
                slow_sd_ms: slow_sd,
                stage_tag: tag,
            });
            sec += 1;
        }
    }

    let mut bpm_rr = Vec::with_capacity(latent.len() + 1);
    bpm_rr.push(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));

    let mut phase_rr = 0.0_f64;
    let mut rr_accum_ms = 0.0_f64;
    for s in &latent {
        let mean_rr = 60_000.0 / s.hr_bpm;
        let rsa_amp = s.rmssd_ms * 0.707;
        let slow_drift = s.slow_sd_ms * (s.sec as f64 * 0.16).sin();
        let mut beats = Vec::new();
        rr_accum_ms += 1000.0;
        while rr_accum_ms >= mean_rr * 0.75 {
            phase_rr += 1.45;
            let noise = (s.rmssd_ms * 0.22) * sample_normal(&mut rng);
            let rr = (mean_rr + rsa_amp * phase_rr.sin() + slow_drift + noise).clamp(320.0, 1950.0);
            beats.push(rr);
            rr_accum_ms -= rr;
            if rr_accum_ms < 0.0 {
                break;
            }
        }
        if beats.is_empty() {
            beats.push(mean_rr.clamp(320.0, 1950.0));
        }

        bpm_rr.push(EngineEvent::HrPacket(HrPacket {
            ts_ms: s.sec * 1000,
            bpm: s.hr_bpm.round(),
            rr_ms: beats,
            contact_status: ContactStatus::Contact,
        }));
    }

    // Huawei Watch GT5 profile: integer-quantized BPM-only with bimodal BLE interval
    // (~938 ms and ~1406 ms averaging ~1.005 Hz).
    let mut bpm_only_gt5 = Vec::with_capacity(latent.len() + 1);
    bpm_only_gt5.push(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Streaming,
    }));
    let total_ms = (latent.len() as i64) * 1000;
    let mut gt5_ts = 938_i64;
    let mut tick = 0_u64;
    while gt5_ts <= total_ms {
        let idx = ((gt5_ts as usize) / 1000).min(latent.len() - 1);
        bpm_only_gt5.push(EngineEvent::HrPacket(HrPacket {
            ts_ms: gt5_ts,
            bpm: latent[idx].hr_bpm.round(),
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        }));
        tick += 1;
        // ~6 of 7 intervals are 938 ms, ~1 of 7 is 1406 ms -> mean = (6*938 + 1406)/7 = 1004.8 ms
        let step_ms = if tick.is_multiple_of(7) { 1406 } else { 938 };
        gt5_ts += step_ms;
    }

    PairedTrace {
        bpm_rr,
        bpm_only_gt5,
        onset_sec,
    }
}

/// Drops packets before `lock_delay_ms` and prepends `StreamState::Acquiring` at `t=0`.
pub fn inject_gt5_optical_lock_ramp(
    events: &[EngineEvent],
    lock_delay_ms: i64,
) -> Vec<EngineEvent> {
    let mut out = Vec::with_capacity(events.len() + 1);
    out.push(EngineEvent::StreamState(StreamState {
        ts_ms: 0,
        status: StreamStatus::Acquiring,
    }));
    for ev in events {
        if let EngineEvent::StreamState(st) = ev
            && st.ts_ms == 0
        {
            continue;
        }
        if ev.ts_ms() >= lock_delay_ms {
            out.push(ev.clone());
        }
    }
    out
}

/// Injects ectopy/motion RR corruption on `every_n`-th packet inside `[start_ms, end_ms]`.
pub fn inject_rr_corruption(
    events: &[EngineEvent],
    start_ms: i64,
    end_ms: i64,
    every_n: usize,
) -> Vec<EngineEvent> {
    let mut counter = 0_usize;
    events
        .iter()
        .map(|ev| match ev {
            EngineEvent::HrPacket(pkt) if pkt.ts_ms >= start_ms && pkt.ts_ms <= end_ms => {
                let mut cloned = pkt.clone();
                counter += 1;
                if every_n > 0 && counter.is_multiple_of(every_n) && !cloned.rr_ms.is_empty() {
                    cloned.rr_ms[0] = 390.0;
                }
                EngineEvent::HrPacket(cloned)
            }
            other => other.clone(),
        })
        .collect()
}

/// Injects transient motion BPM spikes (`+delta_bpm`) on exact packet timestamps near `spike_sec`.
pub fn inject_hampel_spikes(
    events: &[EngineEvent],
    spike_secs: &[i64],
    delta_bpm: f64,
) -> Vec<EngineEvent> {
    events
        .iter()
        .map(|ev| match ev {
            EngineEvent::HrPacket(pkt)
                if spike_secs
                    .iter()
                    .any(|&s| (pkt.ts_ms - s * 1000).abs() <= 600) =>
            {
                let mut cloned = pkt.clone();
                cloned.bpm = (cloned.bpm + delta_bpm).min(198.0);
                EngineEvent::HrPacket(cloned)
            }
            other => other.clone(),
        })
        .collect()
}

/// Freezes `bpm` (and optionally `rr_ms`) across `[start_ms, end_ms]` to simulate firmware flatlines.
pub fn inject_flatline(
    events: &[EngineEvent],
    start_ms: i64,
    end_ms: i64,
    freeze_rr: bool,
) -> Vec<EngineEvent> {
    let mut anchor_bpm: Option<f64> = None;
    let mut anchor_rr: Option<Vec<f64>> = None;

    events
        .iter()
        .map(|ev| match ev {
            EngineEvent::HrPacket(pkt) if pkt.ts_ms >= start_ms && pkt.ts_ms <= end_ms => {
                let bpm = *anchor_bpm.get_or_insert(pkt.bpm);
                let rr_ms = if freeze_rr {
                    anchor_rr
                        .get_or_insert_with(|| {
                            if pkt.rr_ms.is_empty() {
                                vec![]
                            } else {
                                vec![pkt.rr_ms[0]]
                            }
                        })
                        .clone()
                } else {
                    pkt.rr_ms.clone()
                };
                EngineEvent::HrPacket(HrPacket {
                    ts_ms: pkt.ts_ms,
                    bpm,
                    rr_ms,
                    contact_status: pkt.contact_status,
                })
            }
            other => other.clone(),
        })
        .collect()
}

/// Removes packets across `[start_ms, end_ms]` (optionally inserting `SignalLost`/`Streaming`).
pub fn inject_silent_gap(
    events: &[EngineEvent],
    start_ms: i64,
    end_ms: i64,
    with_stream_state: bool,
) -> Vec<EngineEvent> {
    let mut out = Vec::with_capacity(events.len() + 2);
    let mut emitted_lost = false;
    let mut emitted_resumed = false;

    for ev in events {
        let t = ev.ts_ms();
        if t >= start_ms && t < end_ms {
            if with_stream_state && !emitted_lost {
                out.push(EngineEvent::StreamState(StreamState {
                    ts_ms: start_ms,
                    status: StreamStatus::SignalLost,
                }));
                emitted_lost = true;
            }
            continue;
        }
        if t >= end_ms && with_stream_state && emitted_lost && !emitted_resumed {
            out.push(EngineEvent::StreamState(StreamState {
                ts_ms: end_ms,
                status: StreamStatus::Streaming,
            }));
            emitted_resumed = true;
        }
        out.push(ev.clone());
    }
    out
}
