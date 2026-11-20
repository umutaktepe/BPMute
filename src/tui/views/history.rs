use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::super::app::TuiApp;

pub fn render_history(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = app
        .theme
        .card_block(" Monitoring Sessions & Calibration History ");
    let inner = block.inner(area);
    block.render(area, buf);

    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "  History & Session Explorer",
            app.theme.primary(),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Monitoring Sessions, manual feedback overrides, trace export, and",
            app.theme.base(),
        )]),
        Line::from(vec![Span::styled(
            "  epoch detail charts will be fully available in Part 2 (#23).",
            app.theme.muted(),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Use CLI 'bpmute sessions list' to view recorded sessions.",
            app.theme.key_hint(),
        )]),
    ];

    Paragraph::new(lines).render(inner, buf);
}
