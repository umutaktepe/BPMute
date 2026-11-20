use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::super::app::{ModalState, SoftRebaselineField, TuiApp};
use super::super::hit_map::{ClickTarget, HitMap, ScrollTarget};
use super::{build_clickable_row_line, build_modal_clickable_row_line, centered_rect};
use crate::config::{PowerAction, Sensitivity};

pub(crate) fn render_settings(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
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

    render_core_preferences(app, left_chunks[0], buf, hit_map);
    render_advanced_settings(app, left_chunks[1], buf, hit_map);
    render_calibration_storage(app, col_chunks[1], buf, hit_map);

    match app.modal {
        ModalState::SoftRebaselineModal { .. } => {
            render_soft_rebaseline_modal(app, area, buf, hit_map)
        }
        ModalState::PurgeRawModal { .. } => render_purge_raw_modal(app, area, buf, hit_map),
        ModalState::HardWipeModal { .. } => render_hard_wipe_modal(app, area, buf, hit_map),
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

fn render_core_preferences(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    hit_map.push_scroll(area, ScrollTarget::SettingsPreferences);
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
        let row_y = inner.y.saturating_add(idx as u16);
        hit_map.push_clipped_row_click(
            inner,
            inner.x,
            row_y,
            inner.width,
            ClickTarget::SettingsSelectOrToggle(idx),
        );

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

fn render_advanced_settings(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
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

    let mut lines = vec![
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
    ];
    let editor_y = inner.y.saturating_add(lines.len() as u16);
    lines.push(build_clickable_row_line(
        inner,
        editor_y,
        vec![
            (Span::styled("  ", app.theme.base()), None),
            (
                Span::styled("[E] Edit in $EDITOR", app.theme.key_hint()),
                Some(ClickTarget::SettingsEditorHandoff),
            ),
            (Span::styled(" (config.toml)", app.theme.muted()), None),
        ],
        hit_map,
    ));

    Paragraph::new(lines).render(inner, buf);
}

fn render_calibration_storage(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
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
        let undo_y = inner.y.saturating_add(lines.len() as u16);
        let undo_style = if app.daemon_connected {
            app.theme.key_hint()
        } else {
            app.theme.muted()
        };
        let undo_suffix = if app.daemon_connected {
            " (revert to previous baseline)"
        } else {
            " (disabled: daemon offline)"
        };
        lines.push(build_clickable_row_line(
            inner,
            undo_y,
            vec![
                (Span::styled("    ", app.theme.base()), None),
                (
                    Span::styled("[U] Undo Boundary", undo_style),
                    app.daemon_connected
                        .then_some(ClickTarget::SettingsUndoBoundary),
                ),
                (Span::styled(undo_suffix, app.theme.muted()), None),
            ],
            hit_map,
        ));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  Active Boundary     : ", app.theme.key_hint()),
            Span::styled("None (natural baseline)", app.theme.muted()),
        ]));
    }

    lines.push(Line::from(""));
    let btn_style = if app.daemon_connected {
        app.theme.key_hint()
    } else {
        app.theme.muted()
    };
    let buttons = [
        ("[B] Re-baseline", ClickTarget::SettingsOpenRebase),
        ("[P] Purge Raw", ClickTarget::SettingsOpenPurge),
        ("[W] Hard Wipe", ClickTarget::SettingsOpenWipe),
    ];
    let max_w = usize::from(inner.width);
    let mut row_items: Vec<(Span<'_>, Option<ClickTarget>)> = Vec::new();
    let mut row_w = 0usize;
    for (label, target) in buttons {
        let gap = "  ";
        let needed = gap.len() + label.len();
        if row_w + needed > max_w {
            flush_wrapped_row(&mut lines, &mut row_items, &mut row_w, inner, hit_map);
        }
        row_items.push((Span::styled(gap, app.theme.base()), None));
        row_items.push((
            Span::styled(label, btn_style),
            app.daemon_connected.then_some(target),
        ));
        row_w += needed;
    }
    if !app.daemon_connected {
        let offline_note = "  (disabled: daemon offline)";
        if row_w + offline_note.len() > max_w {
            flush_wrapped_row(&mut lines, &mut row_items, &mut row_w, inner, hit_map);
        }
        row_items.push((Span::styled(offline_note, app.theme.muted()), None));
    }
    flush_wrapped_row(&mut lines, &mut row_items, &mut row_w, inner, hit_map);

    Paragraph::new(lines).render(inner, buf);
}

fn flush_wrapped_row<'a>(
    lines: &mut Vec<Line<'a>>,
    items: &mut Vec<(Span<'a>, Option<ClickTarget>)>,
    width: &mut usize,
    inner: Rect,
    hit_map: &mut HitMap,
) {
    if items.is_empty() {
        return;
    }
    let y = inner.y.saturating_add(lines.len() as u16);
    lines.push(build_clickable_row_line(
        inner,
        y,
        std::mem::take(items),
        hit_map,
    ));
    *width = 0;
}

fn push_modal_error_line<'a>(lines: &mut Vec<Line<'a>>, err: Option<&'a str>, app: &TuiApp) {
    if let Some(err) = err {
        lines.push(Line::from(vec![
            Span::styled(" ⚠ ", app.theme.danger()),
            Span::styled(err, app.theme.danger()),
        ]));
    } else {
        lines.push(Line::from(""));
    }
}

struct ModalButtons<'a> {
    leading_pad: &'a str,
    confirm_label: &'a str,
    confirm_style: Style,
    between_pad: &'a str,
    cancel_label: &'a str,
    cancel_style: Style,
}

fn push_modal_button_row<'a>(
    lines: &mut Vec<Line<'a>>,
    inner: Rect,
    spec: ModalButtons<'a>,
    hit_map: &mut HitMap,
) {
    let y = inner.y.saturating_add(lines.len() as u16);
    lines.push(build_modal_clickable_row_line(
        inner,
        y,
        vec![
            (Span::styled(spec.leading_pad, Style::default()), None),
            (
                Span::styled(spec.confirm_label, spec.confirm_style),
                Some(ClickTarget::ModalConfirm),
            ),
            (Span::styled(spec.between_pad, Style::default()), None),
            (
                Span::styled(spec.cancel_label, spec.cancel_style),
                Some(ClickTarget::CloseModal),
            ),
        ],
        hit_map,
    ));
}

fn render_soft_rebaseline_modal(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
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
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);
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

    let seed_target = Some(ClickTarget::SoftRebaselineFocusField(
        SoftRebaselineField::SeedSleepHr,
    ));
    let mut lines = vec![
        Line::from(vec![Span::styled(
            " Insert calibration boundary to reset baseline filters.",
            app.theme.muted(),
        )]),
        Line::from(""),
    ];
    let seed_y = inner.y.saturating_add(lines.len() as u16);
    lines.push(build_modal_clickable_row_line(
        inner,
        seed_y,
        vec![
            (
                Span::styled(" Seed Sleep HR (BPM): ", app.theme.key_hint()),
                seed_target,
            ),
            (
                Span::styled(format!("[ {seed_disp:<10} ]"), seed_style),
                seed_target,
            ),
        ],
        hit_map,
    ));
    let reset_y = inner.y.saturating_add(lines.len() as u16);
    lines.push(build_modal_clickable_row_line(
        inner,
        reset_y,
        vec![
            (
                Span::styled(" Reset Floors       : ", app.theme.key_hint()),
                None,
            ),
            (
                Span::styled(
                    format!("{reset_check} Force cold-start floors"),
                    reset_style,
                ),
                Some(ClickTarget::SoftRebaselineToggleResetFloors),
            ),
        ],
        hit_map,
    ));
    lines.push(Line::from(""));

    push_modal_error_line(&mut lines, error_message.as_deref(), app);
    push_modal_button_row(
        &mut lines,
        inner,
        ModalButtons {
            leading_pad: "     ",
            confirm_label: "[ Submit ]",
            confirm_style: submit_style,
            between_pad: "     ",
            cancel_label: "[ Cancel ]",
            cancel_style,
        },
        hit_map,
    );

    Paragraph::new(lines).render(inner, buf);
}

fn render_purge_raw_modal(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    let modal_area = centered_rect(area, 52, 8);
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);
    Clear.render(modal_area, buf);

    let block = app
        .theme
        .card_block(" Purge Raw Samples ")
        .border_style(app.theme.warning());
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let mut lines = vec![
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
    ];
    push_modal_button_row(
        &mut lines,
        inner,
        ModalButtons {
            leading_pad: "  ",
            confirm_label: "[Enter] Confirm Purge",
            confirm_style: app.theme.warning(),
            between_pad: "    ",
            cancel_label: "[Esc] Cancel",
            cancel_style: app.theme.base(),
        },
        hit_map,
    );

    Paragraph::new(lines).render(inner, buf);
}

fn render_hard_wipe_modal(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    let ModalState::HardWipeModal {
        ref typed_input,
        ref error_message,
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 58, 10);
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);
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
    ];
    let wipe_y = inner.y.saturating_add(lines.len() as u16);
    lines.push(build_modal_clickable_row_line(
        inner,
        wipe_y,
        vec![
            (
                Span::styled(" Type 'WIPE' to confirm: ", app.theme.key_hint()),
                Some(ClickTarget::FocusModalInput),
            ),
            (
                Span::styled(format!("[ {:<8} ]", typed_input), app.theme.highlight()),
                Some(ClickTarget::FocusModalInput),
            ),
        ],
        hit_map,
    ));

    push_modal_error_line(&mut lines, error_message.as_deref(), app);
    lines.push(Line::from(""));
    push_modal_button_row(
        &mut lines,
        inner,
        ModalButtons {
            leading_pad: "  ",
            confirm_label: "[Enter] Confirm Wipe",
            confirm_style: app.theme.danger(),
            between_pad: "    ",
            cancel_label: "[Esc] Cancel",
            cancel_style: app.theme.base(),
        },
        hit_map,
    );

    Paragraph::new(lines).render(inner, buf);
}
