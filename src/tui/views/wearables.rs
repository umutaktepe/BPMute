use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Widget};

use super::super::app::{ModalState, PairingPromptModalState, TuiApp};

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

    // 1. Render Table of Enrolled Wearables
    render_wearables_table(app, table_area, buf);

    // 2. Render Action Bar
    render_wearables_actions(app, action_area, buf);

    // 3. Render any active modal
    match app.modal {
        ModalState::EnrollmentModal { .. } => {
            render_enrollment_modal(app, area, buf);
        }
        ModalState::RenameModal { .. } => {
            render_rename_modal(app, area, buf);
        }
        ModalState::UnenrollModal { .. } => {
            render_unenroll_modal(app, area, buf);
        }
        _ => {}
    }
}

fn render_wearables_table(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" Enrolled Wearables ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let header_style = if app.theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };

    let header = Row::new(vec![
        "Alias / Name",
        "MAC Address",
        "Status",
        "Battery",
        "Capabilities",
        "Last Seen",
    ])
    .style(header_style);

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
            let status_str = format!("{:?}", w.status);
            let bat_str = w
                .battery
                .map(|b| format!("{b}%"))
                .unwrap_or_else(|| "--".to_string());
            let caps = if w.wearable.supports_rr {
                "BPM+RR"
            } else {
                "BPM-only"
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
                caps.to_string(),
                seen.to_string(),
            ])
            .style(row_style)
        })
        .collect();

    let widths = [
        Constraint::Percentage(25),
        Constraint::Percentage(20),
        Constraint::Percentage(18),
        Constraint::Percentage(10),
        Constraint::Percentage(15),
        Constraint::Percentage(12),
    ];

    let table = Table::new(rows, widths).header(header).block(block);

    Widget::render(table, area, buf);
}

fn render_wearables_actions(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(area);
    block.render(area, buf);

    let lines = vec![Line::from(vec![
        Span::styled("  [e]", app.theme.key_hint()),
        Span::styled(" Enroll new  ", app.theme.base()),
        Span::styled("[n]", app.theme.key_hint()),
        Span::styled(" Rename alias  ", app.theme.base()),
        Span::styled("[c]", app.theme.key_hint()),
        Span::styled(" Reconnect  ", app.theme.base()),
        Span::styled("[u]", app.theme.key_hint()),
        Span::styled(" Unenroll", app.theme.base()),
    ])];

    Paragraph::new(lines).render(inner, buf);
}

fn render_enrollment_modal(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let ModalState::EnrollmentModal {
        scanning_timer_sec,
        selected_device_idx,
        ref alias_input,
        alias_focused,
        ref pairing_prompt,
    } = app.modal
    else {
        return;
    };

    let modal_w = 64u16.min(area.width.saturating_sub(4));
    let modal_h = 16u16.min(area.height.saturating_sub(4));

    let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_w, modal_h);

    Clear.render(modal_area, buf);

    // If pairing prompt is active, render pairing prompt dialog over enrollment
    if let Some(prompt) = pairing_prompt {
        render_pairing_prompt_modal(app, prompt, modal_area, buf);
        return;
    }

    let block = Block::default()
        .title(format!(
            " Enroll Wearable (Scanning... {}s) ",
            scanning_timer_sec
        ))
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

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

    // Subtitle / Scanning status
    let status_line = Line::from(vec![Span::styled(
        " Select a discovered 0x180D Heart Rate wearable from BlueZ: ",
        app.theme.muted(),
    )]);
    Paragraph::new(status_line).render(chunks[0], buf);

    // Discovered devices list
    let mut dev_lines = Vec::new();
    if app.discovered_wearables.is_empty() {
        dev_lines.push(Line::from(vec![Span::styled(
            "   (Searching for advertising BLE wearables...)",
            app.theme.muted(),
        )]));
    } else {
        for (idx, d) in app.discovered_wearables.iter().enumerate() {
            let is_selected = idx == selected_device_idx && !alias_focused;
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

    // Alias Input field
    let alias_block = Block::default()
        .title(" Custom Alias (Optional) [Tab to focus] ")
        .borders(Borders::ALL)
        .border_style(if alias_focused {
            app.theme.primary()
        } else {
            app.theme.card_border()
        });
    let alias_inner = alias_block.inner(chunks[2]);
    alias_block.render(chunks[2], buf);

    let input_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(alias_input.as_str(), app.theme.base()),
        if alias_focused {
            Span::styled("█", app.theme.primary())
        } else {
            Span::raw("")
        },
    ]);
    Paragraph::new(input_line).render(alias_inner, buf);

    // Footer actions
    let footer_line = Line::from(vec![
        Span::styled(" [Enter]", app.theme.key_hint()),
        Span::styled(" Enroll Selected   ", app.theme.base()),
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
    let block = Block::default()
        .title(" Bluetooth LE Pairing Authorization (Agent1) ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.primary());

    let inner = block.inner(area);
    block.render(area, buf);

    let lines = match prompt {
        PairingPromptModalState::Confirmation { passkey, device } => {
            vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Device: ", app.theme.muted()),
                    Span::styled(device, app.theme.base()),
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
        PairingPromptModalState::Passkey { device, input } => {
            vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Device: ", app.theme.muted()),
                    Span::styled(device, app.theme.base()),
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
        PairingPromptModalState::PinCode { device, input } => {
            vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Device: ", app.theme.muted()),
                    Span::styled(device, app.theme.base()),
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

    let modal_w = 48u16.min(area.width.saturating_sub(4));
    let modal_h = 8u16.min(area.height.saturating_sub(4));

    let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_w, modal_h);

    Clear.render(modal_area, buf);

    let block = Block::default()
        .title(" Rename Wearable Alias ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let lines = vec![
        Line::from(vec![
            Span::styled(" Current: ", app.theme.muted()),
            Span::styled(current_alias.as_str(), app.theme.base()),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            format!(" New alias: [ {}█ ]", input),
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

    let modal_w = 58u16.min(area.width.saturating_sub(4));
    let modal_h = 12u16.min(area.height.saturating_sub(4));

    let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_w, modal_h);

    Clear.render(modal_area, buf);

    let block = Block::default()
        .title(" Unenroll Wearable ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

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

    let btn0_style = if selected_button == 0 {
        app.theme.highlight()
    } else {
        app.theme.base()
    };

    let btn1_style = if selected_button == 1 {
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
