use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::super::app::TuiApp;

pub fn render_settings(app: &TuiApp, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" System Settings & Storage ")
        .title_style(app.theme.card_title())
        .borders(Borders::ALL)
        .border_style(app.theme.card_border());

    let inner = block.inner(area);
    block.render(area, buf);

    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Configuration & Data Management",
            app.theme.primary(),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Live TOML editing, $EDITOR handoff, biometric retention pruning,",
            app.theme.base(),
        )]),
        Line::from(vec![Span::styled(
            "  and ledger bundle import/export will be fully available in Part 2 (#23).",
            app.theme.muted(),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Use CLI 'bpmute config edit|validate' or 'bpmute data stats' to manage settings.",
            app.theme.key_hint(),
        )]),
    ];

    Paragraph::new(lines).render(inner, buf);
}
