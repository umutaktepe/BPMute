use chrono::{Local, LocalResult, TimeDelta, TimeZone, Timelike};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::time_to_column;

const CLOCK_TICK_STEPS_MIN: &[i64] = &[1, 2, 5, 10, 15, 30, 60, 120, 180, 240, 360, 720];
const CLOCK_LABEL_W: u16 = 5;
const CLOCK_MIN_LABEL_GAP: u16 = 2;

/// Local wall-clock X-axis tick aligned to a chart column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockTick {
    pub ts_ms: i64,
    pub col: u16,
    pub label_x: u16,
    pub label: String,
}

/// Computes round local wall-clock X-axis ticks and non-overlapping label positions for `[t_start_ms, t_end_ms]`.
pub fn compute_local_clock_ticks(
    t_start_ms: i64,
    t_end_ms: i64,
    chart_w: u16,
    ascii: bool,
) -> Vec<ClockTick> {
    compute_local_clock_ticks_in_tz(&Local, t_start_ms, t_end_ms, chart_w, ascii)
}

fn compute_local_clock_ticks_in_tz<Tz: TimeZone>(
    tz: &Tz,
    t_start_ms: i64,
    t_end_ms: i64,
    chart_w: u16,
    ascii: bool,
) -> Vec<ClockTick>
where
    Tz::Offset: std::fmt::Display,
{
    if t_end_ms <= t_start_ms || chart_w < CLOCK_LABEL_W {
        return Vec::new();
    }

    for &step_min in CLOCK_TICK_STEPS_MIN {
        let candidates = place_ticks_for_step(tz, t_start_ms, t_end_ms, chart_w, ascii, step_min);
        let kept = filter_overlapping_ticks(&candidates, CLOCK_MIN_LABEL_GAP);
        if kept.len() == candidates.len() {
            return kept;
        }
    }

    // Fallback: widest step with greedy collision filtering so labels never overlap even on tiny viewports.
    let widest = *CLOCK_TICK_STEPS_MIN.last().unwrap_or(&720);
    let candidates = place_ticks_for_step(tz, t_start_ms, t_end_ms, chart_w, ascii, widest);
    filter_overlapping_ticks(&candidates, 1)
}

/// Renders subtle vertical time gridlines at `ticks` columns underneath reference lines and curves.
pub fn render_vertical_time_gridlines(
    plot_area: Rect,
    buf: &mut Buffer,
    ticks: &[ClockTick],
    style: Style,
    ascii: bool,
) {
    if plot_area.width == 0 || plot_area.height == 0 {
        return;
    }
    let guide_char = if ascii { '|' } else { '┆' };
    for tick in ticks {
        if tick.col >= plot_area.width {
            continue;
        }
        let x = plot_area.x + tick.col;
        for row_y in plot_area.y..plot_area.bottom() {
            if let Some(cell) = buf.cell_mut((x, row_y)) {
                cell.set_char(guide_char).set_style(style);
            }
        }
    }
}

/// Renders local `HH:MM` X-axis labels along the axis row directly beneath `plot_area`.
pub fn render_local_clock_x_axis(
    chart_x: u16,
    axis_y: u16,
    chart_w: u16,
    ticks: &[ClockTick],
    style: Style,
    buf: &mut Buffer,
) {
    if chart_w < CLOCK_LABEL_W {
        return;
    }
    for tick in ticks {
        if tick.label_x + CLOCK_LABEL_W <= chart_w {
            buf.set_string(chart_x + tick.label_x, axis_y, &tick.label, style);
        }
    }
}

struct WallTick {
    ts_ms: i64,
    label: String,
}

fn place_ticks_for_step<Tz: TimeZone>(
    tz: &Tz,
    t_start_ms: i64,
    t_end_ms: i64,
    chart_w: u16,
    ascii: bool,
    step_min: i64,
) -> Vec<ClockTick>
where
    Tz::Offset: std::fmt::Display,
{
    generate_wall_ticks(tz, t_start_ms, t_end_ms, step_min)
        .into_iter()
        .map(|tick| {
            let col = time_to_column(tick.ts_ms, t_start_ms, t_end_ms, chart_w, ascii);
            let label_x = col
                .saturating_sub(2)
                .min(chart_w.saturating_sub(CLOCK_LABEL_W));
            ClockTick {
                ts_ms: tick.ts_ms,
                col,
                label_x,
                label: tick.label,
            }
        })
        .collect()
}

fn filter_overlapping_ticks(ticks: &[ClockTick], min_gap: u16) -> Vec<ClockTick> {
    let mut kept = Vec::with_capacity(ticks.len());
    let mut prev_end: Option<u16> = None;
    for tick in ticks {
        if let Some(end) = prev_end
            && tick.label_x < end.saturating_add(min_gap)
        {
            continue;
        }
        prev_end = Some(tick.label_x + CLOCK_LABEL_W);
        kept.push(tick.clone());
    }
    kept
}

fn generate_wall_ticks<Tz: TimeZone>(
    tz: &Tz,
    t_start_ms: i64,
    t_end_ms: i64,
    step_min: i64,
) -> Vec<WallTick>
where
    Tz::Offset: std::fmt::Display,
{
    let Some(start_dt) = tz.timestamp_millis_opt(t_start_ms).earliest() else {
        return Vec::new();
    };
    let Some(end_dt) = tz.timestamp_millis_opt(t_end_ms).latest() else {
        return Vec::new();
    };

    let start_naive = start_dt.naive_local();
    let end_limit = end_dt.naive_local() + TimeDelta::minutes(step_min);
    let mut cur = if step_min < 60 {
        let step_m = step_min as u32;
        let floored_min = (start_naive.minute() / step_m) * step_m;
        start_naive
            .date()
            .and_hms_opt(start_naive.hour(), floored_min, 0)
            .unwrap_or(start_naive)
    } else {
        let step_h = (step_min / 60) as u32;
        let floored_hour = (start_naive.hour() / step_h) * step_h;
        start_naive
            .date()
            .and_hms_opt(floored_hour, 0, 0)
            .unwrap_or(start_naive)
    };

    let mut out = Vec::new();
    let step_delta = TimeDelta::minutes(step_min);
    for _ in 0..4096 {
        if cur > end_limit {
            break;
        }
        match tz.from_local_datetime(&cur) {
            LocalResult::Single(dt) => {
                let ts = dt.timestamp_millis();
                if ts > t_end_ms {
                    break;
                }
                if ts >= t_start_ms {
                    out.push(WallTick {
                        ts_ms: ts,
                        label: dt.format("%H:%M").to_string(),
                    });
                }
            }
            LocalResult::Ambiguous(dt1, dt2) => {
                let ts1 = dt1.timestamp_millis();
                let ts2 = dt2.timestamp_millis();
                if ts1 > t_end_ms {
                    break;
                }
                for dt in [&dt1, &dt2] {
                    let ts = dt.timestamp_millis();
                    if ts >= t_start_ms && ts <= t_end_ms {
                        out.push(WallTick {
                            ts_ms: ts,
                            label: dt.format("%H:%M").to_string(),
                        });
                    }
                    if ts1 == ts2 {
                        break;
                    }
                }
            }
            LocalResult::None => {}
        }
        cur += step_delta;
    }
    out.sort_by_key(|t| t.ts_ms);
    out.dedup_by_key(|t| t.ts_ms);
    out
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, NaiveDate, NaiveDateTime};

    use super::super::column_to_time;
    use super::*;

    /// Synthetic timezone with DST spring-forward (`02:00..03:00` skipped on 2026-03-08)
    /// and DST fall-back (`01:00..02:00` repeated on 2026-11-01).
    #[derive(Clone, Copy)]
    struct DstTestTz;

    impl DstTestTz {
        const EDT: i32 = -4 * 3600;
        const EST: i32 = -5 * 3600;

        fn spring_utc() -> i64 {
            // 2026-03-08 02:00 EST == 07:00 UTC -> jumps to 03:00 EDT
            NaiveDate::from_ymd_opt(2026, 3, 8)
                .unwrap()
                .and_hms_opt(7, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_millis()
        }

        fn fall_utc() -> i64 {
            // 2026-11-01 02:00 EDT == 06:00 UTC -> falls back to 01:00 EST
            NaiveDate::from_ymd_opt(2026, 11, 1)
                .unwrap()
                .and_hms_opt(6, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_millis()
        }
    }

    impl TimeZone for DstTestTz {
        type Offset = FixedOffset;

        fn from_offset(offset: &Self::Offset) -> Self {
            let _ = offset;
            Self
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<Self::Offset> {
            let spring_d = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
            let fall_d = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
            let secs = if *local >= spring_d && *local < fall_d {
                Self::EDT
            } else {
                Self::EST
            };
            LocalResult::Single(FixedOffset::east_opt(secs).unwrap())
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            let spring_start = NaiveDate::from_ymd_opt(2026, 3, 8)
                .unwrap()
                .and_hms_opt(2, 0, 0)
                .unwrap();
            let spring_end = NaiveDate::from_ymd_opt(2026, 3, 8)
                .unwrap()
                .and_hms_opt(3, 0, 0)
                .unwrap();
            let fall_start = NaiveDate::from_ymd_opt(2026, 11, 1)
                .unwrap()
                .and_hms_opt(1, 0, 0)
                .unwrap();
            let fall_end = NaiveDate::from_ymd_opt(2026, 11, 1)
                .unwrap()
                .and_hms_opt(2, 0, 0)
                .unwrap();

            if *local >= spring_start && *local < spring_end {
                return LocalResult::None;
            }
            if *local >= fall_start && *local < fall_end {
                return LocalResult::Ambiguous(
                    FixedOffset::east_opt(Self::EDT).unwrap(),
                    FixedOffset::east_opt(Self::EST).unwrap(),
                );
            }
            let secs = if *local >= spring_end && *local < fall_start {
                Self::EDT
            } else {
                Self::EST
            };
            LocalResult::Single(FixedOffset::east_opt(secs).unwrap())
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> Self::Offset {
            let spring_d = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
            let fall_d = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
            let secs = if *utc >= spring_d && *utc < fall_d {
                Self::EDT
            } else {
                Self::EST
            };
            FixedOffset::east_opt(secs).unwrap()
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
            let ts = utc.and_utc().timestamp_millis();
            let secs = if ts >= Self::spring_utc() && ts < Self::fall_utc() {
                Self::EDT
            } else {
                Self::EST
            };
            FixedOffset::east_opt(secs).unwrap()
        }
    }

    #[test]
    fn non_integer_utc_offset_aligns_to_round_local_hours() {
        let ist = FixedOffset::east_opt(5 * 3600 + 1800).unwrap(); // UTC+05:30
        let start = ist
            .with_ymd_and_hms(2026, 1, 10, 23, 15, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let end = start + 8 * 3_600_000;
        let ticks = compute_local_clock_ticks_in_tz(&ist, start, end, 40, false);
        let labels: Vec<&str> = ticks.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, vec!["00:00", "02:00", "04:00", "06:00"]);
    }

    #[test]
    fn dst_spring_forward_and_fall_back_preserve_sorted_subhour_ticks() {
        // Spring forward: 01:00 EST .. 04:00 EDT skips nonexistent 02:00
        let spring_start = DstTestTz::spring_utc() - 3_600_000;
        let spring_end = DstTestTz::spring_utc() + 3_600_000;
        let spring_ticks =
            compute_local_clock_ticks_in_tz(&DstTestTz, spring_start, spring_end, 48, false);
        let spring_labels: Vec<&str> = spring_ticks.iter().map(|t| t.label.as_str()).collect();
        assert!(spring_labels.contains(&"01:00"));
        assert!(!spring_labels.contains(&"02:00"));
        assert!(spring_labels.contains(&"03:00"));

        // Fall back ending inside pass-1 (00:45 EDT .. 01:45 EDT): must NOT drop 01:15..01:45
        let fall_pass1_start = DstTestTz::fall_utc() - 75 * 60_000; // 00:45 EDT
        let fall_pass1_end = DstTestTz::fall_utc() - 15 * 60_000; // 01:45 EDT
        let pass1_ticks = compute_local_clock_ticks_in_tz(
            &DstTestTz,
            fall_pass1_start,
            fall_pass1_end,
            56,
            false,
        );
        let pass1_labels: Vec<&str> = pass1_ticks.iter().map(|t| t.label.as_str()).collect();
        assert!(
            pass1_labels.contains(&"01:10")
                && pass1_labels.contains(&"01:20")
                && pass1_labels.contains(&"01:30")
                && pass1_labels.contains(&"01:40"),
            "Sub-hour ticks during pass-1 of DST fall-back must be preserved: {pass1_labels:?}"
        );

        // Fall back spanning both passes (00:30 EDT .. 02:30 EST) must stay strictly sorted
        let both_start = DstTestTz::fall_utc() - 90 * 60_000;
        let both_end = DstTestTz::fall_utc() + 90 * 60_000;
        let both_ticks =
            compute_local_clock_ticks_in_tz(&DstTestTz, both_start, both_end, 64, false);
        assert!(
            both_ticks.windows(2).all(|w| w[0].ts_ms < w[1].ts_ms),
            "Fall-back ticks must be strictly chronological: {both_ticks:?}"
        );
    }

    #[test]
    fn column_to_time_round_trips_time_to_column_in_braille_and_ascii() {
        let start = 1_700_000_000_000i64;
        let end = start + 40 * 60_000;
        for ascii in [false, true] {
            for col in 0..32u16 {
                let ts = column_to_time(col, start, end, 32, ascii);
                assert_eq!(time_to_column(ts, start, end, 32, ascii), col);
            }
        }
    }
}
