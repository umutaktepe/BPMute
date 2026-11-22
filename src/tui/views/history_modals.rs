use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::super::app::{ExportModalField, FeedbackModalField, ModalState, TuiApp};
use super::super::hit_map::{ClickTarget, HitMap};
use super::{build_modal_clickable_row_line, centered_rect};

struct CyclicSelector<'a> {
    label: &'a str,
    value: &'a str,
    focused: bool,
    prev_target: ClickTarget,
    next_target: ClickTarget,
}

fn cyclic_selector_line<'a>(
    inner: Rect,
    y: u16,
    spec: CyclicSelector<'a>,
    app: &TuiApp,
    hit_map: &mut HitMap,
) -> Line<'a> {
    let val_style = if spec.focused {
        app.theme.highlight()
    } else {
        app.theme.base()
    };
    build_modal_clickable_row_line(
        inner,
        y,
        vec![
            (Span::styled(spec.label, app.theme.key_hint()), None),
            (Span::styled("<", val_style), Some(spec.prev_target)),
            (
                Span::styled(format!(" {} ", spec.value), val_style),
                Some(spec.next_target),
            ),
            (Span::styled(">", val_style), Some(spec.next_target)),
            (Span::styled("  (Space / ←/→)", app.theme.muted()), None),
        ],
        hit_map,
    )
}

fn modal_error_line<'a>(error_message: Option<&'a str>, app: &TuiApp) -> Line<'a> {
    if let Some(err) = error_message {
        Line::from(vec![
            Span::styled(" Error: ", app.theme.danger()),
            Span::styled(err, app.theme.danger()),
        ])
    } else {
        Line::from("")
    }
}

struct ModalButtonSpec<'a> {
    confirm_label: &'a str,
    confirm_style: Style,
    confirm_target: ClickTarget,
    cancel_style: Style,
}

fn modal_button_row<'a>(
    inner: Rect,
    y: u16,
    spec: ModalButtonSpec<'a>,
    app: &TuiApp,
    hit_map: &mut HitMap,
) -> Line<'a> {
    build_modal_clickable_row_line(
        inner,
        y,
        vec![
            (Span::styled("   ", app.theme.base()), None),
            (
                Span::styled(spec.confirm_label, spec.confirm_style),
                Some(spec.confirm_target),
            ),
            (Span::styled("       ", app.theme.base()), None),
            (
                Span::styled("[ Cancel ]", spec.cancel_style),
                Some(ClickTarget::CloseModal),
            ),
        ],
        hit_map,
    )
}

pub(super) fn render_feedback_modal(
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) {
    let ModalState::FeedbackModal {
        ref session_id,
        epochs_pruned,
        verdict_idx,
        ref onset_hhmm_input,
        ref avg_sleep_hr_input,
        active_field,
        ref error_message,
        ..
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 58, 14);
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);
    Clear.render(modal_area, buf);

    let block = app
        .theme
        .card_block(" Manual Feedback / Wearable-Report ")
        .border_style(app.theme.primary());
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let verdict_options = [
        "None",
        "False Positive",
        "Missed Sleep",
        "Anomalous (Exclude)",
    ];
    let cur_verdict = verdict_options.get(verdict_idx).unwrap_or(&"None");

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" Session: ", app.theme.muted()),
        Span::styled(session_id, app.theme.base()),
    ]));
    lines.push(Line::from(""));

    lines.push(cyclic_selector_line(
        inner,
        inner.y.saturating_add(2),
        CyclicSelector {
            label: " Verdict: ",
            value: cur_verdict,
            focused: active_field == FeedbackModalField::Verdict,
            prev_target: ClickTarget::FeedbackVerdictStep(false),
            next_target: ClickTarget::FeedbackVerdictStep(true),
        },
        app,
        hit_map,
    ));

    if epochs_pruned {
        lines.push(Line::from(vec![
            Span::styled(" Wearable Onset: ", app.theme.muted()),
            Span::styled(
                "[pruned: epoch trace pruned by retention]",
                app.theme.muted(),
            ),
        ]));
    } else {
        let onset_disp = if onset_hhmm_input.is_empty() {
            "HH:MM"
        } else {
            onset_hhmm_input
        };
        let onset_style = if active_field == FeedbackModalField::Onset {
            app.theme.highlight()
        } else {
            app.theme.base()
        };
        let target = Some(ClickTarget::FeedbackFocusField(FeedbackModalField::Onset));
        lines.push(build_modal_clickable_row_line(
            inner,
            inner.y.saturating_add(3),
            vec![
                (
                    Span::styled(" Wearable Onset: ", app.theme.key_hint()),
                    target,
                ),
                (
                    Span::styled(format!("[ {onset_disp} ]"), onset_style),
                    target,
                ),
                (Span::styled(" (local time)", app.theme.muted()), None),
            ],
            hit_map,
        ));
    }

    let hr_disp = if avg_sleep_hr_input.is_empty() {
        "30–140"
    } else {
        avg_sleep_hr_input
    };
    let hr_style = if active_field == FeedbackModalField::SleepHr {
        app.theme.highlight()
    } else {
        app.theme.base()
    };
    let hr_target = Some(ClickTarget::FeedbackFocusField(FeedbackModalField::SleepHr));
    lines.push(build_modal_clickable_row_line(
        inner,
        inner.y.saturating_add(4),
        vec![
            (
                Span::styled(" Avg Sleep HR:   ", app.theme.key_hint()),
                hr_target,
            ),
            (Span::styled(format!("[ {hr_disp} ]"), hr_style), hr_target),
            (
                Span::styled(" BPM (+2.5 bpm nadir offset applied)", app.theme.muted()),
                None,
            ),
        ],
        hit_map,
    ));

    lines.push(modal_error_line(error_message.as_deref(), app));

    let submit_style = if active_field == FeedbackModalField::Submit {
        app.theme.highlight()
    } else {
        app.theme.base()
    };
    let cancel_style = if active_field == FeedbackModalField::Cancel {
        app.theme.highlight()
    } else {
        app.theme.muted()
    };
    lines.push(modal_button_row(
        inner,
        inner.y.saturating_add(6),
        ModalButtonSpec {
            confirm_label: "[ Submit ]",
            confirm_style: submit_style,
            confirm_target: ClickTarget::ModalConfirm,
            cancel_style,
        },
        app,
        hit_map,
    ));

    Paragraph::new(lines).render(inner, buf);
}

pub(super) fn render_export_trace_modal(
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) {
    let ModalState::ExportTraceModal {
        ref session_id,
        format_idx,
        ref destination_path,
        anonymize,
        active_field,
        ref error_message,
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 60, 12);
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);
    Clear.render(modal_area, buf);

    let block = app
        .theme
        .card_block(" Export Trace ")
        .border_style(app.theme.primary());
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let format_str = if format_idx == 0 { "CSV" } else { "JSONL" };
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" Session: ", app.theme.muted()),
        Span::styled(session_id, app.theme.base()),
    ]));

    lines.push(cyclic_selector_line(
        inner,
        inner.y.saturating_add(1),
        CyclicSelector {
            label: " Format:  ",
            value: format_str,
            focused: active_field == ExportModalField::Format,
            prev_target: ClickTarget::ExportCycleFormat,
            next_target: ClickTarget::ExportCycleFormat,
        },
        app,
        hit_map,
    ));

    let path_style = if active_field == ExportModalField::Path {
        app.theme.highlight()
    } else {
        app.theme.base()
    };
    let path_target = Some(ClickTarget::ExportFocusField(ExportModalField::Path));
    lines.push(build_modal_clickable_row_line(
        inner,
        inner.y.saturating_add(2),
        vec![
            (
                Span::styled(" Path:    ", app.theme.key_hint()),
                path_target,
            ),
            (
                Span::styled(format!("[ {destination_path} ]"), path_style),
                path_target,
            ),
        ],
        hit_map,
    ));

    let anon_check = if anonymize { "[x]" } else { "[ ]" };
    let anon_style = if active_field == ExportModalField::Anonymize {
        app.theme.highlight()
    } else {
        app.theme.base()
    };
    lines.push(build_modal_clickable_row_line(
        inner,
        inner.y.saturating_add(3),
        vec![
            (
                Span::styled(format!(" {anon_check} Anonymize"), anon_style),
                Some(ClickTarget::ExportToggleAnonymize),
            ),
            (
                Span::styled(" (strip MAC addresses and PII)", app.theme.muted()),
                None,
            ),
        ],
        hit_map,
    ));

    lines.push(modal_error_line(error_message.as_deref(), app));

    let submit_style = if active_field == ExportModalField::Submit {
        app.theme.highlight()
    } else {
        app.theme.base()
    };
    let cancel_style = if active_field == ExportModalField::Cancel {
        app.theme.highlight()
    } else {
        app.theme.muted()
    };
    lines.push(modal_button_row(
        inner,
        inner.y.saturating_add(5),
        ModalButtonSpec {
            confirm_label: "[ Export ]",
            confirm_style: submit_style,
            confirm_target: ClickTarget::ModalConfirm,
            cancel_style,
        },
        app,
        hit_map,
    ));

    Paragraph::new(lines).render(inner, buf);
}

pub(super) fn render_delete_session_modal(
    app: &TuiApp,
    area: Rect,
    buf: &mut Buffer,
    hit_map: &mut HitMap,
) {
    let ModalState::DeleteSessionModal {
        ref session_id,
        confirmed,
    } = app.modal
    else {
        return;
    };

    let modal_area = centered_rect(area, 54, 8);
    hit_map.push_modal_click(modal_area, ClickTarget::Consume);
    Clear.render(modal_area, buf);

    let block = app
        .theme
        .card_block(" Delete Session ")
        .border_style(app.theme.danger());
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let submit_style = if confirmed {
        app.theme.highlight()
    } else {
        app.theme.danger()
    };
    let cancel_style = if !confirmed {
        app.theme.highlight()
    } else {
        app.theme.base()
    };

    let action_line = modal_button_row(
        inner,
        inner.y.saturating_add(3),
        ModalButtonSpec {
            confirm_label: "[ Delete ]",
            confirm_style: submit_style,
            confirm_target: ClickTarget::DeleteSessionConfirm,
            cancel_style,
        },
        app,
        hit_map,
    );

    let lines = vec![
        Line::from(vec![
            Span::styled(" Delete session '", app.theme.base()),
            Span::styled(session_id, app.theme.warning()),
            Span::styled("'?", app.theme.base()),
        ]),
        Line::from(vec![Span::styled(
            " Calibration will be recomputed bit-for-bit.",
            app.theme.muted(),
        )]),
        Line::from(""),
        action_line,
    ];

    Paragraph::new(lines).render(inner, buf);
}
