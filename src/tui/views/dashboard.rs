use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Gauge, Paragraph, Widget};

use super::super::app::{Tab, TuiApp};
use super::super::hit_map::{ClickTarget, HitMap, ScrollTarget};
use super::super::sparkline::HrSparklineWidget;
use super::{build_clickable_row_line, centered_rect};
use crate::ipc::protocol::alert_ids;

pub(crate) fn render_dashboard(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    if area.height < 5 || area.width < 10 {
        return;
    }

    let mut current_area = area;

    // 1. Render Alerts Strip if any active alerts exist
    if let Some(alert) = app.active_alert() {
        let alert_h = 3u16;
        if current_area.height > alert_h + 8 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(alert_h), Constraint::Min(0)])
                .split(current_area);

            render_alert_strip(app, alert, chunks[0], buf, hit_map);
            current_area = chunks[1];
        }
    }

    // 2. Decide layout based on terminal size and diagnostics toggle
    let show_diag = app.is_diagnostics_open;
    let diag_height = if show_diag { 4u16 } else { 0u16 };
    let top_card_height = 5u16;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_card_height),
            Constraint::Min(6),
            Constraint::Length(diag_height),
        ])
        .split(current_area);

    let top_cards_area = chunks[0];
    let sparkline_area = chunks[1];
    let diag_area = chunks[2];

    // 3. Render Top Cards (3-column 80x24 baseline; collapses secondary card below 80 cols)
    if top_cards_area.width >= 80 {
        let card_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(23),
                Constraint::Length(23),
                Constraint::Min(34),
            ])
            .split(top_cards_area);

        render_live_bpm_card(app, card_chunks[0], buf);
        render_sleep_state_card(app, card_chunks[1], buf);
        render_system_card(app, card_chunks[2], buf, hit_map);
    } else if top_cards_area.width >= 50 {
        let card_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(top_cards_area);

        render_live_bpm_card(app, card_chunks[0], buf);
        render_sleep_state_card(app, card_chunks[1], buf);
    } else {
        render_live_bpm_card(app, top_cards_area, buf);
    }

    // 4. Render Sparkline or Zero-Wearables CTA
    if app.enrolled_wearables.is_empty() && app.daemon_connected {
        render_zero_wearables_cta(app, sparkline_area, buf, hit_map);
    } else {
        render_sparkline_card(app, sparkline_area, buf);
    }

    // 5. Render Diagnostics Pane if toggled
    if show_diag && diag_height > 0 {
        render_diagnostics_pane(app, diag_area, buf, hit_map);
    }
}

fn render_alert_strip(
    app: &TuiApp,
    alert: &crate::ipc::protocol::DaemonAlert,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) {
    let is_shift = alert.id == alert_ids::PERSISTENT_SHIFT;
    let style = app.theme.alert_strip(is_shift);
    let border_style = app.theme.alert_border(alert.severity);

    let title = format!(" ALERT [{}] ", alert.kind);
    let block = app.theme.card_block(title).border_style(border_style);
    let inner = block.inner(area);

    let mut action_spans: Vec<(Span<'_>, Option<ClickTarget>)> = Vec::new();
    let mut hints_len = 0usize;

    if is_shift {
        let accept = Span::styled("[a] Accept", app.theme.key_hint());
        let ignore = Span::styled("[i] Ignore", app.theme.key_hint());
        hints_len += 2 + accept.width() + 2 + ignore.width();
        action_spans.push((Span::raw("  "), None));
        action_spans.push((accept, Some(ClickTarget::AcceptShift)));
        action_spans.push((Span::raw("  "), None));
        action_spans.push((ignore, Some(ClickTarget::IgnoreShift)));
    }
    if alert.dismissible {
        let dismiss = Span::styled("[x] Dismiss", app.theme.key_hint());
        hints_len += 2 + dismiss.width();
        action_spans.push((Span::raw("  "), None));
        action_spans.push((dismiss, Some(ClickTarget::DismissAlert)));
    }

    let inner_width = usize::from(inner.width);
    let msg_budget = inner_width.saturating_sub(hints_len + 1);
    let clipped_msg = truncate_label(&alert.message, msg_budget);

    let mut items = vec![(Span::styled(clipped_msg, style), None)];
    items.extend(action_spans);
    let line = build_clickable_row_line(inner, inner.y, items, hit_map);

    Paragraph::new(line).block(block).render(area, buf);
}

fn render_live_bpm_card(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app.theme.card_block(" Live Heart Rate ");
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let bpm_str = match app.latest_bpm {
        Some(bpm) => format!("{:.0}", bpm),
        None => "--".to_string(),
    };

    let contact_str = if app.contact_ok {
        "Contact: OK"
    } else {
        "Contact: Lost"
    };
    let contact_style = if app.contact_ok {
        app.theme.success()
    } else {
        app.theme.danger()
    };

    let slope_str = if let Some(ref epoch) = app.latest_epoch {
        format!("Slope: {:+.2} bpm/m", epoch.features.hr_slope_4m)
    } else {
        "Slope: --".to_string()
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(format!(" {bpm_str} "), app.theme.live_bpm()),
            Span::styled("BPM", app.theme.muted()),
        ]),
        Line::from(vec![Span::styled(format!(" {contact_str}"), contact_style)]),
        Line::from(vec![Span::styled(
            format!(" {slope_str}"),
            app.theme.muted(),
        )]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn render_sleep_state_card(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app.theme.card_block(" Sleep State ");
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let state_str = format!("{}", app.monitoring_state);
    let p_sleep = app.latest_p_sleep.unwrap_or(0.0).clamp(0.0, 1.0);
    let p_pct = (p_sleep * 100.0).round() as u16;

    let lines = vec![
        Line::from(vec![
            Span::styled(" State: ", app.theme.muted()),
            Span::styled(state_str, app.theme.primary()),
        ]),
        Line::from(vec![Span::styled(
            format!(" P(SleepNREM): {:>3}%", p_pct),
            app.theme.base(),
        )]),
    ];

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    Paragraph::new(lines).render(chunks[0], buf);

    if app.theme.ascii {
        let bar_w = usize::from(chunks[1].width.saturating_sub(2));
        let filled = ((usize::from(p_pct) * bar_w) + 50) / 100;
        let empty = bar_w.saturating_sub(filled);
        let ascii_bar = format!("[{}{}]", "#".repeat(filled), "-".repeat(empty));
        Paragraph::new(Span::styled(ascii_bar, app.theme.primary())).render(chunks[1], buf);
    } else {
        let gauge = Gauge::default()
            .use_unicode(true)
            .gauge_style(app.theme.sleep_gauge())
            .percent(p_pct)
            .label("");
        gauge.render(chunks[1], buf);
    }
}

fn render_system_card(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    let block = app.theme.card_block(" Wearable & System ");
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    hit_map.push_click(
        Rect::new(inner.x, inner.y, inner.width, 1),
        ClickTarget::SelectTab(Tab::Wearables),
    );
    if inner.height >= 3 && app.daemon_connected {
        hit_map.push_click(
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
            ClickTarget::OpenPausePopover,
        );
    }

    let active_w = app.active_wearable();
    let w_raw_name = active_w
        .as_ref()
        .and_then(|w| {
            w.wearable
                .alias
                .as_deref()
                .or(w.wearable.advertised_name.as_deref())
        })
        .unwrap_or("None");

    let battery_str = active_w
        .as_ref()
        .and_then(|w| w.battery)
        .map(|b| format!("{b}%"))
        .unwrap_or_else(|| "--".to_string());

    let bat_suffix = format!(" ({battery_str})");
    let label_prefix = " Wearable: ";
    let max_name_len =
        usize::from(inner.width).saturating_sub(label_prefix.len() + bat_suffix.len());
    let w_name = truncate_label(w_raw_name, max_name_len);

    let (stream_status, stream_style) = match active_w {
        Some(w) => (w.status.display_label(), app.theme.stream_status(w.status)),
        None => ("No Wearable", app.theme.muted()),
    };

    let maturity_str = app.calibration_maturity.display_label();

    let arm_str = if app.daemon_status.as_ref().is_some_and(|s| s.paused) {
        "Paused"
    } else if app.armed {
        "Armed"
    } else {
        "Disarmed"
    };
    let dry_str = if app.dry_run { " [DRY]" } else { "" };
    let active_hours_str = match active_hours_config(app) {
        Some(ah) if ah.active_hours_enabled => {
            format!("{}-{}", ah.start, ah.end)
        }
        _ => "Active: 24/7".to_string(),
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(label_prefix, app.theme.muted()),
            Span::styled(w_name, app.theme.base()),
            Span::styled(bat_suffix, app.theme.muted()),
        ]),
        Line::from(vec![
            Span::styled(format!(" {stream_status}"), stream_style),
            Span::styled(" | ", app.theme.muted()),
            Span::styled(maturity_str, app.theme.accent()),
        ]),
        Line::from(vec![
            Span::styled(format!(" {arm_str}{dry_str}"), app.theme.primary()),
            Span::styled(format!(" | {active_hours_str}"), app.theme.muted()),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn active_hours_config(app: &TuiApp) -> Option<&crate::config::ScheduleConfig> {
    app.active_config
        .as_ref()
        .map(|c| &c.schedule)
        .or_else(|| app.daemon_status.as_ref().map(|s| &s.schedule))
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    if max_chars <= 1 {
        return s.chars().take(max_chars).collect();
    }
    let mut clipped: String = s.chars().take(max_chars - 1).collect();
    clipped.push('~');
    clipped
}

fn render_sparkline_card(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app.theme.card_block(" 30-Minute Heart Rate Trace ");
    let inner = block.inner(area);
    block.render(area, buf);

    let sparkline = HrSparklineWidget::new(&app.sparkline_data, app.theme);
    sparkline.render(inner, buf);
}

fn render_zero_wearables_cta(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    let block = app.theme.card_block(" Getting Started ");
    let inner = block.inner(area);
    block.render(area, buf);

    let cta_items = vec![
        (Span::styled("  Press ", app.theme.base()), None),
        (
            Span::styled("[e]", app.theme.key_hint()),
            Some(ClickTarget::DashboardEnroll),
        ),
        (
            Span::styled(" on the Dashboard or switch to the ", app.theme.base()),
            None,
        ),
        (
            Span::styled("Wearables tab (3)", app.theme.primary()),
            Some(ClickTarget::SelectTab(Tab::Wearables)),
        ),
        (
            Span::styled(" to enroll your wearable.", app.theme.base()),
            None,
        ),
    ];
    let cta_line = if inner.height > 3 && inner.width > 0 {
        build_clickable_row_line(inner, inner.y.saturating_add(3), cta_items, hit_map)
    } else {
        Line::from(
            cta_items
                .into_iter()
                .map(|(span, _)| span)
                .collect::<Vec<_>>(),
        )
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "  No BLE Heart Rate wearables enrolled yet.",
            app.theme.warning(),
        )]),
        Line::from(""),
        cta_line,
    ];

    if !app.systemd_service_installed {
        lines.push(Line::from(""));
        let hint = if app.theme.ascii {
            "  Hint: systemd --user unit not found - run 'bpmute service install'."
        } else {
            "  Hint: systemd --user unit not found — run 'bpmute service install'."
        };
        lines.push(Line::from(vec![Span::styled(hint, app.theme.muted())]));
    }

    Paragraph::new(lines).render(inner, buf);
}

fn render_diagnostics_pane(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    hit_map.push_click(
        Rect::new(area.x, area.y, area.width, 1),
        ClickTarget::ToggleDiagnostics,
    );
    let block = app.theme.card_block(" Engine Diagnostics [d] ");
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let (slope_str, var_str, sqi_str, rmssd_str, ratio_str) =
        if let Some(ref epoch) = app.latest_epoch {
            (
                format!("{:+.2} bpm/m", epoch.features.hr_slope_4m),
                format!("{:.2}", epoch.features.r_var_120s),
                format!("{:.2}", (1.0 - epoch.rr_artifact_frac).clamp(0.0, 1.0)),
                epoch
                    .features
                    .ln_rmssd_60s
                    .map(|r| format!("{:.2}", r))
                    .unwrap_or_else(|| "--".to_string()),
                epoch
                    .features
                    .rmssd_sdnn_120s
                    .map(|r| format!("{:.2}", r))
                    .unwrap_or_else(|| "--".to_string()),
            )
        } else {
            (
                "--".into(),
                "--".into(),
                "--".into(),
                "--".into(),
                "--".into(),
            )
        };

    let idle_backend = app.idle_backend.as_deref().unwrap_or("None / Probing");
    let mpris_players = app
        .playing_players_count
        .map_or_else(|| "--".to_string(), |n| n.to_string());

    let hmm_line = format!(
        "Slope:{slope_str} | Var:{var_str} | ln(RMSSD):{rmssd_str} | RMSSD/SDNN:{ratio_str} | SQI:{sqi_str}"
    );
    let lines = vec![
        Line::from(vec![Span::styled(hmm_line, app.theme.base())]),
        Line::from(vec![
            Span::styled("Desktop: ", app.theme.muted()),
            Span::styled(
                format!("Idle Probe: {idle_backend} | Playing MPRIS2 Players: {mpris_players}"),
                app.theme.base(),
            ),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

pub(crate) fn render_pause_popover(
    app: &TuiApp,
    selected_idx: usize,
    full_area: Rect,
    content_area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) {
    let popover_area = centered_rect(content_area, 48, 9);
    // Register backdrop close + wheel navigation before popover body absorber and option rows
    hit_map.push_modal_click(full_area, ClickTarget::CloseModal);
    hit_map.push_modal_scroll(full_area, ScrollTarget::PausePopover);
    hit_map.push_modal_click(popover_area, ClickTarget::Consume);

    Clear.render(popover_area, buf);

    let block = app.theme.card_block(" Pause / Resume Monitoring ");
    let inner = block.inner(popover_area);
    block.render(popover_area, buf);

    let until_end = active_hours_config(app)
        .map(|ah| ah.end)
        .unwrap_or_else(|| crate::config::ScheduleConfig::default().end);
    let until_label = format!("5. Pause until {until_end} (Active Hours end)");
    let options = [
        "1. Resume monitoring",
        "2. Pause indefinitely",
        "3. Pause 1 hour",
        "4. Pause 2 hours",
        until_label.as_str(),
    ];

    let mut lines = Vec::new();
    for (idx, opt) in options.iter().enumerate() {
        if idx < usize::from(inner.height) && inner.width > 0 {
            hit_map.push_modal_click(
                Rect::new(inner.x, inner.y.saturating_add(idx as u16), inner.width, 1),
                ClickTarget::PauseOption(idx),
            );
        }
        let is_selected = idx == selected_idx;
        let style = if is_selected {
            app.theme.highlight()
        } else {
            app.theme.base()
        };
        let prefix = if is_selected { " > " } else { "   " };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(*opt, style),
        ]));
    }

    Paragraph::new(lines).render(inner, buf);
}
