use chrono::{Local, TimeZone};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::super::app::TuiApp;
use super::super::hit_map::{ClickTarget, HitMap};
use super::super::trace::{
    TraceSample, TraceSeriesSpec, build_segments, clip_segments, column_to_time,
    compute_hr_y_bounds, compute_local_clock_ticks, is_valid_hr_bpm, render_horizontal_ref_lines,
    render_hr_y_labels, render_local_clock_x_axis, render_trace_series,
    render_vertical_time_gridlines, time_to_column,
};
use crate::sleep_engine::Epoch;
use crate::storage::{SessionFullDetail, SessionListEntry, SessionStatus};

const HISTORY_MAX_GAP_MS: i64 = 30_000;

pub(super) fn render_epoch_chart(
    app: &TuiApp,
    entry: &SessionListEntry,
    detail_opt: Option<&SessionFullDetail>,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) -> u16 {
    if area.width < 10 || area.height < 4 {
        return 0;
    }

    let Some(detail) = detail_opt else {
        buf.set_string(
            area.x + 2,
            area.y + 1,
            "Loading session epoch detail...",
            app.theme.muted(),
        );
        return 0;
    };

    if detail.epochs.is_empty() {
        let msg = if entry.status == SessionStatus::Active {
            "Waiting for first 15s epoch..."
        } else {
            "Epochs were pruned by retention policy; trace unavailable."
        };
        buf.set_string(area.x + 2, area.y + 1, msg, app.theme.muted());
        return 0;
    }

    let left_label_w = 7u16; // e.g. "100bpm "
    let right_label_w = 5u16; // e.g. " 100%"
    let chart_x = area.x + left_label_w;
    let chart_w = area.width.saturating_sub(left_label_w + right_label_w);
    let show_x_axis = area.height >= 5;
    let reserved_rows = if show_x_axis { 3 } else { 2 }; // 1 top marker row, optional 1 X-axis row, 1 bottom legend row
    let chart_h = area.height.saturating_sub(reserved_rows);

    if chart_w < 5 || chart_h < 2 {
        return 0;
    }

    let chart_y = area.y + 1;
    let right_label_x = chart_x + chart_w + 1;
    let t_start = entry.start_ts_ms;
    let t_end = entry.end_ts_ms;
    let clock_ticks = compute_local_clock_ticks(t_start, t_end, chart_w, app.theme.ascii);

    let hr_samples: Vec<TraceSample> = detail
        .epochs
        .iter()
        .map(|ep| {
            let hr = ep.features.hr_bpm_60s;
            TraceSample {
                ts_ms: ep.ts_ms,
                value: is_valid_hr_bpm(hr).then_some(hr),
            }
        })
        .collect();
    let hr_segments = clip_segments(
        &build_segments(&hr_samples, HISTORY_MAX_GAP_MS),
        t_start,
        t_end,
    );

    let p_samples: Vec<TraceSample> = detail
        .epochs
        .iter()
        .map(|ep| TraceSample {
            ts_ms: ep.ts_ms,
            value: ep.p_sleep.is_finite().then(|| ep.p_sleep.clamp(0.0, 1.0)),
        })
        .collect();
    let p_sleep_segments = clip_segments(
        &build_segments(&p_samples, HISTORY_MAX_GAP_MS),
        t_start,
        t_end,
    );

    let (min_hr, max_hr) = compute_hr_y_bounds(
        hr_segments
            .iter()
            .flat_map(|seg| seg.iter().map(|pt| pt.value)),
        &[entry.warmup_mu_hr, entry.sleep_mu_hr],
    );

    // Left HR bpm Y-axis labels
    render_hr_y_labels(
        area.x,
        chart_y,
        chart_h,
        min_hr,
        max_hr,
        app.theme.muted(),
        buf,
    );

    // Right P(Sleep) percentage Y-axis labels (4-column scale)
    buf.set_string(right_label_x, chart_y, "100%", app.theme.muted());
    buf.set_string(
        right_label_x,
        chart_y + chart_h - 1,
        "  0%",
        app.theme.muted(),
    );

    // Vertical time gridlines and horizontal reference lines rendered underneath data curves
    let plot_area = Rect::new(chart_x, chart_y, chart_w, chart_h);
    render_vertical_time_gridlines(
        plot_area,
        buf,
        &clock_ticks,
        app.theme.muted(),
        app.theme.ascii,
    );
    let sleep_ref_style = app.theme.sparkline_sleep_ref().add_modifier(Modifier::DIM);
    render_horizontal_ref_lines(
        plot_area,
        buf,
        min_hr,
        max_hr,
        &[
            (entry.warmup_mu_hr, app.theme.sparkline_qw_ref()),
            (entry.sleep_mu_hr, sleep_ref_style),
        ],
        app.theme.ascii,
    );

    // Overlay P(Sleep) and HR curves (HR has foreground style priority; Braille masks ORed)
    let series = [
        TraceSeriesSpec {
            segments: &p_sleep_segments,
            y_min: 0.0,
            y_max: 1.0,
            style: app.theme.sparkline_sleep_ref(),
            ascii_point_char: '^',
        },
        TraceSeriesSpec {
            segments: &hr_segments,
            y_min: min_hr,
            y_max: max_hr,
            style: app.theme.sparkline_curve(),
            ascii_point_char: '*',
        },
    ];
    render_trace_series(plot_area, buf, t_start, t_end, &series, app.theme.ascii);

    // Topmost layer: [O], [T], [M] vertical event markers aligned with the trace coordinate system
    let onset_char = if app.theme.ascii { 'O' } else { '▼' };
    let markers = [
        (
            entry.onset_confirmed_ts_ms,
            "[O]",
            onset_char,
            app.theme.warning(),
        ),
        (entry.sleeping_ts_ms, "[T]", 'T', app.theme.danger()),
        (
            detail
                .feedback
                .as_ref()
                .and_then(|fb| fb.manual_onset_ts_ms),
            "[M]",
            'M',
            app.theme.success(),
        ),
    ];

    for (ts_opt, tag, top_char, style) in markers {
        if let Some(ts) = ts_opt
            && ts >= t_start
            && ts <= t_end
        {
            let offset = time_to_column(ts, t_start, t_end, chart_w, app.theme.ascii);
            let marker_x = chart_x + offset;
            let tag_x = marker_x
                .saturating_sub(1)
                .min(area.right().saturating_sub(tag.len() as u16));
            buf.set_string(tag_x, area.y, tag, style);
            for row_y in chart_y..chart_y + chart_h {
                if let Some(c) = buf.cell_mut((marker_x, row_y)) {
                    c.set_char(if row_y == chart_y { top_char } else { '|' })
                        .set_style(style);
                }
            }
        }
    }

    // Emit render-aligned per-column HitMap targets across the plot area (ADR-0010)
    for col in 0..chart_w {
        hit_map.push_click(
            Rect::new(chart_x + col, chart_y, 1, chart_h),
            ClickTarget::HistoryChartCol(col),
        );
    }

    // Composite active inspect cursor highlight non-destructively onto plot column
    let inspect_col = app.history_inspect.col.map(|c| c.min(chart_w - 1));
    if let Some(col) = inspect_col {
        let cursor_x = chart_x + col;
        for row_y in chart_y..chart_y + chart_h {
            if let Some(c) = buf.cell_mut((cursor_x, row_y)) {
                let style = c.style().add_modifier(Modifier::REVERSED);
                c.set_style(style);
            }
        }
    }

    // X-axis local wall-clock row directly beneath plot area (when vertical height permits)
    let legend_y = if show_x_axis {
        let axis_y = chart_y + chart_h;
        render_local_clock_x_axis(
            chart_x,
            axis_y,
            chart_w,
            &clock_ticks,
            app.theme.muted(),
            buf,
        );
        axis_y + 1
    } else {
        chart_y + chart_h
    };

    // Legend or active inspect cursor readout line at bottom
    if legend_y < area.bottom() {
        let line = if let Some(col) = inspect_col {
            build_inspect_readout_line(
                app,
                &detail.epochs,
                col,
                t_start,
                t_end,
                chart_w,
                area.width.saturating_sub(2),
            )
        } else {
            build_legend_line(app, entry, area.width, sleep_ref_style)
        };
        buf.set_line(area.x + 1, legend_y, &line, area.width.saturating_sub(2));
    }

    chart_w
}

fn build_legend_line<'a>(
    app: &TuiApp,
    entry: &SessionListEntry,
    area_width: u16,
    sleep_ref_style: ratatui::style::Style,
) -> Line<'a> {
    let ref_char = if app.theme.ascii { '-' } else { '┄' };
    let hr_label = if app.theme.ascii { "* HR" } else { "● HR" };
    let p_label = if app.theme.ascii {
        "^ P(Sleep)"
    } else {
        "▲ P(Sleep)"
    };
    let mut spans = vec![
        Span::styled(hr_label, app.theme.sparkline_curve()),
        Span::styled("  ", app.theme.muted()),
        Span::styled(p_label, app.theme.sparkline_sleep_ref()),
    ];
    if let Some(qw) = entry.warmup_mu_hr {
        spans.push(Span::styled("  ", app.theme.muted()));
        spans.push(Span::styled(
            format!("{ref_char} QW:{qw:.0}"),
            app.theme.sparkline_qw_ref(),
        ));
    }
    if let Some(sl) = entry.sleep_mu_hr
        && area_width >= 52
    {
        spans.push(Span::styled("  ", app.theme.muted()));
        spans.push(Span::styled(
            format!("{ref_char} Sleep:{sl:.0}"),
            sleep_ref_style,
        ));
    }
    spans.push(Span::styled("  ", app.theme.muted()));
    if area_width >= 66 {
        spans.push(Span::styled("[O] Onset ", app.theme.warning()));
        spans.push(Span::styled("[T] Trigger ", app.theme.danger()));
        spans.push(Span::styled("[M] Manual", app.theme.success()));
    } else {
        spans.push(Span::styled("[O] ", app.theme.warning()));
        spans.push(Span::styled("[T] ", app.theme.danger()));
        spans.push(Span::styled("[M]", app.theme.success()));
    }
    Line::from(spans)
}

fn build_inspect_readout_line<'a>(
    app: &TuiApp,
    epochs: &[Epoch],
    col: u16,
    t_start: i64,
    t_end: i64,
    chart_w: u16,
    avail_w: u16,
) -> Line<'a> {
    let col_ts = column_to_time(col, t_start, t_end, chart_w, app.theme.ascii);
    let step_ms = if chart_w > 1 && t_end > t_start {
        (t_end - t_start) / i64::from(chart_w - 1)
    } else {
        0
    };
    let tolerance_ms = (step_ms / 2).max(HISTORY_MAX_GAP_MS / 2);

    let mut has_any_epoch = false;
    let mut best_hr: Option<(i64, f64)> = None;
    let mut best_p: Option<(i64, f64)> = None;

    for ep in epochs {
        if ep.ts_ms < t_start || ep.ts_ms > t_end {
            continue;
        }
        let dist = (ep.ts_ms - col_ts).abs();
        if dist > tolerance_ms {
            continue;
        }
        has_any_epoch = true;
        if is_valid_hr_bpm(ep.features.hr_bpm_60s)
            && best_hr.is_none_or(|(best_d, _)| dist < best_d)
        {
            best_hr = Some((dist, ep.features.hr_bpm_60s));
        }
        if ep.p_sleep.is_finite() && best_p.is_none_or(|(best_d, _)| dist < best_d) {
            best_p = Some((dist, ep.p_sleep.clamp(0.0, 1.0)));
        }
    }

    let hms = format_local_hhmmss(col_ts).unwrap_or_else(|| "00:00:00".to_string());
    let hr_str = best_hr.map_or_else(|| "--".to_string(), |(_, v)| format!("{v:.0}"));
    let p_str = best_p.map_or_else(|| "--".to_string(), |(_, p)| format!("{:.0}", p * 100.0));
    let is_gap = !has_any_epoch;

    let mut spans = vec![Span::styled(format!("@ {hms}"), app.theme.primary())];
    if avail_w < 36 {
        if is_gap {
            spans.push(Span::styled(" -- (gap)", app.theme.warning()));
        } else {
            spans.push(Span::styled(
                format!(" {hr_str}bpm"),
                app.theme.sparkline_curve(),
            ));
            spans.push(Span::styled(
                format!(" {p_str}%"),
                app.theme.sparkline_sleep_ref(),
            ));
        }
        return Line::from(spans);
    }

    let sep = if is_gap && avail_w < 44 { " " } else { "  " };
    spans.push(Span::styled(format!("{sep}HR: "), app.theme.muted()));
    spans.push(Span::styled(
        format!("{hr_str} bpm"),
        if best_hr.is_some() {
            app.theme.sparkline_curve()
        } else {
            app.theme.muted()
        },
    ));
    spans.push(Span::styled(format!("{sep}P(Sleep): "), app.theme.muted()));
    spans.push(Span::styled(
        format!("{p_str}%"),
        if best_p.is_some() {
            app.theme.sparkline_sleep_ref()
        } else {
            app.theme.muted()
        },
    ));
    if is_gap {
        spans.push(Span::styled(format!("{sep}(gap)"), app.theme.warning()));
    }
    Line::from(spans)
}

fn format_local_hhmmss(ts_ms: i64) -> Option<String> {
    Local
        .timestamp_millis_opt(ts_ms)
        .earliest()
        .map(|dt| dt.format("%H:%M:%S").to_string())
}
