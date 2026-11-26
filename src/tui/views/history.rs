use chrono::{Local, TimeZone};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::super::app::{ModalState, TuiApp};
use super::super::hit_map::{ClickTarget, HitMap, ScrollTarget};
use super::history_chart::render_epoch_chart;
use super::history_modals::{
    render_delete_session_modal, render_export_trace_modal, render_feedback_modal,
};
use crate::calibration::{CalibrationBoundaryRecord, FeedbackVerdict};
use crate::logging::format_wearable_label;
use crate::sleep_engine::SessionEndReason;
use crate::storage::{SessionFullDetail, SessionListEntry, SessionStatus};

/// Layout metadata emitted by `render_history` so `TuiApp` can clamp chart inspection bounds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryRenderResult {
    pub(crate) chart_plot_width: u16,
}

enum HistoryRow<'a> {
    Session {
        session_idx: usize,
        entry: &'a SessionListEntry,
    },
    Boundary(&'a CalibrationBoundaryRecord),
}

pub(crate) fn render_history(
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) -> HistoryRenderResult {
    if area.width < 10 || area.height < 5 {
        return HistoryRenderResult::default();
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
    let result = render_detail_pane(app, detail_area, buf, hit_map);

    match app.modal {
        ModalState::FeedbackModal { .. } => render_feedback_modal(app, area, buf, hit_map),
        ModalState::ExportTraceModal { .. } => render_export_trace_modal(app, area, buf, hit_map),
        ModalState::DeleteSessionModal { .. } => {
            render_delete_session_modal(app, area, buf, hit_map)
        }
        _ => {}
    }

    result
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
                    .earliest()
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

fn render_detail_pane(
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) -> HistoryRenderResult {
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
        return HistoryRenderResult::default();
    }

    let selected_entry = app.history_sessions.get(app.selected_session_idx);

    let Some(entry) = selected_entry else {
        return HistoryRenderResult::default();
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
    if chart_y + 4 <= inner.bottom() {
        let chart_area = Rect {
            x: inner.x,
            y: chart_y,
            width: inner.width,
            height: inner.bottom().saturating_sub(chart_y),
        };
        let chart_plot_width = render_epoch_chart(app, entry, detail_opt, chart_area, buf, hit_map);
        return HistoryRenderResult { chart_plot_width };
    }
    HistoryRenderResult::default()
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
        .earliest()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "0000-00-00 00:00".to_string());
    let end_str = format_local_hhmm(entry.end_ts_ms).unwrap_or_else(|| "00:00".to_string());

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

    // Events summary line when at least one marker timestamp is present
    let manual_onset_ts_ms = detail_opt
        .and_then(|d| d.feedback.as_ref())
        .or(entry.feedback.as_ref())
        .and_then(|fb| fb.manual_onset_ts_ms);
    let event_items = [
        (entry.onset_confirmed_ts_ms, "[O]", app.theme.warning()),
        (entry.sleeping_ts_ms, "[T]", app.theme.danger()),
        (manual_onset_ts_ms, "[M]", app.theme.success()),
    ];
    let mut event_spans: Vec<Span<'a>> = Vec::new();
    for (ts_opt, tag, tag_style) in event_items {
        if let Some(ts) = ts_opt
            && let Some(hhmm) = format_local_hhmm(ts)
        {
            if event_spans.is_empty() {
                event_spans.push(Span::styled(" Events: ", app.theme.muted()));
            } else {
                event_spans.push(Span::styled("  ", app.theme.muted()));
            }
            event_spans.push(Span::styled(tag, tag_style));
            event_spans.push(Span::styled(format!(" {hhmm}"), app.theme.base()));
        }
    }
    if !event_spans.is_empty() {
        lines.push(Line::from(event_spans));
    }

    // Feedback summary line if present
    if let Some(detail) = detail_opt
        && let Some(fb) = &detail.feedback
    {
        let mut fb_spans = vec![Span::styled(" Feedback: ", app.theme.key_hint())];
        if let Some(v) = fb.verdict {
            fb_spans.push(Span::styled(format!("verdict={v} "), app.theme.warning()));
        }
        if let Some(onset_ts) = fb.manual_onset_ts_ms {
            let onset_hhmm = format_local_hhmm(onset_ts).unwrap_or_default();
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

fn format_local_hhmm(ts_ms: i64) -> Option<String> {
    Local
        .timestamp_millis_opt(ts_ms)
        .earliest()
        .map(|dt| dt.format("%H:%M").to_string())
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
