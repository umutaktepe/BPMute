use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Row, Table, Widget};

use super::super::app::{ModalState, PairingPromptModalState, TuiApp, UnenrollButton};
use super::super::hit_map::{ClickTarget, HitMap, ScrollTarget, UnenrollCheckbox};
use super::{build_modal_clickable_row_line, centered_rect, fit_scroll_offset};

pub(crate) struct WearablesScrollOffsets {
    pub(crate) table: usize,
    pub(crate) enrollment: usize,
}

pub(crate) fn render_wearables(
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) -> WearablesScrollOffsets {
    if area.height < 5 || area.width < 10 {
        return WearablesScrollOffsets {
            table: app.wearables_scroll_offset,
            enrollment: app.enrollment_scroll_offset,
        };
    }

    let table = render_wearables_table(app, area, buf, hit_map);

    let enrollment = match app.modal {
        ModalState::EnrollmentModal { .. } => render_enrollment_modal(app, area, buf, hit_map),
        ModalState::RenameModal { .. } => {
            render_rename_modal(app, area, buf, hit_map);
            app.enrollment_scroll_offset
        }
        ModalState::UnenrollModal { .. } => {
            render_unenroll_modal(app, area, buf, hit_map);
            app.enrollment_scroll_offset
        }
        _ => app.enrollment_scroll_offset,
    };

    WearablesScrollOffsets { table, enrollment }
}

fn render_wearables_table(
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) -> usize {
    hit_map.push_scroll(area, ScrollTarget::WearablesTable);
    let block = app.theme.card_block(" Enrolled Wearables ");
    let inner = block.inner(area);
    let visible_rows = usize::from(inner.height.saturating_sub(1));
    let scroll_offset = fit_scroll_offset(
        app.wearables_scroll_offset,
        app.selected_wearable_idx,
        app.enrolled_wearables.len(),
        visible_rows,
    );
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
        .skip(scroll_offset)
        .take(visible_rows)
        .enumerate()
        .map(|(visual_idx, (idx, w))| {
            hit_map.push_click(
                Rect::new(
                    inner.x,
                    inner.y.saturating_add(1 + visual_idx as u16),
                    inner.width,
                    1,
                ),
                ClickTarget::SelectWearable(idx),
            );
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
    scroll_offset
}

fn render_enrollment_modal(
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) -> usize {
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
        return app.enrollment_scroll_offset;
    };

    let modal_area = centered_rect(area, 64, 16);
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);
    Clear.render(modal_area, buf);

    if let Some(prompt) = pairing_prompt {
        render_pairing_prompt_modal(app, prompt, modal_area, buf, hit_map);
        return app.enrollment_scroll_offset;
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

    let visible_rows = usize::from(chunks[1].height);
    let scroll_offset = fit_scroll_offset(
        app.enrollment_scroll_offset,
        selected_discovered_idx,
        app.discovered_wearables.len(),
        visible_rows,
    );
    if !enrolling {
        hit_map.push_modal_scroll(chunks[1], ScrollTarget::EnrollmentList);
        hit_map.push_modal_click(chunks[1], ClickTarget::EnrollmentFocusList);
        hit_map.push_modal_click(chunks[2], ClickTarget::FocusModalInput);
    }

    let mut dev_lines = Vec::new();
    if app.discovered_wearables.is_empty() {
        dev_lines.push(Line::from(vec![Span::styled(
            "   (Searching for advertising BLE Wearables...)",
            app.theme.muted(),
        )]));
    } else {
        for (visual_row, (idx, d)) in app
            .discovered_wearables
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_rows)
            .enumerate()
        {
            if !enrolling {
                hit_map.push_modal_click(
                    Rect::new(
                        chunks[1].x,
                        chunks[1].y.saturating_add(visual_row as u16),
                        chunks[1].width,
                        1,
                    ),
                    ClickTarget::EnrollmentSelectRow(idx),
                );
            }
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

    let enroll_target = (!enrolling).then_some(ClickTarget::ModalConfirm);
    let rescan_target = (!enrolling).then_some(ClickTarget::ModalRescan);
    let footer_line = build_modal_clickable_row_line(
        chunks[3],
        chunks[3].y,
        vec![
            (Span::raw(" "), None),
            (Span::styled("[Enter]", app.theme.key_hint()), enroll_target),
            (Span::styled(" Enroll", app.theme.base()), enroll_target),
            (Span::raw("  "), None),
            (Span::styled("[r]", app.theme.key_hint()), rescan_target),
            (Span::styled(" Rescan", app.theme.base()), rescan_target),
            (Span::raw("  "), None),
            (
                Span::styled("[Esc]", app.theme.key_hint()),
                Some(ClickTarget::CloseModal),
            ),
            (
                Span::styled(" Cancel", app.theme.base()),
                Some(ClickTarget::CloseModal),
            ),
        ],
        hit_map,
    );
    Paragraph::new(footer_line).render(chunks[3], buf);
    scroll_offset
}

fn render_pairing_prompt_modal(
    app: &TuiApp,
    prompt: &PairingPromptModalState,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) {
    let block = app
        .theme
        .card_block(" Bluetooth LE Pairing Authorization (Agent1) ")
        .border_style(app.theme.primary());

    let inner = block.inner(area);
    block.render(area, buf);

    let action_y = inner.y.saturating_add(7);
    let lines = match prompt {
        PairingPromptModalState::Confirmation { passkey, wearable } => {
            let action_line = build_modal_clickable_row_line(
                inner,
                action_y,
                vec![
                    (Span::styled(" Press ", app.theme.base()), None),
                    (
                        Span::styled("[y]", app.theme.key_hint()),
                        Some(ClickTarget::PairingConfirm(true)),
                    ),
                    (Span::styled(" to Confirm, or ", app.theme.base()), None),
                    (
                        Span::styled("[n]", app.theme.key_hint()),
                        Some(ClickTarget::PairingConfirm(false)),
                    ),
                    (Span::styled("/", app.theme.key_hint()), None),
                    (
                        Span::styled("[Esc]", app.theme.key_hint()),
                        Some(ClickTarget::CloseModal),
                    ),
                    (Span::styled(" to Reject.", app.theme.base()), None),
                ],
                hit_map,
            );
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
                action_line,
            ]
        }
        PairingPromptModalState::Passkey { wearable, input } => pairing_input_lines(
            app,
            wearable,
            " Enter 6-digit Bluetooth passkey: ",
            input,
            inner,
            action_y,
            hit_map,
        ),
        PairingPromptModalState::PinCode { wearable, input } => pairing_input_lines(
            app,
            wearable,
            " Enter Bluetooth PIN code: ",
            input,
            inner,
            action_y,
            hit_map,
        ),
    };

    Paragraph::new(lines).render(inner, buf);
}

fn pairing_input_lines<'a>(
    app: &TuiApp,
    wearable: &'a str,
    prompt_label: &'static str,
    input: &str,
    inner: Rect,
    action_y: u16,
    hit_map: &mut HitMap,
) -> Vec<Line<'a>> {
    let action_line = build_modal_clickable_row_line(
        inner,
        action_y,
        vec![
            (Span::styled(" Press ", app.theme.base()), None),
            (
                Span::styled("[Enter]", app.theme.key_hint()),
                Some(ClickTarget::ModalConfirm),
            ),
            (Span::styled(" to submit, ", app.theme.base()), None),
            (
                Span::styled("[Esc]", app.theme.key_hint()),
                Some(ClickTarget::CloseModal),
            ),
            (Span::styled(" to cancel.", app.theme.base()), None),
        ],
        hit_map,
    );
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(" Wearable: ", app.theme.muted()),
            Span::styled(wearable, app.theme.base()),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(prompt_label, app.theme.warning())]),
        Line::from(""),
        Line::from(vec![Span::styled(
            format!(" [ {} ]", input),
            app.theme.highlight(),
        )]),
        Line::from(""),
        action_line,
    ]
}

fn render_rename_modal(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
    let ModalState::RenameModal {
        ref current_alias,
        ref input,
        ..
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 48, 8);
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);
    Clear.render(modal_area, buf);

    let block = app.theme.card_block(" Rename Wearable Alias ");
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    if inner.width > 0 && inner.height > 2 {
        hit_map.push_modal_click(
            Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
            ClickTarget::FocusModalInput,
        );
    }

    let cursor = if app.theme.ascii { "_" } else { "█" };
    let action_line = build_modal_clickable_row_line(
        inner,
        inner.y.saturating_add(4),
        vec![
            (Span::raw(" "), None),
            (
                Span::styled("[Enter]", app.theme.key_hint()),
                Some(ClickTarget::ModalConfirm),
            ),
            (
                Span::styled(" Save", app.theme.base()),
                Some(ClickTarget::ModalConfirm),
            ),
            (Span::raw("   "), None),
            (
                Span::styled("[Esc]", app.theme.key_hint()),
                Some(ClickTarget::CloseModal),
            ),
            (
                Span::styled(" Cancel", app.theme.base()),
                Some(ClickTarget::CloseModal),
            ),
        ],
        hit_map,
    );
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
        action_line,
    ];

    Paragraph::new(lines).render(inner, buf);
}

fn render_unenroll_modal(app: &TuiApp, area: Rect, buf: &mut Buffer, hit_map: &mut HitMap) {
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
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);
    Clear.render(modal_area, buf);

    let block = app.theme.card_block(" Unenroll Wearable ");
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    if inner.width > 0 && inner.height > 3 {
        hit_map.push_modal_click(
            Rect::new(inner.x, inner.y.saturating_add(3), inner.width, 1),
            ClickTarget::UnenrollToggle(UnenrollCheckbox::ForgetBluez),
        );
    }
    if inner.width > 0 && inner.height > 4 {
        hit_map.push_modal_click(
            Rect::new(inner.x, inner.y.saturating_add(4), inner.width, 1),
            ClickTarget::UnenrollToggle(UnenrollCheckbox::DeleteSessions),
        );
    }

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

    let action_line = build_modal_clickable_row_line(
        inner,
        inner.y.saturating_add(6),
        vec![
            (Span::styled("   ", app.theme.base()), None),
            (
                Span::styled(" [ Confirm ] ", btn0_style),
                Some(ClickTarget::UnenrollConfirm),
            ),
            (Span::styled("   ", app.theme.base()), None),
            (
                Span::styled(" [ Cancel ] ", btn1_style),
                Some(ClickTarget::CloseModal),
            ),
        ],
        hit_map,
    );

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
        action_line,
    ];

    Paragraph::new(lines).render(inner, buf);
}
