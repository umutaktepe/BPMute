use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Row, Table, Widget};

use super::super::app::{ModalState, PairingPromptModalState, TuiApp, UnenrollButton};
use super::centered_rect;

pub fn render_wearables(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    if area.height < 5 || area.width < 10 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(3)])
        .split(area);

    let table_area = chunks[0];
    let action_area = chunks[1];

    render_wearables_table(app, table_area, buf);
    render_wearables_actions(app, action_area, buf);

    match app.modal {
        ModalState::EnrollmentModal { .. } => render_enrollment_modal(app, area, buf),
        ModalState::RenameModal { .. } => render_rename_modal(app, area, buf),
        ModalState::UnenrollModal { .. } => render_unenroll_modal(app, area, buf),
        _ => {}
    }
}

fn render_wearables_table(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app.theme.card_block(" Enrolled Wearables ");
    let header = Row::new(vec![
        "Alias / Name",
        "MAC Address",
        "Status",
        "Battery",
        "Capabilities",
        "Last Seen",
    ])
    .style(app.theme.table_header());

    let rows: Vec<Row> = app
        .enrolled_wearables
        .iter()
        .enumerate()
        .map(|(idx, w)| {
            let is_selected = idx == app.selected_wearable_idx;
            let row_style = if is_selected {
                app.theme.highlight()
            } else {
                app.theme.base()
            };

            let name = w
                .wearable
                .alias
                .as_deref()
                .or(w.wearable.advertised_name.as_deref())
                .unwrap_or("Unknown");

            let mac = &w.wearable.mac_masked;
            let status_str = w.status.display_label().to_string();
            let bat_str = w
                .battery
                .map(|b| format!("{b}%"))
                .unwrap_or_else(|| "--".to_string());
            let base_cap = if w.wearable.supports_rr {
                "BPM+RR"
            } else {
                "BPM-only"
            };
            let caps = match w.body_location.as_deref() {
                Some(loc) if !loc.is_empty() => format!("{base_cap} ({loc})"),
                _ => base_cap.to_string(),
            };
            let seen = if w.wearable.last_seen_at_ms > 0 {
                "Recent"
            } else {
                "Never"
            };

            Row::new(vec![
                name.to_string(),
                mac.to_string(),
                status_str,
                bat_str,
                caps,
                seen.to_string(),
            ])
            .style(row_style)
        })
        .collect();

    let widths = [
        Constraint::Min(12),
        Constraint::Length(9),
        Constraint::Length(15),
        Constraint::Length(7),
        Constraint::Min(16),
        Constraint::Length(9),
    ];

    let table = Table::new(rows, widths).header(header).block(block);
    Widget::render(table, area, buf);
}

fn render_wearables_actions(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app.theme.card_block("");
    let inner = block.inner(area);
    block.render(area, buf);

    let key_style = if app.daemon_connected {
        app.theme.key_hint()
    } else {
        app.theme.muted()
    };
    let text_style = if app.daemon_connected {
        app.theme.base()
    } else {
        app.theme.muted()
    };

    let mut spans = vec![
        Span::styled("  [e]", key_style),
        Span::styled(" Enroll new  ", text_style),
        Span::styled("[n]", key_style),
        Span::styled(" Rename alias  ", text_style),
        Span::styled("[c]", key_style),
        Span::styled(" Reconnect  ", text_style),
        Span::styled("[u]", key_style),
        Span::styled(" Unenroll", text_style),
    ];
    if !app.daemon_connected {
        spans.push(Span::styled("  (offline: read-only)", app.theme.muted()));
    }

    Paragraph::new(vec![Line::from(spans)]).render(inner, buf);
}

fn render_enrollment_modal(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let ModalState::EnrollmentModal {
        scanning_remaining_ms,
        selected_discovered_idx,
        ref alias_input,
        alias_focused,
        enrolling,
        ref error_message,
        ref pairing_prompt,
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 64, 16);
    Clear.render(modal_area, buf);

    if let Some(prompt) = pairing_prompt {
        render_pairing_prompt_modal(app, prompt, modal_area, buf);
        return;
    }

    let rem_sec = scanning_remaining_ms.div_ceil(1000);
    let title = if enrolling {
        " Enroll Wearable (Bonding / Pairing...) ".to_string()
    } else {
        format!(" Enroll Wearable (Scanning... {}s) ", rem_sec)
    };
    let block = app.theme.card_block(title);
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(inner);

    let status_line = if let Some(err) = error_message {
        Line::from(vec![Span::styled(
            format!(" Error: {err}"),
            app.theme.danger(),
        )])
    } else if enrolling {
        Line::from(vec![Span::styled(
            " Enrolling Wearable... (awaiting Bluetooth pairing)",
            app.theme.warning(),
        )])
    } else {
        Line::from(vec![Span::styled(
            " Select a discovered 0x180D Heart Rate Wearable from BlueZ:",
            app.theme.muted(),
        )])
    };
    Paragraph::new(status_line).render(chunks[0], buf);

    let mut dev_lines = Vec::new();
    if app.discovered_wearables.is_empty() {
        dev_lines.push(Line::from(vec![Span::styled(
            "   (Searching for advertising BLE Wearables...)",
            app.theme.muted(),
        )]));
    } else {
        for (idx, d) in app.discovered_wearables.iter().enumerate() {
            let is_selected = idx == selected_discovered_idx && !alias_focused;
            let style = if is_selected {
                app.theme.highlight()
            } else {
                app.theme.base()
            };
            let name = d.name.as_deref().unwrap_or("Unknown");
            let rssi = d
                .rssi
                .map(|r| format!("{r} dBm"))
                .unwrap_or_else(|| "-".into());
            let paired = if d.paired { "[Paired]" } else { "" };
            let prefix = if is_selected { " > " } else { "   " };

            dev_lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(
                    format!("{} ({})  {} {}", d.address, name, rssi, paired),
                    style,
                ),
            ]));
        }
    }
    Paragraph::new(dev_lines).render(chunks[1], buf);

    let alias_block = app
        .theme
        .card_block(" Custom Alias (Optional) [Tab to focus] ")
        .border_style(if alias_focused {
            app.theme.primary()
        } else {
            app.theme.card_border()
        });
    let alias_inner = alias_block.inner(chunks[2]);
    alias_block.render(chunks[2], buf);

    let cursor = if alias_focused {
        if app.theme.ascii { "_" } else { "█" }
    } else {
        ""
    };
    let input_line = Line::from(vec![
        Span::styled(format!(" {}{cursor}", alias_input), app.theme.base()),
        if alias_input.is_empty() && !alias_focused {
            Span::styled("Press Tab to type custom alias...", app.theme.muted())
        } else {
            Span::raw("")
        },
    ]);
    Paragraph::new(input_line).render(alias_inner, buf);

    let footer_line = Line::from(vec![
        Span::styled(" [Enter]", app.theme.key_hint()),
        Span::styled(" Enroll  ", app.theme.base()),
        Span::styled("[r]", app.theme.key_hint()),
        Span::styled(" Rescan  ", app.theme.base()),
        Span::styled("[Esc]", app.theme.key_hint()),
        Span::styled(" Cancel", app.theme.base()),
    ]);
    Paragraph::new(footer_line).render(chunks[3], buf);
}

fn render_pairing_prompt_modal(
    app: &TuiApp,
    prompt: &PairingPromptModalState,
    area: Rect,
    buf: &mut Buffer,
) {
    let block = app
        .theme
        .card_block(" Bluetooth LE Pairing Authorization (Agent1) ")
        .border_style(app.theme.primary());

    let inner = block.inner(area);
    block.render(area, buf);

    let lines = match prompt {
        PairingPromptModalState::Confirmation { passkey, wearable } => {
            vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Wearable: ", app.theme.muted()),
                    Span::styled(wearable, app.theme.base()),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    " Confirm numeric passkey matches wearable: ",
                    app.theme.warning(),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!("       {:06}       ", passkey),
                    app.theme.highlight(),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Press ", app.theme.base()),
                    Span::styled("[y]", app.theme.key_hint()),
                    Span::styled(" to Confirm, or ", app.theme.base()),
                    Span::styled("[n]/[Esc]", app.theme.key_hint()),
                    Span::styled(" to Reject.", app.theme.base()),
                ]),
            ]
        }
        PairingPromptModalState::Passkey { wearable, input } => {
            vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Wearable: ", app.theme.muted()),
                    Span::styled(wearable, app.theme.base()),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    " Enter 6-digit Bluetooth passkey: ",
                    app.theme.warning(),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!(" [ {} ]", input),
                    app.theme.highlight(),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    " Press [Enter] to submit, [Esc] to cancel.",
                    app.theme.base(),
                )]),
            ]
        }
        PairingPromptModalState::PinCode { wearable, input } => {
            vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Wearable: ", app.theme.muted()),
                    Span::styled(wearable, app.theme.base()),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    " Enter Bluetooth PIN code: ",
                    app.theme.warning(),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!(" [ {} ]", input),
                    app.theme.highlight(),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    " Press [Enter] to submit, [Esc] to cancel.",
                    app.theme.base(),
                )]),
            ]
        }
    };

    Paragraph::new(lines).render(inner, buf);
}

fn render_rename_modal(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let ModalState::RenameModal {
        ref current_alias,
        ref input,
        ..
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 48, 8);
    Clear.render(modal_area, buf);

    let block = app.theme.card_block(" Rename Wearable Alias ");
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let cursor = if app.theme.ascii { "_" } else { "█" };
    let lines = vec![
        Line::from(vec![
            Span::styled(" Current: ", app.theme.muted()),
            Span::styled(current_alias.as_str(), app.theme.base()),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            format!(" New alias: [ {}{cursor} ]", input),
            app.theme.primary(),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" [Enter]", app.theme.key_hint()),
            Span::styled(" Save   ", app.theme.base()),
            Span::styled("[Esc]", app.theme.key_hint()),
            Span::styled(" Cancel", app.theme.base()),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn render_unenroll_modal(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let ModalState::UnenrollModal {
        ref wearable,
        forget_bluez,
        delete_sessions,
        selected_button,
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 58, 12);
    Clear.render(modal_area, buf);

    let block = app.theme.card_block(" Unenroll Wearable ");
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let name = wearable
        .wearable
        .alias
        .as_deref()
        .or(wearable.wearable.advertised_name.as_deref())
        .unwrap_or("Wearable");

    let cb1 = if forget_bluez { "[X]" } else { "[ ]" };
    let cb2 = if delete_sessions { "[X]" } else { "[ ]" };

    let btn0_style = if selected_button == UnenrollButton::Confirm {
        app.theme.highlight()
    } else {
        app.theme.base()
    };

    let btn1_style = if selected_button == UnenrollButton::Cancel {
        app.theme.highlight()
    } else {
        app.theme.base()
    };

    let lines = vec![
        Line::from(vec![Span::styled(
            format!(" Remove '{}' from BPMute allowlist?", name),
            app.theme.base(),
        )]),
        Line::from(vec![Span::styled(
            " (Non-destructive by default)",
            app.theme.muted(),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("  {} [1] Also forget from BlueZ", cb1),
            app.theme.base(),
        )]),
        Line::from(vec![Span::styled(
            format!("  {} [2] Also delete historical Monitoring Sessions", cb2),
            app.theme.base(),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("   ", app.theme.base()),
            Span::styled(" [ Confirm ] ", btn0_style),
            Span::styled("   ", app.theme.base()),
            Span::styled(" [ Cancel ] ", btn1_style),
        ]),
    ];

    Paragraph::new(lines).render(inner, buf);
}
