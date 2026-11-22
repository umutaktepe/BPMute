use std::collections::VecDeque;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use super::theme::Theme;
use super::trace::{
    TracePoint, TraceSample, TraceSeriesSpec, build_segments, clip_segments, compute_hr_y_bounds,
    is_valid_hr_bpm, render_horizontal_ref_lines, render_hr_y_labels, render_trace_series,
    time_to_column,
};
use crate::ipc::protocol::HrSampleEvent;
use crate::sleep_engine::ContactStatus;

/// Maximum number of 1 Hz samples retained in the 30-minute rolling buffer.
pub const MAX_SPARKLINE_SAMPLES: usize = 1800;

/// Fixed 30-minute Dashboard trace window duration in milliseconds.
pub const DASHBOARD_WINDOW_MS: i64 = 30 * 60 * 1_000;

/// Maximum interval between consecutive valid Dashboard readings before breaking the curve.
pub const DASHBOARD_MAX_GAP_MS: i64 = 5_000;

/// Rolling 30-minute HR buffer for dashboard trace rendering.
#[derive(Debug, Clone)]
pub struct HrSparklineData {
    pub samples: VecDeque<HrSampleEvent>,
    pub quiet_wake_baseline: Option<f64>,
    pub sleeping_hr: Option<f64>,
    pub window_end_ms: Option<i64>,
}

impl Default for HrSparklineData {
    fn default() -> Self {
        Self::new()
    }
}

impl HrSparklineData {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SPARKLINE_SAMPLES),
            quiet_wake_baseline: None,
            sleeping_hr: None,
            window_end_ms: None,
        }
    }

    pub fn push_sample(&mut self, sample: HrSampleEvent) {
        self.window_end_ms = Some(
            self.window_end_ms
                .map_or(sample.ts_ms, |cur| cur.max(sample.ts_ms)),
        );
        if self.samples.len() >= MAX_SPARKLINE_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn set_samples(&mut self, samples: Vec<HrSampleEvent>, now_ms: Option<i64>) {
        self.samples = samples.into();
        while self.samples.len() > MAX_SPARKLINE_SAMPLES {
            self.samples.pop_front();
        }
        let max_sample_ts = self.samples.iter().map(|s| s.ts_ms).max();
        self.window_end_ms =
            max_sample_ts.map(|max_ts| now_ms.map_or(max_ts, |now| max_ts.max(now)));
    }

    pub fn advance_tick(&mut self, delta_ms: i64) {
        if let Some(end) = self.effective_window_end_ms() {
            self.window_end_ms = Some(end.saturating_add(delta_ms));
        }
    }

    pub fn effective_window_end_ms(&self) -> Option<i64> {
        self.window_end_ms
            .or_else(|| self.samples.iter().map(|s| s.ts_ms).max())
    }

    /// Returns the most recent HR sample only if it is still live relative to `now_ms`
    /// (within `DASHBOARD_MAX_GAP_MS`). When `now_ms` is `None` (legacy daemon), falls back
    /// to returning the last buffered sample if any.
    pub fn latest_live_sample(&self, now_ms: Option<i64>) -> Option<&HrSampleEvent> {
        let last = self.samples.back()?;
        if let Some(now) = now_ms
            && now.saturating_sub(last.ts_ms) > DASHBOARD_MAX_GAP_MS
        {
            return None;
        }
        Some(last)
    }

    fn clipped_hr_segments(&self) -> Option<(i64, i64, Vec<Vec<TracePoint>>)> {
        let end_ms = self.effective_window_end_ms()?;
        let start_ms = end_ms.saturating_sub(DASHBOARD_WINDOW_MS);
        let raw: Vec<TraceSample> = self
            .samples
            .iter()
            .map(|s| {
                let valid = s.contact_status != ContactStatus::NoContact && is_valid_hr_bpm(s.bpm);
                TraceSample {
                    ts_ms: s.ts_ms,
                    value: valid.then_some(s.bpm),
                }
            })
            .collect();
        let segments = build_segments(&raw, DASHBOARD_MAX_GAP_MS);
        let clipped = clip_segments(&segments, start_ms, end_ms);
        Some((start_ms, end_ms, clipped))
    }

    fn y_bounds_for(&self, clipped: Option<&(i64, i64, Vec<Vec<TracePoint>>)>) -> (f64, f64) {
        let iter = clipped
            .iter()
            .flat_map(|(_, _, segs)| segs.iter().flat_map(|s| s.iter().map(|p| p.value)));
        compute_hr_y_bounds(iter, &[self.quiet_wake_baseline, self.sleeping_hr])
    }

    pub fn min_max_bpm(&self) -> (f64, f64) {
        let clipped = self.clipped_hr_segments();
        self.y_bounds_for(clipped.as_ref())
    }
}

/// Widget rendering a 30-minute HR trace overlaid with baseline reference lines.
pub struct HrSparklineWidget<'a> {
    pub data: &'a HrSparklineData,
    pub theme: Theme,
}

impl<'a> HrSparklineWidget<'a> {
    pub fn new(data: &'a HrSparklineData, theme: Theme) -> Self {
        Self { data, theme }
    }
}

impl Widget for HrSparklineWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 3 {
            return;
        }

        let label_width = 8u16;
        let chart_x = area.x + label_width;
        let chart_w = area.width.saturating_sub(label_width + 1);
        let chart_h = area.height.saturating_sub(2); // Top 1 row for legend/scale, bottom 1 for time

        if chart_w < 5 || chart_h < 1 {
            return;
        }

        let chart_y = area.y + 1;
        let plot_area = Rect::new(chart_x, chart_y, chart_w, chart_h);
        let clipped_data = self.data.clipped_hr_segments();
        let (min_bpm, max_bpm) = self.data.y_bounds_for(clipped_data.as_ref());

        // Draw Y-axis labels
        render_hr_y_labels(
            area.x,
            chart_y,
            chart_h,
            min_bpm,
            max_bpm,
            self.theme.muted(),
            buf,
        );

        // Render -15m midpoint vertical guide underneath horizontal refs and data curves
        if chart_w > 4 {
            let guide_offset = time_to_column(1, 0, 2, chart_w, self.theme.ascii);
            let guide_x = chart_x + guide_offset;
            let guide_char = if self.theme.ascii { '|' } else { '┆' };
            for row in chart_y..chart_y + chart_h {
                if let Some(c) = buf.cell_mut((guide_x, row)) {
                    c.set_char(guide_char).set_style(self.theme.muted());
                }
            }
        }

        let qw_style = self.theme.sparkline_qw_ref();
        let sleep_style = self.theme.sparkline_sleep_ref();
        let ref_char = if self.theme.ascii { '-' } else { '┄' };

        // Render horizontal reference lines underneath data curves
        render_horizontal_ref_lines(
            plot_area,
            buf,
            min_bpm,
            max_bpm,
            &[
                (self.data.quiet_wake_baseline, qw_style),
                (self.data.sleeping_hr, sleep_style),
            ],
            self.theme.ascii,
        );

        // Render continuous HR trace on top of reference lines and -15m guide
        if let Some((start_ms, end_ms, segments)) = clipped_data {
            let spec = TraceSeriesSpec {
                segments: &segments,
                y_min: min_bpm,
                y_max: max_bpm,
                style: self.theme.sparkline_curve(),
                ascii_point_char: '*',
            };
            render_trace_series(plot_area, buf, start_ms, end_ms, &[spec], self.theme.ascii);
        }

        // Render bottom time scale
        let time_y = chart_y + chart_h;
        if time_y < area.bottom() {
            buf.set_string(chart_x, time_y, "-30m", self.theme.muted());
            if chart_w > 20 {
                let mid_x = chart_x + chart_w / 2 - 2;
                buf.set_string(mid_x, time_y, "-15m", self.theme.muted());
            }
            let now_x = chart_x + chart_w.saturating_sub(3);
            buf.set_string(now_x, time_y, "Now", self.theme.muted());
        }

        // Render legend at top row (pure ASCII under --ascii)
        let mut legend_x = chart_x;
        let hr_legend = if self.theme.ascii {
            "* HR (live)"
        } else {
            "● HR (live)"
        };
        buf.set_string(legend_x, area.y, hr_legend, self.theme.sparkline_curve());
        legend_x += 13;

        if let Some(qw) = self.data.quiet_wake_baseline {
            let qw_str = format!("{ref_char} QW ({:.0})", qw);
            if legend_x + (qw_str.len() as u16) < area.right() {
                buf.set_string(legend_x, area.y, &qw_str, qw_style);
                legend_x += (qw_str.len() as u16) + 2;
            }
        }

        if let Some(sl) = self.data.sleeping_hr {
            let sl_str = format!("{ref_char} Sleep ({:.0})", sl);
            if legend_x + (sl_str.len() as u16) < area.right() {
                buf.set_string(legend_x, area.y, &sl_str, sleep_style);
            }
        }
    }
}
