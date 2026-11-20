use chrono::{Local, TimeZone};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::super::app::{ModalState, TuiApp};
use super::super::hit_map::{ClickTarget, HitMap, ScrollTarget};
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
                let div_text = format!("── Boundary: {reason_str} ──");
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
                    "[ACTIVE]"
                } else {
                    match entry.end_reason {
                        Some(SessionEndReason::HostSuspend) => "[Suspend]",
                        Some(SessionEndReason::HostPoweroff) => "[Poweroff]",
                        _ if entry.onset_confirmed_ts_ms.is_some() => "[Sleep]",
                        _ => "[Ended]",
                    }
                };

                let line1 = format!("{local_time} {dur_str:>4} {outcome_tag}");
                let line1_spans = vec![
                    Span::styled(
                        if is_selected { "▶ " } else { "  " },
                        if is_selected {
                            app.theme.primary()
                        } else {
                            app.theme.muted()
                        },
                    ),
                    Span::styled(line1, bg_style),
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

    let detail_opt = app.selected_session_detail.as_ref();
    let selected_entry = app.history_sessions.get(app.selected_session_idx);

    let Some(entry) = selected_entry else {
        return;
    };

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

    lines.push(Line::from(vec![Span::styled(
        format!(" Window: {start_str}..{end_str} ({dur_str})  Outcome: {reason_str}"),
        app.theme.muted(),
    )]));

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
        buf.set_string(
            area.x + 2,
            area.y + 1,
            "Epochs were pruned by retention policy; trace unavailable.",
            app.theme.muted(),
        );
        return;
    }

    let epochs = &detail.epochs;
    let label_width = 8u16;
    let chart_x = area.x + label_width;
    let chart_w = area.width.saturating_sub(label_width + 1);
    let chart_h = area.height.saturating_sub(2); // 1 top legend, 1 bottom scale

    if chart_w < 5 || chart_h < 2 {
        return;
    }

    // Determine min/max HR
    let mut min_hr = 45.0;
    let mut max_hr = 95.0;
    for ep in epochs {
        let hr = ep.features.hr_bpm_60s;
        if hr.is_finite() && hr > 20.0 {
            if hr < min_hr {
                min_hr = hr;
            }
            if hr > max_hr {
                max_hr = hr;
            }
        }
    }
    if let Some(qw) = entry.warmup_mu_hr {
        if qw < min_hr {
            min_hr = qw;
        }
        if qw > max_hr {
            max_hr = qw;
        }
    }
    min_hr -= 2.0;
    max_hr += 2.0;
    let hr_range = (max_hr - min_hr).max(1.0);

    // Y-axis labels
    buf.set_string(
        area.x,
        area.y + 1,
        format!("{:>3.0}bpm", max_hr),
        app.theme.muted(),
    );
    buf.set_string(
        area.x,
        area.y + chart_h,
        format!("{:>3.0}bpm", min_hr),
        app.theme.muted(),
    );

    // Reference lines
    let ref_char = if app.theme.ascii { '-' } else { '┄' };
    if let Some(qw) = entry.warmup_mu_hr {
        let norm = ((qw - min_hr) / hr_range).clamp(0.0, 1.0);
        let row_offset = ((1.0 - norm) * (chart_h.saturating_sub(1) as f64)).round() as u16;
        let row_y = area.y + 1 + row_offset;
        if row_y <= area.y + chart_h {
            for col in 0..chart_w {
                if let Some(c) = buf.cell_mut((chart_x + col, row_y)) {
                    c.set_char(ref_char).set_style(app.theme.sparkline_qw_ref());
                }
            }
        }
    }

    // Draw HR and P(Sleep) curves across chart columns
    let total_epochs = epochs.len();
    for col in 0..chart_w {
        let start_ep = (col as usize * total_epochs) / chart_w as usize;
        let end_ep = (((col + 1) as usize * total_epochs) / chart_w as usize)
            .max(start_ep + 1)
            .min(total_epochs);

        let slice = &epochs[start_ep..end_ep];
        if slice.is_empty() {
            continue;
        }

        let avg_hr =
            slice.iter().map(|e| e.features.hr_bpm_60s).sum::<f64>() / (slice.len() as f64);
        let avg_p_sleep = slice.iter().map(|e| e.p_sleep).sum::<f64>() / (slice.len() as f64);

        // HR curve position
        let hr_norm = ((avg_hr - min_hr) / hr_range).clamp(0.0, 1.0);
        let hr_row = ((1.0 - hr_norm) * (chart_h.saturating_sub(1) as f64)).round() as u16;
        let hr_y = area.y + 1 + hr_row;

        let hr_symbol = if app.theme.ascii { '*' } else { '●' };
        if hr_y <= area.y + chart_h
            && let Some(c) = buf.cell_mut((chart_x + col, hr_y))
        {
            c.set_char(hr_symbol).set_style(app.theme.sparkline_curve());
        }

        // P(Sleep) curve position
        let p_row = ((1.0 - avg_p_sleep.clamp(0.0, 1.0)) * (chart_h.saturating_sub(1) as f64))
            .round() as u16;
        let p_y = area.y + 1 + p_row;
        let p_symbol = if app.theme.ascii { '^' } else { '▲' };
        if p_y <= area.y + chart_h
            && p_y != hr_y
            && let Some(c) = buf.cell_mut((chart_x + col, p_y))
        {
            c.set_char(p_symbol)
                .set_style(app.theme.sparkline_sleep_ref());
        }
    }

    // Event markers (vertical lines / labels)
    let session_span = (entry.end_ts_ms - entry.start_ts_ms).max(1);

    // 1. Detected onset marker [O]
    if let Some(onset_ts) = entry.onset_confirmed_ts_ms
        && onset_ts >= entry.start_ts_ms
        && onset_ts <= entry.end_ts_ms
    {
        let offset = ((onset_ts - entry.start_ts_ms) as f64 / session_span as f64
            * (chart_w.saturating_sub(1) as f64))
            .round() as u16;
        let marker_x = chart_x + offset;
        let marker_char = if app.theme.ascii { 'O' } else { '▼' };
        buf.set_string(
            marker_x.saturating_sub(1),
            area.y,
            "[O]",
            app.theme.warning(),
        );
        for row_y in (area.y + 1)..=(area.y + chart_h) {
            if let Some(c) = buf.cell_mut((marker_x, row_y)) {
                c.set_char(if row_y == area.y + 1 {
                    marker_char
                } else {
                    '|'
                })
                .set_style(app.theme.warning());
            }
        }
    }

    // 2. Trigger marker [T]
    if let Some(trigger_ts) = entry.sleeping_ts_ms
        && trigger_ts >= entry.start_ts_ms
        && trigger_ts <= entry.end_ts_ms
    {
        let offset = ((trigger_ts - entry.start_ts_ms) as f64 / session_span as f64
            * (chart_w.saturating_sub(1) as f64))
            .round() as u16;
        let marker_x = chart_x + offset;
        buf.set_string(
            marker_x.saturating_sub(1),
            area.y,
            "[T]",
            app.theme.danger(),
        );
        for row_y in (area.y + 1)..=(area.y + chart_h) {
            if let Some(c) = buf.cell_mut((marker_x, row_y)) {
                c.set_char(if row_y == area.y + 1 { 'T' } else { '|' })
                    .set_style(app.theme.danger());
            }
        }
    }

    // 3. Manual Wearable-app onset marker [M]
    if let Some(fb) = &detail.feedback
        && let Some(manual_ts) = fb.manual_onset_ts_ms
        && manual_ts >= entry.start_ts_ms
        && manual_ts <= entry.end_ts_ms
    {
        let offset = ((manual_ts - entry.start_ts_ms) as f64 / session_span as f64
            * (chart_w.saturating_sub(1) as f64))
            .round() as u16;
        let marker_x = chart_x + offset;
        buf.set_string(
            marker_x.saturating_sub(1),
            area.y,
            "[M]",
            app.theme.success(),
        );
        for row_y in (area.y + 1)..=(area.y + chart_h) {
            if let Some(c) = buf.cell_mut((marker_x, row_y)) {
                c.set_char(if row_y == area.y + 1 { 'M' } else { '|' })
                    .set_style(app.theme.success());
            }
        }
    }

    // Legend line at bottom
    let legend_y = area.y + chart_h + 1;
    if legend_y < area.bottom() {
        let hr_icon = if app.theme.ascii { "* HR" } else { "● HR" };
        let p_icon = if app.theme.ascii {
            "^ P(Sleep)"
        } else {
            "▲ P(Sleep)"
        };
        let legend_text = format!(
            "{}  {}  - QW: {:.0}  [O] Onset  [T] Trigger  [M] Manual",
            hr_icon,
            p_icon,
            entry.warmup_mu_hr.unwrap_or(0.0)
        );
        buf.set_string(
            chart_x,
            legend_y,
            legend_text
                .chars()
                .take(chart_w as usize)
                .collect::<String>(),
            app.theme.muted(),
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
