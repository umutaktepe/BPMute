use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::super::app::{ModalState, SoftRebaselineField, TuiApp};
use super::centered_rect;
use crate::config::{PowerAction, Sensitivity};

pub fn render_settings(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    if area.width < 10 || area.height < 5 {
        return;
    }

    let (_error_banner_area, content_area) = if let Some(err) = &app.config_error {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(4), Constraint::Min(6)])
            .split(area);
        render_config_error_banner(app, err, chunks[0], buf);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, area)
    };

    let col_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(content_area);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(6)])
        .split(col_chunks[0]);

    render_core_preferences(app, left_chunks[0], buf);
    render_advanced_settings(app, left_chunks[1], buf);
    render_calibration_storage(app, col_chunks[1], buf);

    match app.modal {
        ModalState::SoftRebaselineModal { .. } => render_soft_rebaseline_modal(app, area, buf),
        ModalState::PurgeRawModal { .. } => render_purge_raw_modal(app, area, buf),
        ModalState::HardWipeModal { .. } => render_hard_wipe_modal(app, area, buf),
        _ => {}
    }
}

fn render_config_error_banner(app: &TuiApp, err: &str, area: Rect, buf: &mut Buffer) {
    let block = app
        .theme
        .card_block(" Configuration Error ")
        .border_style(app.theme.danger());
    let inner = block.inner(area);
    block.render(area, buf);

    let lines = vec![
        Line::from(vec![
            Span::styled(" ⚠ ", app.theme.danger()),
            Span::styled(err, app.theme.danger()),
        ]),
        Line::from(vec![Span::styled(
            "   (Active configuration preserved in memory until corrected)",
            app.theme.muted(),
        )]),
    ];
    Paragraph::new(lines).render(inner, buf);
}

fn render_core_preferences(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app.theme.card_block(" Core Preferences ");
    let inner = block.inner(area);
    block.render(area, buf);

    let Some(ref cfg) = app.active_config else {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                " No configuration loaded.",
                app.theme.muted(),
            )]),
        ]);
        msg.render(inner, buf);
        return;
    };

    let sens_str = match cfg.detection.sensitivity {
        Sensitivity::Conservative => "Conservative",
        Sensitivity::Balanced => "Balanced",
        Sensitivity::Responsive => "Responsive",
    };

    let pause_str = if cfg.actions.pause_media { "ON" } else { "OFF" };
    let lock_str = if cfg.actions.lock_screen { "ON" } else { "OFF" };

    let power_str = match cfg.actions.power_action {
        PowerAction::Suspend => "Suspend",
        PowerAction::Poweroff => "Poweroff",
        PowerAction::None => "None",
    };

    let grace_str = match cfg.actions.grace_period_sec {
        Some(s) if s > 0 => format!("{s}s"),
        _ => "Disabled".to_string(),
    };

    let dry_str = if cfg.actions.dry_run { "ON" } else { "OFF" };

    let items = [
        ("Sensitivity", sens_str),
        ("Pause Media", pause_str),
        ("Lock Screen", lock_str),
        ("Power Action", power_str),
        ("Grace Period", grace_str.as_str()),
        ("Dry Run", dry_str),
    ];

    let mut lines = Vec::new();
    for (idx, (label, val)) in items.iter().enumerate() {
        let is_sel = app.selected_setting_idx == idx;
        let prefix = if is_sel { "> " } else { "  " };
        let prefix_style = if is_sel {
            app.theme.highlight()
        } else {
            app.theme.base()
        };
        let label_style = if is_sel {
            app.theme.highlight()
        } else {
            app.theme.base()
        };
        let val_style = if is_sel {
            app.theme.highlight()
        } else {
            app.theme.key_hint()
        };

        lines.push(Line::from(vec![
            Span::styled(prefix, prefix_style),
            Span::styled(format!("{label:<14} : "), label_style),
            Span::styled(format!("[ {val} ]"), val_style),
        ]));
    }

    lines.push(Line::from(""));
    if app.daemon_connected {
        lines.push(Line::from(vec![Span::styled(
            " [Space/Enter] Cycle/Toggle",
            app.theme.muted(),
        )]));
    } else {
        lines.push(Line::from(vec![Span::styled(
            " (mutations disabled: daemon offline)",
            app.theme.muted(),
        )]));
    }

    Paragraph::new(lines).render(inner, buf);
}

fn render_advanced_settings(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app.theme.card_block(" Advanced Settings (Read-Only) ");
    let inner = block.inner(area);
    block.render(area, buf);

    let Some(ref cfg) = app.active_config else {
        return;
    };

    let cmd_str = if cfg.actions.custom_command.is_empty() {
        "none".to_string()
    } else {
        cfg.actions.custom_command.join(" ")
    };

    let players_str = if cfg.actions.ignore_players.is_empty() {
        "none".to_string()
    } else {
        cfg.actions.ignore_players.join(", ")
    };

    let adapter_str = cfg.bluetooth.adapter.as_deref().unwrap_or("default");

    let lines = vec![
        Line::from(vec![
            Span::styled("  custom_command : ", app.theme.key_hint()),
            Span::styled(cmd_str, app.theme.base()),
        ]),
        Line::from(vec![
            Span::styled("  ignore_players : ", app.theme.key_hint()),
            Span::styled(players_str, app.theme.base()),
        ]),
        Line::from(vec![
            Span::styled("  adapter        : ", app.theme.key_hint()),
            Span::styled(adapter_str, app.theme.base()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [E] Edit in $EDITOR", app.theme.key_hint()),
            Span::styled(" (config.toml)", app.theme.muted()),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn render_calibration_storage(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app.theme.card_block(" System Settings & Storage ");
    let inner = block.inner(area);
    block.render(area, buf);

    let Some(ref stats) = app.storage_stats else {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                " No storage stats available.",
                app.theme.muted(),
            )]),
        ]);
        msg.render(inner, buf);
        return;
    };

    let maturity_str = format!("{:?}", stats.calibration_maturity);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("  DB Size             : ", app.theme.key_hint()),
            Span::styled(&stats.db_size_formatted, app.theme.base()),
        ]),
        Line::from(vec![
            Span::styled("  Sessions            : ", app.theme.key_hint()),
            Span::styled(
                format!("{} sessions", stats.sessions_count),
                app.theme.base(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Epochs              : ", app.theme.key_hint()),
            Span::styled(
                format!(
                    "{} epochs (retention: {})",
                    stats.session_epochs_count, stats.session_epochs_retention
                ),
                app.theme.base(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Raw Samples         : ", app.theme.key_hint()),
            Span::styled(
                format!(
                    "{} raw samples (retention: {})",
                    stats.raw_samples_count, stats.raw_samples_retention
                ),
                app.theme.base(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Calibration Maturity: ", app.theme.key_hint()),
            Span::styled(maturity_str, app.theme.base()),
        ]),
        Line::from(""),
    ];

    if let Some(ref bnd) = stats.active_calibration_boundary {
        let seed_str = bnd
            .seed_sleep_hr
            .map(|s| format!("{s:.1} bpm"))
            .unwrap_or_else(|| "none".to_string());
        lines.push(Line::from(vec![
            Span::styled("  Active Boundary     : ", app.theme.key_hint()),
            Span::styled(
                format!("{} ({:?}, seed: {seed_str})", bnd.id, bnd.reason),
                app.theme.base(),
            ),
        ]));
        if app.daemon_connected {
            lines.push(Line::from(vec![
                Span::styled("    [U] Undo Boundary", app.theme.key_hint()),
                Span::styled(" (revert to previous baseline)", app.theme.muted()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("    [U] Undo Boundary", app.theme.muted()),
                Span::styled(" (disabled: daemon offline)", app.theme.muted()),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("  Active Boundary     : ", app.theme.key_hint()),
            Span::styled("None (natural baseline)", app.theme.muted()),
        ]));
    }

    lines.push(Line::from(""));
    if app.daemon_connected {
        lines.push(Line::from(vec![
            Span::styled("  [B] Re-baseline  ", app.theme.key_hint()),
            Span::styled("[P] Purge Raw  ", app.theme.key_hint()),
            Span::styled("[W] Hard Wipe", app.theme.key_hint()),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  [B] Re-baseline  ", app.theme.muted()),
            Span::styled("[P] Purge Raw  ", app.theme.muted()),
            Span::styled("[W] Hard Wipe  ", app.theme.muted()),
            Span::styled("(disabled: daemon offline)", app.theme.muted()),
        ]));
    }

    Paragraph::new(lines).render(inner, buf);
}

fn render_soft_rebaseline_modal(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let ModalState::SoftRebaselineModal {
        ref seed_sleep_hr_input,
        reset_floors,
        active_field,
        ref error_message,
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 56, 12);
    Clear.render(modal_area, buf);

    let block = app
        .theme
        .card_block(" Soft Re-baseline Calibration ")
        .border_style(app.theme.primary());
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let is_seed_focused = active_field == SoftRebaselineField::SeedSleepHr;
    let is_reset_focused = active_field == SoftRebaselineField::ResetFloors;
    let is_submit_focused = active_field == SoftRebaselineField::Submit;
    let is_cancel_focused = active_field == SoftRebaselineField::Cancel;

    let seed_disp = if seed_sleep_hr_input.is_empty() {
        "e.g. 54.0"
    } else {
        seed_sleep_hr_input
    };
    let seed_style = if is_seed_focused {
        app.theme.highlight()
    } else {
        app.theme.base()
    };

    let reset_check = if reset_floors { "[x]" } else { "[ ]" };
    let reset_style = if is_reset_focused {
        app.theme.highlight()
    } else {
        app.theme.base()
    };

    let submit_style = if is_submit_focused {
        app.theme.highlight()
    } else {
        app.theme.base()
    };
    let cancel_style = if is_cancel_focused {
        app.theme.highlight()
    } else {
        app.theme.base()
    };

    let mut lines = vec![
        Line::from(vec![Span::styled(
            " Insert calibration boundary to reset baseline filters.",
            app.theme.muted(),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Seed Sleep HR (BPM): ", app.theme.key_hint()),
            Span::styled(format!("[ {seed_disp:<10} ]"), seed_style),
        ]),
        Line::from(vec![
            Span::styled(" Reset Floors       : ", app.theme.key_hint()),
            Span::styled(
                format!("{reset_check} Force cold-start floors"),
                reset_style,
            ),
        ]),
        Line::from(""),
    ];

    if let Some(err) = error_message {
        lines.push(Line::from(vec![
            Span::styled(" ⚠ ", app.theme.danger()),
            Span::styled(err, app.theme.danger()),
        ]));
    } else {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled("     [ Submit ]", submit_style),
        Span::styled("     [ Cancel ]", cancel_style),
    ]));

    Paragraph::new(lines).render(inner, buf);
}

fn render_purge_raw_modal(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let modal_area = centered_rect(area, 52, 8);
    Clear.render(modal_area, buf);

    let block = app
        .theme
        .card_block(" Purge Raw Samples ")
        .border_style(app.theme.warning());
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            " Purge all raw biometrics older than retention window?",
            app.theme.base(),
        )]),
        Line::from(vec![Span::styled(
            " (Epoch metrics and session history are preserved)",
            app.theme.muted(),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [Enter] Confirm Purge  ", app.theme.warning()),
            Span::styled("  [Esc] Cancel", app.theme.base()),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn render_hard_wipe_modal(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let ModalState::HardWipeModal {
        ref typed_input,
        ref error_message,
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 58, 10);
    Clear.render(modal_area, buf);

    let block = app
        .theme
        .card_block(" Hard Wipe Database ")
        .border_style(app.theme.danger());
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let mut lines = vec![
        Line::from(vec![Span::styled(
            " DANGER: Permanently wipe all sessions, epochs, & samples!",
            app.theme.danger(),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Type 'WIPE' to confirm: ", app.theme.key_hint()),
            Span::styled(format!("[ {:<8} ]", typed_input), app.theme.highlight()),
        ]),
    ];

    if let Some(err) = error_message {
        lines.push(Line::from(vec![
            Span::styled(" ⚠ ", app.theme.danger()),
            Span::styled(err, app.theme.danger()),
        ]));
    } else {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  [Enter] Confirm Wipe  ", app.theme.danger()),
        Span::styled("  [Esc] Cancel", app.theme.base()),
    ]));

    Paragraph::new(lines).render(inner, buf);
}
