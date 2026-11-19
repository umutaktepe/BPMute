use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Widget};

use super::super::app::{ModalState, TuiApp};
use super::super::sparkline::HrSparklineWidget;
use crate::ipc::protocol::AlertSeverity;

pub fn render_dashboard(app: &TuiApp, area: Rect, buf: &mut Buffer) {
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

            render_alert_strip(app, alert, chunks[0], buf);
            current_area = chunks[1];
        }
    }

    // 2. Decide layout based on terminal size and diagnostics toggle
    let show_diag = app.is_diagnostics_open;
    let diag_height = if show_diag { 5u16 } else { 0u16 };

    let top_card_height = if current_area.height >= 18 {
        7u16
    } else {
        5u16
    };

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

    // 3. Render Top Cards (Split horizontally if >= 75 cols, stack or compress if < 75)
    if top_cards_area.width >= 75 {
        let card_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
            ])
            .split(top_cards_area);

        render_live_bpm_card(app, card_chunks[0], buf);
        render_sleep_state_card(app, card_chunks[1], buf);
        render_system_card(app, card_chunks[2], buf);
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
        render_zero_wearables_cta(app, sparkline_area, buf);
    } else {
        render_sparkline_card(app, sparkline_area, buf);
    }

    // 5. Render Diagnostics Pane if toggled
    if show_diag && diag_height > 0 {
        render_diagnostics_pane(app, diag_area, buf);
    }

    // 6. Render Pause Popover if active
    if let ModalState::PausePopover { selected_idx } = app.modal {
        render_pause_popover(app, selected_idx, area, buf);
    }
}

fn render_alert_strip(
    app: &TuiApp,
    alert: &crate::ipc::protocol::DaemonAlert,
    area: Rect,
    buf: &mut Buffer,
) {
    let is_shift = alert.id == "persistent_shift";
    let style = app.theme.alert_strip(is_shift);
    let border_style = if app.theme.no_color {
        Style::default()
    } else {
        match alert.severity {
            AlertSeverity::Error => Style::default().fg(Color::Red),
            AlertSeverity::Warn => Style::default().fg(Color::Yellow),
            AlertSeverity::Info => Style::default().fg(Color::Cyan),
        }
    };

    let title = format!(" ALERT [{}] ", alert.kind);
    let mut spans = vec![Span::styled(&alert.message, style)];

    if is_shift {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("[a] Accept", app.theme.key_hint()));
        spans.push(Span::raw("  "));
        spans.push(Span::styled("[i] Ignore", app.theme.key_hint()));
    }
    if alert.dismissible {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("[x] Dismiss", app.theme.key_hint()));
    }

    let p = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(border_style),
    );
    p.render(area, buf);
}

fn render_live_bpm_card(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" Live Heart Rate ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let bpm_str = match app.latest_bpm {
        Some(bpm) => format!("{:.0}", bpm),
        None => "--".to_string(),
    };

    let bpm_style = if app.theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
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
        format!("Slope: {:+.1} bpm/min", epoch.features.hr_slope_4m * 60.0)
    } else {
        "Slope: --".to_string()
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(format!("  {} ", bpm_str), bpm_style),
            Span::styled("BPM", app.theme.muted()),
        ]),
        Line::from(vec![Span::styled(
            format!("  {}  |  {}", contact_str, slope_str),
            contact_style,
        )]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn render_sleep_state_card(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" Sleep State ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let state_str = format!("{:?}", app.monitoring_state);
    let state_style = app.theme.primary();

    let p_sleep = app.latest_p_sleep.unwrap_or(0.0).clamp(0.0, 1.0);
    let p_pct = (p_sleep * 100.0).round() as u16;

    let lines = vec![
        Line::from(vec![
            Span::styled("  State: ", app.theme.muted()),
            Span::styled(state_str, state_style),
        ]),
        Line::from(vec![Span::styled(
            format!("  P(Sleep): {:>3}% ", p_pct),
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

    let gauge_style = if app.theme.no_color {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(Color::Cyan)
    };

    let gauge = Gauge::default()
        .gauge_style(gauge_style)
        .percent(p_pct)
        .label("");
    gauge.render(chunks[1], buf);
}

fn render_system_card(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" Wearable & System ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let active_w = app.active_wearable();
    let w_name = active_w
        .as_ref()
        .and_then(|w| {
            w.wearable
                .alias
                .as_deref()
                .or(w.wearable.advertised_name.as_deref())
        })
        .unwrap_or("None");

    let stream_status = active_w
        .as_ref()
        .map(|w| format!("{:?}", w.status))
        .unwrap_or_else(|| "No Wearable".to_string());

    let stream_style = match active_w.as_ref().map(|w| w.status) {
        Some(crate::sleep_engine::StreamStatus::Streaming) => app.theme.success(),
        Some(crate::sleep_engine::StreamStatus::SignalLost) => app.theme.danger(),
        _ => app.theme.muted(),
    };

    let battery_str = active_w
        .as_ref()
        .and_then(|w| w.battery)
        .map(|b| format!("{b}%"))
        .unwrap_or_else(|| "--".to_string());

    let maturity_str = format!("{:?}", app.calibration_maturity);

    let arm_str = if app.armed { "Armed" } else { "Disarmed" };
    let dry_str = if app.dry_run { " [DryRun]" } else { "" };

    let lines = vec![
        Line::from(vec![
            Span::styled("  Device: ", app.theme.muted()),
            Span::styled(w_name, app.theme.base()),
            Span::styled(format!(" [{stream_status}] "), stream_style),
            Span::styled(format!("Bat: {battery_str}"), app.theme.muted()),
        ]),
        Line::from(vec![
            Span::styled("  Maturity: ", app.theme.muted()),
            Span::styled(maturity_str, app.theme.accent()),
            Span::styled("  Status: ", app.theme.muted()),
            Span::styled(format!("{arm_str}{dry_str}"), app.theme.primary()),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn render_sparkline_card(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" 30-Minute Heart Rate Sparkline ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(area);
    block.render(area, buf);

    let sparkline = HrSparklineWidget::new(&app.sparkline_data, app.theme);
    sparkline.render(inner, buf);
}

fn render_zero_wearables_cta(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" Getting Started ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(area);
    block.render(area, buf);

    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "  No BLE Heart Rate wearables enrolled yet.",
            app.theme.warning(),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Press ", app.theme.base()),
            Span::styled("[e]", app.theme.key_hint()),
            Span::styled(" on the Dashboard or switch to the ", app.theme.base()),
            Span::styled("Wearables tab (3)", app.theme.primary()),
            Span::styled(" to enroll your wearable.", app.theme.base()),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  hint: systemd --user service not installed — run 'bpmute service install' to run in background.",
            app.theme.muted(),
        )]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn render_diagnostics_pane(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" Engine Diagnostics [d] ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let (slope_str, var_str, sqi_str, rmssd_str) = if let Some(ref epoch) = app.latest_epoch {
        (
            format!("{:+.2} bpm/min", epoch.features.hr_slope_4m * 60.0),
            format!("{:.2} bpm²", epoch.features.r_var_120s),
            format!("{:.2}", 1.0 - epoch.rr_artifact_frac),
            epoch
                .features
                .ln_rmssd_60s
                .map(|r| format!("{:.2}", r))
                .unwrap_or_else(|| "--".to_string()),
        )
    } else {
        ("--".into(), "--".into(), "--".into(), "--".into())
    };

    let idle_backend = app.idle_backend.as_deref().unwrap_or("None / Probing");
    let mpris_players = app.playing_players_count;

    let lines = vec![
        Line::from(vec![
            Span::styled(" HMM: ", app.theme.muted()),
            Span::styled(
                format!(
                    "Slope: {slope_str} | Var: {var_str} | SQI: {sqi_str} | ln(RMSSD): {rmssd_str} "
                ),
                app.theme.base(),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Desktop: ", app.theme.muted()),
            Span::styled(
                format!("Idle Probe: {idle_backend} | Playing MPRIS2 Players: {mpris_players} "),
                app.theme.base(),
            ),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn render_pause_popover(app: &TuiApp, selected_idx: usize, area: Rect, buf: &mut Buffer) {
    let popover_w = 44u16.min(area.width.saturating_sub(4));
    let popover_h = 9u16.min(area.height.saturating_sub(4));

    let popover_x = area.x + (area.width.saturating_sub(popover_w)) / 2;
    let popover_y = area.y + (area.height.saturating_sub(popover_h)) / 2;
    let popover_area = Rect::new(popover_x, popover_y, popover_w, popover_h);

    Clear.render(popover_area, buf);

    let block = Block::default()
        .title(" Pause / Resume Monitoring ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(popover_area);
    block.render(popover_area, buf);

    let options = [
        "1. Resume monitoring",
        "2. Pause indefinitely",
        "3. Pause 1 hour",
        "4. Pause 2 hours",
        "5. Pause until next morning",
    ];

    let mut lines = Vec::new();
    for (idx, opt) in options.iter().enumerate() {
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
