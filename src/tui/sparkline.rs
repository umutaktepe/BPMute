use std::collections::VecDeque;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

use super::theme::Theme;
use crate::ipc::protocol::HrSampleEvent;

/// Maximum number of 1 Hz samples retained in the 30-minute rolling buffer.
pub const MAX_SPARKLINE_SAMPLES: usize = 1800;

/// Rolling 30-minute HR buffer for dashboard sparkline rendering.
#[derive(Debug, Clone)]
pub struct HrSparklineData {
    pub samples: VecDeque<HrSampleEvent>,
    pub quiet_wake_baseline: Option<f64>,
    pub sleeping_hr: Option<f64>,
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
        }
    }

    pub fn push_sample(&mut self, sample: HrSampleEvent) {
        if self.samples.len() >= MAX_SPARKLINE_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn set_samples(&mut self, samples: Vec<HrSampleEvent>) {
        self.samples = samples.into();
        while self.samples.len() > MAX_SPARKLINE_SAMPLES {
            self.samples.pop_front();
        }
    }

    pub fn latest_bpm(&self) -> Option<f64> {
        self.samples.back().map(|s| s.bpm)
    }

    pub fn min_max_bpm(&self) -> (f64, f64) {
        let mut min = 50.0;
        let mut max = 100.0;
        for s in &self.samples {
            if s.bpm < min {
                min = s.bpm;
            }
            if s.bpm > max {
                max = s.bpm;
            }
        }
        if let Some(qw) = self.quiet_wake_baseline {
            if qw < min {
                min = qw;
            }
            if qw > max {
                max = qw;
            }
        }
        if let Some(sl) = self.sleeping_hr {
            if sl < min {
                min = sl;
            }
            if sl > max {
                max = sl;
            }
        }
        (min - 2.0, max + 2.0)
    }
}

/// Widget rendering a 30-minute HR sparkline overlaid with baseline reference lines.
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

        let (min_bpm, max_bpm) = self.data.min_max_bpm();
        let bpm_range = (max_bpm - min_bpm).max(1.0);

        // Draw Y-axis labels
        let top_label = format!("{:>3.0}bpm", max_bpm);
        let bot_label = format!("{:>3.0}bpm", min_bpm);
        buf.set_string(area.x, area.y + 1, &top_label, self.theme.muted());
        buf.set_string(
            area.x,
            area.y + 1 + chart_h.saturating_sub(1),
            &bot_label,
            self.theme.muted(),
        );

        // Calculate reference line rows
        let qw_row = self.data.quiet_wake_baseline.map(|qw| {
            let norm = ((qw - min_bpm) / bpm_range).clamp(0.0, 1.0);
            let row = ((1.0 - norm) * (chart_h.saturating_sub(1) as f64)).round() as u16;
            area.y + 1 + row
        });

        let sleep_row = self.data.sleeping_hr.map(|sl| {
            let norm = ((sl - min_bpm) / bpm_range).clamp(0.0, 1.0);
            let row = ((1.0 - norm) * (chart_h.saturating_sub(1) as f64)).round() as u16;
            area.y + 1 + row
        });

        let qw_style = if self.theme.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::Cyan)
        };

        let sleep_style = if self.theme.no_color {
            Style::default().add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::Magenta)
        };

        let ref_char = if self.theme.ascii { '-' } else { '┄' };

        // Render reference lines first
        if let Some(row) = qw_row
            && row < area.y + 1 + chart_h
        {
            for col in 0..chart_w {
                if let Some(c) = buf.cell_mut((chart_x + col, row)) {
                    c.set_char(ref_char).set_style(qw_style);
                }
            }
        }
        if let Some(row) = sleep_row
            && row < area.y + 1 + chart_h
        {
            for col in 0..chart_w {
                if let Some(c) = buf.cell_mut((chart_x + col, row)) {
                    c.set_char(ref_char).set_style(sleep_style);
                }
            }
        }

        // Render sparkline columns
        let total_samples = self.data.samples.len();
        if total_samples > 0 {
            let samples_vec: Vec<&HrSampleEvent> = self.data.samples.iter().collect();
            let hr_style = if self.theme.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };

            for col in 0..chart_w {
                let start_idx = (col as usize * total_samples) / chart_w as usize;
                let end_idx = (((col + 1) as usize * total_samples) / chart_w as usize)
                    .max(start_idx + 1)
                    .min(total_samples);

                let slice = &samples_vec[start_idx..end_idx];
                if slice.is_empty() {
                    continue;
                }
                let avg_bpm = slice.iter().map(|s| s.bpm).sum::<f64>() / (slice.len() as f64);
                let norm = ((avg_bpm - min_bpm) / bpm_range).clamp(0.0, 1.0);
                let row_offset = ((1.0 - norm) * (chart_h.saturating_sub(1) as f64)).round() as u16;
                let target_y = area.y + 1 + row_offset;

                let symbol = if self.theme.ascii {
                    if norm > 0.8 {
                        '#'
                    } else if norm > 0.6 {
                        '*'
                    } else if norm > 0.4 {
                        '='
                    } else if norm > 0.2 {
                        '-'
                    } else {
                        '.'
                    }
                } else if norm > 0.8 {
                    '█'
                } else if norm > 0.6 {
                    '⣾'
                } else if norm > 0.4 {
                    '⠶'
                } else if norm > 0.2 {
                    '⠤'
                } else {
                    '⠄'
                };

                if target_y < area.y + 1 + chart_h
                    && let Some(cell) = buf.cell_mut((chart_x + col, target_y))
                {
                    cell.set_char(symbol).set_style(hr_style);
                }
            }
        }

        // Render bottom time scale
        let time_y = area.y + 1 + chart_h;
        if time_y < area.bottom() {
            buf.set_string(chart_x, time_y, "-30m", self.theme.muted());
            if chart_w > 20 {
                let mid_x = chart_x + chart_w / 2 - 2;
                buf.set_string(mid_x, time_y, "-15m", self.theme.muted());
            }
            let now_x = chart_x + chart_w.saturating_sub(3);
            buf.set_string(now_x, time_y, "Now", self.theme.muted());
        }

        // Render legend at top row
        let mut legend_x = chart_x;
        buf.set_string(legend_x, area.y, "● HR (live)", self.theme.success());
        legend_x += 13;

        if let Some(qw) = self.data.quiet_wake_baseline {
            let qw_str = format!("┄ QW ({:.0})", qw);
            if legend_x + (qw_str.len() as u16) < area.right() {
                buf.set_string(legend_x, area.y, &qw_str, qw_style);
                legend_x += (qw_str.len() as u16) + 2;
            }
        }

        if let Some(sl) = self.data.sleeping_hr {
            let sl_str = format!("┄ Sleep ({:.0})", sl);
            if legend_x + (sl_str.len() as u16) < area.right() {
                buf.set_string(legend_x, area.y, &sl_str, sleep_style);
            }
        }
    }
}
