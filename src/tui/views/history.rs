use chrono::{Local, TimeZone};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::super::app::{ModalState, TuiApp};
use super::super::hit_map::{ClickTarget, HitMap, ScrollTarget};
use super::super::trace::{
    TraceSample, TraceSeriesSpec, build_segments, clip_segments, compute_hr_y_bounds,
    is_valid_hr_bpm, render_horizontal_ref_lines, render_hr_y_labels, render_trace_series,
    time_to_column,
};
use super::history_modals::{
    render_delete_session_modal, render_export_trace_modal, render_feedback_modal,
};
use crate::calibration::{CalibrationBoundaryRecord, FeedbackVerdict};
use crate::logging::format_wearable_label;
use crate::sleep_engine::SessionEndReason;
use crate::storage::{SessionFullDetail, SessionListEntry, SessionStatus};

enum HistoryRow<'a> {
    Session {
        session_idx: usize,
        entry: &'a SessionListEntry,
    },
    Boundary(&'a CalibrationBoundaryRecord),
}

pub(crate) fn render_history(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    if area.width < 10 || area.height < 5 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(34), // Master list column
            Constraint::Min(20),    // Detail + Chart column
        ])
        .split(area);

    let master_area = chunks[0];
    let detail_area = chunks[1];

    render_master_list(app, master_area, buf, hit_map);
    render_detail_pane(app, detail_area, buf);

    match app.modal {
        ModalState::FeedbackModal { .. } => render_feedback_modal(app, area, buf, hit_map),
        ModalState::ExportTraceModal { .. } => render_export_trace_modal(app, area, buf, hit_map),
        ModalState::DeleteSessionModal { .. } => {
            render_delete_session_modal(app, area, buf, hit_map)
        }
        _ => {}
    }
}

fn render_master_list(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    hit_map.push_scroll(area, ScrollTarget::HistorySessions);
    let block = app.theme.card_block(format!(
        " Monitoring Sessions ({}) ",
        app.history_sessions.len()
    ));
    let inner = block.inner(area);
    block.render(area, buf);

    if app.history_sessions.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                " No sessions recorded.",
                app.theme.muted(),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                " Wearables stream will",
                app.theme.muted(),
            )]),
            Line::from(vec![Span::styled(
                " record sessions here.",
                app.theme.muted(),
            )]),
        ]);
        msg.render(inner, buf);
        return;
    }

    // Merge sessions and boundaries into a chronological (newest-first) list
    let mut rows: Vec<HistoryRow<'_>> = Vec::new();
    let mut boundary_idx = 0;
    let mut active_boundaries: Vec<&CalibrationBoundaryRecord> = app
        .history_boundaries
        .iter()
        .filter(|b| b.reverted_at_ms.is_none())
        .collect();
    // Sort boundaries newest-first
    active_boundaries.sort_by_key(|b| std::cmp::Reverse(b.effective_ts_ms));

    for (s_idx, s) in app.history_sessions.iter().enumerate() {
        while boundary_idx < active_boundaries.len()
            && active_boundaries[boundary_idx].effective_ts_ms > s.start_ts_ms
        {
            rows.push(HistoryRow::Boundary(active_boundaries[boundary_idx]));
            boundary_idx += 1;
        }
        rows.push(HistoryRow::Session {
            session_idx: s_idx,
            entry: s,
        });
    }
    while boundary_idx < active_boundaries.len() {
        rows.push(HistoryRow::Boundary(active_boundaries[boundary_idx]));
        boundary_idx += 1;
    }

    let visible_rows = inner.height as usize / 2;
    let selected_session = app.selected_session_idx;

    // Calculate scroll offset so selected session is in view
    let selected_row_idx = rows
        .iter()
        .position(|r| matches!(r, HistoryRow::Session { session_idx, .. } if *session_idx == selected_session))
        .unwrap_or(0);

    let start_row = if selected_row_idx >= visible_rows {
        selected_row_idx.saturating_sub(visible_rows - 1)
    } else {
        0
    };

    let mut current_y = inner.y;
    for row in rows.iter().skip(start_row) {
        if current_y >= inner.bottom() {
            break;
        }

        match row {
            HistoryRow::Boundary(b) => {
                let reason_str = match b.reason {
                    crate::calibration::BoundaryReason::PersistentShift => "Persistent Shift",
                    crate::calibration::BoundaryReason::ManualRebaseline => "Rebaseline",
                };
                let div_text = if app.theme.ascii {
                    format!("-- Boundary: {reason_str} --")
                } else {
                    format!("── Boundary: {reason_str} ──")
                };
                buf.set_string(
                    inner.x,
                    current_y,
                    div_text
                        .chars()
                        .take(inner.width as usize)
                        .collect::<String>(),
                    app.theme.warning(),
                );
                current_y += 1;
            }
            HistoryRow::Session { session_idx, entry } => {
                let is_selected = *session_idx == selected_session;
                let bg_style = if is_selected {
                    app.theme.highlight()
                } else {
                    app.theme.base()
                };

                let local_time = Local
                    .timestamp_millis_opt(entry.start_ts_ms)
                    .single()
                    .map(|dt| dt.format("%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "00-00 00:00".to_string());

                let dur_sec = (entry.end_ts_ms.saturating_sub(entry.start_ts_ms) / 1000).max(0);
                let dur_str = format_duration_compact(dur_sec);

                let outcome_tag = if entry.status == SessionStatus::Active {
                    if app.session_close_deadline_ms.is_some() {
                        "[CLOSING]"
                    } else {
                        "[ACTIVE]"
                    }
                } else {
                    match entry.end_reason {
                        Some(SessionEndReason::HostSuspend) => "[Suspend]",
                        Some(SessionEndReason::HostPoweroff) => "[Poweroff]",
                        _ if entry.onset_confirmed_ts_ms.is_some() => "[Sleep]",
                        _ => "[Ended]",
                    }
                };

                let outcome_style = if outcome_tag == "[CLOSING]" && !is_selected {
                    app.theme.warning()
                } else {
                    bg_style
                };
                let selected_prefix = if is_selected {
                    if app.theme.ascii { "> " } else { "▶ " }
                } else {
                    "  "
                };
                let line1_spans = vec![
                    Span::styled(
                        selected_prefix,
                        if is_selected {
                            app.theme.primary()
                        } else {
                            app.theme.muted()
                        },
                    ),
                    Span::styled(format!("{local_time} {dur_str:>4} "), bg_style),
                    Span::styled(outcome_tag, outcome_style),
                ];
                buf.set_line(inner.x, current_y, &Line::from(line1_spans), inner.width);
                hit_map.push_click(
                    Rect::new(inner.x, current_y, inner.width, 1),
                    ClickTarget::SelectSession(*session_idx),
                );
                current_y += 1;

                if current_y >= inner.bottom() {
                    break;
                }

                // Line 2: Wearable label + badges
                let wearable_name = entry
                    .wearable_alias
                    .as_deref()
                    .unwrap_or(&entry.wearable_mac_masked);
                let mut badges = String::new();
                if entry.auto_anomalous {
                    badges.push_str(" [ANOMALOUS]");
                }
                if let Some(fb) = &entry.feedback {
                    match fb.verdict {
                        Some(FeedbackVerdict::FalsePositive) => badges.push_str(" [FP]"),
                        Some(FeedbackVerdict::MissedSleep) => badges.push_str(" [Missed]"),
                        Some(FeedbackVerdict::AnomalousExclude) if !entry.auto_anomalous => {
                            badges.push_str(" [ANOMALOUS]");
                        }
                        _ => {}
                    }
                    if fb.manual_onset_ts_ms.is_some() || fb.manual_avg_sleep_hr.is_some() {
                        badges.push_str(" [Manual]");
                    }
                }

                let line2 = format!("   {wearable_name}{badges}");
                buf.set_string(
                    inner.x,
                    current_y,
                    line2.chars().take(inner.width as usize).collect::<String>(),
                    if is_selected {
                        app.theme.primary()
                    } else {
                        app.theme.muted()
                    },
                );
                hit_map.push_click(
                    Rect::new(inner.x, current_y, inner.width, 1),
                    ClickTarget::SelectSession(*session_idx),
                );
                current_y += 1;
            }
        }
    }
}

fn render_detail_pane(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app.theme.card_block(" Epoch Detail & Chart ");
    let inner = block.inner(area);
    block.render(area, buf);

    if app.history_sessions.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                " Select a session to view 15s Epoch metrics and HR chart.",
                app.theme.muted(),
            )]),
        ]);
        msg.render(inner, buf);
        return;
    }

    let selected_entry = app.history_sessions.get(app.selected_session_idx);

    let Some(entry) = selected_entry else {
        return;
    };
    let detail_opt = app
        .selected_session_detail
        .as_ref()
        .filter(|d| d.session.id == entry.id);

    let header_lines = build_detail_header(entry, detail_opt, app);
    let header_height = (header_lines.len() as u16).min(inner.height);

    for (idx, line) in header_lines.iter().enumerate() {
        if (idx as u16) < header_height {
            buf.set_line(inner.x, inner.y + idx as u16, line, inner.width);
        }
    }

    let chart_y = inner.y + header_height;
    if chart_y + 4 < inner.bottom() {
        let chart_area = Rect {
            x: inner.x,
            y: chart_y,
            width: inner.width,
            height: inner.bottom().saturating_sub(chart_y),
        };
        render_epoch_chart(app, entry, detail_opt, chart_area, buf);
    }
}

fn build_detail_header<'a>(
    entry: &'a SessionListEntry,
    detail_opt: Option<&'a SessionFullDetail>,
    app: &TuiApp,
) -> Vec<Line<'a>> {
    let mut lines = Vec::new();

    let wearable_label = format_wearable_label(
        entry.wearable_alias.as_deref(),
        &entry.wearable_mac_masked,
        false,
    );

    let start_str = Local
        .timestamp_millis_opt(entry.start_ts_ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "0000-00-00 00:00".to_string());
    let end_str = Local
        .timestamp_millis_opt(entry.end_ts_ms)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_else(|| "00:00".to_string());

    let dur_sec = (entry.end_ts_ms.saturating_sub(entry.start_ts_ms) / 1000).max(0);
    let dur_str = format_duration_compact(dur_sec);

    let reason_str = entry
        .end_reason
        .map(|r| r.as_str())
        .unwrap_or(entry.status.as_str());

    lines.push(Line::from(vec![
        Span::styled(format!(" Session: {}", entry.id), app.theme.primary()),
        Span::styled(format!("  Wearable: {wearable_label}"), app.theme.base()),
    ]));

    let is_closing =
        entry.status == SessionStatus::Active && app.session_close_deadline_ms.is_some();
    if is_closing && let Some(rem_sec) = app.session_close_remaining_sec() {
        let formatted = crate::tui::format_close_countdown(rem_sec);
        lines.push(Line::from(vec![Span::styled(
            format!(" Window: {start_str}..{end_str} ({dur_str})"),
            app.theme.muted(),
        )]));
        lines.push(Line::from(vec![
            Span::styled(" Outcome: ", app.theme.muted()),
            Span::styled(format!("Closing in {formatted}"), app.theme.warning()),
        ]));
    } else {
        lines.push(Line::from(vec![Span::styled(
            format!(" Window: {start_str}..{end_str} ({dur_str})  Outcome: {reason_str}"),
            app.theme.muted(),
        )]));
    }

    let qw_str = entry
        .warmup_mu_hr
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "--".to_string());
    let sleep_str = entry
        .sleep_mu_hr
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "--".to_string());

    let mut metrics_spans = vec![
        Span::styled(format!(" QW: {qw_str} bpm"), app.theme.success()),
        Span::styled(
            format!("  Sleep: {sleep_str} bpm"),
            app.theme.sparkline_sleep_ref(),
        ),
        Span::styled(
            format!("  Epochs: {}", entry.epochs_count),
            app.theme.base(),
        ),
    ];

    if entry.raw_samples_count > 0 {
        metrics_spans.push(Span::styled(
            format!("  Raw: {}", entry.raw_samples_count),
            app.theme.muted(),
        ));
    } else {
        metrics_spans.push(Span::styled("  Raw: pruned", app.theme.muted()));
    }
    lines.push(Line::from(metrics_spans));

    // Feedback summary line if present
    if let Some(detail) = detail_opt
        && let Some(fb) = &detail.feedback
    {
        let mut fb_spans = vec![Span::styled(" Feedback: ", app.theme.key_hint())];
        if let Some(v) = fb.verdict {
            fb_spans.push(Span::styled(format!("verdict={v} "), app.theme.warning()));
        }
        if let Some(onset_ts) = fb.manual_onset_ts_ms {
            let onset_hhmm = Local
                .timestamp_millis_opt(onset_ts)
                .single()
                .map(|dt| dt.format("%H:%M").to_string())
                .unwrap_or_default();
            fb_spans.push(Span::styled(
                format!("manual_onset={onset_hhmm} "),
                app.theme.base(),
            ));
        }
        if let Some(avg_hr) = fb.manual_avg_sleep_hr {
            fb_spans.push(Span::styled(
                format!("manual_sleep_hr={avg_hr:.1} "),
                app.theme.base(),
            ));
        }
        lines.push(Line::from(fb_spans));
    }

    lines
}

const HISTORY_MAX_GAP_MS: i64 = 30_000;

fn render_epoch_chart(
    app: &TuiApp,
    entry: &SessionListEntry,
    detail_opt: Option<&SessionFullDetail>,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 10 || area.height < 4 {
        return;
    }

    let Some(detail) = detail_opt else {
        buf.set_string(
            area.x + 2,
            area.y + 1,
            "Loading session epoch detail...",
            app.theme.muted(),
        );
        return;
    };

    if detail.epochs.is_empty() {
        let msg = if entry.status == SessionStatus::Active {
            "Waiting for first 15s epoch..."
        } else {
            "Epochs were pruned by retention policy; trace unavailable."
        };
        buf.set_string(area.x + 2, area.y + 1, msg, app.theme.muted());
        return;
    }

    let left_label_w = 7u16; // e.g. "100bpm "
    let right_label_w = 5u16; // e.g. " 100%"
    let chart_x = area.x + left_label_w;
    let chart_w = area.width.saturating_sub(left_label_w + right_label_w);
    let chart_h = area.height.saturating_sub(2); // 1 top marker label row, 1 bottom legend row

    if chart_w < 5 || chart_h < 2 {
        return;
    }

    let chart_y = area.y + 1;
    let right_label_x = chart_x + chart_w + 1;
    let t_start = entry.start_ts_ms;
    let t_end = entry.end_ts_ms;

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

    // Horizontal reference lines rendered underneath curves (distinct DIM|UNDERLINED for Sleep ref in NO_COLOR)
    let plot_area = Rect::new(chart_x, chart_y, chart_w, chart_h);
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

    // Legend line at bottom
    let ref_char = if app.theme.ascii { '-' } else { '┄' };
    let legend_y = chart_y + chart_h;
    if legend_y < area.bottom() {
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
            && area.width >= 52
        {
            spans.push(Span::styled("  ", app.theme.muted()));
            spans.push(Span::styled(
                format!("{ref_char} Sleep:{sl:.0}"),
                sleep_ref_style,
            ));
        }
        spans.push(Span::styled("  ", app.theme.muted()));
        if area.width >= 66 {
            spans.push(Span::styled("[O] Onset ", app.theme.warning()));
            spans.push(Span::styled("[T] Trigger ", app.theme.danger()));
            spans.push(Span::styled("[M] Manual", app.theme.success()));
        } else {
            spans.push(Span::styled("[O] ", app.theme.warning()));
            spans.push(Span::styled("[T] ", app.theme.danger()));
            spans.push(Span::styled("[M]", app.theme.success()));
        }
        let legend_line = Line::from(spans);
        buf.set_line(
            area.x + 1,
            legend_y,
            &legend_line,
            area.width.saturating_sub(2),
        );
    }
}

fn format_duration_compact(sec: i64) -> String {
    if sec < 60 {
        format!("{sec}s")
    } else if sec < 3600 {
        format!("{}m", sec / 60)
    } else {
        let h = sec / 3600;
        let m = (sec % 3600) / 60;
        format!("{h}h {m}m")
    }
}
